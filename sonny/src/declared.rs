//! The tools Sonny declares on the bots it opens: the daemon shows them to
//! the model and records each call with a fixed result, and Sonny gives
//! the calls their meaning. A bot reacts to the person, asks them to
//! decide something, and keeps its plan where they see it.
use serde_json::Value;

/// The most tools a bot may have declared, and the most bytes they take,
/// as the daemon counts them.
const MAX_DECLARED: usize = 16;
const DECLARED_BYTES: usize = 64 * 1024;

/// Sonny's tools, as it declares them.
pub fn tools() -> [Value; 3] {
    [
        crate::reactions::tool(),
        crate::asks::tool(),
        crate::plan::tool(),
    ]
}

/// The tools the bot `record` describes should declare: its own with
/// Sonny's, or `None` when it has those already, or when they would not
/// fit beside its own.
pub fn declare(record: &Value) -> Option<Value> {
    let mut declared = record["declared"].as_array().cloned().unwrap_or_default();
    let ours = tools();
    if ours.iter().all(|tool| declared.contains(tool)) {
        return None;
    }
    declared.retain(|other| !ours.iter().any(|tool| tool["name"] == other["name"]));
    declared.extend(ours);
    let bytes = serde_json::to_vec(&declared).map_or(usize::MAX, |json| json.len());
    (declared.len() <= MAX_DECLARED && bytes <= DECLARED_BYTES).then_some(Value::Array(declared))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn a_bot_is_given_sonnys_tools_beside_its_own_once() {
        let other = json!({"name": "notes", "description": "d", "parameters": {"type": "object"},
            "result": "ok"});
        let wanted = declare(&json!({"declared": [other]})).unwrap();
        let mut all = vec![other];
        all.extend(tools());
        assert_eq!(wanted, Value::Array(all));
        assert_eq!(declare(&json!({"declared": wanted})), None);
        // An older wording of one of Sonny's is replaced.
        let old = json!({"name": "react", "description": "old", "parameters": {"type": "object"},
            "result": "ok"});
        assert_eq!(
            declare(&json!({"declared": [old]})),
            Some(Value::Array(tools().to_vec()))
        );
        // A bot whose own tools leave no room keeps them as they are.
        let full: Vec<Value> = (0..14)
            .map(|n| {
                json!({"name": format!("t{n}"), "description": "d",
                "parameters": {"type": "object"}, "result": "ok"})
            })
            .collect();
        assert_eq!(declare(&json!({"declared": full})), None);
        let big = json!({"name": "big", "description": "d".repeat(63 * 1024),
            "parameters": {"type": "object"}, "result": "ok"});
        assert_eq!(declare(&json!({"declared": [big]})), None);
    }
}
