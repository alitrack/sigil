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

#[tokio::test]
async fn regression_prompt_propagates_protocol_errors() {
    let code = r#"import sys,json
for line in sys.stdin:
 r=json.loads(line)
 if r['method']=='agent/initialize': out={'result':{}}
 elif r['method']=='agent/session/new': out={'result':{'sessionId':'fake'}}
 else: out={'error':{'code':-32000,'message':'provider unavailable'}}
 out.update(jsonrpc='2.0',id=r['id'])
 print(json.dumps(out),flush=True)
"#;
    let mut client = connect("python3", &["-c".into(), code.into()])
        .await
        .unwrap();
    let error = client
        .prompt("hello")
        .await
        .expect_err("failed prompt must not look successful");
    assert!(error.to_string().contains("provider unavailable"));
}

#[tokio::test]
async fn regression_child_inherits_session_directory() {
    let dir = std::env::temp_dir().join(format!(
        "sigil-acp-child-{}",
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    std::fs::create_dir_all(&dir).unwrap();
    let code = r#"import sys,json,os
for line in sys.stdin:
 r=json.loads(line)
 if r['method']=='agent/initialize': out={'result':{}}
 elif r['method']=='agent/session/new':
  cwd=r.get('params',{}).get('cwd','')
  out={'result':{'sessionId':'fake'}}
 else:
  print(json.dumps({'jsonrpc':'2.0','method':'client/session.update','params':{'update':{'message':{'content':[{'text':json.dumps({'process_cwd':os.getcwd(),'session_cwd':cwd})}]}}}}),flush=True)
  out={'result':{'stopReason':'turn_complete'}}
 out.update(jsonrpc='2.0',id=r['id'])
 print(json.dumps(out),flush=True)
"#;
    let context = crate::agent::provider::ExecutionContext {
        cwd: dir.clone(),
        session_id: "parent".into(),
    };
    let reply = crate::agent::provider::with_execution_context(Some(context), async {
        let mut client = connect("python3", &["-c".into(), code.into()])
            .await
            .unwrap();
        client.prompt("hello").await.unwrap()
    })
    .await;
    let result: serde_json::Value = serde_json::from_str(&reply).unwrap();
    assert_eq!(
        std::path::PathBuf::from(result["process_cwd"].as_str().unwrap())
            .canonicalize()
            .unwrap(),
        dir.canonicalize().unwrap()
    );
    std::fs::remove_dir_all(&dir).unwrap();
    assert_eq!(
        std::path::PathBuf::from(result["session_cwd"].as_str().unwrap()),
        dir
    );
}
