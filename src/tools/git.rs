//! git tools: status / diff / log — read-only inspection helpers.

use async_trait::async_trait;
use serde_json::{Value, json};

use super::{Tool, ToolOutput};

/// Run `git <args>` in the current directory, capture combined output.
/// Bounded by the same 60s budget as `bash` — a hung git fetch can't
/// hang the agent loop.
async fn git_with_shell(
    shell: &crate::agent::provider::ArcShell,
    args: &[&str],
) -> anyhow::Result<String> {
    let mut cmd = tokio::process::Command::new("git");
    cmd.args(args);
    crate::agent::provider::apply_working_directory(&mut cmd);
    // Phase B: 执行走 ShellProvider 缝隙（默认 LocalShellProvider）。
    let output = match shell.run(cmd, super::bash::CMD_TIMEOUT).await? {
        Some(o) => o,
        None => return Ok("(git: command timed out after 60s — killed)".into()),
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
    Ok(text)
}

macro_rules! git_tool {
    ($name:ident, $tool_name:literal, $desc:literal, $args:expr) => {
        pub struct $name {
            shell: crate::agent::provider::ArcShell,
        }

        impl $name {
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

        impl Default for $name {
            fn default() -> Self {
                Self::new()
            }
        }

        #[async_trait]
        impl Tool for $name {
            fn name(&self) -> &str {
                $tool_name
            }

            fn description(&self) -> &str {
                $desc
            }

            fn parameters(&self) -> Value {
                json!({ "type": "object", "properties": {} })
            }

            async fn run(&self, _args: &Value) -> anyhow::Result<ToolOutput> {
                match git_with_shell(&self.shell, &$args).await {
                    Ok(text) => Ok(ToolOutput { text }),
                    Err(e) => Ok(ToolOutput { text: format!("git error: {e}") }),
                }
            }
        }
    };
}

git_tool!(
    GitStatusTool,
    "git_status",
    "Show `git status --short` (working tree changes).",
    ["status", "--short"]
);
git_tool!(
    GitDiffTool,
    "git_diff",
    "Show unstaged diff (`git diff --stat`).",
    ["diff", "--stat"]
);
git_tool!(
    GitLogTool,
    "git_log",
    "Show recent commit history (`git log --oneline -20`).",
    ["log", "--oneline", "-20"]
);

#[cfg(test)]
mod tests {
    use super::*;

    // Phase B: git 工具走 ShellProvider 缝隙（注入 provider 不真跑 git）。
    #[tokio::test]
    async fn git_tool_with_injected_provider_routes_through_seam() {
        use std::sync::atomic::Ordering;
        let calls = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let calls2 = calls.clone();
        let shell = crate::agent::provider::RecordingShell { calls: calls2 };

        let tool = GitStatusTool::with_shell(std::sync::Arc::new(shell));
        let out = tool.run(&json!({})).await.unwrap();

        // RecordingShell 返回 "hi\n" —— 证明输出来自注入 provider 而非真实 git
        assert_eq!(out.text, "hi");
        assert_eq!(calls.load(Ordering::SeqCst), 1);
    }
}
