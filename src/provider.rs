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
pub mod login;
pub mod pace;
mod responses;
mod socket;

pub use pace::Report;
pub use socket::Sockets;

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
                    .user_agent(concat!("agent-runtime/", env!("CARGO_PKG_VERSION")))
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
    /// ChatGPT workspace for a ChatGPT-login key, sent as `ChatGPT-Account-ID`.
    account: Option<String>,
    /// A ChatGPT login re-read from its file, in place of a fixed key.
    login: Option<Arc<login::Login>>,
    max_output_tokens: Option<u32>,
    stall_timeout: Duration,
    /// Responses over WebSocket, one connection per bot, instead of HTTP.
    sockets: Option<Arc<Sockets>>,
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
    /// Which bot is asking, and where the items sit in its history, so a
    /// socket provider can send only what the server has not seen.
    pub chain: Option<Chain<'a>>,
}

/// A request's place in its bot's history. `items` is the whole input; when
/// the server already holds the previous response, only `tail(n)`, the
/// items after the first `n` window ids, is sent instead.
pub struct Chain<'a> {
    pub bot: &'a str,
    /// Context bytes ahead of the window items (summary and notes), and the
    /// window's node ids; `None` for a request that is not a window, such as
    /// a summary over a span.
    pub window: Option<(&'a [u8], &'a [i64])>,
    pub tail: Box<dyn FnOnce(usize) -> Items + Send + 'a>,
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
            account: None,
            login: None,
            max_output_tokens: None,
            stall_timeout: STALL_TIMEOUT,
            sockets: None,
        })
    }
    /// Model pool levels behind this provider, for `stats`.
    pub fn status(&self) -> serde_json::Value {
        let mut status = serde_json::json!({
            "pools": self.pools.status(),
        });
        if let Some(sockets) = &self.sockets {
            status["sockets"] = sockets.open().into();
        }
        status
    }

    /// Carry Responses calls over WebSocket, keeping one connection per bot
    /// so a call can continue from the bot's previous response.
    pub fn with_socket(mut self) -> Result<Self> {
        if self.family != Family::Responses {
            return fail("invalid_provider_transport");
        }
        self.sockets = Some(Sockets::new()?);
        Ok(self)
    }

    /// The bot was deleted; close its connection.
    pub fn forget(&self, bot: &str) {
        if let Some(sockets) = &self.sockets {
            sockets.forget(bot);
        }
    }

    /// The bot recorded the response it was just given as these node ids.
    /// Its next window can then continue from that response.
    pub fn recorded(&self, bot: &str, ids: &[i64]) {
        if let Some(sockets) = &self.sockets {
            sockets.recorded(bot, ids);
        }
    }
    pub fn family(&self) -> Family {
        self.family
    }

    /// Planning estimate only: tokens do not bound encoded JSON bytes.
    /// An unset Responses cap is unknown, not an invented output limit.
    pub fn output_byte_estimate(&self) -> Option<usize> {
        match self.family {
            Family::Responses => self.max_output_tokens,
            Family::Anthropic => Some(ANTHROPIC_MAX_TOKENS),
        }
        .map(|tokens| (tokens as usize).saturating_mul(4))
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

    /// Name the ChatGPT workspace a ChatGPT-login access token acts for.
    pub fn with_account(mut self, account: String) -> Result<Self> {
        if self.family != Family::Responses || account.is_empty() {
            return fail("invalid_provider_account");
        }
        self.account = Some(account);
        Ok(self)
    }

    /// Send a ChatGPT login's current token and workspace on every request,
    /// re-read from its file when it expires or the endpoint refuses it.
    pub fn with_login(mut self, login: Arc<login::Login>) -> Result<Self> {
        if self.family != Family::Responses {
            return fail("invalid_provider_account");
        }
        self.login = Some(login);
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
            /// Absent over a socket, where the create event takes no `stream`.
            #[serde(skip_serializing_if = "Option::is_none")]
            stream: Option<bool>,
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
                    stream: self.sockets.is_none().then_some(true),
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
        let mut session = None;
        let result = self
            .complete_inner(request, delta, report, &mut session)
            .await;
        let key = session
            .as_ref()
            .map(|s| s.token.as_str())
            .or(self.key.as_deref());
        result.map_err(|error| sanitize_error(error, key))
    }

    async fn complete_inner<F, Fut>(
        &self,
        request: Request<'_>,
        mut delta: F,
        report: &mut Report,
        session: &mut Option<login::Session>,
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
        let admission = self.admit().await?;
        if let Some(sockets) = &self.sockets {
            return self
                .complete_socket(
                    sockets,
                    request,
                    prefix,
                    delta,
                    report,
                    (&pace, reservation, estimate),
                    admission,
                )
                .await;
        }
        let (body, len) = self.body(prefix, request.items);
        // The lease lives until this function returns, stream included.
        let (client, _lease) = self.transport.lease();
        // A token can expire while pacing or waiting for admission. Read it
        // only now, before building the headers that will carry it.
        if let Some(login) = &self.login {
            *session = Some(login.current()?);
        }
        let key = session.as_ref().map(|s| &s.token).or(self.key.as_ref());
        let account = session
            .as_ref()
            .map(|s| &s.account)
            .or(self.account.as_ref());
        let mut http = client
            .post(self.url.clone())
            .header("content-type", "application/json")
            .header("content-length", len)
            .header("accept", "text/event-stream")
            .body(body);
        http = match (self.family, key) {
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
        if let Some(account) = account {
            http = http.header("chatgpt-account-id", account);
        }
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
            // A refused login is re-read once: Codex may have signed in or
            // selected another account. Retry when either auth field changed.
            if status == 401
                && let (Some(login), Some(session)) = (&self.login, session.as_ref())
            {
                reservation.settle(0);
                let refused = body.detail.unwrap_or_else(|| "HTTP 401".to_owned());
                return Err(match login.reload(session)? {
                    true => Error::with("provider_login_refreshed", refused),
                    false => Error::with(
                        "provider_login_rejected",
                        format!(
                            "{refused}; {} holds the refused token, run any codex command to sign in again",
                            login.path().display()
                        ),
                    ),
                });
            }
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
        let status = response.status().as_u16();
        let content_type = response
            .headers()
            .get("content-type")
            .and_then(|v| v.to_str().ok())
            .map(str::to_owned);
        // The ChatGPT Codex endpoint streams without naming a content type, so
        // only a named non-stream type is refused; an unnamed body is decoded
        // and fails as a truncated or incomplete stream if it is not one.
        if let Some(content_type) = content_type
            .as_deref()
            .filter(|v| v.split(';').next() != Some("text/event-stream"))
        {
            // Name what came instead, and the message it carried when the
            // body is a short error, so a gateway's refusal is diagnosable.
            let body = error_body(response).await.and_then(|body| body.detail);
            let detail = [
                Some(format!("HTTP {status}, content-type {content_type}")),
                body,
            ];
            return Err(Error::with(
                "provider_expected_sse",
                detail.into_iter().flatten().collect::<Vec<_>>().join(": "),
            ));
        }
        let mut stream = response.bytes_stream();
        let mut decoder = Decoder::default();
        let mut parser = match self.family {
            Family::Responses => Parser::Responses(responses::State::default()),
            Family::Anthropic => Parser::Anthropic(anthropic::State::default()),
        };
        let unnamed = content_type.is_none();
        let mut frames = 0usize;
        let mut preview = Vec::new();
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
                if unnamed && frames == 0 && preview.len() < 4096 {
                    let room = 4096 - preview.len();
                    preview.extend_from_slice(&chunk[..chunk.len().min(room)]);
                }
                let mut progressed = false;
                for byte in chunk {
                    let Some(frame) = decoder.byte(byte)? else {
                        continue;
                    };
                    if unnamed && frames == 0 {
                        // A completed frame rules out the no-frame diagnostic.
                        // Release its bounded preview for the rest of the stream.
                        preview = Vec::new();
                    }
                    frames += 1;
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
        // A success that named no content type and carried no SSE frame was
        // not a stream: say so, with the short message it held, instead of
        // reporting a truncated or incomplete stream.
        let no_stream = unnamed
            && frames == 0
            && match &result {
                Ok(()) => true,
                Err(error) => {
                    error.code == "truncated_sse_frame" && !looks_like_sse_prefix(&preview)
                }
            };
        let result = if no_stream {
            let detail = parse_error_body(&preview, false).and_then(|body| body.detail);
            Err(Error::with(
                "provider_expected_sse",
                [Some(format!("HTTP {status}, no content-type")), detail]
                    .into_iter()
                    .flatten()
                    .collect::<Vec<_>>()
                    .join(": "),
            ))
        } else {
            result
        };
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

impl Provider {
    /// A startup permit: held until the provider answers, so at most
    /// `--max-connecting` requests await their first response at once.
    async fn admit(&self) -> Result<tokio::sync::SemaphorePermit<'_>> {
        tokio::time::timeout(Duration::from_secs(60), self.transport.starting.acquire())
            .await
            .map_err(|_| Error::new("provider_admission_timeout"))?
            .map_err(|_| Error::new("provider_admission_closed"))
    }

    /// One call over the bot's socket. A continuation that the server no
    /// longer holds is sent again in full on the same connection; everything
    /// else fails as the HTTP path would.
    #[allow(clippy::too_many_arguments)]
    async fn complete_socket<F, Fut>(
        &self,
        sockets: &Sockets,
        request: Request<'_>,
        prefix: Vec<u8>,
        mut delta: F,
        report: &mut Report,
        (pace, mut reservation, estimate): (&pace::Pace, pace::Reservation<'_>, pace::Cost),
        admission: tokio::sync::SemaphorePermit<'_>,
    ) -> Result<Completion>
    where
        F: FnMut(Delta) -> Fut,
        Fut: Future<Output = Result<()>>,
    {
        let Request { items, chain, .. } = request;
        let (bot, window, tail) = match chain {
            Some(Chain { bot, window, tail }) => (Some(bot), window, Some(tail)),
            None => (None, None, None),
        };
        let key = socket::key(&prefix, window.map_or(&[][..], |(head, _)| head));
        let ids = window.map(|(_, ids)| ids);
        let mut session = match bot.and_then(|bot| sockets.take(bot)) {
            Some(session) => session,
            None => {
                let mut headers = Vec::with_capacity(2);
                let bearer = self.key.as_ref().map(|key| format!("Bearer {key}"));
                if let Some(bearer) = &bearer {
                    headers.push(("authorization", bearer.as_str()));
                }
                if let Some(account) = &self.account {
                    headers.push(("chatgpt-account-id", account.as_str()));
                }
                match sockets.connect(&self.url, &headers).await {
                    // The upgrade's headers predate this call, so they
                    // teach the pool without settling its reservation.
                    Ok((session, response)) => {
                        pace.seed(&response, self.family);
                        session
                    }
                    // A connection that never opened sent no request, and the
                    // reservation is refunded as it drops. A refused upgrade
                    // reached the provider: charge it nothing, but learn from
                    // its headers and honor a rate limit, as on HTTP.
                    Err(failure) => {
                        if failure.refused {
                            reservation.dispatch();
                            report.dispatched = true;
                            if let Some(headers) = &failure.headers {
                                reservation.learn(headers, self.family);
                            }
                            limit(pace, &failure);
                            reservation.settle(0);
                        }
                        return Err(failure.error);
                    }
                }
            }
        };
        let mut admission = Some(admission);
        let mut plan = session.plan(key, ids);
        let (mut items, mut tail) = (Some(items), tail);
        let mut parser = responses::State::default();
        let outcome = loop {
            let input = match (&plan.previous, tail.take()) {
                (Some(_), Some(tail)) => tail(plan.skip),
                _ => items.take().expect("the full input is sent at most once"),
            };
            // Each send holds its own undispatched reservation, refunded if
            // the input cannot be assembled.
            let text = create(&prefix, plan.previous.as_deref(), input).await?;
            reservation.dispatch();
            report.dispatched = true;
            match session
                .exchange(
                    text,
                    &mut parser,
                    &mut delta,
                    self.stall_timeout,
                    &mut admission,
                )
                .await
            {
                Err(failure)
                    if failure.error.code == "provider_previous_response_not_found"
                        && plan.previous.is_some()
                        && items.is_some() =>
                {
                    plan = socket::Plan {
                        previous: None,
                        skip: 0,
                    };
                    session.completed(None, key, ids);
                    parser = responses::State::default();
                    // The refused continuation ran no inference, but it was
                    // a request, so the full send is paced as another. It
                    // awaits a first event under a fresh startup permit.
                    reservation.settle(0);
                    let paced = report.paced_ms;
                    reservation = pace.acquire_reported(estimate, report).await?;
                    report.paced_ms += paced;
                    admission = Some(self.admit().await?);
                }
                outcome => break outcome,
            }
        };
        report.usage = parser.usage();
        let mut keep = bot.is_some();
        let completed = match outcome {
            Ok(()) => {
                session.completed(parser.id(), key, ids);
                reservation.settle_usage(report.usage.as_ref(), estimate);
                parser.finish()
            }
            Err(failure) => {
                if let Some(headers) = failure.headers.as_deref() {
                    reservation.learn(headers, self.family);
                }
                limit(pace, &failure);
                session.failed(&failure);
                keep &= !failure.dead;
                // A refusal ran no inference; anything else may have.
                if failure.refused && report.usage.is_none() {
                    reservation.settle(0);
                } else {
                    reservation.settle_usage(report.usage.as_ref(), estimate);
                }
                Err(failure.error)
            }
        };
        if let Some(bot) = bot.filter(|_| keep) {
            sockets.put(bot, session);
        }
        completed
    }
}

/// Close the model's pool for a rate limit a socket reported: a 429 or 529
/// status with its `retry-after`, as on HTTP, or an in-stream rate limit
/// naming its delay.
fn limit(pace: &pace::Pace, failure: &socket::Failure) {
    if matches!(failure.status, Some(429 | 529)) || failure.error.code == "provider_rate_limited" {
        let after = failure
            .headers
            .as_ref()
            .and_then(|headers| headers.get("retry-after"))
            .and_then(|v| v.to_str().ok())
            .and_then(|v| v.trim().parse::<f64>().ok())
            .and_then(|seconds| Duration::try_from_secs_f64(seconds).ok())
            .or_else(|| failure.error.detail.as_deref().and_then(pace::named_delay));
        pace.limited(after);
    }
}

/// A `response.create` event: the request fields, the continuation if any,
/// and the input. A socket message is whole, so the input is assembled here.
async fn create(prefix: &[u8], previous: Option<&str>, mut items: Items) -> Result<String> {
    let mut text = Vec::with_capacity(prefix.len() + items.bytes + 96);
    text.extend_from_slice(br#"{"type":"response.create","#);
    if let Some(previous) = previous {
        text.extend_from_slice(br#""previous_response_id":"#);
        serde_json::to_writer(&mut text, previous)?;
        text.push(b',');
    }
    text.extend_from_slice(&prefix[1..]);
    while let Some(chunk) = items.stream.next().await {
        text.extend_from_slice(&chunk.map_err(|error| Error {
            code: error.to_string(),
            detail: None,
        })?);
    }
    text.extend_from_slice(b"]}");
    String::from_utf8(text).map_err(|_| Error::new("invalid_item_encoding"))
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
    parse_error_body(&body, expects_json)
}

/// Recognize a partial event stream before treating an unnamed body as a
/// non-stream provider message. Called only after a frame-free clean EOF.
fn looks_like_sse_prefix(body: &[u8]) -> bool {
    body.split(|byte| *byte == b'\n')
        .map(|line| line.strip_suffix(b"\r").unwrap_or(line))
        .find(|line| !line.is_empty())
        .is_some_and(|line| {
            line.starts_with(b"data:")
                || line.starts_with(b"event:")
                || line.starts_with(b"id:")
                || line.starts_with(b"retry:")
                || line.starts_with(b":")
        })
}

/// A short provider message from a bounded body, or nothing safe to show.
fn parse_error_body(body: &[u8], expects_json: bool) -> Option<ErrorBody> {
    let text = std::str::from_utf8(body).ok()?.trim();
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
                chain: None,
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
                chain: None,
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
                chain: None,
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
                chain: None,
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
                            chain: None,
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
                chain: None,
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

    #[tokio::test]
    async fn an_account_rides_with_the_key_on_every_request() {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        let (seen, head) = tokio::sync::oneshot::channel();
        tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.unwrap();
            let mut request = Vec::new();
            let mut buffer = [0; 4096];
            while !request.windows(4).any(|w| w == b"\r\n\r\n") {
                let n = socket.read(&mut buffer).await.unwrap();
                request.extend_from_slice(&buffer[..n]);
            }
            let _ = seen.send(String::from_utf8_lossy(&request).to_lowercase());
            let _ = socket
                .write_all(b"HTTP/1.1 500 Internal Server Error\r\ncontent-length: 0\r\nconnection: close\r\n\r\n")
                .await;
        });
        let provider = Provider::new(
            Transport::new(0, 1).unwrap(),
            Family::Responses,
            &url,
            Some("synthetic-token".into()),
        )
        .unwrap()
        .with_account("synthetic-account".into())
        .unwrap();
        let tools = none();
        let request = Request {
            model: "m",
            instructions: "",
            reasoning: None,
            tools: &tools,
            allow_tool_calls: true,
            items: Items::empty(),
            chain: None,
        };
        let _ = provider.complete(request, |_| async { Ok(()) }).await;
        let head = head.await.unwrap();
        assert!(head.starts_with("post /responses "), "{head}");
        assert!(
            head.contains("\r\nauthorization: bearer synthetic-token\r\n"),
            "{head}"
        );
        assert!(
            head.contains("\r\nchatgpt-account-id: synthetic-account\r\n"),
            "{head}"
        );
        assert!(head.contains("\r\naccept: text/event-stream\r\n"), "{head}");
        assert!(head.contains("\r\nuser-agent: agent-runtime/"), "{head}");
        let anthropic = Provider::new(Transport::new(0, 1).unwrap(), Family::Anthropic, &url, None);
        assert!(anthropic.unwrap().with_account("w".into()).is_err());
    }

    #[tokio::test]
    async fn a_success_without_a_stream_names_what_came_instead() {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.unwrap();
            let _ = socket.read(&mut [0; 4096]).await;
            let body = r#"{"detail":"x","error":{"message":"Unsupported client"}}"#;
            let _ = socket
                .write_all(format!("HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}", body.len()).as_bytes())
                .await;
        });
        let provider =
            Provider::new(Transport::new(0, 1).unwrap(), Family::Responses, &url, None).unwrap();
        let tools = none();
        let request = Request {
            model: "m",
            instructions: "",
            reasoning: None,
            tools: &tools,
            allow_tool_calls: true,
            items: Items::empty(),
            chain: None,
        };
        let error = provider
            .complete(request, |_| async { Ok(()) })
            .await
            .unwrap_err();
        assert_eq!(error.code, "provider_expected_sse");
        assert_eq!(
            error.detail.as_deref(),
            Some("HTTP 200, content-type application/json: Unsupported client")
        );
    }

    /// Answer every connection: 401 unless the bearer is `accepted`, then a
    /// minimal Responses stream without a content type.
    async fn gated(accepted: &'static str) -> String {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        tokio::spawn(async move {
            loop {
                let (mut socket, _) = listener.accept().await.unwrap();
                let mut request = Vec::new();
                let mut buffer = [0; 4096];
                while !request.windows(4).any(|w| w == b"\r\n\r\n") {
                    let n = socket.read(&mut buffer).await.unwrap();
                    request.extend_from_slice(&buffer[..n]);
                }
                let head = String::from_utf8_lossy(&request).to_lowercase();
                let response = if head.contains(&format!("authorization: bearer {accepted}\r\n")) {
                    let body = concat!(
                        "data: {\"type\":\"response.output_text.delta\",\"delta\":\"ok\"}\n\n",
                        "data: {\"type\":\"response.completed\",\"response\":{\"status\":\"completed\",",
                        "\"output\":[{\"type\":\"message\",\"role\":\"assistant\",",
                        "\"content\":[{\"type\":\"output_text\",\"text\":\"ok\"}]}],",
                        "\"usage\":{\"input_tokens\":10,\"output_tokens\":2}}}\n\n",
                    );
                    format!(
                        "HTTP/1.1 200 OK\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
                        body.len()
                    )
                } else {
                    let body = r#"{"error":{"message":"token expired"}}"#;
                    format!(
                        "HTTP/1.1 401 Unauthorized\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
                        body.len()
                    )
                };
                let _ = socket.write_all(response.as_bytes()).await;
            }
        });
        url
    }

    #[tokio::test]
    async fn a_refused_login_is_reread_and_retried_only_when_the_file_changed() {
        let dir = std::env::temp_dir().join(format!("agent-relogin-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("auth.json");
        let write = |token: &str| {
            std::fs::write(
                &path,
                format!(r#"{{"tokens":{{"access_token":"{token}","account_id":"w"}}}}"#),
            )
            .unwrap();
        };
        write("stale");
        let credentials = crate::tools::Credentials::default();
        let login = Arc::new(login::Login::open(&path, Some(credentials.clone())).unwrap());
        let url = gated("fresh").await;
        let provider = Provider::new(Transport::new(0, 1).unwrap(), Family::Responses, &url, None)
            .unwrap()
            .with_login(login)
            .unwrap();
        let tools = none();
        let request = || Request {
            model: "m",
            instructions: "",
            reasoning: None,
            tools: &tools,
            allow_tool_calls: true,
            items: Items::empty(),
        };
        // Refused, and the file still holds the refused token: final, and
        // the detail names the file, never the token.
        let error = provider
            .complete(request(), |_| async { Ok(()) })
            .await
            .unwrap_err();
        assert_eq!(error.code, "provider_login_rejected");
        let detail = error.detail.unwrap();
        assert!(
            detail.starts_with("token expired; ") && detail.contains("auth.json"),
            "{detail}"
        );
        assert!(!detail.contains("stale"));
        // Codex signed in again: the refusal becomes a retryable error, and
        // the retry carries the new token, which is redacted from then on.
        write("fresh");
        let error = provider
            .complete(request(), |_| async { Ok(()) })
            .await
            .unwrap_err();
        assert_eq!(error.code, "provider_login_refreshed");
        assert_eq!(error.detail.as_deref(), Some("token expired"));
        let completion = provider
            .complete(request(), |_| async { Ok(()) })
            .await
            .unwrap();
        assert_eq!(completion.items.len(), 1);
        assert_eq!(
            credentials.redact("stale fresh".into()),
            "[REDACTED] [REDACTED]"
        );
        assert!(
            Provider::new(Transport::new(0, 1).unwrap(), Family::Anthropic, &url, None)
                .unwrap()
                .with_login(Arc::new(login::Login::open(&path, None).unwrap()))
                .is_err()
        );
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[tokio::test]
    async fn a_login_is_checked_after_startup_admission() {
        let dir =
            std::env::temp_dir().join(format!("agent-admission-login-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("auth.json");
        let write = |token: &str| {
            std::fs::write(
                &path,
                format!(r#"{{"tokens":{{"access_token":"{token}","account_id":"w"}}}}"#),
            )
            .unwrap();
        };
        write("stale");
        let login = Arc::new(login::Login::open(&path, None).unwrap());
        let transport = Transport::new(1, 1).unwrap();
        let admission = transport.starting.acquire().await.unwrap();
        let url = gated("fresh").await;
        let provider = Provider::new(transport.clone(), Family::Responses, &url, None)
            .unwrap()
            .with_login(login.clone())
            .unwrap();
        let tools = none();
        let request = Request {
            model: "m",
            instructions: "",
            reasoning: None,
            tools: &tools,
            allow_tool_calls: true,
            items: Items::empty(),
        };
        let call = provider.complete(request, |_| async { Ok(()) });
        tokio::pin!(call);
        std::future::poll_fn(|cx| {
            assert!(call.as_mut().poll(cx).is_pending());
            std::task::Poll::Ready(())
        })
        .await;
        write("fresh");
        login.expire_now();
        drop(admission);
        assert_eq!(call.await.unwrap().items.len(), 1);
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[tokio::test]
    async fn a_success_without_a_content_type_or_a_stream_names_what_it_held() {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.unwrap();
            let _ = socket.read(&mut [0; 4096]).await;
            let body = r#"{"error":{"message":"Unsupported client"}}"#;
            let _ = socket
                .write_all(
                    format!(
                        "HTTP/1.1 200 OK\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
                        body.len()
                    )
                    .as_bytes(),
                )
                .await;
        });
        let provider =
            Provider::new(Transport::new(0, 1).unwrap(), Family::Responses, &url, None).unwrap();
        let tools = none();
        let request = Request {
            model: "m",
            instructions: "",
            reasoning: None,
            tools: &tools,
            allow_tool_calls: true,
            items: Items::empty(),
        };
        let error = provider
            .complete(request, |_| async { Ok(()) })
            .await
            .unwrap_err();
        assert_eq!(error.code, "provider_expected_sse");
        assert_eq!(
            error.detail.as_deref(),
            Some("HTTP 200, no content-type: Unsupported client")
        );
    }

    #[tokio::test]
    async fn an_unnamed_stream_failure_before_the_first_frame_stays_retryable() {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        for (headers, expected) in [
            ("content-length: 100\r\n", "provider_stream_failed"),
            ("", "truncated_sse_frame"),
        ] {
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            let url = format!("http://{}", listener.local_addr().unwrap());
            tokio::spawn(async move {
                let (mut socket, _) = listener.accept().await.unwrap();
                let _ = socket.read(&mut [0; 4096]).await;
                let response = format!(
                    "HTTP/1.1 200 OK\r\n{headers}connection: close\r\n\r\ndata: incomplete"
                );
                let _ = socket.write_all(response.as_bytes()).await;
            });
            let provider =
                Provider::new(Transport::new(0, 1).unwrap(), Family::Responses, &url, None)
                    .unwrap();
            let tools = none();
            let request = Request {
                model: "m",
                instructions: "",
                reasoning: None,
                tools: &tools,
                allow_tool_calls: true,
                items: Items::empty(),
            };
            let error = provider
                .complete(request, |_| async { Ok(()) })
                .await
                .unwrap_err();
            assert_eq!(error.code, expected);
        }
    }

    #[tokio::test]
    async fn an_empty_unnamed_success_is_not_an_incomplete_model_response() {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.unwrap();
            let _ = socket.read(&mut [0; 4096]).await;
            let _ = socket
                .write_all(b"HTTP/1.1 200 OK\r\ncontent-length: 0\r\nconnection: close\r\n\r\n")
                .await;
        });
        let provider =
            Provider::new(Transport::new(0, 1).unwrap(), Family::Responses, &url, None).unwrap();
        let tools = none();
        let request = Request {
            model: "m",
            instructions: "",
            reasoning: None,
            tools: &tools,
            allow_tool_calls: true,
            items: Items::empty(),
        };
        let error = provider
            .complete(request, |_| async { Ok(()) })
            .await
            .unwrap_err();
        assert_eq!(error.code, "provider_expected_sse");
    }

    #[tokio::test]
    async fn a_stream_that_names_no_content_type_is_still_read() {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.unwrap();
            let _ = socket.read(&mut [0; 4096]).await;
            let body = concat!(
                "data: {\"type\":\"response.output_text.delta\",\"delta\":\"ok\"}\n\n",
                "data: {\"type\":\"response.completed\",\"response\":{\"status\":\"completed\",",
                "\"output\":[{\"type\":\"message\",\"role\":\"assistant\",",
                "\"content\":[{\"type\":\"output_text\",\"text\":\"ok\"}]}],",
                "\"usage\":{\"input_tokens\":10,\"output_tokens\":2}}}\n\n",
            );
            let _ = socket
                .write_all(
                    format!(
                        "HTTP/1.1 200 OK\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
                        body.len()
                    )
                    .as_bytes(),
                )
                .await;
        });
        let provider =
            Provider::new(Transport::new(0, 1).unwrap(), Family::Responses, &url, None).unwrap();
        let tools = none();
        let request = Request {
            model: "m",
            instructions: "",
            reasoning: None,
            tools: &tools,
            allow_tool_calls: true,
            items: Items::empty(),
            chain: None,
        };
        let completion = provider
            .complete(request, |_| async { Ok(()) })
            .await
            .unwrap();
        assert_eq!(completion.items.len(), 1);
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
