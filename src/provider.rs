use crate::{Error, Result, fail, history::History, sse::Decoder};
use bytes::Bytes;
use futures_util::{StreamExt, stream};
use serde::Deserialize;
use serde_json::{Value, json, value::RawValue};
use std::{borrow::Cow, future::Future, sync::Arc, time::Duration};
use tokio::sync::Semaphore;

pub const MAX_OUTPUT: usize = 512 * 1024;

#[derive(Clone)]
pub struct Provider {
    client: reqwest::Client,
    url: reqwest::Url,
    prefix: Bytes,
    key: Option<String>,
    starting: Arc<Semaphore>,
}

pub struct Completion {
    pub items: Vec<Bytes>,
    pub calls: Vec<ToolCall>,
}
#[derive(Debug, Deserialize)]
pub struct ToolCall {
    pub name: String,
    pub call_id: String,
    pub arguments: String,
}

#[derive(Deserialize)]
struct Event<'a> {
    #[serde(rename = "type")]
    kind: &'a str,
    #[serde(borrow)]
    delta: Option<Cow<'a, str>>,
    #[serde(borrow)]
    response: Option<&'a RawValue>,
}

impl Provider {
    pub fn new(
        base_url: &str,
        model: &str,
        instructions: &str,
        tools: Value,
        key: Option<String>,
    ) -> Result<Self> {
        let mut url =
            reqwest::Url::parse(base_url).map_err(|_| Error("invalid_provider_url".into()))?;
        if !matches!(url.scheme(), "http" | "https")
            || !url.username().is_empty()
            || url.password().is_some()
            || url.query().is_some()
            || url.fragment().is_some()
        {
            return fail("invalid_provider_url");
        }
        url.set_path(&format!("{}/responses", url.path().trim_end_matches('/')));
        let client = reqwest::Client::builder()
            .no_proxy()
            .redirect(reqwest::redirect::Policy::none())
            .connect_timeout(Duration::from_secs(5))
            .timeout(Duration::from_secs(60))
            .pool_idle_timeout(Duration::from_secs(60))
            .pool_max_idle_per_host(1024)
            .build()
            .map_err(|_| Error("http_client_init".into()))?;
        let mut prefix = serde_json::to_vec(&json!({"model": model, "instructions": instructions,
            "stream": true, "store": false, "tools": tools}))?;
        prefix.pop(); // Replace closing } with the streamed input field.
        prefix.extend_from_slice(b",\"input\":[");
        Ok(Self {
            client,
            url,
            prefix: prefix.into(),
            key,
            starting: Arc::new(Semaphore::new(64)),
        })
    }

    /// Body frames reference immutable history allocations. Content-Length avoids
    /// requiring provider support for chunked uploads; no whole-body JSON copy.
    fn body(&self, history: &History) -> (reqwest::Body, usize) {
        let items = history.items();
        let mut frames = Vec::with_capacity(2 * items.len() + 2);
        frames.push(self.prefix.clone());
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

    pub async fn complete<F, Fut>(&self, history: &History, mut delta: F) -> Result<Completion>
    where
        F: FnMut(String) -> Fut,
        Fut: Future<Output = Result<()>>,
    {
        // Bound request startup until response headers arrive; release before
        // reading SSE so established streams are not capped at this limit.
        let admission = tokio::time::timeout(Duration::from_secs(60), self.starting.acquire())
            .await
            .map_err(|_| Error("provider_admission_timeout".into()))?
            .map_err(|_| Error("provider_admission_closed".into()))?;
        let (body, len) = self.body(history);
        let mut request = self
            .client
            .post(self.url.clone())
            .header("content-type", "application/json")
            .header("content-length", len)
            .body(body);
        if let Some(key) = &self.key {
            request = request.bearer_auth(key);
        }
        let response = request.send().await.map_err(connection_error)?;
        drop(admission);
        if !response.status().is_success() {
            return fail(&format!("provider_http_{}", response.status().as_u16()));
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
        let mut text = String::new();
        let mut completion = None;
        let mut total = 0usize;
        while let Some(chunk) = stream.next().await {
            let chunk = chunk.map_err(|_| Error("provider_stream_failed".into()))?;
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
                let event: Event<'_> = serde_json::from_slice(&frame)?;
                if completion.is_some() {
                    return fail("event_after_completion");
                }
                match event.kind {
                    "response.output_text.delta" => {
                        let part = event.delta.ok_or(Error("missing_text_delta".into()))?;
                        if text.len() + part.len() > MAX_OUTPUT {
                            return fail("output_limit");
                        }
                        text.push_str(&part);
                        delta(part.into_owned()).await?;
                    }
                    "response.completed" => {
                        let raw = event.response.ok_or(Error("missing_response".into()))?;
                        completion = Some(parse_completion(raw, &text)?);
                    }
                    "error" | "response.failed" | "response.incomplete" => {
                        return fail("provider_incomplete");
                    }
                    // Metadata and tool argument deltas are represented by the
                    // validated terminal output. Unknown final item kinds fail.
                    _ => {}
                }
            }
        }
        if !decoder.is_empty() {
            return fail("truncated_sse_frame");
        }
        completion.ok_or(Error("missing_completion".into()))
    }
}

// Preserve only stage, timeout classification, and numeric OS code. Reqwest's
// Display/debug strings can include URLs and must never become diagnostics.
fn connection_error(error: reqwest::Error) -> Error {
    if error.is_timeout() {
        return Error("provider_connection_timeout".into());
    }
    let mut source = std::error::Error::source(&error);
    while let Some(cause) = source {
        if let Some(code) = cause
            .downcast_ref::<std::io::Error>()
            .and_then(|e| e.raw_os_error())
        {
            return Error(format!("provider_connection_os_{code}"));
        }
        source = cause.source();
    }
    Error("provider_connection_failed".into())
}

fn parse_completion(raw: &RawValue, streamed: &str) -> Result<Completion> {
    #[derive(Deserialize)]
    struct Response<'a> {
        status: &'a str,
        #[serde(borrow)]
        output: Vec<&'a RawValue>,
    }
    let response: Response<'_> = serde_json::from_str(raw.get())?;
    if response.status != "completed" {
        return fail("provider_incomplete");
    }
    let mut items = Vec::new();
    let mut calls = Vec::new();
    let mut text = String::new();
    let mut bytes = 0;
    for raw in response.output {
        bytes += raw.get().len();
        if bytes > MAX_OUTPUT || items.len() >= 64 {
            return fail("output_limit");
        }
        let item: Value = serde_json::from_str(raw.get())?;
        match item["type"].as_str() {
            Some("message") if item["role"] == "assistant" => {
                for content in item["content"]
                    .as_array()
                    .ok_or(Error("invalid_content".into()))?
                {
                    if content["type"] != "output_text" {
                        return fail("unsupported_content");
                    }
                    text.push_str(
                        content["text"]
                            .as_str()
                            .ok_or(Error("invalid_text".into()))?,
                    );
                }
            }
            Some("function_call") => {
                let call: ToolCall = serde_json::from_value(item)?;
                if calls.iter().any(|c: &ToolCall| c.call_id == call.call_id)
                    || call.call_id.is_empty()
                {
                    return fail("invalid_tool_call_id");
                }
                calls.push(call);
            }
            Some("reasoning") => {} // Preserve opaque provider reasoning for replay.
            _ => return fail("unsupported_output_item"),
        }
        items.push(Bytes::copy_from_slice(raw.get().as_bytes()));
    }
    if text != streamed {
        return fail("stream_terminal_mismatch");
    }
    Ok(Completion { items, calls })
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn inconsistent_terminal_text_and_duplicate_tool_ids_fail() {
        let text = RawValue::from_string(r#"{"status":"completed","output":[{"type":"message","role":"assistant","content":[{"type":"output_text","text":"wrong"}]}]}"#.into()).unwrap();
        assert!(parse_completion(&text, "expected").is_err());
        let calls = RawValue::from_string(r#"{"status":"completed","output":[{"type":"function_call","name":"echo","call_id":"x","arguments":"{}"},{"type":"function_call","name":"echo","call_id":"x","arguments":"{}"}]}"#.into()).unwrap();
        assert!(parse_completion(&calls, "").is_err());
    }
}
