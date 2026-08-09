//! The agent loop: build messages → stream completion → parse tool calls →
//! execute tools → repeat until the model is done or `max_steps` is hit.
//!
//! Phase 0: minimal ReAct loop with an OpenAI-compatible provider and a
//! registry of native tools. The loop is UI-agnostic — it returns a final
//! text result, which surfaces (TUI / pipe / ACP) will render.

use anyhow::{Context, Result};
use futures_util::StreamExt;

use crate::llm::provider::{ChatMessage, ChatRequest, LlmProvider, Role, ToolCall};
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
            .chat_stream(req)
            .await
            .context("starting completion")?;

        let mut text = String::new();
        while let Some(chunk) = stream.next().await {
            let chunk = chunk.context("reading stream")?;
            if !chunk.thinking {
                text.push_str(&chunk.delta);
            }
            if chunk.done {
                break;
            }
        }

        // If the model emitted tool calls (captured in `text` as the full
        // assistant content), parse and execute them. Phase 0 supports a
        // simplified convention: the model returns a JSON tool_call block.
        // Full native tool-call delta accumulation lands in Phase 1.
        match extract_tool_call(&text, step) {
            Some(call) => {
                let result = tools.execute(&call).await?;
                messages.push(ChatMessage::assistant(&text));
                messages.push(ChatMessage {
                    role: Role::Tool,
                    content: result,
                });
            }
            None => {
                // No tool call — this is the final answer.
                return Ok(text.trim().to_string());
            }
        }
    }
}

/// Phase 0 minimal extraction: the model emits a tool call as a fenced
/// JSON block like:
///
/// ```json
/// {"tool": "bash", "arguments": {"command": "ls -la"}}
/// ```
pub(crate) fn extract_tool_call(text: &str, step: u32) -> Option<ToolCall> {
    let start = text.rfind("```json")?;
    let rest = &text[start + "```json".len()..];
    let end = rest.find("```")?;
    let body = rest[..end].trim();
    let v: serde_json::Value = serde_json::from_str(body).ok()?;
    let name = v.get("tool")?.as_str()?.to_string();
    let arguments = v
        .get("arguments")
        .cloned()
        .unwrap_or(serde_json::Value::Null);
    Some(ToolCall {
        id: format!("call_{}", step),
        name,
        arguments,
    })
}
