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

mod context;
mod gate;
mod scheduler;
#[cfg(test)]
use context::compact_messages;
pub use context::compact_messages_with_budget;
#[cfg(test)]
use context::{estimate_message_tokens, estimate_tokens};
pub use gate::{GateVerdict, ToolGate};

pub mod invariant;
pub mod provider;

use anyhow::{Context, Result};
use futures_util::StreamExt;

use crate::llm::provider::{ChatComplete, ChatMessage, ChatRequest, LlmProvider, Role, TokenUsage};
use crate::safety;
use crate::tools::ToolRegistry;

#[cfg(test)]
mod compact_tests;
#[cfg(test)]
mod parallel_tests;
#[cfg(test)]
mod scheduler_tests;
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
    /// Estimated input-prompt threshold, including tool schemas. Real
    /// request usage calibrates current estimates; reserve output headroom
    /// when configuring this. `0` disables the token trigger.
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
    if !messages.iter().any(|m| matches!(m.role, Role::System)) {
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
    /// Consecutive tool rounds produced identical results (`SIGIL_NO_PROGRESS`):
    /// the loop stopped repeating them and asked for a wrap-up instead.
    NoProgress,
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

/// Some providers send usage after finish_reason, before [DONE]. Read only
/// that trailer, bounded in time and frame count; never create a new request.
async fn trailing_usage(
    stream: &mut crate::llm::provider::ChatStream,
    initial: Option<TokenUsage>,
) -> Option<TokenUsage> {
    let mut latest = initial;
    let _ = tokio::time::timeout(std::time::Duration::from_millis(250), async {
        for _ in 0..32 {
            match stream.next().await {
                Some(Ok(chunk)) => {
                    if let Some(usage) = chunk.usage {
                        latest = Some(usage);
                    }
                    if chunk.done {
                        break;
                    }
                }
                _ => break,
            }
        }
    })
    .await;
    latest
}

fn log_request_usage(config: &AgentConfig, kind: &str, usage: Option<TokenUsage>) {
    if let Some(sink) = &config.session_sink {
        sink(crate::session::Event::RequestUsage {
            ts: None,
            model: config.model.clone(),
            request_kind: kind.into(),
            usage,
        });
    }
    if std::env::var_os("SIGIL_DEBUG_CACHE").is_none() {
        return;
    }
    match usage {
        Some(u) => match u.cache_reported_prompt_tokens {
            Some(n) => {
                let pct = if n > 0 {
                    u.prompt_cache_hit_tokens as f64 / n as f64 * 100.0
                } else {
                    0.0
                };
                eprintln!(
                    "[cache] {kind}: {} prompt + {} completion; {}/{} prompt tokens cached ({:.0}%)",
                    u.prompt_tokens, u.completion_tokens, u.prompt_cache_hit_tokens, n, pct
                );
            }
            None => eprintln!(
                "[cache] {kind}: {} prompt + {} completion; cache telemetry unavailable",
                u.prompt_tokens, u.completion_tokens
            ),
        },
        None => eprintln!("[cache] {kind}: usage unavailable; no diagnostic request sent"),
    }
}

/// Refresh the harness prompt while retaining tool history and compaction summaries.
pub fn messages_for_turn(history: &[ChatMessage], system: String) -> Vec<ChatMessage> {
    let mut messages = history.to_vec();
    if messages.first().is_some_and(|m| {
        m.role == Role::System && !m.content.starts_with("[earlier conversation compressed]")
    }) {
        messages[0] = ChatMessage::system(system);
    } else {
        messages.insert(0, ChatMessage::system(system));
    }
    messages
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
        // Resumed event histories contain earlier user turns but no harness
        // system message. Relevance and prompt hooks must use the current turn.
        let latest_user = messages.iter().rev().find_map(|m| match m.role {
            crate::llm::provider::Role::User => Some(m.content.clone()),
            _ => None,
        });
        let system = config.system_prompt_for(latest_user.as_deref().unwrap_or(""));
        messages.insert(0, ChatMessage::system(system));
    }

    let breaker = safety::CircuitBreaker::new();
    let mut step: u32 = 0;
    let mut tokens: Option<TokenUsage> = None;
    let mut prompt_budget = context::PromptBudget::default();
    // Phase A2: turn 事件只写一次（run = 一个 turn，多 step）。
    let mut turn_started = false;
    // One retry after an EMPTY non-tool response (reasoning models can
    // put everything in reasoning_content, leaving content empty — a
    // silent Success("") is worse than asking once more).
    let mut retried_empty = false;
    // Scheduler ①: first-frame floor. One tool-less draft taken before the
    // loop so a run that spins out its budget still has something to say.
    let fallback_draft: Option<String> = if scheduler::first_frame_enabled() {
        match tool_less_call(
            provider,
            config,
            &messages,
            "Do not call any tools. Give your best answer to the user's request \
             now, as complete as you can make it in one message.",
            None,
            &prompt_budget,
        )
        .await
        {
            Ok((draft, _)) if !draft.trim().is_empty() => {
                eprintln!(
                    "[scheduler] first-frame draft captured ({} chars)",
                    draft.len()
                );
                Some(draft)
            }
            Ok(_) => None,
            Err(e) => {
                eprintln!("[scheduler] first-frame draft unavailable: {e:#}");
                None
            }
        }
    } else {
        None
    };
    // Scheduler ②: progress guard state.
    let no_progress_limit = scheduler::no_progress_limit();
    let mut last_round: Option<u64> = None;
    let mut stalled_rounds: u32 = 0;

    loop {
        // Cross-instance sync: if another sigil process edited the
        // harness dir since our last turn, reload the prompt state so
        // this instance sees the change (cheap fingerprint when idle).
        if let Some(dir) = &config.sync_dir {
            let _ = crate::harness::sync_prompt_state(&config.prompt_state, dir);
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
            let account = |goals: &mut Vec<crate::goals::GoalState>| {
                crate::goals::pause_exhausted(goals);
                let block = crate::goals::active_goals_block(goals);
                crate::goals::bump_turn_usage(goals);
                crate::goals::pause_exhausted(goals);
                Ok(block)
            };
            let block = if let Some(dir) = &config.goals_dir {
                let (block, updated) = crate::goals::update_goals(dir, account)?;
                *g = updated;
                block
            } else {
                account(&mut g)?
            };
            messages.retain(|m| {
                !(m.role == Role::System && m.content.starts_with(crate::goals::GOALS_MARKER))
            });
            if let Some(block) = block {
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
        // Structural cleanup: remove exactly the checkpoint we injected
        // (tagged messages), never a tool result or user text that
        // merely quotes the prompt (the old substring match could).
        // Legacy untagged checkpoints are still matched by content prefix.
        messages
            .retain(|m| !m.planning_checkpoint && !m.content.starts_with("PLANNING CHECKPOINT:"));
        if config.planning_interval > 0
            && (step == 0 || step.is_multiple_of(config.planning_interval))
        {
            messages.push(ChatMessage::planning_checkpoint());
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
            return wrap_up(
                provider,
                config,
                messages,
                stream_cb,
                &prompt_budget,
                tokens,
                step,
                RunState::StepLimitReached,
                fallback_draft,
            )
            .await;
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

        messages = prompt_budget.compact(
            &messages,
            &specs,
            config.max_context_messages,
            config.max_context_tokens,
        );

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

        let mut req = ChatRequest {
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
                    // Adopt the compacted transcript for the REST of the
                    // turn too, not just the retry: `messages` drives the
                    // remaining loop steps and `req` is what the
                    // tool-call replay re-sends below. Without this the
                    // replay reproduced the exact request that just
                    // failed (context overflow again) and every
                    // subsequent step re-paid a failed request +
                    // re-compaction.
                    messages = compacted.clone();
                    req = ChatRequest {
                        model: config.model.clone(),
                        messages: compacted,
                        tools: specs.clone(),
                        stream: true,
                        temperature: None,
                    };
                    provider
                        .chat_stream(req.clone())
                        .await
                        .context("starting completion (after compaction)")?
                } else {
                    return Err(e).context("starting completion");
                }
            }
            Err(e) => return Err(e).context("starting completion"),
        };

        let mut request_usage = None;
        let mut text = String::new();
        let mut tool_calls_requested = false;
        // One bounded mid-stream retry: a transient drop (502/429/reset)
        // after the request was accepted used to abort the whole turn
        // and discard every streamed token — the endpoint fallback chain
        // only covers the pre-headers phase. The retry restarts the
        // request from the CURRENT transcript and discards the partial
        // text (keeping it would duplicate content across the retry).
        let mut stream_retries = 0u8;
        // Streamed tool-call fragments accumulated across the stream;
        // cleared on retry along with the partial text.
        let mut streamed_fragments: Vec<crate::llm::provider::ToolCall> = Vec::new();
        loop {
            let mut stream_err: Option<anyhow::Error> = None;
            while let Some(chunk) = stream.next().await {
                match chunk {
                    Ok(chunk) => {
                        if chunk.usage.is_some() {
                            request_usage = chunk.usage;
                        }
                        if let Some(u) = chunk.usage {
                            prompt_budget.observe(
                                u.prompt_tokens as usize,
                                &req.messages,
                                &req.tools,
                            );
                        }
                        if !chunk.thinking {
                            text.push_str(&chunk.delta);
                            if let Some(cb) = stream_cb {
                                cb(Some(&chunk.delta));
                            }
                        }
                        tool_calls_requested |= chunk.tool_calls_requested;
                        streamed_fragments.extend(chunk.streamed_tool_calls);
                        if chunk.done {
                            if let Some(u) = trailing_usage(&mut stream, request_usage).await {
                                request_usage = Some(u);
                                prompt_budget.observe(
                                    u.prompt_tokens as usize,
                                    &req.messages,
                                    &req.tools,
                                );
                            }
                            break;
                        }
                    }
                    Err(e) => {
                        stream_err = Some(e);
                        break;
                    }
                }
            }
            match stream_err {
                None => break, // completed (done or clean EOF)
                Some(e) if stream_retries < 1 && is_transient_stream_error(&e) => {
                    stream_retries += 1;
                    eprintln!(
                        "  warning: stream died mid-completion ({e:#}); \
                         retrying once from the current transcript"
                    );
                    accumulate_usage(&mut tokens, request_usage);
                    log_request_usage(config, "stream retry", request_usage);
                    request_usage = None;
                    text.clear();
                    tool_calls_requested = false;
                    streamed_fragments.clear();
                    match provider.chat_stream(req.clone()).await {
                        Ok(s) => stream = s,
                        Err(e2) => {
                            return Err(e2).context("starting completion (after mid-stream retry)");
                        }
                    }
                }
                Some(e) => {
                    log_request_usage(config, "stream failed", request_usage);
                    return Err(e).context("reading stream");
                }
            }
        }
        accumulate_usage(&mut tokens, request_usage);
        log_request_usage(config, "stream", request_usage);

        if tool_calls_requested {
            // Tell the UI the text stream is interrupted by tool calls.
            if let Some(cb) = stream_cb {
                cb(None);
            }
            // Streamed-tool-calls fast path: providers that emit
            // `delta.tool_calls` fragments (OpenAI proper, vLLM/SGLang)
            // give us the complete calls in the stream we just read —
            // skip the non-streaming replay entirely. Halves per-tool-
            // call latency. Falls back to the legacy replay whenever the
            // fragments are absent or fail to merge into valid JSON.
            // The legacy replay keeps the same bounded transient retry
            // as the stream: it is a second full request and dies on
            // transient 5xx/429 the same way (the provider's fallback
            // chain visits each endpoint at most once per request).
            let mut tool_calls = crate::llm::openai::merge_tool_call_fragments(&streamed_fragments);
            let mut complete: Option<ChatComplete> = None;
            if tool_calls.is_empty() {
                let mut replay_retries = 0u8;
                let c: ChatComplete = loop {
                    match provider.chat_complete(req.clone()).await {
                        Ok(c) => break c,
                        Err(e) if replay_retries < 1 && is_transient_stream_error(&e) => {
                            replay_retries += 1;
                            eprintln!(
                                "  warning: tool-call replay failed transiently ({e:#}); retrying once"
                            );
                        }
                        Err(e) => return Err(e).context("replaying completion for tool calls"),
                    }
                };
                tool_calls = c.tool_calls.clone();
                complete = Some(c);
            }
            if let Some(c) = &complete {
                accumulate_usage(&mut tokens, c.usage);
                log_request_usage(config, "tool replay", c.usage);
                if let Some(u) = c.usage {
                    prompt_budget.observe(u.prompt_tokens as usize, &req.messages, &req.tools);
                }
            }
            if tool_calls.is_empty() {
                // Model asked for tools but sent none; treat text as answer.
                // (On the replay path `complete` is Some and its text is
                // the authoritative answer; the fast path can't get here
                // with an empty replay — but if it somehow does, fall
                // through to text.)
                let answer = match &complete {
                    Some(c) if text.trim().is_empty() => c.text.clone(),
                    Some(_) => text,
                    None => text,
                };
                // Phase A2: 最终答案事件（模型可见输出入日志）。
                if let Some(sink) = &config.session_sink {
                    sink(crate::session::Event::Assistant {
                        ts: None,
                        text: answer.clone(),
                    });
                    sink(crate::session::Event::TurnEnd { ts: None });
                }
                messages.push(ChatMessage::assistant(answer.trim()));
                return Ok(RunResult {
                    output: answer.trim().to_string(),
                    steps: step,
                    state: RunState::Success,
                    tokens,
                    messages,
                });
            }
            // Phase A2: assistant 事件（文本 + 随后 ToolCall 事件配对成完整消息）。
            // On the streamed fast path the assistant text is whatever the
            // stream carried (often empty); on the replay path prefer the
            // complete response's text.
            let assistant_text = match &complete {
                Some(c) if !c.text.trim().is_empty() => c.text.clone(),
                _ => text,
            };
            if let Some(sink) = &config.session_sink {
                sink(crate::session::Event::Assistant {
                    ts: None,
                    text: assistant_text.clone(),
                });
            }
            messages.push(ChatMessage::assistant_with_tools(
                &assistant_text,
                tool_calls.clone(),
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
            let mut plans: Vec<Plan> = Vec::with_capacity(tool_calls.len());
            let mut executables: Vec<usize> = Vec::new(); // plan indices to run
            // Session event sink (Phase 63 event-sourced logging): emit
            // one ToolCall event per requested call before gating, so
            // the raw log records what the model asked for even when
            // approval later blocks it.
            if let Some(sink) = &config.session_sink {
                for call in &tool_calls {
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
            // Every tool call goes through the single chokepoint
            // (`ToolGate`): approval gate → approver → on_pre_tool veto →
            // circuit breaker → execute → ring annotation → breaker
            // bookkeeping → session events → on_tool_result rewrite.
            //
            // A second execution path must not re-implement this. That is
            // precisely how EDG drifted out of the safety model: it ran tools
            // itself and silently skipped four of the five controls.
            let gate = ToolGate::new(&*tools, &breaker, Some(config), approver);
            for call in &tool_calls {
                match gate.gate(call).await? {
                    GateVerdict::Run => {
                        executables.push(plans.len());
                        plans.push(Plan {
                            call: call.clone(),
                            blocked: None,
                        });
                    }
                    GateVerdict::Blocked(reason) => plans.push(Plan {
                        call: call.clone(),
                        blocked: Some(reason),
                    }),
                }
            }

            // Parallel execution of the cleared calls: gating and
            // bookkeeping are cheap and serial, the runs are independent (the
            // model issues several calls in one response precisely because
            // they do not depend on each other). Results are re-inserted in
            // the original call order so the model always sees tool_results
            // matching its tool_call ids.
            let futs = executables.iter().map(|&pi| {
                let call = plans[pi].call.clone();
                let gate = &gate;
                async move { (pi, gate.run(&call).await) }
            });
            let executed: Vec<(usize, String)> = futures_util::future::join_all(futs).await;
            let mut results_by_plan: std::collections::HashMap<usize, String> =
                executed.into_iter().collect();

            let mut round_receipts: Vec<(String, bool, String)> = Vec::new();
            for (i, plan) in plans.into_iter().enumerate() {
                let blocked = plan.blocked.clone();
                let (ran, raw) = match blocked {
                    Some(m) => (false, m),
                    None => (
                        true,
                        results_by_plan
                            .remove(&i)
                            .unwrap_or_else(|| "error: tool produced no result".into()),
                    ),
                };
                let msg = gate.observe(&plan.call, ran, raw).await?;
                round_receipts.push((
                    plan.call.name.clone(),
                    ran && !msg.starts_with("error:"),
                    msg.clone(),
                ));
                messages.push(ChatMessage::tool_result(msg, plan.call.id));
            }
            // Scheduler ②: a round that produced exactly the same
            // (tool, ok, output) triples as the previous one achieved the same
            // thing. Say so once; if it happens again, stop paying for it and
            // force a wrap-up instead of burning the rest of the budget.
            if no_progress_limit > 0 && !round_receipts.is_empty() {
                let signature = scheduler::progress_signature(&round_receipts);
                stalled_rounds = if last_round == Some(signature) {
                    stalled_rounds + 1
                } else {
                    0
                };
                last_round = Some(signature);
                if stalled_rounds > no_progress_limit {
                    eprintln!(
                        "[scheduler] no progress for {} rounds — wrapping up",
                        stalled_rounds
                    );
                    return wrap_up(
                        provider,
                        config,
                        messages,
                        stream_cb,
                        &prompt_budget,
                        tokens,
                        step,
                        RunState::NoProgress,
                        fallback_draft,
                    )
                    .await;
                }
                if stalled_rounds == no_progress_limit {
                    messages.push(ChatMessage::system(scheduler::no_progress_nudge(
                        stalled_rounds,
                    )));
                }
            }
            // Evolution: hot-register any tools the model created this
            // turn so they are callable (and advertised) next turn.
            tools.drain_hot();
            continue;
        }

        // No tool call — this is the final answer.
        // Empty answer guard: if the stream produced no visible content
        // and no tool calls, ask once more (bounded — a model that keeps
        // answering empty yields the empty answer, not an infinite loop).
        if text.trim().is_empty() && !retried_empty {
            retried_empty = true;
            eprintln!("  warning: empty response — nudging the model to answer in content");
            messages.push(ChatMessage::system(
                "Your previous reply carried no visible message content \
                 (it may have gone to a reasoning channel). Restate your                  final answer as normal message content now.",
            ));
            continue;
        }
        // Scheduler ①: still empty after the nudge — a reasoning-only model
        // must not turn into a silent empty success. The pre-loop draft is the
        // floor this run was promised.
        if text.trim().is_empty()
            && let Some(draft) = fallback_draft.as_ref()
            && !draft.trim().is_empty()
        {
            eprintln!("[scheduler] empty answer — falling back to the first-frame draft");
            text = draft.clone();
        }
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
        // Event sink: the final answer is model-visible output — log it
        // on every success exit (previously only the tool-call-replay
        // exit did, so callers had to append the assistant turn manually
        // and the replay-empty path double-logged).
        if let Some(sink) = &config.session_sink {
            sink(crate::session::Event::Assistant {
                ts: None,
                text: text.trim().to_string(),
            });
            sink(crate::session::Event::TurnEnd { ts: None });
        }
        messages.push(ChatMessage::assistant(text.trim()));
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
/// One tool-less completion: the shared body of the step-limit wrap-up and
/// the first-frame draft. No tool specs are advertised, so the model cannot
/// answer by calling something.
async fn tool_less_call(
    provider: &dyn LlmProvider,
    config: &AgentConfig,
    messages: &[ChatMessage],
    instruction: &str,
    stream_cb: Option<&StreamCb>,
    prompt_budget: &context::PromptBudget,
) -> Result<(String, Option<TokenUsage>)> {
    let mut msgs = messages.to_vec();
    msgs.push(ChatMessage::system(instruction));
    let msgs = prompt_budget.compact(
        &msgs,
        &[],
        config.max_context_messages,
        config.max_context_tokens,
    );
    let req = ChatRequest {
        model: config.model.clone(),
        messages: msgs,
        tools: vec![],
        stream: true,
        temperature: None,
    };
    let mut stream = provider.chat_stream(req).await.context("tool-less call")?;
    let mut text = String::new();
    let mut tokens: Option<TokenUsage> = None;
    while let Some(chunk) = stream.next().await {
        let chunk = chunk.context("reading tool-less call")?;
        if chunk.usage.is_some() {
            tokens = chunk.usage;
        }
        if !chunk.thinking {
            text.push_str(&chunk.delta);
            if let Some(cb) = stream_cb {
                cb(Some(&chunk.delta));
            }
        }
        if chunk.done {
            tokens = trailing_usage(&mut stream, tokens).await;
            break;
        }
    }
    Ok((text, tokens))
}

/// End a run by asking the model for a wrap-up (no tools), then run the same
/// exit ceremony every successful exit uses: `on_done` observation, session
/// events, assistant turn, `RunResult`.
///
/// `fallback_draft` is the first-frame floor: when the wrap-up fails or comes
/// back empty, the run still returns something the user can use instead of an
/// apology string.
#[allow(clippy::too_many_arguments)]
async fn wrap_up(
    provider: &dyn LlmProvider,
    config: &AgentConfig,
    messages: Vec<ChatMessage>,
    stream_cb: Option<&StreamCb>,
    prompt_budget: &context::PromptBudget,
    mut tokens: Option<TokenUsage>,
    step: u32,
    state: RunState,
    fallback_draft: Option<String>,
) -> Result<RunResult> {
    let attempt = tool_less_call(
        provider,
        config,
        &messages,
        "Step budget is used up. Do not call any tools. Summarize what you \
         established and give your best final answer now.",
        stream_cb,
        prompt_budget,
    )
    .await;
    let draft = fallback_draft.filter(|d| !d.trim().is_empty());
    let (summary, summary_tokens) = match attempt {
        Ok((s, t)) if !s.trim().is_empty() => (s, t),
        Ok((s, t)) => match draft {
            Some(d) => {
                eprintln!("[scheduler] wrap-up was empty — using the first-frame draft");
                (d, t)
            }
            None => (s, t),
        },
        Err(e) => match draft {
            Some(d) => {
                eprintln!("[scheduler] wrap-up failed ({e:#}) — using the first-frame draft");
                (d, None)
            }
            None => (
                format!("run ended ({state:?}) — wrap-up unavailable ({e})"),
                None,
            ),
        },
    };
    accumulate_usage(&mut tokens, summary_tokens);
    // P0 hook (emit observation): a budget-exhausted run is exactly when
    // delivery audit (`on_done`) matters most.
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
    // Event sink: the wrap-up is model-visible output — log it (callers do not
    // append the assistant turn themselves; the loop owns assistant events on
    // every success exit).
    if let Some(sink) = &config.session_sink {
        sink(crate::session::Event::Assistant {
            ts: None,
            text: summary.clone(),
        });
        sink(crate::session::Event::TurnEnd { ts: None });
    }
    let mut messages = messages;
    messages.push(ChatMessage::assistant(summary.clone()));
    Ok(RunResult {
        output: summary,
        steps: step,
        state,
        tokens,
        messages,
    })
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

/// Classify a MID-STREAM failure (the request was accepted, some or no
/// chunks arrived, then the body died) as transient: one bounded retry
/// of the request is reasonable. Mirrors the provider's own
/// pre-headers taxonomy (429 → transient, 5xx → transient, 4xx →
/// fatal) which only covers the send phase — once streaming started,
/// nothing retried until now.
fn is_transient_stream_error(err: &anyhow::Error) -> bool {
    let msg = format!("{err:#}").to_lowercase();
    const TRANSIENT: &[&str] = &[
        "429",
        "too many requests",
        "500",
        "502",
        "503",
        "504",
        "bad gateway",
        "service unavailable",
        "gateway timeout",
        "timed out",
        "timeout",
        "connection reset",
        "connection closed",
        "broken pipe",
        "unexpected eof",
        "incomplete message",
        "hyper",
        "closed the stream", // the mid-stream EOF detector's own wording
    ];
    TRANSIENT.iter().any(|k| msg.contains(k))
}
