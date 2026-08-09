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
/// loop's evolution path). Sends a `HotOp` into the registry's queue.
pub type RegisterHook = Box<dyn Fn(&str, std::path::PathBuf) + Send + Sync>;

/// Runtime hook to hot-unregister a scripted tool at runtime.
pub type UnregisterHook = Box<dyn Fn(&str) + Send + Sync>;

/// Tool that manages Lua scripted tools at runtime: create, update
/// (overwrite + hot-refresh), delete, and list.
pub struct CreateLuaToolTool {
    /// Where scripted tools live (hot-loaded by the registry).
    pub tools_dir: std::path::PathBuf,
    /// Runtime handle to hot-register tools without restart.
    pub register: RegisterHook,
    /// Runtime handle to hot-unregister tools without restart.
    pub unregister: UnregisterHook,
}

#[async_trait]
impl Tool for CreateLuaToolTool {
    fn name(&self) -> &str {
        "create_lua_tool"
    }

    fn description(&self) -> &str {
        "Manage Lua scripted tools: create (default), update (overwrite an existing tool's code), delete (remove a tool), or list (show existing tool names). Write Lua code (with optional -- desc: and -- schema: header comments); sigil saves it to the tools dir and makes it callable by name from the next turn on. Use when a task needs a repeatable operation that the built-in tools don't cover, or to iterate on an existing scripted tool. Your Lua code has a global `host` table with safe helpers: host.read_file(path), host.write_file(path, content), host.env(key), host.list_dir(dir), host.cwd(), host.json_encode(value), host.json_decode(text). FFI is available (require('ffi')) for calling system C libraries."
    }

    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "action": {
                    "type": "string",
                    "enum": ["create", "update", "delete", "list"],
                    "description": "create (default): new tool; update: overwrite existing tool's code and hot-refresh it; delete: remove the tool; list: show existing tool names (no name/code needed)"
                },
                "name": { "type": "string", "description": "Tool name (alphanumeric + underscore; becomes the Lua file name)" },
                "code": { "type": "string", "description": "Full Lua source. Must return a function(args) -> result. Optional header comments: -- desc: ... and -- schema: {...}" }
            },
            "required": []
        })
    }

    async fn run(&self, args: &Value) -> anyhow::Result<ToolOutput> {
        let action = args
            .get("action")
            .and_then(Value::as_str)
            .unwrap_or("create")
            .to_string();

        if action == "list" {
            let mut names: Vec<String> = Vec::new();
            if let Ok(entries) = std::fs::read_dir(&self.tools_dir) {
                for e in entries.flatten() {
                    if let Some(stem) = e.path().file_stem().and_then(|s| s.to_str())
                        && e.path().extension().map(|x| x == "lua").unwrap_or(false)
                    {
                        names.push(stem.to_string());
                    }
                }
                names.sort();
            }
            let text = if names.is_empty() {
                "no scripted tools yet".to_string()
            } else {
                format!("scripted tools: {}", names.join(", "))
            };
            return Ok(ToolOutput { text });
        }

        let name = args
            .get("name")
            .and_then(Value::as_str)
            .filter(|n| !n.is_empty())
            .ok_or_else(|| anyhow::anyhow!("{} requires a non-empty 'name'", action))?;

        // Sanitize the name into a safe file stem (drop unsafe chars).
        let safe: String = name
            .chars()
            .filter(|c| c.is_ascii_alphanumeric() || *c == '_')
            .collect();
        if safe.is_empty() {
            anyhow::bail!("tool name must contain alphanumerics");
        }
        let path = self.tools_dir.join(format!("{safe}.lua"));

        match action.as_str() {
            "delete" => {
                if path.exists() {
                    // Archive before removal so a mistaken delete can
                    // be restored from the version library.
                    if let Ok(old) = std::fs::read_to_string(&path) {
                        crate::harness::versions::backup_and_prune(
                            &self.tools_dir,
                            "tools",
                            &safe,
                            &old,
                        );
                    }
                    std::fs::remove_file(&path)?;
                    (self.unregister)(&safe);
                    Ok(ToolOutput {
                        text: format!(
                            "tool '{safe}' deleted and unregistered (previous version archived)."
                        ),
                    })
                } else {
                    Ok(ToolOutput {
                        text: format!("tool '{safe}' does not exist (nothing to delete)."),
                    })
                }
            }
            "update" | "create" => {
                let code = args
                    .get("code")
                    .and_then(Value::as_str)
                    .filter(|c| !c.is_empty())
                    .ok_or_else(|| anyhow::anyhow!("{} requires non-empty 'code'", action))?;
                // Persist the tool so it survives restarts (registry also
                // hot-registers it for this session).
                std::fs::create_dir_all(&self.tools_dir)?;
                // Snapshot the previous version on overwrite.
                if let Ok(old) = std::fs::read_to_string(&path) {
                    crate::harness::versions::backup_and_prune(
                        &self.tools_dir,
                        "tools",
                        &safe,
                        &old,
                    );
                }
                std::fs::write(&path, code)?;
                (self.register)(&safe, path.clone());
                let verb = if action == "update" {
                    "updated"
                } else {
                    "created"
                };
                Ok(ToolOutput {
                    text: format!(
                        "tool '{safe}' {verb} at {} and hot-registered. Callable by name from the next turn.",
                        path.display()
                    ),
                })
            }
            other => anyhow::bail!("unknown action: {other}"),
        }
    }
}

/// Tool that lets the model improve its own harness (prompt + skills).
pub struct ImproveHarnessTool {
    /// Absolute path to the harness dir (system_prompt.md lives here).
    pub harness_dir: std::path::PathBuf,
    /// Live prompt state to update after writes (so the running agent
    /// sees edits immediately). `None` = isolated (tests).
    pub prompt_state: Option<std::sync::Arc<std::sync::RwLock<crate::harness::PromptState>>>,
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
                // Snapshot the current prompt before replacing it.
                if let Ok(old) = std::fs::read_to_string(&path) {
                    crate::harness::versions::backup_and_prune(
                        &self.harness_dir,
                        "prompt",
                        "system_prompt",
                        &old,
                    );
                }
                std::fs::write(&path, content)?;
                if let Some(state) = &self.prompt_state
                    && let Ok(mut s) = state.write()
                {
                    s.system_prompt = content.to_string();
                }
                Ok(ToolOutput {
                    text: format!(
                        "system_prompt.md updated ({} chars). It is live — the very next turn uses it.",
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
                // Snapshot the previous version if this skill exists.
                if let Ok(old) = std::fs::read_to_string(&path) {
                    crate::harness::versions::backup_and_prune(
                        &self.harness_dir,
                        "skills",
                        &safe,
                        &old,
                    );
                }
                std::fs::write(&path, content)?;
                if let Some(state) = &self.prompt_state
                    && let Ok(mut s) = state.write()
                {
                    // Refresh the in-memory skill list from disk so
                    // dynamic inlining picks the new skill up now.
                    s.skills = crate::harness::load_skills(&skills_dir);
                }
                Ok(ToolOutput {
                    text: format!(
                        "skill '{safe}' written to {}. It is live — relevant prompts inline it from the very next turn.",
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
