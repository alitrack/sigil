//! Agent-loop Lua hooks (P-A of the thin-core roadmap: LuaJIT as the
//! plugin layer, Rust as the thin shell).
//!
//! Hook files live in `<hooks-dir>/<name>.lua` (default `~/.sigil/hooks/`)
//! and follow the same contract as scripted tools: the file must return
//! a function that takes the context table as its single argument. The
//! function may return:
//!   - a string  → replacement / injection (what the agent loop uses),
//!   - nil       → no-op (agent loop keeps its default behavior).
//!
//! Absent hook files are a zero-cost no-op (one `path.exists()` check).
//! Hook errors are surfaced, never silently swallowed — hooks are
//! user-owned behavior and must not fail invisibly.
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

    /// Run `<name>.lua` from the hooks dir, if present.
    ///
    /// Returns `Ok(Some(text))` when the hook returned a string,
    /// `Ok(None)` when the file is absent or the hook returned nil.
    pub async fn run(&self, name: &str, ctx: Value) -> Result<Option<String>> {
        let path = self.hooks_dir.join(format!("{name}.lua"));
        if !path.exists() {
            return Ok(None);
        }
        let lua = self.lua.clone();
        let path = path.clone();
        let name = name.to_string();
        // Lua execution is synchronous — run it off the async runtime
        // (same pattern as scripted tools).
        let text = tokio::task::spawn_blocking(move || {
            let lua = lua.lock().expect("lua lock poisoned");
            run_hook_file(&lua, &path, &name, &ctx.to_string())
        })
        .await
        .context("lua hook thread")??;
        Ok(hook_result(text))
    }

    /// Synchronous variant for millisecond-scale hot paths (prompt
    /// assembly). Blocks the caller instead of hopping to a blocking
    /// thread; do not use for slow hooks.
    pub fn run_sync(&self, name: &str, ctx: Value) -> Result<Option<String>> {
        let path = self.hooks_dir.join(format!("{name}.lua"));
        if !path.exists() {
            return Ok(None);
        }
        let lua = self.lua.lock().expect("lua lock poisoned");
        let text = run_hook_file(&lua, &path, name, &ctx.to_string())?;
        Ok(hook_result(text))
    }
}

/// Load a hook file and call its exported function with the JSON ctx.
fn run_hook_file(
    lua: &LuaRuntime,
    path: &std::path::Path,
    name: &str,
    ctx_json: &str,
) -> Result<String> {
    lua.run_file(path.to_str().unwrap_or(name), ctx_json)
}

/// Interpret the JSON-serialized Lua result: `null` → no-op, quoted
/// string → replacement, anything else → passed through verbatim.
fn hook_result(text: String) -> Option<String> {
    let trimmed = text.trim();
    if trimmed == "null" {
        return None;
    }
    let text = serde_json::from_str::<String>(trimmed).unwrap_or_else(|_| trimmed.to_string());
    Some(text)
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
        // The hook runs in the restricted state: `host.env` works,
        // raw `io` does not exist.
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
}
