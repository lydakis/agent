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
