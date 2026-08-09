//! Unit tests for the agent loop: SSE parsing, tool-call extraction.

use crate::llm::openai::{parse_stream, parse_tool_calls};

#[cfg(test)]
mod parser_tests {
    use super::*;
    use futures_util::StreamExt;

    #[tokio::test]
    async fn parses_sse_chunks() {
        let sse = "\
data: {\"choices\":[{\"delta\":{\"content\":\"Hello\"},\"finish_reason\":null}]}

data: {\"choices\":[{\"delta\":{\"content\":\" world\"},\"finish_reason\":null}]}

data: {\"choices\":[{\"delta\":{\"content\":\"\"},\"finish_reason\":\"stop\"}]}

data: [DONE]
";
        let mut stream = parse_stream(sse.as_bytes().to_vec());
        let mut text = String::new();
        let mut tool_calls_requested = false;
        while let Some(chunk) = stream.next().await {
            let chunk = chunk.expect("no parse error");
            if !chunk.thinking {
                text.push_str(&chunk.delta);
            }
            tool_calls_requested |= chunk.tool_calls_requested;
            if chunk.done {
                break;
            }
        }
        assert_eq!(text, "Hello world");
        assert!(!tool_calls_requested);
    }

    #[tokio::test]
    async fn parses_reasoning_content_as_thinking() {
        let sse = "\
data: {\"choices\":[{\"delta\":{\"reasoning_content\":\"think hard\"},\"finish_reason\":null}]}

data: {\"choices\":[{\"delta\":{\"content\":\"answer\"},\"finish_reason\":\"stop\"}]}

data: [DONE]
";
        let mut stream = parse_stream(sse.as_bytes().to_vec());
        let mut saw_thinking = false;
        let mut text = String::new();
        while let Some(chunk) = stream.next().await {
            let chunk = chunk.expect("no parse error");
            saw_thinking |= chunk.thinking;
            if !chunk.thinking {
                text.push_str(&chunk.delta);
            }
            if chunk.done {
                break;
            }
        }
        assert!(saw_thinking);
        assert_eq!(text, "answer");
    }

    #[tokio::test]
    async fn signals_tool_call_finish() {
        let sse = "\
data: {\"choices\":[{\"delta\":{\"content\":\"\"},\"finish_reason\":\"tool_calls\"}]}

data: [DONE]
";
        let mut stream = parse_stream(sse.as_bytes().to_vec());
        let mut tool_calls_requested = false;
        let mut done = false;
        while let Some(chunk) = stream.next().await {
            let chunk = chunk.expect("no parse error");
            tool_calls_requested |= chunk.tool_calls_requested;
            done |= chunk.done;
            if chunk.done {
                break;
            }
        }
        assert!(tool_calls_requested);
        assert!(done);
    }

    #[tokio::test]
    async fn bad_chunk_returns_error_not_panic() {
        let sse = "data: not-json\n\n";
        let mut stream = parse_stream(sse.as_bytes().to_vec());
        let first = stream.next().await;
        assert!(first.is_some());
        assert!(first.unwrap().is_err());
    }

    #[test]
    fn parses_native_tool_calls_from_complete_response() {
        let v: serde_json::Value = serde_json::json!({
            "choices": [{
                "message": {
                    "content": "",
                    "tool_calls": [{
                        "id": "call_00_abc",
                        "type": "function",
                        "function": {
                            "name": "bash",
                            "arguments": "{\"command\": \"ls -la\"}"
                        }
                    }]
                }
            }]
        });
        let calls = parse_tool_calls(&v);
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].id, "call_00_abc");
        assert_eq!(calls[0].name, "bash");
        assert_eq!(calls[0].arguments["command"], "ls -la");
    }

    #[test]
    fn no_tool_calls_when_message_has_none() {
        let v: serde_json::Value = serde_json::json!({
            "choices": [{"message": {"content": "hi", "tool_calls": null}}]
        });
        assert!(parse_tool_calls(&v).is_empty());
    }
}
