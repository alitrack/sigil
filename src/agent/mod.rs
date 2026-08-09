//! The agent loop: build messages → stream completion → parse tool calls →
//! execute tools → repeat until the model is done or `max_steps` is hit.
//!
//! Tool-call flow (native OpenAI function-calling):
//!   1. Stream a completion.
//!   2. If a chunk signals `finish_reason == "tool_calls"`, the streamed
//!      delta carries no call payload — replay the request non-streaming
//!      to get the full `message.tool_calls` (verified against
//!      deepseek-v4-flash via cc-switch proxy, 2026-08-09).
//!   3. Execute every call, append results as `tool` messages, loop.
//!   4. Otherwise the streamed text is the final answer.

use anyhow::{Context, Result};
use futures_util::StreamExt;

use crate::llm::provider::{ChatComplete, ChatMessage, ChatRequest, LlmProvider};
use crate::tools::ToolRegistry;

#[cfg(test)]
mod tests;

/// Agent loop configuration.
#[derive(Debug, Clone)]
pub struct AgentConfig {
    pub model: String,
    /// 0 = no step limit (a turn runs until the model stops calling tools).
    pub max_steps: u32,
    pub system_prompt: String,
}

/// One turn of the agent loop. Returns the assistant's final text.
pub async fn run_turn(
    provider: &dyn LlmProvider,
    tools: &ToolRegistry,
    config: &AgentConfig,
    user_prompt: &str,
) -> Result<String> {
    let mut messages = vec![ChatMessage::system(&config.system_prompt)];
    messages.push(ChatMessage::user(user_prompt));

    let specs = tools.specs();
    let mut step: u32 = 0;

    loop {
        if config.max_steps > 0 && step >= config.max_steps {
            anyhow::bail!("step limit ({}) reached", config.max_steps);
        }
        step += 1;

        let req = ChatRequest {
            model: config.model.clone(),
            messages: messages.clone(),
            tools: specs.clone(),
            stream: true,
            temperature: None,
        };

        let mut stream = provider
            .chat_stream(req.clone())
            .await
            .context("starting completion")?;

        let mut text = String::new();
        let mut tool_calls_requested = false;
        while let Some(chunk) = stream.next().await {
            let chunk = chunk.context("reading stream")?;
            if !chunk.thinking {
                text.push_str(&chunk.delta);
            }
            tool_calls_requested |= chunk.tool_calls_requested;
            if chunk.done {
                break;
            }
        }

        if tool_calls_requested {
            // Replay non-streaming to get the full tool-call payload.
            let complete: ChatComplete = provider
                .chat_complete(req)
                .await
                .context("replaying completion for tool calls")?;
            if complete.tool_calls.is_empty() {
                // Model asked for tools but sent none; treat text as answer.
                let answer = if text.trim().is_empty() {
                    complete.text
                } else {
                    text
                };
                return Ok(answer.trim().to_string());
            }
            messages.push(ChatMessage::assistant_with_tools(
                &complete.text,
                complete.tool_calls.clone(),
            ));
            for call in complete.tool_calls {
                let result = tools.execute(&call).await?;
                messages.push(ChatMessage::tool_result(result, call.id));
            }
            continue;
        }

        // No tool call — this is the final answer.
        return Ok(text.trim().to_string());
    }
}
