//! Configuration: env-var-first, optional TOML config file.
//!
//! Phase 0 supports env vars only (no file yet):
//!   SIGIL_MODEL, SIGIL_BASE_URL, SIGIL_API_KEY
//! The TOML config (`~/.sigil/config.toml`) lands with the provider
//! registry in Phase 1.

use std::env;

use anyhow::Result;

#[derive(Debug, Clone)]
pub struct Config {
    pub model: String,
    pub base_url: String,
    pub api_key: String,
    pub system_prompt: String,
}

impl Config {
    pub fn from_env() -> Result<Self> {
        let model = env::var("SIGIL_MODEL").unwrap_or_else(|_| "deepseek-v4-flash".to_string());
        // Default endpoint: cc-switch local proxy (user's provider manager).
        // Direct vLLM (10.10.10.115:8010) works too via SIGIL_BASE_URL.
        let base_url =
            env::var("SIGIL_BASE_URL").unwrap_or_else(|_| "http://127.0.0.1:15722/v1".to_string());
        let api_key = env::var("SIGIL_API_KEY").unwrap_or_else(|_| "EMPTY".to_string());

        Ok(Self {
            model,
            base_url,
            api_key,
            system_prompt: DEFAULT_SYSTEM_PROMPT.to_string(),
        })
    }

    pub fn with_overrides(
        mut self,
        model: Option<&str>,
        base_url: Option<&str>,
        api_key: Option<&str>,
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
        self
    }
}

pub const DEFAULT_SYSTEM_PROMPT: &str = "\
You are sigil, a coding agent that works inside a terminal.

You solve the user's task by calling tools. To call a tool, reply with a
fenced JSON block:

```json
{\"tool\": \"bash\", \"arguments\": {\"command\": \"ls -la\"}}
```

After the tool result comes back, either call the next tool or give the
final answer in plain text. Keep answers concise. Do not call tools after
you have finished the task.";
