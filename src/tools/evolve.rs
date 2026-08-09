//! Evolution tools: let the model extend its own toolset and improve
//! its own harness.
//!
//! `create_lua_tool` — the model writes a Lua tool, sigil saves it to
//! the tools dir and hot-registers it. Next turn (and every turn after)
//! the new tool is available to every tool-calling model.
//!
//! `improve_harness` — the model edits its own instruction layer
//! (system prompt / skills), the AHE loop's second mutation surface.
//!
//! This is the minimal AHE (Agentic Harness Evolution) loop: the agent
//! observes a need → writes a tool or updates its prompt → the harness
//! grows. All persistent, all recompile-free.

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
        "Create a new Lua scripted tool: write Lua code (with optional -- desc: and -- schema: header comments), sigil saves it to the tools dir and makes it callable by name from the next turn on. Returns the new tool's name. Use when a task needs a repeatable operation that the built-in tools don't cover. Your Lua code has a global `host` table with safe helpers: host.read_file(path), host.write_file(path, content), host.env(key), host.list_dir(dir), host.cwd(), host.json_encode(value), host.json_decode(text). FFI is available (require('ffi')) for calling system C libraries."
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
            .filter(|n| !n.is_empty())
            .ok_or_else(|| anyhow::anyhow!("create_lua_tool requires a non-empty 'name'"))?;
        let code = args
            .get("code")
            .and_then(Value::as_str)
            .filter(|c| !c.is_empty())
            .ok_or_else(|| anyhow::anyhow!("create_lua_tool requires non-empty 'code'"))?;

        // Sanitize the name into a safe file stem (drop unsafe chars).
        let safe: String = name
            .chars()
            .filter(|c| c.is_ascii_alphanumeric() || *c == '_')
            .collect();
        if safe.is_empty() {
            anyhow::bail!("tool name must contain alphanumerics");
        }

        // Persist the tool so it survives restarts (registry also
        // hot-registers it for this session).
        std::fs::create_dir_all(&self.tools_dir)?;
        let path = self.tools_dir.join(format!("{safe}.lua"));
        std::fs::write(&path, code)?;
        (self.register)(&safe, path.clone());

        Ok(ToolOutput {
            text: format!(
                "tool '{safe}' created at {} and hot-registered. Callable by name from the next turn.",
                path.display()
            ),
        })
    }
}

/// Tool that lets the model improve its own harness (prompt + skills).
pub struct ImproveHarnessTool {
    /// Absolute path to the harness dir (system_prompt.md lives here).
    pub harness_dir: std::path::PathBuf,
}

#[async_trait]
impl Tool for ImproveHarnessTool {
    fn name(&self) -> &str {
        "improve_harness"
    }

    fn description(&self) -> &str {
        "Improve sigil's own harness (the instruction layer). Actions: \
         'edit_prompt' (replace system_prompt.md — content is the new full \
         prompt), 'add_skill' (create a skill markdown file whose first \
         line is '> short description', reusable by future sessions), \
         'list_skills' (list existing skills). Use sparingly for durable, \
         generally-useful improvements; the harness persists across \
         sessions."
    }

    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "action": {
                    "type": "string",
                    "enum": ["edit_prompt", "add_skill", "list_skills"],
                    "description": "What to do"
                },
                "content": {
                    "type": "string",
                    "description": "For edit_prompt: the new full system prompt text. For add_skill: the full skill markdown (first line: '> short description')."
                },
                "name": {
                    "type": "string",
                    "description": "For add_skill: skill file name (no extension)."
                }
            },
            "required": ["action"]
        })
    }

    async fn run(&self, args: &Value) -> anyhow::Result<ToolOutput> {
        let action = args
            .get("action")
            .and_then(Value::as_str)
            .unwrap_or_default();
        match action {
            "edit_prompt" => {
                let content = args
                    .get("content")
                    .and_then(Value::as_str)
                    .ok_or_else(|| anyhow::anyhow!("edit_prompt requires 'content'"))?;
                let path = self.harness_dir.join("system_prompt.md");
                std::fs::write(&path, content)?;
                Ok(ToolOutput {
                    text: format!(
                        "system_prompt.md updated ({} chars). It takes effect on the next turn.",
                        content.chars().count()
                    ),
                })
            }
            "add_skill" => {
                let name = args
                    .get("name")
                    .and_then(Value::as_str)
                    .ok_or_else(|| anyhow::anyhow!("add_skill requires 'name'"))?;
                let content = args
                    .get("content")
                    .and_then(Value::as_str)
                    .ok_or_else(|| anyhow::anyhow!("add_skill requires 'content'"))?;
                // Constrain the name to a safe file stem (drop unsafe).
                let safe: String = name
                    .chars()
                    .filter(|c| c.is_ascii_alphanumeric() || *c == '_' || *c == '-')
                    .collect();
                if safe.is_empty() {
                    anyhow::bail!("skill name must contain alphanumerics");
                }
                let skills_dir = self.harness_dir.join("skills");
                std::fs::create_dir_all(&skills_dir)?;
                let path = skills_dir.join(format!("{safe}.md"));
                std::fs::write(&path, content)?;
                Ok(ToolOutput {
                    text: format!(
                        "skill '{safe}' written to {}. Listed in the skill index from the next turn.",
                        path.display()
                    ),
                })
            }
            "list_skills" => {
                let skills_dir = self.harness_dir.join("skills");
                let mut names = Vec::new();
                if let Ok(entries) = std::fs::read_dir(&skills_dir) {
                    for e in entries.flatten() {
                        if let Some(n) = e.file_name().to_str() {
                            names.push(n.to_string());
                        }
                    }
                }
                names.sort();
                let text = if names.is_empty() {
                    "no skills yet".to_string()
                } else {
                    names.join("\n")
                };
                Ok(ToolOutput { text })
            }
            other => anyhow::bail!("unknown action: {other}"),
        }
    }
}
