//! Provider wire families. History items are stored in the family's native
//! encoding so requests stream them without translation; a bot is bound to
//! one family for its lifetime.
use crate::{Result, fail};
use serde_json::{Value, json};

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Family {
    /// OpenAI Responses API and compatible gateways.
    Responses,
    /// Anthropic Messages API.
    Anthropic,
}

/// Provider-neutral tool description; each family encodes it differently.
#[derive(Clone, Debug)]
pub struct ToolSchema {
    pub name: &'static str,
    pub description: &'static str,
    pub parameters: Value,
}

impl Family {
    pub fn parse(name: &str) -> Option<Family> {
        match name {
            "responses" => Some(Family::Responses),
            "anthropic" => Some(Family::Anthropic),
            _ => None,
        }
    }
    /// The quota a model draws on. Dated snapshots and their alias share one
    /// allowance at both providers, so they share one pool: `gpt-5.6-luna`
    /// and `gpt-5.6-luna-2026-05-01`, `claude-sonnet-5` and
    /// `claude-sonnet-5-20260401`. Anything else is its own pool; a shared
    /// quota the provider does not name is still corrected by every
    /// response's headers, bounded by what is in flight.
    pub fn pool_key(self, model: &str) -> String {
        let stripped = match self {
            Family::Responses => model
                .rsplit_once('-')
                .filter(|(_, tail)| tail.len() == 2 && tail.bytes().all(|b| b.is_ascii_digit()))
                .and_then(|(head, _)| head.rsplit_once('-'))
                .filter(|(_, tail)| tail.len() == 2 && tail.bytes().all(|b| b.is_ascii_digit()))
                .and_then(|(head, _)| head.rsplit_once('-'))
                .filter(|(_, tail)| tail.len() == 4 && tail.bytes().all(|b| b.is_ascii_digit()))
                .map(|(head, _)| head),
            Family::Anthropic => model
                .rsplit_once('-')
                .filter(|(_, tail)| tail.len() == 8 && tail.bytes().all(|b| b.is_ascii_digit()))
                .map(|(head, _)| head),
        };
        stripped.unwrap_or(model).to_owned()
    }
    pub fn name(self) -> &'static str {
        match self {
            Family::Responses => "responses",
            Family::Anthropic => "anthropic",
        }
    }
    pub fn user_item(self, text: &str) -> Result<Vec<u8>> {
        let item = match self {
            Family::Responses => {
                json!({"role":"user","content":[{"type":"input_text","text":text}]})
            }
            Family::Anthropic => json!({"role":"user","content":[{"type":"text","text":text}]}),
        };
        Ok(serde_json::to_vec(&item)?)
    }
    pub fn tool_result_item(self, call_id: &str, output: &str) -> Result<Vec<u8>> {
        let item = match self {
            Family::Responses => {
                json!({"type":"function_call_output","call_id":call_id,"output":output})
            }
            Family::Anthropic => json!({"role":"user","content":[
                {"type":"tool_result","tool_use_id":call_id,"content":output}]}),
        };
        Ok(serde_json::to_vec(&item)?)
    }
    pub fn tools(self, tools: &[ToolSchema]) -> Value {
        let encoded: Vec<Value> = tools
            .iter()
            .map(|tool| match self {
                Family::Responses => json!({"type":"function","name":tool.name,
                    "description":tool.description,"parameters":tool.parameters,"strict":false}),
                Family::Anthropic => json!({"name":tool.name,"description":tool.description,
                    "input_schema":tool.parameters}),
            })
            .collect();
        Value::Array(encoded)
    }
}

/// Split `provider/model-id` at the first slash. Model IDs may contain slashes.
pub fn split_model(reference: &str) -> Result<(&str, &str)> {
    match reference.split_once('/') {
        Some((provider, model))
            if !provider.is_empty()
                && !model.is_empty()
                && provider.len() <= 64
                && model.len() <= 256
                && provider
                    .bytes()
                    .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_') =>
        {
            Ok((provider, model))
        }
        _ => fail("invalid_model_reference"),
    }
}

/// Up to `max` characters of a raw JSON value, and whether more follow,
/// decoding nothing past them: a string's text, or any other value as is.
pub fn json_preview(value: &serde_json::value::RawValue, max: usize) -> (String, bool) {
    let raw = value.get();
    if raw.starts_with('"') {
        return json_string_prefix(raw, max);
    }
    let end = raw.char_indices().nth(max).map_or(raw.len(), |(i, _)| i);
    (raw[..end].to_owned(), end < raw.len())
}
/// The first `max` characters of a JSON string literal, and whether more
/// follow, decoding nothing past them. The literal may be cut short; a lone
/// surrogate reads as U+FFFD.
pub fn json_string_prefix(literal: &str, max: usize) -> (String, bool) {
    fn hex(digits: &str) -> u32 {
        u32::from_str_radix(digits.get(..4).unwrap_or_default(), 16).unwrap_or(0xFFFD)
    }
    let (mut out, mut count) = (String::new(), 0);
    let mut chars = literal[1..].chars();
    loop {
        let c = match chars.next() {
            None | Some('"') => return (out, false),
            Some('\\') => match chars.next() {
                Some('n') => '\n',
                Some('t') => '\t',
                Some('r') => '\r',
                Some('b') => '\u{8}',
                Some('f') => '\u{c}',
                Some('u') => {
                    let high = hex(chars.as_str());
                    chars.nth(3);
                    // A high surrogate pairs only with a low one right after it.
                    let low = chars.as_str().strip_prefix("\\u").map_or(0, hex);
                    let code =
                        if (0xD800..0xDC00).contains(&high) && (0xDC00..0xE000).contains(&low) {
                            chars.nth(5);
                            0x10000 + ((high - 0xD800) << 10) + (low - 0xDC00)
                        } else {
                            high
                        };
                    char::from_u32(code).unwrap_or('\u{FFFD}')
                }
                Some(c) => c,
                None => return (out, false),
            },
            Some(c) => c,
        };
        if count == max {
            return (out, true);
        }
        out.push(c);
        count += 1;
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn dated_snapshots_share_their_alias_pool() {
        use super::Family;
        assert_eq!(
            Family::Responses.pool_key("gpt-5.6-luna-2026-05-01"),
            "gpt-5.6-luna"
        );
        assert_eq!(Family::Responses.pool_key("gpt-5.6-luna"), "gpt-5.6-luna");
        assert_eq!(Family::Responses.pool_key("o3-mini-2025-01-31"), "o3-mini");
        assert_eq!(
            Family::Anthropic.pool_key("claude-sonnet-5-20260401"),
            "claude-sonnet-5"
        );
        assert_eq!(
            Family::Anthropic.pool_key("claude-sonnet-5"),
            "claude-sonnet-5"
        );
        // Not dates: version numbers and short numeric tails stay distinct.
        assert_eq!(
            Family::Anthropic.pool_key("claude-haiku-4-5"),
            "claude-haiku-4-5"
        );
        assert_eq!(Family::Responses.pool_key("gpt-4-32k"), "gpt-4-32k");
    }
    use super::*;
    #[test]
    fn model_references_name_a_provider_then_a_model_id() {
        assert_eq!(
            split_model("openrouter/anthropic/claude").unwrap(),
            ("openrouter", "anthropic/claude")
        );
        assert!(split_model("no-slash").is_err());
        assert!(split_model("/model").is_err());
        assert!(split_model("bad name/model").is_err());
    }
    #[test]
    fn families_encode_the_same_facts_differently() {
        let responses: Value =
            serde_json::from_slice(&Family::Responses.tool_result_item("c1", "out").unwrap())
                .unwrap();
        assert_eq!(responses["type"], "function_call_output");
        let anthropic: Value =
            serde_json::from_slice(&Family::Anthropic.tool_result_item("c1", "out").unwrap())
                .unwrap();
        assert_eq!(anthropic["content"][0]["tool_use_id"], "c1");
    }
}
