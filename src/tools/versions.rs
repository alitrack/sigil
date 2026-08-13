//! Version management tools: `skill_history` and `skill_rollback`.
//!
//! These are the restore side of the self-evolution safety net. The
//! model can list every archived version of a skill / prompt / scripted
//! tool and roll the live asset back to any of them.

use async_trait::async_trait;
use serde_json::{Value, json};

use crate::harness::versions;
use crate::tools::{Tool, ToolOutput};

/// Tool that lists archived versions of harness assets.
pub struct SkillHistoryTool {
    /// Harness dir (system_prompt.md, skills/ live here).
    pub harness_dir: std::path::PathBuf,
    /// Tools dir (scripted Lua tools live here).
    pub tools_dir: std::path::PathBuf,
    /// Hooks dir (agent-loop Lua policy modules live here).
    pub hooks_dir: Option<std::path::PathBuf>,
}

#[async_trait]
impl Tool for SkillHistoryTool {
    fn name(&self) -> &str {
        "skill_history"
    }

    fn description(&self) -> &str {
        "List archived versions of a harness asset (skill, the system prompt, a scripted Lua tool, or an agent-loop Lua hook). Params: kind (skills | prompt | tools | hooks), name (asset base name, e.g. math, 'system_prompt' for prompt, or on_tool_result for a hook). Each write performed by improve_harness / create_lua_tool snapshots the previous content, so you can inspect what changed and roll back a bad edit with skill_rollback."
    }

    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "kind": {
                    "type": "string",
                    "enum": ["skills", "prompt", "tools", "hooks"],
                    "description": "Asset kind: skills (skill markdown), prompt (system_prompt.md), tools (scripted Lua tool), hooks (agent-loop Lua hook)"
                },
                "name": {
                    "type": "string",
                    "description": "Asset base name: a skill name, 'system_prompt', a tool name, or a hook base name (e.g. on_tool_result)"
                }
            },
            "required": ["kind", "name"]
        })
    }

    async fn run(&self, args: &Value) -> anyhow::Result<ToolOutput> {
        let kind = args.get("kind").and_then(Value::as_str).unwrap_or_default();
        let name = args.get("name").and_then(Value::as_str).unwrap_or_default();
        if kind.is_empty() || name.is_empty() {
            anyhow::bail!("skill_history requires 'kind' and 'name'");
        }
        let base = match kind {
            "skills" | "prompt" => &self.harness_dir,
            "tools" => &self.tools_dir,
            "hooks" => self.hooks_dir.as_ref().ok_or_else(|| {
                anyhow::anyhow!(
                    "kind=hooks requires a hooks dir — start sigil with --hooks-dir <dir>"
                )
            })?,
            other => anyhow::bail!("unknown kind: {other}"),
        };
        let versions = versions::versions_of(base, kind, name);
        if versions.is_empty() {
            return Ok(ToolOutput {
                text: format!("no archived versions of {kind}/{name} yet"),
            });
        }
        let lines: Vec<String> = versions
            .iter()
            .enumerate()
            .map(|(i, (stamp, len))| format!("{}. {stamp} ({len} bytes)", i + 1))
            .collect();
        Ok(ToolOutput {
            text: format!(
                "archived versions of {kind}/{name} (newest first):\n{}",
                lines.join("\n")
            ),
        })
    }
}

/// Tool that restores a harness asset to an archived version.
pub struct SkillRollbackTool {
    /// Harness dir.
    pub harness_dir: std::path::PathBuf,
    /// Tools dir.
    pub tools_dir: std::path::PathBuf,
    /// Hooks dir (agent-loop Lua policy modules).
    pub hooks_dir: Option<std::path::PathBuf>,
    /// Hook to hot-register a scripted tool after a rollback (so the
    /// restored code is live in the current session, not just on disk).
    pub register: Option<crate::tools::evolve::RegisterHook>,
    /// Live prompt state to update after a prompt/skill rollback (so
    /// the running agent sees it immediately). `None` = isolated.
    pub prompt_state: Option<std::sync::Arc<std::sync::RwLock<crate::harness::PromptState>>>,
}

#[async_trait]
impl Tool for SkillRollbackTool {
    fn name(&self) -> &str {
        "skill_rollback"
    }

    fn description(&self) -> &str {
        "Restore a harness asset (skill, system prompt, scripted Lua tool, or agent-loop Lua hook) to an archived version. Params: kind (skills | prompt | tools | hooks), name (asset base name), version (the stamp shown by skill_history). Use after a bad edit — the harness self-evolution loop is reversible."
    }

    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "kind": {
                    "type": "string",
                    "enum": ["skills", "prompt", "tools", "hooks"],
                    "description": "Asset kind"
                },
                "name": {
                    "type": "string",
                    "description": "Asset base name"
                },
                "version": {
                    "type": "string",
                    "description": "Version stamp from skill_history (newest first)"
                }
            },
            "required": ["kind", "name", "version"]
        })
    }

    async fn run(&self, args: &Value) -> anyhow::Result<ToolOutput> {
        let kind = args.get("kind").and_then(Value::as_str).unwrap_or_default();
        let name = args.get("name").and_then(Value::as_str).unwrap_or_default();
        let version = args
            .get("version")
            .and_then(Value::as_str)
            .unwrap_or_default();
        if kind.is_empty() || name.is_empty() || version.is_empty() {
            anyhow::bail!("skill_rollback requires 'kind', 'name' and 'version'");
        }
        let base = match kind {
            "skills" | "prompt" => &self.harness_dir,
            "tools" => &self.tools_dir,
            "hooks" => self.hooks_dir.as_ref().ok_or_else(|| {
                anyhow::anyhow!(
                    "kind=hooks requires a hooks dir — start sigil with --hooks-dir <dir>"
                )
            })?,
            other => anyhow::bail!("unknown kind: {other}"),
        };
        // Rollback is a write (restore overwrites the live asset) — take
        // the directory lock so concurrent instances serialize.
        crate::harness::lock::with_dir_lock(base, || {
            let restored = versions::restore(base, kind, name, version)?;
            // For scripted tools, hot-register the restored code so it is
            // live immediately (the file on disk changed, but the registry
            // still holds the previous implementation).
            if kind == "tools"
                && let Some(reg) = &self.register
            {
                (reg)(name, self.tools_dir.join(format!("{name}.lua")));
            }
            // For prompts / skills, refresh the live agent state so the
            // restored content is used on the very next turn.
            if kind != "tools"
                && let Some(state) = &self.prompt_state
                && let Ok(mut s) = state.write()
            {
                if kind == "prompt" {
                    s.system_prompt = restored.clone();
                } else {
                    s.skills = crate::harness::load_skills(&self.harness_dir.join("skills"));
                }
            }
            Ok(ToolOutput {
                text: format!(
                    "restored {kind}/{name} to version {version} ({} chars). Live asset updated{}.",
                    restored.chars().count(),
                    if kind == "tools" {
                        " and hot-registered for this session"
                    } else {
                        " — the very next turn uses it"
                    }
                ),
            })
        })
    }
}

/// Tool that prunes archived versions of a harness asset.
///
/// Backups auto-prune at [`versions::AUTO_KEEP`] per asset; this tool
/// lets the model reduce retention explicitly (e.g. keep 3 versions).
pub struct VersionsGcTool {
    /// Harness dir.
    pub harness_dir: std::path::PathBuf,
    /// Tools dir.
    pub tools_dir: std::path::PathBuf,
    /// Hooks dir (agent-loop Lua policy modules).
    pub hooks_dir: Option<std::path::PathBuf>,
}

#[async_trait]
impl Tool for VersionsGcTool {
    fn name(&self) -> &str {
        "versions_gc"
    }

    fn description(&self) -> &str {
        "Prune archived versions of a harness asset, keeping only the newest N. Params: kind (skills | prompt | tools | hooks), name (asset base name), keep (how many versions to retain; default 5). Backups auto-prune at 50 per asset, so use this when you want tighter retention."
    }

    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "kind": {
                    "type": "string",
                    "enum": ["skills", "prompt", "tools", "hooks"],
                    "description": "Asset kind"
                },
                "name": {
                    "type": "string",
                    "description": "Asset base name"
                },
                "keep": {
                    "type": "integer",
                    "minimum": 1,
                    "description": "How many newest versions to keep (default 5)"
                }
            },
            "required": ["kind", "name"]
        })
    }

    async fn run(&self, args: &Value) -> anyhow::Result<ToolOutput> {
        let kind = args.get("kind").and_then(Value::as_str).unwrap_or_default();
        let name = args.get("name").and_then(Value::as_str).unwrap_or_default();
        if kind.is_empty() || name.is_empty() {
            anyhow::bail!("versions_gc requires 'kind' and 'name'");
        }
        let keep = args.get("keep").and_then(Value::as_u64).unwrap_or(5) as usize;
        let base = match kind {
            "skills" | "prompt" => &self.harness_dir,
            "tools" => &self.tools_dir,
            "hooks" => self.hooks_dir.as_ref().ok_or_else(|| {
                anyhow::anyhow!(
                    "kind=hooks requires a hooks dir — start sigil with --hooks-dir <dir>"
                )
            })?,
            other => anyhow::bail!("unknown kind: {other}"),
        };
        // GC deletes archived files — serialize with other writers.
        crate::harness::lock::with_dir_lock(base, || {
            let removed = versions::gc(base, kind, name, keep);
            let remaining = versions::versions_of(base, kind, name).len();
            Ok(ToolOutput {
                text: format!(
                    "pruned {removed} old version(s) of {kind}/{name}; {remaining} kept (newest {keep})"
                ),
            })
        })
    }
}
