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
/// result. `options` (optional) lets the host render a picker; `kind`
/// shapes the picker (single = pick one, multi = pick many, confirm =
/// yes/no, text = free form). Omitting `kind` means `single`.
pub struct AskTool {
    pub cb: std::sync::Arc<AskCb>,
}

#[async_trait]
impl Tool for AskTool {
    fn name(&self) -> &str {
        "ask"
    }

    fn description(&self) -> &str {
        "Ask the human a question mid-turn and wait for their answer. Use this when you need a decision, preference, or missing information before continuing. Pass options for multiple-choice questions; use kind to shape the answer UI: 'single' (pick one, default), 'multi' (pick many), 'confirm' (yes/no), 'text' (free-form, no options). Ask exactly ONE question at a time."
    }

    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "question": {
                    "type": "string",
                    "description": "The question to ask the human."
                },
                "kind": {
                    "type": "string",
                    "enum": ["single", "multi", "confirm", "text"],
                    "description": "Answer UI shape: single = pick one option (default), multi = pick many options, confirm = yes/no confirmation, text = free-form input (do not pass options)."
                },
                "options": {
                    "type": "array",
                    "items": { "type": "string" },
                    "description": "Options for single/multi questions (omit for confirm/text; the human may still answer freely for single/multi)."
                }
            },
            "required": ["question"]
        })
    }

    async fn run(&self, args: &Value) -> anyhow::Result<ToolOutput> {
        let mut req: AskRequest = serde_json::from_value(args.clone())
            .map_err(|e| anyhow::anyhow!("ask: bad arguments: {e}"))?;
        // Normalize: confirm/text never carry options (mirrors the NPP
        // contract "ask_text question must not include options").
        if matches!(
            req.kind,
            crate::agent::QuestionKind::Confirm | crate::agent::QuestionKind::Text
        ) {
            req.options.clear();
        }
        let answer = (self.cb)(req).await?;
        Ok(ToolOutput { text: answer.text })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agent::QuestionKind;
    use std::sync::Arc;

    fn tool() -> AskTool {
        // A callback that captures nothing and always answers "ok".
        let cb: Arc<AskCb> =
            Arc::new(|_req| Box::pin(async { Ok(crate::agent::AskAnswer { text: "ok".into() }) }));
        AskTool { cb }
    }

    #[test]
    fn kind_defaults_to_single_when_omitted() {
        let req: AskRequest = serde_json::from_value(json!({
            "question": "A 还是 B?",
            "options": ["A", "B"]
        }))
        .unwrap();
        assert_eq!(req.kind, QuestionKind::Single);
        assert_eq!(req.options, vec!["A", "B"]);
    }

    #[tokio::test]
    async fn confirm_clears_options() {
        let t = tool();
        let out = t
            .run(&json!({ "question": "确认继续？", "kind": "confirm", "options": ["多余"] }))
            .await
            .unwrap();
        assert_eq!(out.text, "ok");
    }

    #[tokio::test]
    async fn text_kind_runs_with_no_options() {
        let t = tool();
        let out = t
            .run(&json!({ "question": "补充一点背景？", "kind": "text" }))
            .await
            .unwrap();
        assert_eq!(out.text, "ok");
    }

    #[tokio::test]
    async fn unknown_kind_is_rejected() {
        let t = tool();
        let err = t
            .run(&json!({ "question": "x", "kind": "whack_a_mole" }))
            .await
            .unwrap_err();
        assert!(err.to_string().contains("bad arguments"), "{err}");
    }
}
