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
//!     return host.date("%Y-%m-%d %H:%M:%S")
//! end
//! ```
//!
//! Sandbox: the `io` / `os` standard libraries are NOT available —
//! scripts use `host` helpers (read_file, write_file, env, list_dir,
//! cwd, json_encode, json_decode, date) for anything privileged, and
//! `require` only loads built-in modules (ffi, string, table, math,
//! bit, jit, coroutine).

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

/// Sanitize a model-written `-- schema:` value before it is advertised
/// to the LLM endpoint.
///
/// The endpoint requires a JSON object schema; anything else 400s every
/// call that advertises the tool. Instead of rejecting the whole schema
/// outright, we repair what we can:
/// - top-level must be an object, else fall back to the safe default;
/// - a missing top-level `type` is legal JSON Schema — default to
///   `"object"` (the endpoint assumes object parameters);
/// - a `type` that is not `"object"` (e.g. `"string"`) falls back to the
///   safe default (a non-object parameter schema is not a valid tool);
/// - `properties` / `required` that are not an object / array are
///   dropped so a malformed substructure never reaches the endpoint.
fn sanitize_schema(v: Value) -> Value {
    let mut obj = match v.as_object() {
        Some(o) => o.clone(),
        None => return json!({ "type": "object", "properties": {} }),
    };
    match obj.get("type").and_then(Value::as_str) {
        // Missing type: legal, default to object.
        None => {
            obj.insert("type".into(), json!("object"));
        }
        Some("object") => {}
        // A non-object top-level type is not a valid tool schema.
        Some(_) => return json!({ "type": "object", "properties": {} }),
    }
    if let Some(props) = obj.get("properties")
        && !props.is_object()
    {
        obj.remove("properties");
    }
    if let Some(req) = obj.get("required")
        && !req.is_array()
    {
        obj.remove("required");
    }
    serde_json::Value::Object(obj)
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
                    // would 400 every LLM call that advertises this tool.
                    // Sanitize instead of rejecting outright: keep valid
                    // object schemas (a missing `type` is legal JSON
                    // Schema — default it to "object"), and drop invalid
                    // substructures (e.g. `properties` that is not an
                    // object) so they never reach the LLM endpoint.
                    schema = sanitize_schema(v);
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
        // A model-written `-- schema:` that is not an object schema must
        // not leak into the LLM advert (it would 400 every call that
        // advertises this tool). It falls back to the safe default.
        let path = tmp_file(
            "bad_schema.lua",
            "-- schema: [1,2,3]\nreturn function(args) return 'hi' end\n",
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
        assert_eq!(
            params.pointer("/properties/x/type").and_then(Value::as_str),
            Some("string")
        );
    }

    #[test]
    fn schema_without_type_defaults_to_object() {
        // `-- schema: {"properties": {...}}` without a top-level `type`
        // is legal JSON Schema — the properties must be kept, with the
        // type defaulted to "object" (the endpoint assumes object
        // parameters). Previously this valid schema was dropped.
        let path = tmp_file(
            "notype_schema.lua",
            "-- schema: {\"properties\":{\"x\":{\"type\":\"string\"}}}\nreturn function(args) return 'hi' end\n",
        );
        let tool = LuaTool::new("notype_schema", &path, lua_runtime()).unwrap();
        let params = tool.parameters();
        assert_eq!(params.get("type").and_then(Value::as_str), Some("object"));
        assert_eq!(
            params.pointer("/properties/x/type").and_then(Value::as_str),
            Some("string")
        );
    }

    #[test]
    fn malformed_substructures_are_dropped() {
        // `properties` that is not an object (e.g. a string) must not
        // reach the LLM endpoint — it is dropped, the rest is kept.
        let path = tmp_file(
            "badprops_schema.lua",
            "-- schema: {\"type\":\"object\",\"properties\":\"nope\"}\nreturn function(args) return 'hi' end\n",
        );
        let tool = LuaTool::new("badprops_schema", &path, lua_runtime()).unwrap();
        let params = tool.parameters();
        assert_eq!(params.get("type").and_then(Value::as_str), Some("object"));
        assert!(params.get("properties").is_none());

        // Same for `required` that is not an array.
        let path = tmp_file(
            "badreq_schema.lua",
            "-- schema: {\"type\":\"object\",\"properties\":{\"x\":{\"type\":\"string\"}},\"required\":\"x\"}\nreturn function(args) return 'hi' end\n",
        );
        let tool = LuaTool::new("badreq_schema", &path, lua_runtime()).unwrap();
        let params = tool.parameters();
        assert_eq!(params.get("type").and_then(Value::as_str), Some("object"));
        assert!(params.get("required").is_none());
        assert!(params.get("properties").is_some());
    }

    #[test]
    fn non_object_top_level_type_falls_back() {
        // A schema typed as something other than an object (e.g. a
        // string) is not a valid tool parameter schema.
        let path = tmp_file(
            "string_schema.lua",
            "-- schema: {\"type\":\"string\"}\nreturn function(args) return 'hi' end\n",
        );
        let tool = LuaTool::new("string_schema", &path, lua_runtime()).unwrap();
        let params = tool.parameters();
        assert_eq!(params.get("type").and_then(Value::as_str), Some("object"));
        assert!(params.get("properties").is_some());
    }
}
