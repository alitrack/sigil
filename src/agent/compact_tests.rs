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
fn token_only_budget_walks_tokens_not_message_count() {
    // max_msgs == 0 disables the message-count axis: a token-only
    // budget must retain as many recent messages as the TOKEN budget
    // allows — not collapse onto the 2-message floor (the old
    // `saturating_sub(2).max(2)` keep_count folded whole sessions
    // into one summary line no matter how large the token budget).
    let m = msgs(12); // 25 messages, ~70 heuristic tokens
    let total: usize = m.iter().map(estimate_message_tokens).sum();
    // Over budget by a little → compact, but keep a token-sized tail.
    let budget = total / 2; // ~20 tokens: under half the transcript
    let c = compact_messages_with_budget(&m, 0, budget, None);
    assert!(c.len() < m.len(), "over-budget must compress");
    assert!(
        c.len() > 6,
        "token-only walk must keep a token-sized tail, got {} messages",
        c.len()
    );
    // The newest turn survives verbatim.
    assert!(c.iter().any(|x| x.content.contains("assistant-11")));
    // Way under budget → untouched.
    let c2 = compact_messages_with_budget(&m, 0, 10_000, None);
    assert_eq!(c2.len(), m.len());
}

#[test]
fn compact_never_folds_inflight_tool_results() {
    // A transcript that ends mid-turn — assistant tool_calls whose
    // results just came back — must never have the CURRENT turn's
    // results folded into the summary (the model would lose the
    // output of the calls it just made, with no error). Compaction
    // backs off to the enclosing safe boundary instead.
    let call = |id: &str| crate::llm::provider::ToolCall {
        id: id.into(),
        name: "bash".into(),
        arguments: serde_json::json!({}),
    };
    let m = vec![
        ChatMessage::system("SYS"),
        ChatMessage::user("first task"),
        ChatMessage::assistant_with_tools("call-1", vec![call("c1")]),
        ChatMessage::tool_result("result-1", "c1"),
        ChatMessage::user("second task"),
        ChatMessage::assistant_with_tools("call-2", vec![call("c2")]),
        ChatMessage::tool_result("result-2", "c2"),
    ];
    // Aggressive message budget: the walk wants a tiny tail.
    let c = compact_messages(&m, 4);
    // The in-flight pair (assistant c2 + its result) survives.
    assert!(
        c.iter().any(|x| x.tool_call_id.as_deref() == Some("c2")),
        "in-flight tool result folded away: {}",
        c.iter()
            .map(|x| x.content.as_str())
            .collect::<Vec<_>>()
            .join(" | ")
    );
    let has_c2_call = c.iter().any(|x| {
        x.tool_calls
            .as_ref()
            .is_some_and(|t| t.iter().any(|tc| tc.id == "c2"))
    });
    assert!(
        has_c2_call,
        "assistant call c2 must survive with its result"
    );
    // And the compacted transcript stays tool-pairing valid.
    crate::agent::invariant::check_tool_pairing(&c).expect("pairing must survive compaction");
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
