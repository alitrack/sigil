//! Deterministic context budgeting and bounded history extraction.

use crate::llm::provider::{ChatMessage, Role, ToolSpec};

const SUMMARY_CHARS: usize = 16_000;

/// A proactive compaction must land the request under this share of the token
/// budget (%) to be worth invalidating the provider's prefix cache.
const COMPACT_TARGET_SHARE: usize = 70;

/// …or the transcript must be this many times over the budget, in which case
/// folding the old history is damage control for an unfittable request.
const FAR_OVER: usize = 2;

/// Fold the oldest turns of a transcript into one compact summary
/// message so long sessions don't grow the context without bound.
///
/// Deterministic — no LLM call, no sampling variance. Every trimmed
/// user/assistant/tool message contributes a role-tagged
/// multiline excerpt with a role-specific size limit; long excerpts
/// retain both their beginning and end. Earlier summaries are carried forward.
/// Leading system instructions (excluding previous summaries) are pinned.
/// The retained tail stays verbatim, with intact tool-call/result pairs.
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

/// Model-independent cold estimate. Non-ASCII characters receive a larger
/// allowance; real usage subsequently calibrates this heuristic.
pub fn estimate_tokens(text: &str) -> usize {
    let mut ascii = 0usize;
    let mut other = 0usize;
    for c in text.chars() {
        if c.is_ascii() {
            ascii += 1;
        } else {
            other += 1;
        }
    }
    (ascii / 4 + other * 2).max(1)
}

/// Token estimate for one message, including tool-call payloads.
pub fn estimate_message_tokens(m: &ChatMessage) -> usize {
    let mut t = 4 + estimate_tokens(&m.content);
    if let Some(calls) = &m.tool_calls {
        for c in calls {
            t += estimate_tokens(&c.name);
            t += estimate_tokens(&c.arguments.to_string());
        }
    }
    t
}

/// A calibrated estimate is recomputed from the current request, never
/// reused as a stale absolute token count. The sample includes tool schemas.
#[derive(Debug, Clone)]
pub(super) struct PromptBudget {
    scale: f64,
}

impl Default for PromptBudget {
    fn default() -> Self {
        Self { scale: 1.0 }
    }
}

impl PromptBudget {
    fn tools_tokens(tools: &[ToolSpec]) -> usize {
        tools
            .iter()
            .map(|tool| {
                8 + estimate_tokens(
                    &serde_json::to_string(tool).expect("tool specs are serializable"),
                )
            })
            .sum()
    }

    pub(super) fn observe(&mut self, actual: usize, messages: &[ChatMessage], tools: &[ToolSpec]) {
        let estimated =
            messages.iter().map(estimate_message_tokens).sum::<usize>() + Self::tools_tokens(tools);
        if actual > 0 && estimated > 0 {
            self.scale = actual as f64 / estimated as f64;
        }
    }

    fn scaled(&self, tokens: usize) -> usize {
        (tokens as f64 * self.scale).ceil() as usize
    }

    /// Proactive compaction.
    ///
    /// Compacting rewrites the prompt prefix from the summary onward, and a
    /// provider cache pays for exactly that prefix — so a rewrite has to earn
    /// its cost. Measured on a tool-using loop
    /// (`docs/evaluations/2026-11-23-context-compaction-ab.md`): shaving the
    /// transcript to the threshold re-fired every step, dropped the cache-hit
    /// rate from 85.7% to 83.1% and raised total prompt tokens 9.8% at
    /// identical quality.
    ///
    /// Two rules come out of that, both only meaningful on the token axis —
    /// when the budget is expressed in messages, the message count *is* the
    /// contract and any reduction is progress:
    ///
    /// * **Cut deep, once.** Target half the transcript budget rather than
    ///   shaving to the edge, so one rewrite buys many steps of headroom
    ///   instead of firing again on the next step.
    /// * **Skip marginal rewrites.** A rewrite is taken when it lands the
    ///   request under [`COMPACT_TARGET_SHARE`] of the budget, or when the
    ///   transcript is at least [`FAR_OVER`] times the budget and folding the
    ///   old history is damage control for a request that cannot fit anyway.
    pub(super) fn compact(
        &self,
        messages: &[ChatMessage],
        tools: &[ToolSpec],
        max_msgs: usize,
        max_prompt_tokens: usize,
    ) -> Vec<ChatMessage> {
        self.compact_with(messages, tools, max_msgs, max_prompt_tokens, false)
    }

    /// Must-fit variant for the tool-less draft / wrap-up calls: those exist to
    /// fit one last request, so the value rules above do not apply.
    pub(super) fn compact_forced(
        &self,
        messages: &[ChatMessage],
        tools: &[ToolSpec],
        max_msgs: usize,
        max_prompt_tokens: usize,
    ) -> Vec<ChatMessage> {
        self.compact_with(messages, tools, max_msgs, max_prompt_tokens, true)
    }

    fn compact_with(
        &self,
        messages: &[ChatMessage],
        tools: &[ToolSpec],
        max_msgs: usize,
        max_prompt_tokens: usize,
        forced: bool,
    ) -> Vec<ChatMessage> {
        let tool_tokens = self.scaled(Self::tools_tokens(tools));
        let transcript_limit = if max_prompt_tokens == 0 {
            0
        } else {
            max_prompt_tokens.saturating_sub(tool_tokens).max(1)
        };
        let transcript_tokens = self.scaled(messages.iter().map(estimate_message_tokens).sum());
        let attempt = |limit: usize| -> (Vec<ChatMessage>, usize) {
            let candidate =
                compact_messages_with_budget(messages, max_msgs, limit, Some(transcript_tokens));
            let tokens = self.scaled(candidate.iter().map(estimate_message_tokens).sum());
            (candidate, tokens)
        };
        // Message-count axis (or a must-fit call): nothing to trade off.
        let ungated = forced || max_prompt_tokens == 0;
        let limit = if ungated {
            transcript_limit
        } else {
            // Deep cut: aim at half the budget.
            (transcript_limit / 2).max(1)
        };
        let (candidate, candidate_tokens) = attempt(limit);
        if ungated {
            // Message-count axis (or a must-fit call): the message count is the
            // contract, so a smaller transcript wins even when the summary
            // costs a few more tokens than the lines it replaced.
            return if candidate_tokens < transcript_tokens || candidate.len() < messages.len() {
                candidate
            } else {
                messages.to_vec()
            };
        }
        if candidate_tokens >= transcript_tokens {
            return messages.to_vec();
        }
        // Land comfortably under the budget → the rewrite pays for itself.
        let landed =
            candidate_tokens + tool_tokens <= max_prompt_tokens * COMPACT_TARGET_SHARE / 100;
        // Far over → the request cannot fit anyway; folding old history is
        // damage control rather than an optimisation.
        let far_over = transcript_tokens >= transcript_limit.saturating_mul(FAR_OVER);
        if landed || far_over {
            candidate
        } else {
            messages.to_vec()
        }
    }
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
                if let Some(rest) = line.strip_prefix("Files read (JSON): ") {
                    if let Ok(paths) = serde_json::from_str::<Vec<String>>(rest) {
                        for path in paths {
                            push_read(path);
                        }
                    }
                } else if let Some(rest) = line.strip_prefix("Files modified (JSON): ") {
                    if let Ok(paths) = serde_json::from_str::<Vec<String>>(rest) {
                        for path in paths {
                            push_modified(path);
                        }
                    }
                } else if let Some(rest) = line.strip_prefix("Files read: ") {
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

/// Keep ordinary histories readable while escaping delimiter-sensitive paths.
fn append_file_history(summary: &mut String, label: &str, paths: &[String]) {
    if paths.is_empty() {
        return;
    }
    let plain = format!("\n{label}: {}", paths.join(", "));
    let available = SUMMARY_CHARS.saturating_sub(summary.chars().count());
    if paths
        .iter()
        .any(|path| path.contains([',', '\n', '\r']) || path.trim() != path || path.is_empty())
        || plain.chars().count() > available
    {
        const OMITTED: &str =
            "\n[file history truncated; consult original session log for omitted paths]";
        let mut line = format!("\n{label} (JSON): [");
        let mut used = line.chars().count() + 1 + OMITTED.chars().count();
        let mut kept = 0;
        for path in paths {
            let encoded = serde_json::to_string(path).expect("file paths are serializable");
            let required = encoded.chars().count() + usize::from(kept > 0);
            if used + required > available {
                break;
            }
            if kept > 0 {
                line.push(',');
            }
            line.push_str(&encoded);
            used += required;
            kept += 1;
        }
        line.push(']');
        if kept < paths.len() {
            line.push_str(OMITTED);
        }
        if line.chars().count() <= available {
            summary.push_str(&line);
        }
    } else {
        summary.push_str(&plain);
    }
}

/// A message that may start the retained compaction tail: a user turn
/// or a tool-free assistant message. Assistant messages with pending
/// tool calls and tool results must never start the tail — strict
/// OpenAI-compatible APIs reject an assistant `tool_calls` block whose
/// results were folded away (and a tool result whose call was).
fn is_safe_tail_start(m: &ChatMessage) -> bool {
    matches!(m.role, Role::User) || (matches!(m.role, Role::Assistant) && m.tool_calls.is_none())
}

/// [`compact_messages`] with a token budget: compress when the
/// estimated transcript size exceeds `max_tokens` OR the message count
/// exceeds `max_msgs`. Either budget may be `0` to disable that axis.
///
/// `recent_prompt_tokens` must describe this current transcript, excluding
/// tool schemas. `PromptBudget` projects real usage onto the current request
/// before calling here, so a past snapshot cannot hide new tool output or
/// retrigger compaction after history shrinks. The supplied size calibrates
/// both the trigger and the retained tail, including ratios below one.
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
    let prefix_len = messages
        .iter()
        .take_while(|m| {
            m.role == Role::System && !m.content.starts_with("[earlier conversation compressed]")
        })
        .count()
        .max(1);
    // Current projected transcript size wins over the cold heuristic.
    let total_tokens: usize = messages.iter().map(estimate_message_tokens).sum();
    let effective_total = recent_prompt_tokens.unwrap_or(total_tokens);
    let within_msgs = max_msgs == 0 || messages.len() <= max_msgs;
    let within_tokens = max_tokens == 0 || effective_total <= max_tokens;
    if within_msgs && within_tokens {
        return messages.to_vec();
    }
    // Calibrate the per-message estimate for the tail walk.
    let ratio: f64 = if total_tokens > 0 {
        effective_total as f64 / total_tokens as f64
    } else {
        1.0
    };

    // Walk backwards, accumulating token estimates, until the token
    // budget or the message budget (minus the summary slot) is met.
    // `max_msgs == 0` disables the message-count axis entirely: the walk
    // is bounded by tokens alone. (A token-only config used to collapse
    // onto the 2-message floor via `saturating_sub(2).max(2)`, folding
    // whole sessions into one summary line no matter how large the
    // token budget was.)
    let keep_count = if max_msgs > 0 {
        max_msgs.saturating_sub(prefix_len + 1).max(2)
    } else {
        usize::MAX
    };
    let mut cut = prefix_len;
    let prefix_tokens = messages[..prefix_len]
        .iter()
        .map(|m| (estimate_message_tokens(m) as f64 * ratio).ceil() as usize)
        .sum::<usize>();
    let tail_limit = max_tokens.saturating_sub(prefix_tokens);
    let mut acc = 0usize;
    for i in (prefix_len..messages.len()).rev() {
        acc += (estimate_message_tokens(&messages[i]) as f64 * ratio).ceil() as usize;
        let count_over = messages.len() - i >= keep_count;
        let tokens_over = max_tokens > 0 && acc > tail_limit;
        if count_over || tokens_over {
            cut = i;
            break;
        }
    }
    if cut <= prefix_len {
        // Nothing safe to trim — keep everything.
        return messages.to_vec();
    }
    // Extend forward to the first safe boundary at-or-after `cut`. If the
    // tail has no safe start at all (a mid-turn tail of tool results),
    // do NOT run off the end — that would fold the CURRENT turn's
    // in-flight tool results into one-line excerpts, silently dropping
    // the output of calls the model just made. Back off to the newest
    // safe boundary before `cut` instead: over-retention is recoverable
    // on the next compaction, silent result loss is not.
    let origin = cut;
    while cut < messages.len() && !is_safe_tail_start(&messages[cut]) {
        cut += 1;
    }
    if cut >= messages.len() {
        // No safe boundary at-or-after the cut: walk back instead.
        cut = origin;
        while cut > prefix_len && !is_safe_tail_start(&messages[cut]) {
            cut -= 1;
        }
    }
    if cut <= prefix_len {
        // Nothing safe to trim — keep everything.
        return messages.to_vec();
    }
    while cut < messages.len() {
        let safe = matches!(messages[cut].role, Role::User)
            || (matches!(messages[cut].role, Role::Assistant)
                && messages[cut].tool_calls.is_none());
        if safe {
            break;
        }
        cut += 1;
    }
    if cut <= prefix_len {
        // Nothing safe to trim — keep everything.
        return messages.to_vec();
    }

    // Build the summary: file operations the model touched (the most
    // valuable thing to preserve) + role-tagged multiline excerpts.
    let (files_read, files_modified) = extract_file_ops(&messages[prefix_len..cut]);
    let mut summary = String::from("[earlier conversation compressed]");
    append_file_history(&mut summary, "Files read", &files_read);
    append_file_history(&mut summary, "Files modified", &files_modified);
    // Preserve multiline instructions and outcomes; retain earlier summaries
    // verbatim within a bounded record instead of summarizing their header.
    // This is still lossy extraction, not a semantic summary. The full event
    // log remains the source of truth for details omitted here.
    for m in &messages[prefix_len..cut] {
        let (tag, limit) = match m.role {
            Role::System => ("system", 8_000),
            Role::User => ("user", 2_000),
            Role::Assistant => ("assistant", 1_000),
            Role::Tool => ("tool", 500),
        };
        let content = if m.role == Role::System
            && m.content.starts_with("[earlier conversation compressed]")
        {
            m.content
                .strip_prefix("[earlier conversation compressed]")
                .unwrap_or(&m.content)
        } else {
            &m.content
        };
        let chars: Vec<char> = content.chars().collect();
        let excerpt = if chars.len() > limit {
            // Keep the end as well: tool status and test totals often live there.
            format!(
                "{}\n[excerpt truncated]\n{}",
                chars[..limit * 3 / 4].iter().collect::<String>(),
                chars[chars.len() - limit / 4..].iter().collect::<String>()
            )
        } else {
            content.to_string()
        };
        summary.push_str(&format!("\n{tag}: {excerpt}"));
    }
    if summary.chars().count() > SUMMARY_CHARS {
        summary = summary.chars().take(SUMMARY_CHARS).collect();
        summary.push_str(
            "\n[summary truncated; consult the original session log for omitted details]",
        );
    }

    let mut out = Vec::with_capacity(messages.len() - cut + prefix_len + 1);
    out.extend_from_slice(&messages[..prefix_len]);
    out.push(ChatMessage::system(summary));
    out.extend_from_slice(&messages[cut..]);
    out
}

#[cfg(test)]
mod estimate_tests {
    use super::*;
    #[test]
    fn repeated_compaction_preserves_exact_file_paths() {
        let paths = [
            "data/a,b.json",
            " padded.json ",
            "line\nbreak.json",
            "carriage\rreturn.json",
            "quote\"and\\slash.json",
            "[notes].json",
            "正常.json",
        ];
        let mut history = vec![ChatMessage::system("base"), ChatMessage::user("edit files")];
        for (i, path) in paths.iter().enumerate() {
            let id = format!("call-{i}");
            for name in ["read_file", "write_file"] {
                let id = format!("{id}-{name}");
                history.push(ChatMessage::assistant_with_tools(
                    "",
                    vec![crate::llm::provider::ToolCall {
                        id: id.clone(),
                        name: name.into(),
                        arguments: serde_json::json!({"path": path}),
                    }],
                ));
                history.push(ChatMessage::tool_result("ok", id));
            }
        }
        history.push(ChatMessage::user("next"));
        for _ in 0..3 {
            history = compact_messages_with_budget(&history, 0, 5, None);
            let (read, modified) = extract_file_ops(&history);
            assert_eq!(read, paths);
            assert_eq!(modified, paths);
            history.push(ChatMessage::assistant("done"));
            history.push(ChatMessage::user("continue"));
        }
    }

    #[test]
    fn legacy_file_history_still_accumulates() {
        let history = vec![ChatMessage::system(
            "[earlier conversation compressed]\nFiles read: src/main.rs, src/config.rs\nFiles modified: [notes].json",
        )];
        assert_eq!(
            extract_file_ops(&history),
            (
                vec!["src/main.rs".into(), "src/config.rs".into()],
                vec!["[notes].json".into()]
            )
        );
    }

    #[test]
    fn oversized_file_history_retains_complete_paths() {
        let paths: Vec<String> = (0..80)
            .map(|i| format!("{i},{}.json", "x".repeat(300)))
            .collect();
        let mut history = vec![ChatMessage::system("base"), ChatMessage::user("edit")];
        for (i, path) in paths.iter().enumerate() {
            let id = format!("c{i}");
            history.push(ChatMessage::assistant_with_tools(
                "",
                vec![crate::llm::provider::ToolCall {
                    id: id.clone(),
                    name: "write_file".into(),
                    arguments: serde_json::json!({"path": path}),
                }],
            ));
            history.push(ChatMessage::tool_result("ok", id));
        }
        history.push(ChatMessage::user("next"));
        let first = compact_messages_with_budget(&history, 0, 5, None);
        let (_, retained) = extract_file_ops(&first);
        assert!(
            !retained.is_empty(),
            "truncation must preserve complete earlier paths"
        );
        assert_eq!(retained, paths[..retained.len()]);
        assert!(first[1].content.contains("file history truncated"));
        let mut second = first;
        second.push(ChatMessage::assistant("done"));
        second.push(ChatMessage::user("continue"));
        let second = compact_messages_with_budget(&second, 0, 5, None);
        assert_eq!(extract_file_ops(&second).1, retained);
    }

    #[test]
    fn live_goals_are_pinned_outside_compaction_summary() {
        let goal = "## Active goals\n- g: preserve API";
        let mut history = vec![ChatMessage::system("base"), ChatMessage::system(goal)];
        for _ in 0..20 {
            history.extend([
                ChatMessage::user("previous task"),
                ChatMessage::assistant("previous result"),
            ]);
        }
        history.push(ChatMessage::user("current task"));
        let compacted = compact_messages_with_budget(&history, 6, 0, None);
        assert_eq!(
            compacted[1].content, goal,
            "active goals must survive as current instructions"
        );
        let summary = compacted
            .iter()
            .find(|m| m.content.starts_with("[earlier conversation compressed]"))
            .unwrap();
        assert!(
            !summary.content.contains("## Active goals"),
            "do not archive live goal directives into old history"
        );
    }

    #[test]
    fn lower_real_usage_calibrates_retention_as_well_as_trigger() {
        let mut history = vec![ChatMessage::system("base")];
        for _ in 0..10 {
            history.extend([
                ChatMessage::user("x".repeat(1000)),
                ChatMessage::assistant("y".repeat(1000)),
            ]);
        }
        let compacted = compact_messages_with_budget(&history, 0, 500, Some(1000));
        assert!(
            compacted.len() > 6,
            "a lower measured token ratio should retain a proportional tail"
        );
    }

    #[test]
    fn request_budget_includes_tool_schemas_and_reestimates_shrunken_history() {
        let mut history = vec![ChatMessage::system("base")];
        for _ in 0..20 {
            history.extend([
                ChatMessage::user("previous task"),
                ChatMessage::assistant("previous result"),
            ]);
        }
        history.push(ChatMessage::user("current task"));
        let tool = ToolSpec {
            name: "large-schema".into(),
            description: "x".repeat(3400),
            parameters: serde_json::json!({}),
        };
        let budget = PromptBudget::default();
        assert_eq!(budget.compact(&history, &[], 0, 1000).len(), history.len());
        assert!(
            budget.compact(&history, &[tool], 0, 1000).len() < history.len(),
            "schemas consume the same input budget"
        );
        let mut calibrated = PromptBudget::default();
        calibrated.observe(20000, &history, &[]);
        let shortened = vec![
            ChatMessage::system("base"),
            ChatMessage::system("[earlier conversation compressed] short recap"),
            ChatMessage::user("current task"),
            ChatMessage::assistant("done"),
        ];
        let after = calibrated.compact(&shortened, &[], 0, 10000);
        assert_eq!(
            serde_json::to_value(after).unwrap(),
            serde_json::to_value(shortened).unwrap(),
            "the previous absolute prompt size must not retrigger compaction after shrinking"
        );
    }

    #[test]
    fn token_only_budget_does_not_replace_history_with_a_larger_summary() {
        let history = vec![
            ChatMessage::system("base"),
            ChatMessage::user("old"),
            ChatMessage::assistant("reply"),
            ChatMessage::user("new"),
        ];
        let result = PromptBudget::default().compact(&history, &[], 0, 14);
        assert_eq!(
            serde_json::to_value(result).unwrap(),
            serde_json::to_value(history).unwrap(),
            "an expanding summary cannot solve overflow and would invalidate the prefix"
        );
    }

    /// A history that fits the budget must never be rewritten: every rewrite
    /// invalidates the provider's prefix cache for nothing.
    #[test]
    fn a_history_within_budget_is_not_rewritten() {
        let mut history = vec![ChatMessage::system("base")];
        for i in 0..5 {
            history.push(ChatMessage::user(format!("q{i}")));
            history.push(ChatMessage::assistant("a"));
        }
        let out = PromptBudget::default().compact(&history, &[], 0, 100_000);
        assert_eq!(
            serde_json::to_value(&out).unwrap(),
            serde_json::to_value(&history).unwrap()
        );
    }

    /// Marginally-over budget with a trimmable tail: the old policy shaved to
    /// the threshold and re-fired on the next step (measured: +9.8% tokens,
    /// cache hits 85.7% → 83.1%). Now it cuts deep so the next rewrite is far
    /// away — the retained transcript must land well under the budget, not on
    /// its edge.
    #[test]
    fn over_budget_compaction_cuts_deep_instead_of_shaving() {
        // Realistic shape: few messages, large tool results. (Many *small*
        // messages cannot be compressed much at all — the per-message excerpt
        // cap makes the summary roughly as large as the content it replaces,
        // which is why the old policy shaved and re-fired forever.)
        let mut history = vec![ChatMessage::system("base")];
        for i in 0..20 {
            history.push(ChatMessage::user(format!("turn {i}")));
            history.push(ChatMessage::assistant("ran a tool"));
            history.push(ChatMessage::tool_result(
                format!("big output {i} {}", "x".repeat(4_000)),
                "call-1".to_string(),
            ));
        }
        let before = history.iter().map(estimate_message_tokens).sum::<usize>();
        let budget = before / 2;
        let out = PromptBudget::default().compact(&history, &[], 0, budget);
        assert!(out.len() < history.len(), "a rewrite must happen");
        let after = out.iter().map(estimate_message_tokens).sum::<usize>();
        assert!(
            after < budget,
            "one deep cut must put the request back inside the budget:              after={after} budget={budget}"
        );
        assert!(
            before - after >= before / 2,
            "one deep cut must remove at least half, so the next rewrite is far              away: before={before} after={after}"
        );
    }

    /// When the transcript is far over budget and cannot be folded under it,
    /// folding the old history is still damage control — do not refuse.
    #[test]
    fn far_over_budget_still_compacts() {
        let mut history = vec![ChatMessage::system("base")];
        for _ in 0..40 {
            history.push(ChatMessage::user("u"));
            history.push(ChatMessage::assistant("a"));
        }
        let before = history.iter().map(estimate_message_tokens).sum::<usize>();
        // A budget so tight that no compaction can reach it.
        let out = PromptBudget::default().compact(&history, &[], 0, before / 8);
        assert!(
            out.len() < history.len(),
            "damage control must still fold old history"
        );
    }

    #[test]
    fn cold_estimate_does_not_treat_chinese_as_quarter_tokens() {
        assert!(estimate_tokens("中文约束".repeat(100).as_str()) >= 400);
    }
}
