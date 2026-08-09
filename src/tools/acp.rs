//! ACP delegation tool: drive an external ACP server (another agent,
//! an editor, codex, ...) from within the agent loop.
//!
//! `acp_call` spawns the configured command as an ACP server over
//! stdio, initializes a session, sends the task text as a prompt, and
//! collects the external agent's reply. This is cross-process
//! delegation — the counterpart of `delegate` (in-process) and MCP
//! (tool-level).

use async_trait::async_trait;
use serde_json::{Value, json};

use crate::tools::{Tool, ToolOutput};

/// Tool that delegates a task to an external ACP server process.
pub struct AcpCallTool {
    /// Command to spawn as the ACP server (e.g. "codex", "sigil acp").
    pub command: String,
    /// Extra args for the command.
    pub args: Vec<String>,
}

#[async_trait]
impl Tool for AcpCallTool {
    fn name(&self) -> &str {
        "acp_call"
    }

    fn description(&self) -> &str {
        "Delegate a task to an external ACP agent (Agent Client Protocol server) — e.g. another sigil, codex, or an editor agent. Spawns the configured command, sends the task as a prompt, and returns the external agent's reply text. Use for offloading work to a different agent/toolchain."
    }

    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "task": {
                    "type": "string",
                    "description": "The task to send to the external agent"
                },
                "command": {
                    "type": "string",
                    "description": "Optional override: command to spawn as the ACP server (default: sigil acp)"
                }
            },
            "required": ["task"]
        })
    }

    async fn run(&self, args: &Value) -> anyhow::Result<ToolOutput> {
        let task = args
            .get("task")
            .and_then(Value::as_str)
            .filter(|t| !t.is_empty())
            .ok_or_else(|| anyhow::anyhow!("acp_call requires non-empty 'task'"))?;

        // Command override (e.g. a full path) or the configured default.
        let (command, extra_args) = match args.get("command").and_then(Value::as_str) {
            Some(c) if !c.is_empty() => {
                // Allow "codex a b" style: split on whitespace.
                let mut parts = c.split_whitespace();
                let head = parts.next().unwrap_or(&self.command);
                let rest: Vec<String> = parts.map(String::from).collect();
                let mut full = self.args.clone();
                full.splice(0..0, rest);
                (head.to_string(), full)
            }
            _ => (self.command.clone(), self.args.clone()),
        };

        let mut client = crate::acp::client::connect(&command, &extra_args)
            .await
            .map_err(|e| anyhow::anyhow!("acp_call connect: {e}"))?;

        let reply = client
            .prompt(task)
            .await
            .map_err(|e| anyhow::anyhow!("acp_call prompt: {e}"))?;
        let _ = client.close().await;

        let text = if reply.is_empty() {
            "(external agent returned no text)".to_string()
        } else {
            reply
        };
        Ok(ToolOutput { text })
    }
}
