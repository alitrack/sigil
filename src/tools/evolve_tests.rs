//! Evolution tools tests: create_lua_tool (hot registration) and
//! improve_harness (AHE instruction-layer mutation).

use serde_json::json;

use crate::tools::Tool;
use crate::tools::evolve::{CreateLuaToolTool, ImproveHarnessTool};

/// Unique temp dir per test (parallel-test safety).
fn fresh_dir(tag: &str) -> std::path::PathBuf {
    let dir = std::env::temp_dir().join(format!(
        "sigil-evolve-{tag}-{}-{:?}",
        std::process::id(),
        std::thread::current().id()
    ));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

/// Hot-queue alias (matches ToolRegistry's).
type HotQueue = std::sync::Arc<std::sync::Mutex<Vec<(String, std::path::PathBuf)>>>;

fn make_tool(dir: std::path::PathBuf) -> (CreateLuaToolTool, HotQueue) {
    let queue = HotQueue::default();
    let q = queue.clone();
    (
        CreateLuaToolTool {
            tools_dir: dir,
            register: Box::new(move |name: &str, path: std::path::PathBuf| {
                if let Ok(mut q) = q.try_lock() {
                    q.push((name.to_string(), path));
                }
            }),
        },
        queue,
    )
}

#[tokio::test]
async fn creates_lua_tool_and_hot_registers() {
    let dir = fresh_dir("create");
    let (tool, queue) = make_tool(dir.clone());

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

#[tokio::test]
async fn improve_harness_edits_prompt() {
    let dir = fresh_dir("ahe-prompt");
    let harness = dir.join("harness");
    std::fs::create_dir_all(&harness).unwrap();
    std::fs::write(harness.join("system_prompt.md"), "original prompt").unwrap();

    let tool = ImproveHarnessTool {
        harness_dir: harness.clone(),
    };
    let out = tool
        .run(&json!({
            "action": "edit_prompt",
            "content": "original prompt\n\nAlways answer in Chinese."
        }))
        .await
        .unwrap();
    assert!(out.text.contains("system_prompt.md updated"));

    let new = std::fs::read_to_string(harness.join("system_prompt.md")).unwrap();
    assert!(new.contains("Always answer in Chinese."));

    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test]
async fn improve_harness_adds_and_lists_skills() {
    let dir = fresh_dir("ahe-skill");
    let harness = dir.join("harness");
    std::fs::create_dir_all(harness.join("skills")).unwrap();

    let tool = ImproveHarnessTool {
        harness_dir: harness.clone(),
    };

    // Add a skill.
    let out = tool
        .run(&json!({
            "action": "add_skill",
            "name": "my-skill",
            "content": "> A reusable procedure\n\n1. step one\n2. step two"
        }))
        .await
        .unwrap();
    assert!(out.text.contains("my-skill"));
    assert!(harness.join("skills").join("my-skill.md").exists());

    // Unsafe name is sanitized (slashes/dots/! dropped).
    let out2 = tool
        .run(&json!({
            "action": "add_skill",
            "name": "../../bad!name",
            "content": "> x\n\nbody"
        }))
        .await
        .unwrap();
    assert!(out2.text.contains("badname"));

    // List includes both.
    let listed = tool.run(&json!({ "action": "list_skills" })).await.unwrap();
    assert!(listed.text.contains("my-skill"));
    assert!(listed.text.contains("badname"));

    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test]
async fn improve_harness_rejects_unknown_action() {
    let dir = fresh_dir("ahe-unknown");
    let harness = dir.join("harness");
    std::fs::create_dir_all(&harness).unwrap();

    let tool = ImproveHarnessTool {
        harness_dir: harness.clone(),
    };
    let err = tool
        .run(&json!({ "action": "nuke_everything" }))
        .await
        .unwrap_err();
    assert!(err.to_string().contains("unknown action"));

    let _ = std::fs::remove_dir_all(&dir);
}
