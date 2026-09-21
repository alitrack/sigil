//! ACP (Agent Client Protocol) client — drive an external ACP server.
//!
//! The inverse of `serve`: instead of being driven, sigil spawns
//! another agent (any ACP-speaking process: an editor, another sigil,
//! codex, ...) over stdio and talks to it like a client. The agent
//! loop exposes this as the `acp_call` tool, letting the main agent
//! delegate work to an external agent process.
//!
//! Flow: spawn → `agent/initialize` → `agent/session/new` →
//! `agent/session.prompt` → collect `client/session.update` stream
//! (text + tool messages) until the turn ends → `agent/session.close`.

use std::process::Stdio;

use anyhow::{Context, Result};
use serde_json::{Value, json};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::process::Command;

/// A running ACP server session.
pub struct AcpClient {
    /// Kept alive for the session's lifetime: dropping it with
    /// `kill_on_drop` would kill the child. Dropped on close.
    #[allow(dead_code)]
    child: tokio::process::Child,
    stdin: tokio::process::ChildStdin,
    stdout: BufReader<tokio::process::ChildStdout>,
    next_id: u64,
    session_id: String,
}

/// Spawn an ACP server and initialize a session.
pub async fn connect(cmd: &str, args: &[String]) -> Result<AcpClient> {
    let mut command = Command::new(cmd);
    command
        .args(args)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        // Never leak parent secrets into the ACP child.
        .env_clear()
        .env("PATH", std::env::var("PATH").unwrap_or_default())
        .env("HOME", std::env::var("HOME").unwrap_or_default());
    // Windows parity with the MCP stdio transport (upstream 731ca1e):
    // children doing Winsock I/O fail to initialize it without
    // SystemRoot (WSAEDISTRO 10106 — python3/node/curl children);
    // TEMP is needed by anything using the temp dir. Public system
    // paths, not secrets.
    #[cfg(windows)]
    {
        for key in ["SystemRoot", "WINDIR", "TEMP"] {
            if let Ok(v) = std::env::var(key) {
                command.env(key, v);
            }
        }
    }
    let mut child = command
        .kill_on_drop(true)
        .spawn()
        .with_context(|| format!("spawning ACP server {cmd}"))?;

    let stdin = child.stdin.take().context("acp child stdin")?;
    let stdout = child.stdout.take().context("acp child stdout")?;
    // Drain stderr so the child never blocks on a full pipe.
    if let Some(stderr) = child.stderr.take() {
        tokio::spawn(async move {
            let mut r = BufReader::new(stderr);
            let mut buf = String::new();
            loop {
                buf.clear();
                if r.read_line(&mut buf).await.unwrap_or(0) == 0 {
                    break;
                }
                eprintln!("[acp-child stderr] {}", buf.trim_end());
            }
        });
    }

    let mut client = AcpClient {
        child,
        stdin,
        stdout: BufReader::new(stdout),
        next_id: 0,
        session_id: String::new(),
    };

    // initialize
    let resp = client
        .request("agent/initialize", json!({}))
        .await
        .context("acp initialize")?;
    if resp.get("error").is_some() {
        anyhow::bail!("acp initialize error: {resp}");
    }

    // session/new
    let resp = client
        .request("agent/session/new", json!({}))
        .await
        .context("acp session/new")?;
    client.session_id = resp
        .get("result")
        .and_then(|r| r.get("sessionId"))
        .and_then(Value::as_str)
        .context("acp session/new missing sessionId")?
        .to_string();

    Ok(client)
}

impl AcpClient {
    /// Send a JSON-RPC request and wait for its matching response.
    pub async fn request(&mut self, method: &str, params: Value) -> Result<Value> {
        self.next_id += 1;
        let id = self.next_id;
        let msg = json!({
            "jsonrpc": "2.0",
            "id": id,
            "method": method,
            "params": params
        });
        let mut line = serde_json::to_string(&msg)?;
        line.push('\n');
        self.stdin.write_all(line.as_bytes()).await?;
        self.stdin.flush().await?;

        // Read frames until the response with our id arrives.
        let mut line_buf = String::new();
        loop {
            line_buf.clear();
            let n = self.stdout.read_line(&mut line_buf).await?;
            if n == 0 {
                anyhow::bail!("acp child closed stdout while awaiting response {id}");
            }
            let Ok(frame) = serde_json::from_str::<Value>(line_buf.trim()) else {
                continue;
            };
            if frame.get("id").and_then(Value::as_u64) == Some(id) {
                return Ok(frame);
            }
            // Notifications with other ids are ignored here (the
            // prompt stream is collected by `prompt`).
        }
    }

    /// Send a prompt and collect the agent's text until the turn ends.
    /// Returns the accumulated assistant text.
    pub async fn prompt(&mut self, text: &str) -> Result<String> {
        let prompt = json!({
            "sessionId": self.session_id,
            "prompt": [{
                "type": "text",
                "text": text,
                "sessionId": self.session_id
            }]
        });
        // Fire the prompt request (we'll get the response after the
        // turn completes; meanwhile notifications stream in).
        self.next_id += 1;
        let id = self.next_id;
        let msg = json!({
            "jsonrpc": "2.0",
            "id": id,
            "method": "agent/session.prompt",
            "params": prompt
        });
        let mut line = serde_json::to_string(&msg)?;
        line.push('\n');
        self.stdin.write_all(line.as_bytes()).await?;
        self.stdin.flush().await?;

        let mut collected = String::new();
        let mut line_buf = String::new();
        loop {
            line_buf.clear();
            let n = self.stdout.read_line(&mut line_buf).await?;
            if n == 0 {
                anyhow::bail!("acp child closed stdout during prompt");
            }
            let Ok(frame) = serde_json::from_str::<Value>(line_buf.trim()) else {
                continue;
            };
            // Response to our prompt → turn finished.
            if frame.get("id").and_then(Value::as_u64) == Some(id) {
                let result = frame.get("result").cloned().unwrap_or(Value::Null);
                let stop = result
                    .get("stopReason")
                    .and_then(Value::as_str)
                    .unwrap_or("");
                if stop == "turn_complete" {
                    return Ok(collected.trim().to_string());
                }
                // Non-fatal: whatever was collected is what we have.
                return Ok(collected.trim().to_string());
            }
            // client/session.update notification → stream text.
            if frame.get("method").and_then(Value::as_str) == Some("client/session.update")
                && let Some(blocks) = frame
                    .get("params")
                    .and_then(|p| p.get("update"))
                    .and_then(|u| u.get("message"))
                    .and_then(|m| m.get("content"))
                    .and_then(|c| c.as_array())
            {
                for block in blocks {
                    if let Some(t) = block.get("text").and_then(Value::as_str) {
                        collected.push_str(t);
                    }
                }
            }
        }
    }

    /// Close the session politely (best-effort).
    pub async fn close(&mut self) -> Result<()> {
        let _ = self
            .request(
                "agent/session.close",
                json!({ "sessionId": self.session_id }),
            )
            .await;
        Ok(())
    }
}
