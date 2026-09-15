//! Streaming model calls. One shared HTTP transport; per-family request
//! encoding and SSE parsing. History items are streamed by reference.
use crate::{
    Error, Result,
    codec::{Family, ToolSchema},
    fail,
    history::History,
    sse::Decoder,
};
use bytes::Bytes;
use futures_util::{StreamExt, stream};
use serde::Serialize;
use serde_json::{Value, json, value::RawValue};
use std::{future::Future, sync::Arc, time::Duration};
use tokio::sync::Semaphore;

mod anthropic;
mod responses;

pub const MAX_OUTPUT: usize = 512 * 1024;
const ANTHROPIC_MAX_TOKENS: u32 = 32_768;

/// One HTTP client and startup-admission budget shared by every provider.
pub struct Transport {
    client: reqwest::Client,
    starting: Semaphore,
}
impl Transport {
    /// `max_connecting` bounds requests awaiting response headers; zero means
    /// no bound beyond the operating system.
    pub fn new(max_connecting: usize) -> Result<Arc<Self>> {
        // No total deadline: long generations are legitimate. Idle reads are
        // bounded so a stalled stream cannot hold a turn forever.
        let client = reqwest::Client::builder()
            .no_proxy()
            .redirect(reqwest::redirect::Policy::none())
            .connect_timeout(Duration::from_secs(10))
            .read_timeout(Duration::from_secs(120))
            .pool_idle_timeout(Duration::from_secs(60))
            .pool_max_idle_per_host(1024)
            .build()
            .map_err(|_| Error::new("http_client_init"))?;
        Ok(Arc::new(Self {
            client,
            starting: Semaphore::new(if max_connecting == 0 {
                Semaphore::MAX_PERMITS
            } else {
                max_connecting.min(Semaphore::MAX_PERMITS)
            }),
        }))
    }
}

#[derive(Clone)]
pub struct Provider {
    transport: Arc<Transport>,
    family: Family,
    url: reqwest::Url,
    key: Option<String>,
    tools: Arc<RawValue>,
    has_tools: bool,
    max_output_tokens: Option<u32>,
}

#[derive(Debug)]
pub enum Delta {
    Text(String),
    Thinking(String),
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
pub struct Request<'a> {
    pub model: &'a str,
    pub instructions: &'a str,
    pub reasoning: Option<&'a str>,
    pub history: &'a History,
}

enum Parser {
    Responses(responses::State),
    Anthropic(anthropic::State),
}
impl Parser {
    fn frame(&mut self, frame: &[u8]) -> Result<Option<Delta>> {
        match self {
            Parser::Responses(state) => state.frame(frame),
            Parser::Anthropic(state) => state.frame(frame),
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
        tools: &[ToolSchema],
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
        let encoded = serde_json::to_string(&family.tools(tools))?;
        Ok(Self {
            transport,
            family,
            url,
            key,
            tools: Arc::from(RawValue::from_string(encoded)?),
            has_tools: !tools.is_empty(),
            max_output_tokens: None,
        })
    }
    pub fn family(&self) -> Family {
        self.family
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
            reasoning: Option<Value>,
        }
        #[derive(Serialize)]
        struct Anthropic<'a> {
            model: &'a str,
            max_tokens: u32,
            system: &'a str,
            stream: bool,
            #[serde(skip_serializing_if = "Option::is_none")]
            tools: Option<&'a RawValue>,
            #[serde(skip_serializing_if = "Option::is_none")]
            thinking: Option<Value>,
        }
        let (mut bytes, field) = match self.family {
            Family::Responses => (
                serde_json::to_vec(&Responses {
                    model: request.model,
                    instructions: request.instructions,
                    stream: true,
                    store: false,
                    // Explicit for compatibility with older Responses servers.
                    include: ["reasoning.encrypted_content"],
                    max_output_tokens: self.max_output_tokens,
                    tools: &self.tools,
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
                    system: request.instructions,
                    stream: true,
                    tools: self.has_tools.then_some(&*self.tools),
                    thinking: request.reasoning.map(|level| {
                        let budget = match level {
                            "low" => 2048,
                            "medium" => 8192,
                            _ => 16384,
                        };
                        json!({"type":"enabled","budget_tokens":budget})
                    }),
                })?,
                &b",\"messages\":["[..],
            ),
        };
        bytes.pop(); // Replace the closing brace with the streamed array field.
        bytes.extend_from_slice(field);
        Ok(bytes)
    }

    /// Body frames reference immutable history allocations. Content-Length avoids
    /// requiring provider support for chunked uploads; no whole-body JSON copy.
    fn body(&self, prefix: Vec<u8>, history: &History) -> (reqwest::Body, usize) {
        let items = history.items();
        let mut frames = Vec::with_capacity(2 * items.len() + 2);
        frames.push(Bytes::from(prefix));
        for (index, item) in items.into_iter().enumerate() {
            if index != 0 {
                frames.push(Bytes::from_static(b","));
            }
            frames.push(item);
        }
        frames.push(Bytes::from_static(b"]}"));
        let len = frames.iter().map(Bytes::len).sum();
        let body = reqwest::Body::wrap_stream(stream::iter(
            frames.into_iter().map(Ok::<_, std::io::Error>),
        ));
        (body, len)
    }

    pub async fn complete<F, Fut>(&self, request: Request<'_>, delta: F) -> Result<Completion>
    where
        F: FnMut(Delta) -> Fut,
        Fut: Future<Output = Result<()>>,
    {
        self.complete_inner(request, delta)
            .await
            .map_err(|error| sanitize_error(error, self.key.as_deref()))
    }

    async fn complete_inner<F, Fut>(&self, request: Request<'_>, mut delta: F) -> Result<Completion>
    where
        F: FnMut(Delta) -> Fut,
        Fut: Future<Output = Result<()>>,
    {
        // Bound request startup until response headers arrive; release before
        // reading SSE so established streams are not capped at this limit.
        let admission =
            tokio::time::timeout(Duration::from_secs(60), self.transport.starting.acquire())
                .await
                .map_err(|_| Error::new("provider_admission_timeout"))?
                .map_err(|_| Error::new("provider_admission_closed"))?;
        let prefix = self.prefix(&request)?;
        let (body, len) = self.body(prefix, request.history);
        let mut http = self
            .transport
            .client
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
        let response = http.send().await.map_err(connection_error)?;
        drop(admission);
        if !response.status().is_success() {
            let code = format!("provider_http_{}", response.status().as_u16());
            let detail = error_detail(response).await;
            return Err(Error { code, detail });
        }
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
        let mut total = 0usize;
        while let Some(chunk) = stream.next().await {
            let chunk = chunk.map_err(|_| Error::new("provider_stream_failed"))?;
            total += chunk.len();
            if total > 16 * 1024 * 1024 {
                return fail("provider_response_limit");
            }
            for byte in chunk {
                let Some(frame) = decoder.byte(byte)? else {
                    continue;
                };
                if frame == b"[DONE]" {
                    continue;
                }
                if let Some(part) = parser.frame(&frame)? {
                    delta(part).await?;
                }
            }
        }
        if !decoder.is_empty() {
            return fail("truncated_sse_frame");
        }
        parser.finish()
    }
}

// Preserve only stage, timeout classification, and numeric OS code. Reqwest's
// Display/debug strings can include URLs and must never become diagnostics.
fn connection_error(error: reqwest::Error) -> Error {
    if error.is_timeout() {
        return Error::new("provider_connection_timeout");
    }
    let mut source = std::error::Error::source(&error);
    while let Some(cause) = source {
        if let Some(code) = cause
            .downcast_ref::<std::io::Error>()
            .and_then(|e| e.raw_os_error())
        {
            return Error::new(&format!("provider_connection_os_{code}"));
        }
        source = cause.source();
    }
    Error::new("provider_connection_failed")
}

/// Capture only a complete, bounded error body. A partial body may end in a
/// credential, so never publish it as a fallback diagnostic.
async fn error_detail(response: reqwest::Response) -> Option<String> {
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
        Ok(value) => value
            .as_str()
            .or_else(|| value["error"]["message"].as_str())
            .or_else(|| value["error"].as_str())
            .or_else(|| value["message"].as_str())
            .map(str::to_owned),
        // Never fall back to a serialized JSON representation: alternate
        // escapes could conceal a credential from exact text redaction.
        Err(_) if expects_json || text.starts_with(['{', '[', '"']) => None,
        Err(_) => Some(text.to_owned()),
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

    #[test]
    fn request_prefix_streams_history_after_family_specific_fields() {
        let transport = Transport::new(64).unwrap();
        let provider = Provider::new(
            transport.clone(),
            Family::Anthropic,
            "https://api.example.test",
            None,
            &[],
        )
        .unwrap();
        let history = History::default();
        let prefix = provider
            .prefix(&Request {
                model: "m",
                instructions: "i",
                reasoning: Some("low"),
                history: &history,
            })
            .unwrap();
        let text = String::from_utf8(prefix).unwrap();
        assert!(text.ends_with(",\"messages\":["));
        assert!(text.contains("\"budget_tokens\":2048"));
        assert!(!text.contains("\"tools\""));
        assert_eq!(provider.url.path(), "/messages");
        let responses =
            Provider::new(transport, Family::Responses, "http://h/v1/", None, &[]).unwrap();
        assert_eq!(responses.url.path(), "/v1/responses");
        assert!(responses.clone().with_max_output_tokens(0).is_err());
        assert!(provider.with_max_output_tokens(2048).is_err());
        let responses = responses.with_max_output_tokens(2048).unwrap();
        let mut prefix = responses
            .prefix(&Request {
                model: "m",
                instructions: "i",
                reasoning: None,
                history: &history,
            })
            .unwrap();
        prefix.extend_from_slice(b"]}");
        let body: Value = serde_json::from_slice(&prefix).unwrap();
        assert_eq!(body["max_output_tokens"], 2048);
        assert_eq!(body["include"], json!(["reasoning.encrypted_content"]));
        assert_eq!(body["store"], false);
    }
}
