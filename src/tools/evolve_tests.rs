//! Evolution tools tests: create_lua_tool (hot registration) and
//! improve_harness (AHE instruction-layer mutation).

use serde_json::json;

use crate::tools::Tool;
use crate::tools::evolve::{CreateLuaToolTool, ImproveHarnessTool, RevertEffectsTool};

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
        local_dir: None,
        prompt_state: None,
        hooks_dir: None,
        effects: None,
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
        local_dir: None,
        prompt_state: None,
        hooks_dir: None,
        effects: None,
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
        local_dir: None,
        prompt_state: None,
        hooks_dir: None,
        effects: None,
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
        local_dir: None,
        prompt_state: Some(state.clone()),
        hooks_dir: None,
        effects: None,
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
        local_dir: None,
        prompt_state: None,
        hooks_dir: None,
        effects: None,
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
        local_dir: None,
        prompt_state: None,
        hooks_dir: None,
        effects: None,
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
        local_dir: None,
        prompt_state: None,
        hooks_dir: None,
        effects: None,
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
        local_dir: None,
        prompt_state: None,
        hooks_dir: None,
        effects: None,
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

#[tokio::test]
async fn scope_local_writes_to_local_dir_when_configured() {
    let dir = fresh_dir("scope-local");
    let harness = dir.join("harness");
    let local = dir.join("local-harness");
    std::fs::create_dir_all(harness.join("skills")).unwrap();
    std::fs::create_dir_all(local.join("skills")).unwrap();
    std::fs::write(harness.join("system_prompt.md"), "GLOBAL PROMPT").unwrap();

    let tool = ImproveHarnessTool {
        harness_dir: harness.clone(),
        local_dir: Some(local.clone()),
        prompt_state: None,
        hooks_dir: None,
        effects: None,
    };

    // scope=local edit_prompt → lands in the local dir, not the global one.
    tool.run(&json!({
        "action": "edit_prompt",
        "scope": "local",
        "content": "A local experimental prompt that is comfortably over one hundred characters long so the quality gate happily accepts it for the local scope test",
        "reason": "testing a session-local experiment"
    }))
    .await
    .unwrap();

    let local_prompt = std::fs::read_to_string(local.join("system_prompt.md")).unwrap();
    assert!(local_prompt.contains("local experimental prompt"));
    // Global prompt untouched.
    let global_prompt = std::fs::read_to_string(harness.join("system_prompt.md")).unwrap();
    assert_eq!(global_prompt, "GLOBAL PROMPT");
    // Audit lands next to the write (local refinements.jsonl).
    assert!(local.join("refinements.jsonl").exists());

    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test]
async fn scope_local_without_local_dir_is_rejected() {
    let dir = fresh_dir("scope-reject");
    let harness = dir.join("harness");
    std::fs::create_dir_all(harness.join("skills")).unwrap();
    std::fs::write(harness.join("system_prompt.md"), "GLOBAL PROMPT").unwrap();

    let tool = ImproveHarnessTool {
        harness_dir: harness.clone(),
        local_dir: None,
        prompt_state: None,
        hooks_dir: None,
        effects: None,
    };
    let err = tool
        .run(&json!({
            "action": "edit_prompt",
            "scope": "local",
            "content": "A local experimental prompt padded well beyond one hundred characters so the gate accepts it but the missing local dir should reject it first",
            "reason": "test"
        }))
        .await
        .unwrap_err()
        .to_string();
    assert!(err.contains("--local-harness-dir"), "err: {err}");

    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test]
async fn default_scope_is_local_when_local_layer_exists() {
    let dir = fresh_dir("scope-default-local");
    let harness = dir.join("harness");
    let local = dir.join("local-harness");
    std::fs::create_dir_all(harness.join("skills")).unwrap();
    std::fs::create_dir_all(local.join("skills")).unwrap();
    std::fs::write(harness.join("system_prompt.md"), "GLOBAL PROMPT").unwrap();

    let tool = ImproveHarnessTool {
        harness_dir: harness.clone(),
        local_dir: Some(local.clone()),
        prompt_state: None,
        hooks_dir: None,
        effects: None,
    };

    // No scope arg → lands in the local layer (prime-agent semantics).
    tool.run(&json!({
        "action": "edit_prompt",
        "content": "A session-local prompt that is comfortably over one hundred characters long so the quality gate accepts it as a substantial full-prompt replacement",
        "reason": "default scope should be local"
    }))
    .await
    .unwrap();

    assert!(local.join("system_prompt.md").exists());
    let global_prompt = std::fs::read_to_string(harness.join("system_prompt.md")).unwrap();
    assert_eq!(
        global_prompt, "GLOBAL PROMPT",
        "global untouched by default-scope write"
    );

    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test]
async fn explicit_global_scope_writes_global_even_with_local_layer() {
    let dir = fresh_dir("scope-explicit-global");
    let harness = dir.join("harness");
    let local = dir.join("local-harness");
    std::fs::create_dir_all(harness.join("skills")).unwrap();
    std::fs::create_dir_all(local.join("skills")).unwrap();
    std::fs::write(harness.join("system_prompt.md"), "GLOBAL PROMPT").unwrap();

    let tool = ImproveHarnessTool {
        harness_dir: harness.clone(),
        local_dir: Some(local.clone()),
        prompt_state: None,
        hooks_dir: None,
        effects: None,
    };

    // Explicit scope=global bypasses the local default.
    tool.run(&json!({
        "action": "edit_prompt",
        "scope": "global",
        "content": "A cross-session prompt that is comfortably over one hundred characters long so the quality gate accepts it as a substantial full-prompt replacement",
        "reason": "durable cross-session improvement"
    }))
    .await
    .unwrap();

    let global_prompt = std::fs::read_to_string(harness.join("system_prompt.md")).unwrap();
    assert!(global_prompt.contains("cross-session prompt"));
    assert!(
        !local.join("system_prompt.md").exists(),
        "local untouched by explicit global write"
    );

    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test]
async fn scope_local_add_skill_and_refresh_state_merge() {
    let dir = fresh_dir("scope-merge");
    let harness = dir.join("harness");
    let local = dir.join("local-harness");
    std::fs::create_dir_all(harness.join("skills")).unwrap();
    std::fs::create_dir_all(local.join("skills")).unwrap();
    std::fs::write(harness.join("system_prompt.md"), "GLOBAL PROMPT").unwrap();
    // Same-named global skill that the local layer will shadow.
    std::fs::write(
        harness.join("skills").join("dup.md"),
        "> global dup\nglobal body",
    )
    .unwrap();

    let state = std::sync::Arc::new(std::sync::RwLock::new(crate::harness::PromptState {
        system_prompt: "GLOBAL PROMPT".into(),
        skills: crate::harness::load_skills(&harness.join("skills")),
        fingerprint: None,
    }));
    let tool = ImproveHarnessTool {
        harness_dir: harness.clone(),
        local_dir: Some(local.clone()),
        prompt_state: Some(state.clone()),
        hooks_dir: None,
        effects: None,
    };

    // Local add_skill shadows the global same-named skill.
    tool.run(&json!({
        "action": "add_skill",
        "scope": "local",
        "name": "dup",
        "content": "> local dup\nlocal body padded out beyond the minimum length comfortably for the gate",
        "reason": "session experiment"
    }))
    .await
    .unwrap();

    // PromptState was refreshed to the merged view: local skill wins.
    let s = state.read().unwrap();
    let dup = s.skills.iter().find(|sk| sk.name == "dup").unwrap();
    assert!(
        dup.body
            .contains("local body padded out beyond the minimum length comfortably for the gate")
    );
    assert!(!dup.body.contains("global body"));
    assert!(s.skills.iter().any(|sk| sk.name == "dup"));

    let _ = std::fs::remove_dir_all(&dir);
}

/// Cordis P1: a revertible effect round-trips `edit_prompt`. Forward write
/// lands, the tracker records an inverse; `revert_effects` restores the
/// previous prompt exactly.
#[tokio::test]
async fn revertible_edit_prompt_roundtrip() {
    let dir = fresh_dir("revert-prompt");
    let harness = dir.join("harness");
    std::fs::create_dir_all(&harness).unwrap();
    let original = "original prompt v1";
    std::fs::write(harness.join("system_prompt.md"), original).unwrap();

    let tracker = std::sync::Arc::new(crate::revertible::EffectTracker::new());
    let tool = ImproveHarnessTool {
        harness_dir: harness.clone(),
        local_dir: None,
        prompt_state: None,
        hooks_dir: None,
        effects: Some(tracker.clone()),
    };
    let out = tool
        .run(&json!({
            "action": "edit_prompt",
            "content": "revised prompt v2\n\nThis is a substantially longer full-prompt replacement that clears the one-hundred-character quality gate comfortably, so the write is accepted.",
            "reason": "improve clarity"
        }))
        .await
        .unwrap();
    assert!(out.text.contains("updated"));
    assert_eq!(tracker.pending(), 1, "one effect recorded per write");
    let changed = std::fs::read_to_string(harness.join("system_prompt.md")).unwrap();
    assert!(changed.contains("revised prompt v2"));

    // Runtime rollback restores the original.
    let revert = RevertEffectsTool { tracker: tracker.clone() };
    let r = revert.run(&json!({ "action": "all" })).await.unwrap();
    assert!(r.text.contains("reverted 1 effect"), "got: {}", r.text);
    let restored = std::fs::read_to_string(harness.join("system_prompt.md")).unwrap();
    assert_eq!(restored, original, "revert must restore the exact previous prompt");
    assert_eq!(tracker.pending(), 0);

    let _ = std::fs::remove_dir_all(&dir);
}

/// Cordis P1: a revertible effect round-trips `add_skill` — a brand-new
/// skill is reverted by deleting the file again.
#[tokio::test]
async fn revertible_add_skill_roundtrip() {
    let dir = fresh_dir("revert-skill");
    let harness = dir.join("harness");
    std::fs::create_dir_all(harness.join("skills")).unwrap();

    let tracker = std::sync::Arc::new(crate::revertible::EffectTracker::new());
    let tool = ImproveHarnessTool {
        harness_dir: harness.clone(),
        local_dir: None,
        prompt_state: None,
        hooks_dir: None,
        effects: Some(tracker.clone()),
    };
    let skill_path = harness.join("skills").join("my-skill.md");
    tool.run(&json!({
        "action": "add_skill",
        "name": "my-skill",
        "content": "> A reusable procedure\n\n1. step one\n2. step two\n3. this body is padded out beyond the minimum length comfortably",
        "reason": "the workflow repeats across sessions"
    }))
    .await
    .unwrap();
    assert!(skill_path.exists());
    assert_eq!(tracker.pending(), 1);

    let revert = RevertEffectsTool { tracker: tracker.clone() };
    let r = revert.run(&json!({ "action": "last" })).await.unwrap();
    assert!(r.text.contains("add_skill"), "got: {}", r.text);
    assert!(!skill_path.exists(), "revert of a new skill deletes it");
    assert_eq!(tracker.pending(), 0);

    let _ = std::fs::remove_dir_all(&dir);
}

/// Cordis P1: a revertible effect round-trips `edit_hook` — overwriting an
/// existing hook restores its previous content on rollback.
#[tokio::test]
async fn revertible_edit_hook_roundtrip() {
    let dir = fresh_dir("revert-hook");
    let hooks = dir.join("hooks");
    std::fs::create_dir_all(&hooks).unwrap();
    let hook_path = hooks.join("on_tool_result.lua");
    let before = "return function(ctx) return ctx.result end";
    std::fs::write(&hook_path, before).unwrap();

    let tracker = std::sync::Arc::new(crate::revertible::EffectTracker::new());
    let tool = ImproveHarnessTool {
        harness_dir: dir.join("harness"),
        local_dir: None,
        prompt_state: None,
        hooks_dir: Some(hooks.clone()),
        effects: Some(tracker.clone()),
    };
    let after = "return function(ctx) return ctx.result .. ' [annotated]' end";
    tool.run(&json!({
        "action": "edit_hook",
        "name": "on_tool_result.lua",
        "content": after,
        "reason": "annotate tool results"
    }))
    .await
    .unwrap();
    assert_eq!(std::fs::read_to_string(&hook_path).unwrap(), after);
    assert_eq!(tracker.pending(), 1);

    let revert = RevertEffectsTool { tracker: tracker.clone() };
    revert.run(&json!({ "action": "last" })).await.unwrap();
    assert_eq!(
        std::fs::read_to_string(&hook_path).unwrap(),
        before,
        "revert must restore the previous hook content"
    );

    let _ = std::fs::remove_dir_all(&dir);
}

/// Cordis P1: LIFO composition across multiple writes. Three edits to the
/// same prompt; `revert_effects action=last` undoes only the newest, and a
/// subsequent `action=all` undoes the rest — restoring the pre-run state.
#[tokio::test]
async fn revertible_lifo_multi_write() {
    let dir = fresh_dir("revert-lifo");
    let harness = dir.join("harness");
    std::fs::create_dir_all(&harness).unwrap();
    let original = "prompt v0";
    std::fs::write(harness.join("system_prompt.md"), original).unwrap();

    let tracker = std::sync::Arc::new(crate::revertible::EffectTracker::new());
    let tool = ImproveHarnessTool {
        harness_dir: harness.clone(),
        local_dir: None,
        prompt_state: None,
        hooks_dir: None,
        effects: Some(tracker.clone()),
    };
    let pad = "padding padding padding padding padding padding padding padding padding padding padding padding padding padding";
    let write = |v: &str| {
        json!({
            "action": "edit_prompt",
            "content": format!("{v}\n\n{pad}"),
            "reason": "step"
        })
    };
    tool.run(&write("prompt v1")).await.unwrap();
    tool.run(&write("prompt v2")).await.unwrap();
    tool.run(&write("prompt v3")).await.unwrap();
    assert_eq!(tracker.pending(), 3);
    assert!(std::fs::read_to_string(harness.join("system_prompt.md"))
        .unwrap()
        .starts_with("prompt v3"));

    let revert = RevertEffectsTool { tracker: tracker.clone() };
    // Undo the single newest (v3 -> v2).
    revert.run(&json!({ "action": "last" })).await.unwrap();
    assert!(std::fs::read_to_string(harness.join("system_prompt.md"))
        .unwrap()
        .starts_with("prompt v2"), "LIFO: only the newest is undone");
    assert_eq!(tracker.pending(), 2);
    // Undo the remaining two (v2 -> v1 -> v0).
    revert.run(&json!({ "action": "all" })).await.unwrap();
    assert_eq!(
        std::fs::read_to_string(harness.join("system_prompt.md")).unwrap(),
        original,
        "full LIFO rollback restores the pre-run prompt"
    );
    assert_eq!(tracker.pending(), 0);

    let _ = std::fs::remove_dir_all(&dir);
}

/// Cordis P1: `revert_effects` on an empty tracker reports nothing to do
/// (and does not error).
#[tokio::test]
async fn revert_effects_empty_is_noop() {
    let tracker = std::sync::Arc::new(crate::revertible::EffectTracker::new());
    let revert = RevertEffectsTool { tracker };
    for action in ["last", "all"] {
        let r = revert.run(&json!({ "action": action })).await.unwrap();
        assert!(r.text.contains("nothing to revert"), "got: {}", r.text);
    }
}
