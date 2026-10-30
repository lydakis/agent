//! Anthropic Messages streaming: the assistant message is reconstructed from
//! content-block events and stored as one native item, including thinking
//! signatures so tool-using turns can continue.
use super::{
    Completion, Delta, Frame, MAX_OUTPUT, ModelTokens, ToolCall, Usage, detail_of, encoded_len,
    valid_call_id,
};
use crate::{Error, Result, fail, fail_with};
use bytes::Bytes;
use serde_json::{Value, json};

enum Block {
    Text {
        text: String,
        /// Where its words came from, when a search the provider ran
        /// gave them: kept so the reply replays as it was written.
        citations: Vec<Value>,
    },
    Thinking {
        thinking: String,
        signature: String,
    },
    Redacted(Value),
    ToolUse {
        id: String,
        name: String,
        input: String,
    },
    /// Where a server-side fallback switched models. Kept in place: the
    /// API checks the thinking around it by its position.
    Fallback(Value),
    /// A tool the provider runs itself, such as its web search: no call
    /// for the runtime, replayed as it came with its input.
    ServerTool {
        block: Value,
        input: String,
    },
    /// What a provider-run tool found, whole in its start event.
    ServerResult(Value),
}

/// Input usage from a Messages `usage` object. Anthropic reports cache
/// reads and cache writes outside input_tokens; count every processed input
/// token, as the Responses family does, and keep reads and writes separately
/// since each is billed at its own rate. `hour` says every cache marker of
/// the request asked for an hour, for a report that does not split writes.
pub(crate) fn usage(usage: &Value, hour: bool) -> Usage {
    let (input, read, (created, hourly)) = tokens(usage, hour);
    Usage {
        input_tokens: input,
        output_tokens: usage["output_tokens"].as_u64().unwrap_or(0),
        cached_input_tokens: read,
        cache_write_tokens: created,
        cache_write_1h_tokens: hourly,
        ..Usage::default()
    }
}

/// A whole message's usage and the model that answered it: per attempt
/// when a server-side fallback ran, as the stream's final usage is read.
pub(crate) fn message_usage(message: &Value, hour: bool) -> Usage {
    let report = &message["usage"];
    let mut state = State::new(hour);
    state.usage = usage(report, hour);
    state.usage.served_model = served(message);
    if let Some(iterations) = report["iterations"].as_array() {
        state.iterations(iterations);
    }
    state.usage
}

/// The model a message or an attempt names.
fn served(value: &Value) -> String {
    value["model"].as_str().unwrap_or_default().to_owned()
}

/// All input, cache reads, and cache writes with the hour-long part of them.
fn tokens(usage: &Value, hour: bool) -> (u64, u64, (u64, u64)) {
    let count = |key: &str| usage[key].as_u64().unwrap_or(0);
    let (read, created) = (
        count("cache_read_input_tokens"),
        count("cache_creation_input_tokens"),
    );
    let hourly = match usage["cache_creation"]["ephemeral_1h_input_tokens"].as_u64() {
        Some(hourly) => hourly.min(created),
        None if hour => created,
        None => 0,
    };
    (
        count("input_tokens") + read + created,
        read,
        (created, hourly),
    )
}

#[derive(Default)]
pub struct State {
    blocks: Vec<Block>,
    bytes: usize,
    stop_reason: Option<String>,
    usage: Usage,
    saw_usage: bool,
    done: bool,
    thinking_dropped: usize,
    fallbacks: Vec<(Option<String>, String)>,
    stop_details: Option<String>,
    /// What a refusal names for a retry: its category and the credit that
    /// prices a retry on another model as if it had run there all along.
    refusal: serde_json::Map<String, Value>,
    /// The request cached for an hour: see [`usage`].
    hour: bool,
}

impl State {
    pub fn new(hour: bool) -> Self {
        Self {
            hour,
            ..Self::default()
        }
    }
    fn account(&mut self, len: usize) -> Result<()> {
        self.bytes += len;
        if self.bytes > MAX_OUTPUT {
            return fail("output_limit");
        }
        Ok(())
    }
    fn block(&mut self, index: &Value) -> Result<&mut Block> {
        index
            .as_u64()
            .and_then(|i| self.blocks.get_mut(i as usize))
            .ok_or(Error::new("invalid_content_index"))
    }
    pub fn frame(&mut self, frame: &[u8]) -> Result<Frame> {
        let event: Value = serde_json::from_slice(frame)?;
        if self.done {
            return fail("event_after_completion");
        }
        match event["type"].as_str() {
            Some("message_start") => {
                self.usage = usage(&event["message"]["usage"], self.hour);
                self.usage.served_model = served(&event["message"]);
                self.saw_usage = true;
                self.dropped(&event["message"]["input_transformations"]);
                Ok(Frame::Quiet)
            }
            Some("content_block_start") => {
                if event["index"].as_u64() != Some(self.blocks.len() as u64) {
                    return fail("invalid_content_index");
                }
                if self.blocks.len() >= 64 {
                    return fail("output_limit");
                }
                let block = &event["content_block"];
                let string = |key: &str| {
                    block[key]
                        .as_str()
                        .map(str::to_owned)
                        .ok_or(Error::new("invalid_content"))
                };
                let kind = block["type"].as_str().unwrap_or_default();
                let result = kind.ends_with("_tool_result");
                if matches!(kind, "redacted_thinking" | "fallback" | "server_tool_use") || result {
                    self.account(frame.len())?;
                }
                let started = match block["type"].as_str() {
                    Some("text") => Block::Text {
                        text: String::new(),
                        citations: Vec::new(),
                    },
                    Some("thinking") => Block::Thinking {
                        thinking: String::new(),
                        signature: String::new(),
                    },
                    Some("redacted_thinking") => Block::Redacted(block.clone()),
                    Some("fallback") => {
                        let to = block["to"]["model"]
                            .as_str()
                            .ok_or(Error::new("invalid_content"))?;
                        let from = block["from"]["model"].as_str().map(str::to_owned);
                        self.fallbacks.push((from, to.to_owned()));
                        Block::Fallback(block.clone())
                    }
                    Some("server_tool_use") => {
                        string("id")?;
                        string("name")?;
                        Block::ServerTool {
                            block: block.clone(),
                            input: String::new(),
                        }
                    }
                    _ if result => Block::ServerResult(block.clone()),
                    Some("tool_use") => {
                        // Stored in the item, and a gated call's id again in
                        // its announcement, so they count like any output.
                        let (id, name) = (string("id")?, string("name")?);
                        self.account(encoded_len(&id) + encoded_len(&name))?;
                        Block::ToolUse {
                            id,
                            name,
                            input: String::new(),
                        }
                    }
                    _ => return fail("unsupported_content"),
                };
                self.blocks.push(started);
                Ok(Frame::Quiet)
            }
            Some("content_block_delta") => {
                let delta = &event["delta"];
                let kind = delta["type"].as_str().unwrap_or_default().to_owned();
                let text = |key: &str| {
                    delta[key]
                        .as_str()
                        .map(str::to_owned)
                        .ok_or(Error::new("invalid_content"))
                };
                let part = match kind.as_str() {
                    "text_delta" => text("text")?,
                    "thinking_delta" => text("thinking")?,
                    "input_json_delta" => text("partial_json")?,
                    "signature_delta" => text("signature")?,
                    "citations_delta" => {
                        let citation = delta["citation"].clone();
                        if !citation.is_object() {
                            return fail("invalid_content");
                        }
                        self.account(frame.len())?;
                        return match self.block(&event["index"])? {
                            Block::Text { citations, .. } => {
                                citations.push(citation);
                                Ok(Frame::Quiet)
                            }
                            _ => fail("invalid_content"),
                        };
                    }
                    _ => return Ok(Frame::Quiet),
                };
                // Partial JSON and signatures are stored as they arrive;
                // text and thinking are escaped again when stored.
                self.account(match kind.as_str() {
                    "text_delta" | "thinking_delta" => encoded_len(&part),
                    _ => part.len(),
                })?;
                // Every block is kept in the one assistant message, the
                // response's only output item.
                match (kind.as_str(), self.block(&event["index"])?) {
                    ("text_delta", Block::Text { text, .. }) => {
                        text.push_str(&part);
                        Ok(Frame::Delta(Delta::Text(0, part)))
                    }
                    ("thinking_delta", Block::Thinking { thinking, .. }) => {
                        thinking.push_str(&part);
                        Ok(Frame::Delta(Delta::Thinking(0, part)))
                    }
                    ("signature_delta", Block::Thinking { signature, .. }) => {
                        signature.push_str(&part);
                        Ok(Frame::Quiet)
                    }
                    (
                        "input_json_delta",
                        Block::ToolUse { input, .. } | Block::ServerTool { input, .. },
                    ) => {
                        input.push_str(&part);
                        Ok(Frame::Quiet)
                    }
                    _ => fail("invalid_content"),
                }
            }
            Some("message_delta") => {
                if let Some(reason) = event["delta"]["stop_reason"].as_str() {
                    self.stop_reason = Some(reason.to_owned());
                }
                if let Some(output) = event["usage"]["output_tokens"].as_u64() {
                    self.usage.output_tokens = output;
                }
                if let Some(iterations) = event["usage"]["iterations"].as_array() {
                    self.iterations(iterations);
                }
                let details = &event["delta"]["stop_details"];
                if details.is_object() {
                    let part = |key: &str, label: &str| {
                        details[key]
                            .as_str()
                            .map(|value| format!("{label} {value}"))
                    };
                    let parts: Vec<String> = [
                        part("category", "category"),
                        part("recommended_model", "retry on"),
                    ]
                    .into_iter()
                    .flatten()
                    .collect();
                    self.stop_details = (!parts.is_empty()).then(|| parts.join("; "));
                    for key in ["category", "fallback_credit_token"] {
                        if let Some(value) = details[key].as_str() {
                            self.refusal.insert(key.to_owned(), json!(value));
                        }
                    }
                }
                // Sent again after a server-side fallback.
                self.dropped(&event["input_transformations"]);
                self.dropped(&event["delta"]["input_transformations"]);
                Ok(Frame::Quiet)
            }
            Some("message_stop") => {
                self.done = true;
                Ok(Frame::Quiet)
            }
            // An internal error after the stream began is told apart: the
            // runtime may resend without what the request replayed. An
            // overload or a rate limit mid-stream is the 529 or 429 it would
            // have been before the stream began, and is retried as one.
            Some("error") => {
                let code = match event["error"]["type"].as_str() {
                    Some("api_error") => "provider_server_error",
                    Some("overloaded_error") => "provider_http_529",
                    Some("rate_limit_error") => "provider_rate_limited",
                    _ => "provider_error",
                };
                match detail_of(&event) {
                    Some(detail) => fail_with(code, detail),
                    None => fail(code),
                }
            }
            // Sent to hold an idle stream open; it is not progress.
            Some("ping") => Ok(Frame::Keepalive),
            _ => Ok(Frame::Quiet), // content_block_stop, unknown metadata
        }
    }
    /// Per-attempt usage after a server-side fallback, or on a turn a
    /// sticky fallback served. An attempt declined before any output is not
    /// billed; every other attempt is, at its own model's rates.
    fn iterations(&mut self, iterations: &[Value]) {
        let fallback = iterations.len() > 1
            || iterations
                .iter()
                .any(|entry| entry["type"] == "fallback_message");
        if !fallback {
            return;
        }
        let count = |entry: &Value, key: &str| entry[key].as_u64().unwrap_or(0);
        let last = iterations.len() - 1;
        // The last attempt wrote the answer the turn keeps.
        let answered = served(&iterations[last]);
        let served_model = match answered.is_empty() {
            true => std::mem::take(&mut self.usage.served_model),
            false => answered,
        };
        let models: Vec<ModelTokens> = iterations
            .iter()
            .enumerate()
            .filter(|(index, entry)| *index == last || count(entry, "output_tokens") > 0)
            .map(|(_, entry)| {
                let (input, read, (written, hourly)) = tokens(entry, self.hour);
                ModelTokens {
                    model: served(entry),
                    provider: None,
                    input_tokens: input,
                    output_tokens: count(entry, "output_tokens"),
                    cached_input_tokens: read,
                    cache_write_tokens: written,
                    cache_write_1h_tokens: hourly,
                }
            })
            .collect();
        self.usage = Usage {
            input_tokens: models.iter().map(|m| m.input_tokens).sum(),
            output_tokens: models.iter().map(|m| m.output_tokens).sum(),
            cached_input_tokens: models.iter().map(|m| m.cached_input_tokens).sum(),
            cache_write_tokens: models.iter().map(|m| m.cache_write_tokens).sum(),
            cache_write_1h_tokens: models.iter().map(|m| m.cache_write_1h_tokens).sum(),
            models,
            served_model,
            ..Usage::default()
        };
        self.saw_usage = true;
    }
    /// The thinking blocks the request lost to the binding check.
    fn dropped(&mut self, transformations: &Value) {
        if let Some(list) = transformations.as_array() {
            // Only an edit under replayed thinking; a block another model
            // wrote, as after a fallback, is dropped as the API means it to.
            self.thinking_dropped = list
                .iter()
                .filter(|t| {
                    t["type"] == "thinking_dropped" && t["reason"] == "prefix_binding_mismatch"
                })
                .count();
        }
    }
    pub fn usage(&self) -> Option<Usage> {
        self.saw_usage.then(|| self.usage.clone())
    }
    pub fn finish(self) -> Result<Completion> {
        if !self.done {
            return fail("missing_completion");
        }
        let paused = self.stop_reason.as_deref() == Some("pause_turn");
        match self.stop_reason.as_deref() {
            Some("end_turn" | "stop_sequence" | "tool_use" | "pause_turn") => {}
            Some("max_tokens") => return fail_with("provider_incomplete", "max_tokens"),
            // With a fallback requested, the whole chain declined.
            Some("refusal") => {
                // The models already switched to ran and are billed too, so
                // the failure names them.
                let parts: Vec<String> = self
                    .fallbacks
                    .iter()
                    .map(|(_, to)| format!("fell back to {to}"))
                    .chain(self.stop_details)
                    .collect();
                let error = match parts.is_empty() {
                    true => Error::new("provider_refusal"),
                    false => Error::with("provider_refusal", parts.join("; ")),
                };
                return Err(match self.refusal.is_empty() {
                    true => error,
                    false => error.facts(Value::Object(self.refusal)),
                });
            }
            other => {
                return fail_with(
                    "provider_incomplete",
                    other.unwrap_or("missing stop reason"),
                );
            }
        }
        let mut content = Vec::with_capacity(self.blocks.len());
        let mut calls: Vec<ToolCall> = Vec::new();
        // A model that declined partway through leaves its partial output
        // before the last fallback block. Its text stays (an empty block
        // would be rejected); its thinking and tool calls are not sent back,
        // and its calls never run.
        let switch = self
            .blocks
            .iter()
            .rposition(|block| matches!(block, Block::Fallback(_)))
            .unwrap_or(0);
        for (index, block) in self.blocks.into_iter().enumerate() {
            if index < switch
                && !matches!(&block, Block::Text { text, .. } if !text.is_empty())
                && !matches!(block, Block::Fallback(_))
            {
                continue;
            }
            content.push(match block {
                Block::Fallback(block) => block,
                Block::Text { text, citations } if citations.is_empty() => {
                    json!({"type":"text","text":text})
                }
                Block::Text { text, citations } => {
                    json!({"type":"text","text":text,"citations":citations})
                }
                Block::ServerTool { mut block, input } => {
                    block["input"] = match input.trim() {
                        "" => json!({}),
                        input => serde_json::from_str(input)
                            .map_err(|_| Error::new("invalid_tool_arguments"))?,
                    };
                    block
                }
                Block::ServerResult(block) => block,
                Block::Thinking {
                    thinking,
                    signature,
                } => json!({"type":"thinking","thinking":thinking,"signature":signature}),
                Block::Redacted(block) => block,
                Block::ToolUse { id, name, input } => {
                    let arguments = if input.trim().is_empty() {
                        "{}".to_owned()
                    } else {
                        input
                    };
                    let parsed: Value = serde_json::from_str(&arguments)
                        .map_err(|_| Error::new("invalid_tool_arguments"))?;
                    if !valid_call_id(&id) || calls.iter().any(|c| c.call_id == id) {
                        return fail("invalid_tool_call_id");
                    }
                    calls.push(ToolCall {
                        custom: false,
                        name: name.clone(),
                        call_id: id.clone(),
                        arguments,
                    });
                    json!({"type":"tool_use","id":id,"name":name,"input":parsed})
                }
            });
        }
        // An empty reply is kept as no item at all, since Messages refuses an
        // assistant turn without content; the runtime decides whether it
        // ends the turn or fails it.
        let items = match content.is_empty() {
            true => Vec::new(),
            false => vec![Bytes::from(serde_json::to_vec(
                &json!({"role":"assistant","content":content}),
            )?)],
        };
        Ok(Completion {
            thinking_dropped: self.thinking_dropped,
            fallbacks: self.fallbacks,
            items,
            calls,
            usage: self.saw_usage.then_some(self.usage),
            paused,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::provider::MAX_CALL_ID;
    fn feed(state: &mut State, frames: &[&str]) -> Vec<String> {
        let mut deltas = Vec::new();
        for frame in frames {
            if let Frame::Delta(delta) = state.frame(frame.as_bytes()).unwrap() {
                deltas.push(match delta {
                    Delta::Text(_, t) => format!("text:{t}"),
                    Delta::Thinking(_, t) => format!("think:{t}"),
                });
            }
        }
        deltas
    }
    #[test]
    fn tool_use_ids_count_toward_the_output_limit() {
        let mut state = State::default();
        state
            .frame(br#"{"type":"message_start","message":{"usage":{"input_tokens":1}}}"#)
            .unwrap();
        // Each id is within its own bound; together they pass the output's.
        let mut failed = None;
        for index in 0..=MAX_OUTPUT / MAX_CALL_ID {
            let id = format!("{index:02}{}", "c".repeat(MAX_CALL_ID - 2));
            let start = json!({"type":"content_block_start","index":index,
                "content_block":{"type":"tool_use","id":id,"name":"shell","input":{}}});
            if let Err(error) = state.frame(start.to_string().as_bytes()) {
                failed = Some((index, error.code));
                break;
            }
        }
        // Twelve ids alone fill the limit, so the twelfth block's name passes it.
        assert_eq!(
            failed,
            Some((MAX_OUTPUT / MAX_CALL_ID - 1, "output_limit".to_owned()))
        );
    }
    #[test]
    fn a_tool_use_id_over_the_bound_fails() {
        for (id, code) in [
            ("c".repeat(MAX_CALL_ID), None),
            ("c".repeat(MAX_CALL_ID + 1), Some("invalid_tool_call_id")),
            ("x\0y".into(), Some("invalid_tool_call_id")),
        ] {
            let mut state = State::default();
            let start = json!({"type":"content_block_start","index":0,
                "content_block":{"type":"tool_use","id":id,"name":"shell","input":{}}});
            feed(
                &mut state,
                &[
                    r#"{"type":"message_start","message":{"usage":{"input_tokens":1}}}"#,
                    &start.to_string(),
                    r#"{"type":"message_delta","delta":{"stop_reason":"tool_use"},"usage":{"output_tokens":1}}"#,
                    r#"{"type":"message_stop"}"#,
                ],
            );
            assert_eq!(state.finish().err().map(|e| e.code).as_deref(), code);
        }
    }
    #[test]
    fn thinking_text_and_tool_use_become_one_native_assistant_item() {
        let mut state = State::default();
        let deltas = feed(
            &mut state,
            &[
                r#"{"type":"message_start","message":{"model":"claude-test-20260926","usage":{"input_tokens":12,"cache_read_input_tokens":3,"cache_creation_input_tokens":5}}}"#,
                r#"{"type":"content_block_start","index":0,"content_block":{"type":"thinking","thinking":""}}"#,
                r#"{"type":"content_block_delta","index":0,"delta":{"type":"thinking_delta","thinking":"plan"}}"#,
                r#"{"type":"content_block_delta","index":0,"delta":{"type":"signature_delta","signature":"sig"}}"#,
                r#"{"type":"content_block_stop","index":0}"#,
                r#"{"type":"content_block_start","index":1,"content_block":{"type":"text","text":""}}"#,
                r#"{"type":"content_block_delta","index":1,"delta":{"type":"text_delta","text":"run"}}"#,
                r#"{"type":"content_block_start","index":2,"content_block":{"type":"tool_use","id":"t1","name":"shell","input":{}}}"#,
                r#"{"type":"content_block_delta","index":2,"delta":{"type":"input_json_delta","partial_json":"{\"comm"}}"#,
                r#"{"type":"content_block_delta","index":2,"delta":{"type":"input_json_delta","partial_json":"and\":\"ls\"}"}}"#,
                r#"{"type":"message_delta","delta":{"stop_reason":"tool_use"},"usage":{"output_tokens":7}}"#,
                r#"{"type":"message_stop"}"#,
            ],
        );
        assert_eq!(deltas, ["think:plan", "text:run"]);
        let completion = state.finish().unwrap();
        let item: Value = serde_json::from_slice(&completion.items[0]).unwrap();
        assert_eq!(item["content"][0]["signature"], "sig");
        assert_eq!(item["content"][2]["input"]["command"], "ls");
        assert_eq!(completion.calls[0].arguments, r#"{"command":"ls"}"#);
        assert_eq!(completion.thinking_dropped, 0);
        assert_eq!(
            completion.usage,
            Some(Usage {
                input_tokens: 20,
                output_tokens: 7,
                cached_input_tokens: 3,
                cache_write_tokens: 5,
                cache_write_1h_tokens: 0,
                sent_ms: 0,
                models: Vec::new(),
                served_model: "claude-test-20260926".to_owned(),
                context_window: 0,
            })
        );
    }
    /// A hosted search comes back as its call, its results and cited text,
    /// all kept as sent; a turn the server paused is resent, not ended.
    #[test]
    fn hosted_search_blocks_and_citations_are_kept_and_a_pause_resumes() {
        let mut state = State::default();
        let deltas = feed(
            &mut state,
            &[
                r#"{"type":"message_start","message":{"usage":{"input_tokens":1}}}"#,
                r#"{"type":"content_block_start","index":0,"content_block":{"type":"server_tool_use","id":"srvtoolu_1","name":"web_search","input":{}}}"#,
                r#"{"type":"content_block_delta","index":0,"delta":{"type":"input_json_delta","partial_json":"{\"query\":"}}"#,
                r#"{"type":"content_block_delta","index":0,"delta":{"type":"input_json_delta","partial_json":"\"rust\"}"}}"#,
                r#"{"type":"content_block_stop","index":0}"#,
                r#"{"type":"content_block_start","index":1,"content_block":{"type":"web_search_tool_result","tool_use_id":"srvtoolu_1","content":[{"type":"web_search_result","url":"https://example.test","title":"T","encrypted_content":"e"}]}}"#,
                r#"{"type":"content_block_stop","index":1}"#,
                r#"{"type":"content_block_start","index":2,"content_block":{"type":"text","text":""}}"#,
                r#"{"type":"content_block_delta","index":2,"delta":{"type":"citations_delta","citation":{"type":"web_search_result_location","url":"https://example.test","title":"T","encrypted_index":"i","cited_text":"c"}}}"#,
                r#"{"type":"content_block_delta","index":2,"delta":{"type":"text_delta","text":"Rust is"}}"#,
                r#"{"type":"content_block_stop","index":2}"#,
                r#"{"type":"message_delta","delta":{"stop_reason":"pause_turn"},"usage":{"output_tokens":5}}"#,
                r#"{"type":"message_stop"}"#,
            ],
        );
        assert_eq!(deltas, ["text:Rust is"]);
        let completion = state.finish().unwrap();
        assert!(completion.paused);
        assert!(completion.calls.is_empty());
        let item: Value = serde_json::from_slice(&completion.items[0]).unwrap();
        assert_eq!(
            item["content"][0],
            json!({"type":"server_tool_use","id":"srvtoolu_1","name":"web_search","input":{"query":"rust"}})
        );
        assert_eq!(item["content"][1]["type"], "web_search_tool_result");
        assert_eq!(item["content"][1]["content"][0]["encrypted_content"], "e");
        assert_eq!(item["content"][2]["text"], "Rust is");
        assert_eq!(item["content"][2]["citations"][0]["encrypted_index"], "i");
    }
    /// A message's usage, as a refresh's is read, counts every attempt a
    /// server-side fallback billed, as the stream's final usage does.
    #[test]
    fn a_whole_messages_usage_counts_each_billed_attempt() {
        let report = json!({"input_tokens":0,"cache_read_input_tokens":9,"output_tokens":0,
            "iterations":[
                {"type":"message","model":"claude-opus-5-5","input_tokens":0,
                 "cache_read_input_tokens":9,"cache_creation_input_tokens":0,"output_tokens":0},
                {"type":"fallback_message","model":"claude-opus-4-8","input_tokens":2,
                 "cache_read_input_tokens":0,"cache_creation_input_tokens":7,"output_tokens":0}]});
        let usage = super::message_usage(&json!({"model":"claude-opus-5-5","usage":report}), false);
        assert_eq!(usage.models.len(), 1);
        assert_eq!(usage.models[0].model, "claude-opus-4-8");
        assert_eq!((usage.input_tokens, usage.cache_write_tokens), (9, 7));
        // The attempt that answered names the model, not the one asked.
        assert_eq!(usage.served_model, "claude-opus-4-8");
        let plain = super::message_usage(
            &json!({"model":"claude-opus-5-5","usage":{"cache_read_input_tokens":9}}),
            false,
        );
        assert_eq!((plain.input_tokens, plain.cached_input_tokens), (9, 9));
        assert_eq!(plain.served_model, "claude-opus-5-5");
    }
    /// Hour-long writes are read from the split when Anthropic reports one,
    /// and otherwise follow what the request asked for.
    #[test]
    fn hour_long_cache_writes_are_kept_apart() {
        let split = json!({"input_tokens":1,"cache_read_input_tokens":2,
            "cache_creation_input_tokens":7,
            "cache_creation":{"ephemeral_5m_input_tokens":3,"ephemeral_1h_input_tokens":4}});
        let plain = json!({"input_tokens":1,"cache_creation_input_tokens":7});
        let hourly = |usage: &Value, hour| {
            let usage = super::usage(usage, hour);
            (
                usage.input_tokens,
                usage.cache_write_tokens,
                usage.cache_write_1h_tokens,
            )
        };
        assert_eq!(hourly(&split, false), (10, 7, 4));
        assert_eq!(hourly(&split, true), (10, 7, 4));
        assert_eq!(hourly(&plain, true), (8, 7, 7));
        assert_eq!(hourly(&plain, false), (8, 7, 0));
    }
    #[test]
    fn dropped_thinking_is_counted_from_the_latest_report() {
        let start = r#"{"type":"message_start","message":{"usage":{"input_tokens":1},"input_transformations":[{"type":"thinking_dropped","path":"messages.1.content.0","reason":"prefix_binding_mismatch"},{"type":"other"}]}}"#;
        let text = r#"{"type":"content_block_start","index":0,"content_block":{"type":"text","text":"ok"}}"#;
        let delta = r#"{"type":"message_delta","delta":{"stop_reason":"end_turn"},"usage":{"output_tokens":1}}"#;
        let stop = r#"{"type":"message_stop"}"#;
        let mut state = State::default();
        feed(&mut state, &[start, text, delta, stop]);
        assert_eq!(state.finish().unwrap().thinking_dropped, 1);
        // After a fallback the delta repeats the whole list for the final
        // request. A block another model wrote is dropped as the API means
        // to, and an unknown reason is not counted either.
        let fallback = r#"{"type":"message_delta","delta":{"stop_reason":"end_turn"},"usage":{"output_tokens":1},"input_transformations":[{"type":"thinking_dropped","reason":"prefix_binding_mismatch"},{"type":"thinking_dropped","reason":"prefix_binding_mismatch"},{"type":"thinking_dropped","reason":"model_binding_mismatch"},{"type":"thinking_dropped","reason":"later"}]}"#;
        let mut state = State::default();
        feed(&mut state, &[start, text, fallback, stop]);
        assert_eq!(state.finish().unwrap().thinking_dropped, 2);
    }
    #[test]
    fn a_fallback_keeps_the_switch_and_prices_each_billed_attempt() {
        let mut state = State::default();
        feed(
            &mut state,
            &[
                r#"{"type":"message_start","message":{"model":"claude-opus-5-5","usage":{"input_tokens":5}}}"#,
                r#"{"type":"content_block_start","index":0,"content_block":{"type":"thinking","thinking":""}}"#,
                r#"{"type":"content_block_delta","index":0,"delta":{"type":"thinking_delta","thinking":"old"}}"#,
                r#"{"type":"content_block_start","index":1,"content_block":{"type":"text","text":""}}"#,
                r#"{"type":"content_block_start","index":2,"content_block":{"type":"text","text":""}}"#,
                r#"{"type":"content_block_delta","index":2,"delta":{"type":"text_delta","text":"partial"}}"#,
                r#"{"type":"content_block_start","index":3,"content_block":{"type":"tool_use","id":"t0","name":"shell","input":{}}}"#,
                r#"{"type":"content_block_delta","index":3,"delta":{"type":"input_json_delta","partial_json":"{\"comm"}}"#,
                r#"{"type":"content_block_start","index":4,"content_block":{"type":"fallback","from":{"model":"claude-opus-5-5"},"to":{"model":"claude-opus-4-8"}}}"#,
                r#"{"type":"content_block_start","index":5,"content_block":{"type":"text","text":""}}"#,
                r#"{"type":"content_block_delta","index":5,"delta":{"type":"text_delta","text":"done"}}"#,
                r#"{"type":"content_block_start","index":6,"content_block":{"type":"tool_use","id":"t1","name":"shell","input":{}}}"#,
                r#"{"type":"content_block_delta","index":6,"delta":{"type":"input_json_delta","partial_json":"{}"}}"#,
                r#"{"type":"message_delta","delta":{"stop_reason":"tool_use"},"usage":{"output_tokens":7,"iterations":[{"type":"message","model":"claude-opus-5-5","input_tokens":5,"output_tokens":3,"cache_read_input_tokens":2,"cache_creation_input_tokens":0},{"type":"fallback_message","model":"claude-opus-4-8","input_tokens":9,"output_tokens":7,"cache_read_input_tokens":0,"cache_creation_input_tokens":4}]}}"#,
                r#"{"type":"message_stop"}"#,
            ],
        );
        let completion = state.finish().unwrap();
        let item: Value = serde_json::from_slice(&completion.items[0]).unwrap();
        let kinds: Vec<&str> = item["content"]
            .as_array()
            .unwrap()
            .iter()
            .map(|b| b["type"].as_str().unwrap())
            .collect();
        // The declined model's thinking, empty text and unfinished call are gone.
        assert_eq!(kinds, ["text", "fallback", "text", "tool_use"]);
        assert_eq!(item["content"][1]["to"]["model"], "claude-opus-4-8");
        assert_eq!(completion.calls.len(), 1);
        assert_eq!(completion.calls[0].call_id, "t1");
        assert_eq!(
            completion.fallbacks,
            [(
                Some("claude-opus-5-5".to_owned()),
                "claude-opus-4-8".to_owned()
            )]
        );
        let usage = completion.usage.unwrap();
        assert_eq!((usage.input_tokens, usage.output_tokens), (20, 10));
        assert_eq!(usage.cached_input_tokens, 2);
        assert_eq!(usage.models.len(), 2);
        assert_eq!(usage.models[1].model, "claude-opus-4-8");
        assert_eq!(usage.models[1].input_tokens, 13);
        // The answer the turn keeps came from the model fallen back to.
        assert_eq!(usage.served_model, "claude-opus-4-8");
        // Cache writes stay with the attempt that made them.
        assert_eq!(usage.cache_write_tokens, 4);
        assert_eq!(usage.models[1].cache_write_tokens, 4);
    }
    #[test]
    fn a_decline_before_output_is_not_billed_and_a_chain_refusal_says_why() {
        let mut state = State::default();
        feed(
            &mut state,
            &[
                r#"{"type":"message_start","message":{"model":"claude-opus-4-8","usage":{"input_tokens":9}}}"#,
                r#"{"type":"content_block_start","index":0,"content_block":{"type":"fallback","from":{"model":"claude-opus-5-5"},"to":{"model":"claude-opus-4-8"}}}"#,
                r#"{"type":"content_block_start","index":1,"content_block":{"type":"text","text":"ok"}}"#,
                r#"{"type":"message_delta","delta":{"stop_reason":"end_turn"},"usage":{"output_tokens":1,"iterations":[{"type":"message","model":"claude-opus-5-5","input_tokens":9,"output_tokens":0,"cache_read_input_tokens":0,"cache_creation_input_tokens":0},{"type":"fallback_message","model":"claude-opus-4-8","input_tokens":9,"output_tokens":1,"cache_read_input_tokens":0,"cache_creation_input_tokens":0}]}}"#,
                r#"{"type":"message_stop"}"#,
            ],
        );
        let usage = state.finish().unwrap().usage.unwrap();
        assert_eq!((usage.input_tokens, usage.output_tokens), (9, 1));
        assert_eq!(usage.models.len(), 1);
        // A turn a sticky fallback served has no block but is still priced
        // at the model that ran it; a plain turn carries no split.
        let mut state = State::default();
        feed(
            &mut state,
            &[
                r#"{"type":"content_block_start","index":0,"content_block":{"type":"text","text":"ok"}}"#,
                r#"{"type":"message_delta","delta":{"stop_reason":"end_turn"},"usage":{"output_tokens":1,"iterations":[{"type":"fallback_message","model":"claude-opus-4-8","input_tokens":4,"output_tokens":1,"cache_read_input_tokens":0,"cache_creation_input_tokens":0}]}}"#,
                r#"{"type":"message_stop"}"#,
            ],
        );
        assert_eq!(
            state.finish().unwrap().usage.unwrap().models[0].model,
            "claude-opus-4-8"
        );
        let mut state = State::default();
        feed(
            &mut state,
            &[
                r#"{"type":"message_start","message":{"usage":{"input_tokens":4}}}"#,
                r#"{"type":"content_block_start","index":0,"content_block":{"type":"text","text":"ok"}}"#,
                r#"{"type":"message_delta","delta":{"stop_reason":"end_turn"},"usage":{"output_tokens":1,"iterations":[{"type":"message","model":"claude-opus-5-5","input_tokens":4,"output_tokens":1,"cache_read_input_tokens":0,"cache_creation_input_tokens":0}]}}"#,
                r#"{"type":"message_stop"}"#,
            ],
        );
        assert!(state.finish().unwrap().usage.unwrap().models.is_empty());
        let mut state = State::default();
        feed(
            &mut state,
            &[
                r#"{"type":"message_delta","delta":{"stop_reason":"refusal","stop_details":{"type":"refusal","category":"cyber","recommended_model":"claude-opus-4-8"}},"usage":{"output_tokens":0}}"#,
                r#"{"type":"message_stop"}"#,
            ],
        );
        let error = state.finish().unwrap_err();
        assert_eq!(error.code, "provider_refusal");
        assert_eq!(
            error.detail.as_deref(),
            Some("category cyber; retry on claude-opus-4-8")
        );
        assert_eq!(
            serde_json::to_value(error.facts.unwrap()).unwrap(),
            json!({"category":"cyber"})
        );
        // The credit for a retry elsewhere is a fact, not part of the detail.
        let mut state = State::default();
        feed(
            &mut state,
            &[
                r#"{"type":"message_delta","delta":{"stop_reason":"refusal","stop_details":{"type":"refusal","category":"cyber","fallback_credit_token":"credit","fallback_has_prefill_claim":false}},"usage":{"output_tokens":0}}"#,
                r#"{"type":"message_stop"}"#,
            ],
        );
        let error = state.finish().unwrap_err();
        assert_eq!(error.detail.as_deref(), Some("category cyber"));
        assert_eq!(
            serde_json::to_value(error.facts.unwrap()).unwrap(),
            json!({"category":"cyber","fallback_credit_token":"credit"})
        );
        // A refusal naming nothing has no facts.
        let mut state = State::default();
        feed(
            &mut state,
            &[
                r#"{"type":"message_delta","delta":{"stop_reason":"refusal","stop_details":{"type":"refusal","category":null}},"usage":{"output_tokens":0}}"#,
                r#"{"type":"message_stop"}"#,
            ],
        );
        let error = state.finish().unwrap_err();
        assert_eq!((error.detail, error.facts), (None, None));
        // A chain that switched models and still declined names the switch.
        let mut state = State::default();
        feed(
            &mut state,
            &[
                r#"{"type":"content_block_start","index":0,"content_block":{"type":"fallback","from":{"model":"claude-opus-5-5"},"to":{"model":"claude-opus-4-8"}}}"#,
                r#"{"type":"message_delta","delta":{"stop_reason":"refusal","stop_details":{"type":"refusal","category":"cyber"}},"usage":{"output_tokens":0}}"#,
                r#"{"type":"message_stop"}"#,
            ],
        );
        assert_eq!(
            state.finish().unwrap_err().detail.as_deref(),
            Some("fell back to claude-opus-4-8; category cyber")
        );
    }
    #[test]
    fn the_output_bound_counts_text_as_it_will_be_encoded() {
        let mut state = State::default();
        feed(
            &mut state,
            &[
                r#"{"type":"content_block_start","index":0,"content_block":{"type":"text","text":""}}"#,
            ],
        );
        // Each quote takes two bytes once stored, so this fits decoded but
        // not encoded.
        let quotes = "\\\"".repeat(MAX_OUTPUT / 2 + 1);
        let frame = format!(
            r#"{{"type":"content_block_delta","index":0,"delta":{{"type":"text_delta","text":"{quotes}"}}}}"#
        );
        assert_eq!(
            state.frame(frame.as_bytes()).unwrap_err().code,
            "output_limit"
        );
    }

    #[test]
    fn truncated_or_errored_streams_never_complete() {
        let mut state = State::default();
        feed(
            &mut state,
            &[
                r#"{"type":"content_block_start","index":0,"content_block":{"type":"text","text":""}}"#,
                r#"{"type":"message_start","message":{"usage":{"input_tokens":12}}}"#,
                r#"{"type":"message_delta","delta":{"stop_reason":"max_tokens"},"usage":{"output_tokens":1}}"#,
                r#"{"type":"message_stop"}"#,
            ],
        );
        assert_eq!(state.usage().unwrap().input_tokens, 12);
        assert_eq!(state.usage().unwrap().output_tokens, 1);
        assert_eq!(state.finish().unwrap_err().code, "provider_incomplete");
        assert_eq!(
            State::default().finish().unwrap_err().code,
            "missing_completion"
        );
        let error = State::default()
            .frame(br#"{"type":"error","error":{"type":"overloaded_error","message":"busy"}}"#)
            .unwrap_err();
        assert_eq!(error.code, "provider_http_529");
        assert_eq!(error.detail.as_deref(), Some("busy"));
        let error = State::default()
            .frame(br#"{"type":"error","error":{"type":"rate_limit_error","message":"slow down"}}"#)
            .unwrap_err();
        assert_eq!(error.code, "provider_rate_limited");
        let error = State::default()
            .frame(br#"{"type":"error","error":{"type":"invalid_request_error"}}"#)
            .unwrap_err();
        assert_eq!(error.code, "provider_error");
    }
}
