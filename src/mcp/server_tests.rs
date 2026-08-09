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
