//! The tools Sonny declares on the bots it opens: the daemon shows them to
//! the model and records each call, with a fixed result or with the one
//! Sonny's lookout answers (`served`), and Sonny gives the calls their
//! meaning. A bot reacts to the person, asks them to decide something,
//! keeps its plan where they see it, keeps notes in the margin beside the
//! conversation, and puts windows and panes where they look; a lead also
//! resolves its tasks, makes projects and subscribes to the lookout, a
//! task resolves itself, and every agent starts and removes tasks, as deep
//! as the daemon lets a line go.
use crate::served::Role;
use serde_json::{Map, Value, json};

/// The most tools a bot may have declared, and the most bytes they take,
/// as the daemon counts them.
const MAX_DECLARED: usize = 32;
const DECLARED_BYTES: usize = 64 * 1024;

/// Sonny's tools, as it declares them on a bot in `role`: a workflow's
/// worker, which no one talks to, goes without those that reach the
/// person.
pub fn tools(role: Role) -> Vec<Value> {
    if role.worker {
        return crate::served::tools(role).into_iter().map(kinded).collect();
    }
    let mut tools = vec![
        crate::reactions::tool(),
        crate::asks::tool(),
        crate::asks::close_tool(),
        crate::plan::tool(),
        crate::margin::tool(),
    ];
    if role.lead {
        tools.push(crate::resolved::tool());
    }
    tools.extend(crate::served::tools(role));
    tools.into_iter().map(kinded).collect()
}

/// `tool` with the kind its calls' rows in the agent's audit trail name:
/// what it does, in the trail's words. Memory is files; a script's calls
/// are MCP's, and the browser and the desktop their own; the rest work
/// with agents and the person.
fn kinded(mut tool: Value) -> Value {
    let kind = match tool["name"].as_str().unwrap_or_default() {
        crate::served::MEMORY => "file",
        crate::served::EXEC => "mcp",
        crate::served::BROWSER => "browser",
        crate::served::DESKTOP => "desktop",
        _ => "agents",
    };
    tool["kind"] = json!(kind);
    tool
}

/// The lead whose line the bot `record` describes is in: the nearest of
/// its makers that leads, with `maker` giving the bot that made a bot, as
/// a listing of every bot already read does, or the CLI. `None` for a
/// line no lead heads, such as one an agent the person made with `--new`
/// starts, or one whose maker is gone.
pub fn lead_by(record: &Value, maker: impl Fn(&Value) -> Option<Value>) -> Option<Value> {
    let mut at = record.clone();
    // A line of agents each made by the one before; a cycle the store
    // cannot hold, but a bound all the same.
    for _ in 0..256 {
        if crate::project::is_lead(at["name"].as_str().unwrap_or_default()) {
            return Some(at);
        }
        if !at["created_by"].is_string() {
            return None;
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
            _ => return None,
        }
    }
    None
}

/// The role of the bot `record` describes: a lead's or a workflow
/// worker's, by its name, or a task's.
pub fn role(record: &Value) -> Role {
    let name = record["name"].as_str().unwrap_or_default();
    match (
        crate::project::is_lead(name),
        crate::workflow::is_worker(name),
    ) {
        (true, _) => Role::LEAD,
        (false, true) => Role::WORKER,
        (false, false) => Role::TASK,
    }
}

/// Whether the bot `record` describes resolves itself: a lead made it, or
/// another bot made it with `mark_done`, as a task's own helper takes it.
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
/// would not fit beside its own ([`too_many`] says so).
pub fn declare(record: &Value, role: Role) -> Option<Value> {
    let declared = wanted(record, role)?;
    fits(&declared).then_some(Value::Array(declared))
}

/// Why the bot `record` describes cannot take this build's tools in
/// `role`: its own beside Sonny's are past what the daemon takes. `None`
/// when they fit, or it has them already.
pub fn too_many(record: &Value, role: Role) -> Option<String> {
    let declared = wanted(record, role)?;
    let bytes = serde_json::to_vec(&declared).map_or(0, |json| json.len());
    (!fits(&declared)).then(|| {
        format!(
            "{} tools of {bytes} bytes, past the {MAX_DECLARED} tools and {DECLARED_BYTES} bytes an agent takes",
            declared.len()
        )
    })
}

/// The tools [`declare`] would give the bot `record` describes, in
/// `role`, fitting or not; `None` when it has them already.
fn wanted(record: &Value, role: Role) -> Option<Vec<Value>> {
    let mut declared = record["declared"].as_array().cloned().unwrap_or_default();
    let mut ours = tools(role);
    // A task resolves itself, as its lead resolves it.
    if !role.lead && !role.worker && resolves(record) {
        ours.push(kinded(crate::resolved::tool()));
    }
    // A gate Sonny did not set, as another client's tag or several, is
    // not one Sonny can add its gated tools to, so that bot goes without
    // them rather than run them unjudged.
    if crate::approvals::mode(record).is_none() {
        ours.retain(|tool| {
            !(tool["name"].as_str()).is_some_and(|name| crate::served::GATED.contains(&name))
        });
    }
    // The commons is Sonny's while it is off, too, so it gives way, and
    // so does any tool Sonny answers that this build no longer declares.
    let mut names = tools(Role::ALL);
    names.push(crate::served::commons());
    let sonnys = |other: &Value| {
        other["answered_by"] == crate::served::TAG
            || names.iter().any(|tool| tool["name"] == other["name"])
    };
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
    Some(declared)
}

/// Whether `declared` is within what the daemon takes of a bot's tools.
fn fits(declared: &[Value]) -> bool {
    let bytes = serde_json::to_vec(declared).map_or(usize::MAX, |json| json.len());
    declared.len() <= MAX_DECLARED && bytes <= DECLARED_BYTES
}

/// The `configure` fields that bring a bot Sonny declared on before up
/// to this build's tools, as opening its window would, or `None` when it
/// has them or never had Sonny's (a tool Sonny's lookout answers): so an agent no one opens since an
/// upgrade, such as a task out of sight, gets a tool added since. A lead
/// stays a lead, by the tool only a lead has; any other bot takes a
/// task's; and a tool Sonny gates is gated as the bot's mode gates the
/// rest.
pub fn refresh(record: &Value) -> Option<Map<String, Value>> {
    // A trigger's shadow copy keeps the few tools it was made with, and
    // no gate a person answers.
    if (record["name"].as_str()).is_some_and(|name| crate::trigger::shadow_source(name).is_some()) {
        return None;
    }
    // Sonny's own by a tool its lookout answers, not by a name another
    // client's tool could share.
    let declared = record["declared"].as_array()?;
    if !declared
        .iter()
        .any(|tool| tool["answered_by"] == crate::served::TAG)
    {
        return None;
    }
    // A workflow's worker stays one, by its name.
    let role = match (record["name"].as_str()).is_some_and(crate::workflow::is_worker) {
        true => Role::WORKER,
        false => declared_role(declared),
    };
    fields(record, role)
}

/// The role of a bot Sonny declared `declared` on: a lead by the tool only
/// a lead has; any other, a task's.
pub fn declared_role(declared: &[Value]) -> Role {
    match declared
        .iter()
        .any(|tool| tool["name"] == crate::served::PROJECT)
    {
        true => Role::LEAD,
        false => Role::TASK,
    }
}

/// The `configure` fields that give Sonny's tools, a task's, to the bot
/// `record` describes when it declares none, as an agent a `--start`
/// trigger made before its agents had them: gated as its mode gates the
/// rest, and left to [`refresh`] after.
pub fn adopt(record: &Value) -> Option<Map<String, Value>> {
    if (record["declared"].as_array()).is_some_and(|tools| !tools.is_empty()) {
        return None;
    }
    fields(record, Role::TASK)
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

    /// A lead, and any other agent: a task of it, or a task of that task's.
    fn a_lead() -> Role {
        Role::LEAD
    }
    fn a_task() -> Role {
        Role::TASK
    }
    fn a_helper() -> Role {
        Role::TASK
    }

    #[test]
    fn tools_past_the_daemon_s_cap_are_said_not_skipped() {
        let own = |bytes: usize| {
            json!({"name": "notes", "description": "d".repeat(bytes),
                "parameters": {"type": "object"}, "result": "ok"})
        };
        let small = json!({"declared": [own(10)]});
        assert!(declare(&small, a_lead()).is_some());
        assert_eq!(too_many(&small, a_lead()), None);
        let large = json!({"declared": [own(DECLARED_BYTES)]});
        assert_eq!(declare(&large, a_lead()), None);
        let why = too_many(&large, a_lead()).unwrap();
        assert!(why.contains("bytes an agent takes"), "{why}");
        // A task that resolves itself, at every size around the cap: what
        // does not fit is always said, with the tools `declare` would give.
        let resolving = crate::resolved::tool();
        let fitted = declare(&json!({"declared": [own(0), resolving.clone()]}), a_task()).unwrap();
        let base = serde_json::to_vec(&fitted).unwrap().len();
        let edge = DECLARED_BYTES - base;
        for bytes in edge - 300..edge + 300 {
            let task = json!({"declared": [own(bytes), resolving.clone()]});
            let skipped = declare(&task, a_task()).is_none();
            assert_eq!(skipped, too_many(&task, a_task()).is_some(), "{bytes}");
        }
        assert_eq!(
            declared_role(&[json!({"name": crate::served::PROJECT})]),
            a_lead()
        );
        assert_eq!(declared_role(&[]), a_task());
    }

    #[test]
    fn a_workflow_s_worker_starts_no_agents_and_stays_a_worker() {
        let names = |tools: Vec<Value>| -> Vec<String> {
            (tools.iter())
                .filter_map(|tool| tool["name"].as_str().map(str::to_owned))
                .collect()
        };
        let worker = names(tools(Role::WORKER));
        assert!(worker.contains(&crate::served::MEMORY.to_owned()));
        for none in [
            crate::served::START,
            crate::served::REMOVE,
            crate::served::WORKFLOW,
            crate::reactions::TOOL,
            crate::asks::TOOL,
            crate::resolved::TOOL,
        ] {
            assert!(!worker.contains(&none.to_owned()), "{none}");
        }
        // A task runs workflows, while they are on.
        assert_eq!(
            names(tools(Role::TASK)).contains(&crate::served::WORKFLOW.to_owned()),
            crate::workflow::on()
        );
        // A lead made it, yet it marks nothing done, and a refresh keeps it
        // a worker.
        let record = json!({"name": "shop.wf-1a2b3c4d-0a1b2c3d4e5f", "created_by": "shop.lead",
            "declared": tools(Role::WORKER)});
        assert_eq!(role(&record), Role::WORKER);
        assert_eq!(declare(&record, Role::WORKER), None);
        assert!(
            refresh(&record).is_none_or(|fields| fields["declare"] == json!(tools(Role::WORKER)))
        );
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
        // Sonny's own take at most half of the daemon's budget, the rest
        // left to the agent's.
        let size = serde_json::to_vec(&lead).unwrap().len();
        assert!(
            size <= DECLARED_BYTES / 2,
            "{size} over {}",
            DECLARED_BYTES / 2
        );
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
        assert!(names(declare(&auto, a_lead()).unwrap()).contains(&"start_task".to_owned()));
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
        assert_eq!(refresh(&other), None);
        // Nor is one whose tool only shares a name with one of Sonny's.
        let named =
            json!({"name": "x", "declared": [{"name": crate::served::MEMORY, "result": "ok"}]});
        assert_eq!(refresh(&named), None);
        assert_eq!(refresh(&json!({"name": "x"})), None);
        // A lead from an older build, missing a tool: it stays a lead.
        let mut old: Vec<Value> = tools(a_lead());
        old.pop();
        let lead = json!({"name": "calc.lead", "bot_id": 7, "declared": old});
        let fields = refresh(&lead).unwrap();
        assert_eq!(fields["bot"], "calc.lead");
        assert_eq!(fields["bot_id"], 7);
        assert!(names(&fields).contains(&crate::served::START.to_owned()));
        assert!(!fields.contains_key("approve"));
        let mut current = lead.clone();
        current["declared"] = fields["declare"].clone();
        assert_eq!(refresh(&current), None);
        // A task under auto: its new tools are gated as the rest are.
        let mut old: Vec<Value> = tools(a_helper());
        old.retain(|tool| tool["name"] != crate::served::FOLLOWS);
        let task = json!({"name": "calc.fix", "bot_id": 8, "created_by": "calc.lead",
            "tools": ["shell", "read"], "declared": old,
            "gates": [{"tag": "auto", "tools": ["shell"]}]});
        let fields = refresh(&task).unwrap();
        // Every agent starts tasks, as deep as the daemon lets it.
        assert!(names(&fields).contains(&crate::served::START.to_owned()));
        assert!(!names(&fields).contains(&crate::served::PROJECT.to_owned()));
        let approve = fields["approve"].as_array().unwrap();
        assert!(approve.iter().any(|tool| tool == "shell"));
        assert!(approve.iter().any(|tool| tool == crate::served::FOLLOWS));
    }

    #[test]
    fn a_tool_sonny_answered_that_this_build_dropped_goes_with_the_refresh() {
        let mut old: Vec<Value> = tools(a_lead());
        old.push(json!({"name": "goal", "parameters": {"type": "object"},
            "answered_by": crate::served::TAG}));
        old.push(json!({"name": "theirs", "result": "ok"}));
        let fields = refresh(&json!({"name": "calc.lead", "bot_id": 7, "declared": old})).unwrap();
        let names: Vec<&str> = (fields["declare"].as_array().unwrap().iter())
            .filter_map(|tool| tool["name"].as_str())
            .collect();
        assert!(!names.contains(&"goal"));
        assert!(names.contains(&"theirs"));
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
        assert!(approve.iter().any(|tool| tool == crate::served::FOLLOWS));
        // One that declares anything is left to refresh.
        assert_eq!(adopt(&json!({"declared": [{"name": "theirs"}]})), None);
    }

    #[test]
    fn every_agent_starts_tasks_and_a_lead_alone_makes_projects() {
        let named = |role: Role, name: &str| -> Option<Value> {
            tools(role).into_iter().find(|tool| tool["name"] == name)
        };
        // Start and removal reach every generation: the daemon bounds how
        // deep a line goes, and says so when a start goes past it.
        for name in [crate::served::START, crate::served::REMOVE] {
            for role in [a_lead(), a_task(), a_helper()] {
                assert_eq!(named(role, name).unwrap().get("inherit"), None);
            }
        }
        // Making projects is a lead's alone, and never passed on.
        assert_eq!(
            named(a_lead(), crate::served::PROJECT).unwrap()["inherit"],
            false
        );
        assert_eq!(named(a_task(), crate::served::PROJECT), None);
        assert_eq!(role(&json!({"name": "shop.lead"})), Role::LEAD);
        assert_eq!(role(&json!({"name": "shop.lead.web"})), Role::LEAD);
        assert_eq!(role(&json!({"name": "shop.fix"})), Role::TASK);
    }

    #[test]
    fn a_line_s_lead_is_its_nearest_maker_that_leads() {
        let bots = [
            json!({"name": "shop.lead", "bot_id": 1}),
            json!({"name": "shop.fix", "bot_id": 2, "created_by": "shop.lead", "created_by_id": 1}),
            json!({"name": "shop.check", "bot_id": 3, "created_by": "shop.fix", "created_by_id": 2}),
        ];
        let maker = |bots: &[Value]| {
            let bots = bots.to_vec();
            move |at: &Value| bots.iter().find(|b| b["name"] == at["created_by"]).cloned()
        };
        let lead = |record: &Value, bots: &[Value]| {
            lead_by(record, maker(bots)).map(|lead| lead["name"].clone())
        };
        assert_eq!(lead(&bots[2], &bots), Some(json!("shop.lead")));
        assert_eq!(lead(&bots[0], &bots), Some(json!("shop.lead")));
        // Its task removed, a helper has no lead.
        let without = [bots[0].clone(), bots[2].clone()];
        assert_eq!(lead(&bots[2], &without), None);
        // Nor through another that took its task's name.
        let renamed = json!({"name": "shop.fix", "bot_id": 9, "created_by": "shop.lead",
            "created_by_id": 1});
        let reused = [bots[0].clone(), renamed, bots[2].clone()];
        assert_eq!(lead(&bots[2], &reused), None);
        // One the person made, which no agent made, heads no lead's line.
        let theirs = json!({"name": "shop.solo", "bot_id": 4});
        assert_eq!(lead(&theirs, &bots), None);
    }

    #[test]
    fn a_lead_and_its_tasks_mark_tasks_done() {
        let lead = declare(&json!({}), a_lead()).unwrap();
        assert_eq!(lead, Value::Array(tools(a_lead())));
        assert!(
            lead.as_array()
                .unwrap()
                .iter()
                .any(|t| t["name"] == "mark_done")
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
                .any(|t| t["name"] == "mark_done")
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
                .any(|t| t["name"] == "mark_done")
        );
        // What a lead declares, a task it starts takes from its first turn.
        assert!(crate::resolved::tool().get("inherit").is_none());
    }
}
