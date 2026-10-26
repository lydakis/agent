//! Reading a bot's history items the same way in every client, whichever
//! provider family shaped them.
use serde_json::{Value, json};

/// A message item's text.
pub fn text(item: &Value) -> String {
    match &item["content"] {
        Value::String(text) => text.clone(),
        Value::Array(parts) => parts
            .iter()
            .filter_map(|part| part["text"].as_str())
            .collect::<Vec<_>>()
            .join(""),
        _ => String::new(),
    }
}

/// An item as the Responses family shapes it, which the transcript reads.
/// An Anthropic message's `tool_use` blocks become function calls after
/// its text, and a user message's `tool_result` blocks their outputs.
pub fn responses(item: &Value) -> Vec<Value> {
    let Some(parts) = item["content"].as_array() else {
        return Vec::from_iter((!item.is_null()).then(|| item.clone()));
    };
    let tool = |part: &&Value| matches!(part["type"].as_str(), Some("tool_use" | "tool_result"));
    if !parts.iter().any(|part| tool(&part)) {
        return vec![item.clone()];
    }
    let said: Vec<&Value> = parts.iter().filter(|part| part["type"] == "text").collect();
    let mut items = Vec::new();
    if item["role"] == "assistant" || !said.is_empty() {
        items.push(json!({"role": item["role"], "content": said}));
    }
    for part in parts.iter().filter(tool) {
        items.push(match part["type"].as_str() {
            Some("tool_use") => json!({"type": "function_call", "call_id": part["id"],
                "name": part["name"], "arguments": part["input"].to_string()}),
            _ => json!({"type": "function_call_output", "call_id": part["tool_use_id"],
                "output": match &part["content"] {
                    Value::String(output) => output.clone(),
                    content => text(&json!({"content": content})),
                },
                "failed": part["is_error"] == true}),
        });
    }
    items
}
