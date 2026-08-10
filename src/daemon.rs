//! Background daemon: `sigil daemon` serves the agent loop over a Unix
//! socket. Clients (`sigil attach`, `sigil send`) connect, send user
//! messages and receive streamed deltas.
//!
//! The daemon owns the agent loop — a client can disconnect mid-run
//! and the task keeps going (herdr's "the runtime your coding agents
//! live on", minus the pane multiplexing). CLI and socket speak the
//! same surface: one JSON line per message.
//!
//! Protocol (JSON lines, one object per line):
//!   client → daemon: {"type":"user","text":"..."} | {"type":"quit"}
//!   daemon → client: {"type":"delta","text":"..."}  (streamed)
//!                    {"type":"done","text":"..."}   (final answer)
//!                    {"type":"error","text":"..."}

use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use anyhow::{Context, Result};
use serde_json::{Value, json};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::{UnixListener, UnixStream};

use crate::agent::{AgentConfig, run_messages};
use crate::llm::provider::{ChatMessage, LlmProvider};
use crate::session::Turn;
use crate::tools::ToolRegistry;

/// Default socket path: `~/.sigil/daemon.sock`.
pub fn default_socket_path() -> PathBuf {
    crate::session::default_sessions_dir()
        .parent()
        .unwrap_or_else(|| Path::new("."))
        .join("daemon.sock")
}

/// Shared daemon state: the running conversation plus a lock that
/// serializes agent loops (one turn at a time, FIFO across clients).
struct DaemonState {
    messages: tokio::sync::Mutex<Vec<ChatMessage>>,
    loop_lock: tokio::sync::Mutex<()>,
    session_id: String,
}

/// Serve the agent loop on `socket_path` until killed.
pub async fn serve(
    socket_path: &Path,
    provider: Arc<dyn LlmProvider>,
    tools: &mut ToolRegistry,
    config: &AgentConfig,
    sessions_dir: &Path,
    resume: Option<&str>,
) -> Result<()> {
    // Session: resume an existing one or start fresh.
    let session_id = match resume {
        Some(id) if !id.is_empty() => {
            // Validate the session loads before serving.
            crate::session::load(sessions_dir, id)
                .with_context(|| format!("loading session '{id}'"))?;
            id.to_string()
        }
        _ => crate::session::start(sessions_dir)?,
    };
    let state = Arc::new(DaemonState {
        messages: tokio::sync::Mutex::new(Vec::new()),
        loop_lock: tokio::sync::Mutex::new(()),
        session_id,
    });
    // Load the resumed history into the shared state.
    if resume.is_some() {
        let turns = crate::session::load(sessions_dir, &state.session_id)?;
        let mut messages = state.messages.lock().await;
        messages.extend(turns_to_messages(turns));
    }

    if let Some(parent) = socket_path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    // Remove a stale socket left by a crashed daemon.
    let _ = std::fs::remove_file(socket_path);
    let listener = UnixListener::bind(socket_path)
        .with_context(|| format!("binding daemon socket {}", socket_path.display()))?;
    eprintln!(
        "sigil daemon: listening on {} (session {})",
        socket_path.display(),
        state.session_id
    );

    loop {
        let (stream, _) = listener.accept().await?;
        let (provider, config, tools, state, sessions_dir) = (
            provider.clone(),
            config.clone(),
            tools.clone_shallow(),
            state.clone(),
            sessions_dir.to_path_buf(),
        );
        tokio::spawn(async move {
            if let Err(e) =
                handle_connection(stream, provider, tools, &config, &state, &sessions_dir).await
            {
                eprintln!("sigil daemon: connection error: {e:#}");
            }
        });
    }
}

/// One client connection: read JSON-line messages, run agent turns,
/// stream deltas back.
async fn handle_connection(
    stream: UnixStream,
    provider: Arc<dyn LlmProvider>,
    mut tools: ToolRegistry,
    config: &AgentConfig,
    state: &DaemonState,
    sessions_dir: &Path,
) -> Result<()> {
    let (reader, writer) = stream.into_split();
    let mut lines = BufReader::new(reader).lines();

    // Stream deltas to the socket via a std channel (the agent loop's
    // stream callback is synchronous; a writer task drains it).
    let (tx, rx) = std::sync::mpsc::channel::<String>();
    let writer_task = tokio::spawn(async move {
        let mut writer = writer;
        while let Ok(line) = rx.recv() {
            if writer.write_all(line.as_bytes()).await.is_err() {
                break;
            }
            let _ = writer.flush().await;
        }
    });

    while let Some(line) = lines.next_line().await? {
        let v: Value =
            serde_json::from_str(&line).unwrap_or(json!({"type": "error", "text": "bad json"}));
        match v["type"].as_str() {
            Some("quit") => break,
            Some("user") => {
                let text = v["text"].as_str().unwrap_or("").to_string();
                if text.trim().is_empty() {
                    continue;
                }
                let _guard = state.loop_lock.lock().await;
                // Append the user turn (session + in-memory history).
                crate::session::append(
                    sessions_dir,
                    &state.session_id,
                    &Turn {
                        role: "user".into(),
                        text: text.clone(),
                        ts: None,
                    },
                )?;
                {
                    let mut messages = state.messages.lock().await;
                    messages.push(ChatMessage::user(text.clone()));
                }

                // Run the agent loop over the full conversation:
                // system prompt (with skills inlined for this user
                // message) + the accumulated history + the new turn.
                // NOTE: unlike the REPL (which forgets to send the
                // system prompt), the daemon always sends it.
                let mut run_msgs = vec![ChatMessage::system(config.system_prompt_for(&text))];
                {
                    let messages = state.messages.lock().await;
                    run_msgs.extend(messages.clone());
                }

                // Stream deltas to this client via the channel.
                let tx2 = tx.clone();
                let stream_cb = move |delta: Option<&str>| {
                    if let Some(d) = delta {
                        let _ = tx2.send(format!("{}\n", json!({"type": "delta", "text": d})));
                    }
                };
                let answer = run_messages(
                    provider.as_ref(),
                    &mut tools,
                    config,
                    &run_msgs,
                    None,
                    Some(&stream_cb),
                )
                .await;

                match answer {
                    Ok(answer) => {
                        {
                            let mut messages = state.messages.lock().await;
                            messages.push(ChatMessage::assistant(answer.clone()));
                        }
                        crate::session::append(
                            sessions_dir,
                            &state.session_id,
                            &Turn {
                                role: "assistant".into(),
                                text: answer.clone(),
                                ts: None,
                            },
                        )?;
                        let _ = tx.send(format!("{}\n", json!({"type": "done", "text": answer})));
                    }
                    Err(e) => {
                        let _ = tx.send(format!(
                            "{}\n",
                            json!({"type": "error", "text": format!("{e:#}")})
                        ));
                    }
                }
            }
            _ => {}
        }
    }

    drop(tx);
    let _ = writer_task.await;
    Ok(())
}

/// Interactive attach: read stdin lines, send them, print streamed
/// output until `done`.
pub async fn attach(socket_path: &Path) -> Result<()> {
    let stream = UnixStream::connect(socket_path)
        .await
        .with_context(|| format!("connecting to {}", socket_path.display()))?;
    eprintln!("sigil attach: connected to {}", socket_path.display());

    let (reader, mut writer) = stream.into_split();
    let mut lines = BufReader::new(reader).lines();
    // Writer task: stdin -> socket.
    let stdin_task = tokio::spawn(async move {
        let stdin = tokio::io::stdin();
        let mut stdin_lines = BufReader::new(stdin).lines();
        while let Ok(Some(line)) = stdin_lines.next_line().await {
            let msg = json!({"type": "user", "text": line});
            if writer
                .write_all(format!("{msg}\n").as_bytes())
                .await
                .is_err()
            {
                break;
            }
            let _ = writer.flush().await;
        }
    });

    while let Ok(Some(line)) = lines.next_line().await {
        let v: Value = serde_json::from_str(&line).unwrap_or_default();
        match v["type"].as_str() {
            Some("delta") => {
                print!("{}", v["text"].as_str().unwrap_or(""));
                let _ = std::io::stdout().flush();
            }
            Some("done") => {
                println!();
                break;
            }
            Some("error") => {
                eprintln!("daemon error: {}", v["text"].as_str().unwrap_or("?"));
                break;
            }
            _ => {}
        }
    }
    stdin_task.abort();
    Ok(())
}

/// One-shot send: send one message, print the streamed output and the
/// final answer.
pub async fn send_once(socket_path: &Path, text: &str) -> Result<String> {
    let mut stream = UnixStream::connect(socket_path)
        .await
        .with_context(|| format!("connecting to {}", socket_path.display()))?;
    let msg = json!({"type": "user", "text": text});
    stream.write_all(format!("{msg}\n").as_bytes()).await?;
    let (reader, _) = stream.into_split();
    let mut lines = BufReader::new(reader).lines();
    let mut out = String::new();
    while let Ok(Some(line)) = lines.next_line().await {
        let v: Value = serde_json::from_str(&line).unwrap_or_default();
        match v["type"].as_str() {
            Some("delta") => out.push_str(v["text"].as_str().unwrap_or("")),
            Some("done") => return Ok(v["text"].as_str().unwrap_or("").to_string()),
            Some("error") => anyhow::bail!("daemon error: {}", v["text"].as_str().unwrap_or("?")),
            _ => {}
        }
    }
    Ok(out)
}

/// Turn a loaded session's turns into agent messages (used by `serve`
/// when resuming).
fn turns_to_messages(turns: Vec<Turn>) -> Vec<ChatMessage> {
    turns
        .into_iter()
        .map(|t| match t.role.as_str() {
            "assistant" => ChatMessage::assistant(t.text),
            _ => ChatMessage::user(t.text),
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn turns_to_messages_maps_roles() {
        let turns = vec![
            Turn {
                role: "user".into(),
                text: "hi".into(),
                ts: None,
            },
            Turn {
                role: "assistant".into(),
                text: "hello".into(),
                ts: None,
            },
        ];
        let msgs = turns_to_messages(turns);
        assert_eq!(msgs.len(), 2);
        assert_eq!(msgs[0].content, "hi");
        assert_eq!(msgs[1].content, "hello");
    }

    #[tokio::test]
    async fn send_once_parses_delta_and_done() {
        let dir = std::env::temp_dir().join(format!("sigil-daemon-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let sock = dir.join("test.sock");
        let listener = UnixListener::bind(&sock).unwrap();

        let server = tokio::spawn(async move {
            let (stream, _) = listener.accept().await.unwrap();
            let mut buf = String::new();
            let mut reader = BufReader::new(stream);
            let _ = reader.read_line(&mut buf).await;
            assert!(buf.contains("\"type\":\"user\""), "got: {buf}");
            let reply = "{\"type\":\"delta\",\"text\":\"hel\"}\n{\"type\":\"delta\",\"text\":\"lo\"}\n{\"type\":\"done\",\"text\":\"hello world\"}\n";
            let mut writer = reader.into_inner();
            writer.write_all(reply.as_bytes()).await.unwrap();
        });

        let answer = send_once(&sock, "hi there").await.unwrap();
        assert_eq!(answer, "hello world");
        server.await.unwrap();
        let _ = std::fs::remove_dir_all(&dir);
    }
}
