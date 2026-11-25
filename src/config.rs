//! Configuration: TOML file (default `config_dir()/sigil/config.toml`,
//! i.e. `~/.config/sigil/config.toml` on Linux, `%APPDATA%\sigil\`
//! `config.toml` on Windows) with env-var and CLI overrides.
//! Precedence: CLI > env > file > defaults.
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
//! max_context_messages = 0 # no message-count trigger
//! max_context_tokens = 48000 # estimated input threshold; 0 disables
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
    /// Forwarded as `reasoning_effort` on every request (e.g. `medium`).
    /// Omitted = provider default, which for Qwen3.8 is the `xhigh` tier.
    reasoning_effort: Option<String>,
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
    max_context_messages: Option<usize>,
    max_context_tokens: Option<usize>,
    planning_interval: Option<u32>,
    system_prompt: Option<String>,
}

/// An MCP server to connect at startup.
///
/// Two transports:
/// - stdio: `command` + `args` spawn a subprocess (default)
/// - streamable HTTP: `url` (optional `bearer_token`) — when `url` is set,
///   `command`/`args` are ignored
#[derive(Debug, Clone, Deserialize)]
pub struct McpServerConfig {
    pub name: String,
    /// Streamable HTTP endpoint (e.g. http://localhost:4214/_mcp).
    /// When set, transport is HTTP; otherwise stdio via `command`.
    #[serde(default)]
    pub url: Option<String>,
    /// Bearer token for the HTTP transport (sent as Authorization header).
    #[serde(default)]
    pub bearer_token: Option<String>,
    #[serde(default)]
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
    /// `reasoning_effort` sent with every request; `None` = provider default.
    pub reasoning_effort: Option<String>,
    /// System prompt override — superseded by the harness bundle
    /// (kept for future CLI/config precedence).
    #[allow(dead_code)]
    pub system_prompt: String,
    pub max_steps: u32,
    pub max_context_messages: usize,
    pub max_context_tokens: usize,
    /// Ask the model to restate its plan every N agent-loop steps
    /// (0 = off). Keeps long multi-tool turns on track instead of drifting;
    /// a checkpoint is injected, then removed on the next step so history
    /// never accumulates stale ones.
    pub planning_interval: u32,
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
            Ok(text) => toml::from_str(&text).unwrap_or_else(|e| {
                // A malformed config must not silently become defaults —
                // silently losing the user's model/endpoint settings is
                // worse than noise.
                eprintln!(
                    "warning: ignoring malformed config file {}: {e} \
                     (using built-in defaults; fix or delete the file)",
                    path.display()
                );
                FileConfig::default()
            }),
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
        let reasoning_effort = env::var("SIGIL_REASONING_EFFORT")
            .ok()
            .filter(|s| !s.trim().is_empty())
            .or_else(|| file_cfg.llm.reasoning_effort.clone())
            .map(|s| s.trim().to_string());
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
            reasoning_effort,
            system_prompt,
            max_steps,
            max_context_messages: file_cfg.agent.max_context_messages.unwrap_or(0),
            max_context_tokens: file_cfg.agent.max_context_tokens.unwrap_or(48_000),
            planning_interval: file_cfg.agent.planning_interval.unwrap_or(0),
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

#[cfg(test)]
mod context_tests {
    use super::*;
    #[test]
    fn context_policy_defaults_to_tokens_and_accepts_explicit_count_limit() {
        let path = std::env::temp_dir().join(format!(
            "sigil-context-config-{}.toml",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::write(&path, "[agent]\n").unwrap();
        let config = Config::load(path.to_str()).unwrap();
        assert_eq!(config.max_context_messages, 0);
        assert_eq!(config.max_context_tokens, 48_000);
        assert_eq!(
            config.planning_interval, 0,
            "planning checkpoints are opt-in"
        );
        std::fs::write(
            &path,
            "[agent]\nmax_context_messages = 80\nmax_context_tokens = 24000\nplanning_interval = 3\n",
        )
        .unwrap();
        let config = Config::load(path.to_str()).unwrap();
        assert_eq!(config.max_context_messages, 80);
        assert_eq!(config.max_context_tokens, 24_000);
        assert_eq!(config.planning_interval, 3);
        std::fs::remove_file(path).unwrap();
    }
}
