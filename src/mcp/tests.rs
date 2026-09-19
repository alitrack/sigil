//! MCP client integration tests: connect to the bundled echo server
//! (test-tools/mcp_echo_server.py), discover tools, call one.

use crate::mcp::McpClient;

#[cfg(test)]
mod mcp_tests {
    use super::*;

    fn server_path() -> String {
        // Locate the echo server relative to the crate root.
        let mut path = std::env::current_dir().unwrap_or_default();
        // When running via cargo test, cwd is the crate root.
        if !path.join("test-tools/mcp_echo_server.py").exists() {
            // Fallback: one level up (when running in a subdir).
            path.pop();
        }
        path.join("test-tools/mcp_echo_server.py")
            .to_string_lossy()
            .to_string()
    }

    #[tokio::test]
    async fn connects_and_discovers_echo_tool() {
        let (client, tools) = McpClient::connect("echo_test", "python3", &[server_path()])
            .await
            .expect("connect to echo server");

        assert_eq!(tools.len(), 1);
        assert_eq!(tools[0].name, "echo");
        assert!(tools[0].description.contains("Echo"));
        assert!(tools[0].schema.get("type").is_some());

        // Call the tool.
        let result = client
            .call_tool("echo", serde_json::json!({"text": "hello sigil"}))
            .await
            .expect("call echo tool");
        assert_eq!(result, serde_json::json!("echo: hello sigil"));
    }

    #[tokio::test]
    async fn multiple_requests_route_correctly() {
        let (client, _) = McpClient::connect("echo_test2", "python3", &[server_path()])
            .await
            .expect("connect");

        let client = std::sync::Arc::new(client);

        // Fire several requests concurrently; each must get its own reply.
        let mut handles = Vec::new();
        for i in 0..5 {
            let c = client.clone();
            handles.push(tokio::spawn(async move {
                c.call_tool("echo", serde_json::json!({"text": format!("msg{i}")}))
                    .await
                    .unwrap()
            }));
        }
        for (i, h) in handles.into_iter().enumerate() {
            let got = h.await.unwrap();
            assert!(got.as_str().unwrap().contains(&format!("msg{i}")));
        }
    }

    #[tokio::test]
    async fn lists_and_reads_resources() {
        let (client, _) = McpClient::connect("echo_res", "python3", &[server_path()])
            .await
            .expect("connect");

        // resources/list → one resource.
        let resources = client.list_resources().await.expect("list resources");
        assert_eq!(resources.len(), 1);
        assert_eq!(
            resources[0].get("uri").and_then(|v| v.as_str()),
            Some("echo://greeting")
        );

        // resources/read → content.
        let text = client
            .read_resource("echo://greeting")
            .await
            .expect("read resource");
        assert!(text.contains("Hello from the echo server resource"));

        // Unknown URI → error.
        let err = client.read_resource("echo://nope").await;
        assert!(err.is_err());
    }

    #[tokio::test]
    async fn lists_and_gets_prompts() {
        let (client, _) = McpClient::connect("echo_prompt", "python3", &[server_path()])
            .await
            .expect("connect");

        // prompts/list → one prompt.
        let prompts = client.list_prompts().await.expect("list prompts");
        assert_eq!(prompts.len(), 1);
        assert_eq!(
            prompts[0].get("name").and_then(|v| v.as_str()),
            Some("greet")
        );

        // prompts/get → rendered messages.
        let text = client.get_prompt("greet").await.expect("get prompt");
        assert!(text.contains("Please greet the user warmly"));

        // Unknown prompt → error.
        let err = client.get_prompt("nope").await;
        assert!(err.is_err());
    }

    mod sse_parse_tests {
        use crate::mcp::parse_sse_messages;
        use serde_json::json;

        #[test]
        fn parses_all_events_and_joins_multiline_data() {
            // Two events; the first carries a JSON payload split across two
            // `data:` lines (SSE multi-line data must be joined with '\n').
            let body = "event: message\n\
                    data: {\"id\": 7, \"result\": {\"a\":\n\
                    data:  1}}\n\n\
                    event: message\n\
                    data: {\"id\": 8, \"result\": {\"b\": 2}}\n\n";
            let msgs = parse_sse_messages(body);
            assert_eq!(msgs.len(), 2, "both events must parse: {msgs:?}");
            assert_eq!(msgs[0], json!({"id": 7, "result": {"a": 1}}));
            assert_eq!(msgs[1], json!({"id": 8, "result": {"b": 2}}));
        }

        #[test]
        fn notifications_before_response_are_separate_messages() {
            // A Streamable-HTTP server may send progress notifications
            // before the JSON-RPC response: callers must be able to pick the
            // response by id (the old first-data:-line parser matched the
            // notification instead).
            let body = "event: message\n\
                    data: {\"jsonrpc\": \"2.0\", \"method\": \"notifications/progress\", \"params\": {\"progress\": 0.5}}\n\n\
                    event: message\n\
                    data: {\"jsonrpc\": \"2.0\", \"id\": 42, \"result\": {\"ok\": true}}\n\n";
            let msgs = parse_sse_messages(body);
            assert_eq!(msgs.len(), 2);
            let resp = msgs
                .iter()
                .find(|m| m.get("id").and_then(serde_json::Value::as_u64) == Some(42))
                .expect("response with id 42 must be findable");
            assert_eq!(resp["result"]["ok"], json!(true));
        }

        #[test]
        fn data_without_space_and_nonjson_payloads() {
            // `data:x` (no leading space) is a valid SSE field; non-JSON
            // payloads (the [DONE] sentinel) are skipped, not fatal.
            let body = "data:{\"id\": 1}\n\ndata: [DONE]\n\n: keep-alive comment\ndata:\n\n";
            let msgs = parse_sse_messages(body);
            assert_eq!(msgs.len(), 1, "only the JSON event must parse: {msgs:?}");
            assert_eq!(msgs[0]["id"], json!(1));
        }

        #[test]
        fn no_data_lines_yields_empty() {
            assert!(parse_sse_messages("").is_empty());
            assert!(parse_sse_messages(": ping\n: ping\n\n").is_empty());
            assert!(parse_sse_messages("event: message\nid: 1\n\n").is_empty());
        }
    }
}
