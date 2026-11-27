//! The `bash` tool — run a shell command and capture stdout/stderr.

use async_trait::async_trait;
use serde_json::{Value, json};

use super::{Tool, ToolOutput};

/// Default wall-clock budget for a shell command. Prevents a model
/// asking for `sleep 10000` (or a hung git fetch) from hanging the
/// agent loop forever. Scripted Lua tools already have a similar
/// timeout; the native tools must too.
pub const CMD_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(60);

/// Effective per-command wall-clock budget: `SIGIL_BASH_TIMEOUT_SECS`
/// (positive integer seconds) overrides the built-in 60s default.
/// Non-positive or invalid values fall back to the default — a
/// misconfigured env var must not break the bash tool.
pub fn cmd_timeout() -> std::time::Duration {
    std::env::var_os("SIGIL_BASH_TIMEOUT_SECS")
        .and_then(|v| v.to_string_lossy().trim().parse::<u64>().ok())
        .filter(|s| *s > 0)
        .map(std::time::Duration::from_secs)
        .unwrap_or(CMD_TIMEOUT)
}

/// Run a `tokio::process::Command` with a wall-clock budget.
///
/// On timeout the child is killed (via `kill_on_drop`) so no orphan
/// keeps running, and the caller gets `Ok(None)` — a *normal* tool
/// output explaining the timeout, not a hard error, so the agent loop
/// keeps going and the model can react.
///
/// Phase B：已被 `agent::provider::LocalShellProvider` 取代（执行走缝隙），
/// 保留为兼容 API（测试与旧引用）。
#[allow(dead_code)]
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
    description: String,
    timeout: std::time::Duration,
}

impl BashTool {
    pub fn new() -> Self {
        Self::with_timeout(cmd_timeout())
    }

    /// Explicit budget (config surface for the `SIGIL_BASH_TIMEOUT_SECS`
    /// override; tests and future TOML wiring use this).
    pub fn with_timeout(timeout: std::time::Duration) -> Self {
        Self {
            shell: crate::agent::provider::default_shell(),
            description: tool_description_for(timeout),
            timeout,
        }
    }

    /// Phase B: 缝隙注入 —— 宿主可换沙箱/远程/审计 shell 实现。
    #[allow(dead_code)]
    pub fn with_shell(shell: crate::agent::provider::ArcShell) -> Self {
        Self {
            shell,
            description: tool_description_for(CMD_TIMEOUT),
            timeout: CMD_TIMEOUT,
        }
    }

    /// 缝隙 + 超时组合（测试用）。
    #[allow(dead_code)]
    pub fn with_shell_and_timeout(
        shell: crate::agent::provider::ArcShell,
        timeout: std::time::Duration,
    ) -> Self {
        Self {
            shell,
            description: tool_description_for(timeout),
            timeout,
        }
    }
}

/// Model-facing description, honest about the shell's platform. On
/// Windows the child is a Linux bash (WSL interop or Git Bash): its
/// working directory has a `/mnt/<drive>/...` form, so a path copied
/// from bash output into read_file/write_file fails unless the model
/// knows the mapping.
fn tool_description_for(timeout: std::time::Duration) -> String {
    let secs = timeout.as_secs();
    #[cfg_attr(not(windows), allow(unused_mut))] // only Windows pushes the extra note
    let mut d = format!(
        "Run a shell command in the project directory and capture stdout/stderr.          Returns combined output. Commands are killed after {secs}s.          Prefer relative paths — they resolve against the session working directory.",
    );
    #[cfg(windows)]
    d.push_str(
        " Note: on Windows this runs in a Linux bash (WSL or Git Bash); the working          directory appears there in /mnt/<drive>/... form (e.g. D:\\proj → /mnt/d/proj).          Inside bash use that Linux form for absolute paths; read_file/write_file take          the Windows form of the same files.",
    );
    d
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
        &self.description
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
        crate::agent::provider::apply_working_directory(&mut cmd);
        // Phase B: 执行走 ShellProvider 缝隙（默认 LocalShellProvider，行为与旧一致）。
        let output = match self.shell.run(cmd, self.timeout).await? {
            Some(o) => o,
            None => {
                return Ok(ToolOutput {
                    text: format!(
                        "[bash: command timed out after {}s — process killed]",
                        self.timeout.as_secs()
                    ),
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

    #[test]
    fn description_is_platform_honest() {
        // with_timeout(CMD_TIMEOUT) keeps this deterministic: BashTool::new()
        // reads SIGIL_BASH_TIMEOUT_SECS, which a parallel test may set.
        let tool = BashTool::with_timeout(CMD_TIMEOUT);
        let d = tool.description();
        assert!(d.contains("60s"), "{d}");
        assert!(d.contains("relative paths"), "{d}");
        #[cfg(windows)]
        assert!(
            d.contains("/mnt/"),
            "Windows description must mention the Linux path view: {d}"
        );
        #[cfg(not(windows))]
        assert!(
            !d.contains("/mnt/"),
            "non-Windows description has no WSL note: {d}"
        );
    }

    /// Single test owns the env var for its whole body: splitting the
    /// cases across parallel tests would race on the process env.
    #[test]
    fn cmd_timeout_env_semantics() {
        // Guarded: a host that exports SIGIL_BASH_TIMEOUT_SECS would
        // shadow the default.
        if std::env::var_os("SIGIL_BASH_TIMEOUT_SECS").is_some() {
            eprintln!("skipping: SIGIL_BASH_TIMEOUT_SECS is set in this environment");
            return;
        }
        assert_eq!(
            cmd_timeout(),
            CMD_TIMEOUT,
            "unset env falls back to the default"
        );
        unsafe { std::env::set_var("SIGIL_BASH_TIMEOUT_SECS", "120") };
        assert_eq!(cmd_timeout(), std::time::Duration::from_secs(120));
        unsafe { std::env::set_var("SIGIL_BASH_TIMEOUT_SECS", "0") };
        assert_eq!(cmd_timeout(), CMD_TIMEOUT, "0 falls back to the default");
        unsafe { std::env::set_var("SIGIL_BASH_TIMEOUT_SECS", "garbage") };
        assert_eq!(
            cmd_timeout(),
            CMD_TIMEOUT,
            "invalid values fall back to the default"
        );
        unsafe { std::env::remove_var("SIGIL_BASH_TIMEOUT_SECS") };
    }

    #[test]
    fn custom_timeout_is_announced_in_the_description() {
        let tool = BashTool::with_timeout(std::time::Duration::from_secs(120));
        let d = tool.description();
        assert!(d.contains("120s"), "{d}");
    }

    #[tokio::test]
    async fn custom_timeout_is_the_executed_budget() {
        struct BudgetRecorder {
            seen: std::sync::Arc<std::sync::Mutex<Option<std::time::Duration>>>,
        }
        let seen = std::sync::Arc::new(std::sync::Mutex::new(None));
        let rec = BudgetRecorder { seen: seen.clone() };
        #[async_trait::async_trait]
        impl crate::agent::provider::ShellProvider for BudgetRecorder {
            async fn run(
                &self,
                _cmd: tokio::process::Command,
                budget: std::time::Duration,
            ) -> anyhow::Result<Option<std::process::Output>> {
                *self.seen.lock().unwrap() = Some(budget);
                Ok(Some(std::process::Output {
                    status: std::process::ExitStatus::default(),
                    stdout: b"ok\n".to_vec(),
                    stderr: Vec::new(),
                }))
            }
        }
        let tool = BashTool::with_shell_and_timeout(
            std::sync::Arc::new(rec),
            std::time::Duration::from_secs(120),
        );
        let out = tool.run(&json!({ "command": "true" })).await.unwrap();
        assert_eq!(out.text, "ok");
        assert_eq!(
            *seen.lock().unwrap(),
            Some(std::time::Duration::from_secs(120)),
            "the configured budget must reach the executor"
        );
    }

    #[tokio::test]
    async fn timeout_message_uses_the_configured_budget() {
        struct TimeoutShell;
        #[async_trait::async_trait]
        impl crate::agent::provider::ShellProvider for TimeoutShell {
            async fn run(
                &self,
                _cmd: tokio::process::Command,
                _budget: std::time::Duration,
            ) -> anyhow::Result<Option<std::process::Output>> {
                Ok(None)
            }
        }
        let tool = BashTool::with_shell_and_timeout(
            std::sync::Arc::new(TimeoutShell),
            std::time::Duration::from_secs(120),
        );
        let out = tool.run(&json!({ "command": "sleep 999" })).await.unwrap();
        assert_eq!(
            out.text,
            "[bash: command timed out after 120s — process killed]"
        );
    }
}
