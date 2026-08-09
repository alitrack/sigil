//! MCP-backed tools: each tool discovered on an MCP server is wrapped
//! as a normal Tool, so the agent loop treats it like any other.
//! Naming: `mcp_<server>_<tool>` (hyphens → underscores, safe for the
//! OpenAI tool-call API).

use std::sync::Arc;

use async_trait::async_trait;
use serde_json::Value;

use crate::mcp::{McpClient, McpToolInfo};
use crate::tools::{Tool, ToolOutput};

/// Tool adapter over an MCP tool.
pub struct McpTool {
    pub name: String,
    pub description: String,
    pub info: McpToolInfo,
    pub client: Arc<McpClient>,
}

impl McpTool {
    /// Build an adapter with the sanitized full name.
    pub fn new(server: &str, info: McpToolInfo, client: Arc<McpClient>) -> Self {
        let name = Self::full_name(server, &info.name);
        let description = format!("[MCP {}] {}", server, info.description);
        Self {
            name,
            description,
            info,
            client,
        }
    }

    /// Sanitized full name: mcp_<server>_<tool>.
    pub fn full_name(server: &str, tool: &str) -> String {
        let clean = |s: &str| -> String {
            s.chars()
                .map(|c| {
                    if c.is_ascii_alphanumeric() || c == '_' {
                        c
                    } else {
                        '_'
                    }
                })
                .collect()
        };
        format!("mcp_{}_{}", clean(server), clean(tool))
    }
}

#[async_trait]
impl Tool for McpTool {
    fn name(&self) -> &str {
        &self.name
    }

    fn description(&self) -> &str {
        &self.description
    }

    fn parameters(&self) -> Value {
        self.info.schema.clone()
    }

    async fn run(&self, args: &Value) -> anyhow::Result<ToolOutput> {
        let text = self.client.call_tool(&self.info.name, args.clone()).await?;
        let text = serde_json::from_str::<String>(&text.to_string()).unwrap_or(text.to_string());
        Ok(ToolOutput { text })
    }
}
