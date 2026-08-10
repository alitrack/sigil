//! Configuration: TOML file (default `~/.sigil/config.toml`) with
//! env-var and CLI overrides. Precedence: CLI > env > file > defaults.
//!
//! File schema:
//! ```toml
//! [llm]
//! model = "deepseek-v4-flash"
//! base_url = "http://127.0.0.1:15722/v1"
//! api_key = "sk-..."
//! # Optional: fallback endpoints tried in order when the primary
//! # fails transiently (network error / 5xx / 429). 4xx aborts.
//! fallbacks = [
//!   { base_url = "http://10.0.0.5:38440/v1", api_key = "..." },
//! ]
//!
//! [agent]
//! max_steps = 10
//! system_prompt = "..."   # optional override
//! ```

use std::env;
use std::path::PathBuf;

use anyhow::Result;
use serde::Deserialize;

#[derive(Debug, Clone, Deserialize, Default)]
#[serde(default)]
struct FileConfig {
    llm: LlmConfig,
    agent: AgentFileConfig,
    mcp: McpFileConfig,
}

#[derive(Debug, Clone, Deserialize, Default)]
#[serde(default)]
struct LlmConfig {
    model: Option<String>,
    base_url: Option<String>,
    api_key: Option<String>,
    /// Fallback endpoints (base_url + api_key) tried in order after the
    /// primary on transient failures.
    fallbacks: Vec<FallbackConfig>,
}

/// One fallback endpoint. Mirror of the primary `base_url`/`api_key`.
#[derive(Debug, Clone, Deserialize)]
pub struct FallbackConfig {
    pub base_url: String,
    #[serde(default)]
    pub api_key: String,
}

#[derive(Debug, Clone, Deserialize, Default)]
#[serde(default)]
struct AgentFileConfig {
    max_steps: Option<u32>,
    system_prompt: Option<String>,
}

/// An MCP server to spawn at startup (stdio transport).
#[derive(Debug, Clone, Deserialize)]
pub struct McpServerConfig {
    pub name: String,
    pub command: String,
    #[serde(default)]
    pub args: Vec<String>,
}

#[derive(Debug, Clone, Deserialize, Default)]
#[serde(default)]
struct McpFileConfig {
    servers: Vec<McpServerConfig>,
}

#[derive(Debug, Clone)]
pub struct Config {
    pub model: String,
    pub base_url: String,
    pub api_key: String,
    /// System prompt override — superseded by the harness bundle
    /// (kept for future CLI/config precedence).
    #[allow(dead_code)]
    pub system_prompt: String,
    pub max_steps: u32,
    /// MCP servers to connect at startup.
    pub mcp_servers: Vec<McpServerConfig>,
    /// Fallback LLM endpoints (base_url + api_key), tried in order
    /// after the primary on transient failures.
    pub fallbacks: Vec<FallbackConfig>,
}

impl Config {
    /// Load from file (if present), then apply env, then CLI overrides.
    pub fn load(file: Option<&str>) -> Result<Self> {
        let path = file.map(PathBuf::from).unwrap_or(default_config_path());
        let file_cfg: FileConfig = match std::fs::read_to_string(&path) {
            Ok(text) => toml::from_str(&text).unwrap_or_default(),
            Err(_) => FileConfig::default(),
        };

        let model = env::var("SIGIL_MODEL")
            .ok()
            .or(file_cfg.llm.model)
            .unwrap_or_else(|| "deepseek-v4-flash".to_string());
        let base_url = env::var("SIGIL_BASE_URL")
            .ok()
            .or(file_cfg.llm.base_url)
            .unwrap_or_else(|| "http://127.0.0.1:15722/v1".to_string());
        let api_key = env::var("SIGIL_API_KEY")
            .ok()
            .or(file_cfg.llm.api_key.clone())
            .unwrap_or_else(|| "EMPTY".to_string());
        let system_prompt = env::var("SIGIL_SYSTEM_PROMPT")
            .ok()
            .or(file_cfg.agent.system_prompt)
            .unwrap_or_else(|| DEFAULT_SYSTEM_PROMPT.to_string());
        let max_steps = file_cfg.agent.max_steps.unwrap_or(0);
        let mcp_servers = file_cfg.mcp.servers;
        // Fallback endpoints: env overrides file (comma-separated list
        // of base_urls; api_key shared with the primary unless the file
        // specifies per-fallback keys).
        let fallbacks = if let Ok(env_fbs) = env::var("SIGIL_FALLBACK_BASE_URL") {
            env_fbs
                .split(',')
                .map(|u| FallbackConfig {
                    base_url: u.trim().to_string(),
                    api_key: file_cfg
                        .llm
                        .api_key
                        .clone()
                        .unwrap_or_else(|| "EMPTY".into()),
                })
                .filter(|f| !f.base_url.is_empty())
                .collect()
        } else {
            file_cfg.llm.fallbacks
        };

        Ok(Self {
            model,
            base_url,
            api_key,
            system_prompt,
            max_steps,
            mcp_servers,
            fallbacks,
        })
    }

    pub fn with_cli_overrides(
        mut self,
        model: Option<&str>,
        base_url: Option<&str>,
        api_key: Option<&str>,
        max_steps: u32,
    ) -> Self {
        if let Some(m) = model {
            self.model = m.to_string();
        }
        if let Some(b) = base_url {
            self.base_url = b.to_string();
        }
        if let Some(k) = api_key {
            self.api_key = k.to_string();
        }
        if max_steps > 0 {
            self.max_steps = max_steps;
        }
        self
    }
}

pub fn default_config_path() -> PathBuf {
    dirs::config_dir()
        .unwrap_or_else(|| PathBuf::from("~"))
        .join("sigil")
        .join("config.toml")
}

pub const DEFAULT_SYSTEM_PROMPT: &str = "\
You are sigil, a coding agent that works inside a terminal.

You solve the user's task by calling tools. Prefer the dedicated tools
(read_file, write_file, git_status, ...) over bash where they exist.

After a tool result comes back, either call the next tool or give the
final answer in plain text. Keep answers concise. Do not call tools after
you have finished the task.";
