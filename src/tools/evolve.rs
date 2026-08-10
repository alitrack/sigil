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

        // Writes (create/update/delete) take the directory lock so
        // concurrent sigil instances never interleave "read old →
        // archive → write new" on the same tool.
        crate::harness::lock::with_dir_lock(&self.tools_dir, || {
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
        })
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

/// Require an evidence-backed `reason` for a harness write (the
/// refinement gate). The model must justify why the change is a
/// durable, generally-useful improvement — mirrors prime-agent's
/// `/refine` review gate, minus the extra LLM round-trip.
fn gated_reason(args: &Value) -> anyhow::Result<&str> {
    let reason = args
        .get("reason")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|r| !r.is_empty())
        .ok_or_else(|| {
            anyhow::anyhow!(
                "improve_harness requires 'reason': explain in one sentence why this change is a durable, evidence-backed improvement (noise edits are rejected)"
            )
        })?;
    Ok(reason)
}

/// Append one refinement event to `harness_dir/refinements.jsonl`
/// (append-only audit log, inside the harness dir lock so concurrent
/// instances never interleave).
fn record_refinement(harness_dir: &std::path::Path, event: &Value) {
    use std::io::Write;
    let path = harness_dir.join("refinements.jsonl");
    if let Ok(mut f) = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&path)
    {
        let line = serde_json::to_string(event).unwrap_or_default();
        let _ = writeln!(f, "{line}");
    }
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
         'list_skills' (list existing skills). \
         Every write REQUIRES a 'reason' field: one sentence of evidence \
         why this change is a durable, generally-useful improvement. \
         Noise edits (empty/identical content, too-short prompts) are \
         rejected, and every accepted edit is appended to \
         refinements.jsonl for audit. Use sparingly for durable, \
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
                },
                "reason": {
                    "type": "string",
                    "description": "REQUIRED for edit_prompt/add_skill: one sentence of evidence why this change is a durable improvement. Rejected if missing."
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
        // Writes (edit_prompt / add_skill) take the directory lock so
        // concurrent sigil instances never interleave "read old →
        // archive → write new" on the harness assets. list_skills is
        // read-only and stays outside the lock.
        crate::harness::lock::with_dir_lock(&self.harness_dir, || match action {
            "edit_prompt" => {
                let content = args
                    .get("content")
                    .and_then(Value::as_str)
                    .ok_or_else(|| anyhow::anyhow!("edit_prompt requires 'content'"))?;
                // Refinement gate: evidence-backed edits only. A reason
                // is required so harness changes are auditable and
                // reviewable (mirrors prime-agent's /refine review).
                let reason = gated_reason(args)?;
                let path = self.harness_dir.join("system_prompt.md");
                let old = std::fs::read_to_string(&path).unwrap_or_default();
                // Quality gate: a full prompt replacement must actually
                // change something and stay substantial.
                if content.trim().is_empty() {
                    anyhow::bail!("edit_prompt: content is empty");
                }
                if content.chars().count() < 100 {
                    anyhow::bail!(
                        "edit_prompt: content too short ({} chars, min 100) — a full system prompt is needed; small tweaks belong in the 'reason' or a skill",
                        content.chars().count()
                    );
                }
                if old.trim() == content.trim() {
                    anyhow::bail!(
                        "edit_prompt: content identical to the current prompt — no change. If you meant a small tweak, say what changed in 'reason'"
                    );
                }
                // Snapshot the current prompt before replacing it.
                if !old.is_empty() {
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
                record_refinement(
                    &self.harness_dir,
                    &json!({
                        "ts": crate::session::chrono_like_timestamp(),
                        "action": "edit_prompt",
                        "reason": reason,
                        "result": "ok",
                    }),
                );
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
                // Refinement gate: evidence-backed edits only.
                let reason = gated_reason(args)?;
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
                // Quality gate: a skill needs a description header, a
                // real body, and must not be a no-op rewrite.
                if content.trim().is_empty() {
                    anyhow::bail!("add_skill: content is empty");
                }
                if content.chars().count() < 50 {
                    anyhow::bail!(
                        "add_skill: content too short ({} chars, min 50)",
                        content.chars().count()
                    );
                }
                let first_line = content.lines().next().unwrap_or("");
                if !first_line.starts_with("> ") {
                    anyhow::bail!(
                        "add_skill: content must start with '> short description' as its first line"
                    );
                }
                if let Ok(existing) = std::fs::read_to_string(&path)
                    && existing.trim() == content.trim()
                {
                    anyhow::bail!(
                        "add_skill: skill '{safe}' already exists with identical content — no change"
                    );
                }
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
                // Detect dependency cycles across all skills and warn
                // (the write succeeds — a cycle is a quality problem,
                // not a reason to reject the skill).
                let all_skills = crate::harness::load_skills(&skills_dir);
                let cycles = crate::harness::find_dependency_cycles(&all_skills);
                record_refinement(
                    &self.harness_dir,
                    &json!({
                        "ts": crate::session::chrono_like_timestamp(),
                        "action": "add_skill",
                        "name": safe,
                        "reason": reason,
                        "result": "ok",
                    }),
                );
                let mut text = format!(
                    "skill '{safe}' written to {}. It is live — relevant prompts inline it from the very next turn.",
                    path.display()
                );
                if !cycles.is_empty() {
                    let desc: Vec<String> = cycles.iter().map(|c| c.join(" -> ")).collect();
                    text.push_str(&format!(
                        "\n\nWARNING: dependency cycle(s) detected: {}",
                        desc.join("; ")
                    ));
                    text.push_str(" — break the cycle by removing a depends_on line.");
                }
                Ok(ToolOutput { text })
            }
            "list_skills" => {
                let skills_dir = self.harness_dir.join("skills");
                let skills = crate::harness::load_skills(&skills_dir);
                let cycles = crate::harness::find_dependency_cycles(&skills);
                if skills.is_empty() {
                    return Ok(ToolOutput {
                        text: "no skills yet".to_string(),
                    });
                }
                let mut lines = Vec::new();
                for s in &skills {
                    let dep = if s.depends_on.is_empty() {
                        String::new()
                    } else {
                        format!("  depends_on: {}", s.depends_on.join(", "))
                    };
                    lines.push(format!("- {} — {}{}", s.name, s.description, dep));
                }
                if !cycles.is_empty() {
                    let desc: Vec<String> = cycles.iter().map(|c| c.join(" -> ")).collect();
                    lines.push(String::new());
                    lines.push(format!("⚠ dependency cycle(s): {}", desc.join("; ")));
                }
                Ok(ToolOutput {
                    text: lines.join("\n"),
                })
            }
            other => anyhow::bail!("unknown action: {other}"),
        })
    }
}
