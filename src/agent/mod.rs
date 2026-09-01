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

pub mod invariant;
pub mod provider;

use anyhow::{Context, Result};
use futures_util::StreamExt;

use crate::llm::provider::{ChatComplete, ChatMessage, ChatRequest, LlmProvider, Role, TokenUsage};
use crate::safety;
use crate::tools::ToolRegistry;

#[cfg(test)]
mod tests;

/// Agent loop configuration.
#[derive(Clone)]
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
    /// Where `goals.jsonl` lives, for persisting turn-budget
    /// auto-pauses from the agent loop (same file the tool writes).
    pub goals_dir: Option<std::path::PathBuf>,
    /// Planning checkpoints (borrowed from smolagents'
    /// `planning_interval`): every N agent-loop steps the model is
    /// asked to restate its plan before continuing. `0` disables.
    pub planning_interval: u32,
    /// Agent-loop Lua hooks (P-A thin-core roadmap). `None` = hooks
    /// disabled. When set, hook files under the runner's directory
    /// (e.g. `on_tool_result.lua`, `on_turn_start.lua`) run at their
    /// loop points; absent files are a no-op.
    pub hooks: Option<std::sync::Arc<crate::hooks::HookRunner>>,
    /// Event sink for event-sourced session logging (Phase 63). When
    /// set, the agent loop emits `ToolCall` / `ToolResult` events as
    /// tools execute; the owner (REPL/daemon) appends them to the
    /// session log. `None` = no session-event recording (pipe mode,
    /// tests, delegates).
    pub session_sink: Option<std::sync::Arc<dyn Fn(crate::session::Event) + Send + Sync>>,
}

/// Planning checkpoint prompt injected every `planning_interval` steps.
pub const PLANNING_CHECKPOINT_PROMPT: &str = "PLANNING CHECKPOINT: Before continuing, briefly restate your \
     current plan — what you have done so far and what remains. Keep it \
     to 2-3 lines. Then continue working toward the task.";

impl AgentConfig {
    /// System prompt for a specific user prompt: base prompt plus the
    /// most relevant skills inlined (keyword-overlap ranked), plus a
    /// compact index of every skill so the model knows what else is
    /// available and can load any body on demand. Reads the *live*
    /// state, so harness edits apply immediately.
    ///
    /// P-B (thin-core roadmap): an `on_prompt_build.lua` hook in the
    /// hooks dir can override the final prompt. Cache discipline
    /// (Phase 65, DeepSeek Harness "don't break the prefix cache"):
    /// the hook's returned text is **appended** to the base prompt
    /// (prefix stays byte-stable across turns → provider prefix cache
    /// keeps hitting). A hook that truly needs to replace everything
    /// returns text starting with `__REPLACE_ALL__` (escape hatch).
    /// Hook errors fall back to the base prompt (prompt assembly must
    /// never break the agent; unlike the on_turn_start / on_tool_result
    /// hooks, this one is lenient).
    pub fn system_prompt_for(&self, prompt: &str) -> String {
        let state = self.prompt_state.read().unwrap_or_else(|e| e.into_inner());
        let base = if state.skills.is_empty() || self.max_inline_skills == 0 {
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
        };
        if let Some(hooks) = &self.hooks {
            let skills: Vec<serde_json::Value> = state
                .skills
                .iter()
                .map(|s| {
                    serde_json::json!({
                        "name": s.name,
                        "description": s.description,
                        "body": s.body,
                    })
                })
                .collect();
            match hooks.run_sync(
                "on_prompt_build",
                serde_json::json!({
                    "base": base.clone(),
                    "system_prompt": state.system_prompt,
                    "skills": skills,
                    "user_prompt": prompt,
                    "max_inline_skills": self.max_inline_skills,
                }),
            ) {
                Ok(Some(text)) => {
                    // Cache discipline: appended tail keeps the prefix
                    // byte-stable. `__REPLACE_ALL__` is the escape
                    // hatch for full overrides.
                    if let Some(replaced) = text.strip_prefix("__REPLACE_ALL__") {
                        return replaced.to_string();
                    }
                    let mut p = base;
                    p.push_str("\n\n");
                    p.push_str(&text);
                    return p;
                }
                Ok(None) => {}
                Err(e) => eprintln!("warning: on_prompt_build hook failed: {e:#}"),
            }
        }
        base
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
    compact_messages_with_budget(messages, max, 0, None)
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
///
/// Cross-compaction accumulation: system messages that are themselves
/// previous compaction summaries (they carry the
/// `[earlier conversation compressed]` header and `Files read:` /
/// `Files modified:` lines) are also parsed, so a second compaction
/// merges the file history preserved by the first instead of dropping
/// it (Pi's `compaction.details` accumulate the same way).
fn extract_file_ops(messages: &[ChatMessage]) -> (Vec<String>, Vec<String>) {
    let mut read: Vec<String> = Vec::new();
    let mut modified: Vec<String> = Vec::new();
    let mut push_read = |p: String| {
        if !read.contains(&p) {
            read.push(p);
        }
    };
    let mut push_modified = |p: String| {
        if !modified.contains(&p) {
            modified.push(p);
        }
    };
    for m in messages {
        // 1) Tool calls carry the authoritative paths.
        if let Some(calls) = &m.tool_calls {
            for c in calls {
                let Some(path) = c.arguments.get("path").and_then(serde_json::Value::as_str) else {
                    continue;
                };
                let path = path.to_string();
                match c.name.as_str() {
                    "read_file" => push_read(path),
                    "write_file" => push_modified(path),
                    _ => {}
                }
            }
        }
        // 2) Previous compaction summaries already list files — parse
        //    those lines so the history survives repeated compactions.
        if m.role == Role::System && m.content.starts_with("[earlier conversation compressed]") {
            for line in m.content.lines() {
                if let Some(rest) = line.strip_prefix("Files read: ") {
                    for p in rest.split(',').map(str::trim) {
                        if !p.is_empty() {
                            push_read(p.to_string());
                        }
                    }
                } else if let Some(rest) = line.strip_prefix("Files modified: ") {
                    for p in rest.split(',').map(str::trim) {
                        if !p.is_empty() {
                            push_modified(p.to_string());
                        }
                    }
                }
            }
        }
    }
    (read, modified)
}

/// [`compact_messages`] with a token budget: compress when the
/// estimated transcript size exceeds `max_tokens` OR the message count
/// exceeds `max_msgs`. Either budget may be `0` to disable that axis.
///
/// `recent_prompt_tokens` is the real `prompt_tokens` usage from the
/// most recent LLM call (prime-agent uses actual `usage.totalTokens`
/// over the chars/4 heuristic). When present it (a) drives the
/// over-budget trigger and (b) calibrates the per-message estimate by
/// the real/estimated ratio while walking the retained tail — the
/// chars/4 heuristic underestimates CJK-heavy transcripts by ~4x, so
/// an uncalibrated walk would keep far too little.
///
/// The kept tail is chosen by walking backwards from the newest
/// message, accumulating tokens until the token budget (or message
/// budget) is met, then extended forward to the nearest safe cut
/// boundary.
pub fn compact_messages_with_budget(
    messages: &[ChatMessage],
    max_msgs: usize,
    max_tokens: usize,
    recent_prompt_tokens: Option<usize>,
) -> Vec<ChatMessage> {
    // No budget configured → never compress.
    if max_msgs == 0 && max_tokens == 0 {
        return messages.to_vec();
    }
    if messages.len() < 3 {
        return messages.to_vec();
    }
    // Already within every configured budget? Real usage from the last
    // LLM call (≈ current transcript size, since prompts accumulate)
    // wins over the heuristic when available.
    let total_tokens: usize = messages.iter().map(estimate_message_tokens).sum();
    let effective_total = recent_prompt_tokens.unwrap_or(total_tokens);
    let within_msgs = max_msgs == 0 || messages.len() <= max_msgs;
    let within_tokens = max_tokens == 0 || effective_total <= max_tokens;
    if within_msgs && within_tokens {
        return messages.to_vec();
    }
    // Calibrate the per-message estimate for the tail walk.
    let ratio: f64 = if total_tokens > 0 {
        (effective_total as f64 / total_tokens as f64).max(1.0)
    } else {
        1.0
    };

    // Walk backwards, accumulating token estimates, until the token
    // budget or the message budget (minus the summary slot) is met.
    let keep_count = max_msgs.saturating_sub(2).max(2);
    let mut cut = 1usize;
    let mut acc = 0usize;
    for i in (1..messages.len()).rev() {
        acc += (estimate_message_tokens(&messages[i]) as f64 * ratio) as usize;
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

/// Inject (or refresh) the goals block into a message list. The agent
/// loop calls this every iteration; the block is keyed by
/// [`crate::goals::GOALS_MARKER`], so repeated calls replace the
/// previous block in place instead of stacking duplicates.
fn inject_goals_block(messages: &mut Vec<ChatMessage>, block: &str) {
    let existing = messages
        .iter()
        .position(|m| m.role == Role::System && m.content.starts_with(crate::goals::GOALS_MARKER));
    match existing {
        Some(i) => messages[i] = ChatMessage::system(block),
        None => {
            let pos = messages
                .iter()
                .position(|m| m.role == Role::System)
                .map(|p| p + 1)
                .unwrap_or(0);
            messages.insert(pos, ChatMessage::system(block));
        }
    }
}

/// Approval callback: given a tool call that the gate rejected in
/// non-interactive mode, decide whether the human allows it anyway.
/// Returns true to run it (interactive override).
pub type Approver = dyn Fn(&str, &serde_json::Value) -> bool + Send + Sync;

/// The shape of a mid-turn question (mirrors the NPP DecisionCanvas
/// structured question contract: pick_one / pick_many / confirm /
/// ask_text). Defaults to `single` so older clients/models that omit
/// `kind` keep the classic multiple-choice behavior.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Deserialize, serde::Serialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum QuestionKind {
    /// Pick exactly one option (options are required in spirit; the
    /// human may still answer freely).
    #[default]
    Single,
    /// Pick any number of options (multi-select).
    Multi,
    /// Confirm / reject (the host renders 是/否 or equivalent).
    Confirm,
    /// Free-form text only (options must be empty).
    Text,
}

/// A question the agent asks the human mid-turn (brainstorming /
/// requestUserInput-style flows). The host answers via the AskCb; the
/// answer text is handed back to the agent loop as the `ask` tool's
/// result.
#[derive(Debug, Clone, serde::Deserialize)]
pub struct AskRequest {
    pub question: String,
    #[serde(default)]
    pub options: Vec<String>,
    #[serde(default)]
    pub kind: QuestionKind,
}

/// The human's answer to an [`AskRequest`].
#[derive(Debug, Clone)]
pub struct AskAnswer {
    pub text: String,
}

/// Asynchronous human-input callback: given a question, return the
/// human's answer. Runs across an async boundary (the host may need to
/// ask a UI and wait), so it returns a future.
pub type AskCb = dyn Fn(AskRequest) -> futures_util::future::BoxFuture<'static, anyhow::Result<AskAnswer>>
    + Send
    + Sync;

/// Stream callback: called with each chunk of assistant text as it
/// arrives (UI streaming). Also called with `None` when a tool call
/// interrupts the text stream (so the UI can flush).
pub type StreamCb = dyn Fn(Option<&str>) + Send + Sync;

/// One turn of the agent loop over a PRIOR transcript (multi-turn
/// sessions: the caller keeps the transcript returned in
/// [`RunResult::messages`] and feeds it back next turn).
///
/// `history` is the previous transcript (system prompt included when
/// present — the loop will not re-insert one). When it is empty the
/// system prompt is built from the new prompt via
/// [`AgentConfig::system_prompt_for`], so a fresh session behaves
/// exactly like a one-shot prompt. The new user prompt is appended to
/// the history. Returns the full [`RunResult`] including the updated
/// transcript (post-compaction) so the caller can store it.
///
/// This is the ACP-session workhorse: each `agent/session.prompt` runs
/// one turn, and the per-session transcript is what makes a session
/// remember its earlier turns (and lets `agent/session/fork` clone a
/// branch's context).
pub async fn run_session_turn(
    provider: &dyn LlmProvider,
    tools: &mut ToolRegistry,
    config: &AgentConfig,
    history: &[ChatMessage],
    user_prompt: &str,
) -> Result<RunResult> {
    let mut messages: Vec<ChatMessage> = history.to_vec();
    if !messages
        .iter()
        .any(|m| matches!(m.role, Role::System))
    {
        messages.insert(
            0,
            ChatMessage::system(config.system_prompt_for(user_prompt)),
        );
    }
    messages.push(ChatMessage::user(user_prompt));
    run_messages(provider, tools, config, &messages, None, None).await
}

/// Outcome state of an agent loop run.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RunState {
    /// The agent answered normally.
    Success,
    /// `max_steps` was exhausted; the loop asked the model for a
    /// wrap-up summary instead of aborting mid-work.
    StepLimitReached,
}

/// Structured result of a `run_messages` run — the answer text plus
/// run metadata (borrowed from smolagents' `RunResult`: output +
/// steps + state). Callers (REPL / daemon / delegate) no longer have
/// to guess whether a run ended by answering or by exhausting its
/// step budget: the state is explicit, and a budget-exhausted run is
/// *not* an error (the model still produced a summary answer).
///
/// `messages` is the final transcript (system prompt included) after
/// any in-run compaction. Session-scoped callers (ACP sessions) keep
/// it to build the next turn's context; one-shot callers ignore it.
#[derive(Debug, Clone)]
pub struct RunResult {
    /// Final answer text (or the wrap-up summary on step-limit).
    pub output: String,
    /// Number of agent-loop iterations consumed.
    pub steps: u32,
    pub state: RunState,
    /// Cumulative token usage across the run's completions, when the
    /// provider reported usage (streamed + tool-call replays).
    pub tokens: Option<TokenUsage>,
    /// Final transcript (system + history) — see struct docs.
    pub messages: Vec<ChatMessage>,
}

/// Fold one completion's usage into the running total (None-safe).
fn accumulate_usage(tokens: &mut Option<TokenUsage>, u: Option<TokenUsage>) {
    if let Some(u) = u {
        *tokens = Some(match *tokens {
            None => u,
            Some(t) => t + u,
        });
    }
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
) -> Result<RunResult> {
    let mut messages = messages.to_vec();

    // P-B (thin-core roadmap): ensure the harness system prompt is
    // present. Callers that already build it (ACP, daemon, REPL)
    // are untouched; REPL and pipe mode previously ran with no system
    // prompt at all, which also meant `on_prompt_build` hooks never
    // fired there. This also covers resume paths whose transcript
    // predates the system prompt.
    if !messages
        .iter()
        .any(|m| matches!(m.role, crate::llm::provider::Role::System))
    {
        let first_user = messages.iter().find_map(|m| match m.role {
            crate::llm::provider::Role::User => Some(m.content.clone()),
            _ => None,
        });
        let system = config.system_prompt_for(first_user.as_deref().unwrap_or(""));
        messages.insert(0, ChatMessage::system(system));
    }

    let breaker = safety::CircuitBreaker::new();
    let mut step: u32 = 0;
    let mut tokens: Option<TokenUsage> = None;
    // Real prompt-token usage of the most recent LLM call — feeds the
    // compaction trigger/calibration (see compact_messages_with_budget).
    let mut last_prompt_tokens: Option<usize> = None;
    // Phase A2: turn 事件只写一次（run = 一个 turn，多 step）。
    let mut turn_started = false;

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
                last_prompt_tokens,
            );
        }

        // Persistent goals: inject active goals into the system context
        // so the model keeps working toward them this turn. Inserted
        // right after the first system message (the main prompt /
        // compaction summary) so it is never folded away. The agent
        // loop re-runs this every iteration, so an already-injected
        // block is *replaced* in place rather than duplicated.
        if let Some(goals) = &config.goals
            && let Ok(mut g) = goals.write()
        {
            // Turn-budget accounting: one LLM call = one turn. Spend a
            // turn on every active goal, auto-pause the exhausted ones,
            // and persist (the tool holds the same Arc, so the budget
            // state stays consistent for `manage_goals list`).
            let paused =
                crate::goals::bump_turn_usage(&mut g) + crate::goals::pause_exhausted(&mut g);
            if paused > 0
                && let Some(dir) = &config.goals_dir
            {
                let _ = crate::goals::save_goals(dir, &g);
            }
            if let Some(block) = crate::goals::active_goals_block(&g) {
                inject_goals_block(&mut messages, &block);
            }
        }

        // Planning checkpoints (borrowed from smolagents'
        // `planning_interval`): on the first step and every Nth step
        // afterwards, ask the model to restate its plan. Keeps long
        // multi-tool turns on track instead of drifting. A previous
        // checkpoint is dropped first so the history never carries a
        // stale one between checkpoints (and the model does not keep
        // re-answering it on every step).
        messages.retain(|m| !m.content.contains("PLANNING CHECKPOINT"));
        if config.planning_interval > 0
            && (step == 0 || step.is_multiple_of(config.planning_interval))
        {
            messages.push(ChatMessage::system(PLANNING_CHECKPOINT_PROMPT));
        }

        // Fresh specs every turn: evolution tools may have added tools
        // mid-run, and the model should see them on the next turn.
        let specs = tools.specs();

        if config.max_steps > 0 && step >= config.max_steps {
            // Step budget exhausted (borrowed from smolagents'
            // `provide_final_answer`): don't bail with whatever text
            // happened to be streaming — run one final, tool-less LLM
            // call asking for a wrap-up summary so the caller always
            // gets a real answer, tagged with StepLimitReached.
            let (summary, summary_tokens) =
                match provide_final_answer(provider, config, &messages, stream_cb).await {
                    Ok((s, t)) => (s, t),
                    Err(e) => (
                        format!(
                            "step limit ({}) reached — summary unavailable ({e})",
                            config.max_steps
                        ),
                        None,
                    ),
                };
            accumulate_usage(&mut tokens, summary_tokens);
            // P0 hook (emit observation): observe the step-limit wrap-up
            // too — step-budget exhaustion is exactly when delivery audit
            // (on_done) matters most (failures land here, not on the
            // tool-less final-answer path).
            if let Some(hooks) = &config.hooks
                && let Err(e) = hooks
                    .dispatch(
                        "on_done",
                        crate::hooks::HookMode::Emit,
                        serde_json::json!({
                            "output": summary.trim(),
                            "steps": step,
                            "model": config.model,
                        }),
                    )
                    .await
            {
                eprintln!("warning: on_done hook failed: {e:#}");
            }
            return Ok(RunResult {
                output: summary,
                steps: step,
                state: RunState::StepLimitReached,
                tokens,
                messages,
            });
        }
        step += 1;

        // P-A hook: on_turn_start can inject context at the top of
        // each turn (returned string becomes a system message).
        if let Some(hooks) = &config.hooks
            && let Some(text) = hooks
                .run(
                    "on_turn_start",
                    serde_json::json!({ "step": step, "model": config.model }),
                )
                .await?
        {
            messages.push(ChatMessage::system(text));
        }

        // ── Phase A2: 事件溯源 —— 注入即事件 + 请求前配对不变式 ──
        // 本轮请求的上下文（step/header/注入）先入日志，再进请求；
        // 配对校验拦截半完成 step（崩溃残留），fail-fast 而非发脏上下文。
        if let Some(sink) = &config.session_sink {
            if !turn_started {
                sink(crate::session::Event::TurnStart { ts: None });
                turn_started = true;
            }
            sink(crate::session::Event::StepStart { ts: None });
            sink(crate::session::Event::RequestHeader {
                ts: None,
                model: config.model.clone(),
                temperature: None,
                max_tokens: None,
                tools_fingerprint: Some(
                    specs
                        .iter()
                        .map(|s| s.name.clone())
                        .collect::<Vec<_>>()
                        .join("|"),
                ),
            });
            if config.goals.is_some() {
                // 注入上下文（持久目标）—— 注入即事件，审计可复现。
                sink(crate::session::Event::RequestContext {
                    ts: None,
                    values: vec![("goals".into(), "active".into())],
                });
            }
        }
        crate::agent::invariant::check_tool_pairing(&messages).map_err(|v| {
            anyhow::anyhow!("会话一致性校验失败（Model-visible means logged）：{v}")
        })?;

        let req = ChatRequest {
            model: config.model.clone(),
            messages: messages.clone(),
            tools: specs.clone(),
            stream: true,
            temperature: None,
        };

        // Reactive compaction (borrowed from OpenHands' condenser):
        // the proactive budget check above runs every iteration, but a
        // single huge tool output can still push the transcript over
        // the provider's window. When the LLM call fails with a
        // context-window / malformed-history error, compact once and
        // retry before giving up.
        let mut stream = match provider.chat_stream(req.clone()).await {
            Ok(s) => s,
            Err(e) if is_recoverable_llm_error(&e) && messages.len() > 4 => {
                // Halve the transcript: the proactive budget check may
                // already have run (its result can still exceed the
                // provider's window), so a second, more aggressive pass
                // must make progress regardless of the configured
                // budget.
                let compacted =
                    compact_messages_with_budget(&messages, messages.len() / 2, 0, None);
                if compacted.len() < messages.len() {
                    eprintln!(
                        "  llm error ({e:#}); compacting transcript \
                         ({} -> {} messages) and retrying",
                        messages.len(),
                        compacted.len()
                    );
                    let retry_req = ChatRequest {
                        model: config.model.clone(),
                        messages: compacted,
                        tools: specs,
                        stream: true,
                        temperature: None,
                    };
                    provider
                        .chat_stream(retry_req)
                        .await
                        .context("starting completion (after compaction)")?
                } else {
                    return Err(e).context("starting completion");
                }
            }
            Err(e) => return Err(e).context("starting completion"),
        };

        let mut text = String::new();
        let mut tool_calls_requested = false;
        while let Some(chunk) = stream.next().await {
            let chunk = chunk.context("reading stream")?;
            accumulate_usage(&mut tokens, chunk.usage);
            if let Some(u) = chunk.usage {
                last_prompt_tokens = Some(u.prompt_tokens as usize);
            }
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
        // Cache-discipline diagnostics (Phase 65): with SIGIL_DEBUG_CACHE
        // set, log how much of the prompt was served from the provider's
        // prefix cache. A steady hit-rate drop after a prompt-assembly
        // change means the prefix stopped being byte-stable. Goes to
        // stderr (stdout stays pure for pipe mode).
        //
        // Provider coverage: OpenAI returns cached_tokens in usage;
        // vLLM 0.23 does NOT (prompt_tokens_details is null) — but its
        // /metrics endpoint exposes prefix_cache_hits_total, verified
        // end-to-end on 10.10.10.115:8011 at 95% hit for stable
        // prefixes. Ollama also omits cached_tokens. When the provider
        // reports none, the probe still gives prompt_tokens (useful for
        // context-budget math) and the true hit figures must be read
        // from the provider's own metrics endpoint.
        if std::env::var_os("SIGIL_DEBUG_CACHE").is_some() {
            // vLLM / Ollama do NOT return usage on streamed responses;
            // when the stream carried none, probe once non-streamed with
            // the same prompt to learn the true cache-hit figures.
            // (The probe re-sends the prompt, which is exactly what a
            // cache-hit measurement needs — cached tokens only appear
            // when the prefix matches a previously seen one.)
            if tokens.is_none() {
                match provider
                    .chat_complete(ChatRequest {
                        model: config.model.clone(),
                        messages: messages.clone(),
                        tools: tools.specs(),
                        stream: false,
                        temperature: None,
                    })
                    .await
                {
                    Ok(complete) => tokens = complete.usage,
                    Err(e) => eprintln!("[cache] usage probe failed: {e:#}"),
                }
            }
            if let Some(u) = tokens {
                let pct = if u.prompt_tokens > 0 {
                    (u.prompt_cache_hit_tokens as f64 / u.prompt_tokens as f64) * 100.0
                } else {
                    0.0
                };
                eprintln!(
                    "[cache] {}/{} prompt tokens cached ({:.0}%)",
                    u.prompt_cache_hit_tokens, u.prompt_tokens, pct
                );
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
            accumulate_usage(&mut tokens, complete.usage);
            if let Some(u) = complete.usage {
                last_prompt_tokens = Some(u.prompt_tokens as usize);
            }
            if complete.tool_calls.is_empty() {
                // Model asked for tools but sent none; treat text as answer.
                let answer = if text.trim().is_empty() {
                    complete.text
                } else {
                    text
                };
                // Phase A2: 最终答案事件（模型可见输出入日志）。
                if let Some(sink) = &config.session_sink {
                    sink(crate::session::Event::Assistant {
                        ts: None,
                        text: answer.clone(),
                    });
                    sink(crate::session::Event::TurnEnd { ts: None });
                }
                return Ok(RunResult {
                    output: answer.trim().to_string(),
                    steps: step,
                    state: RunState::Success,
                    tokens,
                    messages,
                });
            }
            // Phase A2: assistant 事件（文本 + 随后 ToolCall 事件配对成完整消息）。
            if let Some(sink) = &config.session_sink {
                sink(crate::session::Event::Assistant {
                    ts: None,
                    text: complete.text.clone(),
                });
            }
            messages.push(ChatMessage::assistant_with_tools(
                &complete.text,
                complete.tool_calls.clone(),
            ));

            // Two-phase tool execution (borrowed from OpenHands'
            // ParallelToolExecutor): approval-gate and circuit-breaker
            // checks are cheap and serial; the actual tool runs are
            // independent (the LLM issues several tool calls in one
            // response precisely because they do not depend on each
            // other), so they execute concurrently. Results are
            // re-inserted in the original call order so the LLM always
            // sees tool_results matching its tool_call ids.
            struct Plan {
                call: crate::llm::ToolCall,
                /// Some(reason) = rejected/tripped, do not run.
                blocked: Option<String>,
            }
            let mut plans: Vec<Plan> = Vec::with_capacity(complete.tool_calls.len());
            let mut executables: Vec<usize> = Vec::new(); // plan indices to run
            // Session event sink (Phase 63 event-sourced logging): emit
            // one ToolCall event per requested call before gating, so
            // the raw log records what the model asked for even when
            // approval later blocks it.
            if let Some(sink) = &config.session_sink {
                for call in &complete.tool_calls {
                    // Cordis P3: fingerprint the call so a log replay can
                    // find the matching inverse (per-frame rollback).
                    let key = crate::session::revert_key(&call.name, &call.arguments);
                    sink(crate::session::Event::ToolCall {
                        ts: None,
                        id: call.id.clone(),
                        name: call.name.clone(),
                        args: call.arguments.to_string(),
                        revert_key: key,
                    });
                }
            }
            for call in &complete.tool_calls {
                // Approval gate: reject destructive commands in
                // non-interactive mode before they ever run. In
                // interactive mode the human may override.
                let gate = safety::check(&call.name, &call.arguments);
                let mut allowed = matches!(gate, safety::GateDecision::Allow);
                // Audit event helper: record the approval decision for
                // a gate-affected call (audit-only, never replayed into
                // model context).
                let emit_approval = |decision: &str, reason: Option<String>| {
                    if let Some(sink) = &config.session_sink {
                        sink(crate::session::Event::Approval {
                            ts: None,
                            name: call.name.clone(),
                            args: call.arguments.to_string(),
                            decision: decision.into(),
                            reason,
                        });
                    }
                };
                if !allowed
                    && let Some(approve) = approver
                    && approve(&call.name, &call.arguments)
                {
                    allowed = true;
                    eprintln!("⚠ approved by user: {} {}", call.name, call.arguments);
                    emit_approval("approved", None);
                }
                if !allowed {
                    let decision = if approver.is_some() {
                        "rejected"
                    } else {
                        "auto_rejected"
                    };
                    emit_approval(decision, None);
                    plans.push(Plan {
                        call: call.clone(),
                        blocked: Some(safety::reject_message(&call.name, &call.arguments)),
                    });
                    continue;
                }
                // P0 hook (serial decision): an `on_pre_tool` listener can
                // veto the call before it runs. The first non-null verdict
                // wins; a truthy verdict (boolean true, or a non-empty
                // string used as a human-readable reason) blocks the call.
                // A nil / null verdict is a no-op and the chain continues.
                if let Some(hooks) = &config.hooks
                    && let crate::hooks::DispatchOutput::First(Some(v)) = hooks
                        .dispatch(
                            "on_pre_tool",
                            crate::hooks::HookMode::Serial,
                            serde_json::json!({
                                "name": call.name,
                                "args": call.arguments,
                            }),
                        )
                        .await?
                {
                    // A verdict blocks the call when it is boolean `true`
                    // or a non-empty string (the string is surfaced as the
                    // reason). Anything else — `false`, null, an empty
                    // string — is a pass and the chain continues.
                    let veto_reason = match &v {
                        serde_json::Value::Bool(true) => {
                            Some("vetoed by on_pre_tool hook".to_string())
                        }
                        serde_json::Value::String(s) if !s.is_empty() => Some(s.clone()),
                        _ => None,
                    };
                    if let Some(reason) = veto_reason {
                        emit_approval("hook_vetoed", Some(reason.clone()));
                        plans.push(Plan {
                            call: call.clone(),
                            blocked: Some(format!("blocked by on_pre_tool hook: {reason}")),
                        });
                        continue;
                    }
                }
                // Circuit breaker: skip tripped tools entirely.
                if breaker.is_tripped(&call.name) {
                    plans.push(Plan {
                        call: call.clone(),
                        blocked: Some(safety::CircuitBreaker::tripped_message(&call.name)),
                    });
                    continue;
                }
                executables.push(plans.len());
                plans.push(Plan {
                    call: call.clone(),
                    blocked: None,
                });
            }

            // Parallel execution of the allowed calls. CircuitBreaker
            // is internally locked, so per-call failure/success
            // recording stays correct under concurrency. Errors are
            // returned as tool output (never fatal), matching
            // ToolRegistry::execute's contract.
            let registry = &*tools;
            let breaker_ref = &breaker;
            let futs = executables.iter().map(|&pi| {
                let call = plans[pi].call.clone();
                async move {
                    let result = match registry.execute(&call).await {
                        Ok(r) => r,
                        Err(e) => format!("error: {e}"),
                    };
                    // Execution ring: flag harness mutations to the
                    // model so it knows the toolset changed.
                    let result = match safety::ring_of(&call.name) {
                        safety::Ring::Harness => format!("[harness mutation] {result}"),
                        _ => result,
                    };
                    if result.starts_with("error:") {
                        breaker_ref.record_failure(&call.name);
                    } else {
                        breaker_ref.record_success(&call.name);
                    }
                    (pi, result)
                }
            });
            let executed: Vec<(usize, String)> = futures_util::future::join_all(futs).await;
            let mut results_by_plan: std::collections::HashMap<usize, String> =
                executed.into_iter().collect();

            for (i, plan) in plans.into_iter().enumerate() {
                let blocked = plan.blocked.clone();
                let msg = match &blocked {
                    Some(m) => m.clone(),
                    None => results_by_plan
                        .remove(&i)
                        .unwrap_or_else(|| "error: tool produced no result".into()),
                };
                // Session event sink: emit ToolResult with the observed
                // outcome (ok = executed and not an error-prefixed string;
                // blocked/rejected calls emit ok=false).
                if let Some(sink) = &config.session_sink {
                    let ok = blocked.is_none() && !msg.starts_with("error:");
                    // Cordis P3: annotate whether this call registered a
                    // runtime-revertible effect (P1) so the session can be
                    // rolled back frame-by-frame. Grounded in the actual
                    // wiring: only a tool that opted into the P1
                    // EffectTracker reports Some(true); everything else is
                    // None (not assessed).
                    let revertible = registry.revertible_of(&plan.call.name);
                    sink(crate::session::Event::ToolResult {
                        ts: None,
                        id: plan.call.id.clone(),
                        name: plan.call.name.clone(),
                        ok,
                        output: msg.clone(),
                        revertible,
                    });
                }
                // P-A hook: on_tool_result can rewrite what the model
                // sees (returned string replaces the result text).
                let msg = match &config.hooks {
                    Some(hooks) => hooks
                        .run(
                            "on_tool_result",
                            serde_json::json!({
                                "name": plan.call.name,
                                "args": plan.call.arguments,
                                "result": msg,
                            }),
                        )
                        .await?
                        .unwrap_or(msg),
                    None => msg,
                };
                messages.push(ChatMessage::tool_result(msg, plan.call.id));
            }
            // Evolution: hot-register any tools the model created this
            // turn so they are callable (and advertised) next turn.
            tools.drain_hot();
            continue;
        }

        // No tool call — this is the final answer.
        // P0 hook (emit observation): an `on_done` listener may react to
        // the finished turn (log, metrics, notify). Purely observational —
        // a hook error is logged, never fatal, since the answer is already
        // produced (same leniency as on_prompt_build).
        if let Some(hooks) = &config.hooks
            && let Err(e) = hooks
                .dispatch(
                    "on_done",
                    crate::hooks::HookMode::Emit,
                    serde_json::json!({
                        "output": text.trim(),
                        "steps": step,
                        "model": config.model,
                    }),
                )
                .await
        {
            eprintln!("warning: on_done hook failed: {e:#}");
        }
        return Ok(RunResult {
            output: text.trim().to_string(),
            steps: step,
            state: RunState::Success,
            tokens,
            messages,
        });
    }
}

/// Final tool-less LLM call after the step budget is exhausted
/// (smolagents' `provide_final_answer`). Asks the model to wrap up
/// its progress so far into a summary answer, using the transcript
/// accumulated in the loop. Deliberately passes no tools — the run
/// must end, not extend itself.
async fn provide_final_answer(
    provider: &dyn LlmProvider,
    config: &AgentConfig,
    messages: &[ChatMessage],
    stream_cb: Option<&StreamCb>,
) -> Result<(String, Option<TokenUsage>)> {
    let mut msgs = messages.to_vec();
    msgs.push(ChatMessage::system(
        "Step budget exhausted. Do not call any tools. \
         Summarize your progress so far and give your best final answer now.",
    ));
    let req = ChatRequest {
        model: config.model.clone(),
        messages: msgs,
        tools: vec![],
        stream: true,
        temperature: None,
    };
    let mut stream = provider.chat_stream(req).await.context("final summary")?;
    let mut text = String::new();
    let mut tokens: Option<TokenUsage> = None;
    while let Some(chunk) = stream.next().await {
        let chunk = chunk.context("reading final summary")?;
        accumulate_usage(&mut tokens, chunk.usage);
        if !chunk.thinking {
            text.push_str(&chunk.delta);
            if let Some(cb) = stream_cb {
                cb(Some(&chunk.delta));
            }
        }
        if chunk.done {
            break;
        }
    }
    if text.trim().is_empty() {
        anyhow::bail!("final summary came back empty");
    }
    Ok((text.trim().to_string(), tokens))
}

/// Classify a provider error: can compaction recover from it? OpenHands
/// routes context-window-exceeded and malformed-history failures into
/// condensation retries; sigil does the same with its deterministic
/// compaction.
fn is_recoverable_llm_error(err: &anyhow::Error) -> bool {
    let msg = format!("{err:#}").to_lowercase();
    const RECOVERABLE: &[&str] = &[
        "context length",
        "context window",
        "maximum context",
        "max context",
        "too many tokens",
        "token limit",
        "context is too long",
        "exceeded the maximum",
        "malformed",
        "unmatched tool",
        "invalid tool",
        "previous response",
    ];
    RECOVERABLE.iter().any(|k| msg.contains(k))
}

#[cfg(test)]
mod parallel_tests {
    use super::*;
    use crate::llm::provider::{ChatComplete, ChatRequest, ChatStream, LlmProvider, ToolCall};
    use async_trait::async_trait;
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
    use std::sync::{Arc, Mutex};
    use std::time::{Duration, Instant};

    /// Records its start time, then sleeps — the slow half of the
    /// concurrency check.
    #[derive(Clone)]
    struct SlowTool(Arc<Mutex<Vec<Instant>>>);
    #[async_trait]
    impl crate::tools::Tool for SlowTool {
        fn name(&self) -> &str {
            "slow_tool"
        }
        fn description(&self) -> &str {
            "slow tool for concurrency test"
        }
        fn parameters(&self) -> serde_json::Value {
            serde_json::json!({})
        }
        async fn run(&self, _args: &serde_json::Value) -> anyhow::Result<crate::tools::ToolOutput> {
            self.0.lock().unwrap().push(Instant::now());
            tokio::time::sleep(Duration::from_millis(200)).await;
            Ok(crate::tools::ToolOutput {
                text: "SLOW".into(),
            })
        }
    }

    /// Records its start time and returns immediately — the fast half.
    #[derive(Clone)]
    struct FastTool(Arc<Mutex<Vec<Instant>>>);
    #[async_trait]
    impl crate::tools::Tool for FastTool {
        fn name(&self) -> &str {
            "fast_tool"
        }
        fn description(&self) -> &str {
            "fast tool for concurrency test"
        }
        fn parameters(&self) -> serde_json::Value {
            serde_json::json!({})
        }
        async fn run(&self, _args: &serde_json::Value) -> anyhow::Result<crate::tools::ToolOutput> {
            self.0.lock().unwrap().push(Instant::now());
            Ok(crate::tools::ToolOutput {
                text: "FAST".into(),
            })
        }
    }

    /// First request emits two tool calls (slow_tool + fast_tool);
    /// second request verifies the tool results arrived in original
    /// call order and returns the final answer.
    #[derive(Debug)]
    struct TwoCallProvider {
        calls: AtomicUsize,
    }
    #[async_trait]
    impl LlmProvider for TwoCallProvider {
        fn name(&self) -> &str {
            "two-call-test"
        }
        async fn chat_stream(&self, req: ChatRequest) -> anyhow::Result<ChatStream> {
            let n = self.calls.fetch_add(1, Ordering::SeqCst);
            if n == 0 {
                Ok(Box::new(futures_util::stream::iter(vec![
                    Ok(crate::llm::provider::ChatChunk {
                        delta: String::new(),
                        thinking: false,
                        done: false,
                        tool_calls_requested: true,
                        usage: None,
                    }),
                    Ok(crate::llm::provider::ChatChunk {
                        delta: String::new(),
                        thinking: false,
                        done: true,
                        tool_calls_requested: true,
                        usage: None,
                    }),
                ])))
            } else {
                // Second loop iteration: verify the tool results came
                // back in original call order, then answer.
                let results: Vec<&str> = req
                    .messages
                    .iter()
                    .filter(|m| m.role == crate::llm::provider::Role::Tool)
                    .map(|m| m.content.as_str())
                    .collect();
                assert_eq!(
                    results,
                    vec!["SLOW", "FAST"],
                    "tool results must keep call order"
                );
                Ok(Box::new(futures_util::stream::iter(vec![Ok(
                    crate::llm::provider::ChatChunk {
                        delta: "DONE".into(),
                        thinking: false,
                        done: true,
                        tool_calls_requested: false,
                        usage: None,
                    },
                )])))
            }
        }
        async fn chat_complete(&self, _req: ChatRequest) -> anyhow::Result<ChatComplete> {
            // Only ever called once: the first-iteration tool-call
            // replay (the second iteration's stream carries the final
            // answer, so no replay happens).
            Ok(ChatComplete {
                text: String::new(),
                tool_calls: vec![
                    ToolCall {
                        id: "call-1".into(),
                        name: "slow_tool".into(),
                        arguments: serde_json::json!({}),
                    },
                    ToolCall {
                        id: "call-2".into(),
                        name: "fast_tool".into(),
                        arguments: serde_json::json!({}),
                    },
                ],
                usage: None,
            })
        }
    }

    /// Minimal AgentConfig for tests (no goals, no compression).
    fn test_config() -> AgentConfig {
        AgentConfig {
            model: "m".into(),
            max_steps: 0,
            prompt_state: std::sync::Arc::new(std::sync::RwLock::new(
                crate::harness::PromptState {
                    system_prompt: "base".into(),
                    skills: vec![],
                    fingerprint: None,
                },
            )),
            max_inline_skills: 0,
            sync_dir: None,
            max_context_messages: 0,
            max_context_tokens: 0,
            goals: None,
            goals_dir: None,
            planning_interval: 0,
            hooks: None,
            session_sink: None,
        }
    }

    #[tokio::test]
    async fn reactive_compaction_retries_after_context_error() {
        // The provider fails the first stream with a context-window
        // error; the agent must compact the transcript and retry, and
        // the retried request must carry fewer messages.
        let provider = ContextErrorProvider {
            calls: AtomicUsize::new(0),
            seen_compacted: AtomicUsize::new(0),
        };
        let mut tools = crate::tools::ToolRegistry::new();
        let config = AgentConfig {
            model: "m".into(),
            max_steps: 0,
            prompt_state: std::sync::Arc::new(std::sync::RwLock::new(
                crate::harness::PromptState {
                    system_prompt: "base".into(),
                    skills: vec![],
                    fingerprint: None,
                },
            )),
            max_inline_skills: 0,
            sync_dir: None,
            max_context_messages: 6,
            max_context_tokens: 0,
            goals: None,
            goals_dir: None,
            planning_interval: 0,
            hooks: None,
            session_sink: None,
        };
        // 10 user turns -> well over the 6-message budget.
        let mut msgs = vec![ChatMessage::system("SYS")];
        for i in 0..10 {
            msgs.push(ChatMessage::user(format!("user-{i}")));
            msgs.push(ChatMessage::assistant(format!("assistant-{i}")));
        }
        let answer = run_messages(&provider, &mut tools, &config, &msgs, None, None)
            .await
            .expect("compaction retry succeeds");
        assert_eq!(answer.output, "AFTER-RETRY");
        assert_eq!(answer.state, RunState::Success);
        // First stream failed, second succeeded.
        assert_eq!(provider.calls.load(Ordering::SeqCst), 2);
        assert_eq!(provider.seen_compacted.load(Ordering::SeqCst), 1);
    }

    /// First chat_stream errors with a context-window failure; the
    /// retried stream must receive a compacted transcript and answer.
    #[derive(Debug)]
    struct ContextErrorProvider {
        calls: AtomicUsize,
        seen_compacted: AtomicUsize,
    }
    #[async_trait]
    impl LlmProvider for ContextErrorProvider {
        fn name(&self) -> &str {
            "context-error-test"
        }
        async fn chat_stream(&self, req: ChatRequest) -> anyhow::Result<ChatStream> {
            let n = self.calls.fetch_add(1, Ordering::SeqCst);
            if n == 0 {
                Err(anyhow::anyhow!(
                    "This model's maximum context length is 8192 tokens. \
                     However, your messages resulted in 12000 tokens."
                ))
            } else {
                assert!(
                    req.messages.len() < 8,
                    "retried request must be compacted, got {} messages",
                    req.messages.len()
                );
                self.seen_compacted.fetch_add(1, Ordering::SeqCst);
                Ok(Box::new(futures_util::stream::iter(vec![Ok(
                    crate::llm::provider::ChatChunk {
                        delta: "AFTER-RETRY".into(),
                        thinking: false,
                        done: true,
                        tool_calls_requested: false,
                        usage: None,
                    },
                )])))
            }
        }
        async fn chat_complete(&self, _req: ChatRequest) -> anyhow::Result<ChatComplete> {
            unreachable!("no tool calls in this test")
        }
    }

    #[test]
    fn recoverable_error_classification() {
        let ctx = anyhow::anyhow!("maximum context length is 8192 tokens");
        assert!(is_recoverable_llm_error(&ctx));
        let too_long = anyhow::anyhow!("This model's maximum context length");
        assert!(is_recoverable_llm_error(&too_long));
        let malformed = anyhow::anyhow!("previous response contained invalid tool_use arguments");
        assert!(is_recoverable_llm_error(&malformed));
        let unrecoverable = anyhow::anyhow!("rate limit exceeded, retry in 60s");
        assert!(!is_recoverable_llm_error(&unrecoverable));
        let network = anyhow::anyhow!("failed to connect to 127.0.0.1:15722");
        assert!(!is_recoverable_llm_error(&network));
    }

    #[tokio::test]
    async fn planning_interval_injects_checkpoints() {
        // interval=2: steps 0 and 2 get the checkpoint, step 1 does not.
        // The mock answers with a tool call on step 0, a plain answer
        // on step 1, and verifies checkpoint presence per request.
        let provider = PlanningProvider {
            calls: AtomicUsize::new(0),
            checkpoint_at: Arc::new(Mutex::new(Vec::new())),
        };
        let mut tools = crate::tools::ToolRegistry::new();
        tools.register(Arc::new(FastTool(Arc::new(Mutex::new(Vec::new())))));
        let config = AgentConfig {
            model: "m".into(),
            max_steps: 0,
            prompt_state: std::sync::Arc::new(std::sync::RwLock::new(
                crate::harness::PromptState {
                    system_prompt: "base".into(),
                    skills: vec![],
                    fingerprint: None,
                },
            )),
            max_inline_skills: 0,
            sync_dir: None,
            max_context_messages: 0,
            max_context_tokens: 0,
            goals: None,
            goals_dir: None,
            planning_interval: 2,
            hooks: None,
            session_sink: None,
        };
        let answer = run_messages(
            &provider,
            &mut tools,
            &config,
            &[ChatMessage::user("do the multi-step task")],
            None,
            None,
        )
        .await
        .expect("turn succeeds");
        assert_eq!(answer.output, "PLANNED-DONE");
        assert_eq!(answer.state, RunState::Success);
        let checkpoints = provider.checkpoint_at.lock().unwrap().clone();
        // Only step 0 (first iteration) carries the checkpoint: step 1's
        // request had it stripped, and there is no step 2 in this run.
        assert_eq!(checkpoints, vec![0], "checkpoint only on step 0");
    }

    /// Step 0: emits a tool call (fast_tool). Step 1: plain answer.
    /// Step 2: plain answer; records which requests carried the
    /// PLANNING CHECKPOINT prompt.
    #[derive(Debug)]
    struct PlanningProvider {
        calls: AtomicUsize,
        checkpoint_at: Arc<Mutex<Vec<usize>>>,
    }
    #[async_trait]
    impl LlmProvider for PlanningProvider {
        fn name(&self) -> &str {
            "planning-test"
        }
        async fn chat_stream(&self, req: ChatRequest) -> anyhow::Result<ChatStream> {
            let n = self.calls.fetch_add(1, Ordering::SeqCst);
            let has_checkpoint = req
                .messages
                .iter()
                .any(|m| m.content.contains("PLANNING CHECKPOINT"));
            if has_checkpoint {
                self.checkpoint_at.lock().unwrap().push(n);
            }
            match n {
                0 => Ok(Box::new(futures_util::stream::iter(vec![Ok(
                    crate::llm::provider::ChatChunk {
                        delta: String::new(),
                        thinking: false,
                        done: true,
                        tool_calls_requested: true,
                        usage: None,
                    },
                )]))),
                _ => Ok(Box::new(futures_util::stream::iter(vec![Ok(
                    crate::llm::provider::ChatChunk {
                        delta: "PLANNED-DONE".into(),
                        thinking: false,
                        done: true,
                        tool_calls_requested: false,
                        usage: None,
                    },
                )]))),
            }
        }
        async fn chat_complete(&self, _req: ChatRequest) -> anyhow::Result<ChatComplete> {
            Ok(ChatComplete {
                text: String::new(),
                tool_calls: vec![ToolCall {
                    id: "call-p".into(),
                    name: "fast_tool".into(),
                    arguments: serde_json::json!({}),
                }],
                usage: None,
            })
        }
    }

    #[tokio::test]
    async fn parallel_tool_calls_execute_concurrently_and_keep_order() {
        let starts: Arc<Mutex<Vec<Instant>>> = Arc::new(Mutex::new(Vec::new()));
        let mut tools = crate::tools::ToolRegistry::new();
        tools.register(Arc::new(SlowTool(starts.clone())));
        tools.register(Arc::new(FastTool(starts.clone())));

        let provider = TwoCallProvider {
            calls: AtomicUsize::new(0),
        };
        let config = test_config();

        let answer = run_messages(
            &provider,
            &mut tools,
            &config,
            &[ChatMessage::user("run both tools")],
            None,
            None,
        )
        .await
        .expect("turn succeeds");
        assert_eq!(answer.output, "DONE");
        assert_eq!(answer.state, RunState::Success);

        // Both tools ran: two recorded start instants.
        let recorded = starts.lock().unwrap();
        assert_eq!(recorded.len(), 2, "both tools must run");
        // Concurrency proof: the fast tool started while the slow one
        // was still sleeping (its start is < 200ms before the slow
        // tool finished — far below a serial 200ms gap).
        let gap = recorded[1].duration_since(recorded[0]).as_millis();
        assert!(
            gap < 150,
            "fast tool started {gap}ms after slow tool — expected <150ms (concurrent), got serial execution"
        );
    }

    #[tokio::test]
    async fn session_sink_emits_tool_call_and_result_events() {
        // Phase 63 event-sourced logging: with a session_sink wired,
        // the loop must emit one ToolCall event per requested call and
        // one ToolResult per outcome, in order.
        let mut tools = crate::tools::ToolRegistry::new();
        tools.register(Arc::new(SlowTool(Arc::new(Mutex::new(Vec::new())))));
        tools.register(Arc::new(FastTool(Arc::new(Mutex::new(Vec::new())))));

        let provider = TwoCallProvider {
            calls: AtomicUsize::new(0),
        };
        let events: Arc<Mutex<Vec<crate::session::Event>>> = Arc::new(Mutex::new(Vec::new()));
        let mut config = test_config();
        let sink = events.clone();
        config.session_sink = Some(std::sync::Arc::new(move |ev| sink.lock().unwrap().push(ev)));

        let answer = run_messages(
            &provider,
            &mut tools,
            &config,
            &[ChatMessage::user("run both tools")],
            None,
            None,
        )
        .await
        .expect("turn succeeds");
        assert_eq!(answer.output, "DONE");

        {
            let evs = events.lock().unwrap();
            // Phase A2 补充了结构/元数据事件（TurnStart/StepStart/RequestHeader/Assistant），
            // 此处验证核心契约：2 个 ToolCall + 2 个 ToolResult，且顺序为
            // "全部调用先于任何结果"（calls 在 plan 时发出，results 在执行后）。
            let calls: Vec<_> = evs.iter().filter(|e| e.kind() == "tool_call").collect();
            let results: Vec<_> = evs.iter().filter(|e| e.kind() == "tool_result").collect();
            assert_eq!(calls.len(), 2, "expected 2 tool_call events, got {evs:?}");
            assert_eq!(
                results.len(),
                2,
                "expected 2 tool_result events, got {evs:?}"
            );
            let first_result_idx = evs.iter().position(|e| e.kind() == "tool_result").unwrap();
            let last_call_idx = evs.iter().rposition(|e| e.kind() == "tool_call").unwrap();
            assert!(
                last_call_idx < first_result_idx,
                "all calls must precede any result, got {evs:?}"
            );
            // 结构化事件存在（Phase A2 注入即事件）。
            assert!(evs.iter().any(|e| e.kind() == "turn_start"));
            assert!(evs.iter().any(|e| e.kind() == "request_header"));
            // Names match the requested tools; both succeeded.
            match &calls[0] {
                crate::session::Event::ToolCall { name, .. } => assert_eq!(name, "slow_tool"),
                other => panic!("expected tool_call, got {other:?}"),
            }
            match &results[0] {
                crate::session::Event::ToolResult { ok, output, .. } => {
                    assert!(*ok, "slow_tool should succeed");
                    assert!(!output.is_empty());
                }
                other => panic!("expected tool_result, got {other:?}"),
            }
        } // lock released here, before the second turn awaits
        // No session_sink → turn still succeeds (regression guard).
        let config2 = test_config();
        let provider2 = TwoCallProvider {
            calls: AtomicUsize::new(0),
        };
        let mut tools2 = crate::tools::ToolRegistry::new();
        tools2.register(Arc::new(SlowTool(Arc::new(Mutex::new(Vec::new())))));
        tools2.register(Arc::new(FastTool(Arc::new(Mutex::new(Vec::new())))));
        let _ = run_messages(
            &provider2,
            &mut tools2,
            &config2,
            &[ChatMessage::user("run both tools")],
            None,
            None,
        )
        .await
        .expect("turn succeeds");
    }

    /// Returns "OUT" immediately — the tool the hook tests drive.
    struct EchoTool;
    #[async_trait]
    impl crate::tools::Tool for EchoTool {
        fn name(&self) -> &str {
            "echo_tool"
        }
        fn description(&self) -> &str {
            "echo for hook test"
        }
        fn parameters(&self) -> serde_json::Value {
            serde_json::json!({})
        }
        async fn run(&self, _args: &serde_json::Value) -> anyhow::Result<crate::tools::ToolOutput> {
            Ok(crate::tools::ToolOutput { text: "OUT".into() })
        }
    }

    /// Step 0: one tool call. Step 1: verifies the tool_result message
    /// was rewritten by the on_tool_result hook, then answers.
    #[derive(Debug)]
    struct HookResultProvider {
        calls: AtomicUsize,
        saw_rewrite: AtomicBool,
    }
    #[async_trait]
    impl LlmProvider for HookResultProvider {
        fn name(&self) -> &str {
            "hook-result-test"
        }
        async fn chat_stream(&self, req: ChatRequest) -> anyhow::Result<ChatStream> {
            let n = self.calls.fetch_add(1, Ordering::SeqCst);
            match n {
                0 => Ok(Box::new(futures_util::stream::iter(vec![Ok(
                    crate::llm::provider::ChatChunk {
                        delta: String::new(),
                        thinking: false,
                        done: true,
                        tool_calls_requested: true,
                        usage: None,
                    },
                )]))),
                _ => {
                    let rewritten = req
                        .messages
                        .iter()
                        .filter(|m| matches!(m.role, crate::llm::provider::Role::Tool))
                        .any(|m| m.content.contains("[hook]"));
                    self.saw_rewrite.store(rewritten, Ordering::SeqCst);
                    Ok(Box::new(futures_util::stream::iter(vec![Ok(
                        crate::llm::provider::ChatChunk {
                            delta: "HOOKED-DONE".into(),
                            thinking: false,
                            done: true,
                            tool_calls_requested: false,
                            usage: None,
                        },
                    )])))
                }
            }
        }
        async fn chat_complete(&self, _req: ChatRequest) -> anyhow::Result<ChatComplete> {
            Ok(ChatComplete {
                text: String::new(),
                tool_calls: vec![ToolCall {
                    id: "c1".into(),
                    name: "echo_tool".into(),
                    arguments: serde_json::json!({}),
                }],
                usage: None,
            })
        }
    }

    #[tokio::test]
    async fn on_tool_result_hook_rewrites_tool_output() {
        let hooks_dir = std::env::temp_dir()
            .join("sigil-agent-hooks-tests")
            .join("result");
        std::fs::create_dir_all(&hooks_dir).unwrap();
        std::fs::write(
            hooks_dir.join("on_tool_result.lua"),
            "return function(ctx) return ctx.result .. ' [hook]' end",
        )
        .unwrap();
        let lua = std::sync::Arc::new(std::sync::Mutex::new(
            crate::lua::LuaRuntime::new().unwrap(),
        ));
        let mut config = test_config();
        config.hooks = Some(std::sync::Arc::new(crate::hooks::HookRunner::new(
            lua, hooks_dir,
        )));

        let mut tools = crate::tools::ToolRegistry::new();
        tools.register(Arc::new(EchoTool));
        let provider = HookResultProvider {
            calls: AtomicUsize::new(0),
            saw_rewrite: AtomicBool::new(false),
        };

        let answer = run_messages(
            &provider,
            &mut tools,
            &config,
            &[ChatMessage::user("run echo")],
            None,
            None,
        )
        .await
        .expect("turn succeeds");
        assert_eq!(answer.output, "HOOKED-DONE");
        assert!(
            provider.saw_rewrite.load(Ordering::SeqCst),
            "tool_result must be rewritten by the on_tool_result hook"
        );
    }

    /// Step 0: verifies the first request carries the on_turn_start
    /// hook's injected system message, then emits a tool call; step 1
    /// answers.
    #[derive(Debug)]
    struct TurnStartProvider {
        calls: AtomicUsize,
        saw_inject: AtomicBool,
    }
    #[async_trait]
    impl LlmProvider for TurnStartProvider {
        fn name(&self) -> &str {
            "hook-turn-start-test"
        }
        async fn chat_stream(&self, req: ChatRequest) -> anyhow::Result<ChatStream> {
            let n = self.calls.fetch_add(1, Ordering::SeqCst);
            match n {
                0 => {
                    let injected = req
                        .messages
                        .iter()
                        .filter(|m| matches!(m.role, crate::llm::provider::Role::System))
                        .any(|m| m.content.contains("TURN-START-HOOK"));
                    self.saw_inject.store(injected, Ordering::SeqCst);
                    Ok(Box::new(futures_util::stream::iter(vec![Ok(
                        crate::llm::provider::ChatChunk {
                            delta: String::new(),
                            thinking: false,
                            done: true,
                            tool_calls_requested: true,
                            usage: None,
                        },
                    )])))
                }
                _ => Ok(Box::new(futures_util::stream::iter(vec![Ok(
                    crate::llm::provider::ChatChunk {
                        delta: "INJECTED-DONE".into(),
                        thinking: false,
                        done: true,
                        tool_calls_requested: false,
                        usage: None,
                    },
                )]))),
            }
        }
        async fn chat_complete(&self, _req: ChatRequest) -> anyhow::Result<ChatComplete> {
            Ok(ChatComplete {
                text: String::new(),
                tool_calls: vec![ToolCall {
                    id: "c1".into(),
                    name: "echo_tool".into(),
                    arguments: serde_json::json!({}),
                }],
                usage: None,
            })
        }
    }

    #[tokio::test]
    async fn on_turn_start_hook_injects_system_message() {
        let hooks_dir = std::env::temp_dir()
            .join("sigil-agent-hooks-tests")
            .join("turn-start");
        std::fs::create_dir_all(&hooks_dir).unwrap();
        std::fs::write(
            hooks_dir.join("on_turn_start.lua"),
            "return function(ctx) return 'TURN-START-HOOK step=' .. ctx.step end",
        )
        .unwrap();
        let lua = std::sync::Arc::new(std::sync::Mutex::new(
            crate::lua::LuaRuntime::new().unwrap(),
        ));
        let mut config = test_config();
        config.hooks = Some(std::sync::Arc::new(crate::hooks::HookRunner::new(
            lua, hooks_dir,
        )));

        let mut tools = crate::tools::ToolRegistry::new();
        tools.register(Arc::new(EchoTool));
        let provider = TurnStartProvider {
            calls: AtomicUsize::new(0),
            saw_inject: AtomicBool::new(false),
        };

        let answer = run_messages(
            &provider,
            &mut tools,
            &config,
            &[ChatMessage::user("run echo")],
            None,
            None,
        )
        .await
        .expect("turn succeeds");
        assert_eq!(answer.output, "INJECTED-DONE");
        assert!(
            provider.saw_inject.load(Ordering::SeqCst),
            "on_turn_start hook text must appear in the first request"
        );
    }

    /// P0 (serial decision): an `on_pre_tool` listener that returns `true`
    /// for a tool must block the call before it runs — the tool never
    /// executes and the model sees the veto reason as the tool result.
    #[derive(Debug)]
    struct PreToolVetoProvider {
        calls: AtomicUsize,
        saw_veto: AtomicBool,
    }
    #[async_trait]
    impl LlmProvider for PreToolVetoProvider {
        fn name(&self) -> &str {
            "pre-tool-veto-test"
        }
        async fn chat_stream(&self, req: ChatRequest) -> anyhow::Result<ChatStream> {
            let n = self.calls.fetch_add(1, Ordering::SeqCst);
            if n == 0 {
                Ok(Box::new(futures_util::stream::iter(vec![Ok(
                    crate::llm::provider::ChatChunk {
                        delta: String::new(),
                        thinking: false,
                        done: true,
                        tool_calls_requested: true,
                        usage: None,
                    },
                )])))
            } else {
                let vetoed = req
                    .messages
                    .iter()
                    .filter(|m| matches!(m.role, crate::llm::provider::Role::Tool))
                    .any(|m| m.content.contains("blocked by on_pre_tool hook"));
                self.saw_veto.store(vetoed, Ordering::SeqCst);
                Ok(Box::new(futures_util::stream::iter(vec![Ok(
                    crate::llm::provider::ChatChunk {
                        delta: "VETO-DONE".into(),
                        thinking: false,
                        done: true,
                        tool_calls_requested: false,
                        usage: None,
                    },
                )])))
            }
        }
        async fn chat_complete(&self, _req: ChatRequest) -> anyhow::Result<ChatComplete> {
            Ok(ChatComplete {
                text: String::new(),
                tool_calls: vec![ToolCall {
                    id: "c1".into(),
                    name: "echo_tool".into(),
                    arguments: serde_json::json!({}),
                }],
                usage: None,
            })
        }
    }

    #[tokio::test]
    async fn on_pre_tool_hook_vetoes_tool_call() {
        let hooks_dir = std::env::temp_dir()
            .join("sigil-agent-hooks-tests")
            .join("pre-tool-veto");
        std::fs::create_dir_all(&hooks_dir).unwrap();
        let slot = hooks_dir.join("on_pre_tool.d");
        std::fs::create_dir_all(&slot).unwrap();
        // A serial veto: the first non-null verdict wins. This listener
        // vetoes echo_tool specifically; returning `true` blocks the call.
        std::fs::write(
            slot.join("01_safety.lua"),
            "return function(ctx) return ctx.name == 'echo_tool' end",
        )
        .unwrap();
        let lua = std::sync::Arc::new(std::sync::Mutex::new(
            crate::lua::LuaRuntime::new().unwrap(),
        ));
        let mut config = test_config();
        config.hooks = Some(std::sync::Arc::new(crate::hooks::HookRunner::new(
            lua, hooks_dir,
        )));

        let mut tools = crate::tools::ToolRegistry::new();
        tools.register(Arc::new(EchoTool));
        let provider = PreToolVetoProvider {
            calls: AtomicUsize::new(0),
            saw_veto: AtomicBool::new(false),
        };

        let answer = run_messages(
            &provider,
            &mut tools,
            &config,
            &[ChatMessage::user("run echo")],
            None,
            None,
        )
        .await
        .expect("turn succeeds");
        assert_eq!(answer.output, "VETO-DONE");
        assert!(
            provider.saw_veto.load(Ordering::SeqCst),
            "the vetoes echo_tool call must reach the model as a blocked result"
        );
    }

    /// P0 (emit observation): when the turn ends with a final answer, an
    /// `on_done` listener must observe it. The hook is observational, so it
    /// signals by writing a marker file through a host helper.
    #[derive(Debug)]
    struct DoneAnswerProvider;
    #[async_trait]
    impl LlmProvider for DoneAnswerProvider {
        fn name(&self) -> &str {
            "done-answer-test"
        }
        async fn chat_stream(&self, _req: ChatRequest) -> anyhow::Result<ChatStream> {
            Ok(Box::new(futures_util::stream::iter(vec![Ok(
                crate::llm::provider::ChatChunk {
                    delta: "DONE-ANSWER".into(),
                    thinking: false,
                    done: true,
                    tool_calls_requested: false,
                    usage: None,
                },
            )])))
        }
        async fn chat_complete(&self, _req: ChatRequest) -> anyhow::Result<ChatComplete> {
            unreachable!("no tool calls in this test")
        }
    }

    #[tokio::test]
    async fn on_done_hook_observes_final_answer() {
        let base = std::env::temp_dir()
            .join("sigil-agent-hooks-tests")
            .join("done");
        std::fs::create_dir_all(&base).unwrap();
        let slot = base.join("on_done.d");
        std::fs::create_dir_all(&slot).unwrap();
        let marker = base.join("on_done_marker.txt");
        let _ = std::fs::remove_file(&marker);
        // The path is embedded inside a Lua string literal; backslashes are
        // Lua escape characters, so normalize to forward slashes (Windows
        // file APIs accept `/`). This is a test-only concern — production
        // hook files are user-authored with valid paths.
        let marker_s = marker.to_string_lossy().replace('\\', "/");
        // The emit listener writes a marker carrying the observed output —
        // proving on_done fired and received the final answer.
        std::fs::write(
            slot.join("01.log.lua"),
            &format!(
                "return function(ctx) host.write_file('{marker_s}', 'on_done:' .. ctx.output); return nil end"
            ),
        )
        .unwrap();
        let lua = std::sync::Arc::new(std::sync::Mutex::new(
            crate::lua::LuaRuntime::new().unwrap(),
        ));
        let mut config = test_config();
        config.hooks = Some(std::sync::Arc::new(crate::hooks::HookRunner::new(
            lua, base,
        )));

        let mut tools = crate::tools::ToolRegistry::new();
        let provider = DoneAnswerProvider;

        let answer = run_messages(
            &provider,
            &mut tools,
            &config,
            &[ChatMessage::user("answer")],
            None,
            None,
        )
        .await
        .expect("turn succeeds");
        assert_eq!(answer.output, "DONE-ANSWER");
        let written = std::fs::read_to_string(&marker).expect("on_done marker written");
        assert_eq!(
            written, "on_done:DONE-ANSWER",
            "on_done hook must observe the final answer"
        );
    }

    #[test]
    fn on_prompt_build_hook_appends_by_default() {
        let hooks_dir = std::env::temp_dir()
            .join("sigil-agent-hooks-tests")
            .join("prompt-build");
        std::fs::create_dir_all(&hooks_dir).unwrap();
        std::fs::write(
            hooks_dir.join("on_prompt_build.lua"),
            "return function(ctx) return 'CUSTOM:' .. ctx.user_prompt end",
        )
        .unwrap();
        let lua = std::sync::Arc::new(std::sync::Mutex::new(
            crate::lua::LuaRuntime::new().unwrap(),
        ));
        let mut config = test_config();
        config.hooks = Some(std::sync::Arc::new(crate::hooks::HookRunner::new(
            lua, hooks_dir,
        )));

        // Cache discipline: hook text is appended, base prefix preserved.
        let out = config.system_prompt_for("hello");
        assert!(out.starts_with("base"), "prefix must stay byte-stable");
        assert!(out.ends_with("CUSTOM:hello"), "hook text appended");
        // The base prompt is still reachable through ctx.
        let dir = config
            .hooks
            .as_ref()
            .map(|h| h.hooks_dir().to_path_buf())
            .unwrap();
        std::fs::write(
            dir.join("on_prompt_build.lua"),
            "return function(ctx) return ctx.base .. '|APPENDED' end",
        )
        .unwrap();
        let out2 = config.system_prompt_for("hi");
        assert!(out2.starts_with("base"), "prefix still byte-stable");
        assert!(out2.ends_with("base|APPENDED"));
    }

    #[test]
    fn on_prompt_build_replace_all_escape_hatch() {
        let hooks_dir = std::env::temp_dir()
            .join("sigil-agent-hooks-tests")
            .join("prompt-build-replace-all");
        std::fs::create_dir_all(&hooks_dir).unwrap();
        std::fs::write(
            hooks_dir.join("on_prompt_build.lua"),
            "return function(ctx) return '__REPLACE_ALL__FULL:' .. ctx.user_prompt end",
        )
        .unwrap();
        let lua = std::sync::Arc::new(std::sync::Mutex::new(
            crate::lua::LuaRuntime::new().unwrap(),
        ));
        let mut config = test_config();
        config.hooks = Some(std::sync::Arc::new(crate::hooks::HookRunner::new(
            lua, hooks_dir,
        )));

        assert_eq!(config.system_prompt_for("hello"), "FULL:hello");
    }

    #[test]
    fn on_prompt_build_hook_error_falls_back_to_base() {
        let hooks_dir = std::env::temp_dir()
            .join("sigil-agent-hooks-tests")
            .join("prompt-build-error");
        std::fs::create_dir_all(&hooks_dir).unwrap();
        std::fs::write(
            hooks_dir.join("on_prompt_build.lua"),
            "return function(ctx) error('boom') end",
        )
        .unwrap();
        let lua = std::sync::Arc::new(std::sync::Mutex::new(
            crate::lua::LuaRuntime::new().unwrap(),
        ));
        let mut config = test_config();
        config.hooks = Some(std::sync::Arc::new(crate::hooks::HookRunner::new(
            lua, hooks_dir,
        )));

        // Prompt assembly must never break: hook error → base prompt.
        assert_eq!(config.system_prompt_for("hello"), "base");
    }

    #[tokio::test]
    async fn run_messages_injects_system_prompt_when_missing() {
        // REPL / pipe mode pass user-only message lists; the loop must
        // inject the harness system prompt (and fire on_prompt_build).
        #[derive(Debug)]
        struct SysCheckProvider {
            calls: AtomicUsize,
            saw_system: AtomicBool,
        }
        #[async_trait]
        impl LlmProvider for SysCheckProvider {
            fn name(&self) -> &str {
                "sys-check-test"
            }
            async fn chat_stream(&self, req: ChatRequest) -> anyhow::Result<ChatStream> {
                let n = self.calls.fetch_add(1, Ordering::SeqCst);
                match n {
                    0 => {
                        let sys = req
                            .messages
                            .iter()
                            .find(|m| matches!(m.role, crate::llm::provider::Role::System));
                        self.saw_system.store(
                            sys.is_some_and(|m| m.content.contains("base")),
                            Ordering::SeqCst,
                        );
                        Ok(Box::new(futures_util::stream::iter(vec![Ok(
                            crate::llm::provider::ChatChunk {
                                delta: String::new(),
                                thinking: false,
                                done: true,
                                tool_calls_requested: true,
                                usage: None,
                            },
                        )])))
                    }
                    _ => Ok(Box::new(futures_util::stream::iter(vec![Ok(
                        crate::llm::provider::ChatChunk {
                            delta: "SYS-OK".into(),
                            thinking: false,
                            done: true,
                            tool_calls_requested: false,
                            usage: None,
                        },
                    )]))),
                }
            }
            async fn chat_complete(&self, _req: ChatRequest) -> anyhow::Result<ChatComplete> {
                Ok(ChatComplete {
                    text: String::new(),
                    tool_calls: vec![ToolCall {
                        id: "c1".into(),
                        name: "echo_tool".into(),
                        arguments: serde_json::json!({}),
                    }],
                    usage: None,
                })
            }
        }

        let mut tools = crate::tools::ToolRegistry::new();
        tools.register(Arc::new(EchoTool));
        let provider = SysCheckProvider {
            calls: AtomicUsize::new(0),
            saw_system: AtomicBool::new(false),
        };
        let config = test_config();

        let answer = run_messages(
            &provider,
            &mut tools,
            &config,
            &[ChatMessage::user("hello")],
            None,
            None,
        )
        .await
        .expect("turn succeeds");
        assert_eq!(answer.output, "SYS-OK");
        assert!(
            provider.saw_system.load(Ordering::SeqCst),
            "user-only message list must get the harness system prompt injected"
        );
    }

    #[tokio::test]
    async fn parallel_tool_calls_survive_unknown_tool() {
        // One call names a tool that does not exist: the turn must not
        // abort — the model gets an error tool_result and can answer.
        let starts: Arc<Mutex<Vec<Instant>>> = Arc::new(Mutex::new(Vec::new()));
        let mut tools = crate::tools::ToolRegistry::new();
        tools.register(Arc::new(FastTool(starts.clone())));

        let provider = UnknownToolProvider {
            calls: AtomicUsize::new(0),
        };
        let config = test_config();
        let answer = run_messages(
            &provider,
            &mut tools,
            &config,
            &[ChatMessage::user("do the thing")],
            None,
            None,
        )
        .await
        .expect("unknown tool must not abort the turn");
        assert!(answer.output.contains("RECOVERED"));
    }

    /// Always asks for tools, so the loop can only end via the step
    /// budget — which must yield a wrap-up summary, not an error.
    #[derive(Debug)]
    struct EndlessToolProvider {
        calls: AtomicUsize,
    }
    #[async_trait]
    impl LlmProvider for EndlessToolProvider {
        fn name(&self) -> &str {
            "endless-tools-test"
        }
        async fn chat_stream(&self, req: ChatRequest) -> anyhow::Result<ChatStream> {
            let _ = self.calls.fetch_add(1, Ordering::SeqCst);
            // The final (tool-less) call is provide_final_answer: it
            // must return a plain summary, not request more tools.
            let is_summary = req.tools.is_empty();
            let delta = if is_summary {
                "WRAP-UP-SUMMARY".to_string()
            } else {
                String::new()
            };
            Ok(Box::new(futures_util::stream::iter(vec![Ok(
                crate::llm::provider::ChatChunk {
                    delta,
                    thinking: false,
                    done: true,
                    tool_calls_requested: !is_summary,
                    usage: None,
                },
            )])))
        }
        async fn chat_complete(&self, _req: ChatRequest) -> anyhow::Result<ChatComplete> {
            Ok(ChatComplete {
                text: String::new(),
                tool_calls: vec![ToolCall {
                    id: "call-endless".into(),
                    name: "fast_tool".into(),
                    arguments: serde_json::json!({}),
                }],
                usage: None,
            })
        }
    }

    #[tokio::test]
    async fn step_limit_returns_wrap_up_summary_not_error() {
        let starts: Arc<Mutex<Vec<Instant>>> = Arc::new(Mutex::new(Vec::new()));
        let mut tools = crate::tools::ToolRegistry::new();
        tools.register(Arc::new(FastTool(starts.clone())));

        let provider = EndlessToolProvider {
            calls: AtomicUsize::new(0),
        };
        let mut config = test_config();
        config.max_steps = 3;
        let result = run_messages(
            &provider,
            &mut tools,
            &config,
            &[ChatMessage::user("keep going")],
            None,
            None,
        )
        .await
        .expect("step limit is a normal result, not an error");
        assert_eq!(result.state, RunState::StepLimitReached);
        assert_eq!(result.output, "WRAP-UP-SUMMARY");
        // 3 budgeted loop iterations + 1 final summary call.
        assert_eq!(provider.calls.load(Ordering::SeqCst), 4);
    }

    /// Emits [unknown_tool, fast_tool] then checks the model received
    /// an error for the unknown one and a real result for fast_tool.
    #[derive(Debug)]
    struct UnknownToolProvider {
        calls: AtomicUsize,
    }
    #[async_trait]
    impl LlmProvider for UnknownToolProvider {
        fn name(&self) -> &str {
            "unknown-tool-test"
        }
        async fn chat_stream(&self, req: ChatRequest) -> anyhow::Result<ChatStream> {
            let n = self.calls.fetch_add(1, Ordering::SeqCst);
            if n == 0 {
                Ok(Box::new(futures_util::stream::iter(vec![Ok(
                    crate::llm::provider::ChatChunk {
                        delta: String::new(),
                        thinking: false,
                        done: true,
                        tool_calls_requested: true,
                        usage: None,
                    },
                )])))
            } else {
                // Second loop iteration: verify results for both calls
                // (error for the unknown one, real result for fast).
                let results: Vec<&str> = req
                    .messages
                    .iter()
                    .filter(|m| m.role == crate::llm::provider::Role::Tool)
                    .map(|m| m.content.as_str())
                    .collect();
                assert_eq!(results.len(), 2);
                assert!(results[0].contains("error"), "unknown tool -> error result");
                assert_eq!(results[1], "FAST");
                Ok(Box::new(futures_util::stream::iter(vec![Ok(
                    crate::llm::provider::ChatChunk {
                        delta: "RECOVERED".into(),
                        thinking: false,
                        done: true,
                        tool_calls_requested: false,
                        usage: None,
                    },
                )])))
            }
        }
        async fn chat_complete(&self, _req: ChatRequest) -> anyhow::Result<ChatComplete> {
            // First-iteration replay only.
            Ok(ChatComplete {
                text: String::new(),
                tool_calls: vec![
                    ToolCall {
                        id: "call-x".into(),
                        name: "no_such_tool".into(),
                        arguments: serde_json::json!({}),
                    },
                    ToolCall {
                        id: "call-y".into(),
                        name: "fast_tool".into(),
                        arguments: serde_json::json!({}),
                    },
                ],
                usage: None,
            })
        }
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
    fn goals_injection_replaces_not_duplicates() {
        let mut m = vec![ChatMessage::system("SYS"), ChatMessage::user("user-1")];
        // First injection: inserted after the first system message.
        inject_goals_block(&mut m, "## Active goals\n- g1: ship it");
        assert_eq!(m.len(), 3);
        assert_eq!(m[1].content, "## Active goals\n- g1: ship it");
        // Second iteration of the agent loop: replaces in place.
        inject_goals_block(&mut m, "## Active goals\n- g1: ship it\n- g2: new goal");
        assert_eq!(m.len(), 3, "must not stack duplicate goals blocks");
        assert_eq!(
            m[1].content,
            "## Active goals\n- g1: ship it\n- g2: new goal"
        );
        // Third iteration with no goals: block stays replaced, not stacked.
        inject_goals_block(&mut m, "## Active goals\n- g2: new goal");
        assert_eq!(m.len(), 3);
        assert_eq!(m[1].content, "## Active goals\n- g2: new goal");
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
        let c = compact_messages_with_budget(&m, 0, 200, None);
        assert!(c.len() < m.len(), "expected compression");
        // Summary + kept tail: kept tail alone respects the budget.
        let kept_tail: usize = c[2..].iter().map(estimate_message_tokens).sum();
        assert!(
            kept_tail <= 260,
            "kept tail {kept_tail} tokens exceeds budget with slack"
        );
        // No budget configured → no-op even for a huge transcript.
        assert_eq!(compact_messages_with_budget(&m, 0, 0, None).len(), m.len());
    }

    #[test]
    fn compact_triggered_by_real_usage_when_estimate_within_budget() {
        // A transcript whose chars/4 estimate sits *under* the budget…
        let mut m = vec![ChatMessage::system("sys")];
        for i in 0..10 {
            m.push(ChatMessage::user(format!("user-{i} short line")));
            m.push(ChatMessage::assistant(format!("assistant-{i} short reply")));
        }
        let total: usize = m.iter().map(estimate_message_tokens).sum();
        assert!(total <= 200, "test transcript too big: {total}");
        // …must NOT compress on the heuristic alone…
        assert_eq!(
            compact_messages_with_budget(&m, 0, 200, None).len(),
            m.len()
        );
        // …but MUST once real usage (CJK-heavy, ~10x the estimate) says
        // the transcript is actually over budget (prime-agent: use
        // actual usage, fall back to chars/4).
        let c = compact_messages_with_budget(&m, 0, 200, Some(10 * total));
        assert!(c.len() < m.len(), "expected compression via real usage");
    }

    #[test]
    fn compact_calibrates_tail_by_real_usage_ratio() {
        let mut m = vec![ChatMessage::system("sys")];
        for i in 0..20 {
            m.push(ChatMessage::user(format!("user-{i} line")));
            m.push(ChatMessage::assistant(format!("assistant-{i} reply")));
        }
        // Uncalibrated walk keeps the tail that *estimates* to 1000…
        let unc = compact_messages_with_budget(&m, 0, 1000, None);
        // …calibrated walk (real usage = 10x estimate) keeps a tail that
        // estimates to ~100, i.e. far less transcript survives.
        let cal = compact_messages_with_budget(&m, 0, 1000, Some(10 * 1000));
        assert!(cal.len() < unc.len(), "calibration must trim more");
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
        let c = compact_messages_with_budget(&m, 0, 5, None);
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
    fn compact_file_history_survives_second_compaction() {
        // First compaction folds turns with read_file/write_file calls.
        let m1 = vec![
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
        let c1 = compact_messages_with_budget(&m1, 0, 5, None);
        let summary1 = c1
            .iter()
            .find(|m| m.role == Role::System && m.content.contains("compressed"))
            .expect("first summary present");
        assert!(summary1.content.contains("Files read: src/main.rs"));

        // Second compaction: the first summary is now a system message in
        // the transcript, plus a new turn touching a NEW file. The new
        // summary must accumulate BOTH histories — the old files and the
        // new one (Pi compaction.details accumulates the same way).
        let mut m2 = c1;
        m2.push(ChatMessage::user("new task"));
        m2.push(ChatMessage::assistant_with_tools(
            "wrote another",
            vec![crate::llm::provider::ToolCall {
                id: "c3".into(),
                name: "write_file".into(),
                arguments: serde_json::json!({"path": "src/lib.rs"}),
            }],
        ));
        m2.push(ChatMessage::tool_result("ok", "c3"));
        m2.push(ChatMessage::user("second-newest"));

        let c2 = compact_messages_with_budget(&m2, 0, 5, None);
        let summary2 = c2
            .iter()
            .find(|m| m.role == Role::System && m.content.contains("compressed"))
            .expect("second summary present");
        assert!(
            summary2.content.contains("Files read: src/main.rs"),
            "first-compaction read file lost: {}",
            summary2.content
        );
        assert!(
            summary2.content.contains("src/config.rs") && summary2.content.contains("src/lib.rs"),
            "modified files not accumulated: {}",
            summary2.content
        );
    }

    #[test]
    fn estimate_tokens_chars_over_four() {
        assert_eq!(estimate_tokens(""), 1);
        assert_eq!(estimate_tokens("abcd"), 1);
        assert_eq!(estimate_tokens("abcdefgh"), 2);
    }
}
