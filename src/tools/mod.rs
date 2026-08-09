//! Tool registry: native tools + (Phase 1) scripted LuaJIT tools + MCP
//! behind one lookup, so the agent loop and the model treat all identically.

use std::collections::HashMap;
use std::sync::Arc;

use async_trait::async_trait;
use serde_json::{Value, json};

use crate::llm::ToolSpec;

pub mod bash;
pub mod files;
pub mod git;

/// What a tool returns to the agent loop.
#[derive(Debug, Clone)]
pub struct ToolOutput {
    pub text: String,
}

/// A callable tool.
#[async_trait]
pub trait Tool: Send + Sync {
    fn name(&self) -> &str;
    fn description(&self) -> &str;
    fn parameters(&self) -> Value;
    async fn run(&self, args: &Value) -> anyhow::Result<ToolOutput>;
}

/// Unified registry, keyed by advertised name.
#[derive(Default)]
pub struct ToolRegistry {
    tools: HashMap<String, Arc<dyn Tool>>,
    order: Vec<String>,
}

impl ToolRegistry {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn register(&mut self, tool: Arc<dyn Tool>) {
        let name = tool.name().to_string();
        self.tools.insert(name.clone(), tool);
        self.order.push(name);
    }

    pub fn with_native_tools() -> Self {
        let mut registry = Self::new();
        registry.register(Arc::new(bash::BashTool));
        registry.register(Arc::new(files::ReadFileTool));
        registry.register(Arc::new(files::WriteFileTool));
        registry.register(Arc::new(git::GitStatusTool));
        registry.register(Arc::new(git::GitDiffTool));
        registry.register(Arc::new(git::GitLogTool));
        registry
    }

    /// Tool specs advertised to the model, in registration order.
    pub fn specs(&self) -> Vec<ToolSpec> {
        self.order
            .iter()
            .filter_map(|name| self.tools.get(name))
            .map(|t| ToolSpec {
                name: t.name().to_string(),
                description: t.description().to_string(),
                parameters: t.parameters(),
            })
            .collect()
    }

    /// Execute a tool call by name. Errors are returned as tool output so
    /// the model sees them (never fatal to the loop).
    pub async fn execute(&self, call: &crate::llm::ToolCall) -> anyhow::Result<String> {
        let tool = self
            .tools
            .get(&call.name)
            .ok_or_else(|| anyhow::anyhow!("unknown tool: {}", call.name))?;
        match tool.run(&call.arguments).await {
            Ok(out) => Ok(out.text),
            Err(e) => Ok(format!("error: {e}")),
        }
    }
}

/// Helper: build a JSON error payload. Phase 1: used by scripted/MCP tools.
#[allow(dead_code)]
pub fn error_output(msg: impl Into<String>) -> ToolOutput {
    ToolOutput {
        text: json!({ "error": msg.into() }).to_string(),
    }
}
