//! The agent loop: build messages → stream completion → parse tool calls →
//! execute tools → repeat until the model is done or `max_steps` is hit.
//!
//! Tool-call flow (native OpenAI function-calling):
//!   1. Stream a completion.
//!   2. If a chunk signals `finish_reason == "tool_calls"`, the streamed
//!      delta carries no call payload — replay the request non-streaming
//!      to get the full `message.tool_calls` (verified against
//!      deepseek-v4-flash via cc-switch proxy, 2026-08-09).
//!   3. Execute every call, append results as `tool` messages, loop.
//!   4. Otherwise the streamed text is the final answer.

use anyhow::{Context, Result};
use futures_util::StreamExt;

use crate::llm::provider::{ChatComplete, ChatMessage, ChatRequest, LlmProvider, Role};
use crate::safety;
use crate::tools::ToolRegistry;

#[cfg(test)]
mod tests;

/// Agent loop configuration.
#[derive(Debug, Clone)]
pub struct AgentConfig {
    pub model: String,
    /// 0 = no step limit (a turn runs until the model stops calling tools).
    pub max_steps: u32,
    /// Live prompt state (system prompt + skills). Shared with the
    /// evolution tools so `improve_harness` / `skill_rollback` take
    /// effect on the very next turn — no restart needed.
    pub prompt_state: std::sync::Arc<std::sync::RwLock<crate::harness::PromptState>>,
    /// Max skills inlined per turn (relevance-ranked). Ignored when
    /// the state has no skills.
    pub max_inline_skills: usize,
    /// Optional harness dir for cross-instance sync: each loop
    /// iteration reloads the prompt state when the dir changed on disk
    /// (another sigil process edited it). `None` = no sync.
    pub sync_dir: Option<std::path::PathBuf>,
    /// Max messages kept verbatim in the transcript per turn. When the
    /// history exceeds this, the oldest turns are folded into a single
    /// compact `system` summary message (deterministic, no LLM call) so
    /// long REPL sessions don't grow the context without bound.
    /// `0` disables compression.
    pub max_context_messages: usize,
    /// Token budget for the transcript per turn (chars/4 heuristic,
    /// same shape as the context-window-based budgets used by
    /// prime-agent's compaction). When the estimated transcript size
    /// exceeds this, the oldest turns are folded like
    /// `max_context_messages`. `0` disables the token budget.
    pub max_context_tokens: usize,
    /// Persistent goals shared with the `manage_goals` tool. Active
    /// goals are injected into the system context each turn until
    /// completed. `None` disables the goals feature.
    pub goals: Option<std::sync::Arc<std::sync::RwLock<Vec<crate::goals::GoalState>>>>,
}

impl AgentConfig {
    /// System prompt for a specific user prompt: base prompt plus the
    /// most relevant skills inlined (keyword-overlap ranked), plus a
    /// compact index of every skill so the model knows what else is
    /// available and can load any body on demand. Reads the *live*
    /// state, so harness edits apply immediately.
    pub fn system_prompt_for(&self, prompt: &str) -> String {
        let state = self.prompt_state.read().unwrap_or_else(|e| e.into_inner());
        if state.skills.is_empty() || self.max_inline_skills == 0 {
            state.system_prompt.clone()
        } else {
            let mut p = state.system_prompt.clone();
            p.push_str(&crate::harness::Harness::skills_index_only_from(
                &state.skills,
            ));
            p.push_str(&crate::harness::inline_skills_for(
                prompt,
                &state.skills,
                self.max_inline_skills,
            ));
            p
        }
    }
}

/// Fold the oldest turns of a transcript into one compact summary
/// message so long sessions don't grow the context without bound.
///
/// Deterministic — no LLM call, no sampling variance. Every trimmed
/// user/assistant/tool message contributes a role-tagged one-line
/// excerpt (first line, truncated to 120 chars); tool results are
/// reduced to their first line too. The system prompt (`messages[0]`)
/// and the most recent `max - 2` messages stay verbatim, so the model
/// still sees the current task in full fidelity.
///
/// Cutting happens only at a "safe" boundary — a `User` message or an
/// `Assistant` message *without* tool calls — so the kept tail never
/// starts mid-turn and never orphans a `tool` result from its
/// `assistant` tool-call message (which OpenAI rejects).
///
/// Returns the transcript unchanged when `max == 0` or the length is
/// already within budget.
#[cfg(test)]
pub fn compact_messages(messages: &[ChatMessage], max: usize) -> Vec<ChatMessage> {
    compact_messages_with_budget(messages, max, 0)
}

/// Rough token estimate for a text (chars/4, conservative).
pub fn estimate_tokens(text: &str) -> usize {
    (text.chars().count() / 4).max(1)
}

/// Token estimate for one message, including tool-call payloads.
pub fn estimate_message_tokens(m: &ChatMessage) -> usize {
    let mut t = estimate_tokens(&m.content);
    if let Some(calls) = &m.tool_calls {
        for c in calls {
            t += estimate_tokens(&c.name);
            t += estimate_tokens(&c.arguments.to_string());
        }
    }
    t
}

/// Extract `read_file` / `write_file` paths from assistant tool calls
/// in a message slice, deduplicated in order of first use. Kept so the
/// compaction summary can tell the model which files it touched — the
/// one thing a naive excerpt fold loses (prime-agent tracks the same
/// via `CompactionEntry.details`).
fn extract_file_ops(messages: &[ChatMessage]) -> (Vec<String>, Vec<String>) {
    let mut read: Vec<String> = Vec::new();
    let mut modified: Vec<String> = Vec::new();
    for m in messages {
        let Some(calls) = &m.tool_calls else { continue };
        for c in calls {
            let Some(path) = c.arguments.get("path").and_then(serde_json::Value::as_str) else {
                continue;
            };
            let path = path.to_string();
            match c.name.as_str() {
                "read_file" if !read.contains(&path) => read.push(path),
                "write_file" if !modified.contains(&path) => modified.push(path),
                _ => {}
            }
        }
    }
    (read, modified)
}

/// [`compact_messages`] with a token budget: compress when the
/// estimated transcript size exceeds `max_tokens` (chars/4 heuristic)
/// OR the message count exceeds `max_msgs`. Either budget may be `0`
/// to disable that axis. The kept tail is chosen by walking backwards
/// from the newest message, accumulating tokens until the token budget
/// (or message budget) is met, then extended forward to the nearest
/// safe cut boundary.
pub fn compact_messages_with_budget(
    messages: &[ChatMessage],
    max_msgs: usize,
    max_tokens: usize,
) -> Vec<ChatMessage> {
    // No budget configured → never compress.
    if max_msgs == 0 && max_tokens == 0 {
        return messages.to_vec();
    }
    if messages.len() < 3 {
        return messages.to_vec();
    }
    // Already within every configured budget?
    let total_tokens: usize = messages.iter().map(estimate_message_tokens).sum();
    let within_msgs = max_msgs == 0 || messages.len() <= max_msgs;
    let within_tokens = max_tokens == 0 || total_tokens <= max_tokens;
    if within_msgs && within_tokens {
        return messages.to_vec();
    }

    // Walk backwards, accumulating token estimates, until the token
    // budget or the message budget (minus the summary slot) is met.
    let keep_count = max_msgs.saturating_sub(2).max(2);
    let mut cut = 1usize;
    let mut acc = 0usize;
    for i in (1..messages.len()).rev() {
        acc += estimate_message_tokens(&messages[i]);
        let count_over = messages.len() - i >= keep_count;
        let tokens_over = max_tokens > 0 && acc > max_tokens;
        if count_over || tokens_over {
            cut = i;
            break;
        }
    }
    if cut <= 1 {
        // Nothing safe to trim — keep everything.
        return messages.to_vec();
    }
    // Extend forward to the first safe boundary at-or-after `cut`.
    while cut < messages.len() {
        let safe = matches!(messages[cut].role, Role::User)
            || (matches!(messages[cut].role, Role::Assistant)
                && messages[cut].tool_calls.is_none());
        if safe {
            break;
        }
        cut += 1;
    }
    if cut <= 1 {
        // Nothing safe to trim — keep everything.
        return messages.to_vec();
    }

    // Build the summary: file operations the model touched (the most
    // valuable thing to preserve) + role-tagged one-line excerpts.
    let (files_read, files_modified) = extract_file_ops(&messages[1..cut]);
    let mut summary = String::from("[earlier conversation compressed]");
    if !files_read.is_empty() {
        summary.push_str(&format!("\nFiles read: {}", files_read.join(", ")));
    }
    if !files_modified.is_empty() {
        summary.push_str(&format!("\nFiles modified: {}", files_modified.join(", ")));
    }
    let mut excerpts: Vec<String> = Vec::new();
    for m in &messages[1..cut] {
        let first = m
            .content
            .lines()
            .next()
            .unwrap_or("")
            .chars()
            .take(120)
            .collect::<String>();
        let tag = match m.role {
            Role::System => "system",
            Role::User => "user",
            Role::Assistant => "assistant",
            Role::Tool => "tool",
        };
        excerpts.push(format!("{tag}: {first}"));
    }
    summary.push_str(&format!("\n{}", excerpts.join(" | ")));

    let mut out = Vec::with_capacity(max_msgs.max(8));
    out.push(messages[0].clone());
    out.push(ChatMessage::system(summary));
    out.extend_from_slice(&messages[cut..]);
    out
}

/// Approval callback: given a tool call that the gate rejected in
/// non-interactive mode, decide whether the human allows it anyway.
/// Returns true to run it (interactive override).
pub type Approver = dyn Fn(&str, &serde_json::Value) -> bool + Send + Sync;

/// Stream callback: called with each chunk of assistant text as it
/// arrives (UI streaming). Also called with `None` when a tool call
/// interrupts the text stream (so the UI can flush).
pub type StreamCb = dyn Fn(Option<&str>) + Send + Sync;

/// One turn of the agent loop. Returns the assistant's final text.
pub async fn run_turn(
    provider: &dyn LlmProvider,
    tools: &mut ToolRegistry,
    config: &AgentConfig,
    user_prompt: &str,
) -> Result<String> {
    let messages = vec![
        ChatMessage::system(config.system_prompt_for(user_prompt)),
        ChatMessage::user(user_prompt),
    ];
    run_messages(provider, tools, config, &messages, None, None).await
}

/// [`run_turn`] with optional interactive approver and stream callback.
pub async fn run_turn_with_approver(
    provider: &dyn LlmProvider,
    tools: &mut ToolRegistry,
    config: &AgentConfig,
    user_prompt: &str,
    approver: Option<&Approver>,
    stream_cb: Option<&StreamCb>,
) -> Result<String> {
    let messages = vec![
        ChatMessage::system(config.system_prompt_for(user_prompt)),
        ChatMessage::user(user_prompt),
    ];
    run_messages(provider, tools, config, &messages, approver, stream_cb).await
}

/// The core agent loop over a pre-built message list
/// (system prompt + any prior turns + the new user message). The REPL
/// uses this to keep true multi-turn context across turns and sessions.
pub async fn run_messages(
    provider: &dyn LlmProvider,
    tools: &mut ToolRegistry,
    config: &AgentConfig,
    messages: &[ChatMessage],
    approver: Option<&Approver>,
    stream_cb: Option<&StreamCb>,
) -> Result<String> {
    let mut messages = messages.to_vec();

    let breaker = safety::CircuitBreaker::new();
    let mut step: u32 = 0;

    loop {
        // Cross-instance sync: if another sigil process edited the
        // harness dir since our last turn, reload the prompt state so
        // this instance sees the change (cheap fingerprint when idle).
        if let Some(dir) = &config.sync_dir {
            let _ = crate::harness::sync_prompt_state(&config.prompt_state, dir);
        }

        // Context budget: fold the oldest turns into a compact summary
        // when the transcript outgrew the window (long REPL sessions).
        if config.max_context_messages > 0 || config.max_context_tokens > 0 {
            messages = compact_messages_with_budget(
                &messages,
                config.max_context_messages,
                config.max_context_tokens,
            );
        }

        // Persistent goals: inject active goals into the system context
        // so the model keeps working toward them this turn. Inserted
        // right after the first system message (the main prompt /
        // compaction summary) so it is never folded away.
        if let Some(goals) = &config.goals
            && let Ok(g) = goals.read()
            && let Some(block) = crate::goals::active_goals_block(&g)
        {
            let pos = messages
                .iter()
                .position(|m| m.role == Role::System)
                .map(|p| p + 1)
                .unwrap_or(0);
            messages.insert(pos, ChatMessage::system(block));
        }

        // Fresh specs every turn: evolution tools may have added tools
        // mid-run, and the model should see them on the next turn.
        let specs = tools.specs();

        if config.max_steps > 0 && step >= config.max_steps {
            anyhow::bail!("step limit ({}) reached", config.max_steps);
        }
        step += 1;

        let req = ChatRequest {
            model: config.model.clone(),
            messages: messages.clone(),
            tools: specs.clone(),
            stream: true,
            temperature: None,
        };

        let mut stream = provider
            .chat_stream(req.clone())
            .await
            .context("starting completion")?;

        let mut text = String::new();
        let mut tool_calls_requested = false;
        while let Some(chunk) = stream.next().await {
            let chunk = chunk.context("reading stream")?;
            if !chunk.thinking {
                text.push_str(&chunk.delta);
                if let Some(cb) = stream_cb {
                    cb(Some(&chunk.delta));
                }
            }
            tool_calls_requested |= chunk.tool_calls_requested;
            if chunk.done {
                break;
            }
        }

        if tool_calls_requested {
            // Tell the UI the text stream is interrupted by tool calls.
            if let Some(cb) = stream_cb {
                cb(None);
            }
            // Replay non-streaming to get the full tool-call payload.
            let complete: ChatComplete = provider
                .chat_complete(req)
                .await
                .context("replaying completion for tool calls")?;
            if complete.tool_calls.is_empty() {
                // Model asked for tools but sent none; treat text as answer.
                let answer = if text.trim().is_empty() {
                    complete.text
                } else {
                    text
                };
                return Ok(answer.trim().to_string());
            }
            messages.push(ChatMessage::assistant_with_tools(
                &complete.text,
                complete.tool_calls.clone(),
            ));
            for call in complete.tool_calls {
                // Approval gate: reject destructive commands in
                // non-interactive mode before they ever run. In
                // interactive mode the human may override.
                let gate = safety::check(&call.name, &call.arguments);
                let mut allowed = matches!(gate, safety::GateDecision::Allow);
                if !allowed
                    && let Some(approve) = approver
                    && approve(&call.name, &call.arguments)
                {
                    allowed = true;
                    eprintln!("⚠ approved by user: {} {}", call.name, call.arguments);
                }

                if allowed {
                    // Circuit breaker: skip tripped tools entirely.
                    if breaker.is_tripped(&call.name) {
                        let msg = safety::CircuitBreaker::tripped_message(&call.name);
                        messages.push(ChatMessage::tool_result(msg, call.id));
                        continue;
                    }
                    let result = tools.execute(&call).await?;
                    // Execution ring: flag harness mutations to the
                    // model so it knows the toolset changed.
                    let result = match safety::ring_of(&call.name) {
                        safety::Ring::Harness => {
                            format!("[harness mutation] {result}")
                        }
                        _ => result,
                    };
                    // Circuit breaker: failures trip, successes reset.
                    if result.starts_with("error:") {
                        if breaker.record_failure(&call.name) {
                            eprintln!("circuit breaker tripped: {}", call.name);
                        }
                    } else {
                        breaker.record_success(&call.name);
                    }
                    messages.push(ChatMessage::tool_result(result, call.id));
                } else {
                    let msg = safety::reject_message(&call.name, &call.arguments);
                    messages.push(ChatMessage::tool_result(msg, call.id));
                }
            }
            // Evolution: hot-register any tools the model created this
            // turn so they are callable (and advertised) next turn.
            tools.drain_hot();
            continue;
        }

        // No tool call — this is the final answer.
        return Ok(text.trim().to_string());
    }
}

#[cfg(test)]
mod compact_tests {
    use super::*;
    use crate::llm::provider::{ChatMessage, Role};

    fn msgs(n: usize) -> Vec<ChatMessage> {
        let mut v = vec![ChatMessage::system("SYS")];
        for i in 0..n {
            v.push(ChatMessage::user(format!("user-{i}")));
            v.push(ChatMessage::assistant(format!("assistant-{i}")));
        }
        v
    }

    #[test]
    fn compact_keeps_transcript_within_budget() {
        // 1 system + 20 user + 20 assistant = 41 messages.
        let m = msgs(20);
        let c = compact_messages(&m, 10);
        assert!(c.len() <= 10, "compacted len {} > 10", c.len());
        // Newest turn survives verbatim.
        assert!(c.iter().any(|m| m.content.contains("assistant-19")));
        // Old turns are folded into a summary.
        let summary = c
            .iter()
            .find(|m| m.role == Role::System && m.content.contains("compressed"));
        assert!(summary.is_some(), "missing compression summary");
        assert!(summary.unwrap().content.contains("user-0"));
    }

    #[test]
    fn compact_disabled_or_within_budget_is_noop() {
        let m = msgs(5);
        assert_eq!(compact_messages(&m, 0).len(), m.len());
        assert_eq!(compact_messages(&m, 100).len(), m.len());
    }

    #[test]
    fn compact_never_orphans_tool_results() {
        // A turn with tool calls: user → assistant(with tools) → tool.
        let m = vec![
            ChatMessage::system("SYS"),
            ChatMessage::user("task"),
            ChatMessage::assistant_with_tools(
                "call",
                vec![crate::llm::provider::ToolCall {
                    id: "c1".into(),
                    name: "bash".into(),
                    arguments: serde_json::json!({}),
                }],
            ),
            ChatMessage::tool_result("result-ok", "c1"),
            ChatMessage::user("newest-request"),
        ];
        let c = compact_messages(&m, 4);
        assert!(c.len() <= 4);
        // The kept tail must start at a safe boundary (user or
        // tool-free assistant) — never at the tool result.
        let tail = &c[2..]; // system + summary + tail
        assert!(matches!(tail[0].role, Role::User | Role::Assistant));
        assert!(!tail.iter().any(|m| m.tool_call_id.is_some()));
    }

    #[test]
    fn compact_system_prompt_stays_first() {
        let m = msgs(20);
        let c = compact_messages(&m, 8);
        assert_eq!(c[0].content, "SYS");
        assert_eq!(c[0].role, Role::System);
    }

    #[test]
    fn compact_token_budget_triggers_and_respects_budget() {
        // 1 system + 20 turns of moderately long text (~40+ chars each
        // line → ~10+ tokens/message → well over 200 total).
        let mut m = vec![ChatMessage::system("SYS")];
        for i in 0..20 {
            m.push(ChatMessage::user(format!(
                "user-{i} this line is padded out to be reasonably long for token estimation purposes"
            )));
            m.push(ChatMessage::assistant(format!(
                "assistant-{i} the reply is also padded to be long enough that the budget matters"
            )));
        }
        let total = m.iter().map(estimate_message_tokens).sum::<usize>();
        assert!(total > 200, "test transcript too small: {total}");
        // Tight token budget → compress; kept tail must fit the budget
        // plus the safe-boundary slack.
        let c = compact_messages_with_budget(&m, 0, 200);
        assert!(c.len() < m.len(), "expected compression");
        // Summary + kept tail: kept tail alone respects the budget.
        let kept_tail: usize = c[2..].iter().map(estimate_message_tokens).sum();
        assert!(
            kept_tail <= 260,
            "kept tail {kept_tail} tokens exceeds budget with slack"
        );
        // No budget configured → no-op even for a huge transcript.
        assert_eq!(compact_messages_with_budget(&m, 0, 0).len(), m.len());
    }

    #[test]
    fn compact_summary_tracks_file_operations() {
        // Old turn with read_file + write_file must surface the paths
        // in the summary so the model knows what it touched.
        let m = vec![
            ChatMessage::system("SYS"),
            ChatMessage::user("task"),
            ChatMessage::assistant_with_tools(
                "read then write",
                vec![
                    crate::llm::provider::ToolCall {
                        id: "c1".into(),
                        name: "read_file".into(),
                        arguments: serde_json::json!({"path": "src/main.rs"}),
                    },
                    crate::llm::provider::ToolCall {
                        id: "c2".into(),
                        name: "write_file".into(),
                        arguments: serde_json::json!({"path": "src/config.rs"}),
                    },
                ],
            ),
            ChatMessage::tool_result("ok", "c1"),
            ChatMessage::tool_result("ok", "c2"),
            ChatMessage::user("newest-request"),
        ];
        let c = compact_messages_with_budget(&m, 0, 5);
        let summary = c
            .iter()
            .find(|m| m.role == Role::System && m.content.contains("compressed"))
            .expect("summary present");
        assert!(
            summary.content.contains("Files read: src/main.rs"),
            "missing read file in summary: {}",
            summary.content
        );
        assert!(
            summary.content.contains("Files modified: src/config.rs"),
            "missing modified file in summary: {}",
            summary.content
        );
    }

    #[test]
    fn estimate_tokens_chars_over_four() {
        assert_eq!(estimate_tokens(""), 1);
        assert_eq!(estimate_tokens("abcd"), 1);
        assert_eq!(estimate_tokens("abcdefgh"), 2);
    }
}
