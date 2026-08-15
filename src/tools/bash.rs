//! The `bash` tool — run a shell command and capture stdout/stderr.

use async_trait::async_trait;
use serde_json::{Value, json};

use super::{Tool, ToolOutput};

/// Default wall-clock budget for a shell command. Prevents a model
/// asking for `sleep 10000` (or a hung git fetch) from hanging the
/// agent loop forever. Scripted Lua tools already have a similar
/// timeout; the native tools must too.
pub const CMD_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(60);

/// Run a `tokio::process::Command` with a wall-clock budget.
///
/// On timeout the child is killed (via `kill_on_drop`) so no orphan
/// keeps running, and the caller gets `Ok(None)` — a *normal* tool
/// output explaining the timeout, not a hard error, so the agent loop
/// keeps going and the model can react.
pub async fn run_with_timeout(
    cmd: tokio::process::Command,
    budget: std::time::Duration,
) -> anyhow::Result<Option<std::process::Output>> {
    let mut cmd = cmd;
    cmd.kill_on_drop(true);
    match tokio::time::timeout(budget, cmd.output()).await {
        Ok(Ok(out)) => Ok(Some(out)),
        Ok(Err(e)) => Err(anyhow::anyhow!("{e}")),
        Err(_) => Ok(None),
    }
}

pub struct BashTool {
    shell: crate::agent::provider::ArcShell,
}

impl BashTool {
    pub fn new() -> Self {
        Self {
            shell: crate::agent::provider::default_shell(),
        }
    }

    /// Phase B: 缝隙注入 —— 宿主可换沙箱/远程/审计 shell 实现。
    #[allow(dead_code)]
    pub fn with_shell(shell: crate::agent::provider::ArcShell) -> Self {
        Self { shell }
    }
}

impl Default for BashTool {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl Tool for BashTool {
    fn name(&self) -> &str {
        "bash"
    }

    fn description(&self) -> &str {
        "Run a shell command in the project directory and capture stdout/stderr. Returns combined output. Commands are killed after 60s."
    }

    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "command": { "type": "string", "description": "The shell command to run" }
            },
            "required": ["command"]
        })
    }

    async fn run(&self, args: &Value) -> anyhow::Result<ToolOutput> {
        let command = args
            .get("command")
            .and_then(Value::as_str)
            .ok_or_else(|| anyhow::anyhow!("bash: missing string 'command'"))?;

        let mut cmd = tokio::process::Command::new("bash");
        cmd.arg("-c").arg(command);
        // Phase B: 执行走 ShellProvider 缝隙（默认 LocalShellProvider，行为与旧一致）。
        let output = match self.shell.run(cmd, CMD_TIMEOUT).await? {
            Some(o) => o,
            None => {
                return Ok(ToolOutput {
                    text: "[bash: command timed out after 60s — process killed]".into(),
                });
            }
        };

        let stdout = String::from_utf8_lossy(&output.stdout);
        let stderr = String::from_utf8_lossy(&output.stderr);
        let mut text = stdout.trim().to_string();
        if !stderr.trim().is_empty() {
            if !text.is_empty() {
                text.push('\n');
            }
            text.push_str(&format!("[stderr] {}", stderr.trim()));
        }
        if !output.status.success() {
            text.push_str(&format!(
                "\n[exit code {}]",
                output.status.code().unwrap_or(-1)
            ));
        }
        Ok(ToolOutput { text })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // Phase B: 缝隙注入生效 —— BashTool 走 ShellProvider 而非直接 run_with_timeout。
    #[tokio::test]
    async fn bash_tool_with_injected_provider_routes_through_seam() {
        use std::sync::atomic::Ordering;
        let calls = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let calls2 = calls.clone();
        let shell = crate::agent::provider::RecordingShell { calls: calls2 };

        let tool = BashTool::with_shell(std::sync::Arc::new(shell));
        let out = tool.run(&json!({ "command": "echo hi" })).await.unwrap();

        assert_eq!(out.text, "hi");
        assert_eq!(
            calls.load(Ordering::SeqCst),
            1,
            "command must route through the injected provider"
        );
    }

    #[tokio::test]
    async fn run_with_timeout_returns_output_for_fast_command() {
        let mut cmd = tokio::process::Command::new("echo");
        cmd.arg("hi");
        let out = run_with_timeout(cmd, std::time::Duration::from_secs(5))
            .await
            .unwrap();
        let out = out.expect("fast command must not time out");
        assert_eq!(String::from_utf8_lossy(&out.stdout).trim(), "hi");
    }

    #[tokio::test]
    async fn run_with_timeout_kills_slow_command() {
        let mut cmd = tokio::process::Command::new("bash");
        cmd.arg("-c").arg("sleep 5");
        let out = run_with_timeout(cmd, std::time::Duration::from_millis(300))
            .await
            .unwrap();
        assert!(out.is_none(), "slow command must be killed, got {out:?}");
    }
}
