//! Evolution tools: let the model extend its own toolset and improve
//! its own harness.
//!
//! Phase B 边界说明：本模块的 std::fs 操作对象是 agent 自管理目录
//! （tools_dir / refinements.jsonl / harness 文件）—— 非"执行世界"（用户
//! 项目文件），不走 FsProvider 缝隙（对齐 dsh：seam 覆盖用户世界操作）。
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
        "Manage Lua scripted tools: create (default), update (overwrite an existing tool's code), delete (remove a tool), or list (show existing tool names). Write Lua code (with optional -- desc: and -- schema: header comments); sigil saves it to the tools dir and makes it callable by name from the next turn on. Use when a task needs a repeatable operation that the built-in tools don't cover, or to iterate on an existing scripted tool. Your Lua code has a global `host` table with safe helpers: host.read_file(path), host.write_file(path, content), host.env(key), host.list_dir(dir), host.cwd(), host.json_encode(value), host.json_decode(text), host.date(format). FFI is available (require('ffi')) for calling system C libraries. Sandbox: the io/os standard libraries are NOT exposed (use host helpers for file/env/date), and require() only loads built-in modules (ffi, string, table, math, bit, jit, coroutine) — anything else errors."
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
    /// Session-local harness dir (optional). When set, `scope=local`
    /// writes land here instead of the global dir — the session's
    /// private experiment surface (prime-agent local/global dual-track).
    /// `None` = no local layer; scope=local is rejected with a hint.
    pub local_dir: Option<std::path::PathBuf>,
    /// Live prompt state to update after writes (so the running agent
    /// sees edits immediately). `None` = isolated (tests).
    pub prompt_state: Option<std::sync::Arc<std::sync::RwLock<crate::harness::PromptState>>>,
    /// Agent-loop Lua hooks dir (P-C: the model edits its own policy
    /// modules). `None` = hooks actions rejected with a hint (start
    /// sigil with --hooks-dir to enable).
    pub hooks_dir: Option<std::path::PathBuf>,
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

/// Append one refinement event to `harness_dir/refinements.jsonl`.
/// Append-only (O_APPEND) so concurrent instances never interleave
/// lines.
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

/// Stable refinement id: `action:scope-independent content fingerprint`.
/// Scope is deliberately NOT part of the id — the same improvement
/// applied first as scope=local and later promoted to scope=global
/// keeps one id, so the merged audit can dedupe them (local wins).
pub fn refinement_id(action: &str, fingerprint: &str) -> String {
    format!("{action}:{}", fnv1a_hex(fingerprint))
}

/// FNV-1a 64-bit, first 8 hex chars. Stable across processes (unlike
/// std's DefaultHasher, which seeds randomly per process — ids must
/// survive restarts and match across the local/global files).
fn fnv1a_hex(s: &str) -> String {
    let mut h: u64 = 0xcbf29ce484222325;
    for b in s.as_bytes() {
        h ^= u64::from(*b);
        h = h.wrapping_mul(0x100000001b3);
    }
    format!("{:08x}", h)
}

/// Load every refinement event from one `refinements.jsonl`
/// (bad lines are skipped — append-only logs can be truncated).
pub fn load_refinements(dir: &std::path::Path) -> Vec<Value> {
    let Ok(text) = std::fs::read_to_string(dir.join("refinements.jsonl")) else {
        return Vec::new();
    };
    text.lines()
        .filter_map(|l| serde_json::from_str(l).ok())
        .collect()
}

/// Merge the global and session-local refinement histories into one
/// view (the prime-agent `mergeRefinementHistory` pattern): events
/// are keyed by their stable id, local entries win over same-id
/// global ones, and ordering follows first appearance. Events without
/// an id (legacy lines) get a content-derived fallback id so they are
/// never collapsed onto each other.
pub fn load_merged_refinements(
    local: Option<&std::path::Path>,
    global: &std::path::Path,
) -> Vec<Value> {
    let mut by_id: std::collections::HashMap<String, Value> = std::collections::HashMap::new();
    let mut order: Vec<String> = Vec::new();
    let mut push = |ev: Value| {
        let id = ev
            .get("id")
            .and_then(Value::as_str)
            .filter(|s| !s.is_empty())
            .map(str::to_string)
            .unwrap_or_else(|| fnv1a_hex(&serde_json::to_string(&ev).unwrap_or_default()));
        if !by_id.contains_key(&id) {
            order.push(id.clone());
        }
        by_id.insert(id, ev); // later insert (local) wins
    };
    for ev in load_refinements(global) {
        push(ev);
    }
    if let Some(local) = local {
        for ev in load_refinements(local) {
            push(ev);
        }
    }
    order
        .into_iter()
        .filter_map(|id| by_id.remove(&id))
        .collect()
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
         'list_skills' (list existing skills), \
         'edit_hook' (write an agent-loop Lua hook, e.g. on_tool_result.lua \
         — content is the full Lua source and must return a function; it \
         becomes live from the very next turn, no restart), 'delete_hook' \
         (remove a hook), 'list_hooks' (list hook files). \
         Optional 'scope': 'global' edits the shared harness \
         that persists across sessions; 'local' edits the session-local \
         harness (available when sigil was started with --local-harness-dir). \
         When a local layer exists, the DEFAULT scope is 'local': run \
         experiments in the session layer — the global harness is the \
         reviewed, cross-session surface, so write there only when the \
         change is a durable, generally-useful improvement (and pass \
         scope='global' explicitly). \
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
                    "enum": ["edit_prompt", "add_skill", "list_skills", "edit_hook", "delete_hook", "list_hooks"],
                    "description": "What to do"
                },
                "scope": {
                    "type": "string",
                    "enum": ["global", "local"],
                    "description": "Where to write: 'local' (default when a local layer exists, i.e. --local-harness-dir was passed) edits the session-local harness; 'global' edits the shared persistent harness — use it only for durable, cross-session improvements."
                },
                "content": {
                    "type": "string",
                    "description": "For edit_prompt: the new full system prompt text. For add_skill: the full skill markdown (first line: '> short description'). For edit_hook: the full Lua source (must return a function taking the ctx table)."
                },
                "name": {
                    "type": "string",
                    "description": "For add_skill: skill file name (no extension). For edit_hook/delete_hook: hook file name WITH the on_ prefix and .lua extension (e.g. on_tool_result.lua)."
                },
                "reason": {
                    "type": "string",
                    "description": "REQUIRED for edit_prompt/add_skill/edit_hook/delete_hook: one sentence of evidence why this change is a durable improvement. Rejected if missing."
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
        // local/global dual-track: scope=local writes to the session-local
        // harness dir (when one is configured); default is the global dir.
        // With a local layer configured, the DEFAULT scope is "local"
        // (prime-agent semantics: experiments land in the session layer
        // unless the model explicitly asks for global) — global writes
        // are the explicit, cross-session decision.
        let scope =
            args.get("scope")
                .and_then(Value::as_str)
                .unwrap_or(if self.local_dir.is_some() {
                    "local"
                } else {
                    "global"
                });
        let target_dir = match scope {
            "global" => self.harness_dir.clone(),
            "local" => self.local_dir.clone().ok_or_else(|| {
                anyhow::anyhow!(
                    "scope=local requires a session-local harness dir — start sigil with --local-harness-dir <dir>"
                )
            })?,
            other => anyhow::bail!("unknown scope: {other} (expected 'global' or 'local')"),
        };
        // Refresh the live prompt state from the merged global+local
        // view, so the running agent sees this write on the very next
        // turn (global writes show up too — unless a local layer
        // shadows the same asset).
        let refresh_state = || {
            let Some(state) = &self.prompt_state else {
                return;
            };
            let Ok(mut s) = state.write() else {
                return;
            };
            if let Ok(merged) = crate::harness::Harness::load_with_local(
                Some(&self.harness_dir),
                self.local_dir.as_deref(),
            ) {
                s.system_prompt = merged.system_prompt;
                s.skills = merged.skills;
            }
        };
        // Writes (edit_prompt / add_skill) take the directory lock so
        // concurrent sigil instances never interleave "read old →
        // archive → write new" on the harness assets. list_skills is
        // read-only and stays outside the lock. The lock follows the
        // target dir: local writes lock the local dir, global the global.
        crate::harness::lock::with_dir_lock(&target_dir, || match action {
            "edit_prompt" => {
                let content = args
                    .get("content")
                    .and_then(Value::as_str)
                    .ok_or_else(|| anyhow::anyhow!("edit_prompt requires 'content'"))?;
                // Refinement gate: evidence-backed edits only. A reason
                // is required so harness changes are auditable and
                // reviewable (mirrors prime-agent's /refine review).
                let reason = gated_reason(args)?;
                let path = target_dir.join("system_prompt.md");
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
                        &target_dir,
                        "prompt",
                        "system_prompt",
                        &old,
                    );
                }
                std::fs::write(&path, content)?;
                refresh_state();
                record_refinement(
                    &target_dir,
                    &json!({
                        "ts": crate::session::chrono_like_timestamp(),
                        "id": refinement_id("edit_prompt", content),
                        "action": "edit_prompt",
                        "scope": scope,
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
                let skills_dir = target_dir.join("skills");
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
                    crate::harness::versions::backup_and_prune(&target_dir, "skills", &safe, &old);
                }
                std::fs::write(&path, content)?;
                refresh_state();
                // Detect dependency cycles across all skills and warn
                // (the write succeeds — a cycle is a quality problem,
                // not a reason to reject the skill).
                let all_skills = crate::harness::load_skills(&skills_dir);
                let cycles = crate::harness::find_dependency_cycles(&all_skills);
                record_refinement(
                    &target_dir,
                    &json!({
                        "ts": crate::session::chrono_like_timestamp(),
                        "id": refinement_id("add_skill", &format!("{safe}\n{content}")),
                        "action": "add_skill",
                        "scope": scope,
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
                let skills_dir = target_dir.join("skills");
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
            "list_hooks" => {
                let hooks_dir = self.hooks_dir.clone().ok_or_else(|| {
                    anyhow::anyhow!("list_hooks: no hooks dir — start sigil with --hooks-dir <dir>")
                })?;
                let mut names: Vec<String> = Vec::new();
                if let Ok(entries) = std::fs::read_dir(&hooks_dir) {
                    for e in entries.flatten() {
                        if e.path().extension().map(|x| x == "lua").unwrap_or(false)
                            && let Some(stem) = e.path().file_name().and_then(|s| s.to_str())
                        {
                            names.push(stem.to_string());
                        }
                    }
                    names.sort();
                }
                Ok(ToolOutput {
                    text: if names.is_empty() {
                        "no hooks yet — write one with edit_hook (e.g. on_tool_result.lua)"
                            .to_string()
                    } else {
                        format!("hooks: {}", names.join(", "))
                    },
                })
            }
            "edit_hook" | "delete_hook" => {
                // Hooks are policy modules the model evolves (P-C). Like
                // scripted tools, they reload from disk on every use, so
                // an edit is live from the very next turn — no
                // registration step, no restart.
                let hooks_dir = self.hooks_dir.clone().ok_or_else(|| {
                    anyhow::anyhow!("{action}: no hooks dir — start sigil with --hooks-dir <dir>")
                })?;
                let name = args
                    .get("name")
                    .and_then(Value::as_str)
                    .filter(|n| !n.is_empty())
                    .ok_or_else(|| {
                        anyhow::anyhow!(
                            "{action} requires a non-empty 'name' (e.g. on_tool_result.lua)"
                        )
                    })?;
                // Constrain to a safe file name: hook files are loaded
                // by exact name, so drop path separators and dots.
                // Accept both "on_tool_result" and "on_tool_result.lua".
                let stem = name.strip_suffix(".lua").unwrap_or(name);
                let safe: String = stem
                    .chars()
                    .filter(|c| c.is_ascii_alphanumeric() || *c == '_' || *c == '-')
                    .collect();
                if safe.is_empty() {
                    anyhow::bail!("hook name must contain alphanumerics");
                }
                if action == "delete_hook" {
                    let reason = gated_reason(args)?;
                    let path = hooks_dir.join(format!("{safe}.lua"));
                    if !path.exists() {
                        anyhow::bail!("delete_hook: hook '{safe}' does not exist");
                    }
                    crate::harness::lock::with_dir_lock(&hooks_dir, || {
                        let old = std::fs::read_to_string(&path)?;
                        crate::harness::versions::backup_and_prune(
                            &hooks_dir, "hooks", &safe, &old,
                        );
                        std::fs::remove_file(&path)?;
                        record_refinement(
                            &hooks_dir,
                            &json!({
                                "ts": crate::session::chrono_like_timestamp(),
                                "id": refinement_id("delete_hook", &safe),
                                "action": "delete_hook",
                                "name": safe,
                                "reason": reason,
                                "result": "ok",
                            }),
                        );
                        Ok(ToolOutput {
                            text: format!(
                                "hook '{safe}' deleted (previous version archived; roll back with skill_rollback kind=hooks)."
                            ),
                        })
                    })
                } else {
                    let content = args
                        .get("content")
                        .and_then(Value::as_str)
                        .filter(|c| !c.is_empty())
                        .ok_or_else(|| anyhow::anyhow!("edit_hook requires non-empty 'content'"))?;
                    let reason = gated_reason(args)?;
                    // Contract gate: a hook file must return a function.
                    if !content.contains("return function") {
                        anyhow::bail!(
                            "edit_hook: content must contain 'return function(ctx) ... end' — a hook file exports one function"
                        );
                    }
                    if content.chars().count() < 20 {
                        anyhow::bail!(
                            "edit_hook: content too short ({} chars, min 20)",
                            content.chars().count()
                        );
                    }
                    crate::harness::lock::with_dir_lock(&hooks_dir, || {
                        std::fs::create_dir_all(&hooks_dir)?;
                        let path = hooks_dir.join(format!("{safe}.lua"));
                        if let Ok(old) = std::fs::read_to_string(&path)
                            && old.trim() == content.trim()
                        {
                            anyhow::bail!(
                                "edit_hook: '{safe}' already has identical content — no change"
                            );
                        }
                        if let Ok(old) = std::fs::read_to_string(&path) {
                            crate::harness::versions::backup_and_prune(
                                &hooks_dir, "hooks", &safe, &old,
                            );
                        }
                        std::fs::write(&path, content)?;
                        record_refinement(
                            &hooks_dir,
                            &json!({
                                "ts": crate::session::chrono_like_timestamp(),
                                "id": refinement_id("edit_hook", &format!("{safe}\n{content}")),
                                "action": "edit_hook",
                                "name": safe,
                                "reason": reason,
                                "result": "ok",
                            }),
                        );
                        Ok(ToolOutput {
                            text: format!(
                                "hook '{safe}' written to {}. Live from the very next turn (hooks reload on every use).",
                                path.display()
                            ),
                        })
                    })
                }
            }
            other => anyhow::bail!("unknown action: {other}"),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn tmp_dir(tag: &str) -> std::path::PathBuf {
        let d =
            std::env::temp_dir().join(format!("sigil-evolve-test-{tag}-{}", std::process::id()));
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    #[test]
    fn refinement_id_is_stable_and_scope_independent() {
        let a = refinement_id("edit_prompt", "same content");
        let b = refinement_id("edit_prompt", "same content");
        assert_eq!(a, b, "same content → same id");
        let c = refinement_id("edit_prompt", "different content");
        assert_ne!(a, c, "different content → different id");
        let d = refinement_id("add_skill", "same content");
        assert_ne!(a, d, "different action → different id");
        // FNV-1a ids are deterministic across processes (no random
        // seed, unlike DefaultHasher) — the property the merged audit
        // relies on.
        let e = fnv1a_hex("same content");
        assert_eq!(e, fnv1a_hex("same content"));
    }

    #[test]
    fn load_refinements_skips_bad_lines() {
        let dir = tmp_dir("load");
        std::fs::write(
            dir.join("refinements.jsonl"),
            "{\"id\": \"a\"}\nnot json\n{\"id\": \"b\"}\n",
        )
        .unwrap();
        let evs = load_refinements(&dir);
        assert_eq!(evs.len(), 2);
        assert_eq!(evs[0]["id"], "a");
        assert_eq!(evs[1]["id"], "b");
        // Missing file → empty.
        assert!(load_refinements(&std::env::temp_dir()).is_empty());
    }

    #[test]
    fn merged_refinements_local_wins_same_id() {
        let global = tmp_dir("g");
        let local = tmp_dir("l");
        std::fs::write(
            global.join("refinements.jsonl"),
            serde_json::to_string(&json!({
                "id": "edit_prompt:abc123",
                "ts": "t1",
                "action": "edit_prompt",
                "scope": "global",
                "reason": "global version"
            }))
            .unwrap(),
        )
        .unwrap();
        std::fs::write(
            local.join("refinements.jsonl"),
            serde_json::to_string(&json!({
                "id": "edit_prompt:abc123",
                "ts": "t2",
                "action": "edit_prompt",
                "scope": "local",
                "reason": "local version"
            }))
            .unwrap(),
        )
        .unwrap();

        let merged = load_merged_refinements(Some(&local), &global);
        assert_eq!(merged.len(), 1, "same id deduped to one entry");
        assert_eq!(merged[0]["scope"], "local", "local wins");
        assert_eq!(merged[0]["reason"], "local version");
    }

    #[test]
    fn merged_refinements_keeps_distinct_ids_in_order() {
        let global = tmp_dir("g2");
        let local = tmp_dir("l2");
        std::fs::write(
            global.join("refinements.jsonl"),
            "{\"id\": \"g1\", \"ts\": \"1\", \"reason\": \"g1\"}\n\
             {\"id\": \"g2\", \"ts\": \"2\", \"reason\": \"g2\"}\n",
        )
        .unwrap();
        std::fs::write(
            local.join("refinements.jsonl"),
            "{\"id\": \"l1\", \"ts\": \"3\", \"reason\": \"l1\"}\n",
        )
        .unwrap();

        let merged = load_merged_refinements(Some(&local), &global);
        let ids: Vec<&str> = merged.iter().filter_map(|ev| ev["id"].as_str()).collect();
        assert_eq!(
            ids,
            vec!["g1", "g2", "l1"],
            "global order first, local appended"
        );
    }

    #[test]
    fn merged_refinements_legacy_lines_without_id_not_collapsed() {
        let global = tmp_dir("g3");
        std::fs::write(
            global.join("refinements.jsonl"),
            "{\"ts\": \"1\", \"reason\": \"legacy one\"}\n\
             {\"ts\": \"2\", \"reason\": \"legacy two\"}\n",
        )
        .unwrap();
        let merged = load_merged_refinements(None, &global);
        assert_eq!(merged.len(), 2, "id-less legacy lines stay distinct");
    }
}
