//! `/approvals`: whether the lead's tool calls wait for a verdict, and
//! whose. Sonny's names for the CLI's modes: `auto` is a judge model,
//! `ask` is the person here (the CLI's `manual`), `full` runs everything.
//! Agents the lead makes keep the gate it had when it made them.
use agent_client::approver::{AUTO_UNGATED, UNGATED};
use serde_json::{Map, Value, json};

/// New leads start in the first.
pub const MODES: [(&str, &str); 3] = [
    ("auto", "a judge model decides each call"),
    ("ask", "you answer each call here"),
    ("full", "every call runs"),
];

/// The lead's mode, from its record's gates; `None` for a gate Sonny
/// did not set.
pub fn mode(record: &Value) -> Option<&'static str> {
    let gates = record["gates"].as_array().map(Vec::as_slice).unwrap_or(&[]);
    match gates {
        [] => Some("full"),
        [gate] if gate["tag"] == "auto" => Some("auto"),
        [gate] if gate["tag"] == "manual" => Some("ask"),
        _ => None,
    }
}

/// A mode to set again, for a bot nobody made whose `auto` gate is older
/// than the one `gate` makes: one that lapses, so a doubted call is denied
/// rather than waiting for the person, or one that still judges reads.
pub fn refresh(record: &Value) -> Option<&'static str> {
    let gate = &record["gates"][0];
    let old = gate["expire_ms"].is_u64()
        || gate["tools"]
            .as_array()
            .is_some_and(|tools| tools.iter().any(|t| t == "read"));
    (mode(record) == Some("auto") && old && record["created_by"].is_null()).then_some("auto")
}

/// The `create` or `configure` fields for a mode: every tool the bot has
/// but those that touch only its own records waits for the verdict, and in
/// `auto` reads run unjudged too. `full` is no gate, which `create` takes
/// as no fields.
pub fn gate(mode: &str, tools: &[String]) -> Map<String, Value> {
    let ungated: &[&str] = if mode == "auto" {
        &AUTO_UNGATED
    } else {
        &UNGATED
    };
    let approve: Vec<&String> = tools
        .iter()
        .filter(|tool| !ungated.contains(&tool.as_str()))
        .collect();
    let fields = match mode {
        // No expiry: a call the judge doubts waits for the person here.
        "auto" => json!({"approve": approve, "approver": "auto"}),
        "ask" => json!({"approve": approve, "approver": "manual"}),
        _ => json!({"approve": []}),
    };
    match fields {
        Value::Object(fields) => fields,
        _ => Map::new(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn modes_are_read_back_from_the_gates_they_make() {
        let tools: Vec<String> = ["shell", "read", "wait", "history"]
            .map(String::from)
            .to_vec();
        for (name, _) in MODES {
            let fields = gate(name, &tools);
            let gates = match fields.get("approver") {
                Some(tag) => json!([{"tag": tag, "tools": fields["approve"]}]),
                None => json!([]),
            };
            assert_eq!(mode(&json!({"gates": gates})), Some(name));
        }
        assert_eq!(gate("auto", &tools)["approve"], json!(["shell"]));
        assert_eq!(gate("ask", &tools)["approve"], json!(["shell", "read"]));
        assert!(gate("auto", &tools).get("approve_expire_ms").is_none());
        // A lead's auto gate that still lapses is set again; a made bot's
        // keeps the gate it descends from.
        let lapsing = json!({"gates": [{"tag": "auto", "tools": ["shell"], "expire_ms": 45000}]});
        assert_eq!(refresh(&lapsing), Some("auto"));
        let mut made = lapsing.clone();
        made["created_by"] = json!("lead");
        assert_eq!(refresh(&made), None);
        assert_eq!(
            refresh(&json!({"gates": [{"tag": "auto", "tools": ["shell"]}]})),
            None
        );
        // So is one that still judges reads.
        let reads = json!({"gates": [{"tag": "auto", "tools": ["shell", "read"]}]});
        assert_eq!(refresh(&reads), Some("auto"));
        assert_eq!(mode(&json!({"gates": null})), Some("full"));
        assert_eq!(mode(&json!({"gates": [{"tag": "ci"}]})), None);
    }
}
