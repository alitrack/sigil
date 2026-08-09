//! The `bash` tool — run a shell command and capture stdout/stderr.

use async_trait::async_trait;
use serde_json::{Value, json};

use super::{Tool, ToolOutput};

pub struct BashTool;

#[async_trait]
impl Tool for BashTool {
    fn name(&self) -> &str {
        "bash"
    }

    fn description(&self) -> &str {
        "Run a shell command in the project directory and capture stdout/stderr. Returns combined output."
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

        let output = tokio::process::Command::new("bash")
            .arg("-c")
            .arg(command)
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
        if !output.status.success() {
            text.push_str(&format!(
                "\n[exit code {}]",
                output.status.code().unwrap_or(-1)
            ));
        }
        Ok(ToolOutput { text })
    }
}
