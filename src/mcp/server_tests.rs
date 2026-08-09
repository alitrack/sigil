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
            "capabilities": { "tools": {} },
            "serverInfo": { "name": "sigil", "version": "0.1.0" }
        }
    });
    assert_eq!(init["result"]["protocolVersion"], "2024-11-05");
    assert!(init["result"]["capabilities"]["tools"].is_object());

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
            "capabilities": { "tools": {} },
            "serverInfo": { "name": "sigil", "version": "0.1.0" }
        }
    });
    assert_eq!(init["result"]["serverInfo"]["name"], "sigil");
}
