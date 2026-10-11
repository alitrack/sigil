//! Session persistence: REPL turns are appended to a JSONL file
//! (`~/.sigil/sessions/<timestamp>.jsonl`); `--resume <id>` loads a
//! session's history so a conversation continues across invocations.
//!
//! Storage format is an **event-sourced log** (Phase 63, borrowing the
//! DeepSeek Harness session-log design): each line is a typed event —
//! user/assistant messages, tool calls, tool results — and the line
//! number doubles as the monotonic event `seq` (no stored counter, so
//! concurrent appenders cannot corrupt a shared sequence). Replay is
//! deterministic: the raw log is never rewritten, so future features
//! (fork/resume branches, tool-call visualization, audit trails) can
//! re-derive any projection.
//!
//! Line shapes:
//! - New: `{"e":"user","ts":"...","text":"..."}` (event tag `e`)
//! - Legacy (pre-Phase 63): `{"role":"user","text":"...","ts":"..."}`
//!   — recognized by the absence of `e` and projected into events on load.
//!
//! The public `append(Turn)` / `load() -> Vec<Turn>` surface is
//! unchanged; `append` writes event lines, `load` projects events back
//! to turns, so existing callers (REPL, daemon, export/import) are
//! untouched.

use std::fs::{self, OpenOptions};
use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};

/// Default sessions directory.
pub fn default_sessions_dir() -> PathBuf {
    dirs::home_dir()
        .unwrap_or_else(|| PathBuf::from("."))
        .join(".sigil")
        .join("sessions")
}

/// A single recorded turn (the conversation-level projection of events;
/// tool events are not part of this view).
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct Turn {
    pub role: String, // "user" | "assistant"
    pub text: String,
    /// Write timestamp (`chrono_like_timestamp` format, nanosecond
    /// precision). `None` for legacy JSONL lines written before Phase 31.
    #[serde(default)]
    pub ts: Option<String>,
}

/// A typed session-log event (Phase 63 event-sourced format).
///
/// `seq` is assigned by the reader as the 1-based line number — it is
/// NOT stored in the file, so appends never need to read back the tail
/// to compute the next sequence.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
#[serde(tag = "e", rename_all = "snake_case")]
pub enum Event {
    /// A user message (also the start of a turn).
    User {
        #[serde(default)]
        ts: Option<String>,
        text: String,
    },
    /// An assistant reply text (final answer or streamed text).
    Assistant {
        #[serde(default)]
        ts: Option<String>,
        text: String,
    },
    /// A tool invocation requested by the model.
    /// `id` = tool-call id (Phase A; legacy → empty).
    /// `revert_key` (Cordis P3): a stable fingerprint of the call (name +
    /// canonicalized args) — the inverse-identifier that lets a log replay
    /// find "the same call" for per-frame rollback. Empty on legacy lines.
    ToolCall {
        #[serde(default)]
        ts: Option<String>,
        #[serde(default)]
        id: String,
        name: String,
        args: String,
        #[serde(default)]
        revert_key: String,
    },
    /// The outcome of a tool invocation.
    /// `id` pairs with the originating ToolCall (Phase A; legacy → empty).
    /// `revertible` (Cordis P3): `Some(true)` = this call registered a
    /// runtime-revertible effect (P1) so the session can roll it back
    /// frame-by-frame; `Some(false)` = assessed, not revertible; `None` =
    /// not assessed (legacy line / tool the registry doesn't track).
    ToolResult {
        #[serde(default)]
        ts: Option<String>,
        #[serde(default)]
        id: String,
        name: String,
        /// `true` = tool succeeded; `false` = error output.
        ok: bool,
        output: String,
        #[serde(default)]
        revertible: Option<bool>,
    },
    /// One agent turn begins (structural event; not projected to messages).
    TurnStart {
        #[serde(default)]
        ts: Option<String>,
    },
    /// One agent turn ends.
    TurnEnd {
        #[serde(default)]
        ts: Option<String>,
    },
    /// One agent step begins (step = one model request + its tools).
    StepStart {
        #[serde(default)]
        ts: Option<String>,
    },
    /// One agent step ends.
    StepEnd {
        #[serde(default)]
        ts: Option<String>,
    },
    /// Request header snapshot: model / temperature / max_tokens / tools
    /// fingerprint for the last model request. Metadata: not projected.
    RequestHeader {
        #[serde(default)]
        ts: Option<String>,
        model: String,
        #[serde(default)]
        temperature: Option<f32>,
        #[serde(default)]
        max_tokens: Option<u32>,
        #[serde(default)]
        tools_fingerprint: Option<String>,
    },
    /// Observed usage for one actual model request. Missing telemetry is
    /// explicit; this metadata is never projected into model context.
    RequestUsage {
        #[serde(default)]
        ts: Option<String>,
        model: String,
        request_kind: String,
        usage: Option<crate::llm::provider::TokenUsage>,
    },
    /// Injected request context (pinned workflow path, session binding,
    /// goal, ...). Order-preserving key/value pairs. "注入即事件":
    /// any session-external state entering a request is logged here.
    RequestContext {
        #[serde(default)]
        ts: Option<String>,
        values: Vec<(String, String)>,
    },
    /// An approval decision for a gate-rejected tool call. Audit-only:
    /// recorded in the log, but never projected back into model turns.
    Approval {
        #[serde(default)]
        ts: Option<String>,
        name: String,
        args: String,
        /// "approved" | "rejected" | "auto_approved" | "auto_rejected".
        decision: String,
        /// Optional human/auto approver note.
        #[serde(default)]
        reason: Option<String>,
    },
    /// A safety-gate classification hit. Audit-only.
    Policy {
        #[serde(default)]
        ts: Option<String>,
        name: String,
        /// "read" | "write" | "dangerous" | "harness".
        level: String,
        /// "allowed" | "rejected" | "requires_approval".
        verdict: String,
    },
    /// A Lua hook trigger record. Audit-only.
    Hook {
        #[serde(default)]
        ts: Option<String>,
        name: String,
        /// "ran" | "error" | "skipped".
        outcome: String,
        #[serde(default)]
        detail: Option<String>,
    },
}

impl Event {
    /// Stable display tag for tooling (same strings as the `e` tag).
    /// Used by tests today; wired to the P1 `/session events` viewer.
    #[allow(dead_code)]
    pub fn kind(&self) -> &'static str {
        match self {
            Event::User { .. } => "user",
            Event::Assistant { .. } => "assistant",
            Event::ToolCall { .. } => "tool_call",
            Event::ToolResult { .. } => "tool_result",
            Event::TurnStart { .. } => "turn_start",
            Event::TurnEnd { .. } => "turn_end",
            Event::StepStart { .. } => "step_start",
            Event::StepEnd { .. } => "step_end",
            Event::RequestHeader { .. } => "request_header",
            Event::RequestUsage { .. } => "request_usage",
            Event::RequestContext { .. } => "request_context",
            Event::Approval { .. } => "approval",
            Event::Policy { .. } => "policy",
            Event::Hook { .. } => "hook",
        }
    }

    /// Timestamp, if the event carried one.
    #[allow(dead_code)]
    pub fn ts(&self) -> Option<&str> {
        match self {
            Event::User { ts, .. }
            | Event::Assistant { ts, .. }
            | Event::ToolCall { ts, .. }
            | Event::ToolResult { ts, .. }
            | Event::TurnStart { ts, .. }
            | Event::TurnEnd { ts, .. }
            | Event::StepStart { ts, .. }
            | Event::StepEnd { ts, .. }
            | Event::RequestHeader { ts, .. }
            | Event::RequestUsage { ts, .. }
            | Event::RequestContext { ts, .. }
            | Event::Approval { ts, .. }
            | Event::Policy { ts, .. }
            | Event::Hook { ts, .. } => ts.as_deref(),
        }
    }

    /// Human-readable one-line summary (P1 `/session events` viewer).
    #[allow(dead_code)]
    pub fn summary(&self) -> String {
        match self {
            Event::User { text, .. } => {
                format!("user: {}", flat_preview(text, 60))
            }
            Event::Assistant { text, .. } => {
                format!("assistant: {}", flat_preview(text, 60))
            }
            Event::ToolCall { name, args, .. } => {
                format!("tool_call: {name}({})", flat_preview(args, 40))
            }
            Event::ToolResult {
                name, ok, output, ..
            } => {
                let status = if *ok { "ok" } else { "err" };
                format!(
                    "tool_result: {name} [{status}] {}",
                    flat_preview(output, 40)
                )
            }
            Event::TurnStart { .. } => "turn_start".into(),
            Event::TurnEnd { .. } => "turn_end".into(),
            Event::StepStart { .. } => "step_start".into(),
            Event::StepEnd { .. } => "step_end".into(),
            Event::RequestHeader { model, .. } => {
                format!("request_header: model={model}")
            }
            Event::RequestUsage {
                model,
                request_kind,
                usage,
                ..
            } => match usage {
                Some(u) => format!(
                    "request_usage: {request_kind} model={model} {} prompt + {} completion",
                    u.prompt_tokens, u.completion_tokens
                ),
                None => format!("request_usage: {request_kind} model={model} unavailable"),
            },
            Event::RequestContext { values, .. } => {
                let keys: Vec<&str> = values.iter().map(|(k, _)| k.as_str()).collect();
                format!("request_context: {}", keys.join(","))
            }
            Event::Approval { name, decision, .. } => format!("approval: {name} → {decision}"),
            Event::Policy {
                name,
                level,
                verdict,
                ..
            } => format!("policy: {name} [{level}] → {verdict}"),
            Event::Hook { name, outcome, .. } => format!("hook: {name} {outcome}"),
        }
    }
}

/// First N chars of text with whitespace flattened to single spaces.
fn flat_preview(s: &str, n: usize) -> String {
    let flat: String = s.split_whitespace().collect::<Vec<_>>().join(" ");
    let mut out: String = flat.chars().take(n).collect();
    if flat.chars().count() > n {
        out.push('…');
    }
    out
}

/// Start a new session file, returning its id (file stem).
pub fn start(sessions_dir: &Path) -> std::io::Result<String> {
    fs::create_dir_all(sessions_dir)?;
    let id = chrono_like_timestamp();
    let path = sessions_dir.join(format!("{id}.jsonl"));
    // Touch the file so the session exists immediately.
    OpenOptions::new().create(true).append(true).open(&path)?;
    Ok(id)
}

/// Start a session under an explicit id (daemon panes keep the name
/// the client asked for — `sigil send --session foo` persists as
/// `foo.jsonl`, not a generated timestamp).
pub fn start_with_id(sessions_dir: &Path, id: &str) -> std::io::Result<()> {
    fs::create_dir_all(sessions_dir)?;
    let path = sessions_dir.join(format!("{id}.jsonl"));
    OpenOptions::new().create(true).append(true).open(&path)?;
    Ok(())
}

// ---- Cordis P3: per-call inverse identifier (revert_key) ----
//
// A stable fingerprint of a tool call (name + canonicalized args) that lets
// a log replay find "the same call" for per-frame rollback. We deliberately
// avoid a crypto dependency: the key is an internal fingerprint, not a
// security token, so a 64-bit FNV-1a over a canonical (key-sorted) JSON
// form is collision-resistant enough for de-dup / lookup and needs no new
// crate.

/// FNV-1a over the given bytes → 64-bit hash.
fn fnv1a64(bytes: &[u8]) -> u64 {
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for b in bytes {
        h ^= *b as u64;
        h = h.wrapping_mul(0x0000_0100_0000_01b3);
    }
    h
}

/// Canonical JSON: objects emitted with their keys sorted, arrays in order,
/// so the same logical value always serializes identically (independent of
/// the caller's key ordering). `Value` here is `serde_json::Value`.
fn canonical_json(v: &serde_json::Value, out: &mut String) {
    match v {
        serde_json::Value::Null => out.push_str("null"),
        serde_json::Value::Bool(b) => out.push_str(if *b { "true" } else { "false" }),
        serde_json::Value::Number(n) => out.push_str(&n.to_string()),
        serde_json::Value::String(s) => {
            out.push('"');
            out.push_str(&s.escape_default().to_string());
            out.push('"');
        }
        serde_json::Value::Array(items) => {
            out.push('[');
            for (i, it) in items.iter().enumerate() {
                if i > 0 {
                    out.push(',');
                }
                canonical_json(it, out);
            }
            out.push(']');
        }
        serde_json::Value::Object(map) => {
            let mut keys: Vec<&String> = map.keys().collect();
            keys.sort();
            out.push('{');
            for (i, k) in keys.iter().enumerate() {
                if i > 0 {
                    out.push(',');
                }
                out.push('"');
                out.push_str(&(**k).escape_default().to_string());
                out.push('"');
                out.push(':');
                if let Some(val) = map.get(*k) {
                    canonical_json(val, out);
                } else {
                    out.push_str("null");
                }
            }
            out.push('}');
        }
    }
}

/// The Cordis P3 inverse identifier for a tool call: a hex string derived
/// from the tool name and its canonicalized args. The same logical call
/// (same name, same args regardless of key order) always yields the same
/// key; different calls (even differing by one arg) yield different keys.
pub fn revert_key(name: &str, args: &serde_json::Value) -> String {
    let mut buf = String::new();
    buf.push_str(name);
    buf.push('\x1f'); // record separator: name / args boundary
    canonical_json(args, &mut buf);
    format!("{:016x}", fnv1a64(buf.as_bytes()))
}

/// Append one event to a session file. If the event has no `ts` yet, it
/// is stamped with the current timestamp.
pub fn append_event(sessions_dir: &Path, id: &str, ev: &Event) -> std::io::Result<()> {
    let path = sessions_dir.join(format!("{id}.jsonl"));
    let stamped = stamp_event(ev);
    let line = serde_json::to_string(&stamped).map_err(std::io::Error::other)?;
    // One write() of payload+newline (writeln! issues two — a crash
    // between them leaves an unterminated line that the next append
    // concatenates onto, corrupting both events). Under the dir lock:
    // the REPL, the daemon, and `sigil send --session` can all append
    // to the same session id from different processes.
    let mut buf = String::with_capacity(line.len() + 1);
    buf.push_str(&line);
    buf.push('\n');
    crate::harness::lock::with_dir_lock(sessions_dir, || {
        let mut f = OpenOptions::new().create(true).append(true).open(&path)?;
        use std::io::Write;
        f.write_all(buf.as_bytes())?;
        Ok(())
    })
    .map_err(|e| std::io::Error::other(e.to_string()))
}

/// Append one turn (compat wrapper: writes the equivalent event line).
/// If the turn has no `ts` yet, it is stamped with the current timestamp
/// (callers can supply an explicit `ts` for replay/import of historical
/// turns).
pub fn append(sessions_dir: &Path, id: &str, turn: &Turn) -> std::io::Result<()> {
    let ev = match turn.role.as_str() {
        "assistant" => Event::Assistant {
            ts: turn.ts.clone(),
            text: turn.text.clone(),
        },
        _ => Event::User {
            ts: turn.ts.clone(),
            text: turn.text.clone(),
        },
    };
    append_event(sessions_dir, id, &ev)
}

/// 崩溃残留修复（Phase A2，对齐 cc M4）：扫描事件，对未配对的工具调用
/// （ToolCall 无配对 ToolResult）追加合成结果事件 —— 会话不 brick，
/// 修复本身是日志事件，可审计。返回修复的未配对调用数。
pub fn repair_unpaired_tool_calls(sessions_dir: &Path, id: &str) -> std::io::Result<usize> {
    let events = load_events(sessions_dir, id)?;
    let mut pending: std::collections::HashSet<String> = std::collections::HashSet::new();
    for ev in &events {
        match ev {
            Event::ToolCall { id: call_id, .. } => {
                if !call_id.is_empty() {
                    pending.insert(call_id.clone());
                }
            }
            Event::ToolResult { id: call_id, .. } => {
                pending.remove(call_id);
            }
            _ => {}
        }
    }

    let mut repaired = 0;
    for call_id in pending {
        append_event(
            sessions_dir,
            id,
            &Event::ToolResult {
                ts: None,
                id: call_id,
                name: String::new(),
                ok: false,
                output: "[recovered: tool call interrupted before result]".into(),
                revertible: None,
            },
        )?;
        repaired += 1;
    }
    Ok(repaired)
}

/// Stamp a missing timestamp onto a clone of the event.
fn stamp_event(ev: &Event) -> Event {
    let ts = ev
        .ts()
        .map(|s| s.to_string())
        .unwrap_or_else(chrono_like_timestamp);
    match ev {
        Event::User { text, .. } => Event::User {
            ts: Some(ts),
            text: text.clone(),
        },
        Event::Assistant { text, .. } => Event::Assistant {
            ts: Some(ts),
            text: text.clone(),
        },
        Event::ToolCall {
            id,
            name,
            args,
            revert_key,
            ..
        } => Event::ToolCall {
            ts: Some(ts),
            id: id.clone(),
            name: name.clone(),
            args: args.clone(),
            revert_key: revert_key.clone(),
        },
        Event::ToolResult {
            id,
            name,
            ok,
            output,
            revertible,
            ..
        } => Event::ToolResult {
            ts: Some(ts),
            id: id.clone(),
            name: name.clone(),
            ok: *ok,
            output: output.clone(),
            revertible: *revertible,
        },
        Event::TurnStart { .. } => Event::TurnStart { ts: Some(ts) },
        Event::TurnEnd { .. } => Event::TurnEnd { ts: Some(ts) },
        Event::StepStart { .. } => Event::StepStart { ts: Some(ts) },
        Event::StepEnd { .. } => Event::StepEnd { ts: Some(ts) },
        Event::RequestHeader {
            model,
            temperature,
            max_tokens,
            tools_fingerprint,
            ..
        } => Event::RequestHeader {
            ts: Some(ts),
            model: model.clone(),
            temperature: *temperature,
            max_tokens: *max_tokens,
            tools_fingerprint: tools_fingerprint.clone(),
        },
        Event::RequestUsage {
            model,
            request_kind,
            usage,
            ..
        } => Event::RequestUsage {
            ts: Some(ts),
            model: model.clone(),
            request_kind: request_kind.clone(),
            usage: *usage,
        },
        Event::RequestContext { values, .. } => Event::RequestContext {
            ts: Some(ts),
            values: values.clone(),
        },
        Event::Approval {
            name,
            args,
            decision,
            reason,
            ..
        } => Event::Approval {
            ts: Some(ts),
            name: name.clone(),
            args: args.clone(),
            decision: decision.clone(),
            reason: reason.clone(),
        },
        Event::Policy {
            name,
            level,
            verdict,
            ..
        } => Event::Policy {
            ts: Some(ts),
            name: name.clone(),
            level: level.clone(),
            verdict: verdict.clone(),
        },
        Event::Hook {
            name,
            outcome,
            detail,
            ..
        } => Event::Hook {
            ts: Some(ts),
            name: name.clone(),
            outcome: outcome.clone(),
            detail: detail.clone(),
        },
    }
}

/// Load a session's event log with 1-based `seq` assigned per line.
/// Legacy lines (no `e` tag) are parsed as turns and projected into
/// events on the fly, so old session files keep working unchanged.
/// Returns `(events, next_seq)`.
pub fn load_events(sessions_dir: &Path, id: &str) -> std::io::Result<Vec<Event>> {
    let path = sessions_dir.join(format!("{id}.jsonl"));
    if !path.exists() {
        return Ok(Vec::new());
    }
    let f = fs::File::open(path)?;
    let mut events = Vec::new();
    let mut lineno = 0usize;
    let mut dropped = 0usize;
    for line in BufReader::new(f).lines() {
        let line = line?;
        lineno += 1;
        if line.trim().is_empty() {
            continue;
        }
        let ev: Option<Event> = serde_json::from_str(&line).ok();
        match ev {
            Some(e) => events.push(e),
            // Legacy line without `e` tag → parse as a turn.
            None => {
                if let Ok(t) = serde_json::from_str::<Turn>(&line) {
                    let e = match t.role.as_str() {
                        "assistant" => Event::Assistant {
                            ts: t.ts.clone(),
                            text: t.text.clone(),
                        },
                        _ => Event::User {
                            ts: t.ts.clone(),
                            text: t.text.clone(),
                        },
                    };
                    events.push(e);
                } else {
                    // Corrupt line (torn write, disk damage, foreign
                    // writer): never silent — an unreported drop means
                    // context quietly disappearing from the session.
                    dropped += 1;
                    eprintln!(
                        "warning: session '{id}' line {lineno} is not a valid event                          (dropped; {} chars): {}",
                        line.chars().count(),
                        line.chars().take(80).collect::<String>()
                    );
                }
            }
        }
    }
    if dropped > 1 {
        eprintln!("warning: session '{id}' dropped {dropped} unparseable lines in total");
    }
    Ok(events)
}

/// Project an event log to conversation turns (user/assistant only).
/// Tool events are not part of the turn view — they remain in the raw
/// log for replay/audit.
pub fn events_to_turns(events: &[Event]) -> Vec<Turn> {
    events
        .iter()
        .filter_map(|e| match e {
            Event::User { ts, text } => Some(Turn {
                role: "user".into(),
                text: text.clone(),
                ts: ts.clone(),
            }),
            Event::Assistant { ts, text } => Some(Turn {
                role: "assistant".into(),
                text: text.clone(),
                ts: ts.clone(),
            }),
            _ => None,
        })
        .collect()
}

/// Load a session's turns (empty if the session doesn't exist).
pub fn load(sessions_dir: &Path, id: &str) -> std::io::Result<Vec<Turn>> {
    Ok(events_to_turns(&load_events(sessions_dir, id)?))
}

/// Restore resumable context, repairing interrupted tool calls before replay.
/// All session front ends share this projection so tool history survives switching.
pub fn resume_messages(
    sessions_dir: &Path,
    id: &str,
) -> std::io::Result<Vec<crate::llm::ChatMessage>> {
    repair_unpaired_tool_calls(sessions_dir, id)?;
    Ok(events_to_messages(&load_events(sessions_dir, id)?))
}

/// Full projection of a session's events into model-visible messages
/// (Phase A: "Model-visible means logged").
///
/// - `User` → user message
/// - `Assistant` → pending assistant text (flushed with its tool calls)
/// - `ToolCall` → accumulated into the pending assistant message
///   (consecutive calls merge into one `tool_calls` list)
/// - `ToolResult` → flush the pending assistant message, then emit the
///   tool message (paired by call id)
/// - Structural (turn/step) and metadata (request/*) events are NOT
///   projected — they exist for replay, audit, and invariant checks.
///
/// TODO(Phase A 接入): run_messages 请求构造切换到此视图后移除 allow(dead_code)。
#[allow(dead_code)]
pub fn events_to_messages(events: &[Event]) -> Vec<crate::llm::ChatMessage> {
    use crate::llm::{ChatMessage, ToolCall};

    let mut messages = Vec::new();
    let mut pending_text: Option<String> = None;
    let mut pending_calls: Vec<ToolCall> = Vec::new();

    let flush =
        |messages: &mut Vec<ChatMessage>, text: &mut Option<String>, calls: &mut Vec<ToolCall>| {
            if text.is_none() && calls.is_empty() {
                return;
            }
            let content = text.take().unwrap_or_default();
            if calls.is_empty() {
                messages.push(ChatMessage::assistant(content));
            } else {
                messages.push(ChatMessage::assistant_with_tools(
                    content,
                    std::mem::take(calls),
                ));
            }
        };

    for ev in events {
        match ev {
            Event::User { text, .. } => {
                // A new user turn: flush any dangling assistant state first.
                flush(&mut messages, &mut pending_text, &mut pending_calls);
                messages.push(ChatMessage::user(text.clone()));
            }
            Event::Assistant { text, .. } => {
                // Consecutive assistant texts merge; a new user message or
                // tool result flushes the accumulated assistant message.
                pending_text = Some(match pending_text.take() {
                    Some(prev) => format!("{prev}\n{text}"),
                    None => text.clone(),
                });
            }
            Event::ToolCall { id, name, args, .. } => {
                let arguments: serde_json::Value =
                    serde_json::from_str(args).unwrap_or(serde_json::Value::Null);
                pending_calls.push(ToolCall {
                    id: id.clone(),
                    name: name.clone(),
                    arguments,
                });
            }
            Event::ToolResult { id, output, .. } => {
                flush(&mut messages, &mut pending_text, &mut pending_calls);
                messages.push(ChatMessage::tool_result(output.clone(), id.clone()));
            }
            // Structural and metadata events never reach the model.
            _ => {}
        }
    }
    flush(&mut messages, &mut pending_text, &mut pending_calls);
    messages
}

/// List session ids (newest first) for `--resume` discovery.
pub fn list(sessions_dir: &Path) -> std::io::Result<Vec<String>> {
    let mut ids = Vec::new();
    if !sessions_dir.exists() {
        return Ok(ids);
    }
    for entry in fs::read_dir(sessions_dir)? {
        let entry = entry?;
        let name = entry.file_name().to_string_lossy().into_owned();
        if name.ends_with(".jsonl") {
            ids.push(name.trim_end_matches(".jsonl").to_string());
        }
    }
    ids.sort();
    ids.reverse();
    Ok(ids)
}

/// One line of `session list`: id, turn count, a preview of the first
/// user turn, and the last-activity timestamp (newest first).
#[derive(Debug, Clone)]
pub struct SessionSummary {
    pub id: String,
    pub turns: usize,
    /// First user message, truncated for display.
    pub preview: String,
    /// Last turn timestamp (lexicographic = chronological); `None` for
    /// sessions whose every turn predates Phase 31 (legacy).
    pub last_ts: Option<String>,
}

/// Summaries for all sessions, ordered newest-first by last activity
/// (the `ts` of the most recent turn, falling back to file mtime).
pub fn summaries(sessions_dir: &Path) -> std::io::Result<Vec<SessionSummary>> {
    let mut out = Vec::new();
    for id in list(sessions_dir)? {
        let turns = load(sessions_dir, &id)?;
        let preview = turns
            .iter()
            .find(|t| t.role == "user")
            .map(|t| flat_preview(&t.text, 60))
            .unwrap_or_default();
        let last_ts = turns.iter().rev().find_map(|t| t.ts.clone());
        out.push(SessionSummary {
            id,
            turns: turns.len(),
            preview,
            last_ts,
        });
    }
    // Newest first: by last turn ts, else by file mtime (ids are
    // chronological for legacy files whose turns predate Phase 31 —
    // id order is a fine fallback).
    out.sort_by(|a, b| {
        let ka = a.last_ts.clone().unwrap_or_else(|| a.id.clone());
        let kb = b.last_ts.clone().unwrap_or_else(|| b.id.clone());
        kb.cmp(&ka)
    });
    Ok(out)
}

/// The most recently active session id, for `--resume latest` /
/// `/session latest`. `None` when there are no sessions.
pub fn latest(sessions_dir: &Path) -> std::io::Result<Option<String>> {
    Ok(summaries(sessions_dir)?.into_iter().next().map(|s| s.id))
}

/// Turn a loaded session's turns into agent messages (resume paths:
/// REPL, daemon, pipe mode).
/// 文本级投影（Phase A1 前格式）：仅 user/assistant 文本，工具历史不进模型上下文。
/// 已被 [`events_to_messages`] 取代（完整投影含工具调用/结果）；
/// 保留作兼容 API（export/import 与旧工具）。
#[allow(dead_code)]
pub fn turns_to_messages(turns: Vec<Turn>) -> Vec<crate::llm::ChatMessage> {
    turns
        .into_iter()
        .map(|t| match t.role.as_str() {
            "assistant" => crate::llm::ChatMessage::assistant(t.text),
            _ => crate::llm::ChatMessage::user(t.text),
        })
        .collect()
}

/// Compact timestamp: YYYYMMDD-HHMMSS-nnnnnnnnn (nanosecond precision
/// so sessions started in the same second never collide).
pub fn chrono_like_timestamp() -> String {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default();
    let secs = now.as_secs();
    let days = secs / 86_400;
    let rem = secs % 86_400;
    let (h, m, s) = (rem / 3600, (rem % 3600) / 60, rem % 60);
    // Days since 1970-01-01 → approximate date (ignores leap years' drift
    // beyond 2026; good enough for file naming).
    let y = 1970 + days / 365;
    format!("{y}-{days:05}-{h:02}{m:02}{s:02}-{:09}", now.subsec_nanos())
}

/// Portable session archive: metadata + the full transcript. Written by
/// `export_session`, read by `import_session`. The turns array is what
/// `--resume` replays.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct SessionArchive {
    /// Sigil version that produced the export.
    pub version: String,
    /// Export timestamp.
    pub exported_at: String,
    /// Original session id (informational).
    pub source_id: String,
    /// Transcript.
    pub turns: Vec<Turn>,
}

/// Export a session to a portable JSON archive file.
///
/// `since` optionally filters the transcript to turns written at or after a
/// cutoff (same `chrono_like_timestamp` format as the `ts` field; legacy
/// turns without a `ts` are always kept). Returns the archive.
pub fn export_session(
    sessions_dir: &Path,
    id: &str,
    out_path: &Path,
    since: Option<&str>,
) -> std::io::Result<SessionArchive> {
    let mut turns = load(sessions_dir, id)?;
    if turns.is_empty() {
        return Err(std::io::Error::new(
            std::io::ErrorKind::NotFound,
            format!("session {id} not found or empty"),
        ));
    }
    if let Some(cutoff) = since {
        turns.retain(|t| match &t.ts {
            // Untimestamped legacy turns can't be dated — keep them.
            None => true,
            Some(ts) => ts.as_str() >= cutoff,
        });
        if turns.is_empty() {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                format!("session {id} has no turns at or after {cutoff}"),
            ));
        }
    }
    let archive = SessionArchive {
        version: env!("CARGO_PKG_VERSION").to_string(),
        exported_at: chrono_like_timestamp(),
        source_id: id.to_string(),
        turns,
    };
    let json = serde_json::to_string_pretty(&archive)?;
    // Atomic write: write to a temp file in the same directory, then
    // rename over the target. A crash mid-write leaves the old file
    // intact instead of a truncated export (and the temp file is
    // cleaned up on error).
    if let Some(parent) = out_path.parent() {
        fs::create_dir_all(parent)?;
    }
    let tmp = out_path.with_extension("json.tmp");
    fs::write(&tmp, json)?;
    if let Err(e) = fs::rename(&tmp, out_path) {
        let _ = fs::remove_file(&tmp);
        return Err(e);
    }
    Ok(archive)
}

/// Import a session archive into the sessions directory as a **new**
/// session (so the original is never overwritten). Returns the new id.
pub fn import_session(sessions_dir: &Path, in_path: &Path) -> std::io::Result<String> {
    let json = fs::read_to_string(in_path)?;
    let archive: SessionArchive = serde_json::from_str(&json).map_err(std::io::Error::other)?;
    if archive.turns.is_empty() {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            "archive has no turns",
        ));
    }
    let id = start(sessions_dir)?;
    for turn in &archive.turns {
        append(sessions_dir, &id, turn)?;
    }
    Ok(id)
}

#[cfg(test)]
mod usage_event_tests {
    use super::*;
    #[test]
    fn request_usage_roundtrips_without_becoming_model_context() {
        let raw = serde_json::json!({"e":"request_usage", "model":"m", "request_kind":"stream",
            "usage":{"prompt_tokens":100, "completion_tokens":5,
            "prompt_cache_hit_tokens":0,"cache_reported_prompt_tokens":100}});
        let event: Event = serde_json::from_value(raw.clone()).expect("usage is session metadata");
        assert_eq!(event.kind(), "request_usage");
        assert!(events_to_messages(std::slice::from_ref(&event)).is_empty());
        assert!(events_to_turns(std::slice::from_ref(&event)).is_empty());
        assert_eq!(serde_json::to_value(&event).unwrap()["usage"], raw["usage"]);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // ---- Cordis P3: revert_key (per-call inverse identifier) ----

    #[test]
    fn revert_key_is_stable_and_canonicalized() {
        // Same logical call → same key, even if the args object is built
        // with a different key insertion order (canonical = key-sorted).
        let a = serde_json::json!({"path": "/tmp/x", "mode": "read"});
        let b = serde_json::json!({"mode": "read", "path": "/tmp/x"});
        assert_eq!(revert_key("read_file", &a), revert_key("read_file", &b));

        // Different args → different key.
        let c = serde_json::json!({"path": "/tmp/y", "mode": "read"});
        assert_ne!(revert_key("read_file", &a), revert_key("read_file", &c));

        // Different tool name → different key (even identical args).
        assert_ne!(revert_key("read_file", &a), revert_key("write_file", &a));

        // A non-object arg (array) is canonicalized too.
        let arr1 = serde_json::json!([1, 2, 3]);
        assert_eq!(
            revert_key("t", &arr1),
            revert_key("t", &serde_json::json!([1, 2, 3]))
        );
        assert_ne!(
            revert_key("t", &arr1),
            revert_key("t", &serde_json::json!([1, 3, 2]))
        );
    }

    #[test]
    fn revert_key_is_hex_16() {
        let k = revert_key("bash", &serde_json::json!({"command": "ls"}));
        assert_eq!(k.len(), 16, "key is a 16-hex-char fingerprint: {k}");
        assert!(k.chars().all(|c| c.is_ascii_hexdigit()), "key is hex: {k}");
    }

    #[test]
    fn revert_key_matches_args_string_roundtrip() {
        // The session log stores args as a *string* (call.arguments.to_string());
        // the revert_key is computed from the Value. Parsing the stored string
        // back to a Value must reproduce the same key — so a log replay can
        // recompute the key from the persisted `args` field.
        let v = serde_json::json!({"b": 2, "a": 1, "nested": {"y": 1, "x": 2}});
        let stored = v.to_string();
        let reparsed: serde_json::Value = serde_json::from_str(&stored).unwrap();
        assert_eq!(revert_key("t", &v), revert_key("t", &reparsed));
    }

    #[test]
    fn revertible_field_roundtrips_and_defaults() {
        // A ToolResult with revertible=Some(true) serializes and deserializes
        // with the flag intact.
        let ev = Event::ToolResult {
            ts: None,
            id: "c".into(),
            name: "improve_harness".into(),
            ok: true,
            output: "ok".into(),
            revertible: Some(true),
        };
        let line = serde_json::to_string(&ev).unwrap();
        assert!(line.contains("\"revertible\":true"), "serialized: {line}");
        let back: Event = serde_json::from_str(&line).unwrap();
        if let Event::ToolResult { revertible, .. } = back {
            assert_eq!(revertible, Some(true));
        } else {
            panic!("expected ToolResult");
        }

        // A legacy line (no revertible key) deserializes with None (default).
        let legacy = r#"{"e":"tool_result","id":"c","name":"bash","ok":true,"output":"x"}"#;
        let back: Event = serde_json::from_str(legacy).unwrap();
        if let Event::ToolResult { revertible, .. } = back {
            assert_eq!(revertible, None, "legacy line → revertible None");
        } else {
            panic!("expected ToolResult");
        }
        // A legacy ToolCall line (no revert_key) deserializes with "".
        let legacy_call = r#"{"e":"tool_call","id":"c","name":"bash","args":"{}"}"#;
        let back: Event = serde_json::from_str(legacy_call).unwrap();
        if let Event::ToolCall { revert_key, .. } = back {
            assert_eq!(revert_key, "");
        } else {
            panic!("expected ToolCall");
        }
    }

    #[test]
    fn roundtrip_append_and_load() {
        let dir = std::env::temp_dir().join(format!("sigil-session-test-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();

        let id = start(&dir).unwrap();
        append(
            &dir,
            &id,
            &Turn {
                role: "user".into(),
                text: "hello".into(),
                ts: None,
            },
        )
        .unwrap();
        append(
            &dir,
            &id,
            &Turn {
                role: "assistant".into(),
                text: "hi there".into(),
                ts: None,
            },
        )
        .unwrap();

        let turns = load(&dir, &id).unwrap();
        assert_eq!(turns.len(), 2);
        assert_eq!(turns[0].role, "user");
        assert_eq!(turns[1].text, "hi there");

        // Missing session → empty.
        assert!(load(&dir, "nope").unwrap().is_empty());

        // List includes it.
        assert!(list(&dir).unwrap().contains(&id));

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn event_tool_with_id_roundtrips() {
        let dir = std::env::temp_dir().join(format!("sigil-session-toolid-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();

        let id = start(&dir).unwrap();
        append_event(
            &dir,
            &id,
            &Event::ToolCall {
                ts: None,
                id: "call_1".into(),
                name: "read_file".into(),
                args: r#"{"path":"/tmp/a.txt"}"#.into(),
                revert_key: "".into(),
            },
        )
        .unwrap();
        append_event(
            &dir,
            &id,
            &Event::ToolResult {
                ts: None,
                id: "call_1".into(),
                name: "read_file".into(),
                ok: true,
                output: "content-a".into(),
                revertible: None,
            },
        )
        .unwrap();

        let events = load_events(&dir, &id).unwrap();
        assert_eq!(events.len(), 2);
        match &events[0] {
            Event::ToolCall { id, name, .. } => {
                assert_eq!(id, "call_1");
                assert_eq!(name, "read_file");
            }
            other => panic!("expected ToolCall, got {other:?}"),
        }
        match &events[1] {
            Event::ToolResult { id, ok, output, .. } => {
                assert_eq!(id, "call_1");
                assert!(*ok);
                assert_eq!(output, "content-a");
            }
            other => panic!("expected ToolResult, got {other:?}"),
        }

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn event_legacy_tool_line_without_id_parses() {
        // Phase A 之前的旧 JSONL 行没有 `id` 字段 —— serde default 必须兼容。
        let line =
            r#"{"e":"tool_call","name":"read_file","args":"{}","ts":"2026-08-01T00:00:00Z"}"#;
        let ev: Event = serde_json::from_str(line).unwrap();
        match ev {
            Event::ToolCall { id, name, .. } => {
                assert_eq!(id, ""); // 旧行 → 空 id
                assert_eq!(name, "read_file");
            }
            other => panic!("expected ToolCall, got {other:?}"),
        }
    }

    #[test]
    fn events_to_messages_projects_full_tool_history() {
        use crate::llm::Role;

        let events = vec![
            Event::User {
                ts: None,
                text: "读文件".into(),
            },
            Event::TurnStart { ts: None },
            Event::Assistant {
                ts: None,
                text: "我来读".into(),
            },
            Event::ToolCall {
                ts: None,
                id: "call_1".into(),
                name: "read_file".into(),
                args: r#"{"path":"/tmp/a.txt"}"#.into(),
                revert_key: "".into(),
            },
            Event::ToolCall {
                ts: None,
                id: "call_2".into(),
                name: "read_file".into(),
                args: r#"{"path":"/tmp/b.txt"}"#.into(),
                revert_key: "".into(),
            },
            Event::ToolResult {
                ts: None,
                id: "call_1".into(),
                name: "read_file".into(),
                ok: true,
                output: "content-a".into(),
                revertible: None,
            },
            Event::ToolResult {
                ts: None,
                id: "call_2".into(),
                name: "read_file".into(),
                ok: true,
                output: "content-b".into(),
                revertible: None,
            },
            Event::TurnEnd { ts: None },
        ];

        let messages = events_to_messages(&events);

        // user → assistant(文本 + 2 个 tool_calls 合并) → tool(call_1) → tool(call_2)
        assert_eq!(messages.len(), 4);
        assert_eq!(messages[0].role, Role::User);
        assert_eq!(messages[0].content, "读文件");

        assert_eq!(messages[1].role, Role::Assistant);
        assert_eq!(messages[1].content, "我来读");
        let calls = messages[1].tool_calls.as_ref().expect("tool_calls");
        assert_eq!(calls.len(), 2);
        assert_eq!(calls[0].id, "call_1");
        assert_eq!(calls[1].id, "call_2");
        assert_eq!(calls[0].name, "read_file");

        assert_eq!(messages[2].role, Role::Tool);
        assert_eq!(messages[2].tool_call_id.as_deref(), Some("call_1"));
        assert_eq!(messages[2].content, "content-a");

        assert_eq!(messages[3].role, Role::Tool);
        assert_eq!(messages[3].tool_call_id.as_deref(), Some("call_2"));
        assert_eq!(messages[3].content, "content-b");
    }

    #[test]
    fn events_to_messages_ignores_structural_and_metadata() {
        use crate::llm::Role;

        let events = vec![
            Event::TurnStart { ts: None },
            Event::StepStart { ts: None },
            Event::User {
                ts: None,
                text: "hi".into(),
            },
            Event::RequestHeader {
                ts: None,
                model: "deepseek-v4-flash".into(),
                temperature: None,
                max_tokens: None,
                tools_fingerprint: None,
            },
            Event::RequestContext {
                ts: None,
                values: vec![("pinnedWorkflowPath".into(), "/wf/a.npp".into())],
            },
            Event::StepEnd { ts: None },
            Event::TurnEnd { ts: None },
        ];

        let messages = events_to_messages(&events);

        assert_eq!(messages.len(), 1);
        assert_eq!(messages[0].role, Role::User);
        assert_eq!(messages[0].content, "hi");
    }

    #[test]
    fn request_context_events_preserve_order() {
        let dir = std::env::temp_dir().join(format!("sigil-session-ctx-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();

        let id = start(&dir).unwrap();
        append_event(
            &dir,
            &id,
            &Event::RequestContext {
                ts: None,
                values: vec![
                    ("pinnedWorkflowPath".into(), "/wf/a.npp".into()),
                    ("mode".into(), "workflow".into()),
                ],
            },
        )
        .unwrap();

        let events = load_events(&dir, &id).unwrap();
        match &events[0] {
            Event::RequestContext { values, .. } => {
                assert_eq!(values.len(), 2);
                assert_eq!(values[0], ("pinnedWorkflowPath".into(), "/wf/a.npp".into()));
                assert_eq!(values[1], ("mode".into(), "workflow".into()));
            }
            other => panic!("expected RequestContext, got {other:?}"),
        }

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn repair_unpaired_tool_calls_appends_synthetic_results() {
        let dir = std::env::temp_dir().join(format!("sigil-session-repair-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();

        let id = start(&dir).unwrap();
        append_event(
            &dir,
            &id,
            &Event::User {
                ts: None,
                text: "hi".into(),
            },
        )
        .unwrap();
        append_event(
            &dir,
            &id,
            &Event::ToolCall {
                ts: None,
                id: "call_1".into(),
                name: "read_file".into(),
                args: "{}".into(),
                revert_key: "".into(),
            },
        )
        .unwrap();

        // 修复前：事件投影出的消息配对不完整（半完成 step）
        let events = load_events(&dir, &id).unwrap();
        assert!(
            crate::agent::invariant::check_tool_pairing(&events_to_messages(&events)).is_err(),
            "修复前应检测到未配对工具调用"
        );

        let repaired = repair_unpaired_tool_calls(&dir, &id).unwrap();
        assert_eq!(repaired, 1);

        let events = load_events(&dir, &id).unwrap();
        assert!(
            crate::agent::invariant::check_tool_pairing(&events_to_messages(&events)).is_ok(),
            "修复后配对必须完整"
        );

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn append_stamps_missing_timestamp() {
        let dir = std::env::temp_dir().join(format!("sigil-session-stamp-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();

        let id = start(&dir).unwrap();
        append(
            &dir,
            &id,
            &Turn {
                role: "user".into(),
                text: "hi".into(),
                ts: None,
            },
        )
        .unwrap();
        append(
            &dir,
            &id,
            &Turn {
                role: "assistant".into(),
                text: "yo".into(),
                ts: Some("2026-12345-120000-000000000".into()),
            },
        )
        .unwrap();

        let turns = load(&dir, &id).unwrap();
        // None → auto-stamped; explicit ts preserved.
        assert!(turns[0].ts.is_some());
        assert_eq!(turns[1].ts.as_deref(), Some("2026-12345-120000-000000000"));

        // Legacy line without ts still parses (appended, not overwrite).
        let legacy = dir.join(format!("{id}.jsonl"));
        fs::OpenOptions::new()
            .append(true)
            .open(&legacy)
            .and_then(|mut f| {
                use std::io::Write;
                writeln!(f, "{{\"role\":\"user\",\"text\":\"old\"}}")
            })
            .unwrap();
        let turns = load(&dir, &id).unwrap();
        assert_eq!(turns.len(), 3);
        assert!(turns[2].ts.is_none());

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn event_log_roundtrip() {
        let dir = std::env::temp_dir().join(format!("sigil-session-events-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();

        let id = start(&dir).unwrap();
        append_event(
            &dir,
            &id,
            &Event::User {
                ts: None,
                text: "list files".into(),
            },
        )
        .unwrap();
        append_event(
            &dir,
            &id,
            &Event::ToolCall {
                ts: None,
                id: "call_1".into(),
                name: "bash".into(),
                args: "ls -la".into(),
                revert_key: "".into(),
            },
        )
        .unwrap();
        append_event(
            &dir,
            &id,
            &Event::ToolResult {
                ts: None,
                id: "call_1".into(),
                name: "bash".into(),
                ok: true,
                output: "total 8".into(),
                revertible: None,
            },
        )
        .unwrap();
        append_event(
            &dir,
            &id,
            &Event::Assistant {
                ts: None,
                text: "done".into(),
            },
        )
        .unwrap();

        let events = load_events(&dir, &id).unwrap();
        assert_eq!(events.len(), 4);
        assert_eq!(events[0].kind(), "user");
        assert_eq!(events[1].kind(), "tool_call");
        assert_eq!(events[2].kind(), "tool_result");
        assert_eq!(events[3].kind(), "assistant");
        // All stamped.
        assert!(events.iter().all(|e| e.ts().is_some()));

        // Turn projection drops tool events.
        let turns = load(&dir, &id).unwrap();
        assert_eq!(turns.len(), 2);
        assert_eq!(turns[0].text, "list files");
        assert_eq!(turns[1].text, "done");

        // Raw file uses the event format (no role field).
        let raw = fs::read_to_string(dir.join(format!("{id}.jsonl"))).unwrap();
        assert!(raw.contains("\"e\":\"user\""));
        assert!(!raw.contains("\"role\""));

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn legacy_lines_project_into_events() {
        let dir = std::env::temp_dir().join(format!("sigil-session-legacy-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();

        let id = start(&dir).unwrap();
        // Write legacy + event lines interleaved by hand.
        let path = dir.join(format!("{id}.jsonl"));
        fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&path)
            .and_then(|mut f| {
                use std::io::Write;
                writeln!(f, "{{\"role\":\"user\",\"text\":\"legacy q\"}}")?;
                writeln!(f, "{{\"e\":\"assistant\",\"ts\":\"2026-22000-100000-000000000\",\"text\":\"event a\"}}")?;
                writeln!(f, "{{\"role\":\"assistant\",\"text\":\"legacy a\",\"ts\":\"2026-22000-110000-000000000\"}}")
            })
            .unwrap();

        let events = load_events(&dir, &id).unwrap();
        assert_eq!(events.len(), 3);
        assert_eq!(events[0].kind(), "user");
        assert_eq!(events[0].ts(), None);
        assert_eq!(events[1].kind(), "assistant");
        assert_eq!(events[1].ts(), Some("2026-22000-100000-000000000"));
        assert_eq!(events[2].kind(), "assistant");
        assert_eq!(events[2].ts(), Some("2026-22000-110000-000000000"));

        let turns = load(&dir, &id).unwrap();
        assert_eq!(turns.len(), 3);
        assert_eq!(turns[2].text, "legacy a");

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn event_summary_and_kind() {
        let ev = Event::ToolCall {
            ts: None,
            id: "call_1".into(),
            name: "create_lua_tool".into(),
            args: "{\"name\":\"x\"}".into(),
            revert_key: "".into(),
        };
        assert_eq!(ev.kind(), "tool_call");
        let s = ev.summary();
        assert!(s.contains("create_lua_tool"));
        assert!(s.contains("tool_call"));
    }

    #[test]
    fn export_import_roundtrip() {
        let dir = std::env::temp_dir().join(format!("sigil-session-xport-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();

        let id = start(&dir).unwrap();
        append(
            &dir,
            &id,
            &Turn {
                role: "user".into(),
                text: "hello".into(),
                ts: None,
            },
        )
        .unwrap();
        append(
            &dir,
            &id,
            &Turn {
                role: "assistant".into(),
                text: "hi".into(),
                ts: None,
            },
        )
        .unwrap();

        // Export to a portable JSON file.
        let out = dir.join("archive.json");
        let archive = export_session(&dir, &id, &out, None).unwrap();
        assert_eq!(archive.turns.len(), 2);
        assert_eq!(archive.source_id, id);
        assert!(out.exists());
        // Atomic write leaves no temp file behind.
        assert!(!dir.join("archive.json.tmp").exists(), "tmp file leaked");

        // Import into the same dir → NEW session id, same turns.
        let new_id = import_session(&dir, &out).unwrap();
        assert_ne!(new_id, id);
        let turns = load(&dir, &new_id).unwrap();
        assert_eq!(turns.len(), 2);
        assert_eq!(turns[0].text, "hello");
        assert_eq!(turns[1].text, "hi");

        // Exporting a missing session fails cleanly.
        assert!(export_session(&dir, "nope", &dir.join("x.json"), None).is_err());

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn export_since_filters_turns() {
        let dir = std::env::temp_dir().join(format!("sigil-session-since-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();

        let id = start(&dir).unwrap();
        // Explicit timestamps (chrono_like format, lexicographic = chronological).
        append(
            &dir,
            &id,
            &Turn {
                role: "user".into(),
                text: "early".into(),
                ts: Some("2026-22000-100000-000000000".into()),
            },
        )
        .unwrap();
        append(
            &dir,
            &id,
            &Turn {
                role: "assistant".into(),
                text: "late".into(),
                ts: Some("2026-22000-120000-000000000".into()),
            },
        )
        .unwrap();
        // Legacy turn with no ts — always kept.
        let path = dir.join(format!("{id}.jsonl"));
        fs::OpenOptions::new()
            .append(true)
            .open(&path)
            .and_then(|mut f| {
                use std::io::Write;
                writeln!(f, "{{\"role\":\"user\",\"text\":\"legacy\"}}")
            })
            .unwrap();

        // Cutoff between the two timestamps → only the late + legacy turns.
        let out = dir.join("since.json");
        let archive = export_session(&dir, &id, &out, Some("2026-22000-110000-000000000")).unwrap();
        let texts: Vec<&str> = archive.turns.iter().map(|t| t.text.as_str()).collect();
        assert_eq!(texts, vec!["late", "legacy"]);

        // Cutoff after everything → legacy turn is still kept (undated
        // turns are never dropped), so the export still succeeds.
        let archive = export_session(&dir, &id, &out, Some("2026-22000-130000-000000000")).unwrap();
        let texts: Vec<&str> = archive.turns.iter().map(|t| t.text.as_str()).collect();
        assert_eq!(texts, vec!["legacy"]);

        // A fully-timestamped session with nothing at/after the cutoff
        // fails cleanly instead of producing an empty archive.
        let id2 = start(&dir).unwrap();
        append(
            &dir,
            &id2,
            &Turn {
                role: "user".into(),
                text: "only early".into(),
                ts: Some("2026-22000-090000-000000000".into()),
            },
        )
        .unwrap();
        let out2 = dir.join("since2.json");
        assert!(export_session(&dir, &id2, &out2, Some("2026-22000-130000-000000000")).is_err());

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn summaries_order_by_last_activity_and_preview() {
        let dir = std::env::temp_dir().join(format!("sigil-summary-test-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();

        // Session A: older last turn.
        let a = start(&dir).unwrap();
        append(
            &dir,
            &a,
            &Turn {
                role: "user".into(),
                text: "first question about lua".into(),
                ts: Some("2026-22000-090000-000000000".into()),
            },
        )
        .unwrap();
        append(
            &dir,
            &a,
            &Turn {
                role: "assistant".into(),
                text: "answer a".into(),
                ts: Some("2026-22000-100000-000000000".into()),
            },
        )
        .unwrap();

        // Session B: newer last turn.
        let b = start(&dir).unwrap();
        append(
            &dir,
            &b,
            &Turn {
                role: "user".into(),
                text: "second question".into(),
                ts: Some("2026-22000-110000-000000000".into()),
            },
        )
        .unwrap();
        append(
            &dir,
            &b,
            &Turn {
                role: "assistant".into(),
                text: "answer b".into(),
                ts: Some("2026-22000-120000-000000000".into()),
            },
        )
        .unwrap();

        let s = summaries(&dir).unwrap();
        assert_eq!(s.len(), 2);
        // Newest last-activity first.
        assert_eq!(s[0].id, b);
        assert_eq!(s[1].id, a);
        // Turn counts.
        assert_eq!(s[0].turns, 2);
        // Preview = first user turn, whitespace-flattened, truncated.
        assert_eq!(s[1].preview, "first question about lua");
        // latest() = the newest.
        assert_eq!(latest(&dir).unwrap().as_deref(), Some(b.as_str()));

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn summaries_fallback_to_id_order_for_legacy_sessions() {
        // Sessions whose turns all predate Phase 31 have no `ts` (write
        // the JSONL by hand — `append` auto-stamps); they still appear,
        // sorted by id (chronological for legacy files).
        let dir = std::env::temp_dir().join(format!("sigil-summary-legacy-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();

        let older = start(&dir).unwrap();
        let older_path = dir.join(format!("{older}.jsonl"));
        fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&older_path)
            .and_then(|mut f| {
                use std::io::Write;
                writeln!(f, "{{\"role\":\"user\",\"text\":\"legacy one\"}}")
            })
            .unwrap();
        let newer = start(&dir).unwrap();
        let newer_path = dir.join(format!("{newer}.jsonl"));
        fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&newer_path)
            .and_then(|mut f| {
                use std::io::Write;
                writeln!(f, "{{\"role\":\"user\",\"text\":\"legacy two\"}}")
            })
            .unwrap();

        let s = summaries(&dir).unwrap();
        assert_eq!(s.len(), 2);
        assert_eq!(s[0].id, newer);
        assert_eq!(s[0].preview, "legacy two");
        assert_eq!(s[0].last_ts, None);

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn summaries_empty_dir() {
        let dir = std::env::temp_dir().join(format!("sigil-summary-empty-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        assert!(summaries(&dir).unwrap().is_empty());
        assert!(latest(&dir).unwrap().is_none());
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn approval_event_roundtrip() {
        // Audit events write and read back with all fields intact.
        let dir = std::env::temp_dir().join(format!("sigil-approval-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();

        let id = start(&dir).unwrap();
        append_event(
            &dir,
            &id,
            &Event::Approval {
                ts: None,
                name: "bash".into(),
                args: "rm -rf /tmp/x".into(),
                decision: "rejected".into(),
                reason: Some("destructive".into()),
            },
        )
        .unwrap();

        let events = load_events(&dir, &id).unwrap();
        assert_eq!(events.len(), 1);
        match &events[0] {
            Event::Approval {
                name,
                args,
                decision,
                reason,
                ..
            } => {
                assert_eq!(name, "bash");
                assert_eq!(args, "rm -rf /tmp/x");
                assert_eq!(decision, "rejected");
                assert_eq!(reason.as_deref(), Some("destructive"));
                assert!(events[0].ts().is_some(), "ts auto-stamped");
            }
            other => panic!("expected Approval, got {other:?}"),
        }

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn audit_events_not_in_turns() {
        // Approval/Policy/Hook stay in the raw log but never project
        // into the turn view (model context) — the dsh "audit-only"
        // principle.
        let dir = std::env::temp_dir().join(format!("sigil-audit-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();

        let id = start(&dir).unwrap();
        for ev in [
            Event::User {
                ts: None,
                text: "hi".into(),
            },
            Event::Approval {
                ts: None,
                name: "bash".into(),
                args: "rm x".into(),
                decision: "auto_rejected".into(),
                reason: None,
            },
            Event::Policy {
                ts: None,
                name: "bash".into(),
                level: "dangerous".into(),
                verdict: "rejected".into(),
            },
            Event::Hook {
                ts: None,
                name: "on_tool_result".into(),
                outcome: "ran".into(),
                detail: None,
            },
            Event::Assistant {
                ts: None,
                text: "denied".into(),
            },
        ] {
            append_event(&dir, &id, &ev).unwrap();
        }

        let events = load_events(&dir, &id).unwrap();
        assert_eq!(events.len(), 5, "all events preserved in raw log");

        let turns = load(&dir, &id).unwrap();
        assert_eq!(turns.len(), 2, "audit events filtered from turns");
        assert_eq!(turns[0].role, "user");
        assert_eq!(turns[1].role, "assistant");

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn audit_summary_lines() {
        // summary() renders audit events without leaking full args.
        let a = Event::Approval {
            ts: None,
            name: "bash".into(),
            args: "rm -rf /secret/path".into(),
            decision: "approved".into(),
            reason: None,
        };
        assert_eq!(a.summary(), "approval: bash → approved");
        assert!(
            !a.summary().contains("secret"),
            "args not leaked to summary"
        );

        let p = Event::Policy {
            ts: None,
            name: "bash".into(),
            level: "dangerous".into(),
            verdict: "rejected".into(),
        };
        assert_eq!(p.summary(), "policy: bash [dangerous] → rejected");

        let h = Event::Hook {
            ts: None,
            name: "on_tool_result".into(),
            outcome: "error".into(),
            detail: Some("bad lua".into()),
        };
        assert_eq!(h.summary(), "hook: on_tool_result error");
        assert!(
            !h.summary().contains("bad lua"),
            "detail not leaked to summary"
        );
    }
}
