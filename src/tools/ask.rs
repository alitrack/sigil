//! The `ask` tool — ask the human a question mid-turn and wait for
//! the answer (brainstorming / requestUserInput-style flows).
//!
//! Registered only by hosts that can actually answer (ACP serve mode);
//! other modes do not expose the tool, so models never see it there.

use async_trait::async_trait;
use serde_json::{Value, json};

use super::{Tool, ToolOutput};
use crate::agent::{AskCb, AskRequest};

/// Ask a question; the host (e.g. the ACP client driving sigil)
/// surfaces it to the human and returns their answer as the tool
/// result. `options` (optional) lets the host render a picker.
pub struct AskTool {
    pub cb: std::sync::Arc<AskCb>,
}

#[async_trait]
impl Tool for AskTool {
    fn name(&self) -> &str {
        "ask"
    }

    fn description(&self) -> &str {
        "Ask the human a question mid-turn and wait for their answer. Use this when you need a decision, preference, or missing information before continuing. Pass optional options for multiple-choice questions."
    }

    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "question": {
                    "type": "string",
                    "description": "The question to ask the human."
                },
                "options": {
                    "type": "array",
                    "items": { "type": "string" },
                    "description": "Optional multiple-choice options (the human may still answer freely)."
                }
            },
            "required": ["question"]
        })
    }

    async fn run(&self, args: &Value) -> anyhow::Result<ToolOutput> {
        let req: AskRequest = serde_json::from_value(args.clone())
            .map_err(|e| anyhow::anyhow!("ask: bad arguments: {e}"))?;
        let answer = (self.cb)(req).await?;
        Ok(ToolOutput { text: answer.text })
    }
}
