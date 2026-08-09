//! Memory tools: `remember` and `recall` — the agent's persistent
//! cross-session store (a single markdown file the model maintains).

use std::path::PathBuf;
use std::sync::Arc;

use async_trait::async_trait;
use serde_json::{Value, json};

use crate::memory;
use crate::tools::{Tool, ToolOutput};

/// Append a fact to the memory file.
pub struct RememberTool {
    pub memory_path: PathBuf,
}

#[async_trait]
impl Tool for RememberTool {
    fn name(&self) -> &str {
        "remember"
    }

    fn description(&self) -> &str {
        "Persist a fact to long-term memory (~/.sigil/memory.md, cross-session). Use when you learn something durable about the user, the project, or a preference that will matter in future sessions. Idempotent: duplicates are skipped."
    }

    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "fact": { "type": "string", "description": "The fact to remember, as a concise sentence." }
            },
            "required": ["fact"]
        })
    }

    async fn run(&self, args: &Value) -> anyhow::Result<ToolOutput> {
        let fact = args
            .get("fact")
            .and_then(Value::as_str)
            .ok_or_else(|| anyhow::anyhow!("remember: missing string 'fact'"))?;
        let text = memory::remember(&self.memory_path, fact)?;
        Ok(ToolOutput { text })
    }
}

/// Show (or grep) the memory file.
pub struct RecallTool {
    pub memory_path: PathBuf,
}

#[async_trait]
impl Tool for RecallTool {
    fn name(&self) -> &str {
        "recall"
    }

    fn description(&self) -> &str {
        "Read long-term memory (~/.sigil/memory.md). Pass 'filter' to grep for a topic. Use at the start of a session or when the task relates to previously remembered facts."
    }

    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "filter": { "type": "string", "description": "Optional case-insensitive substring to filter entries." }
            }
        })
    }

    async fn run(&self, args: &Value) -> anyhow::Result<ToolOutput> {
        let filter = args.get("filter").and_then(Value::as_str);
        let text = memory::recall(&self.memory_path, filter)?;
        Ok(ToolOutput { text })
    }
}

/// Convenience: register both memory tools on a registry.
pub fn register(registry: &mut crate::tools::ToolRegistry, memory_path: PathBuf) {
    registry.register(Arc::new(RememberTool {
        memory_path: memory_path.clone(),
    }));
    registry.register(Arc::new(RecallTool { memory_path }));
}
