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
use crate::safety;
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
    tools: &mut ToolRegistry,
    config: &AgentConfig,
    user_prompt: &str,
) -> Result<String> {
    let mut messages = vec![ChatMessage::system(&config.system_prompt)];
    messages.push(ChatMessage::user(user_prompt));

    let breaker = safety::CircuitBreaker::new();
    let mut step: u32 = 0;

    loop {
        // Fresh specs every turn: evolution tools may have added tools
        // mid-run, and the model should see them on the next turn.
        let specs = tools.specs();

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
                // Approval gate: reject destructive commands in
                // non-interactive mode before they ever run.
                match safety::check(&call.name, &call.arguments) {
                    safety::GateDecision::Allow => {
                        // Circuit breaker: skip tripped tools entirely.
                        if breaker.is_tripped(&call.name) {
                            let msg = safety::CircuitBreaker::tripped_message(&call.name);
                            messages.push(ChatMessage::tool_result(msg, call.id));
                            continue;
                        }
                        let result = tools.execute(&call).await?;
                        // Execution ring: flag harness mutations to the
                        // model so it knows the toolset changed.
                        let result = match safety::ring_of(&call.name) {
                            safety::Ring::Harness => {
                                format!("[harness mutation] {result}")
                            }
                            _ => result,
                        };
                        // Circuit breaker: failures trip, successes reset.
                        if result.starts_with("error:") {
                            if breaker.record_failure(&call.name) {
                                eprintln!("circuit breaker tripped: {}", call.name);
                            }
                        } else {
                            breaker.record_success(&call.name);
                        }
                        messages.push(ChatMessage::tool_result(result, call.id));
                    }
                    safety::GateDecision::Reject => {
                        let msg = safety::reject_message(&call.name, &call.arguments);
                        messages.push(ChatMessage::tool_result(msg, call.id));
                    }
                }
            }
            // Evolution: hot-register any tools the model created this
            // turn so they are callable (and advertised) next turn.
            tools.drain_hot();
            continue;
        }

        // No tool call — this is the final answer.
        return Ok(text.trim().to_string());
    }
}
