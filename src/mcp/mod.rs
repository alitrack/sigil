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
    stdin: Mutex<ChildStdin>,
    /// id → response channel (tx dropped on error; reader pushes Err).
    pending: Arc<Mutex<HashMap<u64, mpsc::UnboundedSender<Result<Value>>>>>,
    next_id: std::sync::atomic::AtomicU64,
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
        let mut child = Command::new(command)
            .args(args)
            .env_clear()
            .env("PATH", std::env::var("PATH").unwrap_or_default())
            .env("HOME", std::env::var("HOME").unwrap_or_default())
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .with_context(|| format!("spawning MCP server '{name}' ({command})"))?;

        let stdin = child.stdin.take().context("MCP server stdin")?;
        let stdout = child.stdout.take().context("MCP server stdout")?;

        let client = McpClient {
            name: name.to_string(),
            stdin: Mutex::new(stdin),
            pending: Arc::default(),
            next_id: std::sync::atomic::AtomicU64::new(1),
        };

        // Reader task: parse NDJSON, route by id.
        let pending = client.pending.clone();
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
        let tools = tools_result
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
            .collect();

        Ok((client, tools))
    }

    /// Send a JSON-RPC request and await its response.
    pub async fn request(&self, method: &str, params: Value) -> Result<Value> {
        let id = self
            .next_id
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let (tx, mut rx) = mpsc::unbounded_channel();
        self.pending.lock().await.insert(id, tx);

        let frame = json!({
            "jsonrpc": "2.0",
            "id": id,
            "method": method,
            "params": params,
        });
        let mut line = serde_json::to_string(&frame)?;
        line.push('\n');
        self.stdin
            .lock()
            .await
            .write_all(line.as_bytes())
            .await
            .context("writing to MCP server")?;
        self.stdin.lock().await.flush().await.ok();

        rx.recv()
            .await
            .context(format!("MCP request '{method}' timed out or server died"))?
    }

    /// Fire-and-forget notification.
    pub async fn notify(&self, method: &str, params: Value) {
        let frame = json!({
            "jsonrpc": "2.0",
            "method": method,
            "params": params,
        });
        if let Ok(mut line) = serde_json::to_string(&frame) {
            line.push('\n');
            let _ = self.stdin.lock().await.write_all(line.as_bytes()).await;
            let _ = self.stdin.lock().await.flush().await;
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
