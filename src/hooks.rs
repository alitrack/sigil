//! Agent-loop Lua hooks (P-A of the thin-core roadmap: LuaJIT as the
//! plugin layer, Rust as the thin shell).
//!
//! Hook files live in `<hooks-dir>/` (default `~/.sigil/hooks/`) and
//! follow the same contract as scripted tools: the file must return a
//! function that takes the context table as its single argument.
//!
//! Two forms of a hook *slot* are supported (Cordis P0, 4-mode dispatch):
//!
//!   * a single file `<name>.lua` — one listener (the original form), and
//!   * a directory `<name>.d/` — many listeners, one `*.lua` per file,
//!     run in **sorted filename order** (e.g. `01_safety.lua`,
//!     `02_cost.lua`), so policy order is explicit on disk.
//!
//! The directory form takes precedence when both exist. An absent slot is
//! a zero-cost no-op. Hook errors are surfaced, never silently swallowed
//! — hooks are user-owned behavior and must not fail invisibly.
//!
//! **Dispatch modes** (Cordis "4 种事件分发"):
//!
//!   * `Emit`     — observation: every listener runs, non-null results are
//!                  collected. Order-insensitive, fire-and-forget.
//!   * `Waterfall`— middleware: a running value is threaded through the
//!                  listeners in order; each may replace it (return a
//!                  value) or defer (return nil). A returned string with
//!                  the `__SHORTCIRCUIT__` prefix halts the chain.
//!   * `Parallel` — every independent monitor runs (concurrently), and
//!                  *all* results are collected in listener order.
//!   * `Serial`   — ordered decision: the first non-null result wins and
//!                  the rest are skipped (multi-stage approval).
//!
//! Hooks run in the shared restricted Lua state (no `io`/`os`, host
//! helpers only, allowlisted `require`), same sandbox as scripted tools.

use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use anyhow::{Context, Result};
use serde_json::Value;

use crate::lua::LuaRuntime;

/// Default hooks directory: `~/.sigil/hooks/`.
pub fn default_hooks_dir() -> std::path::PathBuf {
    dirs::home_dir()
        .unwrap_or_else(|| std::path::PathBuf::from("."))
        .join(".sigil")
        .join("hooks")
}

/// How a hook slot's listeners are combined (Cordis 4-mode dispatch).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum HookMode {
    /// Observation: all listeners run; non-null results collected.
    Emit,
    /// Middleware: a value is threaded through the chain in order.
    Waterfall,
    /// Independent monitors: all run; every result collected in order.
    Parallel,
    /// Ordered decision: first non-null result wins, rest skipped.
    Serial,
}

/// The combined outcome of a dispatch, shaped by the mode used.
#[derive(Debug, Clone, PartialEq)]
pub enum DispatchOutput {
    /// `Emit` / `Parallel`: per-listener results, in listener order.
    /// (`Emit` drops nulls; `Parallel` keeps them as faithful slots.)
    All(Vec<Value>),
    /// `Waterfall`: the final threaded value, if any listener set one.
    Chained(Option<Value>),
    /// `Serial`: the first non-null result, if any.
    First(Option<Value>),
}

/// Runs Lua hook files from a directory.
pub struct HookRunner {
    lua: Arc<Mutex<LuaRuntime>>,
    hooks_dir: PathBuf,
}

impl std::fmt::Debug for HookRunner {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("HookRunner")
            .field("hooks_dir", &self.hooks_dir)
            .finish_non_exhaustive()
    }
}

impl HookRunner {
    pub fn new(lua: Arc<Mutex<LuaRuntime>>, hooks_dir: PathBuf) -> Self {
        Self { lua, hooks_dir }
    }

    /// The directory hook files are loaded from.
    #[allow(dead_code)] // public API surface (tests, tooling)
    pub fn hooks_dir(&self) -> &PathBuf {
        &self.hooks_dir
    }

    /// Resolve a hook *name* to its ordered listener files.
    ///
    /// `<name>.d/*.lua` (sorted by filename) wins over the single
    /// `<name>.lua`. An absent slot returns an empty vec — the zero-cost
    /// no-op the rest of the system relies on.
    pub fn slot_files(&self, name: &str) -> Vec<PathBuf> {
        let dir = self.hooks_dir.join(format!("{name}.d"));
        if dir.is_dir() {
            let mut files: Vec<PathBuf> = std::fs::read_dir(&dir)
                .into_iter()
                .flatten()
                .flatten()
                .map(|e| e.path())
                .filter(|p| p.extension().and_then(|x| x.to_str()) == Some("lua"))
                .collect();
            files.sort();
            return files;
        }
        let single = self.hooks_dir.join(format!("{name}.lua"));
        if single.is_file() {
            return vec![single];
        }
        Vec::new()
    }

    /// Run one listener file with a JSON ctx, returning the typed result.
    /// Blocking — call on a worker thread (or from a sync path).
    fn run_listener(
        lua: &Arc<Mutex<LuaRuntime>>,
        path: &PathBuf,
        ctx_json: &str,
    ) -> Result<Value> {
        let lua = lua.lock().expect("lua lock poisoned");
        lua.run_file_value(path.to_str().unwrap_or("hook"), ctx_json)
    }

    /// The empty (no-op) outcome for a slot that has no listener files.
    fn empty(mode: HookMode) -> DispatchOutput {
        match mode {
            HookMode::Emit | HookMode::Parallel => DispatchOutput::All(Vec::new()),
            HookMode::Waterfall => DispatchOutput::Chained(None),
            HookMode::Serial => DispatchOutput::First(None),
        }
    }

    /// Dispatch a hook slot in the requested mode.
    ///
    /// This is the async entry point for the agent loop. `Emit` and
    /// `Parallel` fan the listeners out onto worker threads and join them;
    /// `Waterfall` and `Serial` run them sequentially (order matters).
    /// Each listener runs on its own blocking thread so the Lua state
    /// lock is never held across an `.await`.
    pub async fn dispatch(&self, name: &str, mode: HookMode, ctx: Value) -> Result<DispatchOutput> {
        let files = self.slot_files(name);
        if files.is_empty() {
            return Ok(Self::empty(mode));
        }
        let lua = self.lua.clone();
        let base_json = ctx.to_string();

        match mode {
            // Observation: run all, keep the non-null results.
            HookMode::Emit => {
                let futs = files.iter().map(|p| {
                    let lua = lua.clone();
                    let p = p.clone();
                    let j = base_json.clone();
                    tokio::task::spawn_blocking(move || Self::run_listener(&lua, &p, &j))
                });
                let res = futures_util::future::join_all(futs).await;
                let mut out = Vec::new();
                for r in res {
                    let v = r.context("hook thread")??;
                    if !v.is_null() {
                        out.push(v);
                    }
                }
                Ok(DispatchOutput::All(out))
            }
            // Independent monitors: run all concurrently, keep EVERY
            // result (null included) in listener order.
            HookMode::Parallel => {
                let futs = files.iter().map(|p| {
                    let lua = lua.clone();
                    let p = p.clone();
                    let j = base_json.clone();
                    tokio::task::spawn_blocking(move || Self::run_listener(&lua, &p, &j))
                });
                let res = futures_util::future::join_all(futs).await;
                let mut out = Vec::new();
                for r in res {
                    out.push(r.context("hook thread")??);
                }
                Ok(DispatchOutput::All(out))
            }
            // Middleware: thread the running value through in order.
            HookMode::Waterfall => {
                let mut running = ctx.get("value").cloned().unwrap_or(Value::Null);
                for p in &files {
                    let mut step = ctx.clone();
                    if let Some(obj) = step.as_object_mut() {
                        obj.insert("value".into(), running.clone());
                    }
                    let j = step.to_string();
                    let lua = lua.clone();
                    let p = p.clone();
                    let v = tokio::task::spawn_blocking(move || Self::run_listener(&lua, &p, &j))
                        .await
                        .context("hook thread")??;
                    if v.is_null() {
                        continue; // no opinion — keep the running value
                    }
                    // `__SHORTCIRCUIT__` prefix halts the chain; the suffix
                    // (if any) becomes the final value.
                    if let Some(s) = v.as_str()
                        && let Some(rest) = s.strip_prefix("__SHORTCIRCUIT__")
                    {
                        running = Value::String(rest.to_string());
                        break;
                    }
                    running = v;
                }
                if running.is_null() {
                    Ok(DispatchOutput::Chained(None))
                } else {
                    Ok(DispatchOutput::Chained(Some(running)))
                }
            }
            // Ordered decision: first non-null result wins.
            HookMode::Serial => {
                for p in &files {
                    let j = base_json.clone();
                    let lua = lua.clone();
                    let p = p.clone();
                    let v = tokio::task::spawn_blocking(move || Self::run_listener(&lua, &p, &j))
                        .await
                        .context("hook thread")??;
                    if !v.is_null() {
                        return Ok(DispatchOutput::First(Some(v)));
                    }
                }
                Ok(DispatchOutput::First(None))
            }
        }
    }

    /// Synchronous dispatch for millisecond-scale hot paths (prompt
    /// assembly) that cannot await. Listeners run sequentially in the
    /// current thread. Same semantics as [`Self::dispatch`]; `Emit` /
    /// `Parallel` are not fanned out (a sync caller cannot block on a
    /// worker join without a runtime handle).
    pub fn dispatch_sync(
        &self,
        name: &str,
        mode: HookMode,
        ctx: Value,
    ) -> Result<DispatchOutput> {
        let files = self.slot_files(name);
        if files.is_empty() {
            return Ok(Self::empty(mode));
        }
        let base_json = ctx.to_string();
        match mode {
            HookMode::Emit => {
                let mut out = Vec::new();
                for p in &files {
                    let v = Self::run_listener(&self.lua, p, &base_json)?;
                    if !v.is_null() {
                        out.push(v);
                    }
                }
                Ok(DispatchOutput::All(out))
            }
            HookMode::Parallel => {
                let out: Vec<Value> =
                    files.iter().map(|p| Self::run_listener(&self.lua, p, &base_json)).collect::<Result<_>>()?;
                Ok(DispatchOutput::All(out))
            }
            HookMode::Waterfall => {
                let mut running = ctx.get("value").cloned().unwrap_or(Value::Null);
                for p in &files {
                    let mut step = ctx.clone();
                    if let Some(obj) = step.as_object_mut() {
                        obj.insert("value".into(), running.clone());
                    }
                    let j = step.to_string();
                    let v = Self::run_listener(&self.lua, p, &j)?;
                    if v.is_null() {
                        continue;
                    }
                    if let Some(s) = v.as_str()
                        && let Some(rest) = s.strip_prefix("__SHORTCIRCUIT__")
                    {
                        running = Value::String(rest.to_string());
                        break;
                    }
                    running = v;
                }
                if running.is_null() {
                    Ok(DispatchOutput::Chained(None))
                } else {
                    Ok(DispatchOutput::Chained(Some(running)))
                }
            }
            HookMode::Serial => {
                for p in &files {
                    let v = Self::run_listener(&self.lua, p, &base_json)?;
                    if !v.is_null() {
                        return Ok(DispatchOutput::First(Some(v)));
                    }
                }
                Ok(DispatchOutput::First(None))
            }
        }
    }

    /// Run `<name>` as a **waterfall** and return the threaded value as a
    /// string (the original single-listener contract). Preserved for
    /// backward compatibility: existing `<name>.lua` hooks that return a
    /// string behave exactly as before; a `nil` return / absent slot is a
    /// no-op. Reimplemented over [`Self::dispatch`] so there is one code
    /// path, not two.
    pub async fn run(&self, name: &str, ctx: Value) -> Result<Option<String>> {
        match self.dispatch(name, HookMode::Waterfall, ctx).await? {
            DispatchOutput::Chained(Some(v)) => {
                let s = match v {
                    Value::String(s) => s,
                    other => other.to_string(),
                };
                Ok(Some(s))
            }
            DispatchOutput::Chained(None) => Ok(None),
            // run() only ever requests Waterfall, so these are unreachable.
            DispatchOutput::All(_) | DispatchOutput::First(_) => Ok(None),
        }
    }

    /// Synchronous variant of [`Self::run`] for millisecond-scale hot
    /// paths (prompt assembly). Blocks the caller instead of hopping to a
    /// worker thread; do not use for slow hooks.
    pub fn run_sync(&self, name: &str, ctx: Value) -> Result<Option<String>> {
        match self.dispatch_sync(name, HookMode::Waterfall, ctx)? {
            DispatchOutput::Chained(Some(v)) => {
                let s = match v {
                    Value::String(s) => s,
                    other => other.to_string(),
                };
                Ok(Some(s))
            }
            DispatchOutput::Chained(None) => Ok(None),
            DispatchOutput::All(_) | DispatchOutput::First(_) => Ok(None),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn lua_runtime() -> Arc<Mutex<LuaRuntime>> {
        Arc::new(Mutex::new(LuaRuntime::new().unwrap()))
    }

    fn tmp_hooks_dir(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join("sigil-hooks-tests").join(name);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn tmp_slot_dir(name: &str, stem: &str) -> PathBuf {
        let dir = std::env::temp_dir()
            .join("sigil-hooks-tests")
            .join(name)
            .join(format!("{stem}.d"));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    // ── Backward compatibility: single-file slot, original contract ──

    #[tokio::test]
    async fn absent_hook_is_noop() {
        let runner = HookRunner::new(lua_runtime(), tmp_hooks_dir("absent"));
        assert_eq!(
            runner.run("on_tool_result", Value::Null).await.unwrap(),
            None
        );
    }

    #[tokio::test]
    async fn string_return_is_used() {
        let dir = tmp_hooks_dir("string");
        std::fs::write(
            dir.join("on_tool_result.lua"),
            "return function(ctx) return ctx.result .. ' [hook]' end",
        )
        .unwrap();
        let runner = HookRunner::new(lua_runtime(), dir);
        let out = runner
            .run("on_tool_result", serde_json::json!({ "result": "OUT" }))
            .await
            .unwrap();
        assert_eq!(out.as_deref(), Some("OUT [hook]"));
    }

    #[tokio::test]
    async fn nil_return_is_noop() {
        let dir = tmp_hooks_dir("nil");
        std::fs::write(
            dir.join("on_tool_result.lua"),
            "return function(ctx) return nil end",
        )
        .unwrap();
        let runner = HookRunner::new(lua_runtime(), dir);
        assert_eq!(
            runner.run("on_tool_result", Value::Null).await.unwrap(),
            None
        );
    }

    #[tokio::test]
    async fn hook_error_is_surfaced() {
        let dir = tmp_hooks_dir("error");
        std::fs::write(
            dir.join("on_tool_result.lua"),
            "return function(ctx) error('boom') end",
        )
        .unwrap();
        let runner = HookRunner::new(lua_runtime(), dir);
        let err = runner.run("on_tool_result", Value::Null).await;
        assert!(err.is_err(), "hook error must not be silent");
    }

    #[tokio::test]
    async fn hook_has_sandboxed_host_helpers() {
        let dir = tmp_hooks_dir("sandbox");
        std::fs::write(
            dir.join("on_tool_result.lua"),
            "return function(ctx) return tostring(host.env == nil or type(host.env) == 'function') end",
        )
        .unwrap();
        let runner = HookRunner::new(lua_runtime(), dir);
        let out = runner.run("on_tool_result", Value::Null).await.unwrap();
        assert_eq!(out.as_deref(), Some("true"));
    }

    #[test]
    fn run_sync_matches_async_contract() {
        let dir = tmp_hooks_dir("sync");
        std::fs::write(
            dir.join("on_prompt_build.lua"),
            "return function(ctx) return 'PROMPT:' .. ctx.user_prompt end",
        )
        .unwrap();
        let runner = HookRunner::new(lua_runtime(), dir);
        let out = runner
            .run_sync(
                "on_prompt_build",
                serde_json::json!({ "user_prompt": "hi" }),
            )
            .unwrap();
        assert_eq!(out.as_deref(), Some("PROMPT:hi"));
    }

    #[test]
    fn run_sync_absent_and_nil_are_noop() {
        let runner = HookRunner::new(lua_runtime(), tmp_hooks_dir("sync-absent"));
        assert_eq!(
            runner.run_sync("on_prompt_build", Value::Null).unwrap(),
            None
        );

        let dir = tmp_hooks_dir("sync-nil");
        std::fs::write(dir.join("on_prompt_build.lua"), "return function(ctx) end").unwrap();
        let runner = HookRunner::new(lua_runtime(), dir);
        assert_eq!(
            runner.run_sync("on_prompt_build", Value::Null).unwrap(),
            None
        );
    }

    // ── Slot discovery ──

    #[test]
    fn slot_prefers_dir_over_file_and_sorts_listeners() {
        let base = tmp_hooks_dir("discover");
        // A single file AND a dir both exist → dir wins.
        std::fs::write(base.join("h.lua"), "return function() return 'file' end").unwrap();
        let slot = tmp_slot_dir("discover", "h");
        std::fs::write(slot.join("02_b.lua"), "return function() return 'b' end").unwrap();
        std::fs::write(slot.join("01_a.lua"), "return function() return 'a' end").unwrap();
        let runner = HookRunner::new(lua_runtime(), base);
        let files = runner.slot_files("h");
        assert_eq!(files.len(), 2, "dir slot should override the single file");
        assert!(files[0].file_name().unwrap().to_string_lossy().ends_with("01_a.lua"));
        assert!(files[1].file_name().unwrap().to_string_lossy().ends_with("02_b.lua"));
    }

    // ── Emit: observation, non-null results collected ──

    #[tokio::test]
    async fn emit_collects_non_null_results_in_order() {
        let base = tmp_hooks_dir("emit");
        let slot = tmp_slot_dir("emit", "on_done");
        std::fs::write(slot.join("01.log.lua"), "return function() return 'first' end").unwrap();
        std::fs::write(slot.join("02.skip.lua"), "return function() return nil end").unwrap();
        std::fs::write(slot.join("03.count.lua"), "return function() return 'third' end").unwrap();
        let runner = HookRunner::new(lua_runtime(), base);
        match runner.dispatch("on_done", HookMode::Emit, Value::Null).await.unwrap() {
            DispatchOutput::All(out) => {
                assert_eq!(
                    out,
                    vec![Value::String("first".into()), Value::String("third".into())],
                    "emit keeps non-null results, drops the nil one, in order"
                );
            }
            other => panic!("emit should return All, got {other:?}"),
        }
    }

    // ── Waterfall: thread the value, defer on nil, short-circuit ──

    #[tokio::test]
    async fn waterfall_threads_value_through_chain() {
        let base = tmp_hooks_dir("wf");
        let slot = tmp_slot_dir("wf", "on_tool_result");
        // 01 appends, 02 defers (nil), 03 appends again.
        std::fs::write(
            slot.join("01.lua"),
            "return function(ctx) return ctx.value .. ' A' end",
        )
        .unwrap();
        std::fs::write(slot.join("02.lua"), "return function() return nil end").unwrap();
        std::fs::write(
            slot.join("03.lua"),
            "return function(ctx) return ctx.value .. ' C' end",
        )
        .unwrap();
        let runner = HookRunner::new(lua_runtime(), base);
        let out = runner
            .dispatch(
                "on_tool_result",
                HookMode::Waterfall,
                serde_json::json!({ "value": "seed" }),
            )
            .await
            .unwrap();
        match out {
            DispatchOutput::Chained(Some(v)) => {
                assert_eq!(v, Value::String("seed A C".into()));
            }
            other => panic!("waterfall should return Chained, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn waterfall_short_circuits() {
        let base = tmp_hooks_dir("wfsc");
        let slot = tmp_slot_dir("wfsc", "on_tool_result");
        std::fs::write(
            slot.join("01.lua"),
            "return function(ctx) return '__SHORTCIRCUIT__stopped' end",
        )
        .unwrap();
        // 02 must NOT run.
        std::fs::write(
            slot.join("02.lua"),
            "return function(ctx) error('must not run after short-circuit') end",
        )
        .unwrap();
        let runner = HookRunner::new(lua_runtime(), base);
        let out = runner
            .dispatch("on_tool_result", HookMode::Waterfall, Value::Null)
            .await
            .unwrap();
        match out {
            DispatchOutput::Chained(Some(v)) => assert_eq!(v, Value::String("stopped".into())),
            other => panic!("waterfall short-circuit should return Chained, got {other:?}"),
        }
    }

    // ── Parallel: all results kept in order, nulls included ──

    #[tokio::test]
    async fn parallel_keeps_all_results_in_order() {
        let base = tmp_hooks_dir("par");
        let slot = tmp_slot_dir("par", "on_tool_result");
        std::fs::write(slot.join("01.lua"), "return function() return 'a' end").unwrap();
        std::fs::write(slot.join("02.lua"), "return function() return nil end").unwrap();
        std::fs::write(slot.join("03.lua"), "return function() return {ok=true} end").unwrap();
        let runner = HookRunner::new(lua_runtime(), base);
        match runner
            .dispatch("on_tool_result", HookMode::Parallel, Value::Null)
            .await
            .unwrap()
        {
            DispatchOutput::All(out) => {
                assert_eq!(out.len(), 3, "parallel keeps a slot per listener");
                assert_eq!(out[0], Value::String("a".into()));
                assert!(out[1].is_null(), "null slot preserved in order");
                assert_eq!(out[2], serde_json::json!({ "ok": true }));
            }
            other => panic!("parallel should return All, got {other:?}"),
        }
    }

    // ── Serial: first non-null wins, rest skipped ──

    #[tokio::test]
    async fn serial_first_non_null_wins_and_stops() {
        let base = tmp_hooks_dir("serial");
        let slot = tmp_slot_dir("serial", "on_pre_tool");
        // 01 defers (nil), 02 vetoes (true) → wins, 03 must NOT run.
        std::fs::write(slot.join("01.lua"), "return function() return nil end").unwrap();
        std::fs::write(slot.join("02.lua"), "return function() return true end").unwrap();
        std::fs::write(
            slot.join("03.lua"),
            "return function() error('must not run after a verdict') end",
        )
        .unwrap();
        let runner = HookRunner::new(lua_runtime(), base);
        let out = runner
            .dispatch("on_pre_tool", HookMode::Serial, Value::Null)
            .await
            .unwrap();
        match out {
            DispatchOutput::First(Some(v)) => assert_eq!(v, Value::Bool(true)),
            other => panic!("serial should return First, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn serial_no_verdict_is_none() {
        let base = tmp_hooks_dir("serial-none");
        let slot = tmp_slot_dir("serial-none", "on_pre_tool");
        std::fs::write(slot.join("01.lua"), "return function() return nil end").unwrap();
        let runner = HookRunner::new(lua_runtime(), base);
        let out = runner
            .dispatch("on_pre_tool", HookMode::Serial, Value::Null)
            .await
            .unwrap();
        assert_eq!(out, DispatchOutput::First(None));
    }

    // ── Sync path (prompt assembly) uses the same modes ──

    #[test]
    fn dispatch_sync_waterfall_matches_async() {
        let base = tmp_hooks_dir("sync-wf");
        let slot = tmp_slot_dir("sync-wf", "on_prompt_build");
        std::fs::write(
            slot.join("01.lua"),
            "return function(ctx) return ctx.value .. ' [tail]' end",
        )
        .unwrap();
        let runner = HookRunner::new(lua_runtime(), base);
        let out = runner
            .dispatch_sync(
                "on_prompt_build",
                HookMode::Waterfall,
                serde_json::json!({ "value": "BASE" }),
            )
            .unwrap();
        match out {
            DispatchOutput::Chained(Some(v)) => assert_eq!(v, Value::String("BASE [tail]".into())),
            other => panic!("sync waterfall should return Chained, got {other:?}"),
        }
    }
}
