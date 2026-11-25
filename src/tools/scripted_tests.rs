//! Unit tests for scripted Lua tools.

use std::path::PathBuf;
use std::sync::Arc;

use serde_json::json;
use tokio::sync::Mutex;

use crate::lua::LuaRuntime;
use crate::tools::Tool;
use crate::tools::scripted::{LuaTool, discover};

#[cfg(test)]
mod tests {
    use super::*;

    fn lua_rt() -> Arc<Mutex<LuaRuntime>> {
        Arc::new(Mutex::new(LuaRuntime::new().unwrap()))
    }

    #[test]
    fn discovers_lua_files_only() {
        let dir = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("test-tools");
        let tools = discover(&dir);
        let names: Vec<String> = tools
            .iter()
            .filter_map(|p| p.file_stem().and_then(|s| s.to_str()).map(String::from))
            .collect();
        assert!(names.contains(&"crc32".to_string()));
        assert!(names.contains(&"whoami".to_string()));
    }

    /// crc32.lua needs the host's zlib (`ffi.load('z')`); hosts without
    /// it (stock Windows) skip the test instead of going red.
    #[cfg(feature = "lua-ffi")]
    fn zlib_available() -> bool {
        let rt = lua_rt();
        let Ok(guard) = rt.try_lock() else {
            return false;
        };
        guard
            .run(
                "local ffi = require('ffi')
                 local ok = pcall(ffi.load, 'z')
                 return tostring(ok)",
            )
            .map(|s| s.trim() == "true")
            .unwrap_or(false)
    }

    #[tokio::test]
    #[cfg(feature = "lua-ffi")]
    async fn runs_scripted_tool_with_ffi() {
        if !zlib_available() {
            eprintln!("skipping: host has no zlib for ffi.load('z') (crc32.lua needs it)");
            return;
        }
        let dir = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("test-tools");
        let tool = LuaTool::new("crc32", &dir.join("crc32.lua"), lua_rt()).unwrap();
        assert!(tool.description().contains("CRC32"));
        let out = tool.run(&json!({ "text": "hello" })).await.unwrap();
        assert_eq!(out.text, "0x3610A686"); // CRC32("hello") — IEEE 802.3 standard value
    }

    #[tokio::test]
    async fn runs_scripted_tool_with_host_helpers() {
        let dir = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("test-tools");
        let tool = LuaTool::new("whoami", &dir.join("whoami.lua"), lua_rt()).unwrap();
        let out = tool.run(&json!({})).await.unwrap();
        assert!(out.text.contains("time="));
        assert!(out.text.contains("host="));
    }
}
