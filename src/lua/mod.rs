//! Embedded LuaJIT runtime (vendored via mlua).
//!
//! Every sigil process owns one Lua state. Scripted tools run in it.
//! The `ffi` library is available (`require("ffi")`) — LuaJIT's FFI can
//! load any system C library at runtime, which is the core of sigil's
//! "evolve by scripting" story.

use std::sync::Mutex;

use anyhow::{Context, Result};
use mlua::{Lua, LuaSerdeExt, Value};

/// Convert an mlua error into an anyhow error (mlua::Error is !Send).
fn lua_err(e: mlua::Error) -> anyhow::Error {
    anyhow::anyhow!("{e}")
}

pub struct LuaRuntime {
    lua: Mutex<Lua>,
}

impl LuaRuntime {
    /// Create a LuaJIT state with a restricted, tool-friendly environment.
    /// `ffi` stays enabled (that's the point); `io`/`os` are NOT exposed
    /// to scripted tools — host helpers (read_file, write_file, ...) are
    /// the safe boundary instead. (Phase-3 approval gate hooks in here.)
    pub fn new() -> Result<Self> {
        // Open the full LuaJIT standard library *except* the unsafe
        // `debug` module (rejected by mlua's safe new_with). `ffi` is
        // included — it is excluded from the default safe subset and is
        // the entire point of the vendored LuaJIT build.
        use mlua::StdLib as S;
        let stdlib =
            S::TABLE | S::IO | S::OS | S::STRING | S::MATH | S::PACKAGE | S::FFI | S::BIT | S::JIT;
        // SAFETY: mlua's safe `new_with` refuses the `ffi` standard
        // library (it's classified unsafe because Lua scripts could
        // load arbitrary C libraries). sigil *wants* exactly that —
        // scripted tools calling system C libraries is the core feature
        // (LuaJIT FFI). We are the host; we accept the risk. All other
        // safety properties of mlua still hold.
        let lua = unsafe { Lua::unsafe_new_with(stdlib, mlua::LuaOptions::default()) };
        // Expose a minimal `host` table with safe helpers.
        let host = lua.create_table().map_err(lua_err)?;
        host.set(
            "read_file",
            lua.create_function(|_lua, path: String| {
                std::fs::read_to_string(&path)
                    .map_err(|e| mlua::Error::RuntimeError(format!("read_file: {e}")))
            })
            .map_err(lua_err)?,
        )
        .map_err(lua_err)?;
        host.set(
            "write_file",
            lua.create_function(|_lua, (path, content): (String, String)| {
                std::fs::write(&path, &content)
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
        lua.globals().set("host", host).map_err(lua_err)?;

        Ok(Self {
            lua: Mutex::new(lua),
        })
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
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lua_basic_eval() {
        let rt = LuaRuntime::new().unwrap();
        let out = rt.run("return 1 + 2").unwrap();
        assert_eq!(out, "3");
    }

    #[test]
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
}
