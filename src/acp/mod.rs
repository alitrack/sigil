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

use crate::agent::{AgentConfig, run_session_turn};
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
        done: tokio::sync::oneshot::Receiver<
            Result<(String, Vec<crate::llm::provider::ChatMessage>)>,
        >,
    },
}

/// How one agent turn runs: (session id, prompt, prior transcript) →
/// future of (final answer, updated transcript). The transcript is
/// what makes a session multi-turn: each `agent/session.prompt` feeds
/// the stored transcript back in, and `agent/session/fork` clones it
/// (NPP: each answer branch is a forked Codex thread with its own
/// context).
pub type TurnFn =
    dyn Fn(
        String,
        String,
        Vec<crate::llm::provider::ChatMessage>,
    ) -> futures_util::future::BoxFuture<
        'static,
        Result<(String, Vec<crate::llm::provider::ChatMessage>)>,
    > + Send + Sync;

/// Shared transcript store: session id → accumulated transcript.
///
/// Separate from `AcpState.sessions` (which `handle_message` mutates
/// exclusively) because the BACKGROUND turn executor runs outside
/// `handle_message` and must write the transcript back when a turn
/// finishes — same Arc<Mutex> pattern as `pending_inputs`. This store
/// is authoritative for transcripts; `AcpState.sessions` keeps the
/// per-session cwd. `agent/session/fork` clones from it.
pub type TranscriptStore = Arc<
    std::sync::Mutex<std::collections::HashMap<String, Vec<crate::llm::provider::ChatMessage>>>,
>;

/// Per-session runtime state: the working directory (from
/// `agent/session/new`). The transcript lives in the shared
/// [`TranscriptStore`] (background turns write it back). Forged by
/// `agent/session/new` and cloned (with a fresh id) by
/// `agent/session/fork` — the branch keeps the parent's full context,
/// exactly like NPP's `ForkThreadAsync`.
#[derive(Default, Clone)]
struct SessionState {
    cwd: String,
}

/// Session state shared across messages.
#[derive(Default)]
pub struct AcpState {
    sessions: std::collections::HashMap<String, SessionState>,
    /// The session most recently created, forked, or prompted. In the
    /// serial client protocol (one in-flight prompt per client) this
    /// is the session a mid-turn `ask` notification should be
    /// attributed to — the background turn executor reads it.
    /// Shared (Arc<Mutex>) like `pending_inputs`: the background
    /// executor and callbacks run outside `handle_message`.
    current_session: Arc<std::sync::Mutex<String>>,
    /// session id → accumulated transcript (see [`TranscriptStore`]).
    transcripts: TranscriptStore,
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
        let mut s = Self::default();
        s.transcripts = Arc::new(std::sync::Mutex::new(std::collections::HashMap::new()));
        s.current_session = Arc::new(std::sync::Mutex::new(String::new()));
        s
    }

    /// The shared transcript store (background turns write transcripts
    /// back into it after completion).
    pub fn transcripts(&self) -> TranscriptStore {
        self.transcripts.clone()
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
            let cwd = params
                .get("cwd")
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_string();
            state
                .sessions
                .insert(session_id.clone(), SessionState { cwd });
            state
                .transcripts
                .lock()
                .unwrap()
                .insert(session_id.clone(), Vec::new());
            *state.current_session.lock().unwrap() = session_id.clone();
            outs.push(Out::Response {
                id: id.unwrap_or(Value::Null),
                result: Some(json!({ "sessionId": session_id })),
                error: None,
            });
        }
        // sigil extension (NPP parity): fork a session's context into a
        // new branch session. The branch starts with the parent's full
        // transcript, so its agent remembers everything the parent
        // figured out (NPP: `ForkThreadAsync` — each answer branch is a
        // forked Codex thread). `agent/session/new` = NPP's
        // `CreateThreadAsync` (empty context).
        "agent/session/fork" => {
            let from = params
                .get("fromSessionId")
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_string();
            match state.sessions.get(&from).cloned() {
                Some(parent) => {
                    let session_id = format!(
                        "sigil-fork-{}",
                        std::time::SystemTime::now()
                            .duration_since(std::time::UNIX_EPOCH)
                            .map(|d| d.as_nanos())
                            .unwrap_or(0)
                    );
                    state.sessions.insert(
                        session_id.clone(),
                        SessionState {
                            cwd: parent.cwd.clone(),
                        },
                    );
                    // Clone the parent's accumulated transcript: the
                    // branch's agent starts where the parent left off.
                    let parent_transcript = state
                        .transcripts
                        .lock()
                        .unwrap()
                        .get(&from)
                        .cloned()
                        .unwrap_or_default();
                    state
                        .transcripts
                        .lock()
                        .unwrap()
                        .insert(session_id.clone(), parent_transcript);
                    *state.current_session.lock().unwrap() = session_id.clone();
                    outs.push(Out::Response {
                        id: id.unwrap_or(Value::Null),
                        result: Some(json!({ "sessionId": session_id })),
                        error: None,
                    });
                }
                None => outs.push(Out::Response {
                    id: id.unwrap_or(Value::Null),
                    result: None,
                    error: Some(json!({
                        "code": -32602,
                        "message": format!("cannot fork unknown session {from}")
                    })),
                }),
            }
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
            let transcript = state
                .transcripts
                .lock()
                .unwrap()
                .get(&session_id)
                .cloned()
                .unwrap_or_default();
            *state.current_session.lock().unwrap() = session_id.clone();
            let (done_tx, done_rx) = tokio::sync::oneshot::channel();
            let session_id_for_turn = session_id.clone();
            tokio::spawn(async move {
                let result = match &turn {
                    Some(t) => (t)(session_id_for_turn, prompt_text, transcript).await,
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
            state.transcripts.lock().unwrap().remove(&sid);
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
    delegates_dir: Option<std::path::PathBuf>,
    harness_dir: Option<std::path::PathBuf>,
) -> Result<()> {
    use futures_util::stream::{FuturesUnordered, StreamExt};

    let stdin = tokio::io::stdin();
    let mut lines = BufReader::new(stdin).lines();
    let mut stdout = tokio::io::stdout();

    let mut state = AcpState::new();
    let current_session = state.current_session.clone();

    // Mid-turn question notifications flow from the background turn
    // executor (AskCb) to the stdio loop over this channel.
    let (notif_tx, mut notif_rx) = mpsc::unbounded_channel::<Out>();

    // The AskCb: surface `agent_request_input` to the host and wait
    // for its `agent/request.input` answer. The notification is
    // attributed to the session the prompt handler marked current
    // (serial client protocol: one in-flight prompt).
    {
        let pending_inputs = state.pending_inputs();
        let notif_tx_for_ask = notif_tx.clone();
        let current_for_ask = current_session.clone();
        let ask_cb: Arc<crate::agent::AskCb> = Arc::new(move |req: crate::agent::AskRequest| {
            let pending_inputs = pending_inputs.clone();
            let notif_tx = notif_tx_for_ask.clone();
            let current = current_for_ask.clone();
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
                let session_id = current.lock().unwrap().clone();
                notif_tx
                    .send(Out::Notification {
                        session_id,
                        update: json!({
                            "sessionUpdate": "agent_request_input",
                            "requestId": request_id,
                            "question": req.question,
                            "options": req.options,
                            "kind": req.kind,
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

    // Live sub-agent progress → host canvas. A DelegateTool wired to a
    // progress reporter shadows the default registration (the registry
    // dedupes by name, last registration wins), so a delegate batch
    // running inside a turn streams `agent_delegate_progress`
    // notifications — the host can show branches starting/finishing.
    {
        let progress_tx = notif_tx.clone();
        let current_for_delegate = current_session.clone();
        let provider2 = provider.clone();
        let config2 = agent_config.clone();
        let progress_cb: std::sync::Arc<
            dyn Fn(crate::tools::delegate::DelegateProgress) + Send + Sync,
        > = std::sync::Arc::new(move |p: crate::tools::delegate::DelegateProgress| {
            let session_id = current_for_delegate.lock().unwrap().clone();
            let _ = progress_tx.send(Out::Notification {
                session_id,
                update: json!({
                    "sessionUpdate": "agent_delegate_progress",
                    "id": p.id,
                    "name": p.name,
                    "status": p.status,
                    "step": p.step,
                    "total": p.total,
                }),
            });
        });
        tools.register(std::sync::Arc::new(
            crate::tools::delegate::DelegateTool::new(
                provider2,
                tools.clone_shallow(),
                config2,
                delegates_dir,
                harness_dir,
            )
            .with_progress(progress_cb),
        ));
    }

    // Real turn executor: one session-scoped agent turn. The prior
    // transcript (fed by the prompt handler) makes each session
    // multi-turn; the result's updated transcript is written back to
    // the shared store so the NEXT turn sees this one. A failed turn
    // keeps the prior transcript (no partial corruption).
    let transcripts_shared = state.transcripts();
    let turn = Arc::new(
        move |session_id: String,
              prompt: String,
              history: Vec<crate::llm::provider::ChatMessage>|
              -> futures_util::future::BoxFuture<
                'static,
                Result<(String, Vec<crate::llm::provider::ChatMessage>)>,
              > {
            let mut tools = tools.clone_shallow();
            let config = agent_config.clone();
            let provider = provider.clone();
            let transcripts = transcripts_shared.clone();
            async move {
                let res =
                    run_session_turn(provider.as_ref(), &mut tools, &config, &history, &prompt)
                        .await;
                match res {
                    Ok(r) => {
                        transcripts
                            .lock()
                            .unwrap()
                            .insert(session_id.clone(), r.messages.clone());
                        Ok((r.output, r.messages))
                    }
                    Err(e) => Err(e),
                }
            }
            .boxed()
        },
    );
    state.turn = Some(turn);

    // Background turns awaiting completion: (request id, session id,
    // (answer, updated transcript) — the transcript was already written
    // back by the turn executor; this copy only feeds the wire).
    type PendingTurn = (
        Value,
        String,
        Result<(String, Vec<crate::llm::provider::ChatMessage>)>,
    );
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
    result: Result<(String, Vec<crate::llm::provider::ChatMessage>)>,
) -> Result<()> {
    match result {
        Ok((text, _transcript)) => {
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
