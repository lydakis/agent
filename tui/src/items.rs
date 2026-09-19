//! Turn stored history items (provider-native encoding, either family) into
//! transcript entries. Tool calls are shown from `tool_started` events, so
//! call blocks inside items are skipped here.
use serde_json::Value;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Entry {
    User(String),
    Assistant(String),
    Thinking(String),
    ToolOutput(String),
    /// Lifecycle notes: queued, finished, waiting, steered, forked.
    Note(String),
}

fn text_of(content: &Value, keys: &[&str]) -> String {
    match content {
        Value::String(text) => text.clone(),
        Value::Array(parts) => parts
            .iter()
            .filter(|part| part["type"].as_str().is_some_and(|t| keys.contains(&t)))
            .filter_map(|part| part["text"].as_str())
            .collect::<Vec<_>>()
            .join(""),
        _ => String::new(),
    }
}

/// Entries for one stored item. An unknown shape yields nothing rather than
/// a guess; the raw item stays reachable through the protocol.
pub fn entries(item: &Value) -> Vec<Entry> {
    let mut out = Vec::new();
    match item["type"].as_str() {
        // Responses family.
        Some("function_call_output") => {
            out.push(Entry::ToolOutput(tool_output_text(
                item["output"].as_str().unwrap_or(""),
            )));
            return out;
        }
        Some("function_call") => return out,
        Some("reasoning") => {
            let summary = text_of(&item["summary"], &["summary_text"]);
            if !summary.is_empty() {
                out.push(Entry::Thinking(summary));
            }
            return out;
        }
        Some("message") | None => {}
        Some(_) => return out,
    }
    match item["role"].as_str() {
        Some("user") => {
            // Anthropic tool results ride in user turns.
            if let Some(parts) = item["content"].as_array() {
                let mut text = String::new();
                for part in parts {
                    match part["type"].as_str() {
                        Some("tool_result") => {
                            let output = text_of(&part["content"], &["text"]);
                            out.push(Entry::ToolOutput(tool_output_text(&output)));
                        }
                        Some("text") | Some("input_text") => {
                            text.push_str(part["text"].as_str().unwrap_or(""));
                        }
                        _ => {}
                    }
                }
                if !text.is_empty() {
                    out.insert(0, Entry::User(text));
                }
            } else {
                out.push(Entry::User(text_of(&item["content"], &["text"])));
            }
        }
        Some("assistant") => {
            if let Some(parts) = item["content"].as_array() {
                let mut text = String::new();
                for part in parts {
                    match part["type"].as_str() {
                        Some("thinking") => {
                            let thinking = part["thinking"].as_str().unwrap_or("");
                            if !thinking.is_empty() {
                                out.push(Entry::Thinking(thinking.to_owned()));
                            }
                        }
                        Some("text") | Some("output_text") => {
                            text.push_str(part["text"].as_str().unwrap_or(""));
                        }
                        _ => {}
                    }
                }
                if !text.is_empty() {
                    out.push(Entry::Assistant(text));
                }
            }
        }
        _ => {}
    }
    out
}

/// Shell results are JSON with stdout/stderr/exit_code; show them as a
/// terminal would. Anything else is returned as is.
fn tool_output_text(output: &str) -> String {
    let Ok(Value::Object(fields)) = serde_json::from_str::<Value>(output) else {
        return output.to_owned();
    };
    if !fields.contains_key("stdout") {
        return output.to_owned();
    }
    let mut text = fields["stdout"]
        .as_str()
        .unwrap_or("")
        .trim_end()
        .to_owned();
    if let Some(stderr) = fields["stderr"].as_str().filter(|s| !s.trim().is_empty()) {
        if !text.is_empty() {
            text.push('\n');
        }
        text.push_str("stderr: ");
        text.push_str(stderr.trim_end());
    }
    if let Some(code) = fields["exit_code"].as_i64().filter(|c| *c != 0) {
        if !text.is_empty() {
            text.push('\n');
        }
        text.push_str(&format!("exit {code}"));
    }
    if fields["success"] == false && fields["exit_code"].as_i64() == Some(0) {
        if !text.is_empty() {
            text.push('\n');
        }
        text.push_str("failed");
    }
    if text.is_empty() {
        "(no output)".into()
    } else {
        text
    }
}

/// One-line summary of a tool call's arguments for the transcript and log.
pub fn call_summary(name: &str, arguments: &str) -> String {
    let args: Value = serde_json::from_str(arguments).unwrap_or(Value::Null);
    let summary = match name {
        "shell" => args["command"].as_str().unwrap_or("").to_owned(),
        "read" | "write" | "edit" => args["path"].as_str().unwrap_or("").to_owned(),
        "wait" => args["handles"]
            .as_array()
            .map(|h| {
                h.iter()
                    .filter_map(Value::as_str)
                    .map(|h| h.trim_start_matches("turn:"))
                    .collect::<Vec<_>>()
                    .join(", ")
            })
            .unwrap_or_default(),
        _ => arguments.to_owned(),
    };
    summary
        .lines()
        .next()
        .unwrap_or("")
        .chars()
        .take(160)
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn responses_items_map_to_entries() {
        let user = json!({"role":"user","content":[{"type":"input_text","text":"hi"}]});
        assert_eq!(entries(&user), vec![Entry::User("hi".into())]);
        let message = json!({"type":"message","role":"assistant",
            "content":[{"type":"output_text","text":"yo"}]});
        assert_eq!(entries(&message), vec![Entry::Assistant("yo".into())]);
        let call = json!({"type":"function_call","name":"shell","arguments":"{}"});
        assert!(entries(&call).is_empty());
        let output = json!({"type":"function_call_output","call_id":"c","output":"ok"});
        assert_eq!(entries(&output), vec![Entry::ToolOutput("ok".into())]);
    }

    #[test]
    fn anthropic_items_map_to_entries() {
        let assistant = json!({"role":"assistant","content":[
            {"type":"thinking","thinking":"hmm"},
            {"type":"text","text":"yo"},
            {"type":"tool_use","name":"shell","input":{}}]});
        assert_eq!(
            entries(&assistant),
            vec![Entry::Thinking("hmm".into()), Entry::Assistant("yo".into())]
        );
        let result = json!({"role":"user","content":[
            {"type":"tool_result","tool_use_id":"c","content":"ok"}]});
        assert_eq!(entries(&result), vec![Entry::ToolOutput("ok".into())]);
    }

    #[test]
    fn shell_results_render_like_a_terminal() {
        let output = json!({"type":"function_call_output","call_id":"c",
            "output":r#"{"exit_code":1,"stderr":"nope\n","stdout":"a\nb\n","success":false}"#});
        assert_eq!(
            entries(&output),
            vec![Entry::ToolOutput("a\nb\nstderr: nope\nexit 1".into())]
        );
        let plain =
            json!({"type":"function_call_output","call_id":"c","output":"{\"error\":\"x\"}"});
        assert_eq!(
            entries(&plain),
            vec![Entry::ToolOutput("{\"error\":\"x\"}".into())]
        );
    }

    #[test]
    fn call_summaries_take_the_first_line() {
        assert_eq!(call_summary("shell", r#"{"command":"ls\n-la"}"#), "ls");
        assert_eq!(
            call_summary("wait", r#"{"handles":["turn:a/1","turn:b/2"]}"#),
            "a/1, b/2"
        );
    }
}
