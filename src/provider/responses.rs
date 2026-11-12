//! OpenAI Responses streaming subset: text and reasoning-summary deltas, then
//! a validated terminal `response.completed` payload. The response's items are
//! those streamed as `response.output_item.done`, kept once and moved into the
//! completion; the terminal output stands in only when none streamed. The
//! ChatGPT Codex endpoint streams items and leaves the terminal output empty.
use super::{
    Completion, Delta, Frame, Speed, ToolCall, Usage, detail_of, unparsed, valid_call_id, writing,
};
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
    /// The output item a delta belongs to; one left out reads as the first.
    #[serde(default)]
    output_index: usize,
}

#[derive(Default)]
pub struct State {
    text: String,
    items: Vec<Bytes>,
    /// What the newest output item writes, as a cut reply names it: `text`,
    /// `reasoning`, or the tool it calls.
    writing: Option<String>,
    completion: Option<Completion>,
    usage: Option<Usage>,
    /// The completed response's id, for a socket's `previous_response_id`.
    id: Option<String>,
}

impl State {
    pub fn frame(&mut self, frame: &[u8]) -> Result<Frame> {
        let event: Event<'_> = match serde_json::from_slice(frame) {
            Ok(event) => event,
            // An error in a shape of the endpoint's own is still its error,
            // its message under `error` or at the top, with no `type` or
            // with fields of other types than OpenAI's.
            Err(error) => {
                let value = serde_json::from_slice::<Value>(frame).unwrap_or(Value::Null);
                let kind = match value.get("type") {
                    None => "error",
                    Some(kind) => kind.as_str().unwrap_or_default(),
                };
                let said = detail_of(&value)
                    .or_else(|| detail_of(&value["response"]))
                    .is_some();
                return match kind {
                    "error" | "response.failed" | "response.incomplete" if said => {
                        let kind = kind.to_owned();
                        self.failed(&kind, value)
                    }
                    _ => Err(unparsed(error, frame)),
                };
            }
        };
        if self.completion.is_some() {
            return fail("event_after_completion");
        }
        match event.kind {
            "response.output_text.delta" => {
                let part = event.delta.ok_or(Error::new("missing_text_delta"))?;
                self.text.push_str(&part);
                Ok(Frame::Delta(Delta::Text(
                    event.output_index,
                    part.into_owned(),
                )))
            }
            "response.reasoning_summary_text.delta" => {
                let part = event.delta.ok_or(Error::new("missing_text_delta"))?;
                Ok(Frame::Delta(Delta::Thinking(
                    event.output_index,
                    part.into_owned(),
                )))
            }
            "response.output_item.done" => {
                let item = event.item.ok_or(Error::new("missing_output_item"))?;
                self.items
                    .push(Bytes::copy_from_slice(item.get().as_bytes()));
                Ok(Frame::Quiet)
            }
            // An item begins: a few bytes before its content streams.
            "response.output_item.added" => {
                self.writing = event.item.and_then(|item| writing(item.get()));
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
                self.failed(event.kind, value)
            }
            // Metadata and tool argument deltas are represented by the
            // validated terminal output. Unknown final item kinds fail.
            _ => Ok(Frame::Quiet),
        }
    }
    /// The provider's refusal or failure, from the stream's error event.
    fn failed(&mut self, kind: &str, value: Value) -> Result<Frame> {
        self.usage = value["response"]
            .get("usage")
            .filter(|v| !v.is_null())
            .map(|usage| Usage {
                served_model: value["response"]["model"]
                    .as_str()
                    .unwrap_or_default()
                    .to_owned(),
                speed: value["response"]["service_tier"].as_str().map(served_tier),
                ..parse_usage(usage)
            });
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
        let rate_limited = (kind == "error"
            && value["code"].as_str() == Some("rate_limit_exceeded"))
            || [&value["error"], &value["response"]["error"]]
                .iter()
                .any(|error| error["code"].as_str() == Some("rate_limit_exceeded"))
            || detail
                .as_deref()
                .is_some_and(|d| d.starts_with("Rate limit reached"));
        // A ChatGPT plan's usage limit can also end a started stream.
        // A code at the top level of an `error` event, as above.
        let errors = [&value, &value["error"], &value["response"]["error"]];
        let named = |test: fn(&str) -> bool| {
            errors
                .iter()
                .any(|error| error["code"].as_str().is_some_and(test))
        };
        // An overload or a server failure after the stream began is
        // the provider's, not the request's, and is retried as Codex
        // retries it (openai/codex e8e6a92, sse/responses_error.rs):
        // `slow_down` as a rate limit, an overload holding the pool.
        // A policy classifier declined the request, which Codex ends the
        // turn on (openai/codex d27e7c1, `CyberPolicy`): a refusal as
        // Anthropic's are, its category and the provider's words on why.
        if named(|code| code == "cyber_policy") {
            let begun = self.writing.is_some() || !self.items.is_empty() || !self.text.is_empty();
            let mut facts = serde_json::Map::new();
            facts.insert("category".into(), "cyber".into());
            if let Some(why) = detail {
                facts.insert("explanation".into(), crate::output::bounded(why).into());
            }
            let tokens = self.usage.as_ref().map_or(0, |usage| usage.output_tokens);
            super::refused_at(&mut facts, begun, tokens);
            return Err(
                Error::with("provider_refusal", "category cyber").facts(Value::Object(facts))
            );
        }
        let code = if named(super::quota) {
            "provider_quota_exhausted"
        } else if named(super::plan_unavailable) {
            "provider_plan_unavailable"
        } else if rate_limited || named(|code| code == "slow_down") {
            "provider_rate_limited"
        } else if named(|code| code == "server_is_overloaded") {
            "provider_overloaded"
        } else if named(|code| code == "server_error") {
            "provider_server_error"
        } else {
            "provider_incomplete"
        };
        // The model's own output cap: what it was writing is the newest
        // item the response began, or the last it holds.
        let reason = &value["response"]["incomplete_details"]["reason"];
        if code == "provider_incomplete" && reason == "max_output_tokens" {
            let last = (value["response"]["output"].as_array())
                .and_then(|output| output.last())
                .and_then(|item| {
                    Some(super::writes(item["type"].as_str()?, item["name"].as_str()))
                });
            let tokens = self.usage.as_ref().map_or(0, |usage| usage.output_tokens);
            let writing = self.writing.take().or(last);
            return Err(super::cut_short("max_output_tokens", tokens, writing));
        }
        match detail {
            Some(detail) => fail_with(code, detail),
            None => fail(code),
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
        /// The model that answered, which a dated snapshot or a reroute
        /// can make differ from the one requested.
        #[serde(default, borrow)]
        model: Option<Cow<'a, str>>,
        /// The tier that served the response, which can be the default
        /// one though a faster one was asked.
        #[serde(default, borrow)]
        service_tier: Option<Cow<'a, str>>,
        status: &'a str,
        #[serde(borrow)]
        output: Vec<&'a RawValue>,
        usage: Option<Value>,
    }
    let response: Response<'_> =
        serde_json::from_str(raw.get()).map_err(|e| unparsed(e, raw.get().as_bytes()))?;
    *reported = response.usage.as_ref().map(|usage| Usage {
        served_model: response.model.map(Cow::into_owned).unwrap_or_default(),
        speed: response.service_tier.as_deref().map(served_tier),
        ..parse_usage(usage)
    });
    *id = response.id.map(Cow::into_owned);
    if response.status != "completed" {
        return fail("provider_incomplete");
    }
    let mut items = Vec::new();
    let mut calls = Vec::new();
    let mut text = String::new();
    let output = if streamed_items.is_empty() {
        (response.output.iter())
            .map(|raw| Bytes::copy_from_slice(raw.get().as_bytes()))
            .collect()
    } else {
        streamed_items
    };
    for raw in output {
        let item: Value = serde_json::from_slice(&raw).map_err(|e| unparsed(e, &raw))?;
        match item["type"].as_str() {
            Some("message") if item["role"] == "assistant" => {
                for content in item["content"]
                    .as_array()
                    .ok_or(Error::new("invalid_content"))?
                {
                    match content["type"].as_str() {
                        Some("output_text") => text
                            .push_str(content["text"].as_str().ok_or(Error::new("invalid_text"))?),
                        // The model's own refusal, written whole: its
                        // words are the reason.
                        Some("refusal") => {
                            let mut facts = serde_json::Map::new();
                            if let Some(why) = content["refusal"].as_str() {
                                let why = crate::output::bounded(why.to_owned());
                                facts.insert("explanation".into(), why.into());
                            }
                            let tokens = reported.as_ref().map_or(0, |usage| usage.output_tokens);
                            super::refused_at(&mut facts, true, tokens);
                            return Err(Error::new("provider_refusal").facts(Value::Object(facts)));
                        }
                        _ => return fail("unsupported_content"),
                    }
                }
            }
            Some(kind @ ("function_call" | "custom_tool_call")) => {
                // Search comes back as Codex declares it, `web.run`; the
                // bot calls it `web_search`.
                let search =
                    item["namespace"] == super::RUN_NAMESPACE && item["name"] == super::RUN_NAME;
                let mut call = match kind {
                    "function_call" => {
                        serde_json::from_value::<ToolCall>(item).map_err(|e| unparsed(e, &raw))?
                    }
                    _ => {
                        #[derive(Deserialize)]
                        struct Custom {
                            name: String,
                            call_id: String,
                            input: String,
                        }
                        let custom: Custom =
                            serde_json::from_value(item).map_err(|e| unparsed(e, &raw))?;
                        ToolCall {
                            name: custom.name,
                            call_id: custom.call_id,
                            arguments: serde_json::json!({"input":custom.input}).to_string(),
                            custom: true,
                        }
                    }
                };
                if search {
                    call.name = "web_search".into();
                }
                if calls.iter().any(|c: &ToolCall| c.call_id == call.call_id)
                    || !valid_call_id(&call.call_id)
                {
                    return fail("invalid_tool_call_id");
                }
                calls.push(call);
            }
            Some("reasoning") => {} // Preserve opaque provider reasoning for replay.
            // A search the provider ran itself, replayed as it came.
            Some("web_search_call") => {}
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
        paused: false,
    })
}

/// The speed a response's `service_tier` names: `priority` is fast, as
/// the request names it; any other tier, `default` among them, standard.
fn served_tier(tier: &str) -> Speed {
    match tier {
        "priority" => Speed::Fast,
        "ultrafast" => Speed::Ultrafast,
        _ => Speed::Standard,
    }
}

fn parse_usage(usage: &Value) -> Usage {
    Usage {
        input_tokens: usage["input_tokens"].as_u64().unwrap_or(0),
        output_tokens: usage["output_tokens"].as_u64().unwrap_or(0),
        cached_input_tokens: usage["input_tokens_details"]["cached_tokens"]
            .as_u64()
            .unwrap_or(0),
        ..Usage::default()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::provider::MAX_CALL_ID;
    use serde_json::json;
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
    fn an_error_in_the_endpoint_s_own_shape_says_what_it_said() {
        // No `type`, as an OpenAI-shaped error body; a message alone; and
        // an `error` event whose other fields are not OpenAI's types.
        for frame in [
            r#"{"error":{"code":"validation_error","message":"The request was rejected by target.","param":null,"type":"invalid_request_error"}}"#,
            r#"{"message":"The request was rejected by target."}"#,
            r#"{"type":"error","output_index":null,"error":{"message":"The request was rejected by target."}}"#,
        ] {
            let error = State::default().frame(frame.as_bytes()).unwrap_err();
            assert_eq!(error.code, "provider_incomplete", "{frame}");
            assert_eq!(
                error.detail.as_deref(),
                Some("The request was rejected by target."),
                "{frame}"
            );
        }
        // One that says nothing is still a frame that did not parse.
        for frame in [
            r#"{"error":"x"}"#,
            r#"{"type":"response.created","output_index":"x"}"#,
        ] {
            let error = State::default().frame(frame.as_bytes()).unwrap_err();
            assert_eq!(error.code, "invalid_json", "{frame}");
        }
    }

    #[test]
    fn a_reply_that_does_not_parse_says_why_and_how_it_began() {
        // A call whose arguments come as an object, not the string the
        // Responses API sends.
        let raw = r#"{"status":"completed","output":[{"type":"function_call","name":"echo","call_id":"x","arguments":{"text":"hi"}}]}"#;
        let error = parse_completion(&RawValue::from_string(raw.into()).unwrap(), "").unwrap_err();
        assert_eq!(error.code, "invalid_json");
        let detail = error.detail.unwrap();
        assert!(
            detail.contains("invalid type: map, expected a string"),
            "{detail}"
        );
        assert!(
            detail.contains(r#"it began {"type":"function_call","name":"echo""#),
            "{detail}"
        );
        // A frame is quoted only so far, its line breaks as spaces.
        let mut state = State::default();
        let frame = format!("{{\"type\":\n{}", "x".repeat(4096));
        let detail = state.frame(frame.as_bytes()).unwrap_err().detail.unwrap();
        assert!(detail.contains(r#"it began {"type": xx"#), "{detail}");
        assert!(
            detail.ends_with('…') && detail.len() < 1024,
            "{}",
            detail.len()
        );
    }
    #[test]
    fn a_custom_tool_call_reads_as_its_input() {
        let raw = r#"{"status":"completed","output":[{"type":"custom_tool_call","id":"ctc_1","call_id":"c1","name":"apply_patch","input":"*** Begin Patch\n*** End Patch"}]}"#;
        let completion = parse_completion(&RawValue::from_string(raw.into()).unwrap(), "").unwrap();
        assert_eq!(
            completion.calls,
            [ToolCall {
                name: "apply_patch".into(),
                call_id: "c1".into(),
                arguments: json!({"input":"*** Begin Patch\n*** End Patch"}).to_string(),
                custom: true,
            }]
        );
        // Stored as the provider sent it, so it replays unchanged.
        let item = &raw[raw.find("[{").unwrap() + 1..raw.len() - 2];
        assert_eq!(completion.items[0], item.as_bytes());
    }
    #[test]
    fn web_run_reads_as_web_search_and_hosted_searches_are_kept() {
        let raw = r#"{"status":"completed","output":[{"type":"web_search_call","id":"ws_1","status":"completed","action":{"type":"search","query":"rust"}},{"type":"function_call","namespace":"web","name":"run","call_id":"c1","arguments":"{\"search_query\":[{\"q\":\"rust\"}]}"},{"type":"function_call","namespace":"other","name":"run","call_id":"c2","arguments":"{}"}]}"#;
        let completion = parse_completion(&RawValue::from_string(raw.into()).unwrap(), "").unwrap();
        assert_eq!(completion.calls[0].name, "web_search");
        assert_eq!(
            completion.calls[0].arguments,
            r#"{"search_query":[{"q":"rust"}]}"#
        );
        // Only Codex's own namespace is search.
        assert_eq!(completion.calls[1].name, "run");
        // The hosted search and the call replay as the provider sent them.
        assert_eq!(completion.items.len(), 3);
        let search: Value = serde_json::from_slice(&completion.items[0]).unwrap();
        assert_eq!(search["type"], "web_search_call");
        let call: Value = serde_json::from_slice(&completion.items[1]).unwrap();
        assert_eq!(
            (&call["namespace"], &call["name"]),
            (&json!("web"), &json!("run"))
        );
    }
    #[test]
    fn a_call_id_over_the_bound_fails_by_its_encoded_size() {
        let completion = |id: &str| {
            let call = json!({"type":"function_call","name":"echo","call_id":id,"arguments":"{}"});
            let raw = json!({"status":"completed","output":[call]}).to_string();
            parse_completion(&RawValue::from_string(raw).unwrap(), "")
        };
        assert!(completion(&"c".repeat(MAX_CALL_ID)).is_ok());
        // Each control character encodes as six bytes. A NUL would end the
        // id in a printed command, naming another call.
        let escaped = "\u{1}".repeat(MAX_CALL_ID / 6 + 1);
        for id in ["c".repeat(MAX_CALL_ID + 1), escaped, "x\0y".into()] {
            assert_eq!(completion(&id).unwrap_err().code, "invalid_tool_call_id");
        }
    }
    #[test]
    fn each_delta_names_the_output_item_it_streams() {
        let mut state = State::default();
        let mut index = |frame: &str| match state.frame(frame.as_bytes()).unwrap() {
            Frame::Delta(Delta::Text(index, _) | Delta::Thinking(index, _)) => index,
            frame => panic!("{frame:?}"),
        };
        let reasoning =
            r#"{"type":"response.reasoning_summary_text.delta","output_index":0,"delta":"hmm"}"#;
        let first =
            r#"{"type":"response.output_text.delta","output_index":1,"delta":"Let me check."}"#;
        let second = r#"{"type":"response.output_text.delta","output_index":2,"delta":"Done."}"#;
        let unnamed = r#"{"type":"response.output_text.delta","delta":"x"}"#;
        assert_eq!(
            [
                index(reasoning),
                index(first),
                index(second),
                index(unnamed)
            ],
            [0, 1, 2, 0]
        );
    }

    #[test]
    fn reasoning_summaries_stream_and_usage_is_extracted() {
        let mut state = State::default();
        let delta = state
            .frame(br#"{"type":"response.reasoning_summary_text.delta","delta":"hmm"}"#)
            .unwrap();
        assert!(matches!(delta, Frame::Delta(Delta::Thinking(_, t)) if t == "hmm"));
        state
            .frame(br#"{"type":"response.output_text.delta","delta":"ok"}"#)
            .unwrap();
        state.frame(br#"{"type":"response.completed","response":{"model":"gpt-test-2026-09-26","service_tier":"default","status":"completed","output":[{"type":"reasoning","summary":[]},{"type":"message","role":"assistant","content":[{"type":"output_text","text":"ok"}]}],"usage":{"input_tokens":10,"output_tokens":2,"input_tokens_details":{"cached_tokens":4}}}}"#).unwrap();
        let completion = state.finish().unwrap();
        assert_eq!(completion.items.len(), 2);
        assert_eq!(
            completion.usage,
            Some(Usage {
                input_tokens: 10,
                output_tokens: 2,
                cached_input_tokens: 4,
                cache_write_tokens: 0,
                cache_write_1h_tokens: 0,
                sent_ms: 0,
                models: Vec::new(),
                served_model: "gpt-test-2026-09-26".to_owned(),
                context_window: 0,
                speed: Some(Speed::Standard),
            })
        );
        // A response that failed after billing still names what ran.
        let mut state = State::default();
        state
            .frame(br#"{"type":"response.incomplete","response":{"model":"gpt-test-2026-09-26","status":"incomplete","output":[],"usage":{"input_tokens":10,"output_tokens":2}}}"#)
            .unwrap_err();
        assert_eq!(state.usage().unwrap().served_model, "gpt-test-2026-09-26");
        let failed = State::default()
            .frame(br#"{"type":"response.failed","response":{"error":{"message":"quota"}}}"#)
            .unwrap_err();
        assert_eq!(
            (failed.code.as_str(), failed.detail.as_deref()),
            ("provider_incomplete", Some("quota"))
        );
        let spent = State::default()
            .frame(br#"{"type":"response.failed","response":{"error":{"code":"subscription_sharing_usage_limit_exceeded","message":"Usage limit reached."}}}"#)
            .unwrap_err();
        assert_eq!(spent.code, "provider_quota_exhausted");
        let unavailable = State::default()
            .frame(br#"{"type":"response.failed","response":{"error":{"code":"subscription_sharing_usage_unavailable","message":"Try again later."}}}"#)
            .unwrap_err();
        assert_eq!(unavailable.code, "provider_plan_unavailable");
        let unavailable = State::default()
            .frame(br#"{"type":"error","code":"subscription_sharing_user_unavailable","message":"Try again later."}"#)
            .unwrap_err();
        assert_eq!(unavailable.code, "provider_plan_unavailable");
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
    #[test]
    fn overloads_and_server_errors_inside_the_stream_are_the_providers() {
        for (frame, code) in [
            (
                r#"{"type":"response.failed","response":{"error":{"code":"server_is_overloaded","message":"busy"}}}"#,
                "provider_overloaded",
            ),
            (
                r#"{"type":"error","error":{"code":"server_is_overloaded","message":"busy"}}"#,
                "provider_overloaded",
            ),
            (
                r#"{"type":"response.failed","response":{"error":{"code":"slow_down","message":"busy"}}}"#,
                "provider_rate_limited",
            ),
            (
                r#"{"type":"response.failed","response":{"error":{"code":"server_error","message":"busy"}}}"#,
                "provider_server_error",
            ),
            (
                r#"{"type":"response.incomplete","response":{"incomplete_details":{"reason":"max_output_tokens"}}}"#,
                "provider_incomplete",
            ),
        ] {
            let error = State::default().frame(frame.as_bytes()).unwrap_err();
            assert_eq!(error.code, code, "{frame}");
        }
    }
    #[test]
    fn a_cyber_policy_failure_is_a_refusal_with_the_provider_s_words() {
        let failed = r#"{"type":"response.failed","response":{"status":"failed",
            "error":{"code":"cyber_policy","message":"This request was flagged."},
            "usage":{"input_tokens":10,"output_tokens":120}}}"#
            .replace('\n', "");
        let mut state = State::default();
        state
            .frame(br#"{"type":"response.output_item.added","output_index":0,"item":{"type":"message","role":"assistant","content":[]}}"#)
            .unwrap();
        let error = state.frame(failed.as_bytes()).unwrap_err();
        assert_eq!(error.code, "provider_refusal");
        assert_eq!(error.detail.as_deref(), Some("category cyber"));
        assert!(super::super::refused_mid_stream(&error));
        assert_eq!(
            serde_json::Value::Object(*error.facts.unwrap()),
            serde_json::json!({"category":"cyber","explanation":"This request was flagged.",
                "mid_stream":true,"written_tokens":120})
        );
        // The model's own refusal, written whole, says its words.
        let refused = RawValue::from_string(r#"{"status":"completed","output":[{"type":"message","role":"assistant","content":[{"type":"refusal","refusal":"I can't help with that."}]}],"usage":{"input_tokens":4,"output_tokens":7}}"#.into()).unwrap();
        let mut usage = None;
        let error = parse_completion_with_usage(&refused, Vec::new(), "", &mut usage, &mut None)
            .unwrap_err();
        assert_eq!(
            serde_json::Value::Object(*error.facts.unwrap()),
            serde_json::json!({"explanation":"I can't help with that.","mid_stream":true,
                "written_tokens":7})
        );
        // Before anything streamed, and as an `error` event.
        let error = State::default()
            .frame(br#"{"type":"error","code":"cyber_policy","message":"Flagged."}"#)
            .unwrap_err();
        assert_eq!(error.code, "provider_refusal");
        assert!(!super::super::refused_mid_stream(&error));
        assert_eq!(error.facts.unwrap()["explanation"], "Flagged.");
    }
    #[test]
    fn a_reply_cut_at_max_output_tokens_names_what_it_was_writing() {
        let incomplete = r#"{"type":"response.incomplete","response":{"status":"incomplete",
            "incomplete_details":{"reason":"max_output_tokens"},"output":[],
            "usage":{"input_tokens":10,"output_tokens":32000}}}"#
            .replace('\n', "");
        let facts = |error: Error| {
            assert!(super::super::was_cut_short(&error));
            assert_eq!(error.detail.as_deref(), Some("max_output_tokens"));
            serde_json::Value::Object(*error.facts.unwrap())
        };
        // The item the response began, by the tool it calls.
        let mut state = State::default();
        state
            .frame(br#"{"type":"response.output_item.added","output_index":0,"item":{"type":"function_call","name":"write","call_id":"c1","arguments":""}}"#)
            .unwrap();
        state
            .frame(br#"{"type":"response.function_call_arguments.delta","output_index":0,"delta":"{\"path\""}"#)
            .unwrap();
        assert_eq!(
            facts(state.frame(incomplete.as_bytes()).unwrap_err()),
            serde_json::json!({"limit":"max_tokens","output_tokens":32000,"in":"write"})
        );
        // Words; and with nothing begun, the last item the response holds.
        let mut state = State::default();
        state
            .frame(br#"{"type":"response.output_item.added","output_index":0,"item":{"type":"message","role":"assistant","content":[]}}"#)
            .unwrap();
        assert_eq!(
            facts(state.frame(incomplete.as_bytes()).unwrap_err())["in"],
            "text"
        );
        let held = incomplete.replace(
            r#""output":[]"#,
            r#""output":[{"type":"reasoning","summary":[]}]"#,
        );
        assert_eq!(
            facts(State::default().frame(held.as_bytes()).unwrap_err())["in"],
            "reasoning"
        );
        // Nothing said: no `in`.
        assert!(
            facts(State::default().frame(incomplete.as_bytes()).unwrap_err())
                .get("in")
                .is_none()
        );
    }
}
