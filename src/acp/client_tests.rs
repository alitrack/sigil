//! ACP client tests: drive a fake ACP server process end to end.

use crate::acp::client::connect;

#[tokio::test]
async fn connects_initializes_and_prompts() {
    // Locate the fake server script (repo test-tools dir).
    let script = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("test-tools")
        .join("fake_acp_server.py");
    assert!(
        script.exists(),
        "fake_acp_server.py missing: {}",
        script.display()
    );

    let mut client = connect("python3", &[script.to_string_lossy().to_string()])
        .await
        .expect("connect to fake acp server");

    let reply = client.prompt("hello").await.expect("prompt");
    assert!(
        reply.contains("Hello from fake ACP agent"),
        "unexpected reply: {reply}"
    );

    client.close().await.expect("close");
}
