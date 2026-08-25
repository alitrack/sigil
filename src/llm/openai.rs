//! OpenAI-compatible chat client (works against vLLM, Ollama, llama.cpp
//! `/v1`, moon-bridge, cc-switch proxy, and every OpenAI-compatible
//! endpoint).

use async_trait::async_trait;
use futures_util::Stream;
use serde_json::{Value, json};
use std::pin::Pin;

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
    endpoints: Vec<OpenAiConfig>,
    /// One shared client for the whole chain: connection pools are
    /// reused across hosts, and N per-endpoint clients would double
    /// connection pools and fds.
    client: reqwest::Client,
    /// Bound on waiting for the response HEADERS (status line + headers),
    /// i.e. the `send().await` phase before any body bytes arrive. This is
    /// the phase that hangs indefinitely when a load-balanced endpoint
    /// (SGLang/vLLM with a request queue) drops a queued request without
    /// closing the socket: the TCP connection stays ESTAB/half-open, no
    /// status line is ever sent, and — without this bound — the caller
    /// waits for the full client `.timeout()` (3600s), which reads as an
    /// infinite hang. A head-timeout error is `Transient`, so it flows into
    /// the normal fallback/backoff chain instead of pinning the agent.
    head_timeout: std::time::Duration,
}

impl OpenAiProvider {
    /// Build a provider with a chain of fallback endpoints. The primary
    /// is tried first; on transient failure each fallback is tried in
    /// order with a small backoff. Pass `vec![]` for a single endpoint.
    pub fn with_fallbacks(
        config: OpenAiConfig,
        fallbacks: Vec<OpenAiConfig>,
    ) -> anyhow::Result<Self> {
        validate_base_url(&config.base_url)?;
        for fb in &fallbacks {
            validate_base_url(&fb.base_url)?;
        }
        let mut endpoints = vec![config];
        endpoints.extend(fallbacks);
        Ok(Self {
            endpoints,
            client: Self::build_client()?,
            head_timeout: Self::default_head_timeout(),
        })
    }

    /// Default bound on waiting for response headers: 300s. A local
    /// endpoint under queue pressure should answer or drop well inside
    /// that; a healthy streaming completion can wait arbitrarily long for
    /// BODY bytes (that phase is covered by the per-chunk idle timeout),
    /// so this only caps the pre-body wait.
    pub fn default_head_timeout() -> std::time::Duration {
        std::time::Duration::from_secs(300)
    }

    /// Override the head-wait bound (tests use short values so a
    /// "server accepts the connection but never answers" scenario fails
    /// in milliseconds instead of 300s).
    pub fn with_head_timeout(mut self, d: std::time::Duration) -> Self {
        self.head_timeout = d;
        self
    }

    fn build_client() -> anyhow::Result<reqwest::Client> {
        Ok(reqwest::Client::builder()
            // 3600s: long-context review prompts can exceed 600s when the
            // endpoint is busy (queued behind other requests) or the model
            // streams a long answer. 600s was too tight for 48K-token prompts.
            .timeout(std::time::Duration::from_secs(3600))
            // A stuck endpoint must fail fast on connect, not hang for the
            // full 3600s total timeout.
            .connect_timeout(std::time::Duration::from_secs(10))
            // Never route requests through a proxy from the
            // environment: sigil's endpoints are local or direct, and
            // http_proxy would hijack loopback traffic (a proxy
            // answering 502 to 127.0.0.1 broke local endpoints and
            // the fallback tests).
            .no_proxy()
            .build()?)
    }

    /// Try the endpoint chain: primary, then each fallback on transient
    /// failure with exponential backoff. Fatal (4xx, except 429) errors
    /// abort immediately.
    async fn post_with_fallback(&self, req: &ChatRequest) -> anyhow::Result<reqwest::Response> {
        let mut last: Option<String> = None;
        for (i, config) in self.endpoints.iter().enumerate() {
            if i > 0 {
                // Exponential backoff with deterministic jitter before a
                // fallback attempt: 200/400/800/1600/3200ms + jitter. The
                // fixed 300ms*i scheme re-stormed busy endpoints.
                let exp = 200u64.saturating_mul(1u64 << (i - 1).min(4));
                let jitter = (i as u64 * 37) % 150;
                tokio::time::sleep(std::time::Duration::from_millis(exp + jitter)).await;
                eprintln!(
                    "[sigil] primary endpoint failed — trying fallback {}",
                    config.base_url
                );
            }
            match self.post(config, req).await {
                Ok(resp) => return Ok(resp),
                Err(PostError::Fatal(e)) => return Err(anyhow::anyhow!(e)),
                Err(PostError::Transient(e)) => {
                    eprintln!("[sigil] endpoint {} failed: {e}", config.base_url);
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

    async fn post(
        &self,
        config: &OpenAiConfig,
        req: &ChatRequest,
    ) -> Result<reqwest::Response, PostError> {
        let url = format!("{}/chat/completions", config.base_url.trim_end_matches('/'));
        let body = build_body(req);
        // Bound the wait for response HEADERS. `send()` returns when the
        // status line + headers arrive; for a load-balanced local endpoint
        // whose queue drops the request without closing the socket, that
        // can be arbitrarily long — and the client-level `.timeout(3600s)`
        // only makes it *worse* (a 1-hour hang). Time it out at
        // `head_timeout` and report Transient so the fallback chain kicks
        // in. The body phase is NOT bounded here: streaming completions
        // legitimately take minutes, and per-chunk idle timeout covers stalls.
        let send_fut = self
            .client
            .post(&url)
            .bearer_auth(&config.api_key)
            .json(&body)
            .send();
        let resp = match tokio::time::timeout(self.head_timeout, send_fut).await {
            Ok(Ok(r)) => r,
            Ok(Err(e)) => return Err(PostError::Transient(format!("{e}"))),
            Err(_) => {
                return Err(PostError::Transient(format!(
                    "timed out after {:?} waiting for response headers from {} (endpoint may have dropped the request without closing the connection)",
                    self.head_timeout, config.base_url
                )))
            }
        };

        let status = resp.status();
        if !status.is_success() {
            // Bound the error body: some gateways echo the entire request
            // (tens of KB) back on 4xx/5xx, buffered per failed attempt ×
            // per fallback.
            let text = bounded_error_text(resp).await;
            if status.as_u16() == 429 {
                // Rate limit: the endpoint is overloaded, not the request
                // malformed — safe to try a fallback.
                return Err(PostError::Transient(format!(
                    "provider error {status}: {text}"
                )));
            }
            if status.is_client_error() {
                // 4xx: the request is malformed (bad schema, auth,
                // unknown model) — retrying another endpoint won't help.
                return Err(PostError::Fatal(format!("provider error {status}: {text}")));
            }
            // 5xx: server-side trouble — safe to try a fallback.
            return Err(PostError::Transient(format!(
                "provider error {status}: {text}"
            )));
        }
        Ok(resp)
    }
}

/// Read a response body up to 16 KiB — enough for a diagnostic message,
/// not enough to buffer a full request echo.
async fn bounded_error_text(resp: reqwest::Response) -> String {
    const CAP: usize = 16 * 1024;
    match resp.bytes().await {
        Ok(bytes) => {
            let head = &bytes[..bytes.len().min(CAP)];
            String::from_utf8_lossy(head).to_string()
        }
        Err(_) => String::new(),
    }
}

/// Reject malformed base URLs at construction time so a typo fails fast
/// instead of walking the whole fallback chain and reporting a confusing
/// "all endpoints failed" error.
fn validate_base_url(u: &str) -> anyhow::Result<()> {
    let parsed =
        reqwest::Url::parse(u).map_err(|e| anyhow::anyhow!("invalid base_url {u:?}: {e}"))?;
    if parsed.scheme() != "http" && parsed.scheme() != "https" {
        anyhow::bail!("base_url must be http(s), got {:?}: {u:?}", parsed.scheme());
    }
    if parsed.host_str().is_none() {
        anyhow::bail!("base_url must include a host: {u:?}");
    }
    Ok(())
}

#[async_trait]
impl LlmProvider for OpenAiProvider {
    fn name(&self) -> &str {
        "openai-compatible"
    }

    async fn chat_stream(&self, req: ChatRequest) -> anyhow::Result<ChatStream> {
        let resp = self.post_with_fallback(&req).await?;
        // Incremental body streaming: `resp.bytes()` buffered the whole
        // response before emitting anything — first-token latency equaled
        // the full generation time, and a mid-stream disconnect discarded
        // every token generated so far.
        let body = resp.bytes_stream();
        // Per-chunk idle timeout: a stalled endpoint must not pin the
        // caller for the whole 3600s total timeout after emitting one byte.
        let idle = std::time::Duration::from_secs(120);
        Ok(Box::new(sse_line_stream(with_idle_timeout(body, idle))))
    }

    async fn chat_complete(&self, req: ChatRequest) -> anyhow::Result<ChatComplete> {
        let mut req = req;
        req.stream = false;
        let resp = self.post_with_fallback(&req).await?;
        let v: Value = resp.json().await?;

        // A 200 with no choices/message is a provider malfunction, not an
        // empty answer — surface it instead of silently returning "".
        let msg = v["choices"][0]["message"].as_object().ok_or_else(|| {
            anyhow::anyhow!(
                "provider returned 200 without choices[0].message: {}",
                serde_json::to_string(&v)
                    .unwrap_or_default()
                    .chars()
                    .take(200)
                    .collect::<String>()
            )
        })?;
        let text = msg
            .get("content")
            .and_then(Value::as_str)
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
    // Normalize the message list into JSON values up front.
    let mut msgs: Vec<Value> = req.messages.iter().map(message_to_json).collect();

    // Strict chat-template backends (SGLang, some vLLM builds) reject a
    // request that does not begin with EXACTLY ONE system message — a direct
    // probe of the local 8014 SGLang gateway confirms: [system, user] → 200,
    // but [system, system, user] and [system, user, …, system(mid-list)] →
    // 400 "System message must be at the beginning." (NInfer is lenient, so
    // this only bites on SGLang.) The agent loop appends System messages at
    // the END of the list on several per-iteration paths — the
    // PLANNING_CHECKPOINT prompt and on_turn_start hook text — and compaction
    // can leave a system summary mid-list, so an evolving session produces
    // several system blocks: [base prompt, user, …, goals, checkpoint, hook].
    //
    // Merge every system message's content into a SINGLE leading system
    // message (base prompt first, others appended in order), then keep the
    // dialog (user/assistant/tool) in its original order. This satisfies the
    // strict template while preserving all the system context the model
    // needs; a history with a single leading system and no others passes
    // through unchanged (the merge yields that same one message).
    let mut sys_parts: Vec<String> = Vec::new();
    let mut rest: Vec<Value> = Vec::new();
    for m in msgs {
        if m.get("role").and_then(Value::as_str) == Some("system") {
            let text = m
                .get("content")
                .and_then(Value::as_str)
                .map(str::to_string)
                .unwrap_or_else(|| m.get("content").cloned().map(|v| v.to_string()).unwrap_or_default());
            sys_parts.push(text);
        } else {
            rest.push(m);
        }
    }
    let mut msgs: Vec<Value> = Vec::with_capacity(rest.len() + 1);
    if !sys_parts.is_empty() {
        let merged = if sys_parts.len() == 1 {
            sys_parts.pop().unwrap()
        } else {
            sys_parts.join("\n\n")
        };
        msgs.push(json!({ "role": "system", "content": merged }));
    }
    msgs.extend(rest);

    // Some OpenAI-compatible backends (SGLang, NInfer, vLLM with strict
    // chat templates) reject a request that contains NO user message
    // ("No user query found in messages"). That shape is reachable after
    // history compression trims the original user prompt, leaving
    // [system, system(summary), assistant(tool_calls), tool, ...]. Both
    // local gateways 400 on it, so normalize here instead of per-backend:
    // synthesize a continuation user turn right after the leading
    // system messages. It is invisible to the model's task state (the
    // real prompt is in the compressed summary) and keeps every
    // backend happy.
    let has_user = msgs
        .iter()
        .any(|m| m.get("role").and_then(Value::as_str) == Some("user"));
    if !has_user {
        let pos = msgs
            .iter()
            .take_while(|m| m.get("role").and_then(Value::as_str) == Some("system"))
            .count();
        msgs.insert(
            pos,
            json!({ "role": "user", "content": "(continuing — proceed with the current task)" }),
        );
    }

    let mut body = json!({
        "model": req.model,
        "messages": msgs,
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

/// Chunk stream parser (whole-buffer variant, used by tests). Handles
/// both plain SSE lines and wrapped `data: {...}` frames. Robust to
/// trailing whitespace and `[DONE]`. The live path uses
/// `sse_line_stream` over the incremental body stream instead.
#[cfg(test)]
pub(crate) fn parse_stream(bytes: Vec<u8>) -> impl Stream<Item = anyhow::Result<ChatChunk>> {
    let text = String::from_utf8_lossy(&bytes).to_string();
    // Own the text inside the closure; iterate in place.
    futures_util::stream::iter(
        text.lines()
            .map(str::to_string)
            .collect::<Vec<String>>()
            .into_iter()
            .filter_map(move |line| parse_sse_line(&line)),
    )
}

/// Wrap a byte stream so each item must arrive within `idle`; a stall
/// ends the stream with an error instead of hanging the caller.
///
/// Hand-rolled `poll_next`: `stream::unfold` needs its closure future to
/// be `Unpin`, which async blocks are not (futures-util 0.3.33).
struct IdleTimeout<S> {
    inner: S,
    idle: std::time::Duration,
    sleep: Option<Pin<Box<tokio::time::Sleep>>>,
}

fn with_idle_timeout<S, T, E>(
    stream: S,
    idle: std::time::Duration,
) -> impl Stream<Item = anyhow::Result<T>>
where
    S: Stream<Item = Result<T, E>> + Unpin,
    E: std::fmt::Display,
{
    IdleTimeout {
        inner: stream,
        idle,
        sleep: None,
    }
}

impl<S, T, E> Stream for IdleTimeout<S>
where
    S: Stream<Item = Result<T, E>> + Unpin,
    E: std::fmt::Display,
{
    type Item = anyhow::Result<T>;

    fn poll_next(
        self: Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<Option<Self::Item>> {
        use std::task::Poll;
        let this = self.get_mut();
        // Inner data takes priority: poll it first, and only consult
        // the idle timer when the inner stream is silent. Polling the
        // timer first would swallow a ready item behind a not-yet-fired
        // sleep and stall the stream until the timer expires.
        match Pin::new(&mut this.inner).poll_next(cx) {
            Poll::Ready(Some(Ok(item))) => {
                this.sleep = None;
                Poll::Ready(Some(Ok(item)))
            }
            Poll::Ready(Some(Err(e))) => {
                this.sleep = None;
                Poll::Ready(Some(Err(anyhow::anyhow!("{e}"))))
            }
            Poll::Ready(None) => Poll::Ready(None),
            Poll::Pending => {
                if this.sleep.is_none() {
                    this.sleep = Some(Box::pin(tokio::time::sleep(this.idle)));
                }
                match this.sleep.as_mut().expect("armed").as_mut().poll(cx) {
                    Poll::Ready(()) => {
                        this.sleep = None;
                        Poll::Ready(Some(Err(anyhow::anyhow!(
                            "stream idle timeout after {:?}",
                            this.idle
                        ))))
                    }
                    Poll::Pending => Poll::Pending,
                }
            }
        }
    }
}

/// Incremental SSE parser: accumulates bytes into lines, feeds complete
/// lines to `parse_sse_line`, and drops a trailing partial line at EOF
/// (a torn frame left behind by a mid-stream disconnect).
struct SseLines<S> {
    inner: S,
    pending: String,
    queued: std::collections::VecDeque<anyhow::Result<ChatChunk>>,
    /// Saw a terminal chunk (`[DONE]` or `finish_reason`)? When the body
    /// stream reaches EOF WITHOUT one, the provider dropped the connection
    /// mid-completion — a truncated (often empty) answer. Reporting that
    /// as a clean `None` let the agent treat the partial output as the
    /// final answer ("silent empty output"); surface it as an error
    /// instead.
    saw_terminal: bool,
    /// Inner stream reached EOF. Once set, every subsequent poll returns
    /// `None` — including after we've already emitted the truncation
    /// error. Without this, a `StreamExt::collect()` loop (which keeps
    /// polling past an Err item, since our Item IS `Result<_,_>`) would
    /// re-observe the EOF and re-emit the same error forever → hang.
    finished: bool,
}

fn sse_line_stream<S, T>(stream: S) -> impl Stream<Item = anyhow::Result<ChatChunk>>
where
    S: Stream<Item = anyhow::Result<T>> + Unpin,
    T: AsRef<[u8]>,
{
    SseLines {
        inner: stream,
        pending: String::new(),
        queued: std::collections::VecDeque::new(),
        saw_terminal: false,
        finished: false,
    }
}

impl<S, T> Stream for SseLines<S>
where
    S: Stream<Item = anyhow::Result<T>> + Unpin,
    T: AsRef<[u8]>,
{
    type Item = anyhow::Result<ChatChunk>;

    fn poll_next(
        self: Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<Option<Self::Item>> {
        use std::task::Poll;
        let this = self.get_mut();
        // EOF already reached: terminate. We must return `None` here rather
        // than re-enter the inner stream, otherwise a `collect()` consumer
        // that keeps polling past our truncation error would loop forever
        // re-emitting the same error (our Item is `Result`, so an Err is a
        // value, not a stream terminator).
        if this.finished {
            return Poll::Ready(None);
        }
        if let Some(item) = this.queued.pop_front() {
            if item.as_ref().ok().map(|c| c.done).unwrap_or(false) {
                this.saw_terminal = true;
            }
            return Poll::Ready(Some(item));
        }
        loop {
            match Pin::new(&mut this.inner).poll_next(cx) {
                Poll::Ready(Some(Ok(chunk))) => {
                    this.pending
                        .push_str(&String::from_utf8_lossy(chunk.as_ref()));
                    let mut lines: Vec<String> =
                        this.pending.split('\n').map(str::to_string).collect();
                    // The last element is the unfinished line — keep it
                    // pending for the next chunk.
                    this.pending = lines.pop().unwrap_or_default();
                    this.queued
                        .extend(lines.into_iter().filter_map(|l| parse_sse_line(&l)));
                    if let Some(item) = this.queued.pop_front() {
                        if item.as_ref().ok().map(|c| c.done).unwrap_or(false) {
                            this.saw_terminal = true;
                        }
                        return Poll::Ready(Some(item));
                    }
                    // Chunk contained no complete line yet — keep polling.
                    continue;
                }
                Poll::Ready(Some(Err(e))) => return Poll::Ready(Some(Err(e))),
                Poll::Ready(None) => {
                    // Body stream ended. Mark finished so subsequent polls
                    // return None (see the guard at the top of poll_next).
                    this.finished = true;
                    // A well-formed SSE completion terminates with [DONE]
                    // (or a finish_reason chunk); EOF without either means
                    // the connection was dropped mid-stream and anything
                    // already delivered is partial.
                    if !this.saw_terminal && !this.pending.is_empty() {
                        // Pending bytes that never became a full line —
                        // a torn final frame.
                        return Poll::Ready(Some(Err(anyhow::anyhow!(
                            "provider closed the stream mid-completion (torn final frame: {:?})",
                            &this.pending
                        ))));
                    }
                    if !this.saw_terminal {
                        return Poll::Ready(Some(Err(anyhow::anyhow!(
                            "provider closed the stream before sending a terminal chunk (no [DONE]/finish_reason) — response truncated or dropped"
                        ))));
                    }
                    return Poll::Ready(None);
                }
                Poll::Pending => return Poll::Pending,
            }
        }
    }
}

/// Parse one SSE line into an optional chunk. Non-`data:` lines
/// (keep-alive comments like `: ping`, `event:`/`id:` fields, blank
/// lines) are skipped; a `data:` line whose JSON fails to parse is
/// skipped too — a single malformed frame must not kill the stream.
///
/// NOTE: `delta.tool_calls` is intentionally NOT accumulated here.
/// sigil's tool-call flow detects `finish_reason == "tool_calls"` on the
/// stream and replays the request non-streaming (`chat_complete`) to get
/// the complete call — streaming deltas are unreliable for that, and the
/// replay is what produces the real `ToolCall`s.
fn parse_sse_line(line: &str) -> Option<anyhow::Result<ChatChunk>> {
    let line = line.trim();
    if line.is_empty() {
        return None;
    }
    let payload = line.strip_prefix("data:")?.trim();
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
        // Malformed frame: skip it rather than killing the whole stream.
        // A torn final frame after a disconnect is common and harmless.
        Err(_) => None,
    }
}

/// Parse an OpenAI function-calling response into tool calls.
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
                // Drain the full request (headers + Content-Length body)
                // before replying. Reading only one 4 KiB chunk would
                // drop the tail of large request bodies and could cause a
                // spurious reset on the client side.
                let mut buf = Vec::new();
                let mut tmp = [0u8; 2048];
                loop {
                    match sock.read(&mut tmp) {
                        Ok(0) => break,
                        Ok(n) => {
                            buf.extend_from_slice(&tmp[..n]);
                            if let Some(he) = buf.windows(4).position(|w| w == b"\r\n\r\n") {
                                let headers =
                                    String::from_utf8_lossy(&buf[..he]).to_ascii_lowercase();
                                let cl: usize = headers
                                    .lines()
                                    .find_map(|l| {
                                        l.trim()
                                            .strip_prefix("content-length:")
                                            .and_then(|v| v.trim().parse().ok())
                                    })
                                    .unwrap_or(0);
                                if buf.len() >= he + 4 + cl {
                                    break;
                                }
                            }
                        }
                        Err(_) => break,
                    }
                }
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

    /// Regression: the CLOSE-WAIT hang. A server that accepts the TCP
    /// connection (or a pool-stale socket reused under a load-balanced
    /// gateway that dropped the queued request) and then sends NOTHING —
    /// no status line, no RST — used to pin the caller for the full
    /// client timeout (3600s), reading as an infinite hang. With the
    /// head-wait bound, the call must fail fast with a Transient error.
    #[tokio::test]
    async fn head_wait_bounded_when_server_accepts_but_never_answers() {
        // Accept the connection, read the request, then hold it open
        // without ever sending a status line.
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        std::thread::spawn(move || {
            for _ in 0..4 {
                let (mut sock, _) = match listener.accept() {
                    Ok(x) => x,
                    Err(_) => break,
                };
                // Drain the request, then just hold the socket open.
                let mut buf = [0u8; 4096];
                let _ = sock.set_read_timeout(Some(std::time::Duration::from_millis(300)));
                loop {
                    match sock.read(&mut buf) {
                        Ok(0) => break,
                        Ok(_) => {}
                        Err(_) => break,
                    }
                }
                // Hold for ~3s (longer than the 300ms head timeout, shorter
                // than the test can wait) and then close without a reply.
                std::thread::sleep(std::time::Duration::from_millis(3000));
                drop(sock);
            }
        });

        let provider = OpenAiProvider::with_fallbacks(cfg(port), vec![])
            .unwrap()
            .with_head_timeout(std::time::Duration::from_millis(300));

        let t0 = std::time::Instant::now();
        let err = provider.chat_complete(chat_request()).await.unwrap_err();
        let elapsed = t0.elapsed();
        let msg = format!("{err}");
        // Must be a timeout, not a hang, and must NOT take the full 3600s.
        assert!(
            msg.contains("timed out"),
            "expected head-wait timeout error, got: {msg}"
        );
        assert!(
            elapsed.as_secs() < 30,
            "head wait must fail fast, but took {elapsed:?}"
        );
    }

    /// A body stream that ends WITHOUT a terminal chunk ([DONE]/
    /// finish_reason) — i.e. the provider dropped the connection mid-
    /// completion — must surface an error rather than a clean EOF, so the
    /// agent does not mistake a truncated (often empty) answer for the
    /// final result.
    #[tokio::test]
    async fn mid_stream_eof_without_terminal_is_an_error() {
        use futures_util::StreamExt;
        // One partial data frame, then EOF — no finish_reason, no [DONE].
        let frames: Vec<Vec<u8>> = vec![
            b"data: {\"choices\":[{\"delta\":{\"content\":\"partial\"}}]}\n\n".to_vec(),
        ];
        let stream = futures_util::stream::iter(frames.into_iter().map(Ok));
        let parsed: Vec<anyhow::Result<ChatChunk>> =
            sse_line_stream(stream).collect().await;
        // The partial delta arrives, then an EOF-without-terminal error.
        assert_eq!(parsed.len(), 2, "expected a delta + a terminal error: {parsed:?}");
        assert_eq!(parsed[0].as_ref().unwrap().delta, "partial");
        let err = parsed[1].as_ref().unwrap_err().to_string();
        assert!(
            err.contains("no [DONE]/finish_reason"),
            "expected truncation error, got: {err}"
        );
    }

    /// A well-formed stream that ends with [DONE] must still close
    /// cleanly (no spurious truncation error) — guards against the
    /// EOF-detection false-positiving on the happy path.
    #[tokio::test]
    async fn wellformed_stream_with_done_closes_cleanly() {
        use futures_util::StreamExt;
        let frames: Vec<Vec<u8>> = vec![
            b"data: {\"choices\":[{\"delta\":{\"content\":\"hi\"},\"finish_reason\":\"stop\"}]}\n\n".to_vec(),
            b"data: [DONE]\n\n".to_vec(),
        ];
        let stream = futures_util::stream::iter(frames.into_iter().map(Ok));
        let parsed: Vec<anyhow::Result<ChatChunk>> =
            sse_line_stream(stream).collect().await;
        // The finish_reason chunk (done=true) AND the [DONE] chunk both
        // surface; no truncation error item. Both are terminal markers.
        assert_eq!(parsed.len(), 2, "expected finish chunk + [DONE] chunk: {parsed:?}");
        for c in parsed.iter().map(|r| r.as_ref().unwrap()) {
            assert!(c.done, "both terminal chunks must be done: {c:?}");
        }
        assert_eq!(parsed[0].as_ref().unwrap().delta, "hi");
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

    #[test]
    fn build_body_synthesizes_user_turn_when_history_has_none() {
        // Regression: strict OpenAI-compatible backends (SGLang, NInfer)
        // 400 with "No user query found in messages" when history
        // compression trims the original user prompt, leaving
        // [system, system(summary), assistant(tool_calls), tool, ...].
        let req = ChatRequest {
            model: "test".into(),
            messages: vec![
                ChatMessage::system("sys"),
                ChatMessage::system("[earlier conversation compressed]"),
                ChatMessage::assistant_with_tools("", vec![]),
                ChatMessage::tool_result("res", "c1"),
            ],
            tools: vec![],
            stream: false,
            temperature: None,
        };
        let body = build_body(&req);
        let msgs = body["messages"].as_array().unwrap();
        let roles: Vec<&str> = msgs.iter().map(|m| m["role"].as_str().unwrap()).collect();
        assert!(
            roles.contains(&"user"),
            "no-user history must be normalized to carry a user turn, got {roles:?}"
        );
        // The synthesized turn goes in after the leading system block,
        // before the assistant tool-call turn.
        let first_user = roles.iter().position(|r| *r == "user").unwrap();
        let first_assistant = roles.iter().position(|r| *r == "assistant").unwrap();
        assert!(first_user < first_assistant);
    }

    #[test]
    fn build_body_leaves_user_messages_untouched() {
        let req = ChatRequest {
            model: "test".into(),
            messages: vec![ChatMessage::system("sys"), ChatMessage::user("do it")],
            tools: vec![],
            stream: false,
            temperature: None,
        };
        let body = build_body(&req);
        let msgs = body["messages"].as_array().unwrap();
        assert_eq!(msgs.len(), 2, "normal history must not be modified");
        assert_eq!(msgs[0]["role"], "system");
        assert_eq!(msgs[1]["role"], "user");
        assert_eq!(msgs[1]["content"], "do it");
    }

    #[tokio::test]
    async fn idle_timeout_does_not_swallow_ready_items() {
        // Regression: the idle timer was polled BEFORE the inner stream,
        // so a ready item sitting behind a not-yet-fired sleep was never
        // delivered — the stream stalled until the timer fired (120s).
        // Inner data must always win over the timer.
        //
        // The stream ends with [DONE] (as every real SSE completion does),
        // so the new mid-stream-EOF truncation check does not fire here.
        use std::task::Poll;
        let mut state = 0u8;
        let slow = futures_util::stream::poll_fn(move |cx| match state {
            0 => {
                // Pending on first poll, then wake — this is the stall the
                // regression guards against.
                state = 1;
                cx.waker().wake_by_ref();
                Poll::Pending
            }
            1 => {
                state = 2;
                Poll::Ready(Some(Ok::<Vec<u8>, String>(
                    b"data: {\"choices\":[{\"delta\":{\"content\":\"hi\"}}]}\n".to_vec(),
                )))
            }
            2 => {
                state = 3;
                // Terminal [DONE] frame so the stream is well-formed.
                Poll::Ready(Some(Ok::<Vec<u8>, String>(b"data: [DONE]\n".to_vec())))
            }
            _ => Poll::Ready(None),
        });
        let stream = sse_line_stream(with_idle_timeout(slow, std::time::Duration::from_secs(5)));
        let chunks: Vec<anyhow::Result<ChatChunk>> = futures_util::StreamExt::collect(stream).await;
        // The delta chunk and the [DONE] chunk both surface; no stall, no
        // truncation error. Both are terminal.
        assert!(
            chunks.len() >= 1,
            "pending-then-ready must not stall on the idle timer"
        );
        let first = chunks[0].as_ref().expect("first chunk is Ok");
        assert_eq!(first.delta, "hi");
        // No truncation error anywhere in the collected stream.
        assert!(
            chunks.iter().all(|c| c.is_ok()),
            "well-formed stream must not report a truncation error: {chunks:?}"
        );
    }

    #[tokio::test]
    async fn falls_back_on_rate_limit_429() {
        // 429 is transient (the endpoint is overloaded, not the request
        // malformed): the fallback chain must be tried. Regression: 429
        // was classified as Fatal via is_client_error(), killing the
        // whole chain.
        let p429 = fake_endpoint(429, "rate limited");
        let ok_body = r#"{"choices":[{"message":{"content":"recovered-from-429"}}]}"#;
        let ok_port = fake_endpoint(200, ok_body);
        let provider = OpenAiProvider::with_fallbacks(cfg(p429), vec![cfg(ok_port)]).unwrap();
        let complete = provider.chat_complete(chat_request()).await.unwrap();
        assert_eq!(complete.text, "recovered-from-429");
    }

    #[test]
    fn sse_tolerates_keepalive_comments_and_bad_frames() {
        // Real providers emit `: ping` keep-alive comments and, on a
        // disconnect, torn frames. None of them may kill the stream.
        let sse = concat!(
            ": ping\n",
            "event: message\n",
            "data: {\"choices\":[{\"delta\":{\"content\":\"hi\"}}]}\n",
            "\n",
            "data: {not valid json\n",
            "data: {\"choices\":[{\"delta\":{\"content\":\"\"},\"finish_reason\":\"stop\"}]}\n",
            "data: [DONE]\n",
            "data: {\"choices\":[{\"delta\":{\"content\":\"torn\"}}\n" // torn final frame
        );
        let chunks: Vec<ChatChunk> = futures_util::StreamExt::collect::<
            Vec<anyhow::Result<ChatChunk>>,
        >(parse_stream(sse.as_bytes().to_vec()))
        .now_or_never()
        .unwrap()
        .into_iter()
        .map(|c| c.expect("no chunk may be an error"))
        .collect();
        assert_eq!(
            chunks.len(),
            3,
            "content + stop + [DONE], comments/bad frames skipped"
        );
        assert_eq!(chunks[0].delta, "hi");
        assert!(chunks[1].done);
        assert!(chunks[2].done);
    }

    #[test]
    fn with_fallbacks_rejects_malformed_base_url() {
        // A typo'd base_url must fail at construction, not walk the whole
        // fallback chain and report a confusing "all endpoints failed".
        assert!(
            OpenAiProvider::with_fallbacks(
                OpenAiConfig {
                    base_url: "not-a-url".into(),
                    api_key: "x".into(),
                },
                vec![],
            )
            .is_err()
        );
        assert!(
            OpenAiProvider::with_fallbacks(
                OpenAiConfig {
                    base_url: "ftp://example.com/v1".into(),
                    api_key: "x".into(),
                },
                vec![],
            )
            .is_err()
        );
        assert!(
            OpenAiProvider::with_fallbacks(
                OpenAiConfig {
                    base_url: "http://127.0.0.1:8000/v1".into(),
                    api_key: "x".into(),
                },
                vec![],
            )
            .is_ok()
        );
    }
}
