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
        }
    }
    pub fn user(content: impl Into<String>) -> Self {
        Self {
            role: Role::User,
            content: content.into(),
        }
    }
    pub fn assistant(content: impl Into<String>) -> Self {
        Self {
            role: Role::Assistant,
            content: content.into(),
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
#[derive(Debug, Clone)]
pub struct ChatChunk {
    pub delta: String,
    pub thinking: bool,
    pub done: bool,
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
}

/// Boxed stream of chunks.
pub type ChatStream =
    Box<dyn futures_util::Stream<Item = anyhow::Result<ChatChunk>> + Send + Unpin>;
