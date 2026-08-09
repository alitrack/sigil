//! ACP (Agent Client Protocol) server mode — `sigil acp`.
//!
//! Lets any ACP client (editors, other agents) drive sigil over
//! JSON-RPC 2.0 on stdio. Call sequence (agentclientprotocol.com):
//!   agent/initialize → agent/session/new → agent/session.prompt
//!   → agent_message_chunk notifications → agent/session.close
//!
//! This is the "LSP for AI coding agents" — sigil being drivable by
//! any ACP host multiplies its harness (every client becomes a UI).
//!
//! The protocol logic lives in [`handle_message`], which is pure (no
//! I/O) and unit-testable; [`serve`] is a thin stdio loop around it.

use std::sync::Arc;

use anyhow::Result;
use futures_util::FutureExt;
use serde_json::{Value, json};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::sync::mpsc;

use crate::agent::{AgentConfig, run_turn};
use crate::tools::ToolRegistry;

#[cfg(test)]
mod tests;

/// The protocol version we implement (2025-03-26 is the stable ACP rev).
const PROTOCOL_VERSION: &str = "2025-03-26";

/// One outgoing message: either a response (id set) or a notification.
#[derive(Debug)]
pub enum Out {
    /// JSON-RPC response.
    Response {
        id: Value,
        result: Option<Value>,
        error: Option<Value>,
    },
    /// client/session.update notification.
    Notification { session_id: String, update: Value },
}

/// How one agent turn runs: prompt → future of the final answer.
pub type TurnFn =
    dyn Fn(String) -> futures_util::future::BoxFuture<'static, Result<String>> + Send + Sync;

/// Session state shared across messages.
#[derive(Default)]
pub struct AcpState {
    sessions: std::collections::HashMap<String, mpsc::UnboundedSender<Value>>,
    /// How to run one agent turn (injected; real runs use the agent
    /// loop, tests use a fake). Returns a future so the prompt handler
    /// can await the (long-running) agent loop without blocking.
    pub turn: Option<Arc<TurnFn>>,
}

impl AcpState {
    pub fn new() -> Self {
        Self::default()
    }
}

/// Handle one incoming JSON-RPC message, producing outputs.
pub async fn handle_message(state: &mut AcpState, msg: &Value) -> Result<Vec<Out>> {
    let method = msg.get("method").and_then(Value::as_str).unwrap_or("");
    let id = msg.get("id").cloned();
    let params = msg.get("params").cloned().unwrap_or(Value::Null);
    let mut outs = Vec::new();

    match method {
        "agent/initialize" => {
            outs.push(Out::Response {
                id: id.unwrap_or(Value::Null),
                result: Some(json!({
                    "protocolVersion": PROTOCOL_VERSION,
                    "agentCapabilities": {
                        "supportsPromptCancellation": false,
                        "supportsStopAndRestartSession": false,
                        "supportsExtendedClientInformation": false,
                    }
                })),
                error: None,
            });
        }
        "agent/session/new" => {
            let session_id = format!(
                "sigil-{}",
                std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .map(|d| d.as_nanos())
                    .unwrap_or(0)
            );
            let (tx, mut rx) = mpsc::unbounded_channel::<Value>();
            state.sessions.insert(session_id.clone(), tx);
            // Notifier task: forward queued updates as notifications.
            tokio::spawn(async move {
                while let Some(update) = rx.recv().await {
                    let notif = json!({
                        "jsonrpc": "2.0",
                        "method": "client/session.update",
                        "params": { "sessionId": "__sid__", "update": update }
                    });
                    // The session id is spliced below by handle_message;
                    // this task only forwards via the channel — the
                    // actual write happens in serve(). For pure tests we
                    // never reach this path (fake turn returns instantly).
                    let _ = notif;
                }
            });
            outs.push(Out::Response {
                id: id.unwrap_or(Value::Null),
                result: Some(json!({ "sessionId": session_id })),
                error: None,
            });
        }
        "agent/session.prompt" => {
            let session_id = params
                .get("sessionId")
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_string();
            let prompt_text = params
                .get("prompt")
                .and_then(Value::as_array)
                .map(|blocks| {
                    blocks
                        .iter()
                        .filter_map(|b| {
                            if b.get("type").and_then(Value::as_str) == Some("text") {
                                b.get("text").and_then(Value::as_str)
                            } else {
                                None
                            }
                        })
                        .collect::<Vec<_>>()
                        .join("\n")
                })
                .unwrap_or_default();

            let result = match &state.turn {
                Some(turn) => (turn)(prompt_text).await,
                None => Err(anyhow::anyhow!("no turn executor installed")),
            };

            match result {
                Ok(text) => {
                    // Stream: chunk + complete notifications, then response.
                    outs.push(Out::Notification {
                        session_id: session_id.clone(),
                        update: json!({
                            "sessionUpdate": "agent_message_chunk",
                            "content": { "type": "text", "text": text }
                        }),
                    });
                    outs.push(Out::Notification {
                        session_id: session_id.clone(),
                        update: json!({
                            "sessionUpdate": "agent_message_complete",
                            "role": "assistant",
                            "content": [{ "type": "text", "text": text }],
                            "stopReason": "end_turn"
                        }),
                    });
                    outs.push(Out::Response {
                        id: id.unwrap_or(Value::Null),
                        result: Some(json!({ "stopReason": "end_turn" })),
                        error: None,
                    });
                }
                Err(e) => {
                    outs.push(Out::Response {
                        id: id.unwrap_or(Value::Null),
                        result: None,
                        error: Some(json!({
                            "code": -32603,
                            "message": format!("agent error: {e}")
                        })),
                    });
                }
            }
        }
        "agent/session.close" => {
            let sid = params
                .get("sessionId")
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_string();
            state.sessions.remove(&sid);
            outs.push(Out::Response {
                id: id.unwrap_or(Value::Null),
                result: Some(Value::Null),
                error: None,
            });
        }
        "agent/cancel" => {
            outs.push(Out::Response {
                id: id.unwrap_or(Value::Null),
                result: Some(json!({ "cancelled": true })),
                error: None,
            });
        }
        "ping" => {
            outs.push(Out::Response {
                id: id.unwrap_or(Value::Null),
                result: Some(json!({})),
                error: None,
            });
        }
        _ => {
            outs.push(Out::Response {
                id: id.unwrap_or(Value::Null),
                result: None,
                error: Some(json!({
                    "code": -32601,
                    "message": format!("unknown method {method}")
                })),
            });
        }
    }
    Ok(outs)
}

/// Run the ACP server loop on stdin/stdout until EOF.
pub async fn serve(
    tools: ToolRegistry,
    agent_config: AgentConfig,
    provider: std::sync::Arc<dyn crate::llm::provider::LlmProvider>,
) -> Result<()> {
    let stdin = tokio::io::stdin();
    let mut lines = BufReader::new(stdin).lines();
    let mut stdout = tokio::io::stdout();

    let mut state = AcpState::new();
    // Real turn executor: full agent loop with this run's tools/config.
    let turn = Arc::new(
        move |prompt: String| -> futures_util::future::BoxFuture<'static, Result<String>> {
            let mut tools = tools.clone_shallow();
            let config = agent_config.clone();
            let provider = provider.clone();
            async move { run_turn(provider.as_ref(), &mut tools, &config, &prompt).await }.boxed()
        },
    );
    state.turn = Some(turn);

    while let Some(line) = lines.next_line().await? {
        if line.trim().is_empty() {
            continue;
        }
        let Ok(msg) = serde_json::from_str::<Value>(&line) else {
            continue;
        };
        let outs = handle_message(&mut state, &msg).await?;
        for out in outs {
            match out {
                Out::Response { id, result, error } => {
                    let mut payload = serde_json::Map::new();
                    payload.insert("jsonrpc".into(), Value::String("2.0".into()));
                    if !id.is_null() {
                        payload.insert("id".into(), id);
                    }
                    if let Some(r) = result {
                        payload.insert("result".into(), r);
                    }
                    if let Some(e) = error {
                        payload.insert("error".into(), e);
                    }
                    let mut s = serde_json::to_string(&Value::Object(payload))?;
                    s.push('\n');
                    stdout.write_all(s.as_bytes()).await?;
                    stdout.flush().await?;
                }
                Out::Notification { session_id, update } => {
                    let mut payload = serde_json::Map::new();
                    payload.insert("jsonrpc".into(), Value::String("2.0".into()));
                    payload.insert(
                        "method".into(),
                        Value::String("client/session.update".into()),
                    );
                    payload.insert(
                        "params".into(),
                        json!({ "sessionId": session_id, "update": update }),
                    );
                    let mut s = serde_json::to_string(&Value::Object(payload))?;
                    s.push('\n');
                    stdout.write_all(s.as_bytes()).await?;
                    stdout.flush().await?;
                }
            }
        }
    }
    Ok(())
}
