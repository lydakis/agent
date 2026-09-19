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
