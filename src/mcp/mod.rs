//! MCP (Model Context Protocol) client — stdio transport.
//!
//! Spawns an MCP server subprocess, speaks JSON-RPC 2.0 over
//! newline-delimited JSON on stdin/stdout:
//!   initialize → (server sends tools/list on its own) → tools/call
//!
//! Each connection runs a reader task that routes responses back to
//! pending requests by id. The `McpTool` wrapper (tools/mcp.rs)
//! exposes each discovered tool through the normal Tool trait, so the
//! agent loop treats MCP tools exactly like native ones.

use std::collections::HashMap;
use std::process::Stdio;
use std::sync::Arc;

use anyhow::{Context, Result, anyhow};
use serde_json::{Value, json};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::process::{ChildStdin, Command};
use tokio::sync::{Mutex, mpsc};

#[cfg(test)]
mod tests;

pub mod server;

#[cfg(test)]
mod server_tests;

/// How long a stdio MCP request may wait for its response before the
/// caller gets an error instead of hanging forever. Generous on
/// purpose: `tools/call` can legitimately run minutes (builds, test
/// suites) — but a server that is alive yet never answers used to
/// block the agent indefinitely (`rx.recv()` has no deadline; the
/// error message even claimed a timeout that did not exist).
const STDIO_REQUEST_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(600);

/// A tool discovered on an MCP server.
#[derive(Debug, Clone)]
pub struct McpToolInfo {
    pub name: String,
    pub description: String,
    pub schema: Value,
}

/// One MCP server connection.
pub struct McpClient {
    /// Server name (for diagnostics; kept for future multi-server logs).
    #[allow(dead_code)]
    name: String,
    transport: McpTransport,
    next_id: std::sync::atomic::AtomicU64,
}

enum McpTransport {
    /// Subprocess speaking NDJSON on stdin/stdout.
    Stdio {
        stdin: Mutex<ChildStdin>,
        /// id → response channel (tx dropped on error; reader pushes Err).
        pending: Arc<Mutex<HashMap<u64, mpsc::UnboundedSender<Result<Value>>>>>,
    },
    /// Streamable HTTP: one POST per request (JSON-RPC 2.0 over HTTP).
    Http {
        client: reqwest::Client,
        url: String,
        token: Option<String>,
        /// Server-assigned session (`Mcp-Session-Id` response header on
        /// initialize). Stateful Streamable-HTTP servers 404 any request
        /// that does not present it, so it is captured on first sight and
        /// echoed on every subsequent request. Brief synchronous lock —
        /// never held across an await.
        session: std::sync::Mutex<Option<String>>,
    },
}

/// Parse a `tools/list` result into tool infos.
fn parse_tools(tools_result: Value) -> Vec<McpToolInfo> {
    tools_result
        .get("tools")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default()
        .into_iter()
        .map(|t| McpToolInfo {
            name: t
                .get("name")
                .and_then(Value::as_str)
                .unwrap_or("?")
                .to_string(),
            description: t
                .get("description")
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_string(),
            schema: t
                .get("inputSchema")
                .cloned()
                .unwrap_or(json!({"type": "object"})),
        })
        .collect()
}

/// Parse an SSE (Server-Sent Events) body into its JSON-RPC messages.
///
/// SSE framing per the spec: events are separated by blank lines; each
/// event is a set of `field: value` lines, of which consecutive `data:`
/// lines are joined with '\n' into one payload. A field value may carry
/// one optional leading space (`data:x` and `data: x` are equivalent).
/// Non-`data` fields (`event:`, `id:`, `retry:`, `: ping` comments)
/// carry no JSON-RPC payload and are ignored.
///
/// Returns every payload that parses as JSON. Callers pick the message
/// matching their request id: a Streamable-HTTP server may send
/// notifications (progress, logging, pings) BEFORE the response, so
/// taking the first `data:` line blindly matched a notification (or a
/// `data: {}` ping) instead of the actual response.
fn parse_sse_messages(body: &str) -> Vec<serde_json::Value> {
    fn flush(data: &mut Vec<String>, out: &mut Vec<serde_json::Value>) {
        if data.is_empty() {
            return;
        }
        let joined = data.join("\n");
        data.clear();
        // A non-JSON data payload (e.g. the `[DONE]` sentinel some
        // gateways send) is skipped, not fatal — it is not our response.
        if let Ok(v) = serde_json::from_str::<serde_json::Value>(&joined) {
            out.push(v);
        }
    }
    let mut out = Vec::new();
    let mut data: Vec<String> = Vec::new();
    for line in body.lines() {
        if line.is_empty() {
            // Blank line: the event is complete.
            flush(&mut data, &mut out);
            continue;
        }
        if let Some(rest) = line.strip_prefix("data:") {
            let payload = rest.strip_prefix(' ').unwrap_or(rest);
            data.push(payload.to_string());
        }
    }
    flush(&mut data, &mut out);
    out
}

/// Consume complete SSE events until this request's response arrives.
/// The HTTP body may stay open after the result, so EOF is not a deadline.
async fn read_sse_response(mut response: reqwest::Response, id: u64) -> Result<Value> {
    let mut pending = Vec::<u8>::new();
    let mut event = String::new();
    while let Some(chunk) = response.chunk().await.context("reading MCP SSE body")? {
        pending.extend_from_slice(&chunk);
        while let Some(newline) = pending.iter().position(|&byte| byte == b'\n') {
            let line: Vec<_> = pending.drain(..=newline).collect();
            let line = String::from_utf8_lossy(&line[..newline]);
            let line = line.strip_suffix('\r').unwrap_or(&line);
            if line.is_empty() {
                if let Some(message) = parse_sse_messages(&event)
                    .into_iter()
                    .find(|message| message.get("id").and_then(Value::as_u64) == Some(id))
                {
                    return Ok(message);
                }
                event.clear();
            } else {
                event.push_str(line);
                event.push('\n');
            }
        }
    }
    event.push_str(&String::from_utf8_lossy(&pending));
    parse_sse_messages(&event)
        .into_iter()
        .find(|message| message.get("id").and_then(Value::as_u64) == Some(id))
        .with_context(|| format!("MCP SSE: stream ended without response for request id {id}"))
}

/// Preview of a string for error messages (first 200 chars).
fn preview(s: &str) -> String {
    s.chars().take(200).collect()
}

impl McpClient {
    /// Spawn the server subprocess, run initialize, discover tools.
    pub async fn connect(
        name: &str,
        command: &str,
        args: &[String],
    ) -> Result<(McpClient, Vec<McpToolInfo>)> {
        // env_clear: never leak the parent's secrets to a third-party
        // MCP server (same policy as Hermes' native client).
        //
        // Windows: WSAEDISTRO (10106) — any child that does Winsock I/O
        // (python3, node, curl, ...) fails to initialize Winsock when
        // SystemRoot is missing from its environment; .NET resolves it
        // another way so Rust-std children masked the bug. Pass through
        // the public system-path vars (not secrets).
        let mut child = Command::new(command);
        let child = child.args(args).env_clear();
        let child = child.env("PATH", std::env::var("PATH").unwrap_or_default());
        let child = child.env("HOME", std::env::var("HOME").unwrap_or_default());
        // WSAEDISTRO (10106) — children doing Winsock I/O fail to init
        // Winsock without SystemRoot (python3/node/curl); .NET hides the
        // bug by resolving it separately, so only non-.NET MCP servers
        // hit it. Pass public system-path vars (not secrets). The
        // `#[cfg]` gates a `let` that CONTINUES the binding chain — not a
        // nested block: a rebinding inside `{ }` dies with the block, and
        // the chain below would silently resume from the pre-block binding,
        // dropping all three vars (the pre-fix behavior, which also
        // silently voided the WSAEDISTRO fix).
        #[cfg(windows)]
        let child = child
            .env(
                "SystemRoot",
                std::env::var("SystemRoot").unwrap_or_default(),
            )
            .env("WINDIR", std::env::var("WINDIR").unwrap_or_default())
            .env("TEMP", std::env::var("TEMP").unwrap_or_default());
        let child = child
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null());
        let mut child = child
            .spawn()
            .with_context(|| format!("spawning MCP server '{name}' ({command})"))?;

        let stdin = child.stdin.take().context("MCP server stdin")?;
        let stdout = child.stdout.take().context("MCP server stdout")?;

        let client = McpClient {
            name: name.to_string(),
            transport: McpTransport::Stdio {
                stdin: Mutex::new(stdin),
                pending: Arc::default(),
            },
            next_id: std::sync::atomic::AtomicU64::new(1),
        };

        // Reader task: parse NDJSON, route by id.
        let pending = match &client.transport {
            McpTransport::Stdio { pending, .. } => pending.clone(),
            McpTransport::Http { .. } => unreachable!(),
        };
        let name_owned = name.to_string();
        tokio::spawn(async move {
            let mut lines = BufReader::new(stdout).lines();
            loop {
                let Ok(Some(line)) = lines.next_line().await else {
                    break; // EOF / read error — server died
                };
                let Ok(msg) = serde_json::from_str::<Value>(&line) else {
                    continue; // log line or malformed frame
                };
                // Notifications (no id) are ignored for now.
                let Some(id) = msg.get("id").and_then(Value::as_u64) else {
                    continue;
                };
                let Some(tx) = pending.lock().await.remove(&id) else {
                    continue;
                };
                if let Some(err) = msg.get("error") {
                    let _ = tx.send(Err(anyhow!("MCP error: {err}")));
                } else {
                    let _ = tx.send(Ok(msg.get("result").cloned().unwrap_or(Value::Null)));
                }
            }
            // Reader exited: fail any still-pending requests.
            let p = pending.lock().await;
            for tx in p.values() {
                let _ = tx.send(Err(anyhow!("MCP server '{name_owned}' closed connection")));
            }
        });

        // Initialize handshake.
        let init_result = client
            .request(
                "initialize",
                json!({
                    "protocolVersion": "2024-11-05",
                    "capabilities": {},
                    "clientInfo": { "name": "sigil", "version": "0.1.0" }
                }),
            )
            .await?;
        let _ = init_result;

        // Notify initialized (fire-and-forget).
        client.notify("notifications/initialized", json!({})).await;

        // Discover tools.
        let tools_result = client.request("tools/list", json!({})).await?;
        let tools = parse_tools(tools_result);

        Ok((client, tools))
    }
    /// Connect to a Streamable HTTP MCP server (JSON-RPC 2.0 over HTTP).
    ///
    /// One POST per request; responses come back as a plain JSON body or
    /// an SSE stream (the request parses both, matching by JSON-RPC id).
    /// `token` is sent as `Authorization: Bearer <token>` when present.
    /// A server-assigned `Mcp-Session-Id` header is captured from the
    /// initialize response and echoed on every later request (stateful
    /// servers 404 without it).
    pub async fn connect_http(
        name: &str,
        url: &str,
        token: Option<&str>,
    ) -> Result<(McpClient, Vec<McpToolInfo>)> {
        let http = reqwest::Client::builder()
            // loopback must never go through http_proxy (P66: proxy hijack)
            .no_proxy()
            // Bound the whole request, body read included: a Streamable
            // HTTP server that never returns the requested response
            // must not hang the agent forever. 180s is generous for real tool
            // calls (which may legitimately take minutes).
            .timeout(std::time::Duration::from_secs(180))
            .connect_timeout(std::time::Duration::from_secs(30))
            .build()
            .context("building reqwest client for MCP HTTP transport")?;
        let client = McpClient {
            name: name.to_string(),
            transport: McpTransport::Http {
                client: http,
                url: url.to_string(),
                token: token.map(str::to_string),
                session: std::sync::Mutex::new(None),
            },
            next_id: std::sync::atomic::AtomicU64::new(1),
        };

        // Initialize handshake.
        let init_result = client
            .request(
                "initialize",
                json!({
                    "protocolVersion": "2024-11-05",
                    "capabilities": {},
                    "clientInfo": { "name": "sigil", "version": "0.1.0" }
                }),
            )
            .await?;
        let _ = init_result;

        // Notify initialized (fire-and-forget).
        client.notify("notifications/initialized", json!({})).await;

        // Discover tools.
        let tools_result = client.request("tools/list", json!({})).await?;
        let tools = parse_tools(tools_result);

        client.notify("notifications/initialized", json!({})).await;
        Ok((client, tools))
    }

    /// Send a JSON-RPC request and await its response.
    pub async fn request(&self, method: &str, params: Value) -> Result<Value> {
        let id = self
            .next_id
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let frame = json!({
            "jsonrpc": "2.0",
            "id": id,
            "method": method,
            "params": params,
        });

        match &self.transport {
            McpTransport::Stdio { stdin, pending } => {
                let (tx, mut rx) = mpsc::unbounded_channel();
                pending.lock().await.insert(id, tx);

                let mut line = serde_json::to_string(&frame)?;
                line.push('\n');
                stdin
                    .lock()
                    .await
                    .write_all(line.as_bytes())
                    .await
                    .context("writing to MCP server")?;
                stdin.lock().await.flush().await.ok();

                match tokio::time::timeout(STDIO_REQUEST_TIMEOUT, rx.recv()).await {
                    Ok(res) => res.context(format!(
                        "MCP request '{method}' failed: server closed the connection"
                    ))?,
                    Err(_) => anyhow::bail!(
                        "MCP request '{method}' timed out after {STDIO_REQUEST_TIMEOUT:?} \
                         (server alive but unresponsive — it was NOT killed; \
                         retry, or check the server)"
                    ),
                }
            }
            McpTransport::Http {
                client,
                url,
                token,
                session,
            } => {
                let body = serde_json::to_string(&frame)?;
                let mut req = client
                    .post(url)
                    .header("Content-Type", "application/json")
                    .header("Accept", "application/json, text/event-stream")
                    // Newer Streamable-HTTP servers require the protocol
                    // version on every request, not just initialize.
                    .header("Mcp-Protocol-Version", "2024-11-05")
                    .body(body);
                if let Some(t) = token {
                    req = req.bearer_auth(t);
                }
                {
                    // Echo the server-assigned session id when we have
                    // one (captured below on first sight).
                    let guard = session.lock().unwrap_or_else(|e| e.into_inner());
                    if let Some(sid) = guard.as_deref() {
                        req = req.header("Mcp-Session-Id", sid);
                    }
                }
                let resp = req.send().await.context("MCP HTTP request")?;
                let status = resp.status();
                // Capture the session id the first time the server
                // presents it (initialize assigns it; later responses
                // repeat it harmlessly).
                if let Some(sid) = resp
                    .headers()
                    .get("mcp-session-id")
                    .and_then(|v| v.to_str().ok())
                    .filter(|s| !s.is_empty())
                {
                    *session.lock().unwrap_or_else(|e| e.into_inner()) = Some(sid.to_string());
                }
                let content_type = resp
                    .headers()
                    .get("Content-Type")
                    .and_then(|v| v.to_str().ok())
                    .unwrap_or("")
                    .to_lowercase();

                if content_type.contains("text/event-stream") {
                    // SSE response from Streamable HTTP MCP servers.
                    // Events may interleave: the server can send
                    // notifications (progress/logging/ping) BEFORE the
                    // JSON-RPC response, so parse every event and pick
                    // the message whose id matches ours.
                    if !status.is_success() {
                        return Err(anyhow!("MCP HTTP {status} for '{method}'"));
                    }
                    let v = read_sse_response(resp, id).await?;
                    if let Some(err) = v.get("error") {
                        return Err(anyhow!("MCP error: {err}"));
                    }
                    Ok(v.get("result").cloned().unwrap_or(Value::Null))
                } else {
                    // Plain JSON response
                    let body = resp.text().await.context("reading MCP HTTP body")?;
                    if !status.is_success() {
                        return Err(anyhow!(
                            "MCP HTTP {status} for '{method}': {}",
                            preview(&body)
                        ));
                    }
                    let v: Value = serde_json::from_str(&body)
                        .with_context(|| format!("MCP HTTP bad JSON: {}", preview(&body)))?;
                    if let Some(err) = v.get("error") {
                        return Err(anyhow!("MCP error: {err}"));
                    }
                    Ok(v.get("result").cloned().unwrap_or(Value::Null))
                }
            }
        }
    }

    /// Fire-and-forget notification.
    pub async fn notify(&self, method: &str, params: Value) {
        let frame = json!({
            "jsonrpc": "2.0",
            "method": method,
            "params": params,
        });
        match &self.transport {
            McpTransport::Stdio { stdin, .. } => {
                if let Ok(mut line) = serde_json::to_string(&frame) {
                    line.push('\n');
                    let _ = stdin.lock().await.write_all(line.as_bytes()).await;
                    let _ = stdin.lock().await.flush().await;
                }
            }
            McpTransport::Http {
                client,
                url,
                token,
                session,
            } => {
                let body = serde_json::to_string(&frame).unwrap_or_default();
                let mut req = client
                    .post(url)
                    .header("Content-Type", "application/json")
                    .header("Accept", "application/json, text/event-stream")
                    .header("Mcp-Protocol-Version", "2024-11-05")
                    .body(body);
                if let Some(t) = token {
                    req = req.bearer_auth(t);
                }
                {
                    let guard = session.lock().unwrap_or_else(|e| e.into_inner());
                    if let Some(sid) = guard.as_deref() {
                        req = req.header("Mcp-Session-Id", sid);
                    }
                }
                let _ = req.send().await;
            }
        }
    }

    /// Call a tool and return its structured content.
    pub async fn call_tool(&self, tool: &str, arguments: Value) -> Result<Value> {
        let result = self
            .request(
                "tools/call",
                json!({
                    "name": tool,
                    "arguments": arguments,
                }),
            )
            .await?;
        // MCP result: { content: [{ type: "text", text: "..." }], isError? }
        let content = result
            .get("content")
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default();
        let mut texts: Vec<String> = Vec::new();
        for c in &content {
            if c.get("type").and_then(Value::as_str) == Some("text")
                && let Some(t) = c.get("text").and_then(Value::as_str)
            {
                texts.push(t.to_string());
            }
        }
        let joined = texts.join("\n");
        if result
            .get("isError")
            .and_then(Value::as_bool)
            .unwrap_or(false)
        {
            Err(anyhow!("MCP tool '{tool}' returned error: {}", joined))
        } else {
            Ok(Value::String(joined))
        }
    }

    /// Server name.
    #[allow(dead_code)]
    pub fn name(&self) -> &str {
        &self.name
    }

    /// List resources exposed by the server (MCP resources/list).
    pub async fn list_resources(&self) -> Result<Vec<Value>> {
        let result = self.request("resources/list", json!({})).await?;
        Ok(result
            .get("resources")
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default())
    }

    /// Read a resource's content by URI (MCP resources/read).
    pub async fn read_resource(&self, uri: &str) -> Result<String> {
        let result = self
            .request("resources/read", json!({ "uri": uri }))
            .await?;
        let contents = result
            .get("contents")
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default();
        let mut texts: Vec<String> = Vec::new();
        for c in &contents {
            if let Some(t) = c.get("text").and_then(Value::as_str) {
                texts.push(t.to_string());
            }
        }
        if texts.is_empty() {
            anyhow::bail!("MCP resource '{uri}' returned no text content");
        }
        Ok(texts.join("\n"))
    }

    /// List prompt templates exposed by the server (MCP prompts/list).
    pub async fn list_prompts(&self) -> Result<Vec<Value>> {
        let result = self.request("prompts/list", json!({})).await?;
        Ok(result
            .get("prompts")
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default())
    }

    /// Fetch a prompt template's rendered messages (MCP prompts/get).
    /// Returns the concatenated text of all messages.
    pub async fn get_prompt(&self, name: &str) -> Result<String> {
        let result = self.request("prompts/get", json!({ "name": name })).await?;
        let messages = result
            .get("messages")
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default();
        let mut texts: Vec<String> = Vec::new();
        for m in &messages {
            if let Some(t) = m
                .get("content")
                .and_then(|c| c.get("text"))
                .and_then(Value::as_str)
            {
                texts.push(t.to_string());
            }
        }
        if texts.is_empty() {
            anyhow::bail!("MCP prompt '{name}' returned no text content");
        }
        Ok(texts.join("\n"))
    }
}
