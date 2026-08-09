//! OpenAI-compatible chat client (works against vLLM, Ollama, llama.cpp
//! `/v1`, moon-bridge, cc-switch proxy, and every OpenAI-compatible
//! endpoint).

use async_trait::async_trait;
use futures_util::Stream;
use serde_json::{Value, json};

use super::provider::{
    ChatChunk, ChatComplete, ChatMessage, ChatRequest, ChatStream, LlmProvider, ToolCall,
};

/// Configuration for an OpenAI-compatible provider.
#[derive(Debug, Clone)]
pub struct OpenAiConfig {
    pub base_url: String,
    pub api_key: String,
}

#[derive(Debug, Clone)]
pub struct OpenAiProvider {
    config: OpenAiConfig,
    client: reqwest::Client,
}

impl OpenAiProvider {
    pub fn new(config: OpenAiConfig) -> anyhow::Result<Self> {
        let client = reqwest::Client::builder()
            .timeout(std::time::Duration::from_secs(600))
            .build()?;
        Ok(Self { config, client })
    }

    async fn post(&self, req: &ChatRequest) -> anyhow::Result<reqwest::Response> {
        let url = format!(
            "{}/chat/completions",
            self.config.base_url.trim_end_matches('/')
        );
        let body = build_body(req);
        let resp = self
            .client
            .post(&url)
            .bearer_auth(&self.config.api_key)
            .json(&body)
            .send()
            .await?;

        let status = resp.status();
        if !status.is_success() {
            let text = resp.text().await.unwrap_or_default();
            anyhow::bail!("provider error {status}: {text}");
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
        let resp = self.post(&req).await?;
        let bytes = resp.bytes().await?.to_vec();
        Ok(Box::new(parse_stream(bytes)))
    }

    async fn chat_complete(&self, req: ChatRequest) -> anyhow::Result<ChatComplete> {
        let mut req = req;
        req.stream = false;
        let resp = self.post(&req).await?;
        let v: Value = resp.json().await?;

        let text = v["choices"][0]["message"]["content"]
            .as_str()
            .unwrap_or("")
            .to_string();
        let tool_calls = parse_tool_calls(&v);
        Ok(ChatComplete { text, tool_calls })
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
                    }));
                }
                let parsed: Result<Value, _> = serde_json::from_str(payload);
                match parsed {
                    Ok(v) => {
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
