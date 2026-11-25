use super::*;
use crate::llm::provider::{ChatComplete, ChatRequest, ChatStream, LlmProvider, ToolCall};
use async_trait::async_trait;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

/// Records its start time, then sleeps — the slow half of the
/// concurrency check.
#[derive(Clone)]
struct SlowTool(Arc<Mutex<Vec<Instant>>>);
#[async_trait]
impl crate::tools::Tool for SlowTool {
    fn name(&self) -> &str {
        "slow_tool"
    }
    fn description(&self) -> &str {
        "slow tool for concurrency test"
    }
    fn parameters(&self) -> serde_json::Value {
        serde_json::json!({})
    }
    async fn run(&self, _args: &serde_json::Value) -> anyhow::Result<crate::tools::ToolOutput> {
        self.0.lock().unwrap().push(Instant::now());
        tokio::time::sleep(Duration::from_millis(200)).await;
        Ok(crate::tools::ToolOutput {
            text: "SLOW".into(),
        })
    }
}

/// Records its start time and returns immediately — the fast half.
#[derive(Clone)]
struct FastTool(Arc<Mutex<Vec<Instant>>>);
#[async_trait]
impl crate::tools::Tool for FastTool {
    fn name(&self) -> &str {
        "fast_tool"
    }
    fn description(&self) -> &str {
        "fast tool for concurrency test"
    }
    fn parameters(&self) -> serde_json::Value {
        serde_json::json!({})
    }
    async fn run(&self, _args: &serde_json::Value) -> anyhow::Result<crate::tools::ToolOutput> {
        self.0.lock().unwrap().push(Instant::now());
        Ok(crate::tools::ToolOutput {
            text: "FAST".into(),
        })
    }
}

/// First request emits two tool calls (slow_tool + fast_tool);
/// second request verifies the tool results arrived in original
/// call order and returns the final answer.
#[derive(Debug)]
struct TwoCallProvider {
    calls: AtomicUsize,
}
#[async_trait]
impl LlmProvider for TwoCallProvider {
    fn name(&self) -> &str {
        "two-call-test"
    }
    async fn chat_stream(&self, req: ChatRequest) -> anyhow::Result<ChatStream> {
        let n = self.calls.fetch_add(1, Ordering::SeqCst);
        if n == 0 {
            Ok(Box::new(futures_util::stream::iter(vec![
                Ok(crate::llm::provider::ChatChunk {
                    delta: String::new(),
                    thinking: false,
                    done: false,
                    tool_calls_requested: true,
                    streamed_tool_calls: Vec::new(),
                    usage: None,
                }),
                Ok(crate::llm::provider::ChatChunk {
                    delta: String::new(),
                    thinking: false,
                    done: true,
                    tool_calls_requested: true,
                    streamed_tool_calls: Vec::new(),
                    usage: None,
                }),
            ])))
        } else {
            // Second loop iteration: verify the tool results came
            // back in original call order, then answer.
            let results: Vec<&str> = req
                .messages
                .iter()
                .filter(|m| m.role == crate::llm::provider::Role::Tool)
                .map(|m| m.content.as_str())
                .collect();
            assert_eq!(
                results,
                vec!["SLOW", "FAST"],
                "tool results must keep call order"
            );
            Ok(Box::new(futures_util::stream::iter(vec![Ok(
                crate::llm::provider::ChatChunk {
                    delta: "DONE".into(),
                    thinking: false,
                    done: true,
                    tool_calls_requested: false,
                    streamed_tool_calls: Vec::new(),
                    usage: None,
                },
            )])))
        }
    }
    async fn chat_complete(&self, _req: ChatRequest) -> anyhow::Result<ChatComplete> {
        // Only ever called once: the first-iteration tool-call
        // replay (the second iteration's stream carries the final
        // answer, so no replay happens).
        Ok(ChatComplete {
            text: String::new(),
            tool_calls: vec![
                ToolCall {
                    id: "call-1".into(),
                    name: "slow_tool".into(),
                    arguments: serde_json::json!({}),
                },
                ToolCall {
                    id: "call-2".into(),
                    name: "fast_tool".into(),
                    arguments: serde_json::json!({}),
                },
            ],
            usage: None,
        })
    }
}

#[tokio::test]
async fn missing_usage_trailer_cannot_hold_open_a_completed_answer() {
    let mut stream: crate::llm::provider::ChatStream = Box::new(futures_util::stream::pending());
    let result = tokio::time::timeout(
        std::time::Duration::from_secs(1),
        trailing_usage(&mut stream, None),
    )
    .await
    .expect("completed answers must not wait indefinitely for usage");
    assert!(result.is_none());
}

#[tokio::test]
async fn usage_trailer_timeout_preserves_last_observed_snapshot() {
    let usage = TokenUsage {
        prompt_tokens: 100,
        completion_tokens: 5,
        prompt_cache_hit_tokens: 80,
        cache_reported_prompt_tokens: Some(100),
    };
    let mut stream: crate::llm::provider::ChatStream = Box::new(
        futures_util::stream::iter(vec![Ok(crate::llm::provider::ChatChunk {
            delta: String::new(),
            thinking: false,
            done: false,
            tool_calls_requested: false,
            streamed_tool_calls: Vec::new(),
            usage: Some(usage),
        })])
        .chain(futures_util::stream::pending()),
    );
    let result = trailing_usage(&mut stream, None).await;
    assert_eq!(result.unwrap().completion_tokens, 5);
}

#[tokio::test]
async fn usage_after_finish_reason_is_collected_once() {
    #[derive(Debug)]
    struct TrailingUsageProvider;
    #[async_trait]
    impl LlmProvider for TrailingUsageProvider {
        fn name(&self) -> &str {
            "trailing-usage"
        }
        async fn chat_stream(&self, _: ChatRequest) -> Result<crate::llm::provider::ChatStream> {
            let usage = crate::llm::openai::parse_usage(&serde_json::json!({"usage": {
                "prompt_tokens":100, "completion_tokens":5, "prompt_cache_hit_tokens":80
            }}))
            .unwrap();
            Ok(Box::new(futures_util::stream::iter(vec![
                Ok(crate::llm::provider::ChatChunk {
                    usage: Some(TokenUsage {
                        completion_tokens: 1,
                        ..usage
                    }),
                    ..Default::default()
                }),
                Ok(crate::llm::provider::ChatChunk {
                    delta: "done".into(),
                    done: true,
                    usage: Some(TokenUsage {
                        completion_tokens: 2,
                        ..usage
                    }),
                    ..Default::default()
                }),
                Ok(crate::llm::provider::ChatChunk {
                    usage: Some(TokenUsage {
                        completion_tokens: 3,
                        ..usage
                    }),
                    ..Default::default()
                }),
                Ok(crate::llm::provider::ChatChunk {
                    usage: Some(usage),
                    ..Default::default()
                }),
                Ok(crate::llm::provider::ChatChunk {
                    done: true,
                    ..Default::default()
                }),
            ])))
        }
        async fn chat_complete(&self, _: ChatRequest) -> Result<ChatComplete> {
            panic!("no replay needed")
        }
    }
    let events = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
    let captured = events.clone();
    let mut config = test_config();
    config.session_sink = Some(std::sync::Arc::new(move |event| {
        captured.lock().unwrap().push(event)
    }));
    let output = run_messages(
        &TrailingUsageProvider,
        &mut ToolRegistry::new(),
        &config,
        &[ChatMessage::system("base"), ChatMessage::user("task")],
        None,
        None,
    )
    .await
    .unwrap();
    let usage = output
        .tokens
        .expect("usage-only trailer must survive finish_reason");
    assert_eq!(usage.prompt_tokens, 100);
    assert_eq!(usage.completion_tokens, 5);
    assert_eq!(usage.prompt_cache_hit_tokens, 80);
    let captured = events.lock().unwrap();
    let observed = captured
        .iter()
        .filter(|event| matches!(event, crate::session::Event::RequestUsage { .. }))
        .collect::<Vec<_>>();
    assert_eq!(
        observed.len(),
        1,
        "one actual request gets one metadata event"
    );
    let json = serde_json::to_value(observed[0]).unwrap();
    assert_eq!(json["usage"]["prompt_tokens"], 100);
}

#[tokio::test]
async fn cache_diagnostics_never_generate_an_extra_completion() {
    #[derive(Debug)]
    struct NoUsageProvider;
    #[async_trait]
    impl LlmProvider for NoUsageProvider {
        fn name(&self) -> &str {
            "no-usage"
        }
        async fn chat_stream(&self, _req: ChatRequest) -> Result<crate::llm::provider::ChatStream> {
            Ok(Box::new(futures_util::stream::iter(vec![Ok(
                crate::llm::provider::ChatChunk {
                    delta: "done".into(),
                    thinking: false,
                    done: true,
                    tool_calls_requested: false,
                    streamed_tool_calls: vec![],
                    usage: None,
                },
            )])))
        }
        async fn chat_complete(&self, _req: ChatRequest) -> Result<ChatComplete> {
            panic!("diagnostics must not generate a second completion");
        }
    }
    let output = run_messages(
        &NoUsageProvider,
        &mut ToolRegistry::new(),
        &test_config(),
        &[ChatMessage::system("base"), ChatMessage::user("task")],
        None,
        None,
    )
    .await
    .unwrap();
    assert!(output.tokens.is_none());
}

#[test]
fn compaction_preserves_multiline_user_constraints_and_previous_summary() {
    let msgs = |n| {
        (0..n)
            .flat_map(|_| [ChatMessage::user("next"), ChatMessage::assistant("done")])
            .collect::<Vec<_>>()
    };
    let constraint = "Task: fix parser\nConstraint: do not change public API\nAcceptance: containment tests must pass";
    let mut history = vec![
        ChatMessage::system("base"),
        ChatMessage::user(constraint),
        ChatMessage::assistant("Decision: retain native replay\nValidated: unit tests passed"),
    ];
    history.extend(msgs(8).into_iter().skip(1));
    let first = compact_messages(&history, 6);
    assert!(
        first[1]
            .content
            .contains("Constraint: do not change public API")
    );
    assert!(first[1].content.contains("Validated: unit tests passed"));
    let mut next = first;
    next.extend(msgs(8).into_iter().skip(1));
    let second = compact_messages(&next, 6);
    assert!(
        second[1]
            .content
            .contains("Constraint: do not change public API")
    );
}

#[tokio::test]
async fn appended_tool_output_is_budgeted_after_reported_usage() {
    #[derive(Debug)]
    struct GrowingPrompt(std::sync::atomic::AtomicUsize);
    #[async_trait]
    impl LlmProvider for GrowingPrompt {
        fn name(&self) -> &str {
            "growing-prompt"
        }
        async fn chat_stream(&self, req: ChatRequest) -> Result<crate::llm::provider::ChatStream> {
            let first = self.0.fetch_add(1, std::sync::atomic::Ordering::SeqCst) == 0;
            if !first {
                assert!(
                    req.messages
                        .iter()
                        .any(|m| m.content.starts_with("[earlier conversation compressed]")),
                    "new tool output must not hide behind the previous request's small usage"
                );
                assert!(
                    req.messages
                        .iter()
                        .any(|m| m.tool_call_id.as_deref() == Some("big-call")),
                    "the current tool result must remain verbatim"
                );
            }
            let usage = crate::llm::openai::parse_usage(&serde_json::json!({"usage": {
                "prompt_tokens":400, "completion_tokens":5
            }}));
            Ok(Box::new(futures_util::stream::iter(vec![Ok(
                crate::llm::provider::ChatChunk {
                    delta: if first { String::new() } else { "done".into() },
                    done: true,
                    tool_calls_requested: first,
                    usage,
                    streamed_tool_calls: if first {
                        vec![ToolCall {
                            id: "big-call".into(),
                            name: "big-output".into(),
                            arguments: serde_json::json!({}),
                        }]
                    } else {
                        vec![]
                    },
                    ..Default::default()
                },
            )])))
        }
        async fn chat_complete(&self, _: ChatRequest) -> Result<ChatComplete> {
            panic!("native calls need no replay")
        }
    }
    struct BigOutput;
    #[async_trait]
    impl crate::tools::Tool for BigOutput {
        fn name(&self) -> &str {
            "big-output"
        }
        fn description(&self) -> &str {
            "large output"
        }
        fn parameters(&self) -> serde_json::Value {
            serde_json::json!({})
        }
        async fn run(&self, _: &serde_json::Value) -> Result<crate::tools::ToolOutput> {
            Ok(crate::tools::ToolOutput {
                text: "x".repeat(10_000),
            })
        }
    }
    let mut tools = ToolRegistry::new();
    tools.register(std::sync::Arc::new(BigOutput));
    let mut history = vec![ChatMessage::system("base")];
    for _ in 0..20 {
        history.extend([
            ChatMessage::user("previous task"),
            ChatMessage::assistant("previous result"),
        ]);
    }
    history.push(ChatMessage::user("current task"));
    let mut config = test_config();
    config.max_context_tokens = 1000;
    run_messages(
        &GrowingPrompt(std::sync::atomic::AtomicUsize::new(0)),
        &mut tools,
        &config,
        &history,
        None,
        None,
    )
    .await
    .unwrap();
}

/// Minimal AgentConfig for tests (no goals, no compression).
pub(crate) fn test_config() -> AgentConfig {
    AgentConfig {
        model: "m".into(),
        reasoning_effort: None,
        max_steps: 0,
        prompt_state: std::sync::Arc::new(std::sync::RwLock::new(crate::harness::PromptState {
            system_prompt: "base".into(),
            skills: vec![],
            fingerprint: None,
        })),
        max_inline_skills: 0,
        sync_dir: None,
        max_context_messages: 0,
        max_context_tokens: 0,
        goals: None,
        goals_dir: None,
        planning_interval: 0,
        hooks: None,
        session_sink: None,
    }
}

#[tokio::test]
async fn reactive_compaction_retries_after_context_error() {
    // The provider fails the first stream with a context-window
    // error; the agent must compact the transcript and retry, and
    // the retried request must carry fewer messages.
    let provider = ContextErrorProvider {
        calls: AtomicUsize::new(0),
        seen_compacted: AtomicUsize::new(0),
    };
    let mut tools = crate::tools::ToolRegistry::new();
    let config = AgentConfig {
        model: "m".into(),
        reasoning_effort: None,
        max_steps: 0,
        prompt_state: std::sync::Arc::new(std::sync::RwLock::new(crate::harness::PromptState {
            system_prompt: "base".into(),
            skills: vec![],
            fingerprint: None,
        })),
        max_inline_skills: 0,
        sync_dir: None,
        max_context_messages: 6,
        max_context_tokens: 0,
        goals: None,
        goals_dir: None,
        planning_interval: 0,
        hooks: None,
        session_sink: None,
    };
    // 10 user turns -> well over the 6-message budget.
    let mut msgs = vec![ChatMessage::system("SYS")];
    for i in 0..10 {
        msgs.push(ChatMessage::user(format!("user-{i}")));
        msgs.push(ChatMessage::assistant(format!("assistant-{i}")));
    }
    let answer = run_messages(&provider, &mut tools, &config, &msgs, None, None)
        .await
        .expect("compaction retry succeeds");
    assert_eq!(answer.output, "AFTER-RETRY");
    assert_eq!(answer.state, RunState::Success);
    // First stream failed, second succeeded.
    assert_eq!(provider.calls.load(Ordering::SeqCst), 2);
    assert_eq!(provider.seen_compacted.load(Ordering::SeqCst), 1);
}

/// First chat_stream errors with a context-window failure; the
/// retried stream must receive a compacted transcript and answer.
#[derive(Debug)]
struct ContextErrorProvider {
    calls: AtomicUsize,
    seen_compacted: AtomicUsize,
}
#[async_trait]
impl LlmProvider for ContextErrorProvider {
    fn name(&self) -> &str {
        "context-error-test"
    }
    async fn chat_stream(&self, req: ChatRequest) -> anyhow::Result<ChatStream> {
        let n = self.calls.fetch_add(1, Ordering::SeqCst);
        if n == 0 {
            Err(anyhow::anyhow!(
                "This model's maximum context length is 8192 tokens. \
                 However, your messages resulted in 12000 tokens."
            ))
        } else {
            assert!(
                req.messages.len() < 8,
                "retried request must be compacted, got {} messages",
                req.messages.len()
            );
            self.seen_compacted.fetch_add(1, Ordering::SeqCst);
            Ok(Box::new(futures_util::stream::iter(vec![Ok(
                crate::llm::provider::ChatChunk {
                    delta: "AFTER-RETRY".into(),
                    thinking: false,
                    done: true,
                    tool_calls_requested: false,
                    streamed_tool_calls: Vec::new(),
                    usage: None,
                },
            )])))
        }
    }
    async fn chat_complete(&self, _req: ChatRequest) -> anyhow::Result<ChatComplete> {
        unreachable!("no tool calls in this test")
    }
}

#[test]
fn recoverable_error_classification() {
    let ctx = anyhow::anyhow!("maximum context length is 8192 tokens");
    assert!(is_recoverable_llm_error(&ctx));
    let too_long = anyhow::anyhow!("This model's maximum context length");
    assert!(is_recoverable_llm_error(&too_long));
    let malformed = anyhow::anyhow!("previous response contained invalid tool_use arguments");
    assert!(is_recoverable_llm_error(&malformed));
    let unrecoverable = anyhow::anyhow!("rate limit exceeded, retry in 60s");
    assert!(!is_recoverable_llm_error(&unrecoverable));
    let network = anyhow::anyhow!("failed to connect to 127.0.0.1:15722");
    assert!(!is_recoverable_llm_error(&network));
}

#[tokio::test]
async fn planning_interval_injects_checkpoints() {
    // interval=2: steps 0 and 2 get the checkpoint, step 1 does not.
    // The mock answers with a tool call on step 0, a plain answer
    // on step 1, and verifies checkpoint presence per request.
    let provider = PlanningProvider {
        calls: AtomicUsize::new(0),
        checkpoint_at: Arc::new(Mutex::new(Vec::new())),
    };
    let mut tools = crate::tools::ToolRegistry::new();
    tools.register(Arc::new(FastTool(Arc::new(Mutex::new(Vec::new())))));
    let config = AgentConfig {
        model: "m".into(),
        reasoning_effort: None,
        max_steps: 0,
        prompt_state: std::sync::Arc::new(std::sync::RwLock::new(crate::harness::PromptState {
            system_prompt: "base".into(),
            skills: vec![],
            fingerprint: None,
        })),
        max_inline_skills: 0,
        sync_dir: None,
        max_context_messages: 0,
        max_context_tokens: 0,
        goals: None,
        goals_dir: None,
        planning_interval: 2,
        hooks: None,
        session_sink: None,
    };
    let answer = run_messages(
        &provider,
        &mut tools,
        &config,
        &[ChatMessage::user("do the multi-step task")],
        None,
        None,
    )
    .await
    .expect("turn succeeds");
    assert_eq!(answer.output, "PLANNED-DONE");
    assert_eq!(answer.state, RunState::Success);
    let checkpoints = provider.checkpoint_at.lock().unwrap().clone();
    // Only step 0 (first iteration) carries the checkpoint: step 1's
    // request had it stripped, and there is no step 2 in this run.
    assert_eq!(checkpoints, vec![0], "checkpoint only on step 0");
}

/// Step 0: emits a tool call (fast_tool). Step 1: plain answer.
/// Step 2: plain answer; records which requests carried the
/// PLANNING CHECKPOINT prompt.
#[derive(Debug)]
struct PlanningProvider {
    calls: AtomicUsize,
    checkpoint_at: Arc<Mutex<Vec<usize>>>,
}
#[async_trait]
impl LlmProvider for PlanningProvider {
    fn name(&self) -> &str {
        "planning-test"
    }
    async fn chat_stream(&self, req: ChatRequest) -> anyhow::Result<ChatStream> {
        let n = self.calls.fetch_add(1, Ordering::SeqCst);
        let has_checkpoint = req
            .messages
            .iter()
            .any(|m| m.content.contains("PLANNING CHECKPOINT"));
        if has_checkpoint {
            self.checkpoint_at.lock().unwrap().push(n);
        }
        match n {
            0 => Ok(Box::new(futures_util::stream::iter(vec![Ok(
                crate::llm::provider::ChatChunk {
                    delta: String::new(),
                    thinking: false,
                    done: true,
                    tool_calls_requested: true,
                    streamed_tool_calls: Vec::new(),
                    usage: None,
                },
            )]))),
            _ => Ok(Box::new(futures_util::stream::iter(vec![Ok(
                crate::llm::provider::ChatChunk {
                    delta: "PLANNED-DONE".into(),
                    thinking: false,
                    done: true,
                    tool_calls_requested: false,
                    streamed_tool_calls: Vec::new(),
                    usage: None,
                },
            )]))),
        }
    }
    async fn chat_complete(&self, _req: ChatRequest) -> anyhow::Result<ChatComplete> {
        Ok(ChatComplete {
            text: String::new(),
            tool_calls: vec![ToolCall {
                id: "call-p".into(),
                name: "fast_tool".into(),
                arguments: serde_json::json!({}),
            }],
            usage: None,
        })
    }
}

#[tokio::test]
async fn parallel_tool_calls_execute_concurrently_and_keep_order() {
    let starts: Arc<Mutex<Vec<Instant>>> = Arc::new(Mutex::new(Vec::new()));
    let mut tools = crate::tools::ToolRegistry::new();
    tools.register(Arc::new(SlowTool(starts.clone())));
    tools.register(Arc::new(FastTool(starts.clone())));

    let provider = TwoCallProvider {
        calls: AtomicUsize::new(0),
    };
    let config = test_config();

    let answer = run_messages(
        &provider,
        &mut tools,
        &config,
        &[ChatMessage::user("run both tools")],
        None,
        None,
    )
    .await
    .expect("turn succeeds");
    assert_eq!(answer.output, "DONE");
    assert_eq!(answer.state, RunState::Success);

    // Both tools ran: two recorded start instants.
    let recorded = starts.lock().unwrap();
    assert_eq!(recorded.len(), 2, "both tools must run");
    // Concurrency proof: the fast tool started while the slow one
    // was still sleeping (its start is < 200ms before the slow
    // tool finished — far below a serial 200ms gap).
    let gap = recorded[1].duration_since(recorded[0]).as_millis();
    assert!(
        gap < 150,
        "fast tool started {gap}ms after slow tool — expected <150ms (concurrent), got serial execution"
    );
}

#[tokio::test]
async fn session_sink_emits_tool_call_and_result_events() {
    // Phase 63 event-sourced logging: with a session_sink wired,
    // the loop must emit one ToolCall event per requested call and
    // one ToolResult per outcome, in order.
    let mut tools = crate::tools::ToolRegistry::new();
    tools.register(Arc::new(SlowTool(Arc::new(Mutex::new(Vec::new())))));
    tools.register(Arc::new(FastTool(Arc::new(Mutex::new(Vec::new())))));

    let provider = TwoCallProvider {
        calls: AtomicUsize::new(0),
    };
    let events: Arc<Mutex<Vec<crate::session::Event>>> = Arc::new(Mutex::new(Vec::new()));
    let mut config = test_config();
    let sink = events.clone();
    config.session_sink = Some(std::sync::Arc::new(move |ev| sink.lock().unwrap().push(ev)));

    let answer = run_messages(
        &provider,
        &mut tools,
        &config,
        &[ChatMessage::user("run both tools")],
        None,
        None,
    )
    .await
    .expect("turn succeeds");
    assert_eq!(answer.output, "DONE");

    {
        let evs = events.lock().unwrap();
        // Phase A2 补充了结构/元数据事件（TurnStart/StepStart/RequestHeader/Assistant），
        // 此处验证核心契约：2 个 ToolCall + 2 个 ToolResult，且顺序为
        // "全部调用先于任何结果"（calls 在 plan 时发出，results 在执行后）。
        let calls: Vec<_> = evs.iter().filter(|e| e.kind() == "tool_call").collect();
        let results: Vec<_> = evs.iter().filter(|e| e.kind() == "tool_result").collect();
        assert_eq!(calls.len(), 2, "expected 2 tool_call events, got {evs:?}");
        assert_eq!(
            results.len(),
            2,
            "expected 2 tool_result events, got {evs:?}"
        );
        let first_result_idx = evs.iter().position(|e| e.kind() == "tool_result").unwrap();
        let last_call_idx = evs.iter().rposition(|e| e.kind() == "tool_call").unwrap();
        assert!(
            last_call_idx < first_result_idx,
            "all calls must precede any result, got {evs:?}"
        );
        // 结构化事件存在（Phase A2 注入即事件）。
        assert!(evs.iter().any(|e| e.kind() == "turn_start"));
        assert!(evs.iter().any(|e| e.kind() == "request_header"));
        // Names match the requested tools; both succeeded.
        match &calls[0] {
            crate::session::Event::ToolCall { name, .. } => assert_eq!(name, "slow_tool"),
            other => panic!("expected tool_call, got {other:?}"),
        }
        match &results[0] {
            crate::session::Event::ToolResult { ok, output, .. } => {
                assert!(*ok, "slow_tool should succeed");
                assert!(!output.is_empty());
            }
            other => panic!("expected tool_result, got {other:?}"),
        }
    } // lock released here, before the second turn awaits
    // No session_sink → turn still succeeds (regression guard).
    let config2 = test_config();
    let provider2 = TwoCallProvider {
        calls: AtomicUsize::new(0),
    };
    let mut tools2 = crate::tools::ToolRegistry::new();
    tools2.register(Arc::new(SlowTool(Arc::new(Mutex::new(Vec::new())))));
    tools2.register(Arc::new(FastTool(Arc::new(Mutex::new(Vec::new())))));
    let _ = run_messages(
        &provider2,
        &mut tools2,
        &config2,
        &[ChatMessage::user("run both tools")],
        None,
        None,
    )
    .await
    .expect("turn succeeds");
}

/// Returns "OUT" immediately — the tool the hook tests drive.
struct EchoTool;
#[async_trait]
impl crate::tools::Tool for EchoTool {
    fn name(&self) -> &str {
        "echo_tool"
    }
    fn description(&self) -> &str {
        "echo for hook test"
    }
    fn parameters(&self) -> serde_json::Value {
        serde_json::json!({})
    }
    async fn run(&self, _args: &serde_json::Value) -> anyhow::Result<crate::tools::ToolOutput> {
        Ok(crate::tools::ToolOutput { text: "OUT".into() })
    }
}

/// Step 0: one tool call. Step 1: verifies the tool_result message
/// was rewritten by the on_tool_result hook, then answers.
#[derive(Debug)]
struct HookResultProvider {
    calls: AtomicUsize,
    saw_rewrite: AtomicBool,
}
#[async_trait]
impl LlmProvider for HookResultProvider {
    fn name(&self) -> &str {
        "hook-result-test"
    }
    async fn chat_stream(&self, req: ChatRequest) -> anyhow::Result<ChatStream> {
        let n = self.calls.fetch_add(1, Ordering::SeqCst);
        match n {
            0 => Ok(Box::new(futures_util::stream::iter(vec![Ok(
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
                    .any(|m| m.content.contains("[hook]"));
                self.saw_rewrite.store(rewritten, Ordering::SeqCst);
                Ok(Box::new(futures_util::stream::iter(vec![Ok(
                    crate::llm::provider::ChatChunk {
                        delta: "HOOKED-DONE".into(),
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
    async fn chat_complete(&self, _req: ChatRequest) -> anyhow::Result<ChatComplete> {
        Ok(ChatComplete {
            text: String::new(),
            tool_calls: vec![ToolCall {
                id: "c1".into(),
                name: "echo_tool".into(),
                arguments: serde_json::json!({}),
            }],
            usage: None,
        })
    }
}

#[tokio::test]
async fn on_tool_result_hook_rewrites_tool_output() {
    let hooks_dir = std::env::temp_dir()
        .join("sigil-agent-hooks-tests")
        .join("result");
    std::fs::create_dir_all(&hooks_dir).unwrap();
    std::fs::write(
        hooks_dir.join("on_tool_result.lua"),
        "return function(ctx) return ctx.result .. ' [hook]' end",
    )
    .unwrap();
    let lua = std::sync::Arc::new(std::sync::Mutex::new(
        crate::lua::LuaRuntime::new().unwrap(),
    ));
    let mut config = test_config();
    config.hooks = Some(std::sync::Arc::new(crate::hooks::HookRunner::new(
        lua, hooks_dir,
    )));

    let mut tools = crate::tools::ToolRegistry::new();
    tools.register(Arc::new(EchoTool));
    let provider = HookResultProvider {
        calls: AtomicUsize::new(0),
        saw_rewrite: AtomicBool::new(false),
    };

    let answer = run_messages(
        &provider,
        &mut tools,
        &config,
        &[ChatMessage::user("run echo")],
        None,
        None,
    )
    .await
    .expect("turn succeeds");
    assert_eq!(answer.output, "HOOKED-DONE");
    assert!(
        provider.saw_rewrite.load(Ordering::SeqCst),
        "tool_result must be rewritten by the on_tool_result hook"
    );
}

/// Step 0: verifies the first request carries the on_turn_start
/// hook's injected system message, then emits a tool call; step 1
/// answers.
#[derive(Debug)]
struct TurnStartProvider {
    calls: AtomicUsize,
    saw_inject: AtomicBool,
}
#[async_trait]
impl LlmProvider for TurnStartProvider {
    fn name(&self) -> &str {
        "hook-turn-start-test"
    }
    async fn chat_stream(&self, req: ChatRequest) -> anyhow::Result<ChatStream> {
        let n = self.calls.fetch_add(1, Ordering::SeqCst);
        match n {
            0 => {
                let injected = req
                    .messages
                    .iter()
                    .filter(|m| matches!(m.role, crate::llm::provider::Role::System))
                    .any(|m| m.content.contains("TURN-START-HOOK"));
                self.saw_inject.store(injected, Ordering::SeqCst);
                Ok(Box::new(futures_util::stream::iter(vec![Ok(
                    crate::llm::provider::ChatChunk {
                        delta: String::new(),
                        thinking: false,
                        done: true,
                        tool_calls_requested: true,
                        streamed_tool_calls: Vec::new(),
                        usage: None,
                    },
                )])))
            }
            _ => Ok(Box::new(futures_util::stream::iter(vec![Ok(
                crate::llm::provider::ChatChunk {
                    delta: "INJECTED-DONE".into(),
                    thinking: false,
                    done: true,
                    tool_calls_requested: false,
                    streamed_tool_calls: Vec::new(),
                    usage: None,
                },
            )]))),
        }
    }
    async fn chat_complete(&self, _req: ChatRequest) -> anyhow::Result<ChatComplete> {
        Ok(ChatComplete {
            text: String::new(),
            tool_calls: vec![ToolCall {
                id: "c1".into(),
                name: "echo_tool".into(),
                arguments: serde_json::json!({}),
            }],
            usage: None,
        })
    }
}

#[tokio::test]
async fn on_turn_start_hook_injects_system_message() {
    let hooks_dir = std::env::temp_dir()
        .join("sigil-agent-hooks-tests")
        .join("turn-start");
    std::fs::create_dir_all(&hooks_dir).unwrap();
    std::fs::write(
        hooks_dir.join("on_turn_start.lua"),
        "return function(ctx) return 'TURN-START-HOOK step=' .. ctx.step end",
    )
    .unwrap();
    let lua = std::sync::Arc::new(std::sync::Mutex::new(
        crate::lua::LuaRuntime::new().unwrap(),
    ));
    let mut config = test_config();
    config.hooks = Some(std::sync::Arc::new(crate::hooks::HookRunner::new(
        lua, hooks_dir,
    )));

    let mut tools = crate::tools::ToolRegistry::new();
    tools.register(Arc::new(EchoTool));
    let provider = TurnStartProvider {
        calls: AtomicUsize::new(0),
        saw_inject: AtomicBool::new(false),
    };

    let answer = run_messages(
        &provider,
        &mut tools,
        &config,
        &[ChatMessage::user("run echo")],
        None,
        None,
    )
    .await
    .expect("turn succeeds");
    assert_eq!(answer.output, "INJECTED-DONE");
    assert!(
        provider.saw_inject.load(Ordering::SeqCst),
        "on_turn_start hook text must appear in the first request"
    );
}

/// P0 (serial decision): an `on_pre_tool` listener that returns `true`
/// for a tool must block the call before it runs — the tool never
/// executes and the model sees the veto reason as the tool result.
#[derive(Debug)]
struct PreToolVetoProvider {
    calls: AtomicUsize,
    saw_veto: AtomicBool,
}
#[async_trait]
impl LlmProvider for PreToolVetoProvider {
    fn name(&self) -> &str {
        "pre-tool-veto-test"
    }
    async fn chat_stream(&self, req: ChatRequest) -> anyhow::Result<ChatStream> {
        let n = self.calls.fetch_add(1, Ordering::SeqCst);
        if n == 0 {
            Ok(Box::new(futures_util::stream::iter(vec![Ok(
                crate::llm::provider::ChatChunk {
                    delta: String::new(),
                    thinking: false,
                    done: true,
                    tool_calls_requested: true,
                    streamed_tool_calls: Vec::new(),
                    usage: None,
                },
            )])))
        } else {
            let vetoed = req
                .messages
                .iter()
                .filter(|m| matches!(m.role, crate::llm::provider::Role::Tool))
                .any(|m| m.content.contains("blocked by on_pre_tool hook"));
            self.saw_veto.store(vetoed, Ordering::SeqCst);
            Ok(Box::new(futures_util::stream::iter(vec![Ok(
                crate::llm::provider::ChatChunk {
                    delta: "VETO-DONE".into(),
                    thinking: false,
                    done: true,
                    tool_calls_requested: false,
                    streamed_tool_calls: Vec::new(),
                    usage: None,
                },
            )])))
        }
    }
    async fn chat_complete(&self, _req: ChatRequest) -> anyhow::Result<ChatComplete> {
        Ok(ChatComplete {
            text: String::new(),
            tool_calls: vec![ToolCall {
                id: "c1".into(),
                name: "echo_tool".into(),
                arguments: serde_json::json!({}),
            }],
            usage: None,
        })
    }
}

#[tokio::test]
async fn on_pre_tool_hook_vetoes_tool_call() {
    let hooks_dir = std::env::temp_dir()
        .join("sigil-agent-hooks-tests")
        .join("pre-tool-veto");
    std::fs::create_dir_all(&hooks_dir).unwrap();
    let slot = hooks_dir.join("on_pre_tool.d");
    std::fs::create_dir_all(&slot).unwrap();
    // A serial veto: the first non-null verdict wins. This listener
    // vetoes echo_tool specifically; returning `true` blocks the call.
    std::fs::write(
        slot.join("01_safety.lua"),
        "return function(ctx) return ctx.name == 'echo_tool' end",
    )
    .unwrap();
    let lua = std::sync::Arc::new(std::sync::Mutex::new(
        crate::lua::LuaRuntime::new().unwrap(),
    ));
    let mut config = test_config();
    config.hooks = Some(std::sync::Arc::new(crate::hooks::HookRunner::new(
        lua, hooks_dir,
    )));

    let mut tools = crate::tools::ToolRegistry::new();
    tools.register(Arc::new(EchoTool));
    let provider = PreToolVetoProvider {
        calls: AtomicUsize::new(0),
        saw_veto: AtomicBool::new(false),
    };

    let answer = run_messages(
        &provider,
        &mut tools,
        &config,
        &[ChatMessage::user("run echo")],
        None,
        None,
    )
    .await
    .expect("turn succeeds");
    assert_eq!(answer.output, "VETO-DONE");
    assert!(
        provider.saw_veto.load(Ordering::SeqCst),
        "the vetoes echo_tool call must reach the model as a blocked result"
    );
}

/// P0 (emit observation): when the turn ends with a final answer, an
/// `on_done` listener must observe it. The hook is observational, so it
/// signals by writing a marker file through a host helper.
#[derive(Debug)]
struct DoneAnswerProvider;
#[async_trait]
impl LlmProvider for DoneAnswerProvider {
    fn name(&self) -> &str {
        "done-answer-test"
    }
    async fn chat_stream(&self, _req: ChatRequest) -> anyhow::Result<ChatStream> {
        Ok(Box::new(futures_util::stream::iter(vec![Ok(
            crate::llm::provider::ChatChunk {
                delta: "DONE-ANSWER".into(),
                thinking: false,
                done: true,
                tool_calls_requested: false,
                streamed_tool_calls: Vec::new(),
                usage: None,
            },
        )])))
    }
    async fn chat_complete(&self, _req: ChatRequest) -> anyhow::Result<ChatComplete> {
        unreachable!("no tool calls in this test")
    }
}

#[tokio::test]
async fn on_done_hook_observes_final_answer() {
    let base = std::env::temp_dir()
        .join("sigil-agent-hooks-tests")
        .join("done");
    std::fs::create_dir_all(&base).unwrap();
    let slot = base.join("on_done.d");
    std::fs::create_dir_all(&slot).unwrap();
    let marker = base.join("on_done_marker.txt");
    let _ = std::fs::remove_file(&marker);
    // The path is embedded inside a Lua string literal; backslashes are
    // Lua escape characters, so normalize to forward slashes (Windows
    // file APIs accept `/`). This is a test-only concern — production
    // hook files are user-authored with valid paths.
    let marker_s = marker.to_string_lossy().replace('\\', "/");
    // The emit listener writes a marker carrying the observed output —
    // proving on_done fired and received the final answer.
    std::fs::write(
        slot.join("01.log.lua"),
        format!(
            "return function(ctx) host.write_file('{marker_s}', 'on_done:' .. ctx.output); return nil end"
        ),
    )
    .unwrap();
    let lua = std::sync::Arc::new(std::sync::Mutex::new(
        crate::lua::LuaRuntime::new().unwrap(),
    ));
    let mut config = test_config();
    config.hooks = Some(std::sync::Arc::new(crate::hooks::HookRunner::new(
        lua, base,
    )));

    let mut tools = crate::tools::ToolRegistry::new();
    let provider = DoneAnswerProvider;

    let answer = run_messages(
        &provider,
        &mut tools,
        &config,
        &[ChatMessage::user("answer")],
        None,
        None,
    )
    .await
    .expect("turn succeeds");
    assert_eq!(answer.output, "DONE-ANSWER");
    let written = std::fs::read_to_string(&marker).expect("on_done marker written");
    assert_eq!(
        written, "on_done:DONE-ANSWER",
        "on_done hook must observe the final answer"
    );
}

#[test]
fn on_prompt_build_hook_appends_by_default() {
    let hooks_dir = std::env::temp_dir()
        .join("sigil-agent-hooks-tests")
        .join("prompt-build");
    std::fs::create_dir_all(&hooks_dir).unwrap();
    std::fs::write(
        hooks_dir.join("on_prompt_build.lua"),
        "return function(ctx) return 'CUSTOM:' .. ctx.user_prompt end",
    )
    .unwrap();
    let lua = std::sync::Arc::new(std::sync::Mutex::new(
        crate::lua::LuaRuntime::new().unwrap(),
    ));
    let mut config = test_config();
    config.hooks = Some(std::sync::Arc::new(crate::hooks::HookRunner::new(
        lua, hooks_dir,
    )));

    // Cache discipline: hook text is appended, base prefix preserved.
    let out = config.system_prompt_for("hello");
    assert!(out.starts_with("base"), "prefix must stay byte-stable");
    assert!(out.ends_with("CUSTOM:hello"), "hook text appended");
    // The base prompt is still reachable through ctx.
    let dir = config
        .hooks
        .as_ref()
        .map(|h| h.hooks_dir().to_path_buf())
        .unwrap();
    std::fs::write(
        dir.join("on_prompt_build.lua"),
        "return function(ctx) return ctx.base .. '|APPENDED' end",
    )
    .unwrap();
    let out2 = config.system_prompt_for("hi");
    assert!(out2.starts_with("base"), "prefix still byte-stable");
    assert!(out2.ends_with("base|APPENDED"));
}

#[test]
fn on_prompt_build_replace_all_escape_hatch() {
    let hooks_dir = std::env::temp_dir()
        .join("sigil-agent-hooks-tests")
        .join("prompt-build-replace-all");
    std::fs::create_dir_all(&hooks_dir).unwrap();
    std::fs::write(
        hooks_dir.join("on_prompt_build.lua"),
        "return function(ctx) return '__REPLACE_ALL__FULL:' .. ctx.user_prompt end",
    )
    .unwrap();
    let lua = std::sync::Arc::new(std::sync::Mutex::new(
        crate::lua::LuaRuntime::new().unwrap(),
    ));
    let mut config = test_config();
    config.hooks = Some(std::sync::Arc::new(crate::hooks::HookRunner::new(
        lua, hooks_dir,
    )));

    assert_eq!(config.system_prompt_for("hello"), "FULL:hello");
}

#[test]
fn on_prompt_build_hook_error_falls_back_to_base() {
    let hooks_dir = std::env::temp_dir()
        .join("sigil-agent-hooks-tests")
        .join("prompt-build-error");
    std::fs::create_dir_all(&hooks_dir).unwrap();
    std::fs::write(
        hooks_dir.join("on_prompt_build.lua"),
        "return function(ctx) error('boom') end",
    )
    .unwrap();
    let lua = std::sync::Arc::new(std::sync::Mutex::new(
        crate::lua::LuaRuntime::new().unwrap(),
    ));
    let mut config = test_config();
    config.hooks = Some(std::sync::Arc::new(crate::hooks::HookRunner::new(
        lua, hooks_dir,
    )));

    // Prompt assembly must never break: hook error → base prompt.
    assert_eq!(config.system_prompt_for("hello"), "base");
}

#[tokio::test]
async fn run_messages_injects_system_prompt_when_missing() {
    // REPL / pipe mode pass user-only message lists; the loop must
    // inject the harness system prompt (and fire on_prompt_build).
    #[derive(Debug)]
    struct SysCheckProvider {
        calls: AtomicUsize,
        saw_system: AtomicBool,
    }
    #[async_trait]
    impl LlmProvider for SysCheckProvider {
        fn name(&self) -> &str {
            "sys-check-test"
        }
        async fn chat_stream(&self, req: ChatRequest) -> anyhow::Result<ChatStream> {
            let n = self.calls.fetch_add(1, Ordering::SeqCst);
            match n {
                0 => {
                    let sys = req
                        .messages
                        .iter()
                        .find(|m| matches!(m.role, crate::llm::provider::Role::System));
                    self.saw_system.store(
                        sys.is_some_and(|m| m.content.contains("base")),
                        Ordering::SeqCst,
                    );
                    Ok(Box::new(futures_util::stream::iter(vec![Ok(
                        crate::llm::provider::ChatChunk {
                            delta: String::new(),
                            thinking: false,
                            done: true,
                            tool_calls_requested: true,
                            streamed_tool_calls: Vec::new(),
                            usage: None,
                        },
                    )])))
                }
                _ => Ok(Box::new(futures_util::stream::iter(vec![Ok(
                    crate::llm::provider::ChatChunk {
                        delta: "SYS-OK".into(),
                        thinking: false,
                        done: true,
                        tool_calls_requested: false,
                        streamed_tool_calls: Vec::new(),
                        usage: None,
                    },
                )]))),
            }
        }
        async fn chat_complete(&self, _req: ChatRequest) -> anyhow::Result<ChatComplete> {
            Ok(ChatComplete {
                text: String::new(),
                tool_calls: vec![ToolCall {
                    id: "c1".into(),
                    name: "echo_tool".into(),
                    arguments: serde_json::json!({}),
                }],
                usage: None,
            })
        }
    }

    let mut tools = crate::tools::ToolRegistry::new();
    tools.register(Arc::new(EchoTool));
    let provider = SysCheckProvider {
        calls: AtomicUsize::new(0),
        saw_system: AtomicBool::new(false),
    };
    let config = test_config();

    let answer = run_messages(
        &provider,
        &mut tools,
        &config,
        &[ChatMessage::user("hello")],
        None,
        None,
    )
    .await
    .expect("turn succeeds");
    assert_eq!(answer.output, "SYS-OK");
    assert!(
        provider.saw_system.load(Ordering::SeqCst),
        "user-only message list must get the harness system prompt injected"
    );
}

#[tokio::test]
async fn parallel_tool_calls_survive_unknown_tool() {
    // One call names a tool that does not exist: the turn must not
    // abort — the model gets an error tool_result and can answer.
    let starts: Arc<Mutex<Vec<Instant>>> = Arc::new(Mutex::new(Vec::new()));
    let mut tools = crate::tools::ToolRegistry::new();
    tools.register(Arc::new(FastTool(starts.clone())));

    let provider = UnknownToolProvider {
        calls: AtomicUsize::new(0),
    };
    let config = test_config();
    let answer = run_messages(
        &provider,
        &mut tools,
        &config,
        &[ChatMessage::user("do the thing")],
        None,
        None,
    )
    .await
    .expect("unknown tool must not abort the turn");
    assert!(answer.output.contains("RECOVERED"));
}

/// Always asks for tools, so the loop can only end via the step
/// budget — which must yield a wrap-up summary, not an error.
#[derive(Debug)]
struct EndlessToolProvider {
    calls: AtomicUsize,
}
#[async_trait]
impl LlmProvider for EndlessToolProvider {
    fn name(&self) -> &str {
        "endless-tools-test"
    }
    async fn chat_stream(&self, req: ChatRequest) -> anyhow::Result<ChatStream> {
        let _ = self.calls.fetch_add(1, Ordering::SeqCst);
        // The final (tool-less) call is provide_final_answer: it
        // must return a plain summary, not request more tools.
        let is_summary = req.tools.is_empty();
        let delta = if is_summary {
            "WRAP-UP-SUMMARY".to_string()
        } else {
            String::new()
        };
        Ok(Box::new(futures_util::stream::iter(vec![Ok(
            crate::llm::provider::ChatChunk {
                delta,
                thinking: false,
                done: true,
                tool_calls_requested: !is_summary,
                streamed_tool_calls: Vec::new(),
                usage: None,
            },
        )])))
    }
    async fn chat_complete(&self, _req: ChatRequest) -> anyhow::Result<ChatComplete> {
        Ok(ChatComplete {
            text: String::new(),
            tool_calls: vec![ToolCall {
                id: "call-endless".into(),
                name: "fast_tool".into(),
                arguments: serde_json::json!({}),
            }],
            usage: None,
        })
    }
}

#[tokio::test]
async fn step_limit_returns_wrap_up_summary_not_error() {
    let starts: Arc<Mutex<Vec<Instant>>> = Arc::new(Mutex::new(Vec::new()));
    let mut tools = crate::tools::ToolRegistry::new();
    tools.register(Arc::new(FastTool(starts.clone())));

    let provider = EndlessToolProvider {
        calls: AtomicUsize::new(0),
    };
    let mut config = test_config();
    config.max_steps = 3;
    let result = run_messages(
        &provider,
        &mut tools,
        &config,
        &[ChatMessage::user("keep going")],
        None,
        None,
    )
    .await
    .expect("step limit is a normal result, not an error");
    assert_eq!(result.state, RunState::StepLimitReached);
    assert_eq!(result.output, "WRAP-UP-SUMMARY");
    // 3 budgeted loop iterations + 1 final summary call.
    assert_eq!(provider.calls.load(Ordering::SeqCst), 4);
}

/// Emits [unknown_tool, fast_tool] then checks the model received
/// an error for the unknown one and a real result for fast_tool.
#[derive(Debug)]
struct UnknownToolProvider {
    calls: AtomicUsize,
}
#[async_trait]
impl LlmProvider for UnknownToolProvider {
    fn name(&self) -> &str {
        "unknown-tool-test"
    }
    async fn chat_stream(&self, req: ChatRequest) -> anyhow::Result<ChatStream> {
        let n = self.calls.fetch_add(1, Ordering::SeqCst);
        if n == 0 {
            Ok(Box::new(futures_util::stream::iter(vec![Ok(
                crate::llm::provider::ChatChunk {
                    delta: String::new(),
                    thinking: false,
                    done: true,
                    tool_calls_requested: true,
                    streamed_tool_calls: Vec::new(),
                    usage: None,
                },
            )])))
        } else {
            // Second loop iteration: verify results for both calls
            // (error for the unknown one, real result for fast).
            let results: Vec<&str> = req
                .messages
                .iter()
                .filter(|m| m.role == crate::llm::provider::Role::Tool)
                .map(|m| m.content.as_str())
                .collect();
            assert_eq!(results.len(), 2);
            assert!(results[0].contains("error"), "unknown tool -> error result");
            assert_eq!(results[1], "FAST");
            Ok(Box::new(futures_util::stream::iter(vec![Ok(
                crate::llm::provider::ChatChunk {
                    delta: "RECOVERED".into(),
                    thinking: false,
                    done: true,
                    tool_calls_requested: false,
                    streamed_tool_calls: Vec::new(),
                    usage: None,
                },
            )])))
        }
    }
    async fn chat_complete(&self, _req: ChatRequest) -> anyhow::Result<ChatComplete> {
        // First-iteration replay only.
        Ok(ChatComplete {
            text: String::new(),
            tool_calls: vec![
                ToolCall {
                    id: "call-x".into(),
                    name: "no_such_tool".into(),
                    arguments: serde_json::json!({}),
                },
                ToolCall {
                    id: "call-y".into(),
                    name: "fast_tool".into(),
                    arguments: serde_json::json!({}),
                },
            ],
            usage: None,
        })
    }
}

/// Straight-to-answer provider: one stream with text + done, no
/// tools. chat_complete is never called on this path.
#[derive(Debug)]
struct AnswerProvider;
#[async_trait]
impl LlmProvider for AnswerProvider {
    fn name(&self) -> &str {
        "answer-test"
    }
    async fn chat_stream(&self, _req: ChatRequest) -> anyhow::Result<ChatStream> {
        Ok(Box::new(futures_util::stream::iter(vec![
            Ok(crate::llm::provider::ChatChunk {
                delta: "the answer".into(),
                thinking: false,
                done: false,
                tool_calls_requested: false,
                streamed_tool_calls: Vec::new(),
                usage: None,
            }),
            Ok(crate::llm::provider::ChatChunk {
                delta: String::new(),
                thinking: false,
                done: true,
                tool_calls_requested: false,
                streamed_tool_calls: Vec::new(),
                usage: None,
            }),
        ])))
    }
    async fn chat_complete(&self, _req: ChatRequest) -> anyhow::Result<ChatComplete> {
        anyhow::bail!("not expected on this path")
    }
}

/// Empty-then-answer provider: first stream returns no content,
/// second (after the nudge) answers. Guards the bounded empty-
/// response retry.
#[derive(Debug)]
struct EmptyThenAnswerProvider(std::sync::atomic::AtomicUsize);
#[async_trait]
impl LlmProvider for EmptyThenAnswerProvider {
    fn name(&self) -> &str {
        "empty-then-answer-test"
    }
    async fn chat_stream(&self, _req: ChatRequest) -> anyhow::Result<ChatStream> {
        let n = self.0.fetch_add(1, Ordering::SeqCst);
        let delta = if n == 0 { "" } else { "recovered answer" };
        Ok(Box::new(futures_util::stream::iter(vec![
            Ok(crate::llm::provider::ChatChunk {
                delta: delta.into(),
                thinking: false,
                done: false,
                tool_calls_requested: false,
                streamed_tool_calls: Vec::new(),
                usage: None,
            }),
            Ok(crate::llm::provider::ChatChunk {
                delta: String::new(),
                thinking: false,
                done: true,
                tool_calls_requested: false,
                streamed_tool_calls: Vec::new(),
                usage: None,
            }),
        ])))
    }
    async fn chat_complete(&self, _req: ChatRequest) -> anyhow::Result<ChatComplete> {
        anyhow::bail!("not expected on this path")
    }
}

#[test]
fn planning_checkpoint_cleanup_is_structural() {
    // The checkpoint tag, not the prompt text, decides what gets
    // removed: a tool result quoting the checkpoint prompt (or any
    // other message carrying that text) must survive cleanup.
    let mut msgs = vec![
        ChatMessage::system("SYS"),
        ChatMessage::user("q"),
        ChatMessage::planning_checkpoint(), // tagged injection
        ChatMessage::tool_result(
            format!("output: {}", crate::agent::PLANNING_CHECKPOINT_PROMPT),
            "c1",
        ),
        ChatMessage::assistant_with_tools(
            crate::agent::PLANNING_CHECKPOINT_PROMPT, // legacy untagged
            vec![crate::llm::provider::ToolCall {
                id: "c1".into(),
                name: "bash".into(),
                arguments: serde_json::json!({}),
            }],
        ),
    ];
    // Same retain as the loop's cleanup.
    msgs.retain(|m| !m.planning_checkpoint && !m.content.starts_with("PLANNING CHECKPOINT:"));
    assert_eq!(
        msgs.len(),
        3,
        "only the two checkpoint texts must go: {:?}",
        msgs.len()
    );
    assert_eq!(msgs[0].role, Role::System);
    assert_eq!(msgs[1].role, Role::User);
    assert_eq!(
        msgs[2].role,
        Role::Tool,
        "tool result quoting the prompt must survive: {:?}",
        msgs[2].content
    );
}

/// Mid-stream transient failure: first stream delivers a partial
/// delta then errors with a 502-class message; the retry must
/// restart the request and produce the full answer (the partial
/// text is discarded, not concatenated).
#[derive(Debug)]
struct FlakyStreamProvider(std::sync::atomic::AtomicUsize);
#[async_trait]
impl LlmProvider for FlakyStreamProvider {
    fn name(&self) -> &str {
        "flaky-stream-test"
    }
    async fn chat_stream(&self, _req: ChatRequest) -> anyhow::Result<ChatStream> {
        let n = self.0.fetch_add(1, Ordering::SeqCst);
        if n == 0 {
            // One delta, then the connection dies mid-completion.
            Ok(Box::new(futures_util::stream::iter(vec![
                Ok(crate::llm::provider::ChatChunk {
                    delta: "part".into(),
                    thinking: false,
                    done: false,
                    tool_calls_requested: false,
                    streamed_tool_calls: Vec::new(),
                    usage: None,
                }),
                Err(anyhow::anyhow!("HTTP 502 bad gateway from upstream")),
            ])))
        } else {
            Ok(Box::new(futures_util::stream::iter(vec![
                Ok(crate::llm::provider::ChatChunk {
                    delta: "full answer".into(),
                    thinking: false,
                    done: false,
                    tool_calls_requested: false,
                    streamed_tool_calls: Vec::new(),
                    usage: None,
                }),
                Ok(crate::llm::provider::ChatChunk {
                    delta: String::new(),
                    thinking: false,
                    done: true,
                    tool_calls_requested: false,
                    streamed_tool_calls: Vec::new(),
                    usage: None,
                }),
            ])))
        }
    }
    async fn chat_complete(&self, _req: ChatRequest) -> anyhow::Result<ChatComplete> {
        anyhow::bail!("not expected on this path")
    }
}

#[tokio::test]
async fn transient_mid_stream_failure_gets_one_retry() {
    let provider = FlakyStreamProvider(std::sync::atomic::AtomicUsize::new(0));
    let mut tools = crate::tools::ToolRegistry::new();
    let config = test_config();
    let out = run_messages(
        &provider,
        &mut tools,
        &config,
        &[ChatMessage::user("q")],
        None,
        None,
    )
    .await
    .unwrap();
    assert_eq!(
        out.output, "full answer",
        "mid-stream 502 must retry, got {:?}",
        out.output
    );
    assert_eq!(provider.0.load(Ordering::SeqCst), 2);
}

#[tokio::test]
async fn empty_response_gets_one_bounded_retry() {
    let provider = EmptyThenAnswerProvider(std::sync::atomic::AtomicUsize::new(0));
    let mut tools = crate::tools::ToolRegistry::new();
    let config = test_config();
    let out = run_messages(
        &provider,
        &mut tools,
        &config,
        &[ChatMessage::user("q")],
        None,
        None,
    )
    .await
    .unwrap();
    assert_eq!(
        out.output, "recovered answer",
        "empty stream must trigger one nudge retry, got {:?}",
        out.output
    );
    assert_eq!(
        provider.0.load(Ordering::SeqCst),
        2,
        "exactly two streams: empty + retry"
    );
}

#[derive(Debug)]
struct RecordingProvider {
    requests: Mutex<Vec<Vec<ChatMessage>>>,
    first_tool: bool,
}
#[async_trait]
impl LlmProvider for RecordingProvider {
    fn name(&self) -> &str {
        "recording"
    }
    async fn chat_stream(&self, req: ChatRequest) -> anyhow::Result<ChatStream> {
        let mut requests = self.requests.lock().unwrap();
        let tools = self.first_tool && requests.is_empty();
        requests.push(req.messages);
        Ok(Box::new(futures_util::stream::iter(vec![Ok(
            crate::llm::provider::ChatChunk {
                delta: if tools {
                    String::new()
                } else {
                    "answer".into()
                },
                thinking: false,
                done: true,
                tool_calls_requested: tools,
                streamed_tool_calls: if tools {
                    vec![ToolCall {
                        id: "echo-1".into(),
                        name: "echo_tool".into(),
                        arguments: serde_json::json!({}),
                    }]
                } else {
                    vec![]
                },
                usage: None,
            },
        )])))
    }
    async fn chat_complete(&self, _req: ChatRequest) -> anyhow::Result<ChatComplete> {
        anyhow::bail!("unexpected replay")
    }
}

#[tokio::test]
async fn next_turn_request_retains_tool_results_and_compaction() {
    let provider = RecordingProvider {
        requests: Mutex::new(Vec::new()),
        first_tool: true,
    };
    let mut tools = ToolRegistry::new();
    tools.register(Arc::new(EchoTool));
    let config = test_config();
    let first = run_messages(
        &provider,
        &mut tools,
        &config,
        &[ChatMessage::user("read")],
        None,
        None,
    )
    .await
    .unwrap();
    let mut next = messages_for_turn(&first.messages, "fresh harness".into());
    next.push(ChatMessage::user("follow up"));
    run_messages(&provider, &mut tools, &config, &next, None, None)
        .await
        .unwrap();
    {
        let requests = provider.requests.lock().unwrap();
        let request = requests.last().unwrap();
        assert_eq!(request[0].content, "fresh harness");
        assert!(
            request
                .iter()
                .any(|m| m.role == Role::Tool && m.content == "OUT")
        );
        assert!(
            request
                .iter()
                .any(|m| m.role == Role::Assistant && m.content == "answer")
        );
    }

    let provider = RecordingProvider {
        requests: Mutex::new(Vec::new()),
        first_tool: false,
    };
    let mut config = test_config();
    config.max_context_messages = 6;
    let mut history = vec![ChatMessage::system("old harness")];
    for index in 0..20 {
        history.push(ChatMessage::user(format!("old-{index}")));
        history.push(ChatMessage::assistant("reply"));
    }
    let first = run_messages(&provider, &mut tools, &config, &history, None, None)
        .await
        .unwrap();
    assert!(first.messages.len() <= 7);
    let mut next = messages_for_turn(&first.messages, "fresh harness".into());
    next.push(ChatMessage::user("continue"));
    run_messages(&provider, &mut tools, &config, &next, None, None)
        .await
        .unwrap();
    let requests = provider.requests.lock().unwrap();
    let request = requests.last().unwrap();
    assert!(request.len() <= 6);
    assert!(
        request.iter().any(|m| m.role == Role::System
            && m.content.starts_with("[earlier conversation compressed]"))
    );
    assert_eq!(request[0].content, "fresh harness");
}

#[test]
fn prompt_refresh_keeps_multiple_summaries() {
    let history = vec![
        ChatMessage::system("old"),
        ChatMessage::system("[earlier conversation compressed] first"),
        ChatMessage::system("[earlier conversation compressed] second"),
        ChatMessage::user("q"),
    ];
    let refreshed = messages_for_turn(&history, "new".into());
    assert_eq!(refreshed.len(), history.len());
    assert_eq!(refreshed[0].content, "new");
    assert_eq!(refreshed[1].content, history[1].content);
    assert_eq!(refreshed[2].content, history[2].content);
    let refreshed = messages_for_turn(&history[1..], "new".into());
    assert_eq!(refreshed[1].content, history[1].content);
}

#[tokio::test]
async fn goal_budget_is_persisted_and_one_turn_goal_is_visible() {
    let dir = std::env::temp_dir().join(format!(
        "sigil-budget-{}",
        crate::session::chrono_like_timestamp()
    ));
    let goals = vec![
        crate::goals::GoalState::with_turn_budget("one".into(), "finish objective".into(), 1),
        crate::goals::GoalState::with_turn_budget("many".into(), "another objective".into(), 10),
    ];
    crate::goals::save_goals(&dir, &goals).unwrap();
    let mut config = test_config();
    config.goals = Some(std::sync::Arc::new(std::sync::RwLock::new(goals)));
    config.goals_dir = Some(dir.clone());
    let provider = RecordingProvider {
        requests: Mutex::new(Vec::new()),
        first_tool: false,
    };
    let out = run_messages(
        &provider,
        &mut ToolRegistry::new(),
        &config,
        &[ChatMessage::user("q")],
        None,
        None,
    )
    .await
    .unwrap();
    assert!(
        provider.requests.lock().unwrap()[0]
            .iter()
            .any(|m| m.content.contains("finish objective")),
        "budget-one goal must reach its request"
    );
    assert_eq!(crate::goals::load_goals(&dir)[1].turns_used, 1);
    assert_eq!(
        out.messages.last().unwrap().content,
        out.output,
        "final answer belongs in transcript"
    );
    config.goals = Some(Arc::new(std::sync::RwLock::new(crate::goals::load_goals(
        &dir,
    ))));
    run_messages(
        &provider,
        &mut ToolRegistry::new(),
        &config,
        &[ChatMessage::user("after restart")],
        None,
        None,
    )
    .await
    .unwrap();
    assert_eq!(crate::goals::load_goals(&dir)[1].turns_used, 2);
    assert!(
        !provider
            .requests
            .lock()
            .unwrap()
            .last()
            .unwrap()
            .iter()
            .any(|m| m.content.contains("finish objective"))
    );
    std::fs::remove_dir_all(dir).unwrap();
}

#[tokio::test]
async fn partial_budget_usage_survives_restart() {
    let dir = std::env::temp_dir().join(format!(
        "sigil-partial-budget-{}",
        crate::session::chrono_like_timestamp()
    ));
    crate::goals::save_goals(
        &dir,
        &[crate::goals::GoalState::with_turn_budget(
            "g".into(),
            "finish objective".into(),
            10,
        )],
    )
    .unwrap();
    let mut config = test_config();
    config.goals_dir = Some(dir.clone());
    for expected in 1..=2 {
        config.goals = Some(Arc::new(std::sync::RwLock::new(crate::goals::load_goals(
            &dir,
        ))));
        run_messages(
            &AnswerProvider,
            &mut ToolRegistry::new(),
            &config,
            &[ChatMessage::user("q")],
            None,
            None,
        )
        .await
        .unwrap();
        let persisted = crate::goals::load_goals(&dir);
        assert_eq!(persisted[0].turns_used, expected);
        assert_eq!(persisted[0].status, crate::goals::GoalStatus::Active);
    }
    std::fs::remove_dir_all(dir).unwrap();
}

#[tokio::test]
async fn paused_goals_remove_prior_injection() {
    let mut goal = crate::goals::GoalState::new("g".into(), "finish objective".into());
    goal.status = crate::goals::GoalStatus::Paused;
    let mut config = test_config();
    config.goals = Some(std::sync::Arc::new(std::sync::RwLock::new(vec![goal])));
    let out = run_messages(
        &AnswerProvider,
        &mut ToolRegistry::new(),
        &config,
        &[
            ChatMessage::system("base"),
            ChatMessage::system("## Active goals\nfinish objective"),
            ChatMessage::user("q"),
        ],
        None,
        None,
    )
    .await
    .unwrap();
    assert!(
        !out.messages
            .iter()
            .any(|m| m.content.starts_with(crate::goals::GOALS_MARKER))
    );
}

/// Contract the daemon/REPL rely on since the callers stopped
/// appending the assistant turn manually: EVERY success exit of
/// run_messages must emit the final answer as an Assistant event
/// and close the turn with TurnEnd through the session sink. The
/// plain final-answer exit used to emit neither (daemon turns lost
/// tool history AND relied on manual appends).
#[tokio::test]
async fn success_exits_log_assistant_and_turn_end_via_sink() {
    let events = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
    let mut config = test_config();
    {
        let ev = events.clone();
        config.session_sink = Some(std::sync::Arc::new(move |e| {
            ev.lock().unwrap().push(e);
        }));
    }
    let provider = AnswerProvider;
    let mut tools = crate::tools::ToolRegistry::new();
    let msgs = vec![ChatMessage::user("q")];

    // Plain final-answer exit.
    let out = run_messages(&provider, &mut tools, &config, &msgs, None, None)
        .await
        .unwrap();
    assert_eq!(out.output, "the answer");
    let logged = events.lock().unwrap().clone();
    let kind = |e: &crate::session::Event| -> &'static str {
        match e {
            crate::session::Event::Assistant { .. } => "assistant",
            crate::session::Event::TurnEnd { .. } => "turn_end",
            crate::session::Event::TurnStart { .. } => "turn_start",
            _ => "other",
        }
    };
    let kinds: Vec<&str> = logged.iter().map(kind).collect();
    assert!(
        kinds.contains(&"assistant"),
        "plain exit must log Assistant: {kinds:?}"
    );
    assert!(
        kinds.contains(&"turn_end"),
        "plain exit must log TurnEnd: {kinds:?}"
    );

    // Step-limit exit: one tool round first (the budget check fires
    // at the top of the loop, so the turn must consume its step
    // before the wrap-up), then the limit forces provide_final_answer.
    #[derive(Debug)]
    struct ToolOnceProvider(std::sync::atomic::AtomicUsize);
    #[async_trait]
    impl LlmProvider for ToolOnceProvider {
        fn name(&self) -> &str {
            "tool-once-test"
        }
        async fn chat_stream(&self, _req: ChatRequest) -> anyhow::Result<ChatStream> {
            let n = self.0.fetch_add(1, Ordering::SeqCst);
            let (delta, tool_calls_requested) = if n == 0 {
                (String::new(), true)
            } else {
                ("wrap-up summary".into(), false)
            };
            Ok(Box::new(futures_util::stream::iter(vec![
                Ok(crate::llm::provider::ChatChunk {
                    delta,
                    thinking: false,
                    done: false,
                    tool_calls_requested,
                    streamed_tool_calls: Vec::new(),
                    usage: None,
                }),
                Ok(crate::llm::provider::ChatChunk {
                    delta: String::new(),
                    thinking: false,
                    done: true,
                    tool_calls_requested,
                    streamed_tool_calls: Vec::new(),
                    usage: None,
                }),
            ])))
        }
        async fn chat_complete(&self, _req: ChatRequest) -> anyhow::Result<ChatComplete> {
            Ok(ChatComplete {
                text: String::new(),
                tool_calls: vec![ToolCall {
                    id: "c1".into(),
                    name: "noop".into(),
                    arguments: serde_json::json!({}),
                }],
                usage: None,
            })
        }
    }
    struct NoopTool;
    #[async_trait]
    impl crate::tools::Tool for NoopTool {
        fn name(&self) -> &str {
            "noop"
        }
        fn description(&self) -> &str {
            "noop for the step-limit test"
        }
        fn parameters(&self) -> serde_json::Value {
            serde_json::json!({})
        }
        async fn run(&self, _args: &serde_json::Value) -> anyhow::Result<crate::tools::ToolOutput> {
            Ok(crate::tools::ToolOutput { text: "ok".into() })
        }
    }
    events.lock().unwrap().clear();
    let mut limit_config = config.clone();
    limit_config.max_steps = 1;
    let mut limit_tools = crate::tools::ToolRegistry::new();
    limit_tools.register(std::sync::Arc::new(NoopTool));
    let out = run_messages(
        &ToolOnceProvider(std::sync::atomic::AtomicUsize::new(0)),
        &mut limit_tools,
        &limit_config,
        &msgs,
        None,
        None,
    )
    .await
    .unwrap();
    assert_eq!(out.state, crate::agent::RunState::StepLimitReached);
    assert_eq!(out.output, "wrap-up summary");
    // The tool round must be logged too (the daemon gap this wiring
    // closes): ToolCall + ToolResult + wrap-up Assistant + TurnEnd.
    let logged = events.lock().unwrap().clone();
    let kind2 = |e: &crate::session::Event| -> &'static str {
        match e {
            crate::session::Event::ToolCall { .. } => "tool_call",
            crate::session::Event::ToolResult { .. } => "tool_result",
            _ => kind(e),
        }
    };
    let kinds: Vec<&str> = logged.iter().map(kind2).collect();
    for want in ["tool_call", "tool_result", "assistant", "turn_end"] {
        assert!(
            kinds.contains(&want),
            "step-limit exit must log {want}: {kinds:?}"
        );
    }
    let logged = events.lock().unwrap().clone();
    let kinds: Vec<&str> = logged.iter().map(kind).collect();
    assert!(
        kinds.contains(&"assistant"),
        "step-limit exit must log the wrap-up summary: {kinds:?}"
    );
    assert!(
        kinds.contains(&"turn_end"),
        "step-limit exit must log TurnEnd: {kinds:?}"
    );
}
