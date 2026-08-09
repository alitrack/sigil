//! Sub-agent delegation: the `delegate` tool lets the main agent spin
//! off an independent task. The sub-agent runs its own agent loop
//! (own conversation, shallow copy of the tool registry) and returns
//! its final answer. Isolation: sub-agent turns never enter the main
//! conversation — only the final text does.
//!
//! This is wizard-style sub-agents: parallel exploration without
//! polluting the primary context.

use std::sync::Arc;

use anyhow::{Context, Result};
use async_trait::async_trait;
use serde_json::{Value, json};

use crate::agent::{AgentConfig, run_turn};
use crate::llm::provider::LlmProvider;
use crate::tools::{Tool, ToolRegistry};

/// Tool that delegates a sub-task to a fresh agent turn.
pub struct DelegateTool {
    provider: Arc<dyn LlmProvider>,
    registry: ToolRegistry,
    config: AgentConfig,
}

impl DelegateTool {
    pub fn new(
        provider: Arc<dyn LlmProvider>,
        registry: ToolRegistry,
        config: AgentConfig,
    ) -> Self {
        Self {
            provider,
            registry,
            config,
        }
    }
}

#[async_trait]
impl Tool for DelegateTool {
    fn name(&self) -> &'static str {
        "delegate"
    }

    fn description(&self) -> &str {
        "Delegate a self-contained sub-task to a fresh sub-agent. \
         The sub-agent runs its own tool loop (read_file, bash, etc.) \
         and returns a summary. Use for parallel exploration, research, \
         or long independent subtasks; the result is a text summary, not \
         the full transcript."
    }

    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "task": {
                    "type": "string",
                    "description": "The self-contained task for the sub-agent"
                }
            },
            "required": ["task"]
        })
    }

    async fn run(&self, args: &Value) -> Result<crate::tools::ToolOutput> {
        let task = args
            .get("task")
            .and_then(Value::as_str)
            .context("delegate requires a 'task' string")?;

        let mut sub_tools = self.registry.clone_shallow();
        // Run the sub-agent's own turn (no approver: sub-agents are
        // non-interactive, so the gate's default rejection stands).
        let answer = run_turn(self.provider.as_ref(), &mut sub_tools, &self.config, task)
            .await
            .context("sub-agent turn failed")?;
        Ok(crate::tools::ToolOutput { text: answer })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn delegate_schema_is_valid() {
        let tool = DelegateTool {
            provider: Arc::new(
                crate::llm::openai::OpenAiProvider::new(crate::llm::openai::OpenAiConfig {
                    base_url: "http://localhost:1/v1".into(),
                    api_key: "x".into(),
                })
                .unwrap(),
            ),
            registry: ToolRegistry::default(),
            config: AgentConfig {
                model: "test".into(),
                max_steps: 0,
                prompt_state: std::sync::Arc::new(std::sync::RwLock::new(
                    crate::harness::PromptState::default(),
                )),
                max_inline_skills: 0,
            },
        };
        assert_eq!(tool.name(), "delegate");
        let params = tool.parameters();
        assert!(params["properties"]["task"].is_object());
        assert!(params["required"][0] == "task");
    }
}
