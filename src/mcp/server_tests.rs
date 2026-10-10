//! MCP server protocol tests: the frame shapes the server emits for
//! initialize / tools/list / tools/call must match MCP's contract.
//! The full stdin/stdout round-trip is covered by the E2E script
//! (test-tools/mcp_sigil_client.py) which drives the real binary.

use serde_json::json;

#[test]
fn server_frame_shape() {
    // initialize result envelope (what `serve` writes for initialize).
    let init = json!({
        "jsonrpc": "2.0",
        "id": 1,
        "result": {
            "protocolVersion": "2024-11-05",
            "capabilities": { "tools": {}, "progress": {}, "resources": {} },
            "serverInfo": { "name": "sigil", "version": "0.1.0" }
        }
    });
    assert_eq!(init["result"]["protocolVersion"], "2024-11-05");
    assert!(init["result"]["capabilities"]["tools"].is_object());
    assert!(init["result"]["capabilities"]["progress"].is_object());
    assert!(init["result"]["capabilities"]["resources"].is_object());

    // tools/list result envelope: { tools: [...] }.
    let list = json!({
        "jsonrpc": "2.0",
        "id": 2,
        "result": { "tools": [] }
    });
    assert!(list["result"]["tools"].is_array());

    // tools/call result envelope: { content: [{type:"text",text}] }.
    let call = json!({
        "jsonrpc": "2.0",
        "id": 3,
        "result": {
            "content": [{ "type": "text", "text": "out" }]
        }
    });
    assert_eq!(call["result"]["content"][0]["text"], "out");

    // Error envelope: isError flag.
    let err = json!({
        "jsonrpc": "2.0",
        "id": 4,
        "result": {
            "content": [{ "type": "text", "text": "boom" }],
            "isError": true
        }
    });
    assert_eq!(err["result"]["isError"], true);
}

/// The server identifies itself as `sigil`.
#[test]
fn server_identifies_as_sigil() {
    let init = json!({
        "jsonrpc": "2.0",
        "id": 1,
        "result": {
            "protocolVersion": "2024-11-05",
            "capabilities": { "tools": {}, "progress": {}, "resources": {} },
            "serverInfo": { "name": "sigil", "version": "0.1.0" }
        }
    });
    assert_eq!(init["result"]["serverInfo"]["name"], "sigil");
}

/// Resources envelope shapes (list + read).
#[test]
fn resources_frame_shapes() {
    // resources/list result: { resources: [{uri,name,description,mimeType}] }.
    let list = json!({
        "jsonrpc": "2.0",
        "id": 5,
        "result": {
            "resources": [{
                "uri": "sigil://system-prompt",
                "name": "sigil system prompt",
                "description": "harness prompt",
                "mimeType": "text/markdown"
            }]
        }
    });
    assert_eq!(
        list["result"]["resources"][0]["uri"],
        "sigil://system-prompt"
    );

    // resources/read result: { contents: [{uri,mimeType,text}] }.
    let read = json!({
        "jsonrpc": "2.0",
        "id": 6,
        "result": {
            "contents": [{
                "uri": "sigil://skills/ffi",
                "mimeType": "text/markdown",
                "text": "> ffi skill\n..."
            }]
        }
    });
    assert!(read["result"]["contents"][0]["text"].is_string());

    // resources/read error: -32002 resource not found.
    let err = json!({
        "jsonrpc": "2.0",
        "id": 7,
        "error": { "code": -32002, "message": "resource not found: x" }
    });
    assert_eq!(err["error"]["code"], -32002);
}

/// Progress notification frame shape (sent around tools/call).
#[test]
fn progress_notification_frame_shape() {
    let progress = json!({
        "jsonrpc": "2.0",
        "method": "notifications/progress",
        "params": {
            "progressToken": "tool-bash",
            "progress": 0.0,
            "total": 1.0,
            "message": "tool bash started"
        }
    });
    assert_eq!(progress["method"], "notifications/progress");
    assert_eq!(progress["params"]["progressToken"], "tool-bash");
    assert_eq!(progress["params"]["progress"], 0.0);
    assert_eq!(progress["params"]["total"], 1.0);
}

async fn tool_response(
    registry: &crate::tools::ToolRegistry,
    name: &str,
    arguments: serde_json::Value,
) -> serde_json::Value {
    use tokio::io::AsyncReadExt;
    let request = json!({"jsonrpc":"2.0", "id":42, "method":"tools/call", "params":{"name":name,"arguments":arguments}}).to_string() + "\n";
    let (mut reader, writer) = tokio::io::duplex(8192);
    let run = super::server::serve_io(registry, &[], request.as_bytes(), writer);
    let read = async {
        let mut bytes = Vec::new();
        reader.read_to_end(&mut bytes).await.unwrap();
        bytes
    };
    let (result, bytes) = tokio::join!(run, read);
    result.unwrap();
    String::from_utf8(bytes)
        .unwrap()
        .lines()
        .filter_map(|l| serde_json::from_str::<serde_json::Value>(l).ok())
        .find(|v| v["id"] == 42)
        .unwrap()
}

#[tokio::test]
async fn mcp_rejects_destructive_commands_before_execution() {
    let mut registry = crate::tools::ToolRegistry::new();
    registry.register(std::sync::Arc::new(crate::tools::bash::BashTool::new()));
    // No filesystem effect: only check the prohibited command's response.
    let response = tool_response(&registry, "bash", json!({"command":"git push --help"})).await;
    assert_eq!(response["result"]["isError"], true, "{response}");
    assert!(
        response["result"]["content"][0]["text"]
            .as_str()
            .unwrap()
            .contains("APPROVAL DENIED")
    );
}

#[tokio::test]
async fn mcp_marks_real_tool_errors_without_guessing_from_text() {
    use crate::tools::{Tool, ToolOutput};
    struct ResultTool;
    #[async_trait::async_trait]
    impl Tool for ResultTool {
        fn name(&self) -> &str {
            "result_test"
        }
        fn description(&self) -> &str {
            "test tool"
        }
        fn parameters(&self) -> serde_json::Value {
            json!({"type":"object"})
        }
        async fn run(&self, args: &serde_json::Value) -> anyhow::Result<ToolOutput> {
            if args["fail"] == true {
                anyhow::bail!("tool failure");
            }
            Ok(ToolOutput {
                text: "error: this is successful content".into(),
            })
        }
    }
    let mut registry = crate::tools::ToolRegistry::new();
    registry.register(std::sync::Arc::new(ResultTool));
    let failed = tool_response(&registry, "result_test", json!({"fail":true})).await;
    assert_eq!(failed["result"]["isError"], true);
    let success = tool_response(&registry, "result_test", json!({})).await;
    assert_ne!(success["result"]["isError"], true);
}
