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
type HotQueue = std::sync::Arc<std::sync::Mutex<Vec<crate::tools::HotOp>>>;

fn make_tool(dir: std::path::PathBuf) -> (CreateLuaToolTool, HotQueue) {
    let queue = HotQueue::default();
    let q = queue.clone();
    let q2 = queue.clone();
    (
        CreateLuaToolTool {
            tools_dir: dir,
            register: Box::new(move |name: &str, path: std::path::PathBuf| {
                if let Ok(mut q) = q.try_lock() {
                    q.push(crate::tools::HotOp::register(name, path));
                }
            }),
            unregister: Box::new(move |name: &str| {
                if let Ok(mut q) = q2.try_lock() {
                    q.push(crate::tools::HotOp::unregister(name));
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
    assert_eq!(queued[0].name, "hello_tool");
    assert!(queued[0].register);
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
async fn lists_scripted_tools() {
    let dir = fresh_dir("list");
    let (tool, _queue) = make_tool(dir.clone());

    // Empty dir → friendly message.
    let out = tool.run(&json!({ "action": "list" })).await.unwrap();
    assert!(out.text.contains("no scripted tools"));

    // Create one, then list shows it.
    tool.run(&json!({
        "name": "alpha",
        "code": "return function() return 'a' end"
    }))
    .await
    .unwrap();
    let out = tool.run(&json!({ "action": "list" })).await.unwrap();
    assert!(out.text.contains("alpha"));

    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test]
async fn updates_existing_tool() {
    let dir = fresh_dir("update");
    let (tool, queue) = make_tool(dir.clone());

    // Create.
    tool.run(&json!({
        "name": "greeter",
        "code": "return function() return 'v1' end"
    }))
    .await
    .unwrap();
    assert!(dir.join("greeter.lua").exists());

    // Update: overwrite + hot-register again (queue gets a second op).
    let out = tool
        .run(&json!({
            "action": "update",
            "name": "greeter",
            "code": "return function() return 'v2' end"
        }))
        .await
        .unwrap();
    assert!(out.text.contains("updated"));
    let disk = std::fs::read_to_string(dir.join("greeter.lua")).unwrap();
    assert!(disk.contains("v2"));

    let queued = queue.try_lock().unwrap().clone();
    assert_eq!(queued.len(), 2); // create + update
    assert!(queued[1].register);

    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test]
async fn deletes_existing_tool() {
    let dir = fresh_dir("delete");
    let (tool, queue) = make_tool(dir.clone());

    // Create.
    tool.run(&json!({
        "name": "doomed",
        "code": "return function() return 1 end"
    }))
    .await
    .unwrap();
    assert!(dir.join("doomed.lua").exists());

    // Delete: file removed + unregister queued.
    let out = tool
        .run(&json!({ "action": "delete", "name": "doomed" }))
        .await
        .unwrap();
    assert!(out.text.contains("deleted"));
    assert!(!dir.join("doomed.lua").exists());
    let queued = queue.try_lock().unwrap().clone();
    assert!(!queued[1].register);
    assert_eq!(queued[1].name, "doomed");

    // Deleting a non-existent tool is a friendly no-op.
    let out2 = tool
        .run(&json!({ "action": "delete", "name": "ghost" }))
        .await
        .unwrap();
    assert!(out2.text.contains("does not exist"));

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
        prompt_state: None,
    };
    let out = tool
        .run(&json!({
            "action": "edit_prompt",
            "content": "original prompt\n\nAlways answer in Chinese.\n\nThis prompt is padded out well beyond one hundred characters so the quality gate accepts it as a substantial full-prompt replacement.",
            "reason": "user asked for Chinese answers in every session"
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
        prompt_state: None,
    };

    // Add a skill.
    let out = tool
        .run(&json!({
            "action": "add_skill",
            "name": "my-skill",
            "content": "> A reusable procedure\n\n1. step one\n2. step two\n3. this body is padded out beyond the minimum length comfortably",
            "reason": "the workflow repeats across sessions"
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
            "content": "> x\n\nbody padded out beyond the minimum length comfortably for the gate",
            "reason": "same reason"
        }))
        .await
        .unwrap();
    assert!(out2.text.contains("badname"));

    // List includes both.
    let listed = tool.run(&json!({ "action": "list_skills" })).await.unwrap();
    assert!(listed.text.contains("my-skill"));
    assert!(listed.text.contains("badname"));
    // Description is shown.
    assert!(listed.text.contains("A reusable procedure"));

    // Add a skill with a dependency → listed with depends_on.
    let out3 = tool
        .run(&json!({
            "action": "add_skill",
            "name": "dep-skill",
            "content": "> Dep skill\n> depends_on: my-skill\n\nbody padded beyond the minimum length comfortably for the gate",
            "reason": "dependency reuse"
        }))
        .await
        .unwrap();
    assert!(out3.text.contains("dep-skill"));
    let listed = tool.run(&json!({ "action": "list_skills" })).await.unwrap();
    assert!(listed.text.contains("depends_on: my-skill"));

    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test]
async fn improve_harness_rejects_unknown_action() {
    let dir = fresh_dir("ahe-unknown");
    let harness = dir.join("harness");
    std::fs::create_dir_all(&harness).unwrap();

    let tool = ImproveHarnessTool {
        harness_dir: harness.clone(),
        prompt_state: None,
    };
    let err = tool
        .run(&json!({ "action": "nuke_everything" }))
        .await
        .unwrap_err();
    assert!(err.to_string().contains("unknown action"));

    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test]
async fn edit_prompt_updates_live_prompt_state() {
    let dir = std::env::temp_dir().join(format!("sigil-evolve-live-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join("system_prompt.md"), "old prompt").unwrap();

    let state = std::sync::Arc::new(std::sync::RwLock::new(crate::harness::PromptState {
        system_prompt: "old prompt".into(),
        skills: vec![],
        fingerprint: None,
    }));
    let tool = ImproveHarnessTool {
        harness_dir: dir.clone(),
        prompt_state: Some(state.clone()),
    };
    let out = tool
        .run(&json!({
            "action": "edit_prompt",
            "content": "brand new prompt that is comfortably longer than one hundred characters so the quality gate accepts this full-prompt replacement as substantial",
            "reason": "live-state propagation test"
        }))
        .await
        .unwrap();
    assert!(out.text.contains("live"));

    // The shared state now carries the new prompt — the agent loop
    // picks it up without a restart.
    let s = state.read().unwrap();
    assert_eq!(
        s.system_prompt,
        "brand new prompt that is comfortably longer than one hundred characters so the quality gate accepts this full-prompt replacement as substantial"
    );

    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test]
async fn refinement_gate_rejects_missing_reason() {
    let dir = fresh_dir("ahe-noreason");
    let harness = dir.join("harness");
    std::fs::create_dir_all(&harness).unwrap();
    std::fs::write(harness.join("system_prompt.md"), "original prompt").unwrap();

    let tool = ImproveHarnessTool {
        harness_dir: harness.clone(),
        prompt_state: None,
    };
    // No reason → rejected before any write.
    let err = tool
        .run(&json!({
            "action": "edit_prompt",
            "content": "A brand new full system prompt that is definitely longer than one hundred characters so length alone would not stop it"
        }))
        .await
        .unwrap_err();
    assert!(err.to_string().contains("requires 'reason'"), "got: {err}");
    // The prompt file is untouched.
    assert_eq!(
        std::fs::read_to_string(harness.join("system_prompt.md")).unwrap(),
        "original prompt"
    );

    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test]
async fn refinement_gate_rejects_noop_rewrites() {
    let dir = fresh_dir("ahe-noop");
    let harness = dir.join("harness");
    std::fs::create_dir_all(&harness).unwrap();
    let prompt = "A reasonably long original system prompt that clears the one hundred character minimum by a comfortable margin for the gate test";
    std::fs::write(harness.join("system_prompt.md"), prompt).unwrap();

    let tool = ImproveHarnessTool {
        harness_dir: harness.clone(),
        prompt_state: None,
    };
    // Identical content (just whitespace difference) → rejected.
    let err = tool
        .run(&json!({
            "action": "edit_prompt",
            "content": format!("  {prompt}  "),
            "reason": "test noop"
        }))
        .await
        .unwrap_err();
    assert!(err.to_string().contains("identical"), "got: {err}");

    // Same-skill identical rewrite → rejected.
    std::fs::create_dir_all(harness.join("skills")).unwrap();
    let skill = "> A skill description\n\nbody body body body body body body body";
    std::fs::write(harness.join("skills").join("s.md"), skill).unwrap();
    let err = tool
        .run(&json!({
            "action": "add_skill",
            "name": "s",
            "content": skill,
            "reason": "test noop"
        }))
        .await
        .unwrap_err();
    assert!(err.to_string().contains("identical content"), "got: {err}");

    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test]
async fn refinement_gate_rejects_noise_and_bad_shape() {
    let dir = fresh_dir("ahe-noise");
    let harness = dir.join("harness");
    std::fs::create_dir_all(&harness).unwrap();
    std::fs::write(harness.join("system_prompt.md"), "original prompt").unwrap();

    let tool = ImproveHarnessTool {
        harness_dir: harness.clone(),
        prompt_state: None,
    };
    // Too short to be a full prompt replacement.
    let err = tool
        .run(&json!({
            "action": "edit_prompt",
            "content": "hi",
            "reason": "test"
        }))
        .await
        .unwrap_err();
    assert!(err.to_string().contains("too short"), "got: {err}");

    // Skill without the required '> desc' header.
    let err = tool
        .run(&json!({
            "action": "add_skill",
            "name": "bad",
            "content": "no header here but otherwise long enough to pass the length gate easily yes indeed",
            "reason": "test"
        }))
        .await
        .unwrap_err();
    assert!(
        err.to_string().contains("'> short description'"),
        "got: {err}"
    );

    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test]
async fn accepted_edits_are_audited_in_refinements_jsonl() {
    let dir = fresh_dir("ahe-audit");
    let harness = dir.join("harness");
    std::fs::create_dir_all(harness.join("skills")).unwrap();
    std::fs::write(harness.join("system_prompt.md"), "original prompt").unwrap();

    let tool = ImproveHarnessTool {
        harness_dir: harness.clone(),
        prompt_state: None,
    };
    tool.run(&json!({
        "action": "edit_prompt",
        "content": "A brand new full system prompt that is comfortably over one hundred characters so the quality gate happily accepts it for the audit test",
        "reason": "audit test"
    }))
    .await
    .unwrap();
    tool.run(&json!({
        "action": "add_skill",
        "name": "audit-skill",
        "content": "> Audit skill\n\nbody padded out beyond the minimum length comfortably for the gate",
        "reason": "audit test"
    }))
    .await
    .unwrap();

    let log = std::fs::read_to_string(harness.join("refinements.jsonl")).unwrap();
    let lines: Vec<&str> = log.lines().collect();
    assert_eq!(lines.len(), 2, "two accepted edits → two log lines: {log}");
    assert!(lines[0].contains("\"action\":\"edit_prompt\""));
    assert!(lines[0].contains("\"reason\":\"audit test\""));
    assert!(lines[1].contains("\"action\":\"add_skill\""));
    assert!(lines[1].contains("\"name\":\"audit-skill\""));

    let _ = std::fs::remove_dir_all(&dir);
}
