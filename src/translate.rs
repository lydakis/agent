//! Stored history in one wire family, rewritten once in the other's, for a
//! bot that switches family (see `Database::switch_family`). Requests then
//! stream the new items as they stream any others: nothing is translated
//! per call.
//!
//! What crosses: the person's words and pictures, the model's text, its
//! tool calls and their results. What does not: reasoning. A Responses
//! `reasoning` item is encrypted for OpenAI alone, and an Anthropic
//! `thinking` block is signed for the model that wrote it; sent to the
//! other as text, a model reads it as part of its own visible reply. Both
//! are left out, as pi (`packages/ai/src/api/transform-messages.ts`) and
//! the AI SDK do. A search the provider ran itself goes as text, since
//! the other provider cannot replay it. Call ids keep only the characters
//! both families take, at most 64 of them, each one mapped once.
use crate::codec::Family;
use crate::provider::{RUN_NAME, RUN_NAMESPACE};
use crate::{Result, fail};
use serde_json::{Value, json};
use std::collections::HashSet;

/// The longest call id written: Anthropic's bound, which Responses takes.
const MAX_ID: usize = 64;

pub struct Translator {
    from: Family,
    to: Family,
    /// Tools the target declares as Responses custom tools (a grammar,
    /// on an endpoint that takes them): their calls and results go as
    /// custom ones.
    custom: HashSet<String>,
    /// Source calls that went as custom ones, whose results follow suit.
    custom_calls: HashSet<String>,
    /// Source calls of the provider's own search, whose results go as text.
    searches: HashSet<String>,
}

impl Translator {
    pub fn new(from: Family, to: Family, custom: HashSet<String>) -> Self {
        Self {
            from,
            to,
            custom,
            custom_calls: HashSet::new(),
            searches: HashSet::new(),
        }
    }

    /// The items `item` becomes: none for reasoning alone, several where
    /// one family keeps in one item what the other keeps in many.
    pub fn item(&mut self, item: &[u8]) -> Result<Vec<Vec<u8>>> {
        let value: Value = serde_json::from_slice(item)?;
        let items = match (self.from, self.to) {
            (Family::Anthropic, Family::Responses) => self.as_responses(value)?,
            (Family::Responses, Family::Anthropic) => self.as_anthropic(value)?,
            _ => vec![value],
        };
        items
            .iter()
            .map(|item| Ok(serde_json::to_vec(item)?))
            .collect()
    }

    /// The id written for source call `id`: itself when both families
    /// take it, else what they take of it and a hash of the whole, so two
    /// that clean alike stay apart. The same for every switch, with no state
    /// kept between them.
    fn id(&self, id: &str) -> String {
        let valid = |c: char| c.is_ascii_alphanumeric() || c == '_' || c == '-';
        if !id.is_empty() && id.len() <= MAX_ID && id.chars().all(valid) {
            return id.to_owned();
        }
        // FNV-1a, as `fingerprint` hashes a request's prefix.
        let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
        for byte in id.bytes() {
            hash ^= u64::from(byte);
            hash = hash.wrapping_mul(0x0100_0000_01b3);
        }
        let kept: String = (id.chars())
            .map(|c| if valid(c) { c } else { '_' })
            .take(MAX_ID - 17)
            .collect();
        format!("{kept}_{hash:016x}")
    }

    fn as_responses(&mut self, item: Value) -> Result<Vec<Value>> {
        let role = item["role"].as_str().unwrap_or_default().to_owned();
        let Value::Array(content) = item["content"].clone() else {
            // A plain string of content, as a person's words may be.
            return Ok(match item["content"].as_str() {
                Some(text) if !text.is_empty() => {
                    vec![message(&role, vec![part(Family::Responses, &role, text)])]
                }
                _ => Vec::new(),
            });
        };
        let mut out = Vec::new();
        let mut parts = Vec::new();
        for block in content {
            match block["type"].as_str().unwrap_or_default() {
                "text" => {
                    if let Some(text) = block["text"].as_str().filter(|t| !t.is_empty()) {
                        parts.push(part(Family::Responses, &role, text));
                    }
                }
                "image" => parts.push(image_to_responses(&block)?),
                "tool_use" => {
                    flush(&mut out, &role, &mut parts);
                    let source = block["id"].as_str().unwrap_or_default();
                    let id = self.id(source);
                    let name = block["name"].as_str().unwrap_or_default();
                    match (self.custom.contains(name), block["input"]["input"].as_str()) {
                        (true, Some(input)) => {
                            self.custom_calls.insert(source.to_owned());
                            out.push(json!({"type":"custom_tool_call","call_id":id,
                                "name":name,"input":input}));
                        }
                        _ => out.push(json!({"type":"function_call","call_id":id,"name":name,
                            "arguments":serde_json::to_string(&block["input"])?})),
                    }
                }
                "tool_result" => {
                    flush(&mut out, &role, &mut parts);
                    let source = block["tool_use_id"].as_str().unwrap_or_default();
                    let id = self.id(source);
                    let output = match &block["content"] {
                        Value::Array(parts) => Value::Array(
                            parts
                                .iter()
                                .filter_map(|p| match p["type"].as_str() {
                                    Some("text") => Some(Ok(json!({"type":"input_text",
                                        "text":p["text"]}))),
                                    Some("image") => Some(image_to_responses(p)),
                                    _ => None,
                                })
                                .collect::<Result<_>>()?,
                        ),
                        Value::String(text) => json!(text),
                        _ => json!(""),
                    };
                    let kind = match self.custom_calls.contains(source) {
                        true => "custom_tool_call_output",
                        false => "function_call_output",
                    };
                    out.push(json!({"type":kind,"call_id":id,"output":output}));
                }
                // The provider's own search: the query and what it found,
                // as words.
                "server_tool_use" => {
                    let text = format!(
                        "[{} {}]",
                        block["name"].as_str().unwrap_or("search"),
                        block["input"]
                    );
                    parts.push(part(Family::Responses, &role, &text));
                }
                kind if kind.ends_with("_tool_result") => {
                    let found: Vec<String> = (block["content"].as_array().into_iter().flatten())
                        .filter_map(|r| {
                            let url = r["url"].as_str()?;
                            Some(match r["title"].as_str() {
                                Some(title) => format!("{title} <{url}>"),
                                None => format!("<{url}>"),
                            })
                        })
                        .collect();
                    if !found.is_empty() {
                        let text = format!("[found: {}]", found.join("; "));
                        parts.push(part(Family::Responses, &role, &text));
                    }
                }
                // Thinking, redacted thinking, a fallback's marker: the
                // model's own, which the other cannot read.
                _ => {}
            }
        }
        flush(&mut out, &role, &mut parts);
        Ok(out)
    }

    fn as_anthropic(&mut self, item: Value) -> Result<Vec<Value>> {
        let kind = item["type"].as_str().unwrap_or("message");
        let one = |role: &str, content: Vec<Value>| match content.is_empty() {
            true => Vec::new(),
            false => vec![json!({"role":role,"content":content})],
        };
        Ok(match kind {
            "message" => {
                let role = match item["role"].as_str() {
                    Some("assistant") => "assistant",
                    _ => "user",
                };
                let content = match &item["content"] {
                    Value::String(text) if !text.is_empty() => {
                        vec![json!({"type":"text","text":text})]
                    }
                    Value::Array(parts) => parts
                        .iter()
                        .filter_map(|p| match p["type"].as_str() {
                            Some("input_text" | "output_text" | "text") => p["text"]
                                .as_str()
                                .filter(|t| !t.is_empty())
                                .map(|t| Ok(json!({"type":"text","text":t}))),
                            Some("input_image") => Some(image_to_anthropic(p)),
                            _ => None,
                        })
                        .collect::<Result<_>>()?,
                    _ => Vec::new(),
                };
                one(role, content)
            }
            "function_call" | "custom_tool_call" => {
                let source = item["call_id"].as_str().unwrap_or_default().to_owned();
                if item["namespace"] == RUN_NAMESPACE && item["name"] == RUN_NAME {
                    self.searches.insert(source);
                    let text = format!(
                        "[web search {}]",
                        item["arguments"].as_str().unwrap_or("{}")
                    );
                    return Ok(one("assistant", vec![json!({"type":"text","text":text})]));
                }
                let id = self.id(&source);
                let input = match kind {
                    "custom_tool_call" => json!({"input":item["input"]}),
                    _ => match item["arguments"]
                        .as_str()
                        .map(serde_json::from_str::<Value>)
                    {
                        Some(Ok(input @ Value::Object(_))) => input,
                        _ => return fail("invalid_tool_arguments"),
                    },
                };
                one(
                    "assistant",
                    vec![json!({"type":"tool_use","id":id,"name":item["name"],"input":input})],
                )
            }
            "function_call_output" | "custom_tool_call_output" => {
                let source = item["call_id"].as_str().unwrap_or_default();
                let output = match &item["output"] {
                    Value::Array(parts) => Value::Array(
                        parts
                            .iter()
                            .filter_map(|p| match p["type"].as_str() {
                                Some("input_text" | "output_text") => {
                                    Some(Ok(json!({"type":"text","text":p["text"]})))
                                }
                                Some("input_image") => Some(image_to_anthropic(p)),
                                _ => None,
                            })
                            .collect::<Result<_>>()?,
                    ),
                    Value::String(text) => json!(text),
                    _ => json!(""),
                };
                if self.searches.contains(source) {
                    let text = match &output {
                        Value::String(text) => text.clone(),
                        parts => (parts.as_array().into_iter().flatten())
                            .filter_map(|p| p["text"].as_str())
                            .collect::<Vec<_>>()
                            .join("\n"),
                    };
                    return Ok(one(
                        "user",
                        vec![json!({"type":"text","text":format!("[web search results]\n{text}")})],
                    ));
                }
                let id = self.id(source);
                one(
                    "user",
                    vec![json!({"type":"tool_result","tool_use_id":id,"content":output})],
                )
            }
            "web_search_call" => {
                let text = format!("[web search {}]", item["action"]);
                one("assistant", vec![json!({"type":"text","text":text})])
            }
            // Reasoning, encrypted for OpenAI alone; anything else the
            // other provider has no shape for.
            _ => Vec::new(),
        })
    }
}

fn message(role: &str, content: Vec<Value>) -> Value {
    match role {
        "assistant" => json!({"type":"message","role":"assistant","content":content}),
        _ => json!({"role":role,"content":content}),
    }
}

fn part(to: Family, role: &str, text: &str) -> Value {
    match (to, role) {
        (Family::Responses, "assistant") => json!({"type":"output_text","text":text}),
        (Family::Responses, _) => json!({"type":"input_text","text":text}),
        (Family::Anthropic, _) => json!({"type":"text","text":text}),
    }
}

fn flush(out: &mut Vec<Value>, role: &str, parts: &mut Vec<Value>) {
    if !parts.is_empty() {
        out.push(message(role, std::mem::take(parts)));
    }
}

fn image_to_responses(block: &Value) -> Result<Value> {
    let source = &block["source"];
    match (
        source["type"].as_str(),
        source["media_type"].as_str(),
        source["data"].as_str(),
    ) {
        (Some("base64"), Some(media), Some(data)) => Ok(json!({"type":"input_image",
            "image_url":format!("data:{media};base64,{data}")})),
        _ => fail("unsupported_content"),
    }
}

fn image_to_anthropic(part: &Value) -> Result<Value> {
    let url = part["image_url"].as_str().unwrap_or_default();
    match url
        .strip_prefix("data:")
        .and_then(|u| u.split_once(";base64,"))
    {
        Some((media, data)) => Ok(json!({"type":"image","source":{"type":"base64",
            "media_type":media,"data":data}})),
        None => fail("unsupported_content"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn run(t: &mut Translator, item: Value) -> Vec<Value> {
        t.item(&serde_json::to_vec(&item).unwrap())
            .unwrap()
            .iter()
            .map(|i| serde_json::from_slice(i).unwrap())
            .collect()
    }

    #[test]
    fn an_anthropic_turn_becomes_responses_items_without_its_thinking() {
        let mut t = Translator::new(
            Family::Anthropic,
            Family::Responses,
            HashSet::from(["exec".into()]),
        );
        let prompt = run(
            &mut t,
            json!({"role":"user","content":[{"type":"text","text":"hi"}]}),
        );
        assert_eq!(
            prompt,
            vec![json!({"role":"user","content":[{"type":"input_text","text":"hi"}]})]
        );
        let reply = run(
            &mut t,
            json!({"role":"assistant","content":[
                {"type":"thinking","thinking":"plan","signature":"sig"},
                {"type":"text","text":"Looking."},
                {"type":"tool_use","id":"toolu_1","name":"read","input":{"path":"a"}},
                {"type":"tool_use","id":"toolu_2","name":"exec","input":{"input":"ls"}}]}),
        );
        assert_eq!(
            reply,
            vec![
                json!({"type":"message","role":"assistant","content":[{"type":"output_text","text":"Looking."}]}),
                json!({"type":"function_call","call_id":"toolu_1","name":"read","arguments":"{\"path\":\"a\"}"}),
                json!({"type":"custom_tool_call","call_id":"toolu_2","name":"exec","input":"ls"}),
            ]
        );
        let read = run(
            &mut t,
            json!({"role":"user","content":[
            {"type":"tool_result","tool_use_id":"toolu_1","content":"text"}]}),
        );
        assert_eq!(
            read,
            vec![json!({"type":"function_call_output","call_id":"toolu_1","output":"text"})]
        );
        let exec = run(
            &mut t,
            json!({"role":"user","content":[
            {"type":"tool_result","tool_use_id":"toolu_2","content":"out"}]}),
        );
        assert_eq!(exec[0]["type"], "custom_tool_call_output");
        // Thinking alone is no item at all.
        let thought = run(
            &mut t,
            json!({"role":"assistant","content":[
            {"type":"redacted_thinking","data":"x"}]}),
        );
        assert!(thought.is_empty());
    }

    #[test]
    fn responses_items_become_anthropic_ones_with_ids_both_take() {
        let mut t = Translator::new(Family::Responses, Family::Anthropic, HashSet::new());
        assert!(
            run(
                &mut t,
                json!({"type":"reasoning","encrypted_content":"e","summary":[]})
            )
            .is_empty()
        );
        let call = run(
            &mut t,
            json!({"type":"function_call","id":"fc_1",
            "call_id":"call_weird_id","name":"read","arguments":"{\"path\":\"a\"}"}),
        );
        assert_eq!(
            call,
            vec![json!({"role":"assistant","content":[
            {"type":"tool_use","id":"call_weird_id","name":"read","input":{"path":"a"}}]})]
        );
        let result = run(
            &mut t,
            json!({"type":"function_call_output","call_id":"call_weird_id",
            "output":[{"type":"input_text","text":"a"},
                {"type":"input_image","image_url":"data:image/png;base64,iVBO"}]}),
        );
        assert_eq!(
            result,
            vec![json!({"role":"user","content":[{"type":"tool_result",
            "tool_use_id":"call_weird_id","content":[{"type":"text","text":"a"},
            {"type":"image","source":{"type":"base64","media_type":"image/png","data":"iVBO"}}]}]})]
        );
        let custom = run(
            &mut t,
            json!({"type":"custom_tool_call","call_id":"c2","name":"exec","input":"ls"}),
        );
        assert_eq!(custom[0]["content"][0]["input"], json!({"input":"ls"}));
        let text = run(
            &mut t,
            json!({"type":"message","role":"assistant",
            "content":[{"type":"output_text","text":"done"}]}),
        );
        assert_eq!(
            text,
            vec![json!({"role":"assistant","content":[{"type":"text","text":"done"}]})]
        );
        // Two ids that clean to the same one stay apart, and one taken
        // as it is stays as it is.
        let a = run(
            &mut t,
            json!({"type":"function_call","call_id":"x.y","name":"n","arguments":"{}"}),
        );
        let b = run(
            &mut t,
            json!({"type":"function_call","call_id":"x|y","name":"n","arguments":"{}"}),
        );
        assert_ne!(a[0]["content"][0]["id"], b[0]["content"][0]["id"]);
        assert!(
            a[0]["content"][0]["id"]
                .as_str()
                .unwrap()
                .starts_with("x_y_")
        );
        let long = "c".repeat(80);
        let c = run(
            &mut t,
            json!({"type":"function_call","call_id":long,"name":"n","arguments":"{}"}),
        );
        assert_eq!(c[0]["content"][0]["id"].as_str().unwrap().len(), 64);
    }

    #[test]
    fn the_provider_s_own_search_goes_as_words() {
        let mut t = Translator::new(Family::Responses, Family::Anthropic, HashSet::new());
        let call = run(
            &mut t,
            json!({"type":"function_call","namespace":"web","name":"run",
            "call_id":"s1","arguments":"{\"q\":\"rust\"}"}),
        );
        assert_eq!(call[0]["content"][0]["type"], "text");
        let found = run(
            &mut t,
            json!({"type":"function_call_output","call_id":"s1","output":"Rust is"}),
        );
        assert_eq!(found[0]["role"], "user");
        assert!(
            found[0]["content"][0]["text"]
                .as_str()
                .unwrap()
                .contains("Rust is")
        );
        let mut back = Translator::new(Family::Anthropic, Family::Responses, HashSet::new());
        let searched = run(
            &mut back,
            json!({"role":"assistant","content":[
            {"type":"server_tool_use","id":"srvtoolu_1","name":"web_search","input":{"query":"rust"}},
            {"type":"web_search_tool_result","tool_use_id":"srvtoolu_1","content":[
                {"type":"web_search_result","url":"https://example.test","title":"T","encrypted_content":"e"}]},
            {"type":"text","text":"Rust is","citations":[{"type":"web_search_result_location"}]}]}),
        );
        assert_eq!(searched.len(), 1);
        let parts = searched[0]["content"].as_array().unwrap();
        assert_eq!(parts.len(), 3);
        assert!(
            parts[1]["text"]
                .as_str()
                .unwrap()
                .contains("https://example.test")
        );
        assert_eq!(parts[2], json!({"type":"output_text","text":"Rust is"}));
    }
}
