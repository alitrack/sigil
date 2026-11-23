//! File tools: read_file (read-only) and write_file (mutating).
//!
//! read_file is safe; write_file lands in Phase-1 approval semantics later
//! (the agent loop will gate it once the approval gate lands).

use async_trait::async_trait;
use serde_json::{Value, json};

use super::{Tool, ToolOutput};

pub struct ReadFileTool {
    fs: crate::agent::provider::ArcFs,
}

impl ReadFileTool {
    pub fn new() -> Self {
        Self {
            fs: crate::agent::provider::default_fs(),
        }
    }

    /// Phase B: 缝隙注入 —— 宿主可换沙箱/远程/审计 fs 实现。
    #[allow(dead_code)]
    pub fn with_fs(fs: crate::agent::provider::ArcFs) -> Self {
        Self { fs }
    }
}

impl Default for ReadFileTool {
    fn default() -> Self {
        Self::new()
    }
}

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

        // Phase B: 文件 IO 走 FsProvider 缝隙（默认 LocalFsProvider）。
        let content = match self
            .fs
            .read_to_string(&crate::agent::provider::resolve_path(std::path::Path::new(
                path,
            )))
            .await
        {
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

pub struct WriteFileTool {
    fs: crate::agent::provider::ArcFs,
}

impl WriteFileTool {
    pub fn new() -> Self {
        Self {
            fs: crate::agent::provider::default_fs(),
        }
    }

    /// Phase B: 缝隙注入 —— 宿主可换沙箱/远程/审计 fs 实现。
    #[allow(dead_code)]
    pub fn with_fs(fs: crate::agent::provider::ArcFs) -> Self {
        Self { fs }
    }
}

impl Default for WriteFileTool {
    fn default() -> Self {
        Self::new()
    }
}

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
                "content": {
                    "anyOf": [{"type": "string"}, {"type": "object"}, {"type": "array"}],
                    "description": "Full new text content. For .json files only, a JSON object or array is also accepted and serialized as JSON. Other files require a string."
                }
            },
            "required": ["path", "content"]
        })
    }

    async fn run(&self, args: &Value) -> anyhow::Result<ToolOutput> {
        let path = args
            .get("path")
            .and_then(Value::as_str)
            .ok_or_else(|| anyhow::anyhow!("write_file: missing string 'path'"))?;
        let value = args
            .get("content")
            .ok_or_else(|| anyhow::anyhow!("write_file: missing 'content'"))?;
        let content = match value {
            Value::String(text) => std::borrow::Cow::Borrowed(text.as_str()),
            Value::Object(_) | Value::Array(_)
                if std::path::Path::new(path).extension().is_some_and(|ext| {
                    ext.to_str()
                        .is_some_and(|ext| ext.eq_ignore_ascii_case("json"))
                }) =>
            {
                std::borrow::Cow::Owned(serde_json::to_string_pretty(value)?)
            }
            _ => anyhow::bail!(
                "write_file: content must be a string; JSON objects/arrays are supported only for .json files"
            ),
        };

        // Write-scope policy: a model-directed write may not leave the
        // workspace / temp / sigil-state roots. Without this, `write_file`
        // (which the approval gate allows unconditionally) is an arbitrary
        // absolute-path write with the user's privileges — the shortest path
        // from a prompt injection to a persistent shell.
        let resolved = crate::safety::authorize_write(std::path::Path::new(path))
            .map_err(|e| anyhow::anyhow!("{e}"))?;
        if let Some(parent) = resolved.parent() {
            let _ = self.fs.create_dir_all(parent).await;
        }
        // Phase B: 文件 IO 走 FsProvider 缝隙（默认 LocalFsProvider）。
        match self.fs.write(&resolved, &content).await {
            Ok(()) => Ok(ToolOutput {
                text: format!("wrote {} bytes to {path}", content.len()),
            }),
            Err(e) => Ok(ToolOutput {
                text: format!("error writing {path}: {e}"),
            }),
        }
    }
}

#[cfg(test)]
mod structured_write_tests {
    use super::*;

    #[tokio::test]
    async fn structured_json_content_roundtrips_and_text_stays_verbatim() {
        let dir = std::env::temp_dir().join(format!(
            "sigil-json-write-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let tool = WriteFileTool::new();
        let path = dir.join("settings.JSON");
        let value =
            json!({"enabled": true, "port": 9301, "banner": "保留说明", "items": [1, null]});
        tool.run(&json!({"path":path,"content":value}))
            .await
            .unwrap();
        assert_eq!(
            serde_json::from_str::<Value>(&std::fs::read_to_string(&path).unwrap()).unwrap(),
            value
        );
        let array_path = dir.join("items.json");
        let array = json!([1, true, {"nested": null}]);
        tool.run(&json!({"path": array_path, "content": array}))
            .await
            .unwrap();
        assert_eq!(
            serde_json::from_str::<Value>(&std::fs::read_to_string(array_path).unwrap()).unwrap(),
            array
        );
        let text = dir.join("literal.txt");
        tool.run(&json!({"path":text,"content":"{ raw text }\n"}))
            .await
            .unwrap();
        assert_eq!(std::fs::read_to_string(text).unwrap(), "{ raw text }\n");
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[tokio::test]
    async fn structured_content_cannot_overwrite_non_json_files() {
        let path =
            std::env::temp_dir().join(format!("sigil-json-reject-{}.txt", std::process::id()));
        std::fs::write(&path, "original").unwrap();
        let tool = WriteFileTool::new();
        assert!(
            tool.run(&json!({"path":path,"content":{"x":1}}))
                .await
                .is_err()
        );
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "original");
        assert!(
            tool.run(&json!({"path":path.with_extension("json"),"content":null}))
                .await
                .is_err()
        );
        std::fs::remove_file(path).unwrap();
    }
}
