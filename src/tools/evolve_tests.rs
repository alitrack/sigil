//! Unit tests for evolution tools: creating + hot-registering Lua tools.

use std::sync::Arc;

use serde_json::json;
use tokio::sync::Mutex;

use crate::tools::Tool;
use crate::tools::evolve::CreateLuaToolTool;

fn fresh_dir(tag: &str) -> std::path::PathBuf {
    let dir = std::env::temp_dir().join(format!("sigil-evolve-{tag}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

type HotQueue = Arc<Mutex<Vec<(String, std::path::PathBuf)>>>;

fn make_tool(dir: std::path::PathBuf) -> (CreateLuaToolTool, HotQueue) {
    let queue: HotQueue = Arc::default();
    let q = queue.clone();
    let tool = CreateLuaToolTool {
        tools_dir: dir,
        register: Box::new(move |name: &str, path: std::path::PathBuf| {
            if let Ok(mut q) = q.try_lock() {
                q.push((name.to_string(), path));
            }
        }),
    };
    (tool, queue)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn creates_and_hot_registers_tool() {
        let dir = fresh_dir("create");
        let (tool, queue) = make_tool(dir.clone());

        assert_eq!(tool.name(), "create_lua_tool");

        let out = tool
            .run(&json!({
                "name": "hello_tool",
                "code": "return function(args) return 'hello ' .. (args.who or 'world') end"
            }))
            .await
            .unwrap();

        // File written to tools dir.
        assert!(dir.join("hello_tool.lua").exists());
        // Hot-register queued.
        let queued = queue.try_lock().unwrap().clone();
        assert_eq!(queued.len(), 1);
        assert_eq!(queued[0].0, "hello_tool");
        assert!(out.text.contains("hello_tool"));

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn sanitizes_unsafe_tool_names() {
        let dir = fresh_dir("sanitize");
        let (tool, _queue) = make_tool(dir.clone());

        // Name with path traversal chars gets sanitized to safe chars.
        let out = tool
            .run(&json!({
                "name": "../../evil!name",
                "code": "return function() return 1 end"
            }))
            .await
            .unwrap();
        assert!(out.text.contains("evilname"));
        assert!(dir.join("evilname.lua").exists());

        let _ = std::fs::remove_dir_all(&dir);
    }
}
