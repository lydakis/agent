//! Streaming model calls. One shared HTTP transport; per-family request
//! encoding and SSE parsing. History items are streamed by reference.
use crate::{Error, Result, codec::Family, fail, sse::Decoder};
use bytes::Bytes;
use futures_util::{Stream, StreamExt, stream};
use serde::Serialize;
use serde_json::{Value, json, value::RawValue};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::{future::Future, sync::Arc, time::Duration};
use tokio::sync::Semaphore;

mod anthropic;
pub mod aws;
pub mod login;
pub mod pace;
mod responses;
mod socket;

pub use pace::Report;
pub use socket::Sockets;

/// JSON-encoded bytes one response may stream. A full 128,000-token Claude
/// answer runs about 512 KiB; the bound stays under the 1 MiB event cap so
/// every stored item and live event remains publishable and readable.
pub const MAX_OUTPUT: usize = 768 * 1024;

/// The bytes `text` takes as a JSON string body, as serde_json escapes it.
/// Decoded text can grow up to sixfold when encoded, so output bounds count
/// this rather than the decoded length.
pub(crate) fn encoded_len(text: &str) -> usize {
    text.len()
        + text
            .bytes()
            .map(|b| match b {
                b'"' | b'\\' | b'\n' | b'\r' | b'\t' | 0x08 | 0x0c => 1,
                0..=0x1f => 5,
                _ => 0,
            })
            .sum::<usize>()
}

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

/// Default idle time after which an Anthropic prompt cache is refreshed while
/// a turn runs a tool: under the cache's five-minute lifetime, with a minute's
/// margin for the request itself.
pub const KEEP_WARM: Duration = Duration::from_secs(240);
/// The longest a cache refresh waits for its pool: past this it would land
/// after the cache it was sent to keep had expired.
const KEEP_WARM_WAIT: Duration = Duration::from_secs(30);

/// Betas every Anthropic API request opts into: the thinking-binding check,
/// dropping rather than failing on a mismatch (the drops are reported), and
/// server-side fallbacks, which rerun a declined request on another model
/// instead of ending the turn with a refusal. Bedrock takes only the first.
const ANTHROPIC_BETAS: &str =
    "thinking-binding-controls-2026-08-01,server-side-fallback-2026-07-01";
const BEDROCK_BETAS: &str = "thinking-binding-controls-2026-08-01";

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
    /// Refresh an idle Anthropic prompt cache after this long; `None` disables.
    keep_warm: Option<Duration>,
    /// Responses over WebSocket, one connection per bot, instead of HTTP.
    sockets: Option<Arc<Sockets>>,
    /// Bedrock signs every request with SigV4 instead of sending a key.
    aws: Option<Arc<aws::Aws>>,
    /// A Bedrock endpoint, however it authenticates: it runs no server-side
    /// fallbacks and serves no WebSocket.
    bedrock: bool,
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
    /// Input tokens written to the provider's prompt cache, which Anthropic
    /// bills above the base input rate. Part of `input_tokens`, like cache
    /// reads; zero for providers that do not bill writes.
    #[serde(skip_serializing_if = "is_zero")]
    pub cache_write_tokens: u64,
    /// The billed attempts, when a provider-side fallback ran more than one
    /// model for the call, or the summarizer's model on a compaction call,
    /// so each can be priced at its model's rates. The totals above are
    /// their sum.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub models: Vec<ModelTokens>,
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ModelTokens {
    pub model: String,
    /// The provider binding that ran it, when that may not be the turn's:
    /// set on summarizer calls, which can use another provider.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub provider: Option<String>,
    pub input_tokens: u64,
    pub output_tokens: u64,
    pub cached_input_tokens: u64,
    #[serde(skip_serializing_if = "is_zero")]
    pub cache_write_tokens: u64,
}
fn is_zero(n: &u64) -> bool {
    *n == 0
}
#[derive(Debug)]
pub struct Completion {
    pub items: Vec<Bytes>,
    pub calls: Vec<ToolCall>,
    pub usage: Option<Usage>,
    /// Replayed thinking blocks the provider dropped because the history
    /// before them changed. The runtime avoids this, so any is a bug.
    pub thinking_dropped: usize,
    /// Each model switch a provider-side fallback made, as (from, to).
    pub fallbacks: Vec<(Option<String>, String)>,
}
#[derive(Debug, Clone, PartialEq, Eq, serde::Deserialize, serde::Serialize)]
pub struct ToolCall {
    pub name: String,
    pub call_id: String,
    pub arguments: String,
}
pub type ItemStream = futures_util::stream::BoxStream<'static, std::io::Result<Bytes>>;
/// The conversation items of a request: pre-encoded, comma-separated items
/// of known total length, read as a stream so the body is never assembled in
/// memory. Each stream reads them from the start, so a signer that digests
/// the body can read it once before it is sent.
pub struct Items {
    /// Exact byte length every stream yields.
    pub bytes: usize,
    open: Box<dyn Fn() -> ItemStream + Send + Sync>,
}
impl Items {
    pub fn new(bytes: usize, open: impl Fn() -> ItemStream + Send + Sync + 'static) -> Items {
        Items {
            bytes,
            open: Box::new(open),
        }
    }
    pub fn empty() -> Items {
        Items::new(0, || stream::empty().boxed())
    }
    pub fn stream(&self) -> ItemStream {
        (self.open)()
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
    /// Groups calls that share a prefix for the Responses prompt cache. All
    /// bots share their leading instructions, so without a key their calls
    /// route by that prefix alone, pile onto the same cache machines and
    /// spill; the Messages API has no such field.
    pub cache_key: Option<&'a str>,
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
        let bedrock = aws::endpoint(&url).is_some();
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
            keep_warm: Some(KEEP_WARM),
            sockets: None,
            aws: None,
            bedrock,
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
        // Bedrock serves Responses over HTTP only.
        if self.family != Family::Responses || self.bedrock {
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
    pub fn output_byte_estimate(&self, model: &str) -> Option<usize> {
        match self.family {
            Family::Responses => self.max_output_tokens,
            Family::Anthropic => Some(self.anthropic_max_tokens(model)),
        }
        .map(|tokens| (tokens as usize).saturating_mul(4))
    }
    /// Anthropic's `max_tokens`: `--max-output-tokens` when set, else the
    /// model's full output limit.
    fn anthropic_max_tokens(&self, model: &str) -> u32 {
        self.max_output_tokens
            .unwrap_or_else(|| anthropic_max_tokens(model))
    }
    /// How much longer this model's pool is closed by a rate limit, if it is.
    pub fn blocked_for(&self, model: &str) -> Option<std::time::Duration> {
        self.pools.get(&self.family.pool_key(model)).blocked_for()
    }

    /// Bound generated tokens, reasoning included. Anthropic calls send it
    /// as `max_tokens` in place of the model's full output limit, and need room for a thinking
    /// budget of at least 1,024 tokens beside the answer. Bedrock deducts
    /// input plus this bound from quota when a call starts, so a bound near
    /// real output is throughput there.
    pub fn with_max_output_tokens(mut self, limit: u32) -> Result<Self> {
        if limit == 0 || (self.family == Family::Anthropic && limit < 2048) {
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

    /// Sign every request for this Bedrock endpoint with SigV4, in place of
    /// a key; the URL must be an https Bedrock host of the same region and
    /// service, since the signature and session token travel with it.
    pub fn with_aws(mut self, aws: Arc<aws::Aws>) -> Result<Self> {
        if aws::endpoint(&self.url) != Some((aws.region().to_owned(), aws.service()))
            || self.url.scheme() != "https"
            || self.key.is_some()
            || self.login.is_some()
        {
            return fail("invalid_provider_auth");
        }
        self.aws = Some(aws);
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

    /// Refresh an idle prompt cache after `after`, under its five-minute
    /// lifetime; `None` disables it.
    pub fn with_keep_warm(mut self, after: Option<Duration>) -> Result<Self> {
        if after.is_some_and(|after| after.is_zero() || after >= Duration::from_secs(300)) {
            return fail("invalid_keep_warm");
        }
        self.keep_warm = after;
        Ok(self)
    }

    /// How long a turn may sit on a tool before its prompt cache is refreshed
    /// with [`Provider::keep_warm`], or `None` where that does not apply.
    /// Anthropic's own API only: a request with `max_tokens: 0` generates
    /// nothing, bills a cache read, and restarts the cache's lifetime. It
    /// rejects budgeted thinking, so older models with thinking on are left
    /// out, and Bedrock is left out until the same request is verified there.
    /// The Responses cache outlives a long tool call without help.
    pub fn keep_warm_after(&self, model: &str, reasoning: Option<&str>) -> Option<Duration> {
        let applies = self.family == Family::Anthropic
            && !self.bedrock
            && !(reasoning.is_some() && legacy_thinking(model));
        self.keep_warm.filter(|_| applies)
    }

    /// Everything before the history array, ending with `[`.
    fn prefix(&self, request: &Request<'_>) -> Result<Vec<u8>> {
        self.prefix_for(request, false)
    }

    /// The prefix of `request`, or with `warm` of the same request sent only
    /// to refresh its cache: no output and no stream. Nothing else differs,
    /// since the cache is keyed on everything the request renders.
    fn prefix_for(&self, request: &Request<'_>, warm: bool) -> Result<Vec<u8>> {
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
            #[serde(skip_serializing_if = "Option::is_none")]
            prompt_cache_key: Option<&'a str>,
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
            /// A request a safety classifier declines is rerun on the model
            /// Anthropic recommends for that refusal category. Bedrock does
            /// not run server-side fallbacks.
            #[serde(skip_serializing_if = "Option::is_none")]
            fallbacks: Option<&'static str>,
        }
        let disable_tools = !request.allow_tool_calls && request.tools.get() != "[]";
        let max_tokens = self.anthropic_max_tokens(request.model);
        let (mut bytes, field) = match self.family {
            Family::Responses if warm => return fail("keep_warm_unsupported"),
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
                    prompt_cache_key: request.cache_key,
                })?,
                &b",\"input\":["[..],
            ),
            Family::Anthropic => (
                serde_json::to_vec(&Anthropic {
                    model: request.model,
                    max_tokens: if warm { 0 } else { max_tokens },
                    // An explicit breakpoint after the static prefix (tools
                    // and instructions) guarantees a read point for it.
                    system: (!request.instructions.is_empty()).then(|| {
                        json!([{"type":"text","text":request.instructions,
                            "cache_control":{"type":"ephemeral"}}])
                    }),
                    stream: !warm,
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
                            }
                            .min(max_tokens - 1024);
                            json!({"type":"enabled","budget_tokens":budget,
                                "block_binding":{"prefix_mismatch_behavior":"drop_block"}})
                        } else {
                            json!({"type":"adaptive","display":"summarized",
                                "block_binding":{"prefix_mismatch_behavior":"drop_block"}})
                        }
                    }),
                    output_config: request
                        .reasoning
                        .filter(|_| !legacy_thinking(request.model))
                        .map(|level| json!({"effort":level})),
                    fallbacks: (!self.bedrock).then_some("default"),
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
    fn body(&self, prefix: Bytes, items: &Items) -> (reqwest::Body, usize) {
        let len = prefix.len() + items.bytes + 2;
        (reqwest::Body::wrap_stream(framed(prefix, items)), len)
    }

    /// Send `request` again only to restart its prompt cache's lifetime:
    /// no output and no stream, so it bills a read of the cache the previous
    /// call left. Paced and admitted like any call, but it gives up rather
    /// than wait past the cache's lifetime for a closed or busy pool.
    /// `dispatched` turns true as the request is sent: from then on it is
    /// billed, and dropping it loses what it cost.
    pub async fn keep_warm(
        &self,
        request: Request<'_>,
        dispatched: &std::sync::atomic::AtomicBool,
    ) -> Result<Usage> {
        self.keep_warm_inner(request, dispatched)
            .await
            .map_err(|error| sanitize_error(error, self.key.as_deref()))
    }

    async fn keep_warm_inner(
        &self,
        request: Request<'_>,
        dispatched: &std::sync::atomic::AtomicBool,
    ) -> Result<Usage> {
        if self
            .keep_warm_after(request.model, request.reasoning)
            .is_none()
        {
            return fail("keep_warm_unsupported");
        }
        let pace = self.pools.get(&self.family.pool_key(request.model));
        let prefix = Bytes::from(self.prefix_for(&request, true)?);
        let estimate = pace::Cost {
            input: ((prefix.len() + request.items.bytes) / 4) as u64,
            output: 0,
        };
        let mut park_for = None;
        let mut reservation =
            tokio::time::timeout(KEEP_WARM_WAIT, pace.acquire_cost(estimate, &mut park_for))
                .await
                .map_err(|_| Error::new("provider_paced"))??;
        let admission = self.admit().await?;
        let (body, len) = self.body(prefix, &request.items);
        let (client, _lease) = self.transport.lease();
        let http = client
            .post(self.url.clone())
            .header("content-type", "application/json")
            .header("content-length", len)
            .header("accept", "application/json")
            .body(body);
        let http = self.anthropic_headers(http, self.key.as_ref());
        reservation.dispatch();
        dispatched.store(true, std::sync::atomic::Ordering::Relaxed);
        let response = match http.send().await {
            Ok(response) => response,
            Err(error) => {
                reservation.settle(0);
                return Err(connection_error(error));
            }
        };
        drop(admission);
        let status = response.status().as_u16();
        if !response.status().is_success() {
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
            return Err(Error {
                code: if quota {
                    "provider_quota_exhausted".to_owned()
                } else {
                    format!("provider_http_{status}")
                },
                detail: body.detail,
            });
        }
        reservation.learn(response.headers(), self.family);
        // The answer is a message with no content; only its usage matters.
        let mut stream = response.bytes_stream();
        let mut body = Vec::new();
        while let Some(chunk) = stream.next().await {
            let chunk = chunk.map_err(|_| Error::new("provider_stream_failed"))?;
            if chunk.len() > 64 * 1024 - body.len() {
                reservation.settle(0);
                return fail("provider_response_limit");
            }
            body.extend_from_slice(&chunk);
        }
        let message: Value = serde_json::from_slice(&body)
            .map_err(|_| Error::with("invalid_provider_response", "keep-warm body"))?;
        let usage = anthropic::usage(&message["usage"]);
        reservation.settle_usage(Some(&usage), estimate);
        Ok(usage)
    }

    /// Version, betas, and key for an Anthropic request.
    fn anthropic_headers(
        &self,
        http: reqwest::RequestBuilder,
        key: Option<&String>,
    ) -> reqwest::RequestBuilder {
        let http = http.header("anthropic-version", "2023-06-01").header(
            "anthropic-beta",
            if self.bedrock {
                BEDROCK_BETAS
            } else {
                ANTHROPIC_BETAS
            },
        );
        match key {
            Some(key) => http.header("x-api-key", key),
            None => http,
        }
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
        let prefix = Bytes::from(self.prefix(&request)?);
        let estimate = pace::Cost {
            input: ((prefix.len() + request.items.bytes) / 4) as u64,
            output: u64::from(self.max_output_tokens.unwrap_or(512)),
        };
        let mut reservation = pace.acquire_reported(estimate, report).await?;
        // Bound request startup until response headers arrive; release before
        // reading SSE so established streams are not capped at this limit.
        let admission = self.admit().await?;
        // Bedrock Runtime signs the body's digest, so its items are read once
        // to hash them and again as they stream; Mantle takes the body
        // unsigned over TLS and reads it once. Hashed under admission, which
        // already covers sending the body, so the extra read is bounded too.
        let payload = match &self.aws {
            Some(aws) if aws.signs_payload() => {
                Some(aws::payload(framed(prefix.clone(), &request.items)).await?)
            }
            _ => None,
        };
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
        let cache_key = request.cache_key;
        let (body, len) = self.body(prefix, &request.items);
        // The lease lives until this function returns, stream included.
        let (client, _lease) = self.transport.lease();
        // A token can expire while pacing or waiting for admission. Read it
        // only now, before building the headers that will carry it.
        if let Some(login) = &self.login {
            *session = Some(login.current()?);
        }
        let signer = match &self.aws {
            Some(aws) => Some((aws, aws.current().await?)),
            None => None,
        };
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
            (Family::Anthropic, key) => self.anthropic_headers(http, key),
            (Family::Responses, None) => http,
        };
        if let Some(account) = account {
            http = http.header("chatgpt-account-id", account);
        }
        // The ChatGPT Codex endpoint takes cache affinity from this header, not
        // from prompt_cache_key (openai/codex 53446f9, core/src/client.rs).
        if let (Family::Responses, Some(key)) = (self.family, cache_key) {
            http = http.header("session-id", key);
        }
        // Signed last, at send time: the signature covers the moment it is made.
        if let Some((aws, keys)) = &signer {
            let payload = payload.as_deref().unwrap_or(aws::UNSIGNED_PAYLOAD);
            for (name, value) in aws.sign(
                keys,
                "POST",
                &self.url,
                std::time::SystemTime::now(),
                payload,
            ) {
                http = http.header(name, value);
            }
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
            // Expired or replaced AWS keys are re-resolved once, as a login is.
            if matches!(status, 401 | 403)
                && let Some((aws, keys)) = &signer
                && aws.reload(keys).await?
            {
                reservation.settle(0);
                return Err(Error::with(
                    "provider_login_refreshed",
                    body.detail.unwrap_or_else(|| format!("HTTP {status}")),
                ));
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
        // Only a completed stream ends a refusal streak: a 200 can still end
        // in an in-stream rate limit.
        parser
            .finish()
            .inspect(|_| pace.accepted())
            .inspect_err(|error| {
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
        prefix: Bytes,
        mut delta: F,
        report: &mut Report,
        (pace, mut reservation, estimate): (&pace::Pace, pace::Reservation<'_>, pace::Cost),
        admission: tokio::sync::SemaphorePermit<'_>,
    ) -> Result<Completion>
    where
        F: FnMut(Delta) -> Fut,
        Fut: Future<Output = Result<()>>,
    {
        let Request {
            items,
            chain,
            cache_key,
            ..
        } = request;
        let (bot, window, tail) = match chain {
            Some(Chain { bot, window, tail }) => (Some(bot), window, Some(tail)),
            None => (None, None, None),
        };
        let key = socket::key(&prefix, window.map_or(&[][..], |(head, _)| head));
        let ids = window.map(|(_, ids)| ids);
        let mut session = match bot.and_then(|bot| sockets.take(bot)) {
            Some(session) => session,
            None => {
                let mut headers = Vec::with_capacity(3);
                let bearer = self.key.as_ref().map(|key| format!("Bearer {key}"));
                if let Some(bearer) = &bearer {
                    headers.push(("authorization", bearer.as_str()));
                }
                if let Some(account) = &self.account {
                    headers.push(("chatgpt-account-id", account.as_str()));
                }
                // Cache affinity, as on HTTP; the connection is the bot's own.
                if let Some(key) = cache_key {
                    headers.push(("session-id", key));
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
                parser.finish().inspect(|_| pace.accepted())
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
/// A request body as it is sent: the prefix, the items and the close. Empty
/// chunks (an empty context prefix, a batch of thinking-only items) are
/// dropped: each would be an empty HTTP/2 DATA frame, which Bedrock answers
/// with GOAWAY FRAME_SIZE_ERROR.
fn framed(
    prefix: Bytes,
    items: &Items,
) -> impl Stream<Item = std::io::Result<Bytes>> + Send + 'static {
    stream::iter([Ok(prefix)])
        .chain(items.stream())
        .chain(stream::iter([Ok(Bytes::from_static(b"]}"))]))
        .filter(|chunk| std::future::ready(!matches!(chunk, Ok(bytes) if bytes.is_empty())))
}

async fn create(prefix: &[u8], previous: Option<&str>, items: Items) -> Result<String> {
    let mut text = Vec::with_capacity(prefix.len() + items.bytes + 96);
    text.extend_from_slice(br#"{"type":"response.create","#);
    if let Some(previous) = previous {
        text.extend_from_slice(br#""previous_response_id":"#);
        serde_json::to_writer(&mut text, previous)?;
        text.push(b',');
    }
    text.extend_from_slice(&prefix[1..]);
    let mut stream = items.stream();
    while let Some(chunk) = stream.next().await {
        text.extend_from_slice(&chunk.map_err(|error| Error {
            code: error.to_string(),
            detail: None,
        })?);
    }
    text.extend_from_slice(b"]}");
    String::from_utf8(text).map_err(|_| Error::new("invalid_item_encoding"))
}

/// The Claude model a Bedrock id names: `anthropic.claude-…` and a
/// geographic or global profile such as `us.anthropic.claude-…` or
/// `global.anthropic.claude-…` name `claude-…`. Other ids are unchanged.
fn claude_model(model: &str) -> &str {
    if let Some(name) = model.strip_prefix("anthropic.") {
        return name;
    }
    model
        .split_once('.')
        .filter(|(profile, _)| {
            !profile.is_empty() && profile.bytes().all(|b| b.is_ascii_lowercase() || b == b'-')
        })
        .and_then(|(_, rest)| rest.strip_prefix("anthropic."))
        .unwrap_or(model)
}

/// The model's full output limit. A lower cap fails any answer that runs
/// past it, and Anthropic counts only generated tokens against output rate
/// limits, so the full limit costs nothing until it is used.
/// https://platform.claude.com/docs/en/about-claude/models/overview
/// Bedrock ids are read for the Claude model they name.
fn anthropic_max_tokens(model: &str) -> u32 {
    let model = claude_model(model);
    const LIMITS: [(&str, u32); 12] = [
        ("claude-haiku-4-5", 64_000),
        ("claude-opus-4-5", 64_000),
        ("claude-sonnet-4-5", 64_000),
        ("claude-sonnet-4-0", 64_000),
        ("claude-sonnet-4-2025", 64_000),
        ("claude-3-7-sonnet", 64_000),
        ("claude-opus-4-1", 32_000),
        ("claude-opus-4-0", 32_000),
        ("claude-opus-4-2025", 32_000),
        ("claude-3-5-", 8_192),
        // Every other Claude 3 and Claude 2 model; order matters above.
        ("claude-3-", 4_096),
        ("claude-2", 4_096),
    ];
    // Claude 4.6 and every later model generate up to 128,000 tokens.
    LIMITS
        .iter()
        .find(|(prefix, _)| model.starts_with(prefix))
        .map_or(128_000, |&(_, limit)| limit)
}

/// Model ids that predate adaptive thinking and still require a token budget.
fn legacy_thinking(model: &str) -> bool {
    let model = claude_model(model);
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
    fn encoded_len_matches_serde_escaping() {
        let text: String = (0u8..0x80)
            .map(char::from)
            .chain("é\u{2028}😀".chars())
            .collect();
        assert_eq!(
            encoded_len(&text),
            serde_json::to_string(&text).unwrap().len() - 2
        );
    }

    #[test]
    fn anthropic_requests_ask_for_the_models_full_output_limit() {
        for (model, limit) in [
            ("claude-sonnet-5", 128_000),
            ("claude-opus-5-5", 128_000),
            ("claude-sonnet-4-6", 128_000),
            ("claude-opus-4-6", 128_000),
            ("claude-haiku-4-5-20251001", 64_000),
            ("claude-sonnet-4-5-20250929", 64_000),
            ("claude-opus-4-5", 64_000),
            ("claude-sonnet-4-20250514", 64_000),
            ("claude-opus-4-1-20250805", 32_000),
            ("claude-opus-4-20250514", 32_000),
            ("claude-3-7-sonnet-20250219", 64_000),
            ("claude-3-5-haiku-20241022", 8_192),
            ("claude-3-haiku-20240307", 4_096),
            ("claude-3-sonnet-20240229", 4_096),
            ("claude-3-opus-20240229", 4_096),
            ("claude-2.1", 4_096),
            // Bedrock ids name the same models.
            ("anthropic.claude-haiku-4-5-20251001-v1:0", 64_000),
            ("us.anthropic.claude-sonnet-4-5-20250929-v1:0", 64_000),
            ("global.anthropic.claude-sonnet-5", 128_000),
            ("anthropic.claude-3-haiku-20240307-v1:0", 4_096),
        ] {
            assert_eq!(anthropic_max_tokens(model), limit, "{model}");
        }
        let provider = Provider::new(
            Transport::new(64, 1).unwrap(),
            Family::Anthropic,
            "https://api.example.test",
            None,
        )
        .unwrap();
        assert_eq!(
            provider.output_byte_estimate("claude-sonnet-5"),
            Some(512_000)
        );
        // A budget must stay below max_tokens on small legacy models.
        let mut prefix = provider
            .prefix(&Request {
                model: "claude-3-5-haiku-20241022",
                instructions: "",
                reasoning: Some("high"),
                tools: &none(),
                allow_tool_calls: true,
                cache_key: None,
                items: Items::empty(),
                chain: None,
            })
            .unwrap();
        prefix.extend_from_slice(b"]}");
        let body: Value = serde_json::from_slice(&prefix).unwrap();
        assert_eq!(body["max_tokens"], 8192);
        assert_eq!(body["thinking"]["budget_tokens"], 7168);
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
                cache_key: Some("k"),
                items: Items::empty(),
                chain: None,
            })
            .unwrap();
        let text = String::from_utf8(prefix).unwrap();
        assert!(!text.contains("prompt_cache_key"));
        assert!(text.ends_with(",\"messages\":["));
        assert!(text.contains("\"type\":\"adaptive\""));
        assert!(text.contains("\"block_binding\":{\"prefix_mismatch_behavior\":\"drop_block\"}"));
        assert!(text.contains("\"fallbacks\":\"default\""));
        assert_eq!(text.matches("\"cache_control\"").count(), 2);
        assert!(text.contains("\"effort\":\"low\""));
        let legacy = provider
            .prefix(&Request {
                model: "claude-haiku-4-5-20251001",
                instructions: "i",
                reasoning: Some("low"),
                tools: &none(),
                allow_tool_calls: true,
                cache_key: None,
                items: Items::empty(),
                chain: None,
            })
            .unwrap();
        let legacy = String::from_utf8(legacy).unwrap();
        assert!(legacy.contains("\"budget_tokens\":2048"));
        assert!(legacy.contains("\"max_tokens\":64000"));
        assert!(text.contains("\"max_tokens\":128000"));
        assert!(legacy.contains("\"prefix_mismatch_behavior\":\"drop_block\""));
        assert!(legacy.contains("\"fallbacks\":\"default\""));
        assert!(!legacy.contains("output_config"));
        assert!(!text.contains("budget_tokens"));
        let mut empty_prefix = provider
            .prefix(&Request {
                model: "m",
                instructions: "",
                reasoning: None,
                tools: &none(),
                allow_tool_calls: true,
                cache_key: None,
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
        assert!(provider.clone().with_max_output_tokens(2047).is_err());
        let responses = responses.with_max_output_tokens(2048).unwrap();
        let mut prefix = responses
            .prefix(&Request {
                model: "m",
                instructions: "i",
                reasoning: None,
                tools: &none(),
                allow_tool_calls: true,
                cache_key: Some("k"),
                items: Items::empty(),
                chain: None,
            })
            .unwrap();
        prefix.extend_from_slice(b"]}");
        let body: Value = serde_json::from_slice(&prefix).unwrap();
        assert_eq!(body["max_output_tokens"], 2048);
        assert_eq!(body["prompt_cache_key"], "k");
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
                            cache_key: None,
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

    /// Bedrock ids name Claude models under a vendor prefix and, on runtime,
    /// a geographic or global profile; capability rules read the model.
    #[test]
    fn bedrock_model_ids_follow_the_claude_model_they_name() {
        for (id, legacy) in [
            ("anthropic.claude-haiku-4-5", true),
            ("us.anthropic.claude-haiku-4-5-20251001-v1:0", true),
            ("global.anthropic.claude-sonnet-4-5-20250929-v1:0", true),
            ("us-gov.anthropic.claude-3-7-sonnet-20250219-v1:0", true),
            ("anthropic.claude-opus-5", false),
            ("global.anthropic.claude-opus-5", false),
            ("eu.anthropic.claude-sonnet-4-6", false),
            ("claude-haiku-4-5", true),
            ("claude-opus-5", false),
        ] {
            assert_eq!(legacy_thinking(id), legacy, "{id}");
        }
        assert_eq!(claude_model("openai.gpt-6-sol"), "openai.gpt-6-sol");
        assert_eq!(
            claude_model("US.anthropic.claude-x"),
            "US.anthropic.claude-x"
        );
    }

    /// An output cap reaches Anthropic calls as `max_tokens`, and a legacy
    /// thinking budget stays inside it with room for the answer.
    #[test]
    fn anthropic_output_cap_bounds_max_tokens_and_thinking_budget() {
        let transport = Transport::new(64, 1).unwrap();
        let provider = Provider::new(
            transport,
            Family::Anthropic,
            "https://bedrock-mantle.us-east-1.api.aws/anthropic/v1",
            None,
        )
        .unwrap()
        .with_max_output_tokens(4096)
        .unwrap();
        assert_eq!(
            provider.output_byte_estimate("claude-sonnet-5"),
            Some(4096 * 4)
        );
        let body = |model: &str| {
            let mut prefix = provider
                .prefix(&Request {
                    model,
                    instructions: "i",
                    reasoning: Some("high"),
                    tools: &none(),
                    allow_tool_calls: true,
                    cache_key: None,
                    items: Items::empty(),
                    chain: None,
                })
                .unwrap();
            prefix.extend_from_slice(b"]}");
            serde_json::from_slice::<Value>(&prefix).unwrap()
        };
        let legacy = body("us.anthropic.claude-haiku-4-5-20251001-v1:0");
        assert_eq!(legacy["max_tokens"], 4096);
        assert_eq!(legacy["thinking"]["budget_tokens"], 3072);
        let current = body("anthropic.claude-opus-5");
        assert_eq!(current["max_tokens"], 4096);
        assert_eq!(current["thinking"]["type"], "adaptive");
    }

    #[test]
    fn bedrock_bindings_refuse_websocket_cleartext_and_mismatched_signers() {
        let transport = Transport::new(64, 1).unwrap();
        let mantle = "https://bedrock-mantle.us-east-1.api.aws/openai/v1";
        let bedrock = || Provider::new(transport.clone(), Family::Responses, mantle, None).unwrap();
        assert!(bedrock().with_socket().is_err());
        let keys = || aws::Keys::new("AKIDEXAMPLE".into(), "secret".into(), None);
        let signer = |region: &str, service| Arc::new(aws::Aws::fixed(region, service, keys()));
        assert!(
            bedrock()
                .with_aws(signer("us-east-1", "bedrock-mantle"))
                .is_ok()
        );
        assert!(
            bedrock()
                .with_aws(signer("us-west-2", "bedrock-mantle"))
                .is_err()
        );
        assert!(bedrock().with_aws(signer("us-east-1", "bedrock")).is_err());
        let cleartext = Provider::new(
            transport.clone(),
            Family::Responses,
            "http://bedrock-mantle.us-east-1.api.aws/openai/v1",
            None,
        )
        .unwrap();
        assert!(
            cleartext
                .with_aws(signer("us-east-1", "bedrock-mantle"))
                .is_err()
        );
        let keyed = Provider::new(
            transport.clone(),
            Family::Responses,
            mantle,
            Some("k".into()),
        )
        .unwrap();
        assert!(
            keyed
                .with_aws(signer("us-east-1", "bedrock-mantle"))
                .is_err()
        );
        let first_party = Provider::new(
            transport.clone(),
            Family::Responses,
            "https://api.openai.com/v1",
            None,
        )
        .unwrap();
        assert!(
            first_party
                .with_aws(signer("us-east-1", "bedrock-mantle"))
                .is_err()
        );
    }

    /// Bedrock runs no server-side fallbacks, so its requests do not ask for
    /// them, whichever endpoint and however they authenticate.
    #[test]
    fn bedrock_requests_ask_for_no_server_side_fallbacks() {
        let transport = Transport::new(64, 1).unwrap();
        for (url, key) in [
            (
                "https://bedrock-mantle.us-east-1.api.aws/anthropic/v1",
                None,
            ),
            (
                "https://bedrock-runtime.us-east-1.amazonaws.com/anthropic/v1",
                Some("k".to_owned()),
            ),
        ] {
            let provider = Provider::new(transport.clone(), Family::Anthropic, url, key).unwrap();
            let prefix = provider
                .prefix(&Request {
                    model: "anthropic.claude-sonnet-5",
                    instructions: "i",
                    reasoning: Some("high"),
                    tools: &none(),
                    allow_tool_calls: true,
                    cache_key: None,
                    items: Items::empty(),
                    chain: None,
                })
                .unwrap();
            let text = String::from_utf8(prefix).unwrap();
            assert!(!text.contains("fallbacks"), "{text}");
            assert!(text.contains("\"block_binding\""), "{text}");
        }
    }

    /// A refresh is the call's own request with no output and no stream:
    /// anything else it rendered differently would miss the call's cache.
    #[test]
    fn a_keep_warm_request_differs_only_in_output_and_streaming() {
        let transport = Transport::new(64, 1).unwrap();
        let provider = Provider::new(
            transport,
            Family::Anthropic,
            "https://api.anthropic.com/v1",
            None,
        )
        .unwrap();
        let tools = none();
        let request = || Request {
            model: "claude-sonnet-5",
            instructions: "i",
            reasoning: Some("high"),
            tools: &tools,
            allow_tool_calls: true,
            cache_key: None,
            items: Items::empty(),
            chain: None,
        };
        let parse = |mut bytes: Vec<u8>| -> Value {
            bytes.extend_from_slice(b"]}");
            serde_json::from_slice(&bytes).unwrap()
        };
        let call = parse(provider.prefix(&request()).unwrap());
        let mut warm = parse(provider.prefix_for(&request(), true).unwrap());
        assert_eq!(
            (&warm["max_tokens"], &warm["stream"]),
            (&json!(0), &json!(false))
        );
        warm["max_tokens"] = call["max_tokens"].clone();
        warm["stream"] = json!(true);
        assert_eq!(warm, call);
    }

    /// Only Anthropic's own API refreshes: the Responses cache outlives a
    /// long tool call, budgeted thinking refuses `max_tokens: 0`, and Bedrock
    /// has not been verified to take it.
    #[test]
    fn keep_warm_applies_where_a_zero_output_request_is_accepted() {
        let transport = Transport::new(64, 1).unwrap();
        let anthropic = Provider::new(
            transport.clone(),
            Family::Anthropic,
            "https://api.anthropic.com/v1",
            None,
        )
        .unwrap();
        assert_eq!(
            anthropic.keep_warm_after("claude-sonnet-5", Some("high")),
            Some(KEEP_WARM)
        );
        assert_eq!(
            anthropic.keep_warm_after("claude-haiku-4-5", None),
            Some(KEEP_WARM)
        );
        assert_eq!(
            anthropic.keep_warm_after("claude-haiku-4-5", Some("low")),
            None
        );
        let off = Provider::new(
            transport.clone(),
            Family::Anthropic,
            "https://api.anthropic.com/v1",
            None,
        )
        .unwrap()
        .with_keep_warm(None)
        .unwrap();
        assert_eq!(off.keep_warm_after("claude-sonnet-5", None), None);
        let responses = Provider::new(
            transport.clone(),
            Family::Responses,
            "https://api.openai.com/v1",
            None,
        )
        .unwrap();
        assert_eq!(responses.keep_warm_after("gpt-5", None), None);
        let bedrock = Provider::new(
            transport.clone(),
            Family::Anthropic,
            "https://bedrock-mantle.us-east-1.api.aws/anthropic/v1",
            None,
        )
        .unwrap();
        assert_eq!(
            bedrock.keep_warm_after("anthropic.claude-sonnet-5", None),
            None
        );
        for after in [0, 300] {
            let refused = Provider::new(
                transport.clone(),
                Family::Anthropic,
                "https://a.test/v1",
                None,
            )
            .unwrap()
            .with_keep_warm(Some(Duration::from_secs(after)));
            assert_eq!(
                refused.err().map(|e| e.code),
                Some("invalid_keep_warm".to_owned())
            );
        }
    }

    /// Bedrock serves both wire families this runtime already speaks, on two
    /// endpoints with independent quotas, so each is an ordinary provider
    /// binding: a base URL and the family's own key header. Nothing about the
    /// four routes needs its own encoder.
    #[test]
    fn bedrock_routes_are_ordinary_base_urls_for_the_families_we_speak() {
        let transport = Transport::new(64, 1).unwrap();
        let route = |family, base: String| {
            Provider::new(transport.clone(), family, &base, None)
                .unwrap()
                .url
                .to_string()
        };
        for host in [
            "https://bedrock-mantle.us-east-1.api.aws",
            "https://bedrock-runtime.us-east-1.amazonaws.com",
        ] {
            assert_eq!(
                route(Family::Anthropic, format!("{host}/anthropic/v1")),
                format!("{host}/anthropic/v1/messages")
            );
            assert_eq!(
                route(Family::Responses, format!("{host}/openai/v1")),
                format!("{host}/openai/v1/responses")
            );
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
                cache_key: None,
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

    /// A 200 whose stream ends in a rate limit is a refusal, not an accepted
    /// call, so refusals of that kind still escalate the pool's block.
    #[tokio::test]
    async fn in_stream_refusals_escalate_although_the_status_was_200() {
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
                let _ = socket
                    .write_all(concat!(
                        "HTTP/1.1 200 OK\r\ncontent-type: text/event-stream\r\nconnection: close\r\n\r\n",
                        "event: response.failed\ndata: {\"type\":\"response.failed\",\"response\":",
                        "{\"error\":{\"code\":\"rate_limit_exceeded\",\"message\":\"Slow down.\"}}}\n\n",
                    ).as_bytes())
                    .await;
            }
        });
        let provider =
            Provider::new(Transport::new(0, 1).unwrap(), Family::Responses, &url, None).unwrap();
        let tools = none();
        for expected in [1, 2] {
            let request = Request {
                model: "m",
                instructions: "",
                reasoning: None,
                tools: &tools,
                allow_tool_calls: true,
                cache_key: None,
                items: Items::empty(),
                chain: None,
            };
            let error = provider
                .complete(request, |_| async { Ok(()) })
                .await
                .unwrap_err();
            assert_eq!(error.code, "provider_rate_limited");
            let left = provider.blocked_for("m").unwrap();
            let seconds = left.as_secs_f64();
            assert!(
                seconds > expected as f64 - 0.5 && seconds <= expected as f64,
                "{seconds}"
            );
            // Called while closed, the pool refuses without asking; after it
            // reopens, the next refusal is the next step of the streak.
            if expected == 1 {
                tokio::time::sleep(left + Duration::from_millis(20)).await;
            }
        }
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
            cache_key: None,
            items: Items::empty(),
            chain: None,
        };
        let _ = provider
            .complete(
                Request {
                    cache_key: Some("synthetic-key"),
                    ..request
                },
                |_| async { Ok(()) },
            )
            .await;
        let head = head.await.unwrap();
        assert!(head.contains("\r\nsession-id: synthetic-key\r\n"), "{head}");
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
            cache_key: None,
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
            chain: None,
            cache_key: None,
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
            chain: None,
            cache_key: None,
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
            chain: None,
            cache_key: None,
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
                chain: None,
                cache_key: None,
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
            chain: None,
            cache_key: None,
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
            cache_key: None,
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
        let prefix = Bytes::from_static(b"{\"input\":[");
        let (_, len) = provider.body(
            prefix.clone(),
            &Items::new(joined, || stream::empty().boxed()),
        );
        assert_eq!(len, prefix.len() + joined + 2);
    }

    /// An empty chunk would go out as an empty HTTP/2 DATA frame, which
    /// Bedrock answers by closing the connection. Every stream of the same
    /// items reads the same bytes, so a digest matches the body sent.
    #[tokio::test]
    async fn bodies_carry_no_empty_chunk_and_read_the_same_every_time() {
        let items = Items::new(9, || {
            stream::iter(
                ["", "{\"a\":1}", "", ",", ""].map(|c| Ok(Bytes::from_static(c.as_bytes()))),
            )
            .boxed()
        });
        for _ in 0..2 {
            let chunks: Vec<Bytes> = framed(Bytes::from_static(b"{\"input\":["), &items)
                .map(|chunk| chunk.unwrap())
                .collect()
                .await;
            assert!(chunks.iter().all(|c| !c.is_empty()), "{chunks:?}");
            assert_eq!(chunks.concat(), b"{\"input\":[{\"a\":1},]}");
        }
        let chunks: Vec<Bytes> = framed(Bytes::new(), &Items::empty())
            .map(|chunk| chunk.unwrap())
            .collect()
            .await;
        assert_eq!(chunks, [Bytes::from_static(b"]}")]);
    }
}
