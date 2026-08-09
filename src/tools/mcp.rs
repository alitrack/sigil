//! MCP-backed tools: each tool discovered on an MCP server is wrapped
//! as a normal Tool, so the agent loop treats it like any other.
//! Naming: `mcp_<server>_<tool>` (hyphens → underscores, safe for the
//! OpenAI tool-call API).

use std::sync::Arc;

use async_trait::async_trait;
use serde_json::{Value, json};

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

/// Tool that reads resources from connected MCP servers.
///
/// `list` returns every resource URI across all connected servers;
/// `read` fetches one resource's content by full URI.
/// Naming: `mcp_resources` (singular tool, no per-server registration).
pub struct McpResourcesTool {
    /// (server_name, client) pairs for every connected MCP server.
    pub servers: Vec<(String, Arc<McpClient>)>,
}

#[async_trait]
impl Tool for McpResourcesTool {
    fn name(&self) -> &str {
        "mcp_resources"
    }

    fn description(&self) -> &str {
        "Read resources exposed by connected MCP servers. Actions: 'list' (show every resource URI across all connected servers), 'read' (fetch one resource's content by full URI, e.g. sigil://skills/math). Use to pull knowledge/context from external MCP servers."
    }

    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "action": {
                    "type": "string",
                    "enum": ["list", "read"],
                    "description": "list: show all resource URIs; read: fetch one resource's content"
                },
                "uri": {
                    "type": "string",
                    "description": "Full resource URI to read (required for action=read)"
                }
            },
            "required": ["action"]
        })
    }

    async fn run(&self, args: &Value) -> anyhow::Result<ToolOutput> {
        let action = args.get("action").and_then(Value::as_str).unwrap_or("list");

        match action {
            "list" => {
                let mut lines: Vec<String> = Vec::new();
                for (server, client) in &self.servers {
                    match client.list_resources().await {
                        Ok(resources) => {
                            if resources.is_empty() {
                                lines.push(format!("[{server}]: (no resources)"));
                            } else {
                                for r in &resources {
                                    let uri = r.get("uri").and_then(Value::as_str).unwrap_or("?");
                                    let name = r.get("name").and_then(Value::as_str).unwrap_or("");
                                    lines.push(format!("[{server}] {uri} — {name}"));
                                }
                            }
                        }
                        Err(e) => lines.push(format!("[{server}]: error: {e:#}")),
                    }
                }
                if lines.is_empty() {
                    lines.push("no MCP servers connected".to_string());
                }
                Ok(ToolOutput {
                    text: lines.join("\n"),
                })
            }
            "read" => {
                let uri = args
                    .get("uri")
                    .and_then(Value::as_str)
                    .filter(|u| !u.is_empty())
                    .ok_or_else(|| anyhow::anyhow!("mcp_resources read requires 'uri'"))?;
                for (_server, client) in &self.servers {
                    match client.read_resource(uri).await {
                        Ok(text) => {
                            return Ok(ToolOutput { text });
                        }
                        Err(e) => {
                            // Try the next server; only fail if none has it.
                            let _ = e;
                        }
                    }
                }
                anyhow::bail!("resource '{uri}' not found on any connected MCP server")
            }
            other => anyhow::bail!("unknown action: {other}"),
        }
    }
}

/// Tool that fetches prompt templates from connected MCP servers.
///
/// `list` shows every prompt name across all connected servers;
/// `get` renders one prompt template's messages by name.
pub struct McpPromptsTool {
    /// (server_name, client) pairs for every connected MCP server.
    pub servers: Vec<(String, Arc<McpClient>)>,
}

#[async_trait]
impl Tool for McpPromptsTool {
    fn name(&self) -> &str {
        "mcp_prompts"
    }

    fn description(&self) -> &str {
        "Fetch prompt templates from connected MCP servers. Actions: 'list' (show every prompt name across all connected servers), 'get' (render one prompt template by name, e.g. math). Use to retrieve ready-made instruction templates from external MCP servers."
    }

    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "action": {
                    "type": "string",
                    "enum": ["list", "get"],
                    "description": "list: show all prompt names; get: render one prompt by name"
                },
                "name": {
                    "type": "string",
                    "description": "Prompt name to render (required for action=get)"
                }
            },
            "required": ["action"]
        })
    }

    async fn run(&self, args: &Value) -> anyhow::Result<ToolOutput> {
        let action = args.get("action").and_then(Value::as_str).unwrap_or("list");

        match action {
            "list" => {
                let mut lines: Vec<String> = Vec::new();
                for (server, client) in &self.servers {
                    match client.list_prompts().await {
                        Ok(prompts) => {
                            if prompts.is_empty() {
                                lines.push(format!("[{server}]: (no prompts)"));
                            } else {
                                for p in &prompts {
                                    let name = p.get("name").and_then(Value::as_str).unwrap_or("?");
                                    let desc =
                                        p.get("description").and_then(Value::as_str).unwrap_or("");
                                    lines.push(format!("[{server}] {name} — {desc}"));
                                }
                            }
                        }
                        Err(e) => lines.push(format!("[{server}]: error: {e:#}")),
                    }
                }
                if lines.is_empty() {
                    lines.push("no MCP servers connected".to_string());
                }
                Ok(ToolOutput {
                    text: lines.join("\n"),
                })
            }
            "get" => {
                let name = args
                    .get("name")
                    .and_then(Value::as_str)
                    .filter(|n| !n.is_empty())
                    .ok_or_else(|| anyhow::anyhow!("mcp_prompts get requires 'name'"))?;
                for (_server, client) in &self.servers {
                    match client.get_prompt(name).await {
                        Ok(text) => {
                            return Ok(ToolOutput { text });
                        }
                        Err(_) => {
                            // Try the next server; only fail if none has it.
                        }
                    }
                }
                anyhow::bail!("prompt '{name}' not found on any connected MCP server")
            }
            other => anyhow::bail!("unknown action: {other}"),
        }
    }
}
