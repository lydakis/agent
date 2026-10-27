//! `/approvals`: whether the lead's tool calls wait for a verdict, and
//! whose. Sonny's names for the CLI's modes: `auto` is a judge model,
//! `ask` is the person here (the CLI's `manual`), `full` runs everything.
//! Agents the lead makes keep the gate it had when it made them, until
//! the person picks a mode: it overrides what every one was made with.
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

/// A mode to set again, for a bot whose `auto` gate is older than the one
/// `gate` makes: one that lapses, so a doubted call is denied rather than
/// waiting for the person, or one that still judges reads. A bot another
/// bot made is set again too, as the person's `/approvals` would.
pub fn refresh(record: &Value) -> Option<&'static str> {
    let gate = &record["gates"][0];
    let old = gate["expire_ms"].is_u64()
        || gate["tools"]
            .as_array()
            .is_some_and(|tools| tools.iter().any(|t| t == "read"));
    (mode(record) == Some("auto") && old).then_some("auto")
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
        // Nothing left to gate, such as a bot that only reads in `auto`:
        // a gate needs a tool, so the bot is ungated.
        _ if approve.is_empty() => json!({"approve": []}),
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

/// The agents `lead` made, and those they made, from a `bots` listing:
/// by the maker's identity, so a reused name's children are strangers.
pub fn made_by<'a>(bots: &'a [Value], lead: &str) -> Vec<&'a Value> {
    let Some(lead) = bots
        .iter()
        .find(|bot| bot["name"] == lead)
        .and_then(|bot| bot["bot_id"].as_i64())
    else {
        return Vec::new();
    };
    let mut makers = vec![lead];
    let mut made: Vec<&Value> = Vec::new();
    while let Some(maker) = makers.pop() {
        for bot in bots.iter().filter(|bot| bot["created_by_id"] == maker) {
            if let Some(id) = bot["bot_id"].as_i64()
                && id != lead
                && !made.iter().any(|seen| seen["bot_id"] == id)
            {
                made.push(bot);
                makers.push(id);
            }
        }
    }
    made
}

/// The `configure` fields that give a bot `mode`, or `None` when its gate
/// is already the one `gate` makes: the same approver over the same tools,
/// with no expiry.
pub fn regate(record: &Value, mode: &str) -> Option<Map<String, Value>> {
    let tools: Vec<String> = record["tools"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(Value::as_str)
        .map(str::to_owned)
        .collect();
    let fields = gate(mode, &tools);
    let wanted = match fields.get("approver") {
        Some(tag) => vec![json!({"tag": tag, "tools": fields["approve"]})],
        None => Vec::new(),
    };
    let gates = record["gates"].as_array().map(Vec::as_slice).unwrap_or(&[]);
    let sorted = |tools: &Value| {
        let mut tools: Vec<String> = tools
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(Value::as_str)
            .map(str::to_owned)
            .collect();
        tools.sort();
        tools
    };
    let has = gates.len() == wanted.len()
        && gates.iter().zip(&wanted).all(|(gate, want)| {
            gate["tag"] == want["tag"]
                && sorted(&gate["tools"]) == sorted(&want["tools"])
                && gate["expire_ms"].is_null()
        });
    (!has).then_some(fields)
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
        // A bot with nothing left to gate is ungated, not refused.
        let reader: Vec<String> = ["read", "wait"].map(String::from).to_vec();
        assert_eq!(Value::Object(gate("auto", &reader)), json!({"approve": []}));
        assert!(gate("auto", &tools).get("approve_expire_ms").is_none());
        // An auto gate that still lapses is set again, a made bot's too.
        let lapsing = json!({"gates": [{"tag": "auto", "tools": ["shell"], "expire_ms": 45000}]});
        assert_eq!(refresh(&lapsing), Some("auto"));
        let mut made = lapsing.clone();
        made["created_by"] = json!("lead");
        assert_eq!(refresh(&made), Some("auto"));
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

    #[test]
    fn a_picked_mode_reaches_every_agent_the_lead_made() {
        let bot = |name: &str, id: i64, by: Option<i64>, gates: Value| {
            json!({"name": name, "bot_id": id, "created_by_id": by,
                "tools": ["shell", "read"], "gates": gates})
        };
        let auto = json!([{"tag": "auto", "tools": ["shell"]}]);
        let bots = [
            bot("lead", 2, None, auto.clone()),
            bot("task", 3, Some(2), auto.clone()),
            bot(
                "helper",
                4,
                Some(3),
                json!([{"tag": "ci", "tools": ["shell"]}]),
            ),
            // Made by a lead of the same name, since deleted.
            bot("old", 5, Some(1), auto.clone()),
            bot("loop", 6, Some(6), auto.clone()),
        ];
        let names: Vec<_> = made_by(&bots, "lead").iter().map(|b| &b["name"]).collect();
        assert_eq!(names, [&json!("task"), &json!("helper")]);
        // Each takes the mode unless it has it already; a gate Sonny did
        // not set is replaced too.
        assert_eq!(regate(&bots[1], "full"), Some(gate("full", &[])));
        assert_eq!(regate(&bots[1], "auto"), None);
        assert!(regate(&bots[2], "auto").is_some());
        // Nothing to gate: already as `full` leaves it.
        let reader = json!({"name": "r", "tools": ["read"], "gates": []});
        assert_eq!(regate(&reader, "auto"), None);
        assert!(regate(&reader, "ask").is_some());
        // The same tag over fewer tools, or one that lapses, is set again.
        let partial = json!({"tools": ["shell", "read", "write"],
            "gates": [{"tag": "manual", "tools": ["shell"]}]});
        assert!(regate(&partial, "ask").is_some());
        let lapsing = json!({"tools": ["shell", "read"],
            "gates": [{"tag": "auto", "tools": ["shell"], "expire_ms": 45000}]});
        assert!(regate(&lapsing, "auto").is_some());
    }
}
