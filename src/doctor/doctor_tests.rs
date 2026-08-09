//! `sigil doctor` tests: probe behavior and check-level math.

use std::io::{Read, Write};

use crate::doctor::{self, probe_endpoint};

#[tokio::test]
async fn probe_endpoint_parses_model_count() {
    // Spin up a tiny local HTTP server that returns a models list.
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap();
    std::thread::spawn(move || {
        for _ in 0..2 {
            let (mut sock, _) = listener.accept().unwrap();
            let mut buf = [0u8; 2048];
            let _ = sock.read(&mut buf).unwrap();
            let body = br#"{"object":"list","data":[{"id":"a"},{"id":"b"},{"id":"c"}]}"#;
            let resp = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n",
                body.len()
            );
            let _ = sock.write_all(resp.as_bytes());
            let _ = sock.write_all(body);
        }
    });

    let url = format!("http://{addr}/v1/models");
    let n = probe_endpoint(&url, "").await.unwrap();
    assert_eq!(n, Some(3));
}

#[tokio::test]
async fn probe_endpoint_errors_on_connection_refused() {
    // Grab a port and drop the listener — nothing is listening.
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap();
    drop(listener);

    let url = format!("http://{addr}/v1/models");
    assert!(probe_endpoint(&url, "").await.is_err());
}

#[test]
fn level_ordering_ok_lt_warn_lt_fail() {
    use doctor::Level;
    assert!(Level::Ok < Level::Warn);
    assert!(Level::Warn < Level::Fail);
}
