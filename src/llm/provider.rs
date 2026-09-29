//! Provider abstraction — every backend (OpenAI-compatible, Anthropic,
//! llama.cpp, fusion panel, ...) implements [`LlmProvider`] so the agent
//! loop stays provider-agnostic. Borrowed from wizard's design (verified in
//! the wizard deep-dive, 2026-08-09).

use std::fmt::Debug;

use async_trait::async_trait;
use serde::{Deserialize, Serialize};

/// A single message in the conversation.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ChatMessage {
    pub role: Role,
    pub content: String,
    /// Assistant-only: native tool calls (OpenAI function-calling).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tool_calls: Option<Vec<ToolCall>>,
    /// Tool-only: id of the tool call this message answers.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tool_call_id: Option<String>,
    /// Structural marker for the agent-loop-injected planning
    /// checkpoint. Never reaches the provider (build_body is
    /// hand-written JSON and ignores it) and is not part of the session
    /// log: the checkpoint is request-scoped loop machinery, so a tool
    /// result quoting its text must not be mistaken for it (the old
    /// substring-match cleanup could delete such messages).
    #[serde(default, skip_serializing)]
    pub planning_checkpoint: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Role {
    System,
    User,
    Assistant,
    Tool,
}

impl ChatMessage {
    pub fn system(content: impl Into<String>) -> Self {
        Self {
            role: Role::System,
            content: content.into(),
            tool_calls: None,
            tool_call_id: None,
            planning_checkpoint: false,
        }
    }
    /// The planning checkpoint the agent loop injects and cleans up —
    /// structurally tagged so the loop removes exactly its own
    /// injections (and can stop matching by content).
    pub fn planning_checkpoint() -> Self {
        Self {
            role: Role::System,
            content: crate::agent::PLANNING_CHECKPOINT_PROMPT.into(),
            tool_calls: None,
            tool_call_id: None,
            planning_checkpoint: true,
        }
    }
    pub fn user(content: impl Into<String>) -> Self {
        Self {
            role: Role::User,
            content: content.into(),
            tool_calls: None,
            tool_call_id: None,
            planning_checkpoint: false,
        }
    }
    #[allow(dead_code)]
    pub fn assistant(content: impl Into<String>) -> Self {
        // Kept for Phase-1 plain-text assistant messages; tool-call
        // messages go through `assistant_with_tools`.
        Self {
            role: Role::Assistant,
            content: content.into(),
            tool_calls: None,
            tool_call_id: None,
            planning_checkpoint: false,
        }
    }
    pub fn assistant_with_tools(content: impl Into<String>, calls: Vec<ToolCall>) -> Self {
        Self {
            role: Role::Assistant,
            content: content.into(),
            tool_calls: Some(calls),
            tool_call_id: None,
            planning_checkpoint: false,
        }
    }
    pub fn tool_result(content: impl Into<String>, tool_call_id: impl Into<String>) -> Self {
        Self {
            role: Role::Tool,
            content: content.into(),
            tool_calls: None,
            tool_call_id: Some(tool_call_id.into()),
            planning_checkpoint: false,
        }
    }
}

/// A tool call emitted by the model.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolCall {
    pub id: String,
    pub name: String,
    pub arguments: serde_json::Value,
}

/// Tool schema advertised to the model (OpenAI function-calling format).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolSpec {
    pub name: String,
    pub description: String,
    pub parameters: serde_json::Value,
}

/// Request to the provider.
#[derive(Debug, Clone)]
pub struct ChatRequest {
    pub model: String,
    pub messages: Vec<ChatMessage>,
    pub tools: Vec<ToolSpec>,
    pub stream: bool,
    pub temperature: Option<f32>,
}

/// One streaming chunk from the provider.
#[derive(Debug, Clone, Default)]
pub struct ChatChunk {
    pub delta: String,
    pub thinking: bool,
    /// Terminal chunk: the provider sent a non-null `finish_reason`
    /// (`stop`, `tool_calls`, `length`, `content_filter`, ...) or the
    /// `[DONE]` sentinel. Any finish_reason terminates — a `length`-
    /// truncated answer is a complete, usable completion, not a
    /// mid-stream drop.
    pub done: bool,
    /// `finish_reason == "tool_calls"` — the model wants to call tools.
    /// The call details arrive either in `streamed_tool_calls` (when the
    /// provider streams them in deltas) or in the non-streaming replay.
    pub tool_calls_requested: bool,
    /// Tool calls accumulated from `delta.tool_calls` fragments while
    /// streaming. Providers differ: vLLM/SGLang and OpenAI proper emit
    /// `delta.tool_calls` fragments; some proxies suppress them entirely
    /// and only set `finish_reason == "tool_calls"`. When this vec is
    /// non-empty at end-of-stream, the agent loop skips the non-streaming
    /// replay (halves per-tool-call latency); when empty, the legacy
    /// replay path runs unchanged.
    pub streamed_tool_calls: Vec<ToolCall>,
    /// Token usage, when the provider includes a `usage` block in the
    /// final chunk of a streamed completion.
    pub usage: Option<TokenUsage>,
}

/// Token usage reported by the provider for one completion
/// (OpenAI-compatible `usage` block). `None` means the provider did
/// not report usage.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct TokenUsage {
    pub prompt_tokens: u64,
    pub completion_tokens: u64,
    /// Prompt tokens served from the provider's prefix cache
    /// (vLLM `prompt_cache_hit_tokens`; OpenAI-compatible providers
    /// expose it as `prompt_tokens_details.cached_tokens`). `0` when
    /// the provider did not report it. Cache-discipline diagnostics
    /// (Phase 65): DEBUG logging prints hit/total so a change in
    /// prompt assembly shows up as a measurable hit-rate drop.
    #[serde(default)]
    pub prompt_cache_hit_tokens: u64,
}

impl TokenUsage {
    /// Total tokens across prompt and completion.
    #[cfg(test)]
    pub fn total(&self) -> u64 {
        self.prompt_tokens + self.completion_tokens
    }
}

impl std::ops::Add for TokenUsage {
    type Output = TokenUsage;
    fn add(self, rhs: TokenUsage) -> TokenUsage {
        TokenUsage {
            prompt_tokens: self.prompt_tokens + rhs.prompt_tokens,
            completion_tokens: self.completion_tokens + rhs.completion_tokens,
            prompt_cache_hit_tokens: self.prompt_cache_hit_tokens + rhs.prompt_cache_hit_tokens,
        }
    }
}

/// A complete (non-streaming) completion result.
#[derive(Debug, Clone)]
pub struct ChatComplete {
    pub text: String,
    pub tool_calls: Vec<ToolCall>,
    /// Token usage, when the provider includes a `usage` block.
    pub usage: Option<TokenUsage>,
}

/// The provider contract. All backends implement this.
#[async_trait]
pub trait LlmProvider: Send + Sync + Debug {
    /// Provider display name (used in logs/status).
    #[allow(dead_code)]
    fn name(&self) -> &str;

    /// Health/availability check (optional; default OK).
    #[allow(dead_code)]
    async fn health(&self) -> anyhow::Result<()> {
        Ok(())
    }

    /// Stream a chat completion.
    async fn chat_stream(&self, req: ChatRequest) -> anyhow::Result<ChatStream>;

    /// Non-streaming completion (used for the tool-call replay).
    async fn chat_complete(&self, req: ChatRequest) -> anyhow::Result<ChatComplete>;
}

/// Boxed stream of chunks.
pub type ChatStream =
    Box<dyn futures_util::Stream<Item = anyhow::Result<ChatChunk>> + Send + Unpin>;
