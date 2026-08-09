//! Unit tests for the Phase-0 agent loop: tool-call extraction and the
//! OpenAI chunk parser.

use crate::agent::extract_tool_call;
use crate::llm::openai::parse_stream;

#[cfg(test)]
mod agent_tests {
    use super::*;

    #[test]
    fn extracts_fenced_json_tool_call() {
        let text = "Let me check the repo.\n\n```json\n{\"tool\": \"bash\", \"arguments\": {\"command\": \"ls -la\"}}\n```\n";
        let call = extract_tool_call(text, 3).expect("should extract");
        assert_eq!(call.id, "call_3");
        assert_eq!(call.name, "bash");
        assert_eq!(call.arguments["command"], "ls -la");
    }

    #[test]
    fn no_tool_call_when_plain_answer() {
        let text = "The bug is fixed. Nothing to run.";
        assert!(extract_tool_call(text, 1).is_none());
    }

    #[test]
    fn handles_json_without_args() {
        let text = "```json\n{\"tool\": \"bash\"}\n```";
        let call = extract_tool_call(text, 1).expect("should extract");
        assert_eq!(call.name, "bash");
        assert!(call.arguments.is_null());
    }
}

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
        while let Some(chunk) = stream.next().await {
            let chunk = chunk.expect("no parse error");
            if !chunk.thinking {
                text.push_str(&chunk.delta);
            }
            if chunk.done {
                break;
            }
        }
        assert_eq!(text, "Hello world");
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
    async fn bad_chunk_returns_error_not_panic() {
        let sse = "data: not-json\n\n";
        let mut stream = parse_stream(sse.as_bytes().to_vec());
        let first = stream.next().await;
        assert!(first.is_some());
        assert!(first.unwrap().is_err());
    }
}
