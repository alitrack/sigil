//! ACP protocol unit tests: drive `handle_message` with a fake turn
//! executor and verify the wire-protocol outputs.

use std::sync::Arc;

use serde_json::json;

use crate::acp::{AcpState, Out, TranscriptStore, TurnFn, handle_message};

/// Echo turn that mimics the real serve() executor: (session_id,
/// prompt, history) → (answer, updated transcript), and it writes the
/// updated transcript back to the shared store — exactly what the real
/// turn executor does in `serve` (the store is the source of truth the
/// prompt handler reads on the next turn / fork clones from).
fn make_fake_turn(store: TranscriptStore) -> Arc<TurnFn> {
    Arc::new(
        move |session_id: String,
              prompt: String,
              history: Vec<crate::llm::provider::ChatMessage>| {
            let store2 = store.clone();
            let prompt2 = prompt.clone();
            let sid2 = session_id.clone();
            let fut = async move {
                let mut updated = history;
                updated.push(crate::llm::provider::ChatMessage::user(&prompt2));
                store2.lock().unwrap().insert(sid2, updated.clone());
                Ok::<(String, Vec<crate::llm::provider::ChatMessage>), anyhow::Error>((
                    format!("fake-echo[{session_id}]: {prompt2}"),
                    updated,
                ))
            };
            Box::pin(fut)
        },
    )
}

fn fake_state() -> AcpState {
    let mut s = AcpState::new();
    s.turn = Some(make_fake_turn(s.transcripts()));
    s
}

/// The channel payload a deferred prompt turn completes with.
type DeferredDone = tokio::sync::oneshot::Receiver<
    anyhow::Result<(String, Vec<crate::llm::provider::ChatMessage>)>,
>;

/// Extract the (id, session_id, done) triple from a Deferred out.
fn deferred(out: Out) -> (serde_json::Value, String, DeferredDone) {
    match out {
        Out::Deferred {
            id,
            session_id,
            done,
        } => (id, session_id, done),
        _ => panic!("expected deferred prompt"),
    }
}

/// Pull `result.sessionId` out of a Response (session/new, fork).
fn session_id_of(out: &Out) -> String {
    match out {
        Out::Response {
            result: Some(r), ..
        } => r["sessionId"].as_str().unwrap().to_string(),
        _ => panic!("expected sessionId response"),
    }
}

#[cfg(test)]
mod acp_tests {
    use super::*;

    #[tokio::test]
    async fn initialize_returns_protocol_version() {
        let mut s = fake_state();
        let outs = handle_message(
            &mut s,
            &json!({
                "jsonrpc": "2.0", "id": 1, "method": "agent/initialize",
                "params": { "protocolVersion": "2025-03-26" }
            }),
        )
        .await
        .unwrap();

        assert_eq!(outs.len(), 1);
        match &outs[0] {
            Out::Response { id, result, error } => {
                assert_eq!(id, &json!(1));
                assert!(error.is_none());
                assert!(
                    result.as_ref().unwrap()["protocolVersion"]
                        .as_str()
                        .is_some()
                );
            }
            _ => panic!("expected response"),
        }
    }

    #[tokio::test]
    async fn session_new_and_prompt_and_close() {
        let mut s = fake_state();
        let outs = handle_message(
            &mut s,
            &json!({
                "jsonrpc": "2.0", "id": 2, "method": "agent/session/new",
                "params": { "cwd": "/tmp", "mcpServers": [] }
            }),
        )
        .await
        .unwrap();
        assert_eq!(outs.len(), 1);
        let session_id = match &outs[0] {
            Out::Response {
                result: Some(r), ..
            } => r["sessionId"].as_str().unwrap().to_string(),
            _ => panic!("expected session id"),
        };

        // prompt — now deferred: the turn executor runs in the
        // background so the loop can answer mid-turn messages.
        let outs = handle_message(
            &mut s,
            &json!({
                "jsonrpc": "2.0", "id": 3, "method": "agent/session.prompt",
                "params": { "sessionId": session_id, "prompt": [{ "type": "text", "text": "hi" }] }
            }),
        )
        .await
        .unwrap();
        assert_eq!(outs.len(), 1);
        let (deferred_id, deferred_sid, done) = deferred(outs.into_iter().next().unwrap());
        assert_eq!(deferred_id, json!(3));
        assert_eq!(deferred_sid, session_id);
        let (text, _transcript) = done.await.unwrap().unwrap();
        assert_eq!(text, format!("fake-echo[{session_id}]: hi"));

        // close
        let outs = handle_message(
            &mut s,
            &json!({
                "jsonrpc": "2.0", "id": 4, "method": "agent/session.close",
                "params": { "sessionId": session_id }
            }),
        )
        .await
        .unwrap();
        assert_eq!(outs.len(), 1);
        match &outs[0] {
            Out::Response { id, error, .. } => {
                assert_eq!(id, &json!(4));
                assert!(error.is_none());
            }
            _ => panic!("expected response"),
        }
    }

    #[tokio::test]
    async fn unknown_method_returns_error() {
        let mut s = fake_state();
        let outs = handle_message(
            &mut s,
            &json!({ "jsonrpc": "2.0", "id": 9, "method": "bogus/method", "params": {} }),
        )
        .await
        .unwrap();
        match &outs[0] {
            Out::Response { error: Some(e), .. } => {
                assert_eq!(e["code"], -32601);
            }
            _ => panic!("expected error response"),
        }
    }

    #[tokio::test]
    async fn turn_error_becomes_jsonrpc_error() {
        let mut s = AcpState::new();
        s.turn = Some(Arc::new(
            |_sid: String, _p: String, _h: Vec<crate::llm::provider::ChatMessage>| {
                let fut = async move {
                    Err::<(String, Vec<crate::llm::provider::ChatMessage>), anyhow::Error>(
                        anyhow::anyhow!("boom"),
                    )
                };
                Box::pin(fut)
            },
        ));
        let outs = handle_message(
            &mut s,
            &json!({
                "jsonrpc": "2.0", "id": 5, "method": "agent/session.prompt",
                "params": { "sessionId": "x", "prompt": [{ "type": "text", "text": "go" }] }
            }),
        )
        .await
        .unwrap();
        assert_eq!(outs.len(), 1);
        let done = match outs.into_iter().next().unwrap() {
            Out::Deferred { done, .. } => done,
            _ => panic!("expected deferred prompt"),
        };
        let err = done.await.unwrap().unwrap_err();
        assert!(err.to_string().contains("boom"), "err: {err}");
    }

    #[tokio::test]
    async fn cancel_and_ping() {
        let mut s = fake_state();
        let outs = handle_message(
            &mut s,
            &json!({ "jsonrpc": "2.0", "id": 6, "method": "agent/cancel", "params": {} }),
        )
        .await
        .unwrap();
        match &outs[0] {
            Out::Response {
                result: Some(r), ..
            } => assert_eq!(r["cancelled"], true),
            _ => panic!("expected response"),
        }
        let outs = handle_message(
            &mut s,
            &json!({ "jsonrpc": "2.0", "id": 7, "method": "ping", "params": {} }),
        )
        .await
        .unwrap();
        match &outs[0] {
            Out::Response {
                result: Some(_),
                error: None,
                ..
            } => {}
            _ => panic!("expected ok response"),
        }
    }

    // ---- sigil extension: mid-turn human input (`agent/request.input`) ----

    #[tokio::test]
    async fn request_input_resolves_pending_question() {
        // Server side registers a question (as the AskCb would);
        // the host answers; the answer reaches the waiting AskCb.
        let mut s = AcpState::new();
        let rx = s.register_input("req-1".to_string());
        let outs = handle_message(
            &mut s,
            &json!({
                "jsonrpc": "2.0", "id": 9, "method": "agent/request.input",
                "params": { "requestId": "req-1", "response": { "answers": ["方案B"] } }
            }),
        )
        .await
        .unwrap();

        // The agent loop (AskCb) received the answer.
        let answered = rx.await.unwrap();
        assert_eq!(answered["answers"][0], json!("方案B"));
        // The host got a confirmation response.
        assert_eq!(outs.len(), 1);
        match &outs[0] {
            Out::Response { id, result, error } => {
                assert_eq!(id, &json!(9));
                assert_eq!(result.as_ref().unwrap()["answered"], true);
                assert!(error.is_none());
            }
            _ => panic!("expected response"),
        }
    }

    #[tokio::test]
    async fn request_input_unknown_request_id_errors() {
        let mut s = AcpState::new();
        let outs = handle_message(
            &mut s,
            &json!({
                "jsonrpc": "2.0", "id": 10, "method": "agent/request.input",
                "params": { "requestId": "nope", "response": {} }
            }),
        )
        .await
        .unwrap();
        match &outs[0] {
            Out::Response { error: Some(e), .. } => assert_eq!(e["code"], -32602),
            _ => panic!("expected error response"),
        }
    }

    #[tokio::test]
    async fn prompt_with_mid_turn_question_round_trip() {
        // Full mid-turn question flow on a single state: the prompt
        // returns a Deferred (turn runs in background), the turn
        // blocks on the registered input, the host answers via
        // `agent/request.input`, and the turn resumes with the answer
        // woven into its output.
        let mut s = AcpState::new();
        let rx = s.register_input("req-2".to_string());
        let rx_slot: Arc<std::sync::Mutex<Option<_>>> = Arc::new(std::sync::Mutex::new(Some(rx)));
        let rx_slot2 = rx_slot.clone();
        s.turn = Some(Arc::new(
            move |_sid: String, prompt: String, _h: Vec<crate::llm::provider::ChatMessage>| {
                let rx_slot2 = rx_slot2.clone();
                Box::pin(async move {
                    let rx = rx_slot2
                        .lock()
                        .unwrap()
                        .take()
                        .ok_or_else(|| anyhow::anyhow!("input already taken"))?;
                    let answer = rx.await.map_err(|_| anyhow::anyhow!("input dropped"))?;
                    let choice = answer["answers"][0].as_str().unwrap_or("?");
                    Ok::<(String, Vec<crate::llm::provider::ChatMessage>), anyhow::Error>((
                        format!("{prompt} → chose {choice}"),
                        Vec::new(),
                    ))
                })
            },
        ));

        // Prompt → deferred; the background turn blocks on the question.
        let outs = handle_message(
            &mut s,
            &json!({
                "jsonrpc": "2.0", "id": 11, "method": "agent/session.prompt",
                "params": { "sessionId": "x", "prompt": [{ "type": "text", "text": "方案A还是B?" }] }
            }),
        )
        .await
        .unwrap();
        let done = match outs.into_iter().next().unwrap() {
            Out::Deferred { done, .. } => done,
            _ => panic!("expected deferred prompt"),
        };

        // Give the turn a moment to block on the question, then answer.
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        let outs = handle_message(
            &mut s,
            &json!({
                "jsonrpc": "2.0", "id": 12, "method": "agent/request.input",
                "params": { "requestId": "req-2", "response": { "answers": ["方案B"] } }
            }),
        )
        .await
        .unwrap();
        assert!(matches!(
            &outs[0],
            Out::Response {
                result: Some(_),
                ..
            }
        ));

        // The deferred turn finished with the answer in its output.
        let (text, _transcript) = done.await.unwrap().unwrap();
        assert!(text.contains("chose 方案B"), "text was: {text}");
    }

    // ---- session context: multi-turn transcripts + fork (NPP parity) ----

    #[tokio::test]
    async fn multi_turn_session_accumulates_transcript() {
        // Two prompts on the same session: the second turn's executor
        // sees the first turn's transcript (fed from the shared store).
        let mut s = fake_state();
        let outs = handle_message(
            &mut s,
            &json!({
                "jsonrpc": "2.0", "id": 1, "method": "agent/session/new",
                "params": { "cwd": "/tmp", "mcpServers": [] }
            }),
        )
        .await
        .unwrap();
        let sid = session_id_of(&outs[0]);

        let (_id, _sid, done1) = deferred(
            handle_message(
                &mut s,
                &json!({
                    "jsonrpc": "2.0", "id": 2, "method": "agent/session.prompt",
                    "params": { "sessionId": sid, "prompt": [{ "type": "text", "text": "first" }] }
                }),
            )
            .await
            .unwrap()
            .into_iter()
            .next()
            .unwrap(),
        );
        let (text1, _t1) = done1.await.unwrap().unwrap();
        assert_eq!(text1, format!("fake-echo[{sid}]: first"));

        // The transcript store now holds turn 1's user message.
        let stored = s.transcripts.lock().unwrap().get(&sid).cloned().unwrap();
        assert_eq!(stored.len(), 1);

        let (_id, _sid, done2) = deferred(
            handle_message(
                &mut s,
                &json!({
                    "jsonrpc": "2.0", "id": 3, "method": "agent/session.prompt",
                    "params": { "sessionId": sid, "prompt": [{ "type": "text", "text": "second" }] }
                }),
            )
            .await
            .unwrap()
            .into_iter()
            .next()
            .unwrap(),
        );
        // The fake turn appends to the RECEIVED history — so the
        // returned transcript proves turn 2 saw turn 1's context.
        let (_text2, transcript2) = done2.await.unwrap().unwrap();
        assert_eq!(transcript2.len(), 2);
        assert!(transcript2.iter().any(|m| m.content.contains("first")));
        assert!(transcript2.iter().any(|m| m.content.contains("second")));
    }

    #[tokio::test]
    async fn session_fork_inherits_parent_transcript() {
        let mut s = fake_state();
        let outs = handle_message(
            &mut s,
            &json!({
                "jsonrpc": "2.0", "id": 1, "method": "agent/session/new",
                "params": { "cwd": "/work", "mcpServers": [] }
            }),
        )
        .await
        .unwrap();
        let parent = session_id_of(&outs[0]);
        let (_id, _sid, done) = deferred(
            handle_message(
                &mut s,
                &json!({
                    "jsonrpc": "2.0", "id": 2, "method": "agent/session.prompt",
                    "params": { "sessionId": parent, "prompt": [{ "type": "text", "text": "parent turn" }] }
                }),
            )
            .await
            .unwrap()
            .into_iter()
            .next()
            .unwrap(),
        );
        done.await.unwrap().unwrap();

        // Fork: the branch clones the parent's transcript + cwd.
        let outs = handle_message(
            &mut s,
            &json!({
                "jsonrpc": "2.0", "id": 3, "method": "agent/session/fork",
                "params": { "fromSessionId": parent }
            }),
        )
        .await
        .unwrap();
        let branch = session_id_of(&outs[0]);
        assert_ne!(branch, parent);
        assert!(branch.starts_with("sigil-fork-"));

        let branch_transcript = s.transcripts.lock().unwrap().get(&branch).cloned().unwrap();
        assert!(
            branch_transcript
                .iter()
                .any(|m| m.content.contains("parent turn")),
            "branch must inherit the parent's context"
        );

        // Prompting the branch sees the inherited context.
        let (_id, _sid, done2) = deferred(
            handle_message(
                &mut s,
                &json!({
                    "jsonrpc": "2.0", "id": 4, "method": "agent/session.prompt",
                    "params": { "sessionId": branch, "prompt": [{ "type": "text", "text": "branch turn" }] }
                }),
            )
            .await
            .unwrap()
            .into_iter()
            .next()
            .unwrap(),
        );
        let (text, transcript) = done2.await.unwrap().unwrap();
        assert_eq!(text, format!("fake-echo[{branch}]: branch turn"));
        assert!(transcript.iter().any(|m| m.content.contains("parent turn")));

        // Closing the branch removes only the branch's state.
        let outs = handle_message(
            &mut s,
            &json!({
                "jsonrpc": "2.0", "id": 5, "method": "agent/session.close",
                "params": { "sessionId": branch }
            }),
        )
        .await
        .unwrap();
        assert!(matches!(&outs[0], Out::Response { error: None, .. }));
        let store = s.transcripts.lock().unwrap();
        assert!(!store.contains_key(&branch));
        assert!(store.contains_key(&parent), "parent must survive");
    }

    #[tokio::test]
    async fn fork_unknown_session_errors() {
        let mut s = fake_state();
        let outs = handle_message(
            &mut s,
            &json!({
                "jsonrpc": "2.0", "id": 9, "method": "agent/session/fork",
                "params": { "fromSessionId": "nope" }
            }),
        )
        .await
        .unwrap();
        match &outs[0] {
            Out::Response { error: Some(e), .. } => assert_eq!(e["code"], -32602),
            _ => panic!("expected error response"),
        }
    }
}
