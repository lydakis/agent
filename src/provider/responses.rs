//! OpenAI Responses streaming subset: text and reasoning-summary deltas, then
//! a validated terminal `response.completed` payload.
use super::{Completion, Delta, MAX_OUTPUT, ToolCall, Usage, detail_of};
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
}

#[derive(Default)]
pub struct State {
    text: String,
    thinking: usize,
    completion: Option<Completion>,
    usage: Option<Usage>,
}

impl State {
    pub fn frame(&mut self, frame: &[u8]) -> Result<Option<Delta>> {
        let event: Event<'_> = serde_json::from_slice(frame)?;
        if self.completion.is_some() {
            return fail("event_after_completion");
        }
        match event.kind {
            "response.output_text.delta" => {
                let part = event.delta.ok_or(Error::new("missing_text_delta"))?;
                if self.text.len() + self.thinking + part.len() > MAX_OUTPUT {
                    return fail("output_limit");
                }
                self.text.push_str(&part);
                Ok(Some(Delta::Text(part.into_owned())))
            }
            "response.reasoning_summary_text.delta" => {
                let part = event.delta.ok_or(Error::new("missing_text_delta"))?;
                self.thinking += part.len();
                if self.text.len() + self.thinking > MAX_OUTPUT {
                    return fail("output_limit");
                }
                Ok(Some(Delta::Thinking(part.into_owned())))
            }
            "response.completed" => {
                let raw = event.response.ok_or(Error::new("missing_response"))?;
                self.completion = Some(parse_completion_with_usage(
                    raw,
                    &self.text,
                    &mut self.usage,
                )?);
                Ok(None)
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
            _ => Ok(None),
        }
    }
    pub fn usage(&self) -> Option<Usage> {
        self.usage.clone()
    }
    pub fn finish(self) -> Result<Completion> {
        self.completion.ok_or(Error::new("missing_completion"))
    }
}

#[cfg(test)]
fn parse_completion(raw: &RawValue, streamed: &str) -> Result<Completion> {
    parse_completion_with_usage(raw, streamed, &mut None)
}

fn parse_completion_with_usage(
    raw: &RawValue,
    streamed: &str,
    reported: &mut Option<Usage>,
) -> Result<Completion> {
    #[derive(Deserialize)]
    struct Response<'a> {
        status: &'a str,
        #[serde(borrow)]
        output: Vec<&'a RawValue>,
        usage: Option<Value>,
    }
    let response: Response<'_> = serde_json::from_str(raw.get())?;
    *reported = response.usage.as_ref().map(parse_usage);
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
        items.push(Bytes::copy_from_slice(raw.get().as_bytes()));
    }
    if text != streamed {
        return fail("stream_terminal_mismatch");
    }
    let usage = reported.clone();
    Ok(Completion {
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
        assert!(matches!(delta, Some(Delta::Thinking(t)) if t == "hmm"));
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
                cached_input_tokens: 4
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
