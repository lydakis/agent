//! Anthropic Messages streaming: the assistant message is reconstructed from
//! content-block events and stored as one native item, including thinking
//! signatures so tool-using turns can continue.
use super::{Completion, Delta, MAX_OUTPUT, ToolCall, Usage, detail_of};
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
}

#[derive(Default)]
pub struct State {
    blocks: Vec<Block>,
    bytes: usize,
    stop_reason: Option<String>,
    usage: Usage,
    saw_usage: bool,
    done: bool,
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
    pub fn frame(&mut self, frame: &[u8]) -> Result<Option<Delta>> {
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
                Ok(None)
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
                    _ => return fail("unsupported_content"),
                });
                Ok(None)
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
                    _ => return Ok(None),
                };
                self.account(part.len())?;
                match (kind.as_str(), self.block(&event["index"])?) {
                    ("text_delta", Block::Text(text)) => {
                        text.push_str(&part);
                        Ok(Some(Delta::Text(part)))
                    }
                    ("thinking_delta", Block::Thinking { thinking, .. }) => {
                        thinking.push_str(&part);
                        Ok(Some(Delta::Thinking(part)))
                    }
                    ("signature_delta", Block::Thinking { signature, .. }) => {
                        signature.push_str(&part);
                        Ok(None)
                    }
                    ("input_json_delta", Block::ToolUse { input, .. }) => {
                        input.push_str(&part);
                        Ok(None)
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
                Ok(None)
            }
            Some("message_stop") => {
                self.done = true;
                Ok(None)
            }
            Some("error") => match detail_of(&event) {
                Some(detail) => fail_with("provider_error", detail),
                None => fail("provider_error"),
            },
            _ => Ok(None), // ping, content_block_stop, unknown metadata
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
        let mut content = Vec::with_capacity(self.blocks.len());
        let mut calls: Vec<ToolCall> = Vec::new();
        for block in self.blocks {
            content.push(match block {
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
            if let Some(delta) = state.frame(frame.as_bytes()).unwrap() {
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
