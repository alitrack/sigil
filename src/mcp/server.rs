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
    let registry = tools.clone_shallow();
    let mut stdout = tokio::io::stdout();
    let mut lines = BufReader::new(tokio::io::stdin()).lines();
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
                        "capabilities": { "tools": {}, "progress": {}, "resources": {} },
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
                // Progress: started before executing, finished after.
                let progress_start = json!({
                    "jsonrpc": "2.0",
                    "method": "notifications/progress",
                    "params": {
                        "progressToken": format!("tool-{name}"),
                        "progress": 0.0,
                        "total": 1.0,
                        "message": format!("tool {name} started")
                    }
                });
                write_frame(&mut stdout, &progress_start).await?;
                let result = registry.execute(&call).await;
                match result {
                    Ok(out) => {
                        let progress = json!({
                            "jsonrpc": "2.0",
                            "method": "notifications/progress",
                            "params": {
                                "progressToken": format!("tool-{name}"),
                                "progress": 1.0,
                                "total": 1.0,
                                "message": format!("tool {name} finished")
                            }
                        });
                        write_frame(&mut stdout, &progress).await?;
                        let resp = json!({
                            "jsonrpc": "2.0",
                            "id": id,
                            "result": {
                                "content": [{
                                    "type": "text",
                                    "text": out
                                }]
                            }
                        });
                        write_frame(&mut stdout, &resp).await?;
                    }
                    Err(e) => {
                        let progress = json!({
                            "jsonrpc": "2.0",
                            "method": "notifications/progress",
                            "params": {
                                "progressToken": format!("tool-{name}"),
                                "progress": 1.0,
                                "total": 1.0,
                                "message": format!("tool {name} failed: {e:#}")
                            }
                        });
                        write_frame(&mut stdout, &progress).await?;
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
                // Unknown method: JSON-RPC error response.
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
    Ok(())
}

async fn write_frame(stdout: &mut tokio::io::Stdout, value: &Value) -> Result<()> {
    let mut line = serde_json::to_string(value).context("serializing MCP frame")?;
    line.push('\n');
    stdout.write_all(line.as_bytes()).await?;
    stdout.flush().await?;
    Ok(())
}
