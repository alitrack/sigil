//! ACP protocol unit tests: drive `handle_message` with a fake turn
//! executor and verify the wire-protocol outputs.

use std::sync::Arc;

use serde_json::json;

use crate::acp::{AcpState, Out, handle_message};

fn fake_state() -> AcpState {
    let mut s = AcpState::new();
    s.turn = Some(Arc::new(|prompt: String| {
        let fut = async move { Ok::<String, anyhow::Error>(format!("fake-echo: {prompt}")) };
        Box::pin(fut)
    }));
    s
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

        // prompt
        let outs = handle_message(
            &mut s,
            &json!({
                "jsonrpc": "2.0", "id": 3, "method": "agent/session.prompt",
                "params": { "sessionId": session_id, "prompt": [{ "type": "text", "text": "hi" }] }
            }),
        )
        .await
        .unwrap();
        assert_eq!(outs.len(), 3); // chunk + complete + response
        // First is a chunk notification.
        match &outs[0] {
            Out::Notification {
                session_id: sid,
                update,
            } => {
                assert_eq!(sid, &session_id);
                assert_eq!(update["sessionUpdate"], "agent_message_chunk");
                assert_eq!(update["content"]["text"], "fake-echo: hi");
            }
            _ => panic!("expected notification"),
        }
        // Last is the response.
        match &outs[2] {
            Out::Response { id, result, error } => {
                assert_eq!(id, &json!(3));
                assert!(error.is_none());
                assert_eq!(result.as_ref().unwrap()["stopReason"], "end_turn");
            }
            _ => panic!("expected response"),
        }

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
        s.turn = Some(Arc::new(|_p: String| {
            let fut = async move { Err::<String, anyhow::Error>(anyhow::anyhow!("boom")) };
            Box::pin(fut)
        }));
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
        match &outs[0] {
            Out::Response { error: Some(e), .. } => {
                assert_eq!(e["code"], -32603);
                assert!(e["message"].as_str().unwrap().contains("boom"));
            }
            _ => panic!("expected error response"),
        }
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
        let mut rx = s.register_input("req-1".to_string());

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
            Out::Response {
                error: Some(e), ..
            } => assert_eq!(e["code"], -32602),
            _ => panic!("expected error response"),
        }
    }

    #[tokio::test]
    async fn turn_executor_can_wait_for_input_round_trip() {
        // Mechanism proof: a turn executor that pauses on a oneshot
        // (exactly what the AskCb will do) and resumes once the answer
        // arrives — the protocol-level half of the mid-turn question
        // flow. (The serve loop itself needs a concurrent select to
        // avoid deadlock; that is the documented follow-up.)
        let mut s = AcpState::new();
        let rx = s.register_input("req-2".to_string());
        let rx_slot: Arc<std::sync::Mutex<Option<_>>> = Arc::new(std::sync::Mutex::new(Some(rx)));
        let rx_slot2 = rx_slot.clone();

        // Separate state for the prompt side (its turn executor
        // references the pending input registered on `s`).
        let mut s2 = AcpState::new();
        s2.turn = Some(Arc::new(move |prompt: String| {
            let rx_slot2 = rx_slot2.clone();
            Box::pin(async move {
                let rx = rx_slot2
                    .lock()
                    .unwrap()
                    .take()
                    .ok_or_else(|| anyhow::anyhow!("input already taken"))?;
                let answer = rx.await.map_err(|_| anyhow::anyhow!("input dropped"))?;
                let choice = answer["answers"][0].as_str().unwrap_or("?");
                Ok::<String, anyhow::Error>(format!("{prompt} → chose {choice}"))
            })
        }));

        // Kick the prompt in the background (its turn will block on rx).
        let prompt_handle = tokio::spawn(async move {
            handle_message(
                &mut s2,
                &json!({
                    "jsonrpc": "2.0", "id": 11, "method": "agent/session.prompt",
                    "params": { "sessionId": "x", "prompt": [{ "type": "text", "text": "方案A还是B?" }] }
                }),
            )
            .await
        });

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
        assert!(matches!(&outs[0], Out::Response { result: Some(_), .. }));

        // The turn finished with the answer woven into its output.
        let prompt_outs = prompt_handle.await.unwrap().unwrap();
        let text = prompt_outs
            .iter()
            .find_map(|o| match o {
                Out::Notification { update, .. } => update["content"]["text"].as_str(),
                _ => None,
            })
            .unwrap_or("");
        assert!(text.contains("chose 方案B"), "text was: {text}");
    }
}
