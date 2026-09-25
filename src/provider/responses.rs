//! OpenAI Responses streaming subset: text and reasoning-summary deltas, then
//! a validated terminal `response.completed` payload. The response's items are
//! those streamed as `response.output_item.done`, kept once and moved into the
//! completion; the terminal output stands in only when none streamed. The
//! ChatGPT Codex endpoint streams items and leaves the terminal output empty.
use super::{Completion, Delta, Frame, MAX_OUTPUT, ToolCall, Usage, detail_of, encoded_len};
use crate::{Error, Result, fail, fail_with};
use bytes::Bytes;
use serde::Deserialize;
use serde_json::{Value, value::RawValue};
use std::borrow::Cow;

#[derive(Deserialize)]
struct Event<'a> {
    #[serde(rename = "type")]
    kind: &'a str,
    #[serde(borrow)]
    delta: Option<Cow<'a, str>>,
    #[serde(borrow)]
    response: Option<&'a RawValue>,
    #[serde(borrow)]
    item: Option<&'a RawValue>,
}

#[derive(Default)]
pub struct State {
    text: String,
    /// JSON-encoded bytes of the streamed text and reasoning summaries.
    text_bytes: usize,
    thinking: usize,
    items: Vec<Bytes>,
    item_bytes: usize,
    completion: Option<Completion>,
    usage: Option<Usage>,
    /// The completed response's id, for a socket's `previous_response_id`.
    id: Option<String>,
}

impl State {
    pub fn frame(&mut self, frame: &[u8]) -> Result<Frame> {
        let event: Event<'_> = serde_json::from_slice(frame)?;
        if self.completion.is_some() {
            return fail("event_after_completion");
        }
        match event.kind {
            "response.output_text.delta" => {
                let part = event.delta.ok_or(Error::new("missing_text_delta"))?;
                self.text_bytes += encoded_len(&part);
                if self.text_bytes + self.thinking > MAX_OUTPUT {
                    return fail("output_limit");
                }
                self.text.push_str(&part);
                Ok(Frame::Delta(Delta::Text(part.into_owned())))
            }
            "response.reasoning_summary_text.delta" => {
                let part = event.delta.ok_or(Error::new("missing_text_delta"))?;
                self.thinking += encoded_len(&part);
                if self.text_bytes + self.thinking > MAX_OUTPUT {
                    return fail("output_limit");
                }
                Ok(Frame::Delta(Delta::Thinking(part.into_owned())))
            }
            "response.output_item.done" => {
                let item = event.item.ok_or(Error::new("missing_output_item"))?;
                self.item_bytes += item.get().len();
                if self.item_bytes > MAX_OUTPUT || self.items.len() >= 64 {
                    return fail("output_limit");
                }
                self.items
                    .push(Bytes::copy_from_slice(item.get().as_bytes()));
                Ok(Frame::Quiet)
            }
            "response.completed" => {
                let raw = event.response.ok_or(Error::new("missing_response"))?;
                let streamed = std::mem::take(&mut self.items);
                self.completion = Some(parse_completion_with_usage(
                    raw,
                    streamed,
                    &self.text,
                    &mut self.usage,
                    &mut self.id,
                )?);
                Ok(Frame::Quiet)
            }
            "error" | "response.failed" | "response.incomplete" => {
                let value: Value = serde_json::from_slice(frame).unwrap_or(Value::Null);
                self.usage = value["response"]
                    .get("usage")
                    .filter(|v| !v.is_null())
                    .map(parse_usage);
                let detail = detail_of(&value)
                    .or_else(|| detail_of(&value["response"]))
                    .or_else(|| {
                        value["response"]["incomplete_details"]["reason"]
                            .as_str()
                            .map(str::to_owned)
                    });
                // A rate limit refused inside the stream is a distinct outcome
                // from a response the model could not finish: it is the
                // provider's pace, not the request, and a fleet must see it.
                let rate_limited = (event.kind == "error"
                    && value["code"].as_str() == Some("rate_limit_exceeded"))
                    || [&value["error"], &value["response"]["error"]]
                        .iter()
                        .any(|error| error["code"].as_str() == Some("rate_limit_exceeded"))
                    || detail
                        .as_deref()
                        .is_some_and(|d| d.starts_with("Rate limit reached"));
                let code = if rate_limited {
                    "provider_rate_limited"
                } else {
                    "provider_incomplete"
                };
                match detail {
                    Some(detail) => fail_with(code, detail),
                    None => fail(code),
                }
            }
            // Metadata and tool argument deltas are represented by the
            // validated terminal output. Unknown final item kinds fail.
            _ => Ok(Frame::Quiet),
        }
    }
    pub fn usage(&self) -> Option<Usage> {
        self.usage.clone()
    }
    /// The terminal event arrived; a socket reads no further for this call.
    pub fn done(&self) -> bool {
        self.completion.is_some()
    }
    pub fn id(&self) -> Option<&str> {
        self.id.as_deref()
    }
    pub fn finish(self) -> Result<Completion> {
        self.completion.ok_or(Error::new("missing_completion"))
    }
}

#[cfg(test)]
fn parse_completion(raw: &RawValue, streamed: &str) -> Result<Completion> {
    parse_completion_with_usage(raw, Vec::new(), streamed, &mut None, &mut None)
}

fn parse_completion_with_usage(
    raw: &RawValue,
    streamed_items: Vec<Bytes>,
    streamed: &str,
    reported: &mut Option<Usage>,
    id: &mut Option<String>,
) -> Result<Completion> {
    #[derive(Deserialize)]
    struct Response<'a> {
        #[serde(default, borrow)]
        id: Option<Cow<'a, str>>,
        status: &'a str,
        #[serde(borrow)]
        output: Vec<&'a RawValue>,
        usage: Option<Value>,
    }
    let response: Response<'_> = serde_json::from_str(raw.get())?;
    *reported = response.usage.as_ref().map(parse_usage);
    *id = response.id.map(Cow::into_owned);
    if response.status != "completed" {
        return fail("provider_incomplete");
    }
    let mut items = Vec::new();
    let mut calls = Vec::new();
    let mut text = String::new();
    let mut bytes = 0;
    let output = if streamed_items.is_empty() {
        let mut output = Vec::with_capacity(response.output.len());
        for raw in response.output {
            bytes += raw.get().len();
            if bytes > MAX_OUTPUT || output.len() >= 64 {
                return fail("output_limit");
            }
            output.push(Bytes::copy_from_slice(raw.get().as_bytes()));
        }
        output
    } else {
        streamed_items
    };
    for raw in output {
        let item: Value = serde_json::from_slice(&raw)?;
        match item["type"].as_str() {
            Some("message") if item["role"] == "assistant" => {
                for content in item["content"]
                    .as_array()
                    .ok_or(Error::new("invalid_content"))?
                {
                    match content["type"].as_str() {
                        Some("output_text") => text
                            .push_str(content["text"].as_str().ok_or(Error::new("invalid_text"))?),
                        Some("refusal") => return fail("provider_refusal"),
                        _ => return fail("unsupported_content"),
                    }
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
        items.push(raw);
    }
    if text != streamed {
        return fail("stream_terminal_mismatch");
    }
    let usage = reported.clone();
    Ok(Completion {
        thinking_dropped: 0,
        fallbacks: Vec::new(),
        items,
        calls,
        usage,
    })
}

fn parse_usage(usage: &Value) -> Usage {
    Usage {
        input_tokens: usage["input_tokens"].as_u64().unwrap_or(0),
        output_tokens: usage["output_tokens"].as_u64().unwrap_or(0),
        cached_input_tokens: usage["input_tokens_details"]["cached_tokens"]
            .as_u64()
            .unwrap_or(0),
        cache_write_tokens: 0,
        models: Vec::new(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn inconsistent_terminal_text_and_duplicate_tool_ids_fail() {
        let text = RawValue::from_string(r#"{"status":"completed","output":[{"type":"message","role":"assistant","content":[{"type":"output_text","text":"wrong"}]}]}"#.into()).unwrap();
        assert!(parse_completion(&text, "expected").is_err());
        let mut state = State::default();
        assert!(state.frame(br#"{"type":"response.completed","response":{"status":"completed","output":[{"type":"message","role":"assistant","content":[{"type":"output_text","text":"unstreamed"}]}],"usage":{"input_tokens":10,"output_tokens":2}}}"#).is_err());
        assert_eq!(state.usage().unwrap().input_tokens, 10);
        assert_eq!(state.usage().unwrap().output_tokens, 2);

        let calls = RawValue::from_string(r#"{"status":"completed","output":[{"type":"function_call","name":"echo","call_id":"x","arguments":"{}"},{"type":"function_call","name":"echo","call_id":"x","arguments":"{}"}]}"#.into()).unwrap();
        assert!(parse_completion(&calls, "").is_err());
    }
    #[test]
    fn reasoning_summaries_stream_and_usage_is_extracted() {
        let mut state = State::default();
        let delta = state
            .frame(br#"{"type":"response.reasoning_summary_text.delta","delta":"hmm"}"#)
            .unwrap();
        assert!(matches!(delta, Frame::Delta(Delta::Thinking(t)) if t == "hmm"));
        state
            .frame(br#"{"type":"response.output_text.delta","delta":"ok"}"#)
            .unwrap();
        state.frame(br#"{"type":"response.completed","response":{"status":"completed","output":[{"type":"reasoning","summary":[]},{"type":"message","role":"assistant","content":[{"type":"output_text","text":"ok"}]}],"usage":{"input_tokens":10,"output_tokens":2,"input_tokens_details":{"cached_tokens":4}}}}"#).unwrap();
        let completion = state.finish().unwrap();
        assert_eq!(completion.items.len(), 2);
        assert_eq!(
            completion.usage,
            Some(Usage {
                input_tokens: 10,
                output_tokens: 2,
                cached_input_tokens: 4,
                cache_write_tokens: 0,
                models: Vec::new(),
            })
        );
        let failed = State::default()
            .frame(br#"{"type":"response.failed","response":{"error":{"message":"quota"}}}"#)
            .unwrap_err();
        assert_eq!(
            (failed.code.as_str(), failed.detail.as_deref()),
            ("provider_incomplete", Some("quota"))
        );
        let limited = State::default()
            .frame(br#"{"type":"response.failed","response":{"error":{"code":"rate_limit_exceeded","message":"Rate limit reached for m on tokens per min (TPM): Limit 4000000"}}}"#)
            .unwrap_err();
        assert_eq!(limited.code, "provider_rate_limited");
        assert!(limited.detail.unwrap().contains("TPM"));
    }
    #[test]
    fn streamed_items_stand_in_for_an_empty_terminal_output() {
        let mut state = State::default();
        state
            .frame(br#"{"type":"response.output_text.delta","delta":"ok"}"#)
            .unwrap();
        state.frame(br#"{"type":"response.output_item.done","item":{"type":"message","role":"assistant","content":[{"type":"output_text","text":"ok"}]}}"#).unwrap();
        state.frame(br#"{"type":"response.output_item.done","item":{"type":"function_call","name":"echo","call_id":"c1","arguments":"{}"}}"#).unwrap();
        state
            .frame(
                br#"{"type":"response.completed","response":{"status":"completed","output":[]}}"#,
            )
            .unwrap();
        let completion = state.finish().unwrap();
        assert_eq!(completion.items.len(), 2);
        assert_eq!(completion.calls[0].call_id, "c1");

        // Items that streamed and also came in the terminal output are the
        // response once.
        let mut state = State::default();
        state.frame(br#"{"type":"response.output_item.done","item":{"type":"function_call","name":"echo","call_id":"c1","arguments":"{}"}}"#).unwrap();
        state.frame(br#"{"type":"response.completed","response":{"status":"completed","output":[{"type":"function_call","name":"echo","call_id":"c1","arguments":"{}"}]}}"#).unwrap();
        assert_eq!(state.finish().unwrap().calls.len(), 1);

        // Streamed items are validated like terminal ones.
        let mut state = State::default();
        state
            .frame(br#"{"type":"response.output_item.done","item":{"type":"message","role":"assistant","content":[{"type":"output_text","text":"unstreamed"}]}}"#)
            .unwrap();
        let error = state
            .frame(
                br#"{"type":"response.completed","response":{"status":"completed","output":[]}}"#,
            )
            .unwrap_err();
        assert_eq!(error.code, "stream_terminal_mismatch");
    }
    #[test]
    fn rate_limit_codes_do_not_depend_on_message_wording() {
        for frame in [
            r#"{"type":"error","code":"rate_limit_exceeded","message":"Please try again in 10ms.","param":null,"sequence_number":1}"#,
            r#"{"type":"error","error":{"code":"rate_limit_exceeded","message":"Please try again in 10ms."}}"#,
            r#"{"type":"response.failed","response":{"error":{"code":"rate_limit_exceeded","message":"Please try again in 10ms."}}}"#,
        ] {
            let error = State::default().frame(frame.as_bytes()).unwrap_err();
            assert_eq!(error.code, "provider_rate_limited", "{frame}");
            assert_eq!(error.detail.as_deref(), Some("Please try again in 10ms."));
        }
        let error = State::default()
            .frame(br#"{"type":"error","code":"invalid_request_error","message":"Invalid input."}"#)
            .unwrap_err();
        assert_eq!(error.code, "provider_incomplete");
    }
}
