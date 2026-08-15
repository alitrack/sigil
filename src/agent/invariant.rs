//! 请求不变式 — "Model-visible means logged"（Phase A2，对齐 DeepSeek Harness
//! invariant / cc RequestInvariant）。
//!
//! 两层校验：
//! - [`check_tool_pairing`]：工具调用配对完整性（半完成 step = 崩溃残留 → 硬拦截）。
//!   自包含，run_messages 每轮请求前调用。
//! - [`validate_against_derived`]：请求消息与会话事件派生视图一致（旁路注入/篡改拦截；
//!   消息被压缩时降级为配对检查）。调用方（REPL/daemon）持有完整历史事件，在此校验。

use crate::llm::{ChatMessage, Role};
use crate::session::{Event, events_to_messages};

/// 不变式违例（可显示、可传播）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InvariantViolation(pub String);

impl std::fmt::Display for InvariantViolation {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.0)
    }
}

impl std::error::Error for InvariantViolation {}

/// 工具配对完整性：每条 assistant(tool_calls) 的 call id 必须有配对的 tool 消息。
/// 未配对 = 半完成 step（崩溃/异常残留）→ 硬拦截，防止把残缺工具历史发给模型。
pub fn check_tool_pairing(messages: &[ChatMessage]) -> Result<(), InvariantViolation> {
    let mut pending: std::collections::HashSet<String> = std::collections::HashSet::new();

    for m in messages {
        if let Some(calls) = &m.tool_calls {
            for c in calls {
                if !c.id.is_empty() {
                    pending.insert(c.id.clone());
                }
            }
        }
        if let Some(tid) = &m.tool_call_id {
            pending.remove(tid);
        }
    }

    if !pending.is_empty() {
        let mut ids: Vec<&String> = pending.iter().collect();
        ids.sort();
        return Err(InvariantViolation(format!(
            "未配对的工具调用（半完成 step）：{}",
            ids.iter()
                .map(|s| s.as_str())
                .collect::<Vec<_>>()
                .join(", ")
        )));
    }
    Ok(())
}

/// 消息一致性：请求消息的对话部分（去 system）必须与事件派生视图一致。
/// - 实际更少（压缩/裁剪）→ 降级为配对检查；
/// - 实际更多（旁路注入/脱同步）→ 拦截；
/// - 数量相同但内容不同（篡改）→ 拦截。
///
/// TODO(Phase A2 接入): REPL/daemon 请求构造切换到"先写 user 事件再校验"后接入；
/// run_messages 内部目前只用 check_tool_pairing（消息由调用方持有完整历史事件）。
#[allow(dead_code)]
pub fn validate_against_derived(
    events: &[Event],
    messages: &[ChatMessage],
) -> Result<(), InvariantViolation> {
    let derived = events_to_messages(events);
    let derived_dialog: Vec<&ChatMessage> =
        derived.iter().filter(|m| m.role != Role::System).collect();
    let actual_dialog: Vec<&ChatMessage> =
        messages.iter().filter(|m| m.role != Role::System).collect();

    if actual_dialog.len() < derived_dialog.len() {
        // 压缩降级：只做配对完整性
        return check_tool_pairing(messages);
    }

    if actual_dialog.len() > derived_dialog.len() {
        return Err(InvariantViolation(format!(
            "请求消息比日志派生多 {} 条（旁路注入或脱同步）",
            actual_dialog.len() - derived_dialog.len()
        )));
    }

    for (actual, expected) in actual_dialog.iter().zip(derived_dialog.iter()) {
        if !message_equal(actual, expected) {
            return Err(InvariantViolation(
                "请求消息与日志派生不一致（log-reconstruction desync）".into(),
            ));
        }
    }

    check_tool_pairing(messages)
}

/// 逐条消息比较（role + content + tool_call_id + tool_calls）。
#[allow(dead_code)]
fn message_equal(a: &ChatMessage, b: &ChatMessage) -> bool {
    if a.role != b.role || a.content != b.content || a.tool_call_id != b.tool_call_id {
        return false;
    }
    match (&a.tool_calls, &b.tool_calls) {
        (None, None) => true,
        (Some(x), Some(y)) => {
            x.len() == y.len()
                && x.iter()
                    .zip(y.iter())
                    .all(|(p, q)| p.id == q.id && p.name == q.name && p.arguments == q.arguments)
        }
        _ => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::llm::{ChatMessage, Role, ToolCall};

    fn user(text: &str) -> ChatMessage {
        ChatMessage::user(text)
    }

    fn assistant_with_call(id: &str, name: &str) -> ChatMessage {
        ChatMessage::assistant_with_tools(
            "",
            vec![ToolCall {
                id: id.into(),
                name: name.into(),
                arguments: serde_json::json!({}),
            }],
        )
    }

    #[test]
    fn pairing_passes_when_all_calls_paired() {
        let messages = vec![
            user("读文件"),
            assistant_with_call("call_1", "read_file"),
            ChatMessage::tool_result("content", "call_1"),
        ];
        assert!(check_tool_pairing(&messages).is_ok());
    }

    #[test]
    fn pairing_rejects_unpaired_call() {
        let messages = vec![
            user("读文件"),
            assistant_with_call("call_1", "read_file"),
            // 无配对 tool 消息（崩溃残留）
        ];
        let err = check_tool_pairing(&messages).unwrap_err();
        assert!(err.to_string().contains("call_1"));
    }

    #[test]
    fn pairing_rejects_unpaired_call_with_two_calls_one_result() {
        let messages = vec![
            user("并行读"),
            ChatMessage::assistant_with_tools(
                "",
                vec![
                    ToolCall {
                        id: "call_1".into(),
                        name: "read_file".into(),
                        arguments: serde_json::json!({}),
                    },
                    ToolCall {
                        id: "call_2".into(),
                        name: "read_file".into(),
                        arguments: serde_json::json!({}),
                    },
                ],
            ),
            ChatMessage::tool_result("a", "call_1"),
        ];
        assert!(check_tool_pairing(&messages).is_err());
    }

    #[test]
    fn derived_validation_passes_on_full_history() {
        let events = vec![
            Event::User {
                ts: None,
                text: "读文件".into(),
            },
            Event::ToolCall {
                ts: None,
                id: "call_1".into(),
                name: "read_file".into(),
                args: "{}".into(),
            },
            Event::ToolResult {
                ts: None,
                id: "call_1".into(),
                name: "read_file".into(),
                ok: true,
                output: "content".into(),
            },
            Event::Assistant {
                ts: None,
                text: "完成".into(),
            },
        ];
        // 完整派生（含工具历史）== 请求消息
        let messages = events_to_messages(&events);
        assert!(validate_against_derived(&events, &messages).is_ok());
    }

    #[test]
    fn derived_validation_rejects_extra_unlogged_message() {
        let events = vec![Event::User {
            ts: None,
            text: "hi".into(),
        }];
        let mut messages = events_to_messages(&events);
        // 旁路注入一条日志里没有的消息
        messages.push(user("继续"));

        assert!(validate_against_derived(&events, &messages).is_err());
    }

    #[test]
    fn derived_validation_degrades_to_pairing_on_compaction() {
        let events = vec![
            Event::User {
                ts: None,
                text: "hi".into(),
            },
            Event::Assistant {
                ts: None,
                text: "回答".into(),
            },
        ];
        // 压缩后消息更少（只剩 user）→ 降级配对检查，不拦截
        let messages = vec![user("hi")];
        assert!(validate_against_derived(&events, &messages).is_ok());
    }

    #[test]
    fn derived_validation_rejects_tampered_content() {
        let events = vec![
            Event::User {
                ts: None,
                text: "原始输入".into(),
            },
            Event::Assistant {
                ts: None,
                text: "原始回答".into(),
            },
        ];
        let mut messages = events_to_messages(&events);
        // 篡改回答内容（数量相同）
        messages[1] = ChatMessage {
            role: Role::Assistant,
            content: "被篡改的回答".into(),
            tool_calls: None,
            tool_call_id: None,
        };

        assert!(validate_against_derived(&events, &messages).is_err());
    }
}
