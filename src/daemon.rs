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

use std::collections::HashMap;
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

/// One parallel session: its own message history (herdr's "pane" —
/// an independent agent conversation living in the same daemon).
struct SessionState {
    messages: tokio::sync::Mutex<Vec<ChatMessage>>,
}

/// Shared daemon state: one session state per session id (the default
/// session is the one the daemon started with), plus a lock that
/// serializes agent loops (one turn at a time, FIFO across clients
/// and sessions — turns never run concurrently, matching herdr's
/// single-runtime model).
struct DaemonState {
    sessions: tokio::sync::Mutex<HashMap<String, Arc<SessionState>>>,
    default_session: String,
    loop_lock: tokio::sync::Mutex<()>,
}

fn bind_socket(path: &Path) -> Result<UnixListener> {
    let parent = path.parent().unwrap_or_else(|| Path::new("."));
    crate::harness::lock::with_dir_lock(parent, || {
        use std::os::unix::fs::FileTypeExt;
        match std::fs::symlink_metadata(path) {
            Ok(metadata) => {
                anyhow::ensure!(
                    metadata.file_type().is_socket(),
                    "refusing to remove non-socket {}",
                    path.display()
                );
                match std::os::unix::net::UnixStream::connect(path) {
                    Ok(_) => anyhow::bail!("daemon already listening on {}", path.display()),
                    Err(error)
                        if matches!(
                            error.kind(),
                            std::io::ErrorKind::ConnectionRefused | std::io::ErrorKind::NotFound
                        ) =>
                    {
                        std::fs::remove_file(path)?;
                    }
                    Err(error) => return Err(error.into()),
                }
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => return Err(error.into()),
        }
        Ok(UnixListener::bind(path)?)
    })
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
    // Session: resume an existing one or start fresh. `latest`
    // resolves to the most recently active session (same as the REPL).
    let session_id = match resume {
        Some("latest") => crate::session::latest(sessions_dir)?.ok_or_else(|| {
            anyhow::anyhow!("no sessions yet — start a new daemon without --resume")
        })?,
        Some(id) if !id.is_empty() => {
            // Validate the session loads before serving.
            crate::session::load(sessions_dir, id)
                .with_context(|| format!("loading session '{id}'"))?;
            id.to_string()
        }
        _ => crate::session::start(sessions_dir)?,
    };
    // Load the resumed history into the shared state.
    let mut sessions = HashMap::new();
    let turns = crate::session::load(sessions_dir, &session_id)?;
    if !turns.is_empty() {
        // Phase A2: 完整派生（含工具历史）+ 崩溃残留修复。
        let msgs = crate::session::resume_messages(sessions_dir, &session_id)?;
        let st = SessionState {
            messages: tokio::sync::Mutex::new(msgs),
        };
        sessions.insert(session_id.clone(), Arc::new(st));
    }
    let state = Arc::new(DaemonState {
        sessions: tokio::sync::Mutex::new(sessions),
        default_session: session_id.clone(),
        loop_lock: tokio::sync::Mutex::new(()),
    });

    if let Some(parent) = socket_path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let listener = bind_socket(socket_path)
        .with_context(|| format!("binding daemon socket {}", socket_path.display()))?;
    eprintln!(
        "sigil daemon: listening on {} (pid {}, session {})",
        socket_path.display(),
        std::process::id(),
        state.default_session
    );
    eprintln!("  attach:  sigil attach --socket {}", socket_path.display());
    eprintln!(
        "  send:    sigil send \"message\" --socket {}",
        socket_path.display()
    );

    // Cron scheduler: every 30s, fire due jobs (prime-agent's
    // cron-jobs.ts, minimal interval/once schedules). A due job
    // injects its prompt into the target session exactly like a user
    // turn — appended, run, answered, recorded — then reschedules
    // itself. Runs serially through loop_lock like any other turn.
    let cron_provider = provider.clone();
    let cron_config = config.clone();
    let mut cron_tools = tools.clone_shallow();
    let cron_state = state.clone();
    let cron_sessions_dir = sessions_dir.to_path_buf();
    tokio::spawn(async move {
        let mut ticker = tokio::time::interval(std::time::Duration::from_secs(30));
        loop {
            ticker.tick().await;
            let now = crate::cron::now_unix();
            let cron_path = cron_sessions_dir.join("cron.json");
            let jobs = crate::cron::load(&cron_path);
            let due = crate::cron::due(&jobs, now);
            if due.is_empty() {
                continue;
            }
            for job in due {
                let _guard = cron_state.loop_lock.lock().await;
                // A queued job may have been paused or removed while another turn ran.
                let Some(job) = crate::cron::load(&cron_path).into_iter().find(|current| {
                    current.id == job.id
                        && current.status == "active"
                        && current.next_run_at <= crate::cron::now_unix()
                }) else {
                    continue;
                };
                let target = job
                    .session
                    .clone()
                    .unwrap_or_else(|| cron_state.default_session.clone());
                let Ok((sid, session)) =
                    resolve_session(&cron_state, &cron_sessions_dir, Some(&target)).await
                else {
                    continue;
                };
                eprintln!(
                    "  [daemon] cron '{}' firing into session {}: {:?}",
                    truncate_log(&job.id, 16),
                    truncate_log(&sid, 24),
                    truncate_log(&job.prompt, 40)
                );
                let _ = crate::session::append(
                    &cron_sessions_dir,
                    &sid,
                    &Turn {
                        role: "user".into(),
                        text: job.prompt.clone(),
                        ts: None,
                    },
                );
                {
                    let mut messages = session.messages.lock().await;
                    messages.push(ChatMessage::user(job.prompt.clone()));
                }
                let run_msgs = crate::agent::messages_for_turn(
                    &session.messages.lock().await,
                    cron_config.system_prompt_for(&job.prompt),
                );
                // Phase 63 event-sourced logging: per-job config clone
                // with a sink pinned to this job's session id (daemon
                // runs jobs concurrently, so a shared sink would cross
                // sessions).
                let mut job_config = cron_config.clone();
                {
                    let sink_dir = cron_sessions_dir.clone();
                    let sink_sid = sid.clone();
                    job_config.session_sink =
                        Some(std::sync::Arc::new(move |ev: crate::session::Event| {
                            let _ = crate::session::append_event(&sink_dir, &sink_sid, &ev);
                        }));
                }
                if let Ok(result) = run_messages(
                    cron_provider.as_ref(),
                    &mut cron_tools,
                    &job_config,
                    &run_msgs,
                    None,
                    None,
                )
                .await
                {
                    {
                        let mut messages = session.messages.lock().await;
                        *messages = result.messages;
                    }
                    // (assistant turn is logged by the loop's session
                    //  sink — job_config carries the per-job sink above)
                }
                if let Err(error) =
                    crate::cron::record_run(&cron_path, &job, crate::cron::now_unix())
                {
                    eprintln!("warning: saving cron run: {error:#}");
                }
            }
        }
    });

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

/// Resolve the session for a user message: an explicit `session`
/// field wins, otherwise the daemon's default session. Unknown ids
/// are first looked up on disk (a previously saved session) and then
/// started fresh — `sigil send --session foo` opens a new parallel
/// pane on first use and keeps appending to it afterwards.
async fn resolve_session(
    state: &DaemonState,
    sessions_dir: &Path,
    requested: Option<&str>,
) -> Result<(String, Arc<SessionState>)> {
    let id = requested.unwrap_or(&state.default_session).to_string();
    let mut sessions = state.sessions.lock().await;
    if let Some(st) = sessions.get(&id) {
        return Ok((id, st.clone()));
    }
    // Keep the map locked through initialization so concurrent first requests
    // obtain the same session history.
    // Not in memory: adopt an existing on-disk session or start one.
    let turns = crate::session::load(sessions_dir, &id)?;
    let on_disk = sessions_dir.join(format!("{id}.jsonl")).exists();
    let st = if turns.is_empty() && !on_disk {
        crate::session::start_with_id(sessions_dir, &id)?;
        SessionState {
            messages: tokio::sync::Mutex::new(Vec::new()),
        }
    } else {
        // Phase A2: 完整派生（含工具历史）+ 崩溃残留修复。
        SessionState {
            messages: tokio::sync::Mutex::new(crate::session::resume_messages(sessions_dir, &id)?),
        }
    };
    let st = Arc::new(st);
    sessions.insert(id.clone(), st.clone());
    Ok((id, st))
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

    // Stream deltas to the socket via a channel (the agent loop's
    // stream callback is synchronous; a writer task drains it). Must
    // be an ASYNC channel: a blocking std mpsc recv() inside the
    // writer task would occupy a tokio worker per connection and
    // deadlock once connections outnumber workers (the whole daemon
    // froze at N=8 concurrent sends on an 8-core box).
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<String>();
    let writer_task = tokio::spawn(async move {
        let mut writer = writer;
        while let Some(line) = rx.recv().await {
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
            Some("session") => {
                // Session control: {"type":"session","action":"list"} lists
                // every on-disk session; {"type":"session","action":"new"}
                // starts a fresh parallel session (pane) and reports its id.
                let action = v["action"].as_str().unwrap_or("");
                match action {
                    "list" => {
                        let ids = crate::session::list(sessions_dir)?;
                        let mut sessions_out = Vec::new();
                        for id in ids {
                            let turns = crate::session::load(sessions_dir, &id)?;
                            sessions_out.push(json!({
                                "id": id,
                                "turns": turns.len(),
                                "preview": turns.last().map(|t| truncate_log(&t.text, 60)),
                            }));
                        }
                        let _ = tx.send(format!(
                            "{}\n",
                            json!({"type": "sessions", "sessions": sessions_out})
                        ));
                    }
                    "new" => {
                        let new_id = crate::session::start(sessions_dir)?;
                        let st = SessionState {
                            messages: tokio::sync::Mutex::new(Vec::new()),
                        };
                        state
                            .sessions
                            .lock()
                            .await
                            .insert(new_id.clone(), Arc::new(st));
                        let _ = tx.send(format!(
                            "{}\n",
                            json!({"type": "session_created", "id": new_id})
                        ));
                    }
                    other => {
                        let _ = tx.send(format!(
                            "{}\n",
                            json!({"type": "error", "text": format!("unknown session action '{other}' (expected 'list' or 'new')")})
                        ));
                    }
                }
            }
            Some("user") => {
                let text = v["text"].as_str().unwrap_or("").to_string();
                if text.trim().is_empty() {
                    continue;
                }
                let (session_id, session) =
                    resolve_session(state, sessions_dir, v["session"].as_str()).await?;
                eprintln!(
                    "  [daemon] user turn (session {}): {:?}",
                    truncate_log(&session_id, 24),
                    truncate_log(&text, 40)
                );
                let _guard = state.loop_lock.lock().await;
                // Append the user turn (session + in-memory history).
                crate::session::append(
                    sessions_dir,
                    &session_id,
                    &Turn {
                        role: "user".into(),
                        text: text.clone(),
                        ts: None,
                    },
                )?;
                {
                    let mut messages = session.messages.lock().await;
                    messages.push(ChatMessage::user(text.clone()));
                }

                // Run the agent loop over the full conversation:
                // system prompt (with skills inlined for this user
                // message) + the accumulated history + the new turn.
                let run_msgs = crate::agent::messages_for_turn(
                    &session.messages.lock().await,
                    config.system_prompt_for(&text),
                );
                // Event-sourced logging (Phase 63): wire a sink pinned to
                // this turn's session so ToolCall/ToolResult events land in
                // the event log. Without it, ordinary daemon turns logged
                // only the user/assistant turns and ALL tool history was
                // silently lost across a daemon restart — yet serve()
                // restores context from exactly that event log
                // (repair_unpaired_tool_calls + events_to_messages).
                let mut turn_config = config.clone();
                {
                    let sink_dir = sessions_dir.to_path_buf();
                    let sink_sid = session_id.clone();
                    turn_config.session_sink =
                        Some(std::sync::Arc::new(move |ev: crate::session::Event| {
                            let _ = crate::session::append_event(&sink_dir, &sink_sid, &ev);
                        }));
                }

                // Stream deltas to this client via the channel.
                let tx2 = tx.clone();
                let stream_cb = move |delta: Option<&str>| {
                    if let Some(d) = delta {
                        let _ = tx2.send(format!("{}\n", json!({"type": "delta", "text": d})));
                    }
                };
                let result = run_messages(
                    provider.as_ref(),
                    &mut tools,
                    &turn_config,
                    &run_msgs,
                    None,
                    Some(&stream_cb),
                )
                .await;
                eprintln!(
                    "  [daemon] turn done: {:?}",
                    result
                        .as_ref()
                        .map(|r| truncate_log(&r.output, 40))
                        .unwrap_or_else(|_| "ERR".to_string())
                );

                match result {
                    Ok(result) => {
                        {
                            let mut messages = session.messages.lock().await;
                            *messages = result.messages;
                        }
                        // (assistant turn is logged by the loop's session
                        //  sink on every success exit — see turn_config)
                        let state_tag = match result.state {
                            crate::agent::RunState::StepLimitReached => "step_limit",
                            crate::agent::RunState::Success => "ok",
                        };
                        let _ = tx.send(format!(
                            "{}\n",
                            json!({
                                "type": "done",
                                "text": result.output,
                                "state": state_tag,
                            })
                        ));
                    }
                    Err(e) => {
                        let _ = tx.send(format!(
                            "{}\n",
                            json!({"type": "error", "text": format!("{e:#}")})
                        ));
                    }
                }
            }
            Some("side_question") => {
                // prime-agent's side-question pattern: the user can ask
                // the agent a question against the current conversation
                // context WITHOUT adding anything to the session. The
                // answer is computed from a clone of the session
                // messages with NO tools and a single-turn budget, then
                // reported via a `side_answer` frame — the session
                // history (memory + on-disk) is untouched. Serial like
                // a regular turn (waits for any in-flight turn).
                let text = v["text"].as_str().unwrap_or("").to_string();
                if text.trim().is_empty() {
                    continue;
                }
                let (session_id, session) =
                    resolve_session(state, sessions_dir, v["session"].as_str()).await?;
                eprintln!(
                    "  [daemon] side question (session {}): {:?}",
                    truncate_log(&session_id, 24),
                    truncate_log(&text, 40)
                );
                let _guard = state.loop_lock.lock().await;
                let mut sq_msgs = crate::agent::messages_for_turn(
                    &session.messages.lock().await,
                    config.system_prompt_for(&text),
                );
                sq_msgs.push(ChatMessage::user(text.clone()));
                let mut no_tools = ToolRegistry::default();
                let side_config = AgentConfig {
                    max_steps: 1,
                    goals: None,
                    goals_dir: None,
                    session_sink: None,
                    ..config.clone()
                };
                let tx2 = tx.clone();
                let stream_cb = move |delta: Option<&str>| {
                    if let Some(d) = delta {
                        let _ = tx2.send(format!("{}\n", json!({"type": "side_delta", "text": d})));
                    }
                };
                let answer = match run_messages(
                    provider.as_ref(),
                    &mut no_tools,
                    &side_config,
                    &sq_msgs,
                    None,
                    Some(&stream_cb),
                )
                .await
                {
                    Ok(r) => r.output,
                    Err(e) => format!("side question failed: {e:#}"),
                };
                let _ = tx.send(format!(
                    "{}\n",
                    json!({
                        "type": "side_answer",
                        "id": v["id"].as_str().unwrap_or(""),
                        "text": answer,
                    })
                ));
            }
            Some("cron") => {
                // Scheduled turns (prime-agent cron-jobs.ts, minimal):
                //   {"type":"cron","action":"add","kind":"interval|once",
                //    "interval_secs":N|"at":unix,"prompt":"...","session":?}
                //   {"type":"cron","action":"list"}
                //   {"type":"cron","action":"pause|resume|remove","id":"..."}
                // Jobs persist in <sessions_dir>/cron.json; the daemon's
                // scheduler loop fires due ones into their target session.
                let action = v["action"].as_str().unwrap_or("");
                let cron_path = sessions_dir.join("cron.json");
                let result = crate::cron::update(&cron_path, |jobs| {
                    let reply = match action {
                        "add" => {
                            let prompt = v["prompt"].as_str().unwrap_or("").to_string();
                            if prompt.trim().is_empty() {
                                json!({"type": "error", "text": "cron add requires a 'prompt'"})
                            } else {
                                let now = crate::cron::now_unix();
                                let (kind, interval_secs, at, next) = match v["kind"].as_str() {
                                    Some("once") => {
                                        let at = v["at"].as_u64().unwrap_or(now);
                                        ("once".to_string(), None, Some(at), at)
                                    }
                                    _ => {
                                        let iv = v["interval_secs"].as_u64().unwrap_or(60).max(1);
                                        ("interval".to_string(), Some(iv), None, now + iv)
                                    }
                                };
                                // Never reuse a removed job's id: an in-flight run may
                                // still merge its completion bookkeeping by that id.
                                let id = format!("c{}", crate::session::chrono_like_timestamp());
                                let job = crate::cron::CronJob {
                                    id: id.clone(),
                                    status: "active".into(),
                                    kind,
                                    interval_secs,
                                    at,
                                    prompt,
                                    session: v["session"].as_str().map(|s| s.to_string()),
                                    created_at: now,
                                    next_run_at: next,
                                    last_run_at: None,
                                    run_count: 0,
                                };
                                jobs.push(job);
                                json!({"type": "cron_result", "action": "added", "id": id, "next_run_at": next})
                            }
                        }
                        "list" => json!({
                            "type": "cron_result",
                            "action": "list",
                            "jobs": jobs,
                        }),
                        "pause" | "resume" | "remove" => {
                            let id = v["id"].as_str().unwrap_or("").to_string();
                            let mut found = false;
                            for j in jobs.iter_mut() {
                                if j.id == id {
                                    found = true;
                                    match action {
                                        "pause" => j.status = "paused".into(),
                                        "resume" => {
                                            j.status = "active".into();
                                            j.next_run_at = if j.kind == "once" {
                                                j.at.unwrap_or(crate::cron::now_unix())
                                            } else {
                                                crate::cron::now_unix()
                                                    + j.interval_secs.unwrap_or(60)
                                            };
                                        }
                                        _ => {}
                                    }
                                }
                            }
                            if action == "remove" {
                                jobs.retain(|j| j.id != id);
                            }
                            if found {
                                json!({"type": "cron_result", "action": action, "id": id})
                            } else {
                                json!({"type": "error", "text": format!("no cron job '{id}' (see cron list)")})
                            }
                        }
                        other => json!({
                            "type": "error",
                            "text": format!("unknown cron action '{other}' (expected add|list|pause|resume|remove)")
                        }),
                    };
                    Ok(reply)
                });
                let reply = result.unwrap_or_else(|error| {
                    json!({"type":"error", "text":format!("saving cron schedule: {error:#}")})
                });
                let _ = tx.send(format!("{reply}\n"));
            }
            _ => {}
        }
    }

    drop(tx);
    let _ = writer_task.await;
    Ok(())
}

/// Interactive attach: a persistent session. Send lines, watch the
/// streamed output; after each turn the prompt comes back so you can
/// keep chatting (one daemon session, multi-turn context). `/quit`
/// (or Ctrl-D) exits; `/help` lists the local commands.
pub async fn attach(socket_path: &Path, session: Option<&str>) -> Result<()> {
    let stream = UnixStream::connect(socket_path)
        .await
        .with_context(|| friendly_connect_error(socket_path))?;
    let session = session.map(|s| s.to_string());
    eprintln!("sigil attach: connected to {}", socket_path.display());
    eprintln!("type /help for commands, /quit to exit");

    let (reader, mut writer) = stream.into_split();
    let mut lines = BufReader::new(reader).lines();
    // Writer task: stdin -> socket. Local commands are handled here so
    // they never reach the daemon.
    let stdin_task = tokio::spawn(async move {
        let stdin = tokio::io::stdin();
        let mut stdin_lines = BufReader::new(stdin).lines();
        while let Ok(Some(line)) = stdin_lines.next_line().await {
            match line.trim() {
                "/quit" | "/exit" | "/q" => break,
                "/help" | "/h" => {
                    eprintln!("commands: /quit — leave; /help — this list");
                    eprintln!("anything else is sent to the daemon as a message");
                    continue;
                }
                _ => {}
            }
            let mut msg = json!({"type": "user", "text": line});
            if let Some(s) = &session {
                msg["session"] = json!(s);
            }
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

    // Read loop: stream deltas; after `done` the turn finished — loop
    // back to the prompt instead of exiting (multi-turn session).
    while let Ok(Some(line)) = lines.next_line().await {
        let v: Value = serde_json::from_str(&line).unwrap_or_default();
        match v["type"].as_str() {
            Some("delta") => {
                print!("{}", v["text"].as_str().unwrap_or(""));
                let _ = std::io::stdout().flush();
            }
            Some("done") => println!(),
            Some("error") => {
                eprintln!("daemon error: {}", v["text"].as_str().unwrap_or("?"));
            }
            _ => {}
        }
    }
    stdin_task.abort();
    Ok(())
}

/// Truncate a string for log previews without panicking on a UTF-8
/// char boundary (byte slicing `&s[..n]` panics when `n` lands inside
/// a multi-byte char, e.g. Chinese text longer than the cap).
fn truncate_log(s: &str, max_chars: usize) -> String {
    s.chars().take(max_chars).collect()
}

/// One-shot send: send one message, print the streamed output and the
/// final answer. `timeout_secs` bounds the wait (0 = wait forever).
/// `session` targets a specific session (pane); `None` uses the
/// daemon's default session.
pub async fn send_once(
    socket_path: &Path,
    text: &str,
    timeout_secs: u64,
    session: Option<&str>,
) -> Result<String> {
    let mut stream = UnixStream::connect(socket_path)
        .await
        .with_context(|| friendly_connect_error(socket_path))?;
    let mut msg = json!({"type": "user", "text": text});
    if let Some(s) = session {
        msg["session"] = json!(s);
    }
    stream.write_all(format!("{msg}\n").as_bytes()).await?;
    let (reader, _) = stream.into_split();
    let mut lines = BufReader::new(reader).lines();
    let mut out = String::new();
    // Read until `done`, optionally bounded by the caller's timeout.
    let deadline = if timeout_secs > 0 {
        Some(tokio::time::Instant::now() + std::time::Duration::from_secs(timeout_secs))
    } else {
        None
    };
    loop {
        let line = match deadline {
            Some(dl) => match tokio::time::timeout_at(dl, lines.next_line()).await {
                Ok(Ok(Some(l))) => l,
                Ok(Ok(None)) => break, // daemon closed without `done`
                Ok(Err(_)) => break,
                Err(_) => anyhow::bail!(
                    "timed out after {timeout_secs}s waiting for the daemon's reply (the turn keeps running in the daemon — attach or send again to see it)"
                ),
            },
            None => match lines.next_line().await {
                Ok(Some(l)) => l,
                _ => break,
            },
        };
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

/// A friendlier error when the socket isn't there: the daemon is
/// probably just not running.
fn friendly_connect_error(socket_path: &Path) -> String {
    format!(
        "connecting to {}: is the daemon running? start it with `sigil daemon --socket {}`",
        socket_path.display(),
        socket_path.display()
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn second_daemon_cannot_unlink_live_socket() {
        let dir = std::path::PathBuf::from("/tmp").join(format!(
            "sgsock-{}",
            crate::session::chrono_like_timestamp()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("daemon.sock");
        let _listener = UnixListener::bind(&path).unwrap();
        assert!(bind_socket(&path).is_err());
        assert!(UnixStream::connect(&path).await.is_ok());
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[tokio::test]
    async fn concurrent_startups_replace_stale_socket_once() {
        let dir = std::path::PathBuf::from("/tmp").join(format!(
            "sgstale-{}",
            crate::session::chrono_like_timestamp()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("d.sock");
        drop(std::os::unix::net::UnixListener::bind(&path).unwrap());
        let barrier = std::sync::Barrier::new(2);
        let handle = tokio::runtime::Handle::current();
        let results = std::thread::scope(|scope| {
            let first = scope.spawn(|| {
                let _entered = handle.enter();
                barrier.wait();
                bind_socket(&path)
            });
            let second = scope.spawn(|| {
                let _entered = handle.enter();
                barrier.wait();
                bind_socket(&path)
            });
            vec![first.join().unwrap(), second.join().unwrap()]
        });
        assert_eq!(
            results.iter().filter(|r| r.is_ok()).count(),
            1,
            "{results:?}"
        );
        assert!(UnixStream::connect(&path).await.is_ok());
        drop(results);
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[tokio::test]
    async fn restored_session_retains_tool_call_and_result() {
        let dir = std::env::temp_dir().join(format!(
            "sigil-resume-tools-{}",
            crate::session::chrono_like_timestamp()
        ));
        crate::session::start_with_id(&dir, "saved").unwrap();
        for event in [
            crate::session::Event::User {
                ts: None,
                text: "read".into(),
            },
            crate::session::Event::ToolCall {
                ts: None,
                id: "c1".into(),
                name: "read_file".into(),
                args: "{}".into(),
                revert_key: String::new(),
            },
            crate::session::Event::ToolResult {
                ts: None,
                id: "c1".into(),
                name: "read_file".into(),
                ok: true,
                output: "saved content".into(),
                revertible: None,
            },
            crate::session::Event::Assistant {
                ts: None,
                text: "finished".into(),
            },
        ] {
            crate::session::append_event(&dir, "saved", &event).unwrap();
        }
        let state = DaemonState {
            sessions: tokio::sync::Mutex::new(HashMap::new()),
            default_session: "saved".into(),
            loop_lock: tokio::sync::Mutex::new(()),
        };
        let (_, restored) = resolve_session(&state, &dir, None).await.unwrap();
        let messages = restored.messages.lock().await;
        assert!(messages.iter().any(|m| {
            m.tool_calls
                .as_ref()
                .is_some_and(|calls| calls[0].id == "c1")
        }));
        assert!(
            messages
                .iter()
                .any(|m| m.role == crate::llm::Role::Tool && m.content == "saved content")
        );
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn turns_to_messages_maps_roles() {
        let turns = vec![
            Turn {
                role: "user".into(),
                text: "hello".into(),
                ts: None,
            },
            Turn {
                role: "assistant".into(),
                text: "hi".into(),
                ts: None,
            },
        ];
        let msgs = crate::session::turns_to_messages(turns);
        assert_eq!(msgs[0].role, crate::llm::Role::User);
        assert_eq!(msgs[1].role, crate::llm::Role::Assistant);
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

        let answer = send_once(&sock, "hi there", 10, None).await.unwrap();
        assert_eq!(answer, "hello world");
        server.await.unwrap();
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn send_once_with_session_targets_the_frame() {
        let dir = std::env::temp_dir().join(format!("sigil-daemon-sess-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let sock = dir.join("test.sock");
        let listener = UnixListener::bind(&sock).unwrap();

        let server = tokio::spawn(async move {
            let (stream, _) = listener.accept().await.unwrap();
            let mut buf = String::new();
            let mut reader = BufReader::new(stream);
            let _ = reader.read_line(&mut buf).await;
            assert!(
                buf.contains("\"session\":\"pane-a\""),
                "session field missing: {buf}"
            );
            let reply = "{\"type\":\"done\",\"text\":\"ok\"}\n";
            let mut writer = reader.into_inner();
            writer.write_all(reply.as_bytes()).await.unwrap();
        });

        let answer = send_once(&sock, "hi", 10, Some("pane-a")).await.unwrap();
        assert_eq!(answer, "ok");
        server.await.unwrap();
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn resolve_session_falls_back_to_default_and_starts_fresh() {
        let dir = std::env::temp_dir().join(format!("sigil-daemon-resolve-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let def = crate::session::start(&dir).unwrap();
        let state = Arc::new(DaemonState {
            sessions: tokio::sync::Mutex::new(HashMap::new()),
            default_session: def.clone(),
            loop_lock: tokio::sync::Mutex::new(()),
        });

        // No request → the default session.
        let (id, st) = resolve_session(&state, &dir, None).await.unwrap();
        assert_eq!(id, def);
        assert!(st.messages.lock().await.is_empty());

        // Unknown id → a fresh session, remembered for next time.
        let (id, st) = resolve_session(&state, &dir, Some("pane-x")).await.unwrap();
        assert_eq!(id, "pane-x");
        st.messages
            .lock()
            .await
            .push(ChatMessage::user("hi".to_string()));
        let (id2, st2) = resolve_session(&state, &dir, Some("pane-x")).await.unwrap();
        assert_eq!(id2, "pane-x");
        assert_eq!(
            st2.messages.lock().await.len(),
            1,
            "same session state reused"
        );
        assert!(
            dir.join("pane-x.jsonl").exists(),
            "session persisted on disk"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }
}
