//! Evolution tools: let the model extend its own toolset.
//!
//! `create_lua_tool` — the model writes a Lua tool, sigil saves it to
//! the tools dir and hot-registers it. Next turn (and every turn after)
//! the new tool is available to every tool-calling model.
//!
//! This is the minimal AHE (Agentic Harness Evolution) loop: the agent
//! observes a need → writes a tool → the harness grows. The harness
//! bundle (system prompt, skills) is the second mutation surface.

use async_trait::async_trait;
use serde_json::{Value, json};

use crate::tools::{Tool, ToolOutput};

/// Runtime hook to hot-register a new scripted tool (used by the agent
/// loop's evolution path).
pub type RegisterHook = Box<dyn Fn(&str, std::path::PathBuf) + Send + Sync>;

/// Tool that registers new Lua tools at runtime.
pub struct CreateLuaToolTool {
    /// Where scripted tools live (hot-loaded by the registry).
    pub tools_dir: std::path::PathBuf,
    /// Runtime handle to hot-register tools without restart.
    pub register: RegisterHook,
}

#[async_trait]
impl Tool for CreateLuaToolTool {
    fn name(&self) -> &str {
        "create_lua_tool"
    }

    fn description(&self) -> &str {
        "Create a new Lua scripted tool: write Lua code (with optional -- desc: and -- schema: header comments), sigil saves it to the tools dir and makes it callable by name from the next turn on. Returns the new tool's name. Use when a task needs a repeatable operation that the built-in tools don't cover."
    }

    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "name": { "type": "string", "description": "Tool name (alphanumeric + underscore; becomes the Lua file name)" },
                "code": { "type": "string", "description": "Full Lua source. Must return a function(args) -> result. Optional header comments: -- desc: ... and -- schema: {...}" }
            },
            "required": ["name", "code"]
        })
    }

    async fn run(&self, args: &Value) -> anyhow::Result<ToolOutput> {
        let name = args
            .get("name")
            .and_then(Value::as_str)
            .ok_or_else(|| anyhow::anyhow!("create_lua_tool: missing string 'name'"))?;
        let code = args
            .get("code")
            .and_then(Value::as_str)
            .ok_or_else(|| anyhow::anyhow!("create_lua_tool: missing string 'code'"))?;

        // Validate the tool name: alphanumeric + underscore, no slashes.
        let safe: String = name
            .chars()
            .filter(|c| c.is_ascii_alphanumeric() || *c == '_')
            .collect();
        if safe.is_empty() {
            return Ok(ToolOutput {
                text: "error: invalid tool name (must contain letters/digits/underscore)".into(),
            });
        }

        std::fs::create_dir_all(&self.tools_dir)?;
        let path = self.tools_dir.join(format!("{safe}.lua"));
        std::fs::write(&path, code)?;

        // Hot-register so it's callable immediately (next tool call).
        (self.register)(&safe, path.clone());

        Ok(ToolOutput {
            text: format!(
                "created Lua tool '{safe}' at {} — callable from the next turn (and persisted for future sessions).",
                path.display()
            ),
        })
    }
}
