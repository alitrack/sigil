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

/// Numbered index of every memory entry (for `memory_delete`).
pub struct MemoryListTool {
    pub memory_path: PathBuf,
}

#[async_trait]
impl Tool for MemoryListTool {
    fn name(&self) -> &str {
        "memory_list"
    }

    fn description(&self) -> &str {
        "List all long-term memory entries as numbered lines (1-based). Use the numbers with memory_delete to remove stale or wrong entries. Entries are the '-' bullet lines the model maintains."
    }

    fn parameters(&self) -> Value {
        json!({ "type": "object", "properties": {} })
    }

    async fn run(&self, _args: &Value) -> anyhow::Result<ToolOutput> {
        let text = memory::list(&self.memory_path)?;
        Ok(ToolOutput { text })
    }
}

/// Delete memory entries by index (from memory_list) or substring.
pub struct MemoryDeleteTool {
    pub memory_path: PathBuf,
}

#[async_trait]
impl Tool for MemoryDeleteTool {
    fn name(&self) -> &str {
        "memory_delete"
    }

    fn description(&self) -> &str {
        "Delete long-term memory entries: by 'index' (a 1-based number from memory_list) or by 'substring' (case-insensitive, removes every matching line). Use when a remembered fact is stale, wrong, or superseded."
    }

    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "index": { "type": "integer", "description": "1-based entry number from memory_list (mutually exclusive with substring)." },
                "substring": { "type": "string", "description": "Case-insensitive substring; every matching line is removed (mutually exclusive with index)." }
            }
        })
    }

    async fn run(&self, args: &Value) -> anyhow::Result<ToolOutput> {
        let index = args
            .get("index")
            .and_then(Value::as_u64)
            .map(|i| i as usize);
        let substring = args.get("substring").and_then(Value::as_str);
        let text = memory::delete(&self.memory_path, index, substring)?;
        Ok(ToolOutput { text })
    }
}

/// Convenience: register all memory tools on a registry.
pub fn register(registry: &mut crate::tools::ToolRegistry, memory_path: PathBuf) {
    registry.register(Arc::new(RememberTool {
        memory_path: memory_path.clone(),
    }));
    registry.register(Arc::new(RecallTool {
        memory_path: memory_path.clone(),
    }));
    registry.register(Arc::new(MemoryListTool {
        memory_path: memory_path.clone(),
    }));
    registry.register(Arc::new(MemoryDeleteTool { memory_path }));
}
