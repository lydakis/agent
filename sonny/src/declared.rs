//! The tools Sonny declares on the bots it opens: the daemon shows them to
//! the model and records each call, with a fixed result or with the one
//! Sonny's lookout answers (`served`), and Sonny gives the calls their
//! meaning. A bot reacts to the person, asks them to decide something,
//! keeps its plan where they see it, keeps notes in the margin beside the
//! conversation, and puts windows and panes where they look; a lead also starts, removes and resolves its tasks, makes
//! projects and subscribes to the lookout, and a task resolves itself.
use serde_json::Value;

/// The most tools a bot may have declared, and the most bytes they take,
/// as the daemon counts them.
const MAX_DECLARED: usize = 16;
const DECLARED_BYTES: usize = 64 * 1024;

/// Sonny's tools, as it declares them on a lead, or on another bot.
pub fn tools(lead: bool) -> Vec<Value> {
    let mut tools = vec![
        crate::reactions::tool(),
        crate::asks::tool(),
        crate::plan::tool(),
        crate::margin::tool(),
    ];
    if lead {
        tools.push(crate::resolved::tool());
    }
    tools.extend(crate::served::tools(lead));
    tools
}

/// Whether the bot `record` describes resolves itself: a lead made it, or
/// another bot made it with `resolve_task`, as a task's own helper takes it.
fn resolves(record: &Value) -> bool {
    let inherited = || {
        (record["declared"].as_array().into_iter().flatten())
            .any(|tool| tool["name"] == crate::resolved::TOOL)
    };
    match record["created_by"].as_str() {
        Some(by) => by.ends_with(".lead") || inherited(),
        None => false,
    }
}

/// The tools the bot `record` describes, a lead or not, should declare:
/// its own with Sonny's, or `None` when it has those already, or when they
/// would not fit beside its own.
pub fn declare(record: &Value, lead: bool) -> Option<Value> {
    let mut declared = record["declared"].as_array().cloned().unwrap_or_default();
    let mut ours = tools(lead);
    // A task resolves itself, as its lead resolves it.
    if !lead && resolves(record) {
        ours.push(crate::resolved::tool());
    }
    // A gate Sonny did not set, as another client's tag or several, is
    // not one Sonny can add its gated tools to, so that bot goes without
    // them rather than run them unjudged.
    if crate::approvals::mode(record).is_none() {
        ours.retain(|tool| {
            !(tool["name"].as_str()).is_some_and(|name| crate::served::GATED.contains(&name))
        });
    }
    let names = tools(true);
    let sonnys = |other: &Value| names.iter().any(|tool| tool["name"] == other["name"]);
    if ours.iter().all(|tool| declared.contains(tool))
        && declared
            .iter()
            .all(|other| ours.contains(other) || !sonnys(other))
    {
        return None;
    }
    // Any of Sonny's there gives way to these: a lead's only on a lead.
    declared.retain(|other| !sonnys(other));
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
        // A lead's fit beside a few of its own, by count and by size.
        let lead = declare(&json!({"declared": [other.clone(), other.clone()]}), true).unwrap();
        assert!(serde_json::to_vec(&lead).unwrap().len() <= DECLARED_BYTES / 2);
        let wanted = declare(&json!({"declared": [other]}), false).unwrap();
        let mut all = vec![other];
        all.extend(tools(false));
        assert_eq!(wanted, Value::Array(all));
        assert_eq!(declare(&json!({"declared": wanted}), false), None);
        // An older wording of one of Sonny's is replaced.
        let old = json!({"name": "react", "description": "old", "parameters": {"type": "object"},
            "result": "ok"});
        assert_eq!(
            declare(&json!({"declared": [old]}), false),
            Some(Value::Array(tools(false)))
        );
        // A bot gated as Sonny does not gate goes without the tools Sonny
        // gates, and loses them if it had them.
        let names = |declared: Value| -> Vec<String> {
            (declared.as_array().into_iter().flatten())
                .filter_map(|tool| tool["name"].as_str().map(str::to_owned))
                .collect()
        };
        for gates in [
            json!([{"tag": "ci", "tools": ["shell"]}]),
            json!([{"tag": "auto", "tools": ["shell"]}, {"tag": "manual", "tools": ["shell"]}]),
        ] {
            let custom = declare(&json!({"gates": gates, "declared": tools(true)}), true).unwrap();
            let custom = names(custom);
            assert!(custom.contains(&"show_agent".to_owned()), "{custom:?}");
            for gated in crate::served::GATED {
                assert!(!custom.contains(&gated.to_owned()), "{gated} in {custom:?}");
            }
        }
        let auto = json!({"gates": [{"tag": "auto", "tools": ["shell"]}]});
        assert!(names(declare(&auto, true).unwrap()).contains(&"run_in_pane".to_owned()));
        // A bot whose own tools leave no room keeps them as they are.
        let full: Vec<Value> = (0..14)
            .map(|n| {
                json!({"name": format!("t{n}"), "description": "d",
                "parameters": {"type": "object"}, "result": "ok"})
            })
            .collect();
        assert_eq!(declare(&json!({"declared": full}), false), None);
        let big = json!({"name": "big", "description": "d".repeat(63 * 1024),
            "parameters": {"type": "object"}, "result": "ok"});
        assert_eq!(declare(&json!({"declared": [big]}), false), None);
    }

    #[test]
    fn a_lead_and_its_tasks_resolve_tasks() {
        let lead = declare(&json!({}), true).unwrap();
        assert_eq!(lead, Value::Array(tools(true)));
        assert!(
            lead.as_array()
                .unwrap()
                .iter()
                .any(|t| t["name"] == "resolve_task")
        );
        assert_eq!(declare(&json!({"declared": lead}), true), None);
        // A bot no longer a lead gives it up, unless a lead made it: a
        // task resolves itself.
        assert_eq!(
            declare(&json!({"declared": lead}), false),
            Some(Value::Array(tools(false)))
        );
        let task = json!({"created_by": "calc.lead"});
        let own = declare(&task, false).unwrap();
        assert!(
            own.as_array()
                .unwrap()
                .iter()
                .any(|t| t["name"] == "resolve_task")
        );
        let mut kept = task.clone();
        kept["declared"] = own;
        assert_eq!(declare(&kept, false), None);
        // A task's own helper keeps what it took from the task.
        let helper = json!({"created_by": "calc.fix", "declared": [crate::resolved::tool()]});
        let kept = declare(&helper, false).unwrap();
        assert!(
            kept.as_array()
                .unwrap()
                .iter()
                .any(|t| t["name"] == "resolve_task")
        );
        // What a lead declares, a task it starts takes from its first turn.
        assert!(crate::resolved::tool().get("inherit").is_none());
    }
}
