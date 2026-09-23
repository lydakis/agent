//! Streaming model calls. One shared HTTP transport; per-family request
//! encoding and SSE parsing. History items are streamed by reference.
use crate::{Error, Result, codec::Family, fail, sse::Decoder};
use bytes::Bytes;
use futures_util::{StreamExt, stream};
use serde::Serialize;
use serde_json::{Value, json, value::RawValue};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::{future::Future, sync::Arc, time::Duration};
use tokio::sync::Semaphore;

mod anthropic;
pub mod pace;
mod responses;

pub use pace::Report;

pub const MAX_OUTPUT: usize = 512 * 1024;
const ANTHROPIC_MAX_TOKENS: u32 = 32_768;

/// Concurrent streams one HTTP/2 connection may carry, as both current
/// providers advertise in SETTINGS_MAX_CONCURRENT_STREAMS. Requests beyond
/// it on the same connection queue in the HTTP layer, so a fleet needs
/// several connections per provider to actually run in parallel.
/// Both providers advertise 100; using fewer per connection bounds how many
/// turns one reset connection takes with it.
pub const STREAMS_PER_CONNECTION: usize = 64;

/// Default bound on time between content frames of an established stream.
/// Keepalives do not count: a provider that only pings is stalled.
pub const STALL_TIMEOUT: Duration = Duration::from_secs(120);

/// HTTP connections and the startup-admission budget shared by every
/// provider. Each shard is its own client, so its own pooled HTTP/2
/// connection per host; a request takes the least-loaded shard and holds it
/// for the life of its stream.
pub struct Transport {
    shards: Vec<Shard>,
    starting: Semaphore,
}
struct Shard {
    client: reqwest::Client,
    in_flight: AtomicUsize,
}
/// Holds a shard's in-flight count until the request, stream included, ends.
struct Lease<'a>(&'a AtomicUsize);
impl Drop for Lease<'_> {
    fn drop(&mut self) {
        self.0.fetch_sub(1, Ordering::Relaxed);
    }
}
impl Transport {
    /// `max_connecting` bounds requests awaiting response headers; zero means
    /// no bound beyond the operating system. `connections` is the number of
    /// shards, at least one.
    pub fn new(max_connecting: usize, connections: usize) -> Result<Arc<Self>> {
        let shards = (0..connections.max(1))
            .map(|_| {
                // No total deadline: long generations are legitimate. Idle
                // reads are bounded so a stalled stream cannot hold a turn.
                let client = reqwest::Client::builder()
                    .no_proxy()
                    .redirect(reqwest::redirect::Policy::none())
                    .connect_timeout(Duration::from_secs(10))
                    .read_timeout(Duration::from_secs(120))
                    .pool_idle_timeout(Duration::from_secs(60))
                    .pool_max_idle_per_host(1024)
                    .build()
                    .map_err(|_| Error::new("http_client_init"))?;
                Ok(Shard {
                    client,
                    in_flight: AtomicUsize::new(0),
                })
            })
            .collect::<Result<Vec<_>>>()?;
        Ok(Arc::new(Self {
            shards,
            starting: Semaphore::new(if max_connecting == 0 {
                Semaphore::MAX_PERMITS
            } else {
                max_connecting.min(Semaphore::MAX_PERMITS)
            }),
        }))
    }
    pub fn connections(&self) -> usize {
        self.shards.len()
    }
    /// Startup permits still available; none when the bound is off.
    pub fn starting_permits(&self) -> usize {
        self.starting.available_permits()
    }
    /// The least-loaded shard. The counts are advisory: a concurrent lease
    /// may pick the same shard, which only costs balance, never correctness.
    fn lease(&self) -> (&reqwest::Client, Lease<'_>) {
        let shard = self
            .shards
            .iter()
            .min_by_key(|shard| shard.in_flight.load(Ordering::Relaxed))
            .expect("at least one shard");
        shard.in_flight.fetch_add(1, Ordering::Relaxed);
        (&shard.client, Lease(&shard.in_flight))
    }
    /// In-flight requests per shared client shard across all providers, for `stats`.
    pub fn loads(&self) -> Vec<usize> {
        self.shards
            .iter()
            .map(|shard| shard.in_flight.load(Ordering::Relaxed))
            .collect()
    }
}

#[derive(Clone)]
pub struct Provider {
    transport: Arc<Transport>,
    /// One pace per model behind this provider, shared by every clone.
    pools: Arc<pace::Pools>,
    family: Family,
    url: reqwest::Url,
    key: Option<String>,
    max_output_tokens: Option<u32>,
    stall_timeout: Duration,
}

#[derive(Debug)]
pub enum Delta {
    Text(String),
    Thinking(String),
}
/// What one decoded SSE frame carried.
#[derive(Debug)]
enum Frame {
    /// Text or thinking to publish as it streams.
    Delta(Delta),
    /// Progress with nothing to publish.
    Quiet,
    /// Only holds the connection open, so it does not renew the stall bound.
    Keepalive,
}
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
pub struct Usage {
    pub input_tokens: u64,
    pub output_tokens: u64,
    pub cached_input_tokens: u64,
}
#[derive(Debug)]
pub struct Completion {
    pub items: Vec<Bytes>,
    pub calls: Vec<ToolCall>,
    pub usage: Option<Usage>,
}
#[derive(Debug, Clone, PartialEq, Eq, serde::Deserialize, serde::Serialize)]
pub struct ToolCall {
    pub name: String,
    pub call_id: String,
    pub arguments: String,
}
/// The conversation items of a request: a stream of pre-encoded,
/// comma-separated items of known total length, so the body is never
/// assembled in memory.
pub struct Items {
    /// Exact byte length the stream will yield.
    pub bytes: usize,
    pub stream: futures_util::stream::BoxStream<'static, std::io::Result<Bytes>>,
}
impl Items {
    pub fn empty() -> Items {
        Items {
            bytes: 0,
            stream: stream::empty().boxed(),
        }
    }
}
pub struct Request<'a> {
    pub model: &'a str,
    pub instructions: &'a str,
    pub reasoning: Option<&'a str>,
    /// The bot's tools, encoded for this family; `[]` when it has none.
    pub tools: &'a RawValue,
    /// Keep schemas needed to interpret history while disabling new calls.
    pub allow_tool_calls: bool,
    pub items: Items,
}

enum Parser {
    Responses(responses::State),
    Anthropic(anthropic::State),
}
impl Parser {
    fn frame(&mut self, frame: &[u8]) -> Result<Frame> {
        match self {
            Parser::Responses(state) => state.frame(frame),
            Parser::Anthropic(state) => state.frame(frame),
        }
    }
    fn usage(&self) -> Option<Usage> {
        match self {
            Parser::Responses(state) => state.usage(),
            Parser::Anthropic(state) => state.usage(),
        }
    }
    fn finish(self) -> Result<Completion> {
        match self {
            Parser::Responses(state) => state.finish(),
            Parser::Anthropic(state) => state.finish(),
        }
    }
}

impl Provider {
    pub fn new(
        transport: Arc<Transport>,
        family: Family,
        base_url: &str,
        key: Option<String>,
    ) -> Result<Self> {
        let mut url =
            reqwest::Url::parse(base_url).map_err(|_| Error::new("invalid_provider_url"))?;
        if !matches!(url.scheme(), "http" | "https")
            || !url.username().is_empty()
            || url.password().is_some()
            || url.query().is_some()
            || url.fragment().is_some()
        {
            return fail("invalid_provider_url");
        }
        let route = match family {
            Family::Responses => "responses",
            Family::Anthropic => "messages",
        };
        url.set_path(&format!("{}/{route}", url.path().trim_end_matches('/')));
        Ok(Self {
            transport,
            pools: Arc::new(pace::Pools::default()),
            family,
            url,
            key,
            max_output_tokens: None,
            stall_timeout: STALL_TIMEOUT,
        })
    }
    /// Model pool levels behind this provider, for `stats`.
    pub fn status(&self) -> serde_json::Value {
        serde_json::json!({
            "pools": self.pools.status(),
        })
    }
    pub fn family(&self) -> Family {
        self.family
    }
    /// How much longer this model's pool is closed by a rate limit, if it is.
    pub fn blocked_for(&self, model: &str) -> Option<std::time::Duration> {
        self.pools.get(&self.family.pool_key(model)).blocked_for()
    }

    /// Bound generated tokens (including reasoning) for Responses calls.
    /// Other families need their own budget validation and reject this option.
    pub fn with_max_output_tokens(mut self, limit: u32) -> Result<Self> {
        if self.family != Family::Responses || limit == 0 {
            return fail("invalid_output_token_limit");
        }
        self.max_output_tokens = Some(limit);
        Ok(self)
    }

    /// Bound the time an established stream may go without a content frame:
    /// more than zero, at most a day.
    pub fn with_stall_timeout(mut self, bound: Duration) -> Result<Self> {
        if bound.is_zero() || bound > Duration::from_secs(86_400) {
            return fail("invalid_stall_timeout");
        }
        self.stall_timeout = bound;
        Ok(self)
    }

    /// Everything before the history array, ending with `[`.
    fn prefix(&self, request: &Request<'_>) -> Result<Vec<u8>> {
        #[derive(Serialize)]
        struct Responses<'a> {
            model: &'a str,
            instructions: &'a str,
            stream: bool,
            store: bool,
            include: [&'static str; 1],
            #[serde(skip_serializing_if = "Option::is_none")]
            max_output_tokens: Option<u32>,
            tools: &'a RawValue,
            #[serde(skip_serializing_if = "Option::is_none")]
            tool_choice: Option<&'static str>,
            #[serde(skip_serializing_if = "Option::is_none")]
            reasoning: Option<Value>,
        }
        #[derive(Serialize)]
        struct ToolChoice {
            r#type: &'static str,
        }
        #[derive(Serialize)]
        struct Anthropic<'a> {
            model: &'a str,
            max_tokens: u32,
            #[serde(skip_serializing_if = "Option::is_none")]
            system: Option<Value>,
            stream: bool,
            /// Automatic caching: the API places a breakpoint on the last
            /// cacheable block and moves it forward as the history grows.
            cache_control: Value,
            #[serde(skip_serializing_if = "Option::is_none")]
            tools: Option<&'a RawValue>,
            #[serde(skip_serializing_if = "Option::is_none")]
            tool_choice: Option<ToolChoice>,
            #[serde(skip_serializing_if = "Option::is_none")]
            thinking: Option<Value>,
            #[serde(skip_serializing_if = "Option::is_none")]
            output_config: Option<Value>,
        }
        let disable_tools = !request.allow_tool_calls && request.tools.get() != "[]";
        let (mut bytes, field) = match self.family {
            Family::Responses => (
                serde_json::to_vec(&Responses {
                    model: request.model,
                    instructions: request.instructions,
                    stream: true,
                    store: false,
                    // Request opaque reasoning for stateless continuation across endpoints.
                    include: ["reasoning.encrypted_content"],
                    max_output_tokens: self.max_output_tokens,
                    tools: request.tools,
                    tool_choice: disable_tools.then_some("none"),
                    reasoning: request
                        .reasoning
                        .map(|effort| json!({"effort":effort,"summary":"auto"})),
                })?,
                &b",\"input\":["[..],
            ),
            Family::Anthropic => (
                serde_json::to_vec(&Anthropic {
                    model: request.model,
                    max_tokens: ANTHROPIC_MAX_TOKENS,
                    // An explicit breakpoint after the static prefix (tools
                    // and instructions) guarantees a read point for it.
                    system: (!request.instructions.is_empty()).then(|| {
                        json!([{"type":"text","text":request.instructions,
                            "cache_control":{"type":"ephemeral"}}])
                    }),
                    stream: true,
                    cache_control: json!({"type":"ephemeral"}),
                    tools: (request.tools.get() != "[]").then_some(request.tools),
                    tool_choice: disable_tools.then_some(ToolChoice { r#type: "none" }),
                    // Current Claude models take adaptive thinking with an
                    // effort level and reject budgets; Haiku 4.5 and older
                    // models still need an explicit budget.
                    thinking: request.reasoning.map(|level| {
                        if legacy_thinking(request.model) {
                            let budget = match level {
                                "low" => 2048,
                                "medium" => 8192,
                                _ => 16384,
                            };
                            json!({"type":"enabled","budget_tokens":budget})
                        } else {
                            json!({"type":"adaptive","display":"summarized"})
                        }
                    }),
                    output_config: request
                        .reasoning
                        .filter(|_| !legacy_thinking(request.model))
                        .map(|level| json!({"effort":level})),
                })?,
                &b",\"messages\":["[..],
            ),
        };
        bytes.pop(); // Replace the closing brace with the streamed array field.
        bytes.extend_from_slice(field);
        Ok(bytes)
    }

    /// Content-Length avoids requiring provider support for chunked uploads;
    /// the items stream through without a whole-body copy.
    fn body(&self, prefix: Vec<u8>, items: Items) -> (reqwest::Body, usize) {
        let len = prefix.len() + items.bytes + 2;
        let framed = stream::iter([Ok(Bytes::from(prefix))])
            .chain(items.stream)
            .chain(stream::iter([Ok(Bytes::from_static(b"]}"))]));
        (reqwest::Body::wrap_stream(framed), len)
    }

    pub async fn complete<F, Fut>(&self, request: Request<'_>, delta: F) -> Result<Completion>
    where
        F: FnMut(Delta) -> Fut,
        Fut: Future<Output = Result<()>>,
    {
        self.complete_accounted(request, delta, &mut Report::default())
            .await
    }

    /// Also return provider-reported usage when a stream or completion fails.
    /// Successful callers still commit usage with their accepted transcript.
    pub async fn complete_accounted<F, Fut>(
        &self,
        request: Request<'_>,
        delta: F,
        report: &mut Report,
    ) -> Result<Completion>
    where
        F: FnMut(Delta) -> Fut,
        Fut: Future<Output = Result<()>>,
    {
        *report = Report::default();
        self.complete_inner(request, delta, report)
            .await
            .map_err(|error| sanitize_error(error, self.key.as_deref()))
    }

    async fn complete_inner<F, Fut>(
        &self,
        request: Request<'_>,
        mut delta: F,
        report: &mut Report,
    ) -> Result<Completion>
    where
        F: FnMut(Delta) -> Fut,
        Fut: Future<Output = Result<()>>,
    {
        // Pace first: the provider's allowance is the scarce resource. The
        // estimate is input bytes over four plus the output cap, or 512 when
        // none is configured; that is an estimate to reserve against, not a
        // ceiling on what the provider generates or bills, and the usage the
        // response reports corrects it.
        let pace = self.pools.get(&self.family.pool_key(request.model));
        let prefix = self.prefix(&request)?;
        let estimate = pace::Cost {
            input: ((prefix.len() + request.items.bytes) / 4) as u64,
            output: u64::from(self.max_output_tokens.unwrap_or(512)),
        };
        let mut reservation = pace.acquire_reported(estimate, report).await?;
        // Bound request startup until response headers arrive; release before
        // reading SSE so established streams are not capped at this limit.
        let admission =
            tokio::time::timeout(Duration::from_secs(60), self.transport.starting.acquire())
                .await
                .map_err(|_| Error::new("provider_admission_timeout"))?
                .map_err(|_| Error::new("provider_admission_closed"))?;
        let (body, len) = self.body(prefix, request.items);
        // The lease lives until this function returns, stream included.
        let (client, _lease) = self.transport.lease();
        let mut http = client
            .post(self.url.clone())
            .header("content-type", "application/json")
            .header("content-length", len)
            .body(body);
        http = match (self.family, &self.key) {
            (Family::Responses, Some(key)) => http.bearer_auth(key),
            (Family::Anthropic, key) => {
                let http = http.header("anthropic-version", "2023-06-01");
                match key {
                    Some(key) => http.header("x-api-key", key),
                    None => http,
                }
            }
            (Family::Responses, None) => http,
        };
        reservation.dispatch();
        report.dispatched = true;
        let response = match http.send().await {
            Ok(response) => response,
            Err(error) => {
                reservation.settle(0);
                return Err(connection_error(error));
            }
        };
        drop(admission);
        if !response.status().is_success() {
            let status = response.status().as_u16();
            let headers = response.headers().clone();
            let body = error_body(response).await.unwrap_or_default();
            let quota = status == 429 && body.quota;
            if !quota {
                reservation.learn(&headers, self.family);
            }
            if matches!(status, 429 | 529) && !quota {
                let after = headers
                    .get("retry-after")
                    .and_then(|v| v.to_str().ok())
                    .and_then(|v| v.trim().parse::<f64>().ok())
                    .and_then(|seconds| Duration::try_from_secs_f64(seconds).ok());
                pace.limited(after);
            }
            reservation.settle(0);
            let code = if quota {
                "provider_quota_exhausted".to_owned()
            } else {
                format!("provider_http_{status}")
            };
            return Err(Error {
                code,
                detail: body.detail,
            });
        }
        reservation.learn(response.headers(), self.family);
        if response
            .headers()
            .get("content-type")
            .and_then(|v| v.to_str().ok())
            .is_none_or(|v| v.split(';').next() != Some("text/event-stream"))
        {
            return fail("provider_expected_sse");
        }
        let mut stream = response.bytes_stream();
        let mut decoder = Decoder::default();
        let mut parser = match self.family {
            Family::Responses => Parser::Responses(responses::State::default()),
            Family::Anthropic => Parser::Anthropic(anthropic::State::default()),
        };
        let result = async {
            let mut total = 0usize;
            // Only a content frame renews the stall deadline. The client's
            // read timeout restarts on any byte, so pings alone never trip it.
            let stall = tokio::time::sleep(self.stall_timeout);
            tokio::pin!(stall);
            loop {
                let chunk = tokio::select! {
                    chunk = stream.next() => chunk,
                    () = &mut stall => return fail("provider_stream_stalled"),
                };
                let Some(chunk) = chunk else { break };
                let chunk = chunk.map_err(|_| Error::new("provider_stream_failed"))?;
                total += chunk.len();
                if total > 16 * 1024 * 1024 {
                    return fail("provider_response_limit");
                }
                let mut progressed = false;
                for byte in chunk {
                    let Some(frame) = decoder.byte(byte)? else {
                        continue;
                    };
                    if frame == b"[DONE]" {
                        continue;
                    }
                    match parser.frame(&frame)? {
                        Frame::Keepalive => continue,
                        Frame::Delta(part) => delta(part).await?,
                        Frame::Quiet => {}
                    }
                    progressed = true;
                }
                // Renewed after publishing, so a slow consumer is not a stall.
                if progressed {
                    stall
                        .as_mut()
                        .reset(tokio::time::Instant::now() + self.stall_timeout);
                }
            }
            if !decoder.is_empty() {
                return fail("truncated_sse_frame");
            }
            Ok(())
        }
        .await;
        report.usage = parser.usage();
        reservation.settle_usage(report.usage.as_ref(), estimate);
        if let Err(error) = &result
            && error.code == "provider_rate_limited"
        {
            pace.limited(error.detail.as_deref().and_then(pace::named_delay));
        }
        result?;
        parser.finish().inspect_err(|error| {
            if error.code == "provider_rate_limited" {
                pace.limited(error.detail.as_deref().and_then(pace::named_delay));
            }
        })
    }
}

/// Model ids that predate adaptive thinking and still require a token budget.
fn legacy_thinking(model: &str) -> bool {
    [
        "claude-haiku-4-5",
        "claude-sonnet-4-5",
        "claude-opus-4-5",
        "claude-opus-4-1",
        "claude-sonnet-4-",
        "claude-opus-4-0",
        "claude-3-",
    ]
    .iter()
    .any(|prefix| model.starts_with(prefix))
        && !model.starts_with("claude-sonnet-4-6")
        && !model.starts_with("claude-opus-4-6")
}

// Preserve only stage, timeout classification, and numeric OS code. Reqwest's
// Display/debug strings can include URLs and must never become diagnostics.
/// Classify a transport failure and keep its cause chain as the detail. The
/// chain names the URL and the HTTP, TLS, or socket layer that failed, never a
/// header, so it is safe once the caller's key redaction has run.
fn connection_error(error: reqwest::Error) -> Error {
    if error.is_timeout() {
        return Error::new("provider_connection_timeout");
    }
    let mut chain = error.to_string();
    let mut os_code = None;
    let mut source = std::error::Error::source(&error);
    while let Some(cause) = source {
        chain.push_str(": ");
        chain.push_str(&cause.to_string());
        if os_code.is_none() {
            os_code = cause
                .downcast_ref::<std::io::Error>()
                .and_then(|e| e.raw_os_error());
        }
        source = cause.source();
    }
    match os_code {
        Some(code) => Error::with(&format!("provider_connection_os_{code}"), chain),
        None => Error::with("provider_connection_failed", chain),
    }
}

/// Capture only a complete, bounded error body. A partial body may end in a
/// credential, so never publish it as a fallback diagnostic.
#[derive(Default)]
struct ErrorBody {
    detail: Option<String>,
    quota: bool,
}

async fn error_body(response: reqwest::Response) -> Option<ErrorBody> {
    let expects_json = response
        .headers()
        .get("content-type")
        .and_then(|header| header.to_str().ok())
        .is_some_and(|header| {
            let mime = header
                .split(';')
                .next()
                .unwrap_or_default()
                .trim()
                .to_ascii_lowercase();
            mime == "application/json" || mime.ends_with("+json")
        });
    let mut stream = response.bytes_stream();
    let mut body = Vec::new();
    while let Some(chunk) = stream.next().await {
        let chunk = chunk.ok()?;
        if chunk.len() > 4096 - body.len() {
            return None;
        }
        body.extend_from_slice(&chunk);
    }
    let text = std::str::from_utf8(&body).ok()?.trim();
    if text.is_empty() {
        return None;
    }
    match serde_json::from_str::<Value>(text) {
        Ok(value) => Some(ErrorBody {
            quota: ["code", "type"]
                .iter()
                .any(|field| value["error"][field].as_str() == Some("insufficient_quota")),
            detail: value
                .as_str()
                .or_else(|| value["error"]["message"].as_str())
                .or_else(|| value["error"].as_str())
                .or_else(|| value["message"].as_str())
                .map(str::to_owned),
        }),
        // Never fall back to a serialized JSON representation: alternate
        // escapes could conceal a credential from exact text redaction.
        Err(_) if expects_json || text.starts_with(['{', '[', '"']) => None,
        Err(_) => Some(ErrorBody {
            detail: Some(text.to_owned()),
            quota: false,
        }),
    }
}

pub(crate) fn detail_of(value: &Value) -> Option<String> {
    value["error"]["message"]
        .as_str()
        .or_else(|| value["message"].as_str())
        .map(str::to_owned)
}

/// Provider-origin details must cross this boundary before any caller can
/// display or persist them. Decode/extract first, redact next, truncate last.
fn sanitize_error(mut error: Error, key: Option<&str>) -> Error {
    if let Some(mut detail) = error.detail.take() {
        if let Some(key) = key.filter(|key| !key.is_empty()) {
            if detail.contains(key) {
                detail = detail.replace(key, "[REDACTED]");
            }
            // Plain-text and decoded provider messages may themselves quote
            // the standard JSON representation of a credential.
            if let Ok(encoded) = serde_json::to_string(key) {
                let escaped = &encoded[1..encoded.len() - 1];
                if escaped != key && detail.contains(escaped) {
                    detail = detail.replace(escaped, "[REDACTED]");
                }
            }
        }
        let trimmed = detail.trim();
        if !trimmed.is_empty() {
            error.detail = Some(trimmed.chars().take(512).collect());
        }
    }
    error
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::codec::Family;
    fn none() -> Box<RawValue> {
        RawValue::from_string("[]".into()).unwrap()
    }

    #[test]
    fn request_prefix_streams_history_after_family_specific_fields() {
        let transport = Transport::new(64, 1).unwrap();
        let provider = Provider::new(
            transport.clone(),
            Family::Anthropic,
            "https://api.example.test",
            None,
        )
        .unwrap();
        let prefix = provider
            .prefix(&Request {
                model: "m",
                instructions: "i",
                reasoning: Some("low"),
                tools: &none(),
                allow_tool_calls: true,
                items: Items::empty(),
            })
            .unwrap();
        let text = String::from_utf8(prefix).unwrap();
        assert!(text.ends_with(",\"messages\":["));
        assert!(text.contains("\"type\":\"adaptive\""));
        assert_eq!(text.matches("\"cache_control\"").count(), 2);
        assert!(text.contains("\"effort\":\"low\""));
        let legacy = provider
            .prefix(&Request {
                model: "claude-haiku-4-5-20251001",
                instructions: "i",
                reasoning: Some("low"),
                tools: &none(),
                allow_tool_calls: true,
                items: Items::empty(),
            })
            .unwrap();
        let legacy = String::from_utf8(legacy).unwrap();
        assert!(legacy.contains("\"budget_tokens\":2048"));
        assert!(!legacy.contains("output_config"));
        assert!(!text.contains("budget_tokens"));
        let mut empty_prefix = provider
            .prefix(&Request {
                model: "m",
                instructions: "",
                reasoning: None,
                tools: &none(),
                allow_tool_calls: true,
                items: Items::empty(),
            })
            .unwrap();
        empty_prefix.extend_from_slice(b"]}");
        let empty: Value = serde_json::from_slice(&empty_prefix).unwrap();
        assert!(empty.get("system").is_none());
        assert_eq!(empty["cache_control"], json!({"type":"ephemeral"}));
        assert!(!text.contains("\"tools\""));
        assert_eq!(provider.url.path(), "/messages");
        let responses = Provider::new(transport, Family::Responses, "http://h/v1/", None).unwrap();
        assert_eq!(responses.url.path(), "/v1/responses");
        assert!(responses.clone().with_max_output_tokens(0).is_err());
        assert!(provider.with_max_output_tokens(2048).is_err());
        let responses = responses.with_max_output_tokens(2048).unwrap();
        let mut prefix = responses
            .prefix(&Request {
                model: "m",
                instructions: "i",
                reasoning: None,
                tools: &none(),
                allow_tool_calls: true,
                items: Items::empty(),
            })
            .unwrap();
        prefix.extend_from_slice(b"]}");
        let body: Value = serde_json::from_slice(&prefix).unwrap();
        assert_eq!(body["max_output_tokens"], 2048);
        assert_eq!(body["include"], json!(["reasoning.encrypted_content"]));
        assert_eq!(body["store"], false);
    }

    #[test]
    fn tool_call_policy_preserves_schemas_and_uses_family_wire_format() {
        let transport = Transport::new(64, 1).unwrap();
        for family in [Family::Anthropic, Family::Responses] {
            let provider =
                Provider::new(transport.clone(), family, "http://example.test", None).unwrap();
            let schemas = crate::tools::Registry::all()
                .unwrap()
                .encoded(family, &["echo".into()])
                .unwrap();
            for allow in [true, false] {
                for tools in [&*schemas, &*none()] {
                    let mut prefix = provider
                        .prefix(&Request {
                            model: "m",
                            instructions: "i",
                            reasoning: None,
                            tools,
                            allow_tool_calls: allow,
                            items: Items::empty(),
                        })
                        .unwrap();
                    prefix.extend_from_slice(b"]}");
                    let request: Value = serde_json::from_slice(&prefix).unwrap();
                    if tools.get() != "[]" {
                        assert_eq!(
                            request["tools"],
                            serde_json::from_str::<Value>(tools.get()).unwrap()
                        );
                    }
                    if allow || tools.get() == "[]" {
                        assert!(request.get("tool_choice").is_none());
                    } else {
                        assert_eq!(
                            request["tool_choice"],
                            match family {
                                Family::Anthropic => json!({"type":"none"}),
                                Family::Responses => json!("none"),
                            }
                        );
                    }
                }
            }
        }
    }

    #[test]
    fn leases_take_the_least_loaded_connection_and_release_on_drop() {
        let transport = Transport::new(64, 3).unwrap();
        assert_eq!(transport.connections(), 3);
        let a = transport.lease().1;
        let b = transport.lease().1;
        let c = transport.lease().1;
        assert_eq!(transport.loads(), vec![1, 1, 1]);
        drop(b);
        let d = transport.lease().1;
        assert_eq!(transport.loads(), vec![1, 1, 1]);
        drop(a);
        drop(c);
        drop(d);
        assert_eq!(transport.loads(), vec![0, 0, 0]);
        assert_eq!(Transport::new(0, 0).unwrap().connections(), 1);
    }

    /// Serve one SSE response: text deltas `gap` apart with a ping and a
    /// comment keepalive between them, then only keepalives, forever.
    async fn pinging(deltas: usize, gap: Duration) -> String {
        use tokio::io::AsyncWriteExt;
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.unwrap();
            let frame = |data: &str| format!("data: {data}\n\n");
            let mut out = String::from(
                "HTTP/1.1 200 OK\r\ncontent-type: text/event-stream\r\nconnection: close\r\n\r\n",
            );
            out += &frame(r#"{"type":"message_start","message":{"usage":{"input_tokens":1}}}"#);
            out += &frame(
                r#"{"type":"content_block_start","index":0,"content_block":{"type":"text","text":""}}"#,
            );
            let keepalive = frame(r#"{"type":"ping"}"#) + ": keepalive\n\n";
            for n in 0.. {
                if n < deltas {
                    out += &frame(&format!(
                        r#"{{"type":"content_block_delta","index":0,"delta":{{"type":"text_delta","text":"{n}"}}}}"#
                    ));
                }
                out += &keepalive;
                if socket.write_all(out.as_bytes()).await.is_err() {
                    return;
                }
                out.clear();
                tokio::time::sleep(gap / 4).await;
                if socket.write_all(keepalive.as_bytes()).await.is_err() {
                    return;
                }
                tokio::time::sleep(gap * 3 / 4).await;
            }
        });
        url
    }

    #[tokio::test]
    async fn a_stream_that_only_pings_stalls_after_its_last_content() {
        let gap = Duration::from_millis(100);
        let bound = Duration::from_millis(300);
        let url = pinging(6, gap).await;
        let provider = Provider::new(Transport::new(0, 1).unwrap(), Family::Anthropic, &url, None)
            .unwrap()
            .with_stall_timeout(bound)
            .unwrap();
        let tools = none();
        let mut text = String::new();
        let started = tokio::time::Instant::now();
        let call = provider.complete(
            Request {
                model: "m",
                instructions: "",
                reasoning: None,
                tools: &tools,
                allow_tool_calls: true,
                items: Items::empty(),
            },
            |delta| {
                if let Delta::Text(part) = delta {
                    text.push_str(&part);
                }
                async { Ok(()) }
            },
        );
        // Keepalives arrive well inside the bound, so only the guard can end
        // this call; the client's read timeout never fires.
        let error = tokio::time::timeout(Duration::from_secs(10), call)
            .await
            .expect("the stall guard ends a stream that only pings")
            .unwrap_err();
        assert_eq!(error.code, "provider_stream_stalled");
        // Content kept the stream alive past the bound; pings did not.
        assert_eq!(text, "012345");
        assert!(started.elapsed() >= gap * 5 + bound);
        assert!(
            Provider::new(Transport::new(0, 1).unwrap(), Family::Anthropic, &url, None)
                .unwrap()
                .with_stall_timeout(Duration::ZERO)
                .is_err()
        );
    }

    #[test]
    fn body_length_counts_prefix_items_separators_and_close() {
        let transport = Transport::new(64, 1).unwrap();
        let provider = Provider::new(transport, Family::Responses, "http://h/v1/", None).unwrap();
        let items: Vec<Bytes> = (0..3)
            .map(|n| Bytes::from(Family::Responses.user_item(&format!("m{n}")).unwrap()))
            .collect();
        let joined = items.iter().map(Bytes::len).sum::<usize>() + items.len() - 1;
        let prefix = b"{\"input\":[".to_vec();
        let (_, len) = provider.body(
            prefix.clone(),
            Items {
                bytes: joined,
                stream: stream::empty().boxed(),
            },
        );
        assert_eq!(len, prefix.len() + joined + 2);
    }
}
