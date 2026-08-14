//! OpenAI-compatible chat client (works against vLLM, Ollama, llama.cpp
//! `/v1`, moon-bridge, cc-switch proxy, and every OpenAI-compatible
//! endpoint).

use async_trait::async_trait;
use futures_util::Stream;
use serde_json::{Value, json};

use super::provider::{
    ChatChunk, ChatComplete, ChatMessage, ChatRequest, ChatStream, LlmProvider, TokenUsage,
    ToolCall,
};

/// Configuration for an OpenAI-compatible provider.
#[derive(Debug, Clone)]
pub struct OpenAiConfig {
    pub base_url: String,
    pub api_key: String,
}

/// Error from a single endpoint attempt. `Fatal` (4xx client errors)
/// must not be retried — the request itself is wrong. `Transient`
/// (network failure, 5xx, 429) is safe to try on a fallback endpoint.
#[derive(Debug)]
pub enum PostError {
    Fatal(String),
    Transient(String),
}

impl std::fmt::Display for PostError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            PostError::Fatal(s) | PostError::Transient(s) => write!(f, "{s}"),
        }
    }
}

#[derive(Debug, Clone)]
pub struct OpenAiProvider {
    /// Endpoint chain: index 0 is primary, the rest are fallbacks tried
    /// in order on transient failures (network error / 5xx / 429).
    endpoints: Vec<Endpoint>,
}

#[derive(Debug, Clone)]
struct Endpoint {
    config: OpenAiConfig,
    client: reqwest::Client,
}

impl OpenAiProvider {
    /// Build a provider with a chain of fallback endpoints. The primary
    /// is tried first; on transient failure each fallback is tried in
    /// order with a small backoff. Pass `vec![]` for a single endpoint.
    pub fn with_fallbacks(
        config: OpenAiConfig,
        fallbacks: Vec<OpenAiConfig>,
    ) -> anyhow::Result<Self> {
        let mut endpoints = vec![Endpoint {
            config: config.clone(),
            client: Self::build_client()?,
        }];
        for fb in fallbacks {
            endpoints.push(Endpoint {
                config: fb,
                client: Self::build_client()?,
            });
        }
        Ok(Self { endpoints })
    }

    fn build_client() -> anyhow::Result<reqwest::Client> {
        Ok(reqwest::Client::builder()
            .timeout(std::time::Duration::from_secs(600))
            // Never route requests through a proxy from the
            // environment: sigil's endpoints are local or direct, and
            // http_proxy would hijack loopback traffic (a proxy
            // answering 502 to 127.0.0.1 broke local endpoints and
            // the fallback tests).
            .no_proxy()
            .build()?)
    }

    /// Try the endpoint chain: primary, then each fallback on transient
    /// failure with backoff. Fatal (4xx) errors abort immediately.
    async fn post_with_fallback(&self, req: &ChatRequest) -> anyhow::Result<reqwest::Response> {
        let mut last: Option<String> = None;
        for (i, ep) in self.endpoints.iter().enumerate() {
            if i > 0 {
                // Small backoff before a fallback attempt.
                tokio::time::sleep(std::time::Duration::from_millis(300 * i as u64)).await;
                eprintln!(
                    "[sigil] primary endpoint failed — trying fallback {}",
                    ep.config.base_url
                );
            }
            match ep.post(req).await {
                Ok(resp) => return Ok(resp),
                Err(PostError::Fatal(e)) => return Err(anyhow::anyhow!(e)),
                Err(PostError::Transient(e)) => {
                    eprintln!("[sigil] endpoint {} failed: {e}", ep.config.base_url);
                    last = Some(e);
                }
            }
        }
        Err(anyhow::anyhow!(
            "all {} endpoint(s) failed: {}",
            self.endpoints.len(),
            last.unwrap_or_default()
        ))
    }
}

impl Endpoint {
    async fn post(&self, req: &ChatRequest) -> Result<reqwest::Response, PostError> {
        let url = format!(
            "{}/chat/completions",
            self.config.base_url.trim_end_matches('/')
        );
        let body = build_body(req);
        let resp = match self
            .client
            .post(&url)
            .bearer_auth(&self.config.api_key)
            .json(&body)
            .send()
            .await
        {
            Ok(r) => r,
            Err(e) => return Err(PostError::Transient(format!("{e}"))),
        };

        let status = resp.status();
        if !status.is_success() {
            let text = resp.text().await.unwrap_or_default();
            if status.is_client_error() {
                // 4xx: the request is malformed (bad schema, auth,
                // unknown model) — retrying another endpoint won't help.
                return Err(PostError::Fatal(format!("provider error {status}: {text}")));
            }
            // 5xx / 429: server-side trouble — safe to try a fallback.
            return Err(PostError::Transient(format!(
                "provider error {status}: {text}"
            )));
        }
        Ok(resp)
    }
}

#[async_trait]
impl LlmProvider for OpenAiProvider {
    fn name(&self) -> &str {
        "openai-compatible"
    }

    async fn chat_stream(&self, req: ChatRequest) -> anyhow::Result<ChatStream> {
        let resp = self.post_with_fallback(&req).await?;
        let bytes = resp.bytes().await?.to_vec();
        Ok(Box::new(parse_stream(bytes)))
    }

    async fn chat_complete(&self, req: ChatRequest) -> anyhow::Result<ChatComplete> {
        let mut req = req;
        req.stream = false;
        let resp = self.post_with_fallback(&req).await?;
        let v: Value = resp.json().await?;

        let text = v["choices"][0]["message"]["content"]
            .as_str()
            .unwrap_or("")
            .to_string();
        let tool_calls = parse_tool_calls(&v);
        Ok(ChatComplete {
            text,
            tool_calls,
            usage: parse_usage(&v),
        })
    }
}

/// Build the OpenAI chat-completions request body.
fn build_body(req: &ChatRequest) -> Value {
    let mut body = json!({
        "model": req.model,
        "messages": req.messages.iter().map(message_to_json).collect::<Vec<_>>(),
        "stream": req.stream,
    });
    if !req.tools.is_empty() {
        body["tools"] = json!(
            req.tools
                .iter()
                .map(|t| json!({
                    "type": "function",
                    "function": {
                        "name": t.name,
                        "description": t.description,
                        "parameters": t.parameters,
                    }
                }))
                .collect::<Vec<_>>()
        );
    }
    if let Some(t) = req.temperature {
        body["temperature"] = json!(t);
    }
    body
}

/// Serialize one message, including native tool-call fields when present.
fn message_to_json(m: &ChatMessage) -> Value {
    let mut v = json!({ "role": m.role });
    if let Some(calls) = &m.tool_calls {
        v["tool_calls"] = json!(
            calls
                .iter()
                .map(|c| json!({
                    "id": c.id,
                    "type": "function",
                    "function": {
                        "name": c.name,
                        "arguments": c.arguments.to_string(),
                    }
                }))
                .collect::<Vec<_>>()
        );
        // When tool_calls is present, content may be empty/absent.
        if !m.content.is_empty() {
            v["content"] = Value::String(m.content.clone());
        }
    } else {
        v["content"] = Value::String(m.content.clone());
    }
    if let Some(id) = &m.tool_call_id {
        v["tool_call_id"] = Value::String(id.clone());
    }
    v
}

/// Extract an OpenAI-compatible `usage` block (absent → None).
pub fn parse_usage(v: &Value) -> Option<TokenUsage> {
    let usage = v.get("usage")?;
    let prompt_tokens = usage.get("prompt_tokens")?.as_u64()?;
    let completion_tokens = usage.get("completion_tokens")?.as_u64()?;
    // Cache-hit tokens: vLLM exposes `prompt_cache_hit_tokens`;
    // OpenAI exposes `prompt_tokens_details.cached_tokens`.
    let prompt_cache_hit_tokens = usage
        .get("prompt_cache_hit_tokens")
        .and_then(|n| n.as_u64())
        .or_else(|| {
            usage
                .get("prompt_tokens_details")
                .and_then(|d| d.get("cached_tokens"))
                .and_then(|n| n.as_u64())
        })
        .unwrap_or(0);
    Some(TokenUsage {
        prompt_tokens,
        completion_tokens,
        prompt_cache_hit_tokens,
    })
}

/// Chunk stream parser. Handles both plain SSE lines and wrapped
/// `data: {...}` frames. Robust to trailing whitespace and `[DONE]`.
pub(crate) fn parse_stream(bytes: Vec<u8>) -> impl Stream<Item = anyhow::Result<ChatChunk>> {
    let text = String::from_utf8_lossy(&bytes).to_string();
    // Own the text inside the closure; iterate in place.
    futures_util::stream::iter(
        text.lines()
            .map(str::to_string)
            .collect::<Vec<String>>()
            .into_iter()
            .filter_map(move |line| {
                let line = line.trim();
                if line.is_empty() {
                    return None;
                }
                let payload = line.strip_prefix("data:").unwrap_or(line).trim();
                if payload == "[DONE]" {
                    return Some(Ok(ChatChunk {
                        delta: String::new(),
                        thinking: false,
                        done: true,
                        tool_calls_requested: false,
                        usage: None,
                    }));
                }
                let parsed: Result<Value, _> = serde_json::from_str(payload);
                match parsed {
                    Ok(v) => {
                        // Some providers (incl. the sigil local test
                        // endpoint) attach `usage` to the final chunk.
                        let usage = parse_usage(&v);
                        let delta = v["choices"][0]["delta"]["content"]
                            .as_str()
                            .unwrap_or("")
                            .to_string();
                        let reasoning = v["choices"][0]["delta"]["reasoning_content"]
                            .as_str()
                            .or_else(|| v["choices"][0]["delta"]["reasoning"].as_str())
                            .unwrap_or("")
                            .to_string();
                        let finish = v["choices"][0]["finish_reason"].as_str();
                        let tool_calls_requested = finish == Some("tool_calls");
                        Some(Ok(ChatChunk {
                            delta,
                            thinking: !reasoning.is_empty(),
                            done: finish == Some("stop") || tool_calls_requested,
                            tool_calls_requested,
                            usage,
                        }))
                    }
                    Err(e) => Some(Err(anyhow::anyhow!("bad chunk: {e}"))),
                }
            }),
    )
}

/// Parse an OpenAI function-calling response into tool calls.
#[allow(dead_code)]
pub fn parse_tool_calls(v: &Value) -> Vec<ToolCall> {
    let mut out = Vec::new();
    if let Some(calls) = v["choices"][0]["message"]["tool_calls"].as_array() {
        for c in calls {
            let id = c["id"].as_str().unwrap_or("").to_string();
            let name = c["function"]["name"].as_str().unwrap_or("").to_string();
            let args: Value =
                serde_json::from_str(c["function"]["arguments"].as_str().unwrap_or("{}"))
                    .unwrap_or(Value::Null);
            out.push(ToolCall {
                id,
                name,
                arguments: args,
            });
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use futures_util::FutureExt;
    use std::io::{Read, Write};

    /// Spin up a tiny HTTP server that answers every POST with the given
    /// status + body. Returns the bound port.
    fn fake_endpoint(status: u16, body: &'static str) -> u16 {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        std::thread::spawn(move || {
            for _ in 0..8 {
                let (mut sock, _) = match listener.accept() {
                    Ok(x) => x,
                    Err(_) => break,
                };
                // Drain the request headers, then reply.
                let mut buf = [0u8; 4096];
                let _ = sock.read(&mut buf);
                let resp = format!(
                    "HTTP/1.1 {status} X\r\nContent-Length: {}\r\n\r\n{}",
                    body.len(),
                    body
                );
                let _ = sock.write_all(resp.as_bytes());
            }
        });
        port
    }

    fn chat_request() -> ChatRequest {
        ChatRequest {
            model: "test".into(),
            messages: vec![ChatMessage::user("hi")],
            tools: vec![],
            stream: false,
            temperature: None,
        }
    }

    fn cfg(port: u16) -> OpenAiConfig {
        OpenAiConfig {
            base_url: format!("http://127.0.0.1:{port}/v1"),
            api_key: "x".into(),
        }
    }

    #[tokio::test]
    async fn falls_back_to_second_endpoint_when_primary_is_down() {
        // Primary: closed port (connection refused = transient).
        let dead = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let dead_port = dead.local_addr().unwrap().port();
        drop(dead);
        // Fallback: real endpoint answering 200.
        let ok_body = r#"{"choices":[{"message":{"content":"fallback-works"}}]}"#;
        let ok_port = fake_endpoint(200, ok_body);

        let provider = OpenAiProvider::with_fallbacks(cfg(dead_port), vec![cfg(ok_port)]).unwrap();
        let complete = provider.chat_complete(chat_request()).await.unwrap();
        assert_eq!(complete.text, "fallback-works");
    }

    #[tokio::test]
    async fn falls_back_on_server_error_but_not_on_client_error() {
        // Primary answers 500 (transient) → falls back to the OK endpoint.
        let p500 = fake_endpoint(500, "boom");
        let ok_body = r#"{"choices":[{"message":{"content":"recovered"}}]}"#;
        let ok_port = fake_endpoint(200, ok_body);
        let provider = OpenAiProvider::with_fallbacks(cfg(p500), vec![cfg(ok_port)]).unwrap();
        let complete = provider.chat_complete(chat_request()).await.unwrap();
        assert_eq!(complete.text, "recovered");

        // Primary answers 400 (fatal) → must NOT touch the fallback.
        let p400 = fake_endpoint(400, "bad request");
        let ok_body2 = r#"{"choices":[{"message":{"content":"should-not-reach"}}]}"#;
        let ok2 = fake_endpoint(200, ok_body2);
        let provider2 = OpenAiProvider::with_fallbacks(cfg(p400), vec![cfg(ok2)]).unwrap();
        let err = provider2.chat_complete(chat_request()).await.unwrap_err();
        let msg = format!("{err}");
        assert!(msg.contains("400"), "expected 400 error, got: {msg}");
    }

    #[tokio::test]
    async fn all_endpoints_down_reports_each_failure() {
        let dead = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let dead_port = dead.local_addr().unwrap().port();
        drop(dead);
        let dead2 = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let dead2_port = dead2.local_addr().unwrap().port();
        drop(dead2);

        let provider =
            OpenAiProvider::with_fallbacks(cfg(dead_port), vec![cfg(dead2_port)]).unwrap();
        let err = provider.chat_complete(chat_request()).await.unwrap_err();
        let msg = format!("{err}");
        assert!(
            msg.contains("all 2 endpoint(s) failed"),
            "expected chain exhaustion message, got: {msg}"
        );
    }

    #[test]
    fn usage_parsed_from_response_block() {
        let v = serde_json::json!({
            "usage": { "prompt_tokens": 12, "completion_tokens": 34, "total_tokens": 46 }
        });
        let u = parse_usage(&v).expect("usage block present");
        assert_eq!(u.prompt_tokens, 12);
        assert_eq!(u.completion_tokens, 34);
        assert_eq!(u.total(), 46);
        // Absent or partial usage blocks yield None (no crash).
        assert!(parse_usage(&serde_json::json!({})).is_none());
        assert!(parse_usage(&serde_json::json!({ "usage": {} })).is_none());
        assert!(parse_usage(&serde_json::json!({ "usage": { "prompt_tokens": 1 } })).is_none());
    }

    #[test]
    fn usage_parses_cache_hit_tokens_both_formats() {
        // vLLM: `prompt_cache_hit_tokens` at usage top level.
        let vllm = serde_json::json!({
            "usage": { "prompt_tokens": 100, "completion_tokens": 5, "prompt_cache_hit_tokens": 80 }
        });
        let u = parse_usage(&vllm).unwrap();
        assert_eq!(u.prompt_cache_hit_tokens, 80);

        // OpenAI: `prompt_tokens_details.cached_tokens`.
        let oai = serde_json::json!({
            "usage": {
                "prompt_tokens": 100,
                "completion_tokens": 5,
                "prompt_tokens_details": { "cached_tokens": 60 }
            }
        });
        let u2 = parse_usage(&oai).unwrap();
        assert_eq!(u2.prompt_cache_hit_tokens, 60);

        // Neither → 0, never None.
        let none = serde_json::json!({ "usage": { "prompt_tokens": 1, "completion_tokens": 1 } });
        assert_eq!(parse_usage(&none).unwrap().prompt_cache_hit_tokens, 0);
    }

    #[test]
    fn stream_chunk_carries_usage_when_provider_sends_it() {
        let sse = concat!(
            "data: {\"choices\":[{\"delta\":{\"content\":\"hi\"}}]}\n",
            "data: {\"choices\":[{\"delta\":{\"content\":\"\"},\"finish_reason\":\"stop\"}],",
            "\"usage\":{\"prompt_tokens\":7,\"completion_tokens\":3}}\n",
            "data: [DONE]\n"
        );
        let chunks: Vec<ChatChunk> = futures_util::StreamExt::collect::<
            Vec<anyhow::Result<ChatChunk>>,
        >(parse_stream(sse.as_bytes().to_vec()))
        .now_or_never()
        .unwrap()
        .into_iter()
        .map(|c| c.unwrap())
        .collect();
        let with_usage = chunks
            .iter()
            .find(|c| c.usage.is_some())
            .expect("the finish_reason chunk carries usage");
        let u = with_usage.usage.expect("final chunk carries usage");
        assert_eq!(u.prompt_tokens, 7);
        assert_eq!(u.completion_tokens, 3);
    }
}
