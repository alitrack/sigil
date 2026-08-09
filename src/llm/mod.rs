//! LLM providers: the [`LlmProvider`] trait and every backend.
//!
//! The re-exports below are the public surface the agent loop consumes;
//! they are marked `allow(dead_code)` until all Phase-1 backends land.

pub mod openai;
pub mod provider;

#[allow(unused_imports)]
pub use provider::{
    ChatChunk, ChatMessage, ChatRequest, ChatStream, LlmProvider, Role, ToolCall, ToolSpec,
};
