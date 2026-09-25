//! Anthropic Messages streaming: the assistant message is reconstructed from
//! content-block events and stored as one native item, including thinking
//! signatures so tool-using turns can continue.
use super::{Completion, Delta, Frame, MAX_OUTPUT, ToolCall, Usage, detail_of};
use crate::{Error, Result, fail, fail_with};
use bytes::Bytes;
use serde_json::{Value, json};

enum Block {
    Text(String),
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
    /// A server-side fallback switched models here; blocks before the last
    /// one belong to attempts another model declined.
    Fallback,
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
    /// The model that produced the message, when a fallback changed it.
    fallback: Option<String>,
}

impl State {
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
                // Anthropic reports cache reads and cache writes outside
                // input_tokens; count every processed input token, as the
                // Responses family does, and keep cache reads separately.
                let usage = &event["message"]["usage"];
                let read = usage["cache_read_input_tokens"].as_u64().unwrap_or(0);
                let created = usage["cache_creation_input_tokens"].as_u64().unwrap_or(0);
                self.usage.input_tokens =
                    usage["input_tokens"].as_u64().unwrap_or(0) + read + created;
                self.usage.cached_input_tokens = read;
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
                if block["type"] == "redacted_thinking" {
                    self.account(frame.len())?;
                }
                self.blocks.push(match block["type"].as_str() {
                    Some("text") => Block::Text(String::new()),
                    Some("thinking") => Block::Thinking {
                        thinking: String::new(),
                        signature: String::new(),
                    },
                    Some("redacted_thinking") => Block::Redacted(block.clone()),
                    Some("tool_use") => Block::ToolUse {
                        id: string("id")?,
                        name: string("name")?,
                        input: String::new(),
                    },
                    Some("fallback") => {
                        self.fallback = block["to"]["model"].as_str().map(str::to_owned);
                        Block::Fallback
                    }
                    _ => return fail("unsupported_content"),
                });
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
                    _ => return Ok(Frame::Quiet),
                };
                self.account(part.len())?;
                match (kind.as_str(), self.block(&event["index"])?) {
                    ("text_delta", Block::Text(text)) => {
                        text.push_str(&part);
                        Ok(Frame::Delta(Delta::Text(part)))
                    }
                    ("thinking_delta", Block::Thinking { thinking, .. }) => {
                        thinking.push_str(&part);
                        Ok(Frame::Delta(Delta::Thinking(part)))
                    }
                    ("signature_delta", Block::Thinking { signature, .. }) => {
                        signature.push_str(&part);
                        Ok(Frame::Quiet)
                    }
                    ("input_json_delta", Block::ToolUse { input, .. }) => {
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
                self.iterations(&event["usage"]["iterations"]);
                // Sent again after a server-side fallback.
                self.dropped(&event["input_transformations"]);
                self.dropped(&event["delta"]["input_transformations"]);
                Ok(Frame::Quiet)
            }
            Some("message_stop") => {
                self.done = true;
                Ok(Frame::Quiet)
            }
            Some("error") => match detail_of(&event) {
                Some(detail) => fail_with("provider_error", detail),
                None => fail("provider_error"),
            },
            // Sent to hold an idle stream open; it is not progress.
            Some("ping") => Ok(Frame::Keepalive),
            _ => Ok(Frame::Quiet), // content_block_stop, unknown metadata
        }
    }
    /// With server-side fallbacks the top-level usage covers only the attempt
    /// that produced the message; the per-attempt list is the billed total.
    /// An attempt declined before any output is reported but not billed.
    fn iterations(&mut self, iterations: &Value) {
        let Some(list) = iterations.as_array().filter(|l| !l.is_empty()) else {
            return;
        };
        let mut usage = Usage::default();
        for (i, attempt) in list.iter().enumerate() {
            let tokens = |key: &str| attempt[key].as_u64().unwrap_or(0);
            if tokens("output_tokens") == 0 && i + 1 < list.len() {
                continue;
            }
            let read = tokens("cache_read_input_tokens");
            usage.input_tokens +=
                tokens("input_tokens") + read + tokens("cache_creation_input_tokens");
            usage.cached_input_tokens += read;
            usage.output_tokens += tokens("output_tokens");
            if attempt["type"] == "fallback_message"
                && let Some(model) = attempt["model"].as_str()
            {
                self.fallback = Some(model.to_owned());
            }
        }
        self.usage = usage;
        self.saw_usage = true;
    }
    /// The thinking blocks the request lost to the binding check.
    fn dropped(&mut self, transformations: &Value) {
        if let Some(list) = transformations.as_array() {
            self.thinking_dropped = list
                .iter()
                .filter(|t| t["type"] == "thinking_dropped")
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
        match self.stop_reason.as_deref() {
            Some("end_turn" | "stop_sequence" | "tool_use") => {}
            Some("max_tokens") => return fail_with("provider_incomplete", "max_tokens"),
            Some("refusal") => return fail("provider_refusal"),
            other => {
                return fail_with(
                    "provider_incomplete",
                    other.unwrap_or("missing stop reason"),
                );
            }
        }
        // Blocks before the last fallback are a declined attempt's partial
        // output: its text continues the answer, but its thinking and tool
        // calls are neither replayed nor run.
        let boundary = self
            .blocks
            .iter()
            .rposition(|b| matches!(b, Block::Fallback))
            .map_or(0, |i| i + 1);
        let mut content = Vec::with_capacity(self.blocks.len());
        let mut calls: Vec<ToolCall> = Vec::new();
        for (i, block) in self.blocks.into_iter().enumerate() {
            if i < boundary && !matches!(block, Block::Text(_)) {
                continue;
            }
            content.push(match block {
                Block::Fallback => continue,
                Block::Text(text) => json!({"type":"text","text":text}),
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
                    if id.is_empty() || calls.iter().any(|c| c.call_id == id) {
                        return fail("invalid_tool_call_id");
                    }
                    calls.push(ToolCall {
                        name: name.clone(),
                        call_id: id.clone(),
                        arguments,
                    });
                    json!({"type":"tool_use","id":id,"name":name,"input":parsed})
                }
            });
        }
        if content.is_empty() {
            return fail("empty_completion");
        }
        let item = serde_json::to_vec(&json!({"role":"assistant","content":content}))?;
        Ok(Completion {
            thinking_dropped: self.thinking_dropped,
            fallback: self.fallback,
            items: vec![Bytes::from(item)],
            calls,
            usage: self.saw_usage.then_some(self.usage),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn feed(state: &mut State, frames: &[&str]) -> Vec<String> {
        let mut deltas = Vec::new();
        for frame in frames {
            if let Frame::Delta(delta) = state.frame(frame.as_bytes()).unwrap() {
                deltas.push(match delta {
                    Delta::Text(t) => format!("text:{t}"),
                    Delta::Thinking(t) => format!("think:{t}"),
                });
            }
        }
        deltas
    }
    #[test]
    fn thinking_text_and_tool_use_become_one_native_assistant_item() {
        let mut state = State::default();
        let deltas = feed(
            &mut state,
            &[
                r#"{"type":"message_start","message":{"usage":{"input_tokens":12,"cache_read_input_tokens":3,"cache_creation_input_tokens":5}}}"#,
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
                cached_input_tokens: 3
            })
        );
    }
    #[test]
    fn dropped_thinking_is_counted_from_the_latest_report() {
        let start = r#"{"type":"message_start","message":{"usage":{"input_tokens":1},"input_transformations":[{"type":"thinking_dropped","message_index":1,"block_index":0},{"type":"other"}]}}"#;
        let text = r#"{"type":"content_block_start","index":0,"content_block":{"type":"text","text":"ok"}}"#;
        let delta = r#"{"type":"message_delta","delta":{"stop_reason":"end_turn"},"usage":{"output_tokens":1}}"#;
        let stop = r#"{"type":"message_stop"}"#;
        let mut state = State::default();
        feed(&mut state, &[start, text, delta, stop]);
        assert_eq!(state.finish().unwrap().thinking_dropped, 1);
        // After a fallback the delta repeats the whole list for the final request.
        let fallback = r#"{"type":"message_delta","delta":{"stop_reason":"end_turn"},"usage":{"output_tokens":1},"input_transformations":[{"type":"thinking_dropped"},{"type":"thinking_dropped"}]}"#;
        let mut state = State::default();
        feed(&mut state, &[start, text, fallback, stop]);
        assert_eq!(state.finish().unwrap().thinking_dropped, 2);
    }
    #[test]
    fn a_fallback_keeps_the_declined_text_but_not_its_thinking_or_calls() {
        let mut state = State::default();
        let deltas = feed(
            &mut state,
            &[
                r#"{"type":"message_start","message":{"model":"claude-opus-5-5","usage":{"input_tokens":10}}}"#,
                r#"{"type":"content_block_start","index":0,"content_block":{"type":"thinking","thinking":""}}"#,
                r#"{"type":"content_block_delta","index":0,"delta":{"type":"thinking_delta","thinking":"plan"}}"#,
                r#"{"type":"content_block_start","index":1,"content_block":{"type":"text","text":""}}"#,
                r#"{"type":"content_block_delta","index":1,"delta":{"type":"text_delta","text":"Partial "}}"#,
                r#"{"type":"content_block_start","index":2,"content_block":{"type":"tool_use","id":"t0","name":"shell","input":{}}}"#,
                r#"{"type":"content_block_delta","index":2,"delta":{"type":"input_json_delta","partial_json":"{\"comm"}}"#,
                r#"{"type":"content_block_start","index":3,"content_block":{"type":"fallback","from":{"model":"claude-opus-5-5"},"to":{"model":"claude-opus-4-8"}}}"#,
                r#"{"type":"content_block_start","index":4,"content_block":{"type":"text","text":""}}"#,
                r#"{"type":"content_block_delta","index":4,"delta":{"type":"text_delta","text":"rest"}}"#,
                r#"{"type":"content_block_start","index":5,"content_block":{"type":"tool_use","id":"t1","name":"shell","input":{}}}"#,
                r#"{"type":"content_block_delta","index":5,"delta":{"type":"input_json_delta","partial_json":"{\"command\":\"ls\"}"}}"#,
                r#"{"type":"message_delta","delta":{"stop_reason":"tool_use"},"usage":{"output_tokens":7,"iterations":[{"type":"message","model":null,"input_tokens":10,"cache_read_input_tokens":0,"cache_creation_input_tokens":0,"output_tokens":5},{"type":"fallback_message","model":"claude-opus-4-8","input_tokens":8,"cache_read_input_tokens":0,"cache_creation_input_tokens":4,"output_tokens":7}]}}"#,
                r#"{"type":"message_stop"}"#,
            ],
        );
        assert_eq!(deltas, ["think:plan", "text:Partial ", "text:rest"]);
        let completion = state.finish().unwrap();
        let item: Value = serde_json::from_slice(&completion.items[0]).unwrap();
        let kinds: Vec<_> = item["content"]
            .as_array()
            .unwrap()
            .iter()
            .map(|b| b["type"].as_str().unwrap())
            .collect();
        assert_eq!(kinds, ["text", "text", "tool_use"]);
        assert_eq!(item["content"][2]["id"], "t1");
        assert_eq!(completion.calls.len(), 1);
        assert_eq!(completion.calls[0].call_id, "t1");
        assert_eq!(completion.fallback.as_deref(), Some("claude-opus-4-8"));
        // Both attempts produced output, so both are billed.
        assert_eq!(
            completion.usage,
            Some(Usage {
                input_tokens: 22,
                output_tokens: 12,
                cached_input_tokens: 0
            })
        );
    }
    #[test]
    fn an_attempt_declined_before_output_is_not_billed() {
        // A pre-output decline: the stream opens on the fallback model, whose
        // marker comes first; a later sticky turn names it only in usage.
        let mut state = State::default();
        feed(
            &mut state,
            &[
                r#"{"type":"message_start","message":{"model":"claude-opus-4-8","usage":{"input_tokens":8}}}"#,
                r#"{"type":"content_block_start","index":0,"content_block":{"type":"fallback","from":{"model":"claude-opus-5-5"},"to":{"model":"claude-opus-4-8"}}}"#,
                r#"{"type":"content_block_start","index":1,"content_block":{"type":"text","text":""}}"#,
                r#"{"type":"content_block_delta","index":1,"delta":{"type":"text_delta","text":"ok"}}"#,
                r#"{"type":"message_delta","delta":{"stop_reason":"end_turn"},"usage":{"output_tokens":1,"iterations":[{"type":"message","model":null,"input_tokens":10,"output_tokens":0},{"type":"fallback_message","model":"claude-opus-4-8","input_tokens":8,"cache_read_input_tokens":3,"output_tokens":1}]}}"#,
                r#"{"type":"message_stop"}"#,
            ],
        );
        let completion = state.finish().unwrap();
        let item: Value = serde_json::from_slice(&completion.items[0]).unwrap();
        assert_eq!(item["content"], json!([{"type":"text","text":"ok"}]));
        assert_eq!(completion.fallback.as_deref(), Some("claude-opus-4-8"));
        assert_eq!(
            completion.usage,
            Some(Usage {
                input_tokens: 11,
                output_tokens: 1,
                cached_input_tokens: 3
            })
        );
        let mut sticky = State::default();
        feed(
            &mut sticky,
            &[
                r#"{"type":"message_start","message":{"model":"claude-opus-4-8","usage":{"input_tokens":8}}}"#,
                r#"{"type":"content_block_start","index":0,"content_block":{"type":"text","text":""}}"#,
                r#"{"type":"message_delta","delta":{"stop_reason":"refusal"},"usage":{"output_tokens":0,"iterations":[{"type":"fallback_message","model":"claude-opus-4-8","input_tokens":8,"output_tokens":0}]}}"#,
                r#"{"type":"message_stop"}"#,
            ],
        );
        assert_eq!(sticky.usage().unwrap().input_tokens, 8);
        // A refusal after the fallbacks means every model in the chain declined.
        assert_eq!(sticky.finish().unwrap_err().code, "provider_refusal");
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
        assert_eq!(error.detail.as_deref(), Some("busy"));
    }
}
