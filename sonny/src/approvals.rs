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

/// The sandbox fields, as `configure` takes them and `create`'s settings
/// hold them, for an agent working in `workspace`: Codex's
/// `workspace-write`, reaching the network, with the folder's repository
/// writable too so a commit runs without asking (its hooks and config stay
/// read-only), and the commands the person's config lets out of it
/// (`[sonny] unsandboxed`). In `full` mode an agent runs unconfined, as
/// under Codex's full access, where nothing would answer a call that asks
/// to leave the sandbox.
pub fn sandbox(mode: &str, workspace: &std::path::Path) -> Map<String, Value> {
    sandbox_with(mode, workspace, &unsandboxed())
}

/// [`sandbox`]'s fields, with `unsandboxed` the commands let out of it.
fn sandbox_with(
    mode: &str,
    workspace: &std::path::Path,
    unsandboxed: &[String],
) -> Map<String, Value> {
    let fields = match mode {
        "full" => json!({"sandbox": "danger-full-access"}),
        _ => {
            let writable: Vec<String> = repository(workspace).into_iter().collect();
            let mut fields =
                json!({"sandbox": "workspace-write", "writable": writable, "network": true});
            if !unsandboxed.is_empty() {
                fields["unsandboxed"] = json!(unsandboxed);
            }
            fields
        }
    };
    match fields {
        Value::Object(fields) => fields,
        _ => Map::new(),
    }
}

/// The commands the person's config lets agents run outside their
/// sandboxes; none when it can't be read, which Sonny reports on start.
pub fn unsandboxed() -> Vec<String> {
    agent_client::config::load()
        .map(|config| config.sonny.unsandboxed)
        .unwrap_or_default()
}

/// The git folder of the repository `dir` is in, which its linked
/// worktrees share, by its real path as the daemon keeps it. Only one
/// named as the daemon knows a git folder, `.git` or `NAME.git`: in one
/// named otherwise, such as a separate git folder, its hooks and config
/// would not be kept, so a commit there asks instead.
pub fn repository(dir: &std::path::Path) -> Option<String> {
    let found = crate::task::git(
        dir,
        &["rev-parse", "--path-format=absolute", "--git-common-dir"],
    )
    .ok()
    .filter(|path| !path.is_empty())?;
    let real = std::fs::canonicalize(found).ok()?;
    real.to_str()
        .filter(|path| path.ends_with(".git"))
        .map(str::to_owned)
}

/// Whether `record`'s sandbox is the one `fields` give it: its mode, its
/// writable folders, which a moved folder leaves stale, and its network.
pub fn sandboxed_as(record: &Value, fields: &Map<String, Value>) -> bool {
    let fields = Value::Object(fields.clone());
    let settings = &record["settings"];
    let list = |value: &Value, key: &str| value[key].as_array().cloned().unwrap_or_default();
    settings["sandbox"] == fields["sandbox"]
        && list(settings, "writable") == list(&fields, "writable")
        && list(settings, "unsandboxed") == list(&fields, "unsandboxed")
        && settings["network"].as_bool().unwrap_or(false)
            == fields["network"].as_bool().unwrap_or(false)
}

/// What a bot's gate may hold back: its own `tools`, and of the tools its
/// client declared, those Sonny gates (`served::GATED`).
pub fn gateable(tools: &[String], declared: &Value) -> Vec<String> {
    let mut gateable = tools.to_vec();
    for tool in declared.as_array().into_iter().flatten() {
        if let Some(name) = tool["name"].as_str()
            && crate::served::GATED.contains(&name)
            && !gateable.iter().any(|have| have == name)
        {
            gateable.push(name.to_owned());
        }
    }
    gateable
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

/// The `configure` fields that give a bot `mode`, with the sandbox the
/// mode gives an agent in its folder, or `None` when its gate is already
/// the one `gate` makes and its sandbox the mode's: the same approver over the same tools,
/// with no expiry. `record` is the bot as `inspect` gives it, with the
/// tools its client declared.
pub fn regate(record: &Value, mode: &str) -> Option<Map<String, Value>> {
    let tools: Vec<String> = record["tools"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(Value::as_str)
        .map(str::to_owned)
        .collect();
    let fields = gate(mode, &gateable(&tools, &record["declared"]));
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
    let mut fields = fields;
    let mut confined = true;
    if let Some(workspace) = record["workspace"].as_str() {
        let sandbox = sandbox(mode, std::path::Path::new(workspace));
        confined = sandboxed_as(record, &sandbox);
        fields.extend(sandbox);
    }
    (!has || !confined).then_some(fields)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_agent_is_confined_but_in_full_mode() {
        let dir = std::env::temp_dir().join(format!("sonny-sandbox-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let dir = std::fs::canonicalize(&dir).unwrap();
        crate::task::git(&dir, &["init", "-q"]).unwrap();
        let git = dir.join(".git").display().to_string();
        let confined = json!({"sandbox": "workspace-write", "writable": [git], "network": true});
        for (mode, _) in MODES {
            let fields = sandbox_with(mode, &dir, &[]);
            if mode == "full" {
                continue;
            }
            assert_eq!(Value::Object(fields.clone()), confined);
            assert!(sandboxed_as(&json!({"settings": fields}), &fields));
        }
        assert_eq!(Value::Object(sandbox_with("auto", &dir, &[])), confined);
        let full = sandbox_with("full", &dir, &[]);
        assert_eq!(
            Value::Object(full.clone()),
            json!({"sandbox": "danger-full-access"})
        );
        // A task the person moves to `full` leaves its sandbox with the
        // gate, and one already there is left alone.
        let task = json!({"workspace": dir, "gates": [],
            "settings": {"sandbox": "workspace-write", "writable": [git], "network": true}});
        assert_eq!(
            regate(&task, "full").unwrap()["sandbox"],
            "danger-full-access"
        );
        let freed = json!({"workspace": dir, "gates": [],
            "settings": {"sandbox": "danger-full-access", "writable": [], "network": false}});
        assert_eq!(regate(&freed, "full"), None);
        let unconfined = json!({"settings": {"sandbox": "danger-full-access", "writable": [], "network": false}});
        assert!(sandboxed_as(&unconfined, &full));
        // A lead made before leads were confined is confined again, and
        // one whose repository moved is given it where it is now.
        let auto = sandbox_with("auto", &dir, &[]);
        assert!(!sandboxed_as(&json!({"settings": {}}), &auto));
        let moved = json!({"settings": {"sandbox": "workspace-write", "writable": ["/gone/.git"],
            "network": true}});
        assert!(!sandboxed_as(&moved, &auto));
        // The commands the config lets out of the sandbox go with it, and
        // a lead made before they were named takes them.
        let mcpx = sandbox_with("auto", &dir, &["mcpx".to_owned()]);
        assert_eq!(mcpx["unsandboxed"], json!(["mcpx"]));
        assert!(!sandboxed_as(&json!({"settings": auto}), &mcpx));
        assert!(sandboxed_as(&json!({"settings": mcpx}), &mcpx));
        assert!(
            sandbox_with("full", &dir, &["mcpx".to_owned()])
                .get("unsandboxed")
                .is_none()
        );
        // Outside a repository, only its own folder; with a git folder
        // named otherwise, whose hooks the daemon wouldn't know to keep,
        // commits ask.
        let plain = std::env::temp_dir();
        assert_eq!(sandbox_with("ask", &plain, &[])["writable"], json!([]));
        let apart = dir.join("apart");
        let tree = dir.join("tree");
        std::fs::create_dir_all(&tree).unwrap();
        crate::task::git(
            &tree,
            &["init", "-q", "--separate-git-dir", apart.to_str().unwrap()],
        )
        .unwrap();
        assert_eq!(sandbox_with("ask", &tree, &[])["writable"], json!([]));
        std::fs::remove_dir_all(&dir).unwrap();
    }

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
        // Of the tools its client declared, those that can drop work, run
        // something outside the sandbox or send words elsewhere wait for
        // the verdict too.
        let panes = json!({"tools": ["shell", "read"], "declared": crate::declared::tools(true),
            "gates": [{"tag": "auto", "tools": ["shell"]}]});
        assert_eq!(
            regate(&panes, "auto").unwrap()["approve"],
            json!([
                "shell",
                "start_task",
                "remove_task",
                "new_project",
                "subscriptions",
                "run_in_pane",
                "triggers"
            ])
        );
    }
}
