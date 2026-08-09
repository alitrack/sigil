//! Scripted tools: Lua files in `~/.sigil/tools/` (or `SIGIL_TOOLS_DIR`)
//! become callable tools. Each file exports a function that receives the
//! tool arguments (a Lua table) and returns a result (string or table).
//!
//! This is the "evolve by scripting" layer: the model itself can write a
//! Lua tool, drop it in the tools dir, and it becomes available next
//! turn — no recompile. (Phase-3 self-evolution harness builds on this.)
//!
//! Example tool (`~/.sigil/tools/date.lua`):
//! ```lua
//! return function(args)
//!     return os.date("%Y-%m-%d %H:%M:%S")
//! end
//! ```

use std::path::{Path, PathBuf};
use std::sync::Arc;

use anyhow::{Context, Result};
use async_trait::async_trait;
use serde_json::{Value, json};
use tokio::sync::Mutex;

use crate::lua::LuaRuntime;
use crate::tools::{Tool, ToolOutput};

/// Discover scripted tools in a directory. Each `.lua` file whose name
/// starts with a letter becomes a tool named after the file stem.
pub fn discover(dir: &Path) -> Vec<PathBuf> {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return Vec::new();
    };
    entries
        .filter_map(|e| e.ok())
        .map(|e| e.path())
        .filter(|p| {
            p.extension().map(|ext| ext == "lua").unwrap_or(false)
                && p.file_stem()
                    .and_then(|s| s.to_str())
                    .map(|s| s.chars().next().is_some_and(|c| c.is_ascii_alphabetic()))
                    .unwrap_or(false)
        })
        .collect()
}

pub fn default_tools_dir() -> PathBuf {
    dirs::config_dir()
        .unwrap_or_else(|| PathBuf::from("~"))
        .join("sigil")
        .join("tools")
}

/// A tool implemented in Lua.
pub struct LuaTool {
    name: String,
    path: PathBuf,
    lua: Arc<Mutex<LuaRuntime>>,
    description: String,
    /// JSON Schema for arguments (optional, from `--schema` first line comment).
    schema: Value,
}

impl LuaTool {
    pub fn new(name: &str, path: &Path, lua: Arc<Mutex<LuaRuntime>>) -> Result<Self> {
        // Optional description + arg schema from the file's first-line
        // comment: `-- desc: does X` and `-- schema: {...}`.
        let mut description = format!("Lua scripted tool: {name}");
        let mut schema = json!({ "type": "object", "properties": {} });
        if let Ok(src) = std::fs::read_to_string(path) {
            for line in src.lines().take(8) {
                let line = line.trim_start();
                if let Some(rest) = line.strip_prefix("-- desc:") {
                    description = rest.trim().to_string();
                } else if let Some(rest) = line.strip_prefix("-- schema:")
                    && let Ok(v) = serde_json::from_str::<Value>(rest.trim())
                {
                    // A model-written schema must be a JSON object schema;
                    // a malformed one (e.g. missing the top-level `type`)
                    // would 400 every LLM call that advertises this tool —
                    // fall back to the safe default instead.
                    if v.get("type").and_then(Value::as_str) == Some("object") {
                        schema = v;
                    }
                }
            }
        }
        Ok(Self {
            name: name.to_string(),
            path: path.to_owned(),
            lua,
            description,
            schema,
        })
    }
}

#[async_trait]
impl Tool for LuaTool {
    fn name(&self) -> &str {
        &self.name
    }

    fn description(&self) -> &str {
        &self.description
    }

    fn parameters(&self) -> Value {
        self.schema.clone()
    }

    async fn run(&self, args: &Value) -> Result<ToolOutput> {
        let lua = self.lua.clone();
        let path = self.path.clone();
        let args = args.clone();
        let name = self.name.clone();
        // Run the Lua script on a blocking thread — Lua execution is
        // synchronous and cannot yield back to async.
        let text = tokio::task::spawn_blocking(move || {
            let lua = lua.blocking_lock();
            lua.run_file(path.to_str().unwrap_or(&name), &args.to_string())
        })
        .await
        .context("lua tool thread")??;

        // A string result is JSON-serialized with quotes by the runtime;
        // unwrap it so the model sees the plain text.
        let text = serde_json::from_str::<String>(&text).unwrap_or(text);

        Ok(ToolOutput { text })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn lua_runtime() -> Arc<Mutex<LuaRuntime>> {
        Arc::new(Mutex::new(LuaRuntime::new().unwrap()))
    }

    fn tmp_file(name: &str, content: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join("sigil-scripted-tests");
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join(name);
        std::fs::write(&path, content).unwrap();
        path
    }

    #[test]
    fn malformed_schema_falls_back_to_default() {
        // A model-written `-- schema:` missing the top-level `type` must
        // not leak into the LLM advert (it would 400 every call that
        // advertises this tool). It falls back to the safe default.
        let path = tmp_file(
            "bad_schema.lua",
            "-- schema: {\"name\":\"greet\",\"parameters\":{\"type\":\"object\"}}\nreturn function(args) return 'hi' end\n",
        );
        let tool = LuaTool::new("bad_schema", &path, lua_runtime()).unwrap();
        let params = tool.parameters();
        assert_eq!(params.get("type").and_then(Value::as_str), Some("object"));
        assert!(params.get("properties").is_some());
    }

    #[test]
    fn valid_schema_is_kept() {
        let path = tmp_file(
            "good_schema.lua",
            "-- schema: {\"type\":\"object\",\"properties\":{\"x\":{\"type\":\"string\"}}}\nreturn function(args) return 'hi' end\n",
        );
        let tool = LuaTool::new("good_schema", &path, lua_runtime()).unwrap();
        let params = tool.parameters();
        assert_eq!(params.get("type").and_then(Value::as_str), Some("object"));
        assert!(params.get("properties").is_some());
    }
}
