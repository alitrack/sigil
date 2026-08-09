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
}

#[async_trait]
impl Tool for SkillHistoryTool {
    fn name(&self) -> &str {
        "skill_history"
    }

    fn description(&self) -> &str {
        "List archived versions of a harness asset (skill, the system prompt, or a scripted Lua tool). Params: kind (skills | prompt | tools), name (asset base name, e.g. math, or 'system_prompt' for prompt). Each write performed by improve_harness / create_lua_tool snapshots the previous content, so you can inspect what changed and roll back a bad edit with skill_rollback."
    }

    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "kind": {
                    "type": "string",
                    "enum": ["skills", "prompt", "tools"],
                    "description": "Asset kind: skills (skill markdown), prompt (system_prompt.md), tools (scripted Lua tool)"
                },
                "name": {
                    "type": "string",
                    "description": "Asset base name: a skill name, 'system_prompt', or a tool name"
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
}

#[async_trait]
impl Tool for SkillRollbackTool {
    fn name(&self) -> &str {
        "skill_rollback"
    }

    fn description(&self) -> &str {
        "Restore a harness asset (skill, system prompt, or scripted Lua tool) to an archived version. Params: kind (skills | prompt | tools), name (asset base name), version (the stamp shown by skill_history). Use after a bad edit — the harness self-evolution loop is reversible."
    }

    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "kind": {
                    "type": "string",
                    "enum": ["skills", "prompt", "tools"],
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
            other => anyhow::bail!("unknown kind: {other}"),
        };
        let restored = versions::restore(base, kind, name, version)?;
        Ok(ToolOutput {
            text: format!(
                "restored {kind}/{name} to version {version} ({} chars). Live asset updated.",
                restored.chars().count()
            ),
        })
    }
}
