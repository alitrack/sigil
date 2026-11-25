//! P-C tests: the model evolves its own Lua policy modules (hooks).
//!
//! `improve_harness` gains edit_hook / delete_hook / list_hooks; hooks
//! reload from disk on every use, so an edit is live from the very next
//! turn with no restart — the self-evolution loop for the policy layer.
//! Version library covers kind=hooks (backup → skill_history →
//! skill_rollback), so a bad policy edit is reversible.

use serde_json::json;

use crate::tools::Tool;
use crate::tools::evolve::ImproveHarnessTool;

/// Unique temp dir per test (parallel-test safety).
fn fresh_dir(tag: &str) -> std::path::PathBuf {
    let dir = std::env::temp_dir().join(format!(
        "sigil-evolve-hooks-{tag}-{}-{:?}",
        std::process::id(),
        std::thread::current().id()
    ));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

fn improve_tool(hooks: std::path::PathBuf, harness: std::path::PathBuf) -> ImproveHarnessTool {
    ImproveHarnessTool {
        harness_dir: harness,
        local_dir: None,
        prompt_state: None,
        hooks_dir: Some(hooks),
        effects: None,
    }
}

#[tokio::test]
async fn edit_hook_creates_updates_and_rejects_bad_code() {
    let dir = fresh_dir("crud");
    let hooks = dir.join("hooks");
    let harness = dir.join("harness");
    std::fs::create_dir_all(&hooks).unwrap();
    std::fs::create_dir_all(&harness).unwrap();
    let tool = improve_tool(hooks.clone(), harness);

    // Contract gate: no `return function` → rejected before any write.
    let err = tool
        .run(&json!({
            "action": "edit_hook",
            "name": "on_tool_result.lua",
            "content": "print('nope')",
            "reason": "test"
        }))
        .await
        .unwrap_err();
    assert!(err.to_string().contains("return function"));

    // Create.
    let out = tool
        .run(&json!({
            "action": "edit_hook",
            "name": "on_tool_result.lua",
            "content": "return function(ctx) return ctx.result .. ' [EVOLVED]' end",
            "reason": "append a marker to every tool result"
        }))
        .await
        .unwrap();
    assert!(out.text.contains("on_tool_result"));
    let path = hooks.join("on_tool_result.lua");
    assert!(path.exists());

    // Update: previous version archived under versions/hooks/.
    tool.run(&json!({
        "action": "edit_hook",
        "name": "on_tool_result.lua",
        "content": "return function(ctx) return ctx.result .. ' [EVOLVED v2]' end",
        "reason": "v2 marker"
    }))
    .await
    .unwrap();
    let archived = hooks.join("versions").join("hooks");
    assert!(std::fs::read_dir(&archived).unwrap().count() >= 1);

    // Identical content → rejected (no-op guard).
    let err = tool
        .run(&json!({
            "action": "edit_hook",
            "name": "on_tool_result.lua",
            "content": "return function(ctx) return ctx.result .. ' [EVOLVED v2]' end",
            "reason": "no-op"
        }))
        .await
        .unwrap_err();
    assert!(err.to_string().contains("identical"));

    // Missing hooks dir → helpful error instead of panic.
    let bare = ImproveHarnessTool {
        harness_dir: dir.join("h2"),
        local_dir: None,
        prompt_state: None,
        hooks_dir: None,
        effects: None,
    };
    let err = bare
        .run(&json!({ "action": "list_hooks" }))
        .await
        .unwrap_err();
    assert!(err.to_string().contains("--hooks-dir"));

    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test]
async fn hooks_are_versioned_listable_and_rollback_restores() {
    let dir = fresh_dir("rollback");
    let hooks = dir.join("hooks");
    let harness = dir.join("harness");
    let tools_dir = dir.join("tools");
    std::fs::create_dir_all(&hooks).unwrap();
    std::fs::create_dir_all(&harness).unwrap();
    std::fs::create_dir_all(&tools_dir).unwrap();

    let improve = improve_tool(hooks.clone(), harness.clone());
    let v1 = "return function(ctx) return ctx.result .. ' [V1]' end";
    let v2 = "return function(ctx) return ctx.result .. ' [V2]' end";
    improve
        .run(&json!({ "action": "edit_hook", "name": "on_tool_result.lua", "content": v1, "reason": "v1" }))
        .await
        .unwrap();
    improve
        .run(&json!({ "action": "edit_hook", "name": "on_tool_result.lua", "content": v2, "reason": "v2" }))
        .await
        .unwrap();
    assert!(
        std::fs::read_to_string(hooks.join("on_tool_result.lua"))
            .unwrap()
            .contains("[V2]")
    );

    // skill_history kind=hooks lists the archived v1.
    let history = crate::tools::versions::SkillHistoryTool {
        harness_dir: harness.clone(),
        tools_dir: tools_dir.clone(),
        hooks_dir: Some(hooks.clone()),
    };
    let out = history
        .run(&json!({ "kind": "hooks", "name": "on_tool_result" }))
        .await
        .unwrap();
    assert!(
        out.text
            .contains("archived versions of hooks/on_tool_result")
    );

    // skill_rollback kind=hooks restores v1 over v2.
    let rollback = crate::tools::versions::SkillRollbackTool {
        harness_dir: harness.clone(),
        tools_dir: tools_dir.clone(),
        hooks_dir: Some(hooks.clone()),
        register: None,
        prompt_state: None,
    };
    let stamp = crate::harness::versions::versions_of(&hooks, "hooks", "on_tool_result")[0]
        .0
        .clone();
    let out = rollback
        .run(&json!({ "kind": "hooks", "name": "on_tool_result", "version": stamp }))
        .await
        .unwrap();
    assert!(out.text.contains("restored"));
    assert!(
        std::fs::read_to_string(hooks.join("on_tool_result.lua"))
            .unwrap()
            .contains("[V1]")
    );

    // delete_hook removes it (previous version archived first).
    let out = improve
        .run(&json!({
            "action": "delete_hook",
            "name": "on_tool_result.lua",
            "reason": "no longer needed"
        }))
        .await
        .unwrap();
    assert!(out.text.contains("deleted"));
    assert!(!hooks.join("on_tool_result.lua").exists());

    // list_hooks reflects the empty dir.
    let out = improve
        .run(&json!({ "action": "list_hooks" }))
        .await
        .unwrap();
    assert!(out.text.contains("no hooks yet"));

    let _ = std::fs::remove_dir_all(&dir);
}

/// The self-evolution loop, end to end: the model (a mock provider)
/// writes an `on_tool_result.lua` hook via improve_harness mid-turn,
/// the hook goes live from the very next turn with no restart, and the
/// agent's subsequent tool result is rewritten by the hook the model
/// itself authored. This is the P-C milestone: Lua improving Lua.
#[tokio::test]
async fn self_evolution_loop_hook_goes_live_same_session() {
    use async_trait::async_trait;
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

    let dir = fresh_dir("self-evolve");
    let hooks = dir.join("hooks");
    let harness = dir.join("harness");
    std::fs::create_dir_all(&hooks).unwrap();
    std::fs::create_dir_all(&harness).unwrap();
    std::fs::write(harness.join("system_prompt.md"), "base prompt").unwrap();

    let prompt_state = std::sync::Arc::new(std::sync::RwLock::new(crate::harness::PromptState {
        system_prompt: "base prompt".into(),
        skills: vec![],
        fingerprint: None,
    }));

    let mut tools = crate::tools::ToolRegistry::new();
    tools.register(std::sync::Arc::new(HookEchoTool));
    tools.register(std::sync::Arc::new(ImproveHarnessTool {
        harness_dir: harness.clone(),
        local_dir: None,
        prompt_state: Some(prompt_state.clone()),
        hooks_dir: Some(hooks.clone()),
        effects: None,
    }));

    let lua = std::sync::Arc::new(std::sync::Mutex::new(
        crate::lua::LuaRuntime::new().unwrap(),
    ));
    let config = crate::agent::AgentConfig {
        model: "test".into(),
        reasoning_effort: None,
        max_steps: 8,
        prompt_state: prompt_state.clone(),
        max_inline_skills: 0,
        sync_dir: None,
        max_context_messages: 0,
        max_context_tokens: 0,
        goals: None,
        goals_dir: None,
        planning_interval: 0,
        hooks: Some(std::sync::Arc::new(crate::hooks::HookRunner::new(
            lua,
            hooks.clone(),
        ))),
        session_sink: None,
    };

    /// Echo tool the model's hook will rewrite the output of.
    struct HookEchoTool;
    #[async_trait]
    impl crate::tools::Tool for HookEchoTool {
        fn name(&self) -> &str {
            "echo_tool"
        }
        fn description(&self) -> &str {
            "echo for self-evolution test"
        }
        fn parameters(&self) -> serde_json::Value {
            serde_json::json!({})
        }
        async fn run(&self, _args: &serde_json::Value) -> anyhow::Result<crate::tools::ToolOutput> {
            Ok(crate::tools::ToolOutput {
                text: "ECHO-OUT".into(),
            })
        }
    }

    /// Step 0: improve_harness edit_hook (author the hook). Step 1:
    /// echo_tool. Step 2: verify the tool result carries the hook's
    /// rewrite, then answer.
    #[derive(Debug)]
    struct EvolveProvider {
        streams: AtomicUsize,
        completes: AtomicUsize,
        saw_rewrite: AtomicBool,
    }
    #[async_trait]
    impl crate::llm::provider::LlmProvider for EvolveProvider {
        fn name(&self) -> &str {
            "evolve-test"
        }
        async fn chat_stream(
            &self,
            req: crate::llm::provider::ChatRequest,
        ) -> anyhow::Result<crate::llm::provider::ChatStream> {
            let n = self.streams.fetch_add(1, Ordering::SeqCst);
            match n {
                0 | 1 => Ok(Box::new(futures_util::stream::iter(vec![Ok(
                    crate::llm::provider::ChatChunk {
                        delta: String::new(),
                        thinking: false,
                        done: true,
                        tool_calls_requested: true,
                        streamed_tool_calls: Vec::new(),
                        usage: None,
                    },
                )]))),
                _ => {
                    let rewritten = req
                        .messages
                        .iter()
                        .filter(|m| matches!(m.role, crate::llm::provider::Role::Tool))
                        .any(|m| m.content.contains("[SELF-EVOLVED]"));
                    self.saw_rewrite.store(rewritten, Ordering::SeqCst);
                    Ok(Box::new(futures_util::stream::iter(vec![Ok(
                        crate::llm::provider::ChatChunk {
                            delta: "EVOLVED-DONE".into(),
                            thinking: false,
                            done: true,
                            tool_calls_requested: false,
                            streamed_tool_calls: Vec::new(),
                            usage: None,
                        },
                    )])))
                }
            }
        }
        async fn chat_complete(
            &self,
            _req: crate::llm::provider::ChatRequest,
        ) -> anyhow::Result<crate::llm::provider::ChatComplete> {
            let n = self.completes.fetch_add(1, Ordering::SeqCst);
            let call =
                |id: &str, name: &str, args: serde_json::Value| crate::llm::provider::ToolCall {
                    id: id.into(),
                    name: name.into(),
                    arguments: args,
                };
            match n {
                0 => Ok(crate::llm::provider::ChatComplete {
                    text: String::new(),
                    tool_calls: vec![call(
                        "c1",
                        "improve_harness",
                        serde_json::json!({
                            "action": "edit_hook",
                            "name": "on_tool_result.lua",
                            "content": "return function(ctx) return ctx.result .. ' [SELF-EVOLVED]' end",
                            "reason": "append a marker to tool results"
                        }),
                    )],
                    usage: None,
                }),
                1 => Ok(crate::llm::provider::ChatComplete {
                    text: String::new(),
                    tool_calls: vec![call("c2", "echo_tool", serde_json::json!({}))],
                    usage: None,
                }),
                _ => Ok(crate::llm::provider::ChatComplete {
                    text: String::new(),
                    tool_calls: vec![],
                    usage: None,
                }),
            }
        }
    }

    let provider = EvolveProvider {
        streams: AtomicUsize::new(0),
        completes: AtomicUsize::new(0),
        saw_rewrite: AtomicBool::new(false),
    };
    let answer = crate::agent::run_messages(
        &provider,
        &mut tools,
        &config,
        &[crate::llm::ChatMessage::user("evolve")],
        None,
        None,
    )
    .await
    .expect("self-evolution turn succeeds");
    assert_eq!(answer.output, "EVOLVED-DONE");
    assert!(
        provider.saw_rewrite.load(Ordering::SeqCst),
        "the hook the model authored mid-turn must rewrite the next tool result (Lua improving Lua)"
    );
    assert!(hooks.join("on_tool_result.lua").exists());

    let _ = std::fs::remove_dir_all(&dir);
}
