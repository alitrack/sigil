//! Embedded LuaJIT runtime (vendored via mlua).
//!
//! Every sigil process owns one Lua state. Scripted tools run in it.
//! The `ffi` library is available (`require("ffi")`) — LuaJIT's FFI can
//! load any system C library at runtime, which is the core of sigil's
//! "evolve by scripting" story.

use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use mlua::{Lua, LuaSerdeExt, Value};

/// Convert an mlua error into an anyhow error (mlua::Error is !Send).
fn lua_err(e: mlua::Error) -> anyhow::Error {
    anyhow::anyhow!("{e}")
}

pub struct LuaRuntime {
    lua: Mutex<Lua>,
    budget: Arc<Mutex<Option<ExecutionBudget>>>,
}

struct ExecutionBudget {
    deadline: Instant,
    instructions_left: u64,
}

impl ExecutionBudget {
    fn check(&self) -> mlua::Result<()> {
        if self.instructions_left == 0 || Instant::now() >= self.deadline {
            return Err(mlua::Error::RuntimeError(
                "Lua execution budget exceeded".into(),
            ));
        }
        Ok(())
    }
}

impl LuaRuntime {
    /// Create a LuaJIT state with a restricted, tool-friendly environment.
    /// `ffi` stays enabled (that's the point); `io`/`os` are NOT exposed
    /// to scripted tools — host helpers (read_file, write_file, ...) are
    /// the safe boundary instead, and `require` is wrapped in an
    /// allowlist (borrowed from smolagents' `authorized_imports`) so a
    /// script can only pull in built-in modules, never `io`/`os` or
    /// anything off the filesystem. (Phase-3 approval gate hooks in here.)
    pub fn new() -> Result<Self> {
        // Standard library *without* the unsafe `io` and `os` modules
        // (file/process access — scripts must go through host helpers)
        // and without `debug` (rejected by mlua's safe new_with).
        //
        // `ffi` is a build mode (default ON): scripted tools calling
        // system C libraries is the core self-evolution capability. A
        // containment build (`--no-default-features`) drops it — then
        // no Lua script can load arbitrary C libraries.
        use mlua::StdLib as S;
        let stdlib = S::TABLE | S::STRING | S::MATH | S::PACKAGE | S::BIT | S::JIT;
        #[cfg(feature = "lua-ffi")]
        let stdlib = stdlib | S::FFI;
        // SAFETY: mlua's safe `new_with` refuses the `ffi` standard
        // library (it's classified unsafe because Lua scripts could
        // load arbitrary C libraries). sigil *wants* exactly that in
        // the default build — we are the host; we accept the risk. The
        // containment build uses the safe constructor, which also disables
        // package.loadlib and native module searchers.
        #[cfg(feature = "lua-ffi")]
        let lua = unsafe { Lua::unsafe_new_with(stdlib, mlua::LuaOptions::default()) };
        #[cfg(not(feature = "lua-ffi"))]
        let lua = Lua::new_with(stdlib, mlua::LuaOptions::default()).map_err(lua_err)?;

        // Count hooks do not reliably interrupt JIT traces. Disable the engine
        // and expose only the harmless JIT controls, so scripts cannot turn it
        // back on. FFI/native calls remain outside this cooperative CPU budget.
        lua.load(
            r#"
            jit.off()
            jit.flush()
            jit = {
                off = jit.off, flush = jit.flush, status = jit.status,
                version = jit.version, version_num = jit.version_num,
                os = jit.os, arch = jit.arch,
            }
            package.loaded.jit = jit
            package.preload.jit = nil
        "#,
        )
        .exec()
        .map_err(lua_err)?;
        let budget: Arc<Mutex<Option<ExecutionBudget>>> = Arc::new(Mutex::new(None));
        // Hook errors can be caught by Lua pcall/xpcall. Check the same budget
        // after protected calls return, outside their protected Lua frame.
        for name in ["pcall", "xpcall"] {
            let original: mlua::Function = lua.globals().get(name).map_err(lua_err)?;
            let state = budget.clone();
            let wrapped = lua
                .create_function(move |_, args: mlua::MultiValue| {
                    if let Some(budget) = state.lock().unwrap().as_ref() {
                        budget.check()?;
                    }
                    let result = original.call::<mlua::MultiValue>(args);
                    if let Some(budget) = state.lock().unwrap().as_ref() {
                        budget.check()?;
                    }
                    result
                })
                .map_err(lua_err)?;
            lua.globals().set(name, wrapped).map_err(lua_err)?;
        }
        // Containment build: LuaJIT registers `ffi` in package.preload
        // even when the stdlib flag is absent — strip the loaders so
        // `require("ffi")` cannot reach it (belt and suspenders on top
        // of the require allowlist below).
        #[cfg(not(feature = "lua-ffi"))]
        {
            if let Ok(pkg) = lua.globals().get::<mlua::Table>("package") {
                if let Ok(preload) = pkg.get::<mlua::Table>("preload") {
                    let _ = preload.set("ffi", mlua::Nil);
                }
                if let Ok(loaded) = pkg.get::<mlua::Table>("loaded") {
                    let _ = loaded.set("ffi", mlua::Nil);
                }
            }
        }
        // Restricted `require`: only allowlisted built-in modules may be
        // loaded (smolagents' `authorized_imports` pattern). Without
        // this, `require` can walk the filesystem loaders and load
        // arbitrary Lua from disk — a file-read side channel.
        {
            let real_require: mlua::Function = lua.globals().get("require").map_err(lua_err)?;
            let wrapped = lua
                .create_function(move |_lua, name: String| {
                    #[cfg(feature = "lua-ffi")]
                    const ALLOWED: &[&str] =
                        &["ffi", "string", "table", "math", "bit", "jit", "coroutine"];
                    #[cfg(not(feature = "lua-ffi"))]
                    const ALLOWED: &[&str] =
                        &["string", "table", "math", "bit", "jit", "coroutine"];
                    if !ALLOWED.contains(&name.as_str()) {
                        #[cfg(feature = "lua-ffi")]
                        let list = "ffi, string, table, math, bit, jit, coroutine";
                        #[cfg(not(feature = "lua-ffi"))]
                        let list = "string, table, math, bit, jit, coroutine";
                        #[cfg(feature = "lua-ffi")]
                        let extra = "";
                        #[cfg(not(feature = "lua-ffi"))]
                        let extra = " (this is a containment build: FFI is disabled)";
                        return Err(mlua::Error::RuntimeError(format!(
                            "unauthorized require('{name}'): only built-in modules are \
                             allowed ({list}).{extra} \
                             File access goes through host helpers."
                        )));
                    }
                    real_require.call::<mlua::Value>(name)
                })
                .map_err(lua_err)?;
            lua.globals().set("require", wrapped).map_err(lua_err)?;
        }
        // Expose a minimal `host` table with safe helpers.
        let host = lua.create_table().map_err(lua_err)?;
        host.set(
            "read_file",
            lua.create_function(|_lua, path: String| {
                std::fs::read_to_string(crate::agent::provider::resolve_path(std::path::Path::new(
                    &path,
                )))
                .map_err(|e| mlua::Error::RuntimeError(format!("read_file: {e}")))
            })
            .map_err(lua_err)?,
        )
        .map_err(lua_err)?;
        host.set(
            "write_file",
            lua.create_function(|_lua, (path, content): (String, String)| {
                std::fs::write(
                    crate::agent::provider::resolve_path(std::path::Path::new(&path)),
                    &content,
                )
                .map_err(|e| mlua::Error::RuntimeError(format!("write_file: {e}")))?;
                Ok(())
            })
            .map_err(lua_err)?,
        )
        .map_err(lua_err)?;
        host.set(
            "env",
            lua.create_function(|_lua, key: String| Ok(std::env::var(key).unwrap_or_default()))
                .map_err(lua_err)?,
        )
        .map_err(lua_err)?;
        host.set(
            "list_dir",
            lua.create_function(|_lua, dir: String| {
                let mut names: Vec<String> = Vec::new();
                let entries = std::fs::read_dir(crate::agent::provider::resolve_path(
                    std::path::Path::new(&dir),
                ))
                .map_err(|e| mlua::Error::RuntimeError(format!("list_dir: {e}")))?;
                for e in entries.flatten() {
                    names.push(e.file_name().to_string_lossy().into_owned());
                }
                names.sort();
                Ok(names)
            })
            .map_err(lua_err)?,
        )
        .map_err(lua_err)?;
        host.set(
            "cwd",
            lua.create_function(|_lua, ()| {
                Ok(crate::agent::provider::execution_context()
                    .map(|context| context.cwd)
                    .or_else(|| std::env::current_dir().ok())
                    .map(|p| p.to_string_lossy().into_owned())
                    .unwrap_or_default())
            })
            .map_err(lua_err)?,
        )
        .map_err(lua_err)?;
        host.set(
            "json_encode",
            lua.create_function(|lua, value: mlua::Value| {
                let json = lua
                    .to_value(&value)
                    .map_err(|e| mlua::Error::RuntimeError(format!("json_encode: {e}")))?;
                serde_json::to_string(&json)
                    .map_err(|e| mlua::Error::RuntimeError(format!("json_encode: {e}")))
            })
            .map_err(lua_err)?,
        )
        .map_err(lua_err)?;
        host.set(
            "json_decode",
            lua.create_function(|lua, text: String| {
                let json: serde_json::Value = serde_json::from_str(&text)
                    .map_err(|e| mlua::Error::RuntimeError(format!("json_decode: {e}")))?;
                lua.to_value(&json)
                    .map_err(|e| mlua::Error::RuntimeError(format!("json_decode: {e}")))
            })
            .map_err(lua_err)?,
        )
        .map_err(lua_err)?;
        host.set(
            "date",
            lua.create_function(|_lua, fmt: Option<String>| {
                let now = std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .unwrap_or_default();
                let secs = now.as_secs();
                let days = secs / 86_400;
                let rem = secs % 86_400;
                let (h, mi, s) = (rem / 3600, (rem % 3600) / 60, rem % 60);
                // Civil date from days-since-epoch (Howard Hinnant's
                // algorithm — no chrono dependency needed).
                let z = days as i64 + 719_468;
                let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
                let doe = (z - era * 146_097) as u64;
                let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
                let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
                let mp = (5 * doy + 2) / 153;
                let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
                let mon = (if mp < 10 { mp + 3 } else { mp - 9 }) as u32;
                let y = (yoe + era as u64 * 400 + (if mon <= 2 { 1 } else { 0 })) as u32;
                let out = match fmt.as_deref() {
                    Some(f) if !f.is_empty() => f.to_string(),
                    _ => "%Y-%m-%d %H:%M:%S".to_string(),
                };
                Ok(out
                    .replace("%Y", &format!("{y:04}"))
                    .replace("%m", &format!("{mon:02}"))
                    .replace("%d", &format!("{d:02}"))
                    .replace("%H", &format!("{h:02}"))
                    .replace("%M", &format!("{mi:02}"))
                    .replace("%S", &format!("{s:02}")))
            })
            .map_err(lua_err)?,
        )
        .map_err(lua_err)?;
        lua.globals().set("host", host).map_err(lua_err)?;

        Ok(Self {
            lua: Mutex::new(lua),
            budget,
        })
    }

    /// Bound both chunk initialization and the exported function. The lock
    /// serializes state access; the budget starts only after obtaining it.
    fn execute<T>(&self, lua: &Lua, run: impl FnOnce() -> Result<T>) -> Result<T> {
        const HOOK_INTERVAL: u64 = 1000;
        *self.budget.lock().unwrap() = Some(ExecutionBudget {
            deadline: Instant::now() + Duration::from_secs(5),
            instructions_left: 10_000_000,
        });
        let state = self.budget.clone();
        let hook = lua
            .set_global_hook(
                mlua::HookTriggers::new().every_nth_instruction(HOOK_INTERVAL as u32),
                move |_, _| {
                    if let Some(budget) = state.lock().unwrap().as_mut() {
                        budget.instructions_left =
                            budget.instructions_left.saturating_sub(HOOK_INTERVAL);
                        budget.check()?;
                    }
                    Ok(mlua::VmState::Continue)
                },
            )
            .map_err(lua_err);
        let result = hook.and_then(|_| run()).and_then(|value| {
            if let Some(budget) = self.budget.lock().unwrap().as_ref() {
                budget.check().map_err(lua_err)?;
            }
            Ok(value)
        });
        lua.remove_global_hook();
        lua.remove_hook();
        *self.budget.lock().unwrap() = None;
        result
    }

    /// Run a Lua chunk, returning its JSON-serialized result.
    /// (Phase 3: the self-evolution harness evaluates Lua snippets
    /// through this path.)
    #[allow(dead_code)]
    pub fn run(&self, source: &str) -> Result<String> {
        let lua = self
            .lua
            .lock()
            .map_err(|_| anyhow::anyhow!("lua lock poisoned"))?;
        self.execute(&lua, || {
            let result: Value = lua
                .load(source)
                .eval()
                .map_err(lua_err)
                .with_context(|| format!("lua eval failed for source: {source}"))?;
            let json = lua
                .to_value(&result)
                .map_err(lua_err)
                .context("lua value → JSON")?;
            let text = serde_json::to_string(&json).context("JSON serialize")?;
            Ok(text)
        })
    }

    /// Load a Lua file, get its exported function, call it with the
    /// JSON args, and serialize the result.
    ///
    /// Script contract: the file must return a function that takes one
    /// argument (the args table) and returns a result.
    pub fn run_file(&self, path: &str, args_json: &str) -> Result<String> {
        let lua = self
            .lua
            .lock()
            .map_err(|_| anyhow::anyhow!("lua lock poisoned"))?;
        self.execute(&lua, || {
            let source = std::fs::read_to_string(path)?;
            // 1. Evaluate the file → get the exported function.
            let exported: Value = lua.load(&source).set_name(path).eval().map_err(lua_err)?;
            let func: mlua::Function = exported
                .as_function()
                .cloned()
                .ok_or_else(|| anyhow::anyhow!("scripted tool {path} must return a function"))?;
            // 2. Call it with the args table.
            let args: serde_json::Value =
                serde_json::from_str(args_json).unwrap_or(serde_json::json!({}));
            let result: Value = func
                .call(lua.to_value(&args).map_err(lua_err)?)
                .map_err(lua_err)?;
            let json = lua.to_value(&result).map_err(lua_err)?;
            Ok(serde_json::to_string(&json)?)
        })
    }

    /// Like `run_file` but returns the hook's return value as a typed
    /// `serde_json::Value` instead of a string. Needed by the agent-loop
    /// hook dispatch modes that consume non-string results: `parallel`
    /// (a table of per-monitor outputs) and `serial` (a boolean veto).
    ///
    /// A Lua `nil` return serializes to `Value::Null` — call sites that
    /// distinguish "absent" from "present but null" should check their
    /// own discovery logic first (see `hooks::HookRunner::resolve_slot`).
    pub fn run_file_value(&self, path: &str, args_json: &str) -> Result<serde_json::Value> {
        let lua = self
            .lua
            .lock()
            .map_err(|_| anyhow::anyhow!("lua lock poisoned"))?;
        self.execute(&lua, || {
            let source = std::fs::read_to_string(path)?;
            let exported: Value = lua.load(&source).set_name(path).eval().map_err(lua_err)?;
            let func: mlua::Function = exported
                .as_function()
                .cloned()
                .ok_or_else(|| anyhow::anyhow!("scripted tool {path} must return a function"))?;
            let args: serde_json::Value =
                serde_json::from_str(args_json).unwrap_or(serde_json::json!({}));
            let result: Value = func
                .call(lua.to_value(&args).map_err(lua_err)?)
                .map_err(lua_err)?;
            // Serialize the Lua value to a JSON string, then parse it into a
            // typed serde value. (Reuses the same serde round-trip the string
            // variant of run_file relies on, so tables/bools/nil all land as
            // the corresponding JSON object/bool/null.)
            let json = lua.to_value(&result).map_err(lua_err)?;
            let s = serde_json::to_string(&json)?;
            Ok(serde_json::from_str(&s)?)
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    #[cfg(not(feature = "lua-ffi"))]
    fn containment_disables_native_library_loaders() {
        let rt = LuaRuntime::new().unwrap();
        assert_eq!(
            rt.run("local ok = pcall(package.loadlib, '/not/a/library', '*'); return ok")
                .unwrap(),
            "false"
        );
        assert_eq!(rt.run("local loader = package.loaders[3]; return loader and type(loader('native')) == 'string'").unwrap(), "true");
    }

    #[test]
    fn scripts_cannot_enable_jit_and_evade_instruction_hooks() {
        let rt = LuaRuntime::new().unwrap();
        assert_eq!(rt.run("local j=require('jit'); local ok=pcall(function() j.on() end); return ok or j.status()").unwrap(), "false");
        assert_eq!(
            rt.run("local ok = pcall(function() jit.on() end); return ok or jit.status()")
                .unwrap(),
            "false"
        );
    }

    #[test]
    fn lua_execution_budget_interrupts_infinite_loop() {
        const PROBE: &str = "SIGIL_LUA_TIMEOUT_PROBE";
        if std::env::var_os(PROBE).is_some() {
            let rt = LuaRuntime::new().unwrap();
            for source in [
                "while true do end",
                "local co = coroutine.create(function() while true do end end); coroutine.resume(co)",
                "while true do pcall(function() while true do end end) end",
                "while true do xpcall(function() while true do end end, function() return 'caught' end) end",
            ] {
                let error = rt.run(source).unwrap_err();
                assert!(format!("{error:#}").contains("execution budget"));
                assert_eq!(rt.run("return 42").unwrap(), "42");
            }
            let dir = std::env::temp_dir().join(format!("sigil-budget-{}", std::process::id()));
            std::fs::create_dir_all(&dir).unwrap();
            let path = dir.join("loop.lua");
            std::fs::write(&path, "return function() while true do end end").unwrap();
            assert!(
                format!(
                    "{:#}",
                    rt.run_file(path.to_str().unwrap(), "{}").unwrap_err()
                )
                .contains("execution budget")
            );
            assert!(
                format!(
                    "{:#}",
                    rt.run_file_value(path.to_str().unwrap(), "{}").unwrap_err()
                )
                .contains("execution budget")
            );
            std::fs::write(
                &path,
                "while true do end; return function() return 'never' end",
            )
            .unwrap();
            assert!(
                format!(
                    "{:#}",
                    rt.run_file(path.to_str().unwrap(), "{}").unwrap_err()
                )
                .contains("execution budget")
            );
            std::fs::remove_dir_all(dir).unwrap();
            return;
        }
        let mut child = std::process::Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "lua::tests::lua_execution_budget_interrupts_infinite_loop",
                "--nocapture",
            ])
            .env(PROBE, "1")
            .spawn()
            .unwrap();
        let started = std::time::Instant::now();
        loop {
            if let Some(status) = child.try_wait().unwrap() {
                assert!(status.success(), "budget probe failed");
                break;
            }
            if started.elapsed() > std::time::Duration::from_secs(40) {
                child.kill().unwrap();
                child.wait().unwrap();
                panic!("Lua infinite loop did not respect execution budget");
            }
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
    }

    #[test]
    fn lua_basic_eval() {
        let rt = LuaRuntime::new().unwrap();
        let out = rt.run("return 1 + 2").unwrap();
        assert_eq!(out, "3");
    }

    #[test]
    fn run_file_value_typed_returns() {
        // Non-string returns (bool veto, table of outputs) are the whole
        // point of run_file_value; a bare `return nil` must surface as
        // Value::Null so dispatch modes can treat it as "no opinion".
        let dir = std::env::temp_dir().join("sigil-lua-value-tests");
        std::fs::create_dir_all(&dir).unwrap();
        let rt = LuaRuntime::new().unwrap();

        let p = dir.join("veto.lua");
        std::fs::write(&p, "return function(ctx) return ctx.block == true end").unwrap();
        assert_eq!(
            rt.run_file_value(p.to_str().unwrap(), r#"{"block":true}"#)
                .unwrap(),
            serde_json::Value::Bool(true)
        );
        assert_eq!(
            rt.run_file_value(p.to_str().unwrap(), r#"{"block":false}"#)
                .unwrap(),
            serde_json::Value::Bool(false)
        );

        let p = dir.join("tbl.lua");
        std::fs::write(&p, "return function(ctx) return {ok=true, n=3} end").unwrap();
        assert_eq!(
            rt.run_file_value(p.to_str().unwrap(), "{}").unwrap(),
            serde_json::json!({"ok": true, "n": 3})
        );

        let p = dir.join("nil.lua");
        std::fs::write(&p, "return function() return nil end").unwrap();
        assert!(
            rt.run_file_value(p.to_str().unwrap(), "{}")
                .unwrap()
                .is_null()
        );
    }

    #[test]
    #[cfg(feature = "lua-ffi")]
    fn lua_ffi_available() {
        // LuaJIT's FFI library must be require-able — this is the whole
        // point of the vendored LuaJIT build.
        let rt = LuaRuntime::new().unwrap();
        let out = rt
            .run("local ffi = require('ffi'); return type(ffi)")
            .unwrap();
        assert_eq!(out, "\"table\"");
    }

    #[test]
    #[cfg(feature = "lua-ffi")]
    fn lua_ffi_calls_system_libc() {
        // Real FFI: call strlen from libc. Exercises cdef + ffi.load +
        // cdata conversion in one shot.
        let rt = LuaRuntime::new().unwrap();
        let out = rt
            .run(
                r#"
local ffi = require('ffi')
ffi.cdef[[ int strlen(const char* s); ]]
local libc = ffi.load('c')
return tostring(libc.strlen('hello'))
"#,
            )
            .unwrap();
        assert_eq!(out, "\"5\"");
    }

    #[test]
    fn host_helpers_available() {
        let rt = LuaRuntime::new().unwrap();
        // write + read round-trip through host helpers.
        let dir = std::env::temp_dir();
        let path = dir.join(format!("sigil-host-helper-{}.txt", std::process::id()));
        let p = path.to_string_lossy();
        let out = rt
            .run(&format!(
                "host.write_file('{p}', 'hello helper'); return host.read_file('{p}')"
            ))
            .unwrap();
        assert_eq!(out, "\"hello helper\"");
        std::fs::remove_file(&path).ok();
    }

    #[test]
    fn host_json_helpers_roundtrip() {
        let rt = LuaRuntime::new().unwrap();
        let out = rt
            .run("local t = host.json_decode('[1, 2, 3]'); return host.json_encode(t)")
            .unwrap();
        assert_eq!(out, "\"[1,2,3]\"");
    }

    #[test]
    fn host_list_dir_and_cwd() {
        let rt = LuaRuntime::new().unwrap();
        // list_dir on the current dir contains at least Cargo.toml
        // (when the cwd is the project root) — the helper must return
        // a sorted array.
        let out = rt.run("return #host.list_dir(host.cwd()) > 0").unwrap();
        assert_eq!(out, "true");
    }

    #[test]
    fn io_and_os_not_exposed() {
        // The sandbox contract: scripts go through host helpers, so the
        // raw `io` / `os` standard libraries must be absent (they would
        // bypass the host boundary and the approval gate).
        let rt = LuaRuntime::new().unwrap();
        let io = rt.run("return io").unwrap();
        assert_eq!(io, "null");
        let os = rt.run("return os").unwrap();
        assert_eq!(os, "null");
    }

    #[test]
    fn require_allowlist_blocks_dangerous_modules() {
        // authorized_imports (smolagents pattern): only built-in modules
        // are require-able; `io` / `os` / anything off the filesystem
        // must be rejected with a clear error.
        let rt = LuaRuntime::new().unwrap();
        let err = rt.run("return require('io')").unwrap_err();
        // `{err:#}` (not to_string) because run() wraps errors with
        // context; the chain must contain the allowlist message.
        assert!(format!("{err:#}").contains("unauthorized require('io')"));
        let err2 = rt.run("return require('os')").unwrap_err();
        assert!(format!("{err2:#}").contains("unauthorized require('os')"));
    }

    #[test]
    #[cfg(feature = "lua-ffi")]
    fn require_allowlist_still_loads_ffi() {
        // The core feature survives the allowlist: ffi must remain
        // require-able.
        let rt = LuaRuntime::new().unwrap();
        let out = rt
            .run("local ffi = require('ffi'); return type(ffi)")
            .unwrap();
        assert_eq!(out, "\"table\"");
    }

    /// Containment build (`--no-default-features`): FFI must be
    /// unreachable — not via require, not via package.preload, not
    /// preloaded as a global. This is the sandbox contract that makes
    /// the no-FFI build suitable for untrusted environments.
    #[test]
    #[cfg(not(feature = "lua-ffi"))]
    fn containment_build_blocks_ffi() {
        let rt = LuaRuntime::new().unwrap();
        // require('ffi') is rejected by the allowlist...
        let err = rt.run("return require('ffi')").unwrap_err();
        assert!(
            format!("{err:#}").contains("unauthorized require('ffi')"),
            "allowlist must reject ffi: {err:#}"
        );
        assert!(
            format!("{err:#}").contains("containment build"),
            "error must name the build mode: {err:#}"
        );
        // ...and the loader is stripped, so the raw require (bypassing
        // our wrapper via package.preload) cannot reach it either.
        let out = rt
            .run("local p = package and package.preload; return p and p['ffi']")
            .unwrap();
        // Lua nil serializes to JSON null through run().
        assert_eq!(out, "null", "package.preload['ffi'] must be stripped");
        // No ffi global leaked into the sandbox.
        assert_eq!(rt.run("return ffi").unwrap(), "null");
    }

    #[test]
    fn host_date_formats_civil_date() {
        // host.date replaces os.date — default format and a custom one.
        let rt = LuaRuntime::new().unwrap();
        let out = rt.run("return host.date('%Y-%m-%d')").unwrap();
        // 8 digits, dashed, e.g. "2026-08-11" — matches ^\d{4}-\d{2}-\d{2}$
        let unquoted = out.trim_matches('"');
        assert_eq!(
            unquoted.len(),
            10,
            "date must be YYYY-MM-DD, got {unquoted}"
        );
        assert_eq!(unquoted.chars().filter(|c| *c == '-').count(), 2);
        assert!(unquoted.chars().all(|c| c.is_ascii_digit() || c == '-'));
    }
}
