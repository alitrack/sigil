//! Scheduler-guard tests: the first-frame floor and the no-progress stop.
//!
//! Both need a real loop run, so they drive `run_messages` against scripted
//! providers rather than calling the helpers directly.

use super::*;
use crate::llm::provider::{ChatChunk, ChatComplete, ChatRequest, ChatStream, LlmProvider};
use async_trait::async_trait;
use std::sync::atomic::{AtomicUsize, Ordering};

fn chunk(delta: &str, done: bool) -> ChatChunk {
    ChatChunk {
        delta: delta.into(),
        thinking: false,
        done,
        tool_calls_requested: false,
        streamed_tool_calls: Vec::new(),
        usage: None,
    }
}

fn text_stream(text: &str) -> ChatStream {
    Box::new(futures_util::stream::iter(vec![
        Ok(chunk(text, false)),
        Ok(chunk("", true)),
    ]))
}

fn tool_wanted_stream() -> ChatStream {
    Box::new(futures_util::stream::iter(vec![
        Ok(ChatChunk {
            delta: String::new(),
            thinking: false,
            done: false,
            tool_calls_requested: true,
            streamed_tool_calls: Vec::new(),
            usage: None,
        }),
        Ok(ChatChunk {
            delta: String::new(),
            thinking: false,
            done: true,
            tool_calls_requested: true,
            streamed_tool_calls: Vec::new(),
            usage: None,
        }),
    ]))
}

/// Streams a tool call forever; records how many tool-less calls it saw.
#[derive(Debug)]
struct AlwaysToolsProvider {
    streams: AtomicUsize,
    tool_less_calls: AtomicUsize,
}

#[async_trait]
impl LlmProvider for AlwaysToolsProvider {
    fn name(&self) -> &str {
        "always-tools"
    }
    async fn chat_stream(&self, req: ChatRequest) -> anyhow::Result<ChatStream> {
        if req.tools.is_empty() {
            self.tool_less_calls.fetch_add(1, Ordering::SeqCst);
            return Ok(text_stream("DRAFT"));
        }
        Ok(tool_wanted_stream())
    }
    async fn chat_complete(&self, _req: ChatRequest) -> anyhow::Result<ChatComplete> {
        Ok(ChatComplete {
            text: String::new(),
            tool_calls: vec![crate::llm::provider::ToolCall {
                id: format!("t{}", self.streams.fetch_add(1, Ordering::SeqCst)),
                name: "noop".into(),
                arguments: serde_json::json!({}),
            }],
            usage: None,
        })
    }
}

/// Answers with empty content on the first (tool-less draft) call and streams a
/// real answer afterwards — pins that the draft does not leak into the reply.
#[derive(Debug)]
struct EmptyThenAnswerProvider {
    calls: AtomicUsize,
}

#[async_trait]
impl LlmProvider for EmptyThenAnswerProvider {
    fn name(&self) -> &str {
        "empty-then-answer"
    }
    async fn chat_stream(&self, req: ChatRequest) -> anyhow::Result<ChatStream> {
        let n = self.calls.fetch_add(1, Ordering::SeqCst);
        if req.tools.is_empty() && n == 0 {
            Ok(text_stream(""))
        } else {
            Ok(text_stream("the real answer"))
        }
    }
    async fn chat_complete(&self, _req: ChatRequest) -> anyhow::Result<ChatComplete> {
        Ok(ChatComplete {
            text: String::new(),
            tool_calls: Vec::new(),
            usage: None,
        })
    }
}

/// A tool that always returns the identical text, so every round has the same
/// progress signature.
struct SameResultTool;

#[async_trait::async_trait]
impl crate::tools::Tool for SameResultTool {
    fn name(&self) -> &str {
        "noop"
    }
    fn description(&self) -> &str {
        "returns the same thing every time"
    }
    fn parameters(&self) -> serde_json::Value {
        serde_json::json!({})
    }
    async fn run(&self, _args: &serde_json::Value) -> anyhow::Result<crate::tools::ToolOutput> {
        Ok(crate::tools::ToolOutput {
            text: "same".into(),
        })
    }
}

fn config(max_steps: u32) -> AgentConfig {
    let mut c = crate::agent::parallel_tests::test_config();
    c.max_steps = max_steps;
    c
}

#[tokio::test]
async fn no_progress_stops_the_run_and_wraps_up() {
    // Thread-local override, not the process-global env var: parallel tests
    // must not see each other's scheduler settings.
    scheduler::set_test_limits(None, Some(2));
    let provider = AlwaysToolsProvider {
        streams: AtomicUsize::new(0),
        tool_less_calls: AtomicUsize::new(0),
    };
    let mut tools = crate::tools::ToolRegistry::new();
    tools.register(std::sync::Arc::new(SameResultTool));
    let msgs = vec![ChatMessage::user("do the thing")];
    let out = run_messages(&provider, &mut tools, &config(0), &msgs, None, None)
        .await
        .unwrap();
    assert_eq!(out.state, RunState::NoProgress, "{:?}", out.state);
    // Bounded: the run stopped instead of running forever.
    assert!(out.steps <= 5, "steps={}", out.steps);
}

#[tokio::test]
async fn first_frame_draft_is_the_floor_when_the_loop_cannot_finish() {
    scheduler::set_test_limits(Some(true), Some(1));
    let provider = AlwaysToolsProvider {
        streams: AtomicUsize::new(0),
        tool_less_calls: AtomicUsize::new(0),
    };
    let mut tools = crate::tools::ToolRegistry::new();
    tools.register(std::sync::Arc::new(SameResultTool));
    let msgs = vec![ChatMessage::user("do the thing")];
    let out = run_messages(&provider, &mut tools, &config(3), &msgs, None, None)
        .await
        .unwrap();
    // The draft was taken up front and is what the caller got back.
    assert_eq!(out.output, "DRAFT");
}

#[tokio::test]
async fn an_empty_draft_does_not_replace_a_real_answer() {
    scheduler::set_test_limits(Some(true), None);
    let provider = EmptyThenAnswerProvider {
        calls: AtomicUsize::new(0),
    };
    let mut tools = crate::tools::ToolRegistry::new();
    let msgs = vec![ChatMessage::user("say something")];
    let out = run_messages(&provider, &mut tools, &config(0), &msgs, None, None)
        .await
        .unwrap();
    assert_eq!(out.output, "the real answer");
    assert_eq!(out.state, RunState::Success);
}
