//! File tools: read_file (read-only) and write_file (mutating).
//!
//! read_file is safe; write_file lands in Phase-1 approval semantics later
//! (the agent loop will gate it once the approval gate lands).

use async_trait::async_trait;
use serde_json::{Value, json};
use tokio::fs;

use super::{Tool, ToolOutput};

pub struct ReadFileTool;

#[async_trait]
impl Tool for ReadFileTool {
    fn name(&self) -> &str {
        "read_file"
    }

    fn description(&self) -> &str {
        "Read a text file. Returns contents with line numbers. Supports optional line range (offset/limit)."
    }

    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "path": { "type": "string", "description": "Path to the file" },
                "offset": { "type": "integer", "description": "First line (1-based, default 1)" },
                "limit": { "type": "integer", "description": "Max lines (default 500)" }
            },
            "required": ["path"]
        })
    }

    async fn run(&self, args: &Value) -> anyhow::Result<ToolOutput> {
        let path = args
            .get("path")
            .and_then(Value::as_str)
            .ok_or_else(|| anyhow::anyhow!("read_file: missing string 'path'"))?;
        let offset = args
            .get("offset")
            .and_then(Value::as_u64)
            .unwrap_or(1)
            .max(1);
        let limit = args
            .get("limit")
            .and_then(Value::as_u64)
            .unwrap_or(500)
            .min(2000);

        let content = match fs::read_to_string(path).await {
            Ok(c) => c,
            Err(e) => {
                return Ok(ToolOutput {
                    text: format!("error reading {path}: {e}"),
                });
            }
        };

        let total = content.lines().count();
        let mut out = String::new();
        for (i, line) in content.lines().enumerate() {
            let lineno = i as u64 + 1;
            if lineno < offset || lineno >= offset + limit {
                continue;
            }
            out.push_str(&format!("{lineno}|{line}\n"));
        }
        if offset + limit <= total as u64 {
            out.push_str(&format!(
                "... ({} lines total, showing {}-{})",
                total,
                offset,
                offset + limit - 1
            ));
        } else if total as u64 > limit {
            out.push_str(&format!("... ({} lines total)", total));
        }
        Ok(ToolOutput { text: out })
    }
}

pub struct WriteFileTool;

#[async_trait]
impl Tool for WriteFileTool {
    fn name(&self) -> &str {
        "write_file"
    }

    fn description(&self) -> &str {
        "Write a text file, creating parent directories. Overwrites existing content. Mutating — approval-gated."
    }

    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "path": { "type": "string", "description": "Path to the file" },
                "content": { "type": "string", "description": "Full new content" }
            },
            "required": ["path", "content"]
        })
    }

    async fn run(&self, args: &Value) -> anyhow::Result<ToolOutput> {
        let path = args
            .get("path")
            .and_then(Value::as_str)
            .ok_or_else(|| anyhow::anyhow!("write_file: missing string 'path'"))?;
        let content = args
            .get("content")
            .and_then(Value::as_str)
            .ok_or_else(|| anyhow::anyhow!("write_file: missing string 'content'"))?;

        if let Some(parent) = std::path::Path::new(path).parent() {
            let _ = fs::create_dir_all(parent).await;
        }
        match fs::write(path, content).await {
            Ok(()) => Ok(ToolOutput {
                text: format!("wrote {} bytes to {path}", content.len()),
            }),
            Err(e) => Ok(ToolOutput {
                text: format!("error writing {path}: {e}"),
            }),
        }
    }
}
