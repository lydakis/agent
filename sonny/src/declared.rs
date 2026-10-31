//! The tools Sonny declares on the bots it opens: the daemon shows them to
//! the model and records each call, with a fixed result or with the one
//! Sonny's lookout answers (`served`), and Sonny gives the calls their
//! meaning. A bot reacts to the person, asks them to decide something,
//! keeps its plan where they see it, keeps notes in the margin beside the
//! conversation, and puts windows and panes where they look; a lead also
//! resolves its tasks, makes projects and subscribes to the lookout, a
//! task resolves itself, and any agent with room below it in its lead's
//! line starts and removes tasks.
use crate::served::Role;
use agent_client::Client;
use serde_json::{Map, Value, json};

/// The most tools a bot may have declared, and the most bytes they take,
/// as the daemon counts them.
const MAX_DECLARED: usize = 32;
const DECLARED_BYTES: usize = 64 * 1024;

/// Sonny's tools, as it declares them on a bot in `role`.
pub fn tools(role: Role) -> Vec<Value> {
    let mut tools = vec![
        crate::reactions::tool(),
        crate::asks::tool(),
        crate::plan::tool(),
        crate::margin::tool(),
    ];
    if role.lead {
        tools.push(crate::resolved::tool());
    }
    tools.extend(crate::served::tools(role));
    tools
}

/// Where the bot `record` describes stands in its lead's line: how many
/// generations below the lead it is, 0 for the lead or a side chat of it,
/// and the lead's record, `None` for a line no lead heads, such as one an
/// agent the person made with `--new` starts. It walks no further than
/// knowing the bot is `limit` deep needs, 0 for no end. A line whose
/// maker is gone is as deep as the limit: removing a finished task
/// never gives what it made more room.
pub async fn line(client: &Client, record: &Value, limit: u64) -> (u64, Option<Value>) {
    let mut depth = 0;
    let mut at = record.clone();
    // A line of agents each made by the one before; a cycle the store
    // cannot hold, but a bound all the same.
    for _ in 0..256 {
        let name = at["name"].as_str().unwrap_or_default();
        if crate::project::is_lead(name) {
            return (depth, Some(at));
        }
        // A side chat is a copy of its agent, as deep as it is.
        if crate::side_copy(&at).is_none() {
            depth += 1;
        }
        if limit != 0 && depth >= limit {
            return (depth, None);
        }
        let Some(by) = at["created_by"].as_str().map(str::to_owned) else {
            return (depth, None);
        };
        let asked = json!({"bot": by, "bot_id": at["created_by_id"]});
        match client.request("inspect", asked).await {
            Ok(mut maker) => {
                maker["name"] = json!(by);
                at = maker;
            }
            Err(_) => return (gone(depth, limit), None),
        }
    }
    (depth, None)
}

/// [`line`], with `maker` giving the bot that made a bot, as a listing of
/// every bot already read does, or the CLI.
pub fn line_by(
    record: &Value,
    limit: u64,
    maker: impl Fn(&Value) -> Option<Value>,
) -> (u64, Option<Value>) {
    let mut depth = 0;
    let mut at = record.clone();
    for _ in 0..256 {
        if crate::project::is_lead(at["name"].as_str().unwrap_or_default()) {
            return (depth, Some(at));
        }
        if crate::side_copy(&at).is_none() {
            depth += 1;
        }
        if limit != 0 && depth >= limit {
            return (depth, None);
        }
        if !at["created_by"].is_string() {
            return (depth, None);
        }
        // The maker by the id it made it with, not another of its name.
        match maker(&at) {
            Some(next)
                if at["created_by_id"].is_null()
                    || next["bot_id"].is_null()
                    || next["bot_id"] == at["created_by_id"] =>
            {
                at = next
            }
            _ => return (gone(depth, limit), None),
        }
    }
    (depth, None)
}

/// How deep a line whose maker is gone counts: as deep as `limit` lets
/// lines go, so it starts none.
fn gone(depth: u64, limit: u64) -> u64 {
    match limit {
        0 => depth,
        _ => depth.max(limit),
    }
}

/// The role of the bot `record` describes, by where it stands in its
/// lead's line, as deep as the person's config lets lines go.
pub async fn role(client: &Client, record: &Value) -> Role {
    let limit = crate::served::depth_limit();
    let (depth, _) = line(client, record, limit).await;
    Role::at(depth, limit)
}

/// Whether the bot `record` describes resolves itself: a lead made it, or
/// another bot made it with `resolve_task`, as a task's own helper takes it.
fn resolves(record: &Value) -> bool {
    let inherited = || {
        (record["declared"].as_array().into_iter().flatten())
            .any(|tool| tool["name"] == crate::resolved::TOOL)
    };
    match record["created_by"].as_str() {
        Some(by) => crate::project::is_lead(by) || inherited(),
        None => false,
    }
}

/// The tools the bot `record` describes, in `role`, should declare: its
/// own with Sonny's, or `None` when it has those already, or when they
/// would not fit beside its own.
pub fn declare(record: &Value, role: Role) -> Option<Value> {
    let mut declared = record["declared"].as_array().cloned().unwrap_or_default();
    let mut ours = tools(role);
    // A task resolves itself, as its lead resolves it.
    if !role.lead && resolves(record) {
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
    // The commons is Sonny's while it is off, too, so it gives way.
    let mut names = tools(Role::ALL);
    names.push(crate::served::commons());
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

/// The `configure` fields that bring a bot Sonny declared on before up
/// to this build's tools, as opening its window would, or `None` when it
/// has them or never had Sonny's (a tool Sonny's lookout answers): so an agent no one opens since an
/// upgrade, such as a task out of sight, gets a tool added since. A lead
/// stays a lead, by the tool only a lead has; any other bot takes the role
/// of `depth`, where it stands in its lead's line ([`line`]); and a tool
/// Sonny gates is gated as the bot's mode gates the rest.
pub fn refresh(record: &Value, depth: u64) -> Option<Map<String, Value>> {
    // Sonny's own by a tool its lookout answers, not by a name another
    // client's tool could share.
    let declared = record["declared"].as_array()?;
    if !declared
        .iter()
        .any(|tool| tool["answered_by"] == crate::served::TAG)
    {
        return None;
    }
    let lead = declared
        .iter()
        .any(|tool| tool["name"] == crate::served::PROJECT);
    fields(record, Role::of(if lead { 0 } else { depth.max(1) }))
}

/// The `configure` fields that give Sonny's tools, a task's, to the bot
/// `record` describes when it declares none, as an agent a `--start`
/// trigger made before its agents had them: gated as its mode gates the
/// rest, and left to [`refresh`] after.
pub fn adopt(record: &Value) -> Option<Map<String, Value>> {
    if (record["declared"].as_array()).is_some_and(|tools| !tools.is_empty()) {
        return None;
    }
    fields(record, Role::of(1))
}

/// The `configure` fields that bring `record` to Sonny's tools in `role`,
/// with the approvals its mode gives the ones it gates.
fn fields(record: &Value, role: Role) -> Option<Map<String, Value>> {
    let declare = declare(record, role)?;
    let mut fields = Map::new();
    if let Some(mode) = crate::approvals::mode(record).filter(|mode| *mode != "full") {
        let tools: Vec<String> = (record["tools"].as_array().into_iter().flatten())
            .filter_map(Value::as_str)
            .map(str::to_owned)
            .collect();
        fields.extend(crate::approvals::gate(
            mode,
            &crate::approvals::gateable(&tools, &declare),
        ));
    }
    fields.insert("declare".into(), declare);
    fields.insert("bot".into(), record["name"].clone());
    fields.insert("bot_id".into(), json!(record["bot_id"]));
    Some(fields)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    /// A lead, a task of it, and a task of that task's, in a line two deep.
    fn a_lead() -> Role {
        Role::at(0, 2)
    }
    fn a_task() -> Role {
        Role::at(1, 2)
    }
    fn a_helper() -> Role {
        Role::at(2, 2)
    }

    #[test]
    fn a_bot_is_given_sonnys_tools_beside_its_own_once() {
        let other = json!({"name": "notes", "description": "d", "parameters": {"type": "object"},
            "result": "ok"});
        // A lead's fit beside a few of its own, by count and by size.
        let lead = declare(
            &json!({"declared": [other.clone(), other.clone()]}),
            a_lead(),
        )
        .unwrap();
        assert!(serde_json::to_vec(&lead).unwrap().len() <= DECLARED_BYTES / 2);
        let wanted = declare(&json!({"declared": [other]}), a_helper()).unwrap();
        let mut all = vec![other];
        all.extend(tools(a_helper()));
        assert_eq!(wanted, Value::Array(all));
        assert_eq!(declare(&json!({"declared": wanted}), a_helper()), None);
        // An older wording of one of Sonny's is replaced.
        let old = json!({"name": "react", "description": "old", "parameters": {"type": "object"},
            "result": "ok"});
        assert_eq!(
            declare(&json!({"declared": [old]}), a_helper()),
            Some(Value::Array(tools(a_helper())))
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
            let custom = declare(
                &json!({"gates": gates, "declared": tools(a_lead())}),
                a_lead(),
            )
            .unwrap();
            let custom = names(custom);
            assert!(custom.contains(&"show_agent".to_owned()), "{custom:?}");
            for gated in crate::served::GATED {
                assert!(!custom.contains(&gated.to_owned()), "{gated} in {custom:?}");
            }
        }
        let auto = json!({"gates": [{"tag": "auto", "tools": ["shell"]}]});
        assert!(names(declare(&auto, a_lead()).unwrap()).contains(&"run_in_pane".to_owned()));
        // A bot whose own tools leave no room keeps them as they are.
        let full: Vec<Value> = (0..30)
            .map(|n| {
                json!({"name": format!("t{n}"), "description": "d",
                "parameters": {"type": "object"}, "result": "ok"})
            })
            .collect();
        assert_eq!(declare(&json!({"declared": full}), a_helper()), None);
        let big = json!({"name": "big", "description": "d".repeat(63 * 1024),
            "parameters": {"type": "object"}, "result": "ok"});
        assert_eq!(declare(&json!({"declared": [big]}), a_helper()), None);
    }

    #[test]
    fn an_agent_sonny_declared_on_before_takes_this_build_s_tools() {
        let names = |fields: &Map<String, Value>| -> Vec<String> {
            (fields["declare"].as_array().unwrap().iter())
                .map(|tool| tool["name"].as_str().unwrap().to_owned())
                .collect()
        };
        // Another client's bot, or one with no tools declared, is left alone.
        let other = json!({"name": "x", "declared": [{"name": "theirs"}]});
        assert_eq!(refresh(&other, 1), None);
        // Nor is one whose tool only shares a name with one of Sonny's.
        let named =
            json!({"name": "x", "declared": [{"name": crate::served::MEMORY, "result": "ok"}]});
        assert_eq!(refresh(&named, 1), None);
        assert_eq!(refresh(&json!({"name": "x"}), 1), None);
        // A lead from an older build, missing a tool: it stays a lead.
        let mut old: Vec<Value> = tools(a_lead());
        old.pop();
        let lead = json!({"name": "calc.lead", "bot_id": 7, "declared": old});
        let fields = refresh(&lead, 0).unwrap();
        assert_eq!(fields["bot"], "calc.lead");
        assert_eq!(fields["bot_id"], 7);
        assert!(names(&fields).contains(&crate::served::START.to_owned()));
        assert!(!fields.contains_key("approve"));
        let mut current = lead.clone();
        current["declared"] = fields["declare"].clone();
        assert_eq!(refresh(&current, 0), None);
        // A task under auto: its new tools are gated as the rest are.
        let mut old: Vec<Value> = tools(a_helper());
        old.retain(|tool| tool["name"] != crate::served::RUN);
        let task = json!({"name": "calc.fix", "bot_id": 8, "created_by": "calc.lead",
            "tools": ["shell", "read"], "declared": old,
            "gates": [{"tag": "auto", "tools": ["shell"]}]});
        let fields = refresh(&task, 2).unwrap();
        assert!(!names(&fields).contains(&crate::served::START.to_owned()));
        let approve = fields["approve"].as_array().unwrap();
        assert!(approve.iter().any(|tool| tool == "shell"));
        assert!(approve.iter().any(|tool| tool == crate::served::RUN));
    }

    #[test]
    fn an_agent_declaring_nothing_takes_a_tasks_tools_gated_as_its_mode_gates() {
        let names = |fields: &Map<String, Value>| -> Vec<String> {
            (fields["declare"].as_array().unwrap().iter())
                .map(|tool| tool["name"].as_str().unwrap().to_owned())
                .collect()
        };
        // Ungated: the tools, nothing to approve.
        let full = adopt(&json!({"name": "memory-cleanup", "bot_id": 3})).unwrap();
        assert!(names(&full).contains(&crate::served::MEMORY.to_owned()));
        assert!(!names(&full).contains(&crate::served::PROJECT.to_owned()));
        assert!(!full.contains_key("approve"));
        // Under auto, the ones Sonny gates wait for the verdict as the rest do.
        let auto = json!({"name": "memory-cleanup", "bot_id": 3, "tools": ["shell"],
            "gates": [{"tag": "auto", "tools": ["shell"]}]});
        let fields = adopt(&auto).unwrap();
        let approve = fields["approve"].as_array().unwrap();
        assert!(approve.iter().any(|tool| tool == crate::served::RUN));
        // One that declares anything is left to refresh.
        assert_eq!(adopt(&json!({"declared": [{"name": "theirs"}]})), None);
    }

    #[test]
    fn starting_tasks_reaches_as_deep_as_the_line_goes() {
        let role = |lead, starts, passes| Role {
            lead,
            starts,
            passes,
        };
        // Two deep: the lead's tasks start their own, which start none.
        assert_eq!(a_lead(), role(true, true, true));
        assert_eq!(a_task(), role(false, true, false));
        assert_eq!(a_helper(), role(false, false, false));
        // One deep: only the lead starts tasks.
        assert_eq!(Role::at(0, 1), role(true, true, false));
        assert_eq!(Role::at(1, 1), role(false, false, false));
        // No end.
        assert_eq!(Role::at(40, 0), role(false, true, true));
        let named = |role: Role, name: &str| -> Option<Value> {
            tools(role).into_iter().find(|tool| tool["name"] == name)
        };
        // A lead's start and removal go to its tasks from their first
        // turn; a task's stay its own, so its helpers start none.
        for name in [crate::served::START, crate::served::REMOVE] {
            assert_eq!(named(a_lead(), name).unwrap().get("inherit"), None);
            assert_eq!(named(a_task(), name).unwrap()["inherit"], false);
            assert_eq!(named(a_helper(), name), None);
        }
        // Making projects is a lead's alone, and never passed on.
        assert_eq!(
            named(a_lead(), crate::served::PROJECT).unwrap()["inherit"],
            false
        );
        assert_eq!(named(a_task(), crate::served::PROJECT), None);
        assert_eq!(named(Role::at(3, 0), crate::served::PROJECT), None);
    }

    #[test]
    fn a_line_whose_maker_is_gone_is_as_deep_as_lines_go() {
        let bots = [
            json!({"name": "shop.lead", "bot_id": 1}),
            json!({"name": "shop.fix", "bot_id": 2, "created_by": "shop.lead", "created_by_id": 1}),
            json!({"name": "shop.check", "bot_id": 3, "created_by": "shop.fix", "created_by_id": 2}),
        ];
        let maker = |bots: &[Value]| {
            let bots = bots.to_vec();
            move |at: &Value| bots.iter().find(|b| b["name"] == at["created_by"]).cloned()
        };
        assert_eq!(line_by(&bots[2], 2, maker(&bots)).0, 2);
        assert_eq!(line_by(&bots[1], 3, maker(&bots)).0, 1);
        // Its task removed, a helper gets no more room.
        let without = [bots[0].clone(), bots[2].clone()];
        assert_eq!(line_by(&bots[2], 3, maker(&without)).0, 3);
        // Nor from another that took its task's name.
        let renamed = json!({"name": "shop.fix", "bot_id": 9, "created_by": "shop.lead",
            "created_by_id": 1});
        let reused = [bots[0].clone(), renamed, bots[2].clone()];
        assert_eq!(line_by(&bots[2], 3, maker(&reused)).0, 3);
        // With no end to lines, none is counted deeper than it is.
        assert_eq!(line_by(&bots[2], 0, maker(&without)).0, 1);
        // One the person made, which no agent made, is its line's root.
        let theirs = json!({"name": "shop.solo", "bot_id": 4});
        assert_eq!(line_by(&theirs, 2, maker(&bots)).0, 1);
    }

    #[test]
    fn a_lead_and_its_tasks_resolve_tasks() {
        let lead = declare(&json!({}), a_lead()).unwrap();
        assert_eq!(lead, Value::Array(tools(a_lead())));
        assert!(
            lead.as_array()
                .unwrap()
                .iter()
                .any(|t| t["name"] == "resolve_task")
        );
        assert_eq!(declare(&json!({"declared": lead}), a_lead()), None);
        // A bot no longer a lead gives it up, unless a lead made it: a
        // task resolves itself.
        assert_eq!(
            declare(&json!({"declared": lead}), a_helper()),
            Some(Value::Array(tools(a_helper())))
        );
        let task = json!({"created_by": "calc.lead"});
        let own = declare(&task, a_helper()).unwrap();
        assert!(
            own.as_array()
                .unwrap()
                .iter()
                .any(|t| t["name"] == "resolve_task")
        );
        let mut kept = task.clone();
        kept["declared"] = own;
        assert_eq!(declare(&kept, a_helper()), None);
        // A task's own helper keeps what it took from the task.
        let helper = json!({"created_by": "calc.fix", "declared": [crate::resolved::tool()]});
        let kept = declare(&helper, a_helper()).unwrap();
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
