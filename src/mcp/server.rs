//! MCP (Model Context Protocol) server — stdio transport.
//!
//! The inverse of the client: `sigil mcp` speaks the server side of MCP,
//! exposing sigil's own tool registry (bash, read_file, write_file, git,
//! scripted Lua tools, memory, ...) to any MCP client — Claude Desktop,
//! Cursor, other agents. JSON-RPC 2.0 over newline-delimited JSON:
//!   initialize → notifications/initialized → tools/list → tools/call

use anyhow::{Context, Result};
use serde_json::{Value, json};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};

use crate::llm::ToolCall;
use crate::tools::ToolRegistry;

/// A read-only resource exposed via MCP `resources/*` (e.g. harness
/// skills, the system prompt). URI, human name, description, content.
#[derive(Debug, Clone)]
pub struct Resource {
    pub uri: String,
    pub name: String,
    pub description: String,
    pub content: String,
}

/// Serve MCP on stdin/stdout until EOF. `tools` is the registry whose
/// tools get exposed; a shallow clone is taken so hot-registered tools
/// (created by evolution mid-session) show up on subsequent tools/list.
/// `resources` (harness skills etc.) are exposed via resources/list and
/// resources/read.
pub async fn serve(tools: &ToolRegistry, resources: &[Resource]) -> Result<()> {
    serve_io(tools, resources, tokio::io::stdin(), tokio::io::stdout()).await
}

pub(super) async fn serve_io<R, W>(
    tools: &ToolRegistry,
    resources: &[Resource],
    stdin: R,
    mut stdout: W,
) -> Result<()>
where
    R: tokio::io::AsyncRead + Unpin,
    W: tokio::io::AsyncWrite + Unpin,
{
    let mut registry = tools.clone_shallow();
    let mut lines = BufReader::new(stdin).lines();
    let mut next_id: u64 = 0;

    while let Some(line) = lines.next_line().await? {
        let msg: Value = match serde_json::from_str(&line) {
            Ok(m) => m,
            Err(_) => continue, // skip malformed frames
        };
        let method = msg
            .get("method")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string();
        let params = msg.get("params").cloned().unwrap_or(Value::Null);
        let id = msg.get("id").cloned();

        match method.as_str() {
            "initialize" => {
                let resp = json!({
                    "jsonrpc": "2.0",
                    "id": id,
                    "result": {
                        "protocolVersion": "2024-11-05",
                        "capabilities": { "tools": {}, "progress": {}, "resources": {}, "prompts": {} },
                        "serverInfo": { "name": "sigil", "version": "0.1.0" }
                    }
                });
                write_frame(&mut stdout, &resp).await?;
            }
            "notifications/initialized" => {
                // Acknowledge by doing nothing.
            }
            "tools/list" => {
                let specs = registry.specs();
                let tool_list: Vec<Value> = specs
                    .iter()
                    .map(|s| {
                        json!({
                            "name": s.name,
                            "description": s.description,
                            "inputSchema": s.parameters,
                        })
                    })
                    .collect();
                let resp = json!({
                    "jsonrpc": "2.0",
                    "id": id,
                    "result": { "tools": tool_list }
                });
                write_frame(&mut stdout, &resp).await?;
            }
            "resources/list" => {
                let list: Vec<Value> = resources
                    .iter()
                    .map(|r| {
                        json!({
                            "uri": r.uri,
                            "name": r.name,
                            "description": r.description,
                            "mimeType": "text/markdown",
                        })
                    })
                    .collect();
                let resp = json!({
                    "jsonrpc": "2.0",
                    "id": id,
                    "result": { "resources": list }
                });
                write_frame(&mut stdout, &resp).await?;
            }
            "resources/read" => {
                let uri = params
                    .get("uri")
                    .and_then(Value::as_str)
                    .unwrap_or_default();
                match resources.iter().find(|r| r.uri == uri) {
                    Some(r) => {
                        let resp = json!({
                            "jsonrpc": "2.0",
                            "id": id,
                            "result": {
                                "contents": [{
                                    "uri": r.uri,
                                    "mimeType": "text/markdown",
                                    "text": r.content,
                                }]
                            }
                        });
                        write_frame(&mut stdout, &resp).await?;
                    }
                    None => {
                        let resp = json!({
                            "jsonrpc": "2.0",
                            "id": id,
                            "error": {
                                "code": -32002,
                                "message": format!("resource not found: {uri}")
                            }
                        });
                        write_frame(&mut stdout, &resp).await?;
                    }
                }
            }
            "prompts/list" => {
                // Every exposed resource doubles as a prompt template.
                let list: Vec<Value> = resources
                    .iter()
                    .map(|r| {
                        json!({
                            "name": r.name,
                            "description": r.description,
                        })
                    })
                    .collect();
                let resp = json!({
                    "jsonrpc": "2.0",
                    "id": id,
                    "result": { "prompts": list }
                });
                write_frame(&mut stdout, &resp).await?;
            }
            "prompts/get" => {
                let name = params
                    .get("name")
                    .and_then(Value::as_str)
                    .unwrap_or_default();
                match resources.iter().find(|r| r.name == name) {
                    Some(r) => {
                        let resp = json!({
                            "jsonrpc": "2.0",
                            "id": id,
                            "result": {
                                "description": r.description,
                                "messages": [{
                                    "role": "user",
                                    "content": {
                                        "type": "text",
                                        "text": r.content,
                                    }
                                }]
                            }
                        });
                        write_frame(&mut stdout, &resp).await?;
                    }
                    None => {
                        let resp = json!({
                            "jsonrpc": "2.0",
                            "id": id,
                            "error": { "code": -32602, "message": format!("prompt not found: {name}") }
                        });
                        write_frame(&mut stdout, &resp).await?;
                    }
                }
            }
            "tools/call" => {
                let name = params
                    .get("name")
                    .and_then(Value::as_str)
                    .unwrap_or_default();
                let arguments = params.get("arguments").cloned().unwrap_or(json!({}));
                let call = ToolCall {
                    name: name.to_string(),
                    arguments,
                    id: format!("mcp-{}", next_id),
                };
                next_id += 1;
                // Progress (spec): echo the token the CLIENT supplied in
                // `_meta.progressToken` — a fabricated token cannot be
                // correlated by the caller. Unsolicited progress (no
                // token) is skipped: notifications a client never asked
                // for are at best noise.
                let progress_token = params
                    .get("_meta")
                    .and_then(|m| m.get("progressToken"))
                    .cloned();
                let progress = |fraction: f64, message: String| {
                    progress_token.clone().map(|token| {
                        json!({
                            "jsonrpc": "2.0",
                            "method": "notifications/progress",
                            "params": {
                                "progressToken": token,
                                "progress": fraction,
                                "total": 1.0,
                                "message": message,
                            }
                        })
                    })
                };
                if let Some(frame) = progress(0.0, format!("tool {name} started")) {
                    write_frame(&mut stdout, &frame).await?;
                }
                let result = if crate::safety::check(&call.name, &call.arguments)
                    == crate::safety::GateDecision::Reject
                {
                    Err(anyhow::anyhow!(crate::safety::reject_message(
                        &call.name,
                        &call.arguments
                    )))
                } else {
                    registry.execute_detailed(&call).await
                };
                match result {
                    Ok(out) => {
                        if let Some(frame) = progress(1.0, format!("tool {name} finished")) {
                            write_frame(&mut stdout, &frame).await?;
                        }
                        // A tool may have hot-registered / unregistered
                        // scripted tools (create_lua_tool, skill_rollback);
                        // apply the queued ops so the very next tools/call
                        // sees the new implementation.
                        registry.drain_hot();
                        let resp = json!({
                            "jsonrpc": "2.0",
                            "id": id,
                            "result": {
                                "content": [{
                                    "type": "text",
                                    "text": out.text
                                }],
                                "isError": out.is_error
                            }
                        });
                        write_frame(&mut stdout, &resp).await?;
                    }
                    Err(e) => {
                        if let Some(frame) = progress(1.0, format!("tool {name} failed: {e:#}")) {
                            write_frame(&mut stdout, &frame).await?;
                        }
                        let resp = json!({
                            "jsonrpc": "2.0",
                            "id": id,
                            "result": {
                                "content": [{ "type": "text", "text": format!("{e:#}") }],
                                "isError": true
                            }
                        });
                        write_frame(&mut stdout, &resp).await?;
                    }
                }
            }
            "ping" => {
                let resp = json!({
                    "jsonrpc": "2.0",
                    "id": id,
                    "result": {}
                });
                write_frame(&mut stdout, &resp).await?;
            }
            _ => {
                // Unknown REQUEST: JSON-RPC error response. Unknown
                // NOTIFICATION (no id): no response — JSON-RPC
                // notifications never get replies, and an `id: null`
                // error frame trips strict clients.
                if let Some(id) = id {
                    let resp = json!({
                        "jsonrpc": "2.0",
                        "id": id,
                        "error": {
                            "code": -32601,
                            "message": format!("method not found: {method}")
                        }
                    });
                    write_frame(&mut stdout, &resp).await?;
                }
            }
        }
    }
    Ok(())
}

async fn write_frame<W: tokio::io::AsyncWrite + Unpin>(
    stdout: &mut W,
    value: &Value,
) -> Result<()> {
    let mut line = serde_json::to_string(value).context("serializing MCP frame")?;
    line.push('\n');
    stdout.write_all(line.as_bytes()).await?;
    stdout.flush().await?;
    Ok(())
}
