//! git tools: status / diff / log — read-only inspection helpers.

use async_trait::async_trait;
use serde_json::{Value, json};

use super::{Tool, ToolOutput};

/// Run `git <args>` in the current directory, capture combined output.
async fn git(args: &[&str]) -> anyhow::Result<String> {
    let output = tokio::process::Command::new("git")
        .args(args)
        .output()
        .await?;
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
        pub struct $name;

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
                match git(&$args).await {
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
