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

//! ACP (Agent Client Protocol) — server mode (`sigil acp`) plus a
//! client for driving external ACP servers (`acp_call` tool).

pub mod client;

#[cfg(test)]
mod client_tests;

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
    /// A prompt was accepted: the turn executor runs in the background
    /// and `done` yields its outcome. The serve loop watches these
    /// (FuturesUnordered) so the stdio loop stays free to process
    /// other messages — required for mid-turn questions
    /// (`agent/request.input`) which arrive while a turn is running.
    Deferred {
        id: Value,
        session_id: String,
        done: tokio::sync::oneshot::Receiver<Result<String>>,
    },
}

/// How one agent turn runs: prompt → future of the final answer.
pub type TurnFn =
    dyn Fn(String) -> futures_util::future::BoxFuture<'static, Result<String>> + Send + Sync;

/// Session state shared across messages.
#[derive(Default)]
pub struct AcpState {
    sessions: std::collections::HashMap<String, mpsc::UnboundedSender<Value>>,
    /// Mid-turn human-input requests: requestId → responder. The
    /// server-side AskCb registers a oneshot here before emitting an
    /// `agent_request_input` notification; the client answers with
    /// `agent/request.input`, which resolves it. This is the sigil
    /// extension that turns a one-shot prompt into a two-way
    /// conversation (brainstorming UI, requestUserInput-style flows).
    ///
    /// Shared (Arc<Mutex>) so the background turn executor — which
    /// runs outside `handle_message` — can register requests while the
    /// message loop answers them.
    pending_inputs: Arc<
        std::sync::Mutex<std::collections::HashMap<String, tokio::sync::oneshot::Sender<Value>>>,
    >,
    /// How to run one agent turn (injected; real runs use the agent
    /// loop, tests use a fake). Returns a future so the prompt handler
    /// can await the (long-running) agent loop without blocking.
    pub turn: Option<Arc<TurnFn>>,
}

impl AcpState {
    pub fn new() -> Self {
        Self::default()
    }

    /// Register a mid-turn input request and return the receiver the
    /// server-side AskCb awaits. The host answers via
    /// `agent/request.input` (see [`handle_message`]), which resolves
    /// this channel.
    ///
    /// NOTE: the stdio loop is serial today — a blocking await here
    /// while `handle_message` is mid-prompt would deadlock (the
    /// client's answer would queue behind the prompt on stdin). The
    /// turn executor must therefore run concurrently with the message
    /// loop (spawn + select) once mid-turn questions are used.
    #[allow(dead_code)] // used by tests and host integrations
    pub fn register_input(&mut self, request_id: String) -> tokio::sync::oneshot::Receiver<Value> {
        let (tx, rx) = tokio::sync::oneshot::channel();
        self.pending_inputs.lock().unwrap().insert(request_id, tx);
        rx
    }

    /// The shared pending-input table (used by the AskCb built in
    /// [`serve`], which runs outside `handle_message`).
    pub fn pending_inputs(
        &self,
    ) -> Arc<std::sync::Mutex<std::collections::HashMap<String, tokio::sync::oneshot::Sender<Value>>>>
    {
        self.pending_inputs.clone()
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

            let turn = state.turn.clone();
            let (done_tx, done_rx) = tokio::sync::oneshot::channel();
            tokio::spawn(async move {
                let result = match &turn {
                    Some(t) => (t)(prompt_text).await,
                    None => Err(anyhow::anyhow!("no turn executor installed")),
                };
                let _ = done_tx.send(result);
            });
            outs.push(Out::Deferred {
                id: id.unwrap_or(Value::Null),
                session_id,
                done: done_rx,
            });
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
        // sigil extension: the host answers a mid-turn question that
        // the agent raised via an `agent_request_input` notification.
        // `requestId` must match the one from the notification; the
        // `response` payload is handed back to the paused agent loop.
        "agent/request.input" => {
            let request_id = params
                .get("requestId")
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_string();
            let response = params.get("response").cloned().unwrap_or(Value::Null);
            match state.pending_inputs.lock().unwrap().remove(&request_id) {
                Some(tx) => {
                    let _ = tx.send(response);
                    outs.push(Out::Response {
                        id: id.unwrap_or(Value::Null),
                        result: Some(json!({ "answered": true })),
                        error: None,
                    });
                }
                None => outs.push(Out::Response {
                    id: id.unwrap_or(Value::Null),
                    result: None,
                    error: Some(json!({
                        "code": -32602,
                        "message": format!("unknown requestId {request_id}")
                    })),
                }),
            }
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
///
/// The loop is concurrent: prompts spawn a background turn and yield
/// an [`Out::Deferred`], so the stdio loop stays free to answer
/// mid-turn messages (`agent/request.input`) while a turn runs —
/// without this, a turn that asks the host a question would deadlock
/// (its answer would queue on stdin behind the prompt).
pub async fn serve(
    mut tools: ToolRegistry,
    agent_config: AgentConfig,
    provider: std::sync::Arc<dyn crate::llm::provider::LlmProvider>,
) -> Result<()> {
    use futures_util::stream::{FuturesUnordered, StreamExt};

    let stdin = tokio::io::stdin();
    let mut lines = BufReader::new(stdin).lines();
    let mut stdout = tokio::io::stdout();

    let mut state = AcpState::new();

    // Mid-turn question notifications flow from the background turn
    // executor (AskCb) to the stdio loop over this channel.
    let (notif_tx, mut notif_rx) = mpsc::unbounded_channel::<Out>();

    // The AskCb: surface `agent_request_input` to the host and wait
    // for its `agent/request.input` answer.
    {
        let pending_inputs = state.pending_inputs();
        let notif_tx_for_ask = notif_tx.clone();
        let ask_cb: Arc<crate::agent::AskCb> = Arc::new(move |req: crate::agent::AskRequest| {
            let pending_inputs = pending_inputs.clone();
            let notif_tx = notif_tx_for_ask.clone();
            Box::pin(async move {
                let request_id = format!(
                    "in-{}",
                    std::time::SystemTime::now()
                        .duration_since(std::time::UNIX_EPOCH)
                        .map(|d| d.as_nanos())
                        .unwrap_or(0)
                );
                let (tx, rx) = tokio::sync::oneshot::channel();
                pending_inputs
                    .lock()
                    .unwrap()
                    .insert(request_id.clone(), tx);
                notif_tx
                    .send(Out::Notification {
                        session_id: String::new(), // the client matches by requestId
                        update: json!({
                            "sessionUpdate": "agent_request_input",
                            "requestId": request_id,
                            "question": req.question,
                            "options": req.options,
                        }),
                    })
                    .map_err(|_| anyhow::anyhow!("host disconnected while asking"))?;
                let answer = rx
                    .await
                    .map_err(|_| anyhow::anyhow!("host never answered the question"))?;
                Ok(crate::agent::AskAnswer {
                    text: answer
                        .get("text")
                        .and_then(Value::as_str)
                        .unwrap_or("")
                        .to_string(),
                })
            })
        });
        tools.register(Arc::new(crate::tools::ask::AskTool { cb: ask_cb }));
    }

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

    // Background turns awaiting completion: (request id, session id, outcome).
    type PendingTurn = (Value, String, Result<String>);
    let mut pending: FuturesUnordered<
        std::pin::Pin<Box<dyn futures_util::Future<Output = PendingTurn>>>,
    > = FuturesUnordered::new();

    loop {
        tokio::select! {
            line = lines.next_line() => {
                match line {
                    Ok(Some(line)) if !line.trim().is_empty() => {
                        let Ok(msg) = serde_json::from_str::<Value>(&line) else {
                            continue;
                        };
                        let outs = handle_message(&mut state, &msg).await?;
                        for out in outs {
                            match out {
                                Out::Deferred { id, session_id, done } => {
                                    pending.push(Box::pin(async move {
                                        let outcome = done
                                            .await
                                            .unwrap_or_else(|_| Err(anyhow::anyhow!("turn dropped")));
                                        (id, session_id, outcome)
                                    }));
                                }
                                other => write_out(&mut stdout, other).await?,
                            }
                        }
                    }
                    Ok(Some(_)) => continue, // blank line
                    Ok(None) => break,       // EOF
                    Err(e) => return Err(e.into()),
                }
            }
            Some((id, session_id, result)) = pending.next() => {
                write_turn_outcome(&mut stdout, id, session_id, result).await?;
            }
            Some(out) = notif_rx.recv() => {
                // Mid-turn question (or any AskCb-emitted) notification.
                write_out(&mut stdout, out).await?;
            }
        }
    }
    Ok(())
}

/// Write one immediate `Out` (response or notification) to stdout.
async fn write_out(stdout: &mut tokio::io::Stdout, out: Out) -> Result<()> {
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
        Out::Deferred { .. } => {
            anyhow::bail!("internal: deferred must be handled by the serve loop")
        }
    }
    Ok(())
}

/// Write the final outcome of a background turn: streaming chunk +
/// complete notifications, then the prompt response (or an error).
async fn write_turn_outcome(
    stdout: &mut tokio::io::Stdout,
    id: Value,
    session_id: String,
    result: Result<String>,
) -> Result<()> {
    match result {
        Ok(text) => {
            write_out(
                stdout,
                Out::Notification {
                    session_id: session_id.clone(),
                    update: json!({
                        "sessionUpdate": "agent_message_chunk",
                        "content": { "type": "text", "text": text }
                    }),
                },
            )
            .await?;
            write_out(
                stdout,
                Out::Notification {
                    session_id: session_id.clone(),
                    update: json!({
                        "sessionUpdate": "agent_message_complete",
                        "role": "assistant",
                        "content": [{ "type": "text", "text": text }],
                        "stopReason": "end_turn"
                    }),
                },
            )
            .await?;
            write_out(
                stdout,
                Out::Response {
                    id,
                    result: Some(json!({ "stopReason": "end_turn" })),
                    error: None,
                },
            )
            .await?;
        }
        Err(e) => {
            write_out(
                stdout,
                Out::Response {
                    id,
                    result: None,
                    error: Some(json!({
                        "code": -32603,
                        "message": format!("agent error: {e}")
                    })),
                },
            )
            .await?;
        }
    }
    Ok(())
}
