use agent_runtime::{
    Error, Result,
    codec::Family,
    provider::{ToolCall, Usage},
    store::{
        Answered, Binding, Bot, CacheTtl, CompactionPlan, ContextUsage, Database, Decision,
        Delivery, Fork, Gate, Gated, Planning, QuoteRef, Settings, Strip, TurnBudget, TurnOptions,
        Wake,
    },
    tools::{Declared, Inherit, Outcome},
};
use bytes::Bytes;
use rusqlite::Connection;
use serde_json::{Value, json};

// These tests isolate store contracts; runtime tests cover provider admission.
fn allow_provider(_: &Bot, _: Option<&str>) -> Result<()> {
    Ok(())
}

fn assistant(text: &str) -> Bytes {
    serde_json::to_vec(&json!({"type":"message","role":"assistant",
        "content":[{"type":"output_text","text":text}]}))
    .unwrap()
    .into()
}
fn db() -> Database {
    Database::initialize(Connection::open_in_memory().unwrap()).unwrap()
}
/// One node's item, read the way clients read it: a batch of one.
fn read_item(db: &Database, bot: &str, node: i64) -> Result<Value> {
    db.history_items(bot, &[node])
        .map(|read| read["items"][0]["item"].clone())
}
/// The read tool, which a bot needs for its results to get stubs.
static READ: std::sync::LazyLock<Vec<String>> = std::sync::LazyLock::new(|| vec!["read".into()]);
fn binding() -> Binding<'static> {
    Binding {
        provider: "openai",
        family: Family::Responses,
        model: "synthetic-model",
        instructions: "test",
        effort: None,
        budget_tokens: None,
        tools: &READ,
        created_by: None,
        created_by_id: None,
        compaction_instructions: None,
        compaction_model: None,
        fallback: None,
        gate: None,
        settings: Default::default(),
        declared: None,
        request_id: None,
    }
}
/// Compaction planning as a turn runs it: a catch-up walk goes in pieces.
fn compaction_plan(
    db: &Database,
    name: &str,
    keep: i64,
    max_bytes: i64,
    max_items: i64,
) -> Result<Option<CompactionPlan>> {
    match db.compaction_plan(name, keep, i64::MAX, max_bytes, max_items, false)? {
        None => Ok(None),
        Some(Planning::Plan(plan)) => Ok(Some(plan)),
        Some(Planning::CatchUp(mut walk)) => {
            while !walk.done() {
                db.catch_up_piece(&mut walk, 16)?;
            }
            db.catch_up_plan(name, walk)
        }
    }
}
/// Every stored item of a bot, through the same window the runtime streams.
fn stored(db: &mut Database, name: &str) -> Vec<Value> {
    let Some(window) = db.window(name, i64::MAX, i64::MAX).unwrap() else {
        return Vec::new();
    };
    let joined = db.items_by_ids(&window.ids, 0, 0).unwrap();
    serde_json::from_slice(&[b"[", &joined[..], b"]"].concat()).unwrap()
}
fn result(output: &str) -> Outcome {
    Outcome {
        output: output.into(),
        artifacts: Vec::new(),
        parts: Vec::new(),
        note: None,
        failed: false,
        files: Default::default(),
    }
}

#[test]
fn historical_fork_and_exact_resume_preserve_independent_lineage() {
    let mut db = db();
    assert!(db.inspect("missing").is_err());
    db.create("Bob", Some("/synthetic/bob"), binding()).unwrap();
    let first = db
        .begin(
            "Bob",
            "r1",
            "first",
            true,
            &TurnOptions::default(),
            allow_provider,
        )
        .unwrap()
        .turn;
    db.append(first, vec![assistant("answer one")], &[], None)
        .unwrap();
    let checkpoint = db.finish(first, None).unwrap().last().unwrap()["data"]["checkpoint"]
        .as_i64()
        .unwrap();
    let second = db
        .begin(
            "Bob",
            "r2",
            "second",
            true,
            &TurnOptions::default(),
            allow_provider,
        )
        .unwrap()
        .turn;
    db.append(second, vec![assistant("answer two")], &[], None)
        .unwrap();
    db.finish(second, None).unwrap();
    db.fork(
        "Bob",
        "Alternative",
        Fork {
            checkpoint: Some(checkpoint),
            ..Fork::default()
        },
    )
    .unwrap();
    assert_eq!(stored(&mut db, "Alternative").len(), 2);
    assert_eq!(stored(&mut db, "Bob").len(), 4);
    assert!(
        db.fork(
            "Bob",
            "bad",
            Fork {
                checkpoint: Some(checkpoint + 1000),
                ..Fork::default()
            }
        )
        .is_err()
    );
    // The fork starts in its source's folder; a turn that names another moves it.
    assert_eq!(
        db.inspect("Alternative").unwrap().workspace.as_deref(),
        Some("/synthetic/bob")
    );
    let branch = TurnOptions {
        workspace: Some("/synthetic/alternative".into()),
        model: None,
        effort: None,
        delivery: Delivery::Reject,
        expected_turn: None,
        from: None,
        origin: None,
        person: None,
        quotes: Vec::new(),
        budget_tokens: None,
        images: Vec::new(),
    };
    let alt = db
        .begin("Alternative", "r1", "different", true, &branch, |_, _| {
            Ok(())
        })
        .unwrap()
        .turn;
    db.append(alt, vec![assistant("another direction")], &[], None)
        .unwrap();
    db.finish(alt, None).unwrap();
    assert_ne!(stored(&mut db, "Alternative"), stored(&mut db, "Bob"));
    let events = db.events("Alternative", 0, 2).unwrap();
    let cursor = events["next_cursor"].as_i64().unwrap();
    let following = db.events("Alternative", cursor, 256).unwrap();
    assert!(
        following["events"]
            .as_array()
            .unwrap()
            .iter()
            .all(|e| e["cursor"].as_i64().unwrap() > cursor)
    );
    assert!(
        db.history_items("Bob", &[db.inspect("Alternative").unwrap().head.unwrap()])
            .is_err()
    );
}

#[test]
fn submission_is_idempotent_and_conflicting_or_overlapping_work_is_rejected() {
    let mut db = db();
    db.create("Bob", Some("/synthetic"), binding()).unwrap();
    let started = db
        .begin(
            "Bob",
            "same",
            "work",
            true,
            &TurnOptions::default(),
            allow_provider,
        )
        .unwrap();
    assert!(started.fresh);
    let retry = db
        .begin(
            "Bob",
            "same",
            "work",
            true,
            &TurnOptions::default(),
            allow_provider,
        )
        .unwrap();
    assert!(!retry.fresh);
    assert_eq!(started.turn, retry.turn);
    assert!(
        db.begin(
            "Bob",
            "same",
            "different",
            true,
            &TurnOptions::default(),
            allow_provider
        )
        .is_err()
    );
    assert!(
        db.begin(
            "Bob",
            "other",
            "work",
            true,
            &TurnOptions::default(),
            allow_provider
        )
        .is_err()
    );
    db.append(started.turn, vec![assistant("done")], &[], None)
        .unwrap();
    db.finish(started.turn, None).unwrap();
    assert!(
        !db.begin(
            "Bob",
            "same",
            "work",
            true,
            &TurnOptions::default(),
            allow_provider
        )
        .unwrap()
        .fresh
    );
    assert_eq!(stored(&mut db, "Bob").len(), 2);
    assert!(
        db.append(started.turn, vec![assistant("late")], &[], None)
            .is_err()
    );
}

#[test]
fn admission_reconciles_retries_without_accepting_fresh_work_at_capacity() {
    let mut db = db();
    db.create("Bob", Some("/synthetic"), binding()).unwrap();
    db.create("Other", Some("/synthetic"), binding()).unwrap();
    let turn = db
        .begin(
            "Bob",
            "same",
            "work",
            true,
            &TurnOptions::default(),
            allow_provider,
        )
        .unwrap()
        .turn;
    let retry = db
        .begin(
            "Bob",
            "same",
            "work",
            false,
            &TurnOptions::default(),
            allow_provider,
        )
        .unwrap();
    assert!(!retry.fresh);
    assert_eq!(retry.turn, turn);
    // The refusal names the field that differs from the first request.
    let changed = db
        .begin(
            "Bob",
            "same",
            "changed",
            false,
            &TurnOptions::default(),
            allow_provider,
        )
        .err()
        .unwrap();
    assert_eq!(changed.code, "idempotency_conflict");
    assert_eq!(changed.facts.unwrap()["field"], "prompt");
    assert_eq!(
        db.begin(
            "Other",
            "fresh",
            "work",
            false,
            &TurnOptions::default(),
            allow_provider
        )
        .err()
        .unwrap()
        .code,
        "active_agent_limit"
    );
    assert!(db.inspect("Other").unwrap().head.is_none());
    assert!(db.inspect("Other").unwrap().running_turn.is_none());
    assert_eq!(
        db.events("Other", 0, 256).unwrap()["events"]
            .as_array()
            .unwrap()
            .len(),
        1
    );
    assert!(
        db.begin(
            "Other",
            "fresh",
            "work",
            true,
            &TurnOptions::default(),
            allow_provider
        )
        .unwrap()
        .fresh
    );
}

#[test]
fn unfinished_tools_are_answered_truthfully_without_disabling_the_bot() {
    for family in [Family::Responses, Family::Anthropic] {
        for error in [
            "cancelled",
            "daemon_shutdown",
            "process_interrupted",
            "provider_failed",
        ] {
            let mut db = db();
            db.create(
                "Bob",
                Some("/synthetic"),
                Binding {
                    family,
                    compaction_instructions: None,
                    compaction_model: None,
                    fallback: None,
                    gate: None,
                    ..binding()
                },
            )
            .unwrap();
            let turn = db
                .begin(
                    "Bob",
                    "request",
                    "work",
                    true,
                    &TurnOptions::default(),
                    allow_provider,
                )
                .unwrap()
                .turn;
            let calls = ["executing", "planned"].map(|id| ToolCall {
                custom: false,
                name: "write".into(),
                call_id: id.into(),
                arguments: r#"{"path":"file","content":"hello"}"#.into(),
            });
            let items = match family {
                Family::Responses => calls.iter().map(|call| json!({"type":"function_call","call_id":call.call_id,"name":call.name,"arguments":call.arguments})).collect::<Vec<_>>(),
                Family::Anthropic => vec![json!({"role":"assistant","content":calls.iter().map(|call| json!({"type":"tool_use","id":call.call_id,"name":call.name,"input":{"path":"file","content":"hello"}})).collect::<Vec<_>>()})],
            };
            db.append(
                turn,
                items
                    .iter()
                    .map(|item| serde_json::to_vec(item).unwrap().into())
                    .collect(),
                &calls,
                None,
            )
            .unwrap();
            db.tool_start(turn, &calls[0]).unwrap();
            let events = db.finish(turn, Some(&Error::new(error))).unwrap();
            assert_eq!(events.len(), 3);
            assert_eq!(events[0]["data"]["outcome_unknown"], true);
            assert_eq!(events[1]["data"]["cancelled"], true);
            let history = stored(&mut db, "Bob");
            for (entry, expected) in history[history.len() - 2..]
                .iter()
                .zip(["tool_outcome_unknown", "cancelled"])
            {
                let output = match family {
                    Family::Responses => &entry["output"],
                    Family::Anthropic => &entry["content"][0]["content"],
                };
                let output: Value = serde_json::from_str(output.as_str().unwrap()).unwrap();
                assert_eq!(output["error"], expected);
                if expected == "tool_outcome_unknown" {
                    assert!(
                        output["detail"]
                            .as_str()
                            .unwrap()
                            .contains("may still be running")
                    );
                }
            }
            assert_eq!(
                db.inspect("Bob").unwrap().status,
                if error == "provider_failed" {
                    "failed"
                } else {
                    "interrupted"
                }
            );
            assert!(
                db.begin(
                    "Bob",
                    "retry",
                    "inspect before continuing",
                    true,
                    &TurnOptions::default(),
                    allow_provider
                )
                .unwrap()
                .fresh
            );
        }
    }
}

#[test]
fn restart_repairs_unanswered_tools_once_including_previously_blocked_bots() {
    for (family, state) in [
        (Family::Responses, "running"),
        (Family::Responses, "blocked"),
        (Family::Responses, "pruned"),
        (Family::Anthropic, "pruned"),
    ] {
        let path = std::env::temp_dir().join(format!(
            "agent-repair-{}-{}-{state}.sqlite",
            std::process::id(),
            family.name()
        ));
        let _ = std::fs::remove_file(&path);
        let turn;
        {
            let mut db = Database::initialize(Connection::open(&path).unwrap()).unwrap();
            db.create(
                "Bob",
                Some("/synthetic"),
                Binding {
                    family,
                    compaction_instructions: None,
                    compaction_model: None,
                    fallback: None,
                    gate: None,
                    ..binding()
                },
            )
            .unwrap();
            turn = db
                .begin(
                    "Bob",
                    "request",
                    "write",
                    true,
                    &TurnOptions::default(),
                    allow_provider,
                )
                .unwrap()
                .turn;
            let call = ToolCall {
                custom: false,
                name: "write".into(),
                call_id: "c1".into(),
                arguments: "{}".into(),
            };
            // Keep a completed call in the same turn. A pruned migration
            // must repair only the unanswered suffix, not duplicate results.
            for id in ["done", "c1"] {
                let call = ToolCall {
                    custom: false,
                    call_id: id.into(),
                    ..call.clone()
                };
                let item = match family {
                    Family::Responses => {
                        json!({"type":"function_call","call_id":id,"name":"write","arguments":"{}"})
                    }
                    Family::Anthropic => {
                        json!({"role":"assistant","content":[{"type":"tool_use","id":id,"name":"write","input":{}}]})
                    }
                };
                db.append(
                    turn,
                    vec![serde_json::to_vec(&item).unwrap().into()],
                    std::slice::from_ref(&call),
                    None,
                )
                .unwrap();
                db.tool_start(turn, &call).unwrap();
                if id == "done" {
                    db.tool_finish(turn, id, &result("written")).unwrap();
                }
            }
        }
        if state != "running" {
            let conn = Connection::open(&path).unwrap();
            conn.execute_batch("UPDATE bots SET status='uncertain',running_turn=NULL; UPDATE turns SET status='uncertain'; PRAGMA user_version=19;").unwrap();
            if state == "pruned" {
                conn.execute_batch(
                    "DELETE FROM tools; DELETE FROM events; DELETE FROM retained_turns;",
                )
                .unwrap();
            }
        }
        let mut original = None;
        for _ in 0..2 {
            let mut db = Database::initialize(Connection::open(&path).unwrap()).unwrap();
            assert_eq!(db.turn_status("Bob", turn).unwrap(), "interrupted");
            let history = stored(&mut db, "Bob");
            assert_eq!(history.len(), 5);
            let encoded = match family {
                Family::Responses => &history[4]["output"],
                Family::Anthropic => &history[4]["content"][0]["content"],
            };
            let output: Value = serde_json::from_str(encoded.as_str().unwrap()).unwrap();
            assert_eq!(output["error"], "tool_outcome_unknown");
            if let Some(ref original) = original {
                assert_eq!(&history, original);
            }
            original = Some(history);
        }
        {
            let mut db = Database::initialize(Connection::open(&path).unwrap()).unwrap();
            db.begin(
                "Bob",
                "next",
                "inspect and continue",
                true,
                &TurnOptions::default(),
                allow_provider,
            )
            .unwrap();
        }
        for suffix in ["", "-wal", "-shm"] {
            let _ = std::fs::remove_file(format!("{}{suffix}", path.display()));
        }
    }
}

#[test]
fn tool_results_and_cursor_events_commit_together() {
    let mut db = db();
    db.create("Bob", Some("/synthetic"), binding()).unwrap();
    let turn = db
        .begin(
            "Bob",
            "request",
            "work",
            true,
            &TurnOptions::default(),
            allow_provider,
        )
        .unwrap()
        .turn;
    let call = ToolCall {
        custom: false,
        name: "echo".into(),
        call_id: "c1".into(),
        arguments: "{}".into(),
    };
    db.append(turn, vec![], std::slice::from_ref(&call), None)
        .unwrap();
    assert!(db.tool_finish(turn, "c1", &result("too early")).is_err());
    let started = db.tool_start(turn, &call).unwrap();
    assert_eq!(started["event"], "tool_started");
    assert!(started["cursor"].as_i64().is_some());
    db.tool_finish(turn, "c1", &result("hi")).unwrap();
    assert!(db.tool_start(turn, &call).is_err());
    db.append(turn, vec![assistant("done")], &[], None).unwrap();
    assert_eq!(
        db.finish(turn, None).unwrap().last().unwrap()["data"]["status"],
        "completed"
    );
    assert!(
        db.events("Bob", 0, 256).unwrap()["events"]
            .as_array()
            .unwrap()
            .iter()
            .any(|e| e["event"] == "tool_completed")
    );
}

/// A running turn of Bob, whose `shell` calls wait on a `manual` gate.
fn gated_turn(db: &mut Database, expire_ms: Option<u64>) -> i64 {
    let tools = ["shell".to_owned()];
    let gate = Gate {
        tag: "manual".into(),
        tools: vec!["shell".into()],
        expire_ms,
    };
    let binding = Binding {
        tools: &tools,
        gate: Some(&gate),
        ..binding()
    };
    db.create("Bob", Some("/synthetic"), binding).unwrap();
    db.begin(
        "Bob",
        "request",
        "work",
        true,
        &TurnOptions::default(),
        allow_provider,
    )
    .unwrap()
    .turn
}
/// A shell call and the Responses item, with its own item id, planning it.
fn shell_call(item_id: &str, call_id: &str, command: &str) -> (Bytes, ToolCall) {
    let arguments = json!({"command":command}).to_string();
    let item = json!({"type":"function_call","id":item_id,"call_id":call_id,
        "name":"shell","arguments":arguments});
    let call = ToolCall {
        custom: false,
        name: "shell".into(),
        call_id: call_id.into(),
        arguments,
    };
    (serde_json::to_vec(&item).unwrap().into(), call)
}
fn allow(db: &mut Database, turn: i64, call_id: &str) -> Result<Answered> {
    db.answer(Decision {
        bot: "Bob",
        turn,
        call_id,
        request: 1,
        tag: None,
        allow: true,
        unreviewed: false,
        reason: None,
        by: Some("test"),
    })
}

#[test]
fn late_verdicts_are_refused_and_listings_find_each_calls_own_item() {
    let mut db = db();
    let turn = gated_turn(&mut db, Some(1));
    // The first item's own id is the second call's id.
    let (first, s1) = shell_call("s2", "s1", "true");
    let (second, s2) = shell_call("fc_2", "s2", "ls");
    db.append(turn, vec![first, second], &[s1.clone(), s2], None)
        .unwrap();
    let listed = db.approvals(Some("Bob"), None, 0, None, 64).unwrap();
    let listed = listed["approvals"].as_array().unwrap();
    let [first, second] = &listed[..] else {
        panic!("{listed:?}")
    };
    assert_eq!(second["call_id"], "s2");
    assert_eq!(second["arguments"], json!({"command":"ls"}));
    assert_eq!(second["arguments_cut"], json!([]));
    assert_eq!(second["arguments_omitted"], 0);
    assert_eq!(
        second["node"].as_i64(),
        first["node"].as_i64().map(|n| n + 1)
    );
    // An answer after the gate's expiry is refused, even before the turn
    // gets to deny the call for it.
    std::thread::sleep(std::time::Duration::from_millis(5));
    let late = allow(&mut db, turn, "s1");
    assert_eq!(
        late.err().map(|e| e.to_string()).as_deref(),
        Some("approval_expired")
    );
    let now_ms = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_millis() as u64;
    assert!(matches!(
        db.approval_start(turn, &s1, now_ms).unwrap(),
        Gated::Expired
    ));
}

/// Create `name` with one gate on `shell`, delegated by `creator`.
fn gated_bot(
    db: &mut Database,
    name: &str,
    tag: &str,
    expire_ms: Option<u64>,
    creator: Option<&Bot>,
) -> Result<Bot> {
    let tools = ["shell".to_owned()];
    let gate = Gate {
        tag: tag.into(),
        tools: vec!["shell".into()],
        expire_ms,
    };
    let binding = Binding {
        tools: &tools,
        gate: Some(&gate),
        created_by: creator.map(|c| c.name.as_str()),
        created_by_id: creator.map(|c| c.bot_id),
        ..binding()
    };
    Ok(db.create(name, Some("/synthetic"), binding)?.0)
}

#[test]
fn gates_are_capped_before_a_bot_is_written() {
    let mut db = db();
    let mut creator = gated_bot(&mut db, "g0", "t0", None, None).unwrap();
    for i in 1..8 {
        creator = gated_bot(
            &mut db,
            &format!("g{i}"),
            &format!("t{i}"),
            None,
            Some(&creator),
        )
        .unwrap();
    }
    assert_eq!(creator.gates.len(), 8);
    let refused = gated_bot(&mut db, "g8", "t8", None, Some(&creator));
    assert_eq!(
        refused.err().map(|e| e.to_string()).as_deref(),
        Some("gate_limit: 8")
    );
    // Nothing was written: the name is still free.
    gated_bot(&mut db, "g8", "t8", None, None).unwrap();
}

/// A bot that a sandboxed bot makes takes its sandbox unless it names a
/// stricter one, as Codex's subagents inherit their parent's.
#[test]
fn a_bot_made_by_a_sandboxed_bot_is_no_less_sandboxed() {
    use agent_runtime::sandbox::Mode;
    let mut db = db();
    let ann = db
        .create(
            "Ann",
            Some("/work"),
            Binding {
                settings: Settings {
                    sandbox: Some(Mode::WorkspaceWrite),
                    writable: vec!["/shared".into()],
                    ..Settings::default()
                },
                ..binding()
            },
        )
        .unwrap()
        .0;
    let made = |db: &mut Database, name: &str, settings: Settings| {
        db.create(
            name,
            Some("/elsewhere"),
            Binding {
                settings,
                created_by: Some(ann.name.as_str()),
                created_by_id: Some(ann.bot_id),
                ..binding()
            },
        )
        .unwrap()
        .0
        .settings
    };
    let unconfined = made(&mut db, "Bob", Settings::default());
    assert_eq!(unconfined.sandbox, Some(Mode::WorkspaceWrite));
    assert_eq!(unconfined.writable, ["/shared"]);
    let full = made(
        &mut db,
        "Cid",
        Settings {
            sandbox: Some(Mode::DangerFullAccess),
            ..Settings::default()
        },
    );
    assert_eq!(full.sandbox, Some(Mode::WorkspaceWrite));
    let networked = made(
        &mut db,
        "Dee",
        Settings {
            sandbox: Some(Mode::WorkspaceWrite),
            writable: vec!["/other".into()],
            network: true,
            ..Settings::default()
        },
    );
    assert!(!networked.network);
    assert_eq!(networked.writable, ["/other"]);
    let read_only = made(
        &mut db,
        "Eve",
        Settings {
            sandbox: Some(Mode::ReadOnly),
            ..Settings::default()
        },
    );
    assert_eq!(read_only.sandbox, Some(Mode::ReadOnly));
    assert!(read_only.writable.is_empty());
    // An unconfined creator imposes nothing.
    assert_eq!(
        Settings::default().under(&Settings::default()),
        Settings::default()
    );
}

#[test]
fn a_sandboxed_bot_waits_for_a_verdict_only_on_an_escape() {
    let mut db = db();
    let tools = ["shell".to_owned(), "write".to_owned()];
    let gate = Gate {
        tag: "manual".into(),
        tools: tools.to_vec(),
        expire_ms: None,
    };
    let binding = Binding {
        tools: &tools,
        gate: Some(&gate),
        settings: Settings {
            sandbox: Some(agent_runtime::sandbox::Mode::WorkspaceWrite),
            ..Settings::default()
        },
        ..binding()
    };
    db.create("Bob", Some("/synthetic"), binding).unwrap();
    let turn = db
        .begin(
            "Bob",
            "request",
            "work",
            true,
            &TurnOptions::default(),
            allow_provider,
        )
        .unwrap()
        .turn;
    let call = |id: &str, name: &str, arguments: Value| {
        let arguments = arguments.to_string();
        let item = json!({"type":"function_call","id":format!("fc_{id}"),"call_id":id,
            "name":name,"arguments":arguments});
        let call = ToolCall {
            name: name.into(),
            call_id: id.into(),
            arguments,
            custom: false,
        };
        (Bytes::from(serde_json::to_vec(&item).unwrap()), call)
    };
    let round = [
        call("plain", "shell", json!({"command":"make"})),
        call(
            "up",
            "shell",
            json!({"command":"git push","sandbox_permissions":"require_escalated",
            "justification":"Do you want to push the branch?"}),
        ),
        call(
            "inside",
            "write",
            json!({"path":"notes/a.md","content":"x"}),
        ),
        call(
            "climbs",
            "write",
            json!({"path":"../elsewhere/a.md","content":"x"}),
        ),
        call(
            "outside",
            "write",
            json!({"path":"/etc/hosts","content":"x"}),
        ),
    ];
    let (items, calls): (Vec<_>, Vec<_>) = round.into_iter().unzip();
    db.append(turn, items, &calls, None).unwrap();
    let bot = db.inspect("Bob").unwrap();
    let folder = std::path::Path::new("/synthetic");
    let gated: Vec<&str> = calls
        .iter()
        .filter(|call| bot.gated(call, folder))
        .map(|call| call.call_id.as_str())
        .collect();
    assert_eq!(gated, ["up", "climbs", "outside"]);
    // What runs confined was never announced; each escape was.
    for call in &calls {
        let announced = db.approval_start(turn, call, 0);
        match gated.contains(&call.call_id.as_str()) {
            true => assert!(
                matches!(announced, Ok(Gated::Pending { .. })),
                "{}",
                call.call_id
            ),
            false => assert_eq!(announced.err().unwrap().code, "invalid_tool_state"),
        }
    }
}

#[test]
fn an_expiry_that_came_first_beats_a_later_denial() {
    let mut db = db();
    let ann = gated_bot(&mut db, "Ann", "second", None, None).unwrap();
    gated_bot(&mut db, "Bob", "manual", Some(1), Some(&ann)).unwrap();
    let turn = db
        .begin(
            "Bob",
            "request",
            "work",
            true,
            &TurnOptions::default(),
            allow_provider,
        )
        .unwrap()
        .turn;
    let (item, call) = shell_call("fc_1", "s1", "true");
    db.append(turn, vec![item], std::slice::from_ref(&call), None)
        .unwrap();
    std::thread::sleep(std::time::Duration::from_millis(5));
    // The other gate may still deny, but `manual` lapsed first.
    db.answer(Decision {
        bot: "Bob",
        turn,
        call_id: "s1",
        request: 1,
        tag: Some("second"),
        allow: false,
        unreviewed: false,
        reason: Some("no"),
        by: Some("test"),
    })
    .unwrap();
    assert!(matches!(
        db.approval_start(turn, &call, u64::MAX).unwrap(),
        Gated::Expired
    ));
}

#[test]
fn a_deny_decides_the_call_and_later_answers_are_refused() {
    let mut db = db();
    let ann = gated_bot(&mut db, "Ann", "second", None, None).unwrap();
    gated_bot(&mut db, "Bob", "manual", None, Some(&ann)).unwrap();
    let turn = db
        .begin(
            "Bob",
            "request",
            "work",
            true,
            &TurnOptions::default(),
            allow_provider,
        )
        .unwrap()
        .turn;
    let (item, call) = shell_call("fc_1", "s1", "true");
    db.append(turn, vec![item], std::slice::from_ref(&call), None)
        .unwrap();
    let long = "y".repeat(16 * 1024);
    let answer = |db: &mut Database, tag, allow, reason| {
        db.answer(Decision {
            bot: "Bob",
            turn,
            call_id: "s1",
            request: 1,
            tag: Some(tag),
            allow,
            unreviewed: false,
            reason,
            by: Some("test"),
        })
    };
    let denied = answer(&mut db, "manual", false, Some("no")).unwrap();
    assert_eq!(denied.reply["pending"], json!([]));
    // Neither verdict nor reason is kept once the call is denied.
    for (allow, reason) in [(true, None), (false, Some(long.as_str()))] {
        let late = answer(&mut db, "second", allow, reason);
        let late = late.err().unwrap();
        assert_eq!(late.code, "approval_already_answered");
        assert_eq!(late.facts.unwrap()["decision"], "deny");
    }
    assert!(matches!(
        db.approval_start(turn, &call, 0).unwrap(),
        Gated::Denied
    ));
}

/// A running turn of Bob, whose `shell` calls wait on a gate that never
/// lapses and whose `write` calls on one that lapses 50 ms after the round
/// is announced, planning `calls` (tool, call id) in one round.
fn lapsing_round(db: &mut Database, calls: &[(&str, &str)]) -> (i64, Vec<ToolCall>) {
    let tools = ["shell".to_owned(), "write".to_owned(), "read".to_owned()];
    let gate = |tag: &str, tool: &str, expire_ms| Gate {
        tag: tag.into(),
        tools: vec![tool.into()],
        expire_ms,
    };
    let (slow, quick) = (
        gate("slow", "shell", None),
        gate("quick", "write", Some(50)),
    );
    let mut create = |name, gate, creator: Option<&Bot>| {
        let binding = Binding {
            tools: &tools,
            gate: Some(gate),
            created_by: creator.map(|c| c.name.as_str()),
            created_by_id: creator.map(|c| c.bot_id),
            ..binding()
        };
        db.create(name, Some("/synthetic"), binding).unwrap().0
    };
    let ann = create("Ann", &slow, None);
    create("Bob", &quick, Some(&ann));
    let turn = db
        .begin(
            "Bob",
            "request",
            "work",
            true,
            &TurnOptions::default(),
            allow_provider,
        )
        .unwrap()
        .turn;
    let (items, calls): (Vec<Bytes>, Vec<ToolCall>) = calls
        .iter()
        .enumerate()
        .map(|(i, &(name, call_id))| {
            let arguments = match name {
                "shell" => json!({"command":"true"}),
                "write" => json!({"path":"note.txt","content":"x"}),
                _ => json!({"path":"note.txt"}),
            }
            .to_string();
            let item = json!({"type":"function_call","id":format!("fc_{i}"),
                "call_id":call_id,"name":name,"arguments":arguments});
            let call = ToolCall {
                custom: false,
                name: name.into(),
                call_id: call_id.into(),
                arguments,
            };
            (serde_json::to_vec(&item).unwrap().into(), call)
        })
        .unzip();
    db.append(turn, items, &calls, None).unwrap();
    (turn, calls)
}
fn epoch_now() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_millis() as u64
}

#[test]
fn a_turn_parked_on_one_verdict_ends_when_a_later_call_lapses_first() {
    let mut db = db();
    // The first call waits on a gate with no expiry; the second one's lapses.
    let (turn, round) = lapsing_round(&mut db, &[("shell", "s1"), ("write", "w1")]);
    let s1 = &round[0];
    let Gated::Pending {
        expires_ms: Some(lapse),
        ..
    } = db.approval_start(turn, s1, epoch_now()).unwrap()
    else {
        panic!("the first call waits until the second call's gate lapses")
    };
    // Parked on the first call, the turn wakes when the second one lapses.
    assert_eq!(
        db.suspend_approval(turn, &round, epoch_now(), None, None)
            .unwrap(),
        Some(Some(lapse))
    );
    assert!(matches!(
        db.wake("Bob", turn, true).unwrap(),
        Wake::Later(Some(at)) if at == lapse
    ));
    while epoch_now() < lapse {
        std::thread::sleep(std::time::Duration::from_millis(5));
    }
    assert!(matches!(db.wake("Bob", turn, true).unwrap(), Wake::Resume));
    db.resume(turn).unwrap();
    assert!(matches!(
        db.approval_start(turn, s1, epoch_now()).unwrap(),
        Gated::Lapsed
    ));
}

#[test]
fn a_failure_after_a_later_call_lapsed_keeps_its_request() {
    let mut db = db();
    let (turn, round) = lapsing_round(&mut db, &[("read", "r1"), ("write", "w1")]);
    // The ungated call runs past the later call's lapse, then fails.
    db.tool_start(turn, &round[0]).unwrap();
    std::thread::sleep(std::time::Duration::from_millis(60));
    let failed = Outcome {
        failed: true,
        files: Default::default(),
        ..result("{}")
    };
    db.tool_finish(turn, "r1", &failed).unwrap();
    // Announcing it again would restart its clock; its lapse ends the turn.
    let page = db.approvals(Some("Bob"), None, 0, None, 64).unwrap();
    assert_eq!(page["approvals"][0]["request"], 1);
    assert!(matches!(
        db.approval_start(turn, &round[1], epoch_now()).unwrap(),
        Gated::Expired
    ));
}

#[test]
fn every_parked_turns_lapse_is_read_at_once() {
    let mut db = db();
    let (turn, round) = lapsing_round(&mut db, &[("shell", "s1"), ("write", "w1")]);
    let next = db.next_lapse(turn).unwrap().expect("w1 lapses");
    // A running turn's lapse is its task's to watch.
    assert!(db.lapses().unwrap().is_empty());
    db.suspend_approval(turn, &round, epoch_now(), None, None)
        .unwrap();
    assert_eq!(db.lapses().unwrap(), [(turn, next)].into());
    // An allow decides the only call that can lapse.
    db.answer(Decision {
        bot: "Bob",
        turn,
        call_id: "w1",
        request: 1,
        tag: Some("quick"),
        allow: true,
        unreviewed: false,
        reason: None,
        by: Some("test"),
    })
    .unwrap();
    assert_eq!(db.next_lapse(turn).unwrap(), None);
    assert!(db.lapses().unwrap().is_empty());
}

#[test]
fn no_call_starts_once_a_later_call_lapsed() {
    let mut db = db();
    let (turn, round) = lapsing_round(&mut db, &[("read", "r1"), ("shell", "s1"), ("write", "w1")]);
    let (r1, s1) = (&round[0], &round[1]);
    db.answer(Decision {
        bot: "Bob",
        turn,
        call_id: "s1",
        request: 1,
        tag: Some("slow"),
        allow: true,
        unreviewed: false,
        reason: None,
        by: Some("test"),
    })
    .unwrap();
    std::thread::sleep(std::time::Duration::from_millis(60));
    // Neither an ungated call nor an allowed one starts after the lapse.
    let started = db.tool_start(turn, r1);
    assert_eq!(
        started.err().map(|e| e.to_string()).as_deref(),
        Some("approval_expired: a later call's gate lapsed first")
    );
    assert!(matches!(
        db.approval_start(turn, s1, epoch_now()).unwrap(),
        Gated::Lapsed
    ));
}

#[test]
fn a_tag_listing_reads_only_that_tags_calls() {
    let mut db = db();
    let (turn, round) = lapsing_round(
        &mut db,
        &[("shell", "s1"), ("write", "w1"), ("shell", "s2")],
    );
    let listed = |db: &Database, tag| {
        let page = db.approvals(None, Some(tag), 0, None, 64).unwrap();
        page["approvals"]
            .as_array()
            .unwrap()
            .iter()
            .map(|call| call["call_id"].as_str().unwrap().to_owned())
            .collect::<Vec<_>>()
    };
    assert_eq!(listed(&db, "slow"), ["s1", "s2"]);
    assert_eq!(listed(&db, "quick"), ["w1"]);
    assert_eq!(listed(&db, "none"), Vec::<String>::new());
    // A started call leaves the index with its row.
    db.answer(Decision {
        bot: "Bob",
        turn,
        call_id: "s1",
        request: 1,
        tag: Some("slow"),
        allow: true,
        unreviewed: false,
        reason: None,
        by: Some("test"),
    })
    .unwrap();
    assert!(matches!(
        db.approval_start(turn, &round[0], epoch_now()).unwrap(),
        Gated::Started
    ));
    assert_eq!(listed(&db, "slow"), ["s2"]);
}

#[test]
fn a_tag_listing_reads_no_call_that_tag_answered() {
    let mut db = db();
    let tools = ["shell".to_owned()];
    let gate = |tag: &str| Gate {
        tag: tag.into(),
        tools: vec!["shell".into()],
        expire_ms: None,
    };
    let (a, b) = (gate("a"), gate("b"));
    let ann = db
        .create(
            "Ann",
            Some("/synthetic"),
            Binding {
                tools: &tools,
                gate: Some(&a),
                ..binding()
            },
        )
        .unwrap()
        .0;
    db.create(
        "Bob",
        Some("/synthetic"),
        Binding {
            tools: &tools,
            gate: Some(&b),
            created_by: Some(&ann.name),
            created_by_id: Some(ann.bot_id),
            ..binding()
        },
    )
    .unwrap();
    let turn = db
        .begin(
            "Bob",
            "request",
            "work",
            true,
            &TurnOptions::default(),
            allow_provider,
        )
        .unwrap()
        .turn;
    // One more call than a listing page reads.
    let (items, round): (Vec<Bytes>, Vec<ToolCall>) = (0..1025)
        .map(|i| shell_call(&format!("fc_{i}"), &format!("s{i}"), "true"))
        .unzip();
    db.append(turn, items, &round, None).unwrap();
    let answer_all = |db: &mut Database, tag, calls: &[ToolCall]| {
        for call in calls {
            db.answer(Decision {
                bot: "Bob",
                turn,
                call_id: &call.call_id,
                request: 1,
                tag: Some(tag),
                allow: true,
                unreviewed: false,
                reason: None,
                by: Some("test"),
            })
            .unwrap();
        }
    };
    let empty = json!({"approvals":[],"next_after":null});
    let count = |page: &Value| page["approvals"].as_array().unwrap().len();
    // Answers held for a running turn are stepped past unread, each counted
    // toward the page's reads: the page ends there, and the next one goes on.
    answer_all(&mut db, "a", &round[..1024]);
    let first = db.approvals(None, Some("a"), 0, None, 64).unwrap();
    assert_eq!(first["approvals"], json!([]));
    let after = first["next_after"].as_i64().unwrap();
    let rest = db.approvals(None, Some("a"), after, None, 64).unwrap();
    assert_eq!(rest["approvals"][0]["call_id"], "s1024");
    assert_eq!(rest["next_after"], Value::Null);
    // They leave the index when the turn parks.
    assert_eq!(
        db.suspend_approval(turn, &round, epoch_now(), None, None)
            .unwrap(),
        Some(None)
    );
    let listed = db.approvals(None, Some("a"), 0, None, 64).unwrap();
    assert_eq!((count(&listed), &listed["next_after"]), (1, &Value::Null));
    assert_eq!(
        count(&db.approvals(None, Some("b"), 0, None, 64).unwrap()),
        64
    );
    // Answers stored for a parked turn leave it at once.
    answer_all(&mut db, "b", &round);
    assert_eq!(db.approvals(None, Some("b"), 0, None, 64).unwrap(), empty);
}

#[test]
fn a_long_field_hides_no_other_in_a_listing() {
    let mut db = db();
    let turn = gated_turn(&mut db, None);
    // Keys sort, so the long field comes first, as `content` does before
    // `path` in an Anthropic `write`.
    let arguments = json!({"aaa":"x".repeat(5000),"command":"ls"}).to_string();
    let item = json!({"type":"function_call","id":"fc_1","call_id":"s1",
        "name":"shell","arguments":arguments});
    let call = ToolCall {
        custom: false,
        name: "shell".into(),
        call_id: "s1".into(),
        arguments,
    };
    db.append(
        turn,
        vec![serde_json::to_vec(&item).unwrap().into()],
        &[call],
        None,
    )
    .unwrap();
    let page = db.approvals(Some("Bob"), None, 0, None, 64).unwrap();
    let [listed] = &page["approvals"].as_array().unwrap()[..] else {
        panic!("{page}")
    };
    assert_eq!(listed["arguments"]["command"], "ls");
    assert_eq!(
        listed["arguments"]["aaa"].as_str().map(str::len),
        Some(2048)
    );
    assert_eq!(listed["arguments_cut"], json!(["aaa"]));
}

#[test]
fn a_round_of_long_call_ids_is_announced_in_events_that_page() {
    let mut db = db();
    let turn = gated_turn(&mut db, None);
    let planned: Vec<(Bytes, ToolCall)> = (1..=12)
        .map(|n| {
            shell_call(
                &format!("fc_{n}"),
                &format!("{n:02}{}", "c".repeat(60 << 10)),
                "true",
            )
        })
        .collect();
    let (items, calls): (Vec<Bytes>, Vec<ToolCall>) = planned.into_iter().unzip();
    db.append(turn, items, &calls, None).unwrap();
    // Every event pages, and together they announce each call once, in order.
    let (mut announced, mut events, mut after) = (Vec::new(), 0, 0);
    loop {
        let page = db.events("Bob", after, 256).unwrap();
        let page_events = page["events"].as_array().unwrap();
        if page_events.is_empty() {
            break;
        }
        for event in page_events
            .iter()
            .filter(|e| e["event"] == "approval_requested")
        {
            events += 1;
            for call in event["data"]["calls"].as_array().unwrap() {
                announced.push(call["call_id"].as_str().unwrap().to_owned());
            }
        }
        after = page["next_cursor"].as_i64().unwrap();
    }
    assert!(events > 1, "one event would be over the page bound");
    assert_eq!(
        announced,
        calls.iter().map(|c| c.call_id.clone()).collect::<Vec<_>>()
    );
}

#[test]
fn a_call_announced_again_is_found_by_a_listing_already_past_it() {
    let mut db = db();
    let turn = gated_turn(&mut db, None);
    let planned: Vec<(Bytes, ToolCall)> = (1..=4)
        .map(|n| shell_call(&format!("fc_{n}"), &format!("s{n}"), "true"))
        .collect();
    let (items, calls): (Vec<Bytes>, Vec<ToolCall>) = planned.into_iter().unzip();
    db.append(turn, items, &calls, None).unwrap();
    allow(&mut db, turn, "s1").unwrap();
    allow(&mut db, turn, "s2").unwrap();
    // A caller pages past the answered calls to s3.
    let page = db.approvals(Some("Bob"), None, 0, None, 1).unwrap();
    assert_eq!(page["approvals"][0]["call_id"], "s3");
    let after = page["next_after"].as_i64().unwrap();
    // Then s1 fails, and s2 needs a verdict again.
    assert!(matches!(
        db.approval_start(turn, &calls[0], 0).unwrap(),
        Gated::Started
    ));
    let failed = Outcome {
        failed: true,
        files: Default::default(),
        ..result("{}")
    };
    db.tool_finish(turn, "s1", &failed).unwrap();
    let mut seen = Vec::new();
    let mut after = json!(after);
    while let Some(from) = after.as_i64() {
        let page = db.approvals(Some("Bob"), None, from, None, 1).unwrap();
        for call in page["approvals"].as_array().unwrap() {
            seen.push((call["call_id"].clone(), call["request"].clone()));
        }
        after = page["next_after"].clone();
    }
    assert_eq!(
        seen,
        [
            (json!("s2"), json!(2)),
            (json!("s3"), json!(2)),
            (json!("s4"), json!(2))
        ]
    );
}

#[test]
fn a_call_larger_than_a_page_still_fills_one() {
    let mut db = db();
    let turn = gated_turn(&mut db, None);
    let (item, call) = shell_call("fc_1", &"h".repeat(300 * 1024), "true");
    db.append(turn, vec![item], &[call], None).unwrap();
    let page = db.approvals(Some("Bob"), None, 0, None, 64).unwrap();
    assert_eq!(page["approvals"].as_array().unwrap().len(), 1);
    assert!(page["next_after"].is_null());
}

#[test]
fn held_verdicts_follow_the_group_that_holds_them() {
    let mut db = db();
    let turn = gated_turn(&mut db, None);
    let (item, call) = shell_call("fc_1", "s1", "true");
    db.append(turn, vec![item], std::slice::from_ref(&call), None)
        .unwrap();
    // An allow held for a running turn goes with a group that fails to
    // commit: its approver was told the answer failed.
    db.begin_group().unwrap();
    assert!(allow(&mut db, turn, "s1").unwrap().notify.is_some());
    db.abandon_group().unwrap();
    assert!(matches!(
        db.approval_start(turn, &call, 0).unwrap(),
        Gated::Pending { .. }
    ));
    // One that committed survives a later group that consumed it and failed.
    db.begin_group().unwrap();
    allow(&mut db, turn, "s1").unwrap();
    db.commit_group().unwrap();
    db.begin_group().unwrap();
    assert!(matches!(
        db.approval_start(turn, &call, 0).unwrap(),
        Gated::Started
    ));
    db.abandon_group().unwrap();
    assert!(matches!(
        db.approval_start(turn, &call, 0).unwrap(),
        Gated::Started
    ));
}

#[test]
fn the_announced_call_count_follows_the_rows() {
    let path = std::env::temp_dir().join(format!(
        "agent-approval-count-{}.sqlite",
        std::process::id()
    ));
    let _ = std::fs::remove_file(&path);
    let mut db = Database::initialize(Connection::open(&path).unwrap()).unwrap();
    let reader = Connection::open(&path).unwrap();
    let rows = || -> i64 {
        reader
            .query_row("SELECT count(*) FROM approvals", [], |r| r.get(0))
            .unwrap()
    };
    let turn = gated_turn(&mut db, None);
    let (items, round): (Vec<Bytes>, Vec<ToolCall>) = ["s1", "s2", "s3"]
        .iter()
        .enumerate()
        .map(|(i, id)| shell_call(&format!("fc_{i}"), id, "true"))
        .unzip();
    db.append(turn, items, &round, None).unwrap();
    assert_eq!((db.approval_requests(), rows()), (3, 3));
    allow(&mut db, turn, "s1").unwrap();
    assert!(matches!(
        db.approval_start(turn, &round[0], 0).unwrap(),
        Gated::Started
    ));
    assert_eq!((db.approval_requests(), rows()), (2, 2));
    // A group that fails to commit takes its start back, and the count.
    db.begin_group().unwrap();
    allow(&mut db, turn, "s2").unwrap();
    assert!(matches!(
        db.approval_start(turn, &round[1], 0).unwrap(),
        Gated::Started
    ));
    assert_eq!(db.approval_requests(), 1);
    db.abandon_group().unwrap();
    assert_eq!((db.approval_requests(), rows()), (2, 2));
    // A denial removes its call; the rest of the round, announced again,
    // keeps its count.
    db.answer(Decision {
        bot: "Bob",
        turn,
        call_id: "s2",
        request: 1,
        tag: None,
        allow: false,
        unreviewed: false,
        reason: Some("no"),
        by: Some("test"),
    })
    .unwrap();
    assert!(matches!(
        db.approval_start(turn, &round[1], 0).unwrap(),
        Gated::Denied
    ));
    assert_eq!((db.approval_requests(), rows()), (1, 1));
    // A parked turn's call is counted again when the store opens.
    db.suspend_approval(turn, &round[2..], epoch_now(), None, None)
        .unwrap();
    drop(db);
    let mut db = Database::initialize(Connection::open(&path).unwrap()).unwrap();
    assert_eq!((db.approval_requests(), rows()), (1, 1));
    // The turn's end takes the rest.
    db.resume(turn).unwrap();
    db.finish(turn, Some(&Error::new("cancelled"))).unwrap();
    assert_eq!((db.approval_requests(), rows()), (0, 0));
    drop((db, reader));
    let _ = std::fs::remove_file(&path);
}

/// What a serving approver would get: each approval publication since the
/// watermark.
fn served_since(db: &mut Database, watermark: &mut i64) -> Vec<Value> {
    let mut served = Vec::new();
    db.publish_since(watermark, |p| {
        if let agent_runtime::store::Publication::Approval(round) = p {
            for tag in &round.tags {
                served.extend(round.messages(tag).unwrap());
            }
        }
        true
    })
    .unwrap();
    served
}

#[test]
fn served_announcements_carry_each_call_and_the_denials_so_far() {
    let mut db = db();
    let mut watermark = 0;
    let turn = gated_turn(&mut db, None);
    let (items, round): (Vec<Bytes>, Vec<ToolCall>) = [("s1", "rm -rf build"), ("s2", "ls")]
        .iter()
        .enumerate()
        .map(|(i, (id, command))| shell_call(&format!("fc_{i}"), id, command))
        .unzip();
    // The plan commit publishes its announcement to serving approvers
    // after its events, under the announcing event's cursor.
    let entries = db.append(turn, items, &round, None).unwrap();
    let announced = entries
        .iter()
        .find(|e| e["event"] == "approval_requested")
        .unwrap();
    let served = served_since(&mut db, &mut watermark);
    let [message] = &served[..] else {
        panic!("{served:?}")
    };
    assert_eq!(message["cursor"], announced["cursor"]);
    assert_eq!(
        (&message["bot"], &message["turn"], &message["tag"]),
        (&json!("Bob"), &json!(turn), &json!("manual"))
    );
    assert_eq!(
        (&message["data"]["part"], &message["data"]["parts"]),
        (&json!(1), &json!(1))
    );
    let calls = message["data"]["calls"].as_array().unwrap();
    assert_eq!(calls.len(), 2);
    assert_eq!(calls[0]["arguments"], json!({"command":"rm -rf build"}));
    assert_eq!(calls[0]["gates"], json!(["manual"]));
    assert_eq!(
        calls[0]["denials"],
        json!({"manual":{"in_row":0,"recent":0}})
    );
    // Each call is as `approvals` lists it.
    let listed = db.approvals(None, Some("manual"), 0, None, 64).unwrap();
    assert_eq!(listed["approvals"].as_array().unwrap(), calls);
    // A denial counts, and the rest of the round is announced again with
    // the count it made.
    db.answer(Decision {
        bot: "Bob",
        turn,
        call_id: "s1",
        request: 1,
        tag: None,
        allow: false,
        unreviewed: false,
        reason: Some("not that"),
        by: Some("test"),
    })
    .unwrap();
    assert!(matches!(
        db.approval_start(turn, &round[0], 0).unwrap(),
        Gated::Denied
    ));
    let served = served_since(&mut db, &mut watermark);
    let [again] = &served[..] else {
        panic!("{served:?}")
    };
    assert_eq!(again["data"]["failed"], "s1");
    let calls = again["data"]["calls"].as_array().unwrap();
    assert_eq!(
        (&calls[0]["call_id"], &calls[0]["request"]),
        (&json!("s2"), &json!(2))
    );
    assert_eq!(
        calls[0]["denials"],
        json!({"manual":{"in_row":1,"recent":1}})
    );
    // A call every gate allowed ends the run of denials; the turn's count
    // stays, and a later turn starts from none.
    db.answer(Decision {
        bot: "Bob",
        turn,
        call_id: "s2",
        request: 2,
        tag: None,
        allow: true,
        unreviewed: false,
        reason: None,
        by: Some("test"),
    })
    .unwrap();
    assert!(matches!(
        db.approval_start(turn, &round[1], 0).unwrap(),
        Gated::Started
    ));
    let denials = db.inspect("Bob").unwrap().denials;
    assert_eq!(
        (
            denials["manual"].in_row,
            denials["manual"].window.count_ones(),
            denials["manual"].turn
        ),
        (0, 1, turn)
    );
    assert!(served_since(&mut db, &mut watermark).is_empty());
    // A group that does not commit publishes no announcement.
    db.finish(turn, None).unwrap();
    let next = db
        .begin(
            "Bob",
            "again",
            "more",
            true,
            &TurnOptions::default(),
            allow_provider,
        )
        .unwrap()
        .turn;
    served_since(&mut db, &mut watermark);
    db.begin_group().unwrap();
    let (item, call) = shell_call("fc_9", "s9", "ls");
    db.append(next, vec![item], std::slice::from_ref(&call), None)
        .unwrap();
    db.abandon_group().unwrap();
    assert!(served_since(&mut db, &mut watermark).is_empty());
    // Serving starts after the newest event: the listing holds what came
    // before it, and delivery what comes after.
    db.append(
        next,
        vec![shell_call("fc_8", "s8", "ls").0],
        &[shell_call("fc_8", "s8", "ls").1],
        None,
    )
    .unwrap();
    let (cursor, page) = db.serve_approvals("manual", 64).unwrap();
    assert_eq!(cursor, db.last_event_id().unwrap());
    let listed = page["approvals"].as_array().unwrap();
    assert_eq!(listed.len(), 1);
    assert_eq!(
        listed[0]["denials"],
        json!({"manual":{"in_row":0,"recent":0}}),
        "a denial in an earlier turn is none in this one"
    );
}

#[test]
fn a_gate_hands_a_call_to_a_person_who_then_decides_it() {
    let mut db = db();
    let turn = gated_turn(&mut db, None);
    let (item, call) = shell_call("fc_0", "s0", "git push");
    db.append(turn, vec![item], std::slice::from_ref(&call), None)
        .unwrap();
    let decision = |reason| Decision {
        bot: "Bob",
        turn,
        call_id: "s0",
        request: 1,
        tag: None,
        allow: false,
        unreviewed: false,
        reason,
        by: Some("approver"),
    };
    let asked = db.ask(decision(Some("judged risky: it pushes"))).unwrap();
    assert_eq!(
        (&asked.reply["decision"], &asked.reply["pending"]),
        (&json!("ask"), &json!(["manual"]))
    );
    assert!(!asked.resume && asked.notify.is_none());
    // The call still waits, with what was doubted, and followers hear it.
    let listed = db.approvals(Some("Bob"), None, 0, None, 64).unwrap();
    let [waiting] = &listed["approvals"].as_array().unwrap()[..] else {
        panic!("{listed}")
    };
    assert_eq!(waiting["asked"][0]["tag"], "manual");
    assert_eq!(waiting["asked"][0]["reason"], "judged risky: it pushes");
    assert_eq!(waiting["asked"][0]["by"], "approver");
    let events = db.events("Bob", 0, 256).unwrap();
    let told = events["events"]
        .as_array()
        .unwrap()
        .iter()
        .find(|e| e["event"] == "approval_asked")
        .unwrap();
    assert_eq!(
        (&told["data"]["call_id"], &told["data"]["reason"]),
        (&json!("s0"), &json!("judged risky: it pushes"))
    );
    // A person's answer decides it.
    allow(&mut db, turn, "s0").unwrap();
    assert!(matches!(
        db.approval_start(turn, &call, 0).unwrap(),
        Gated::Started
    ));
    // A gate that lapses has nobody to wait on: it cannot ask.
    let mut db = self::db();
    let turn = gated_turn(&mut db, Some(60_000));
    let (item, call) = shell_call("fc_1", "s0", "git push");
    db.append(turn, vec![item], std::slice::from_ref(&call), None)
        .unwrap();
    let refused = db
        .ask(Decision {
            turn,
            ..decision(None)
        })
        .err()
        .unwrap();
    assert_eq!(refused.code, "approval_ask_expiring");
    assert!(
        db.approvals(Some("Bob"), None, 0, None, 64).unwrap()["approvals"][0]
            .get("asked")
            .is_none()
    );
}

#[test]
fn each_gate_keeps_its_own_handoff_to_a_person() {
    let mut db = db();
    let tools = ["shell".to_owned()];
    let gate = |tag: &str| Gate {
        tag: tag.into(),
        tools: vec!["shell".into()],
        expire_ms: None,
    };
    let (a, b) = (gate("a"), gate("b"));
    let first = Binding {
        tools: &tools,
        gate: Some(&a),
        ..binding()
    };
    let ann = db.create("Ann", Some("/synthetic"), first).unwrap().0;
    let made = Binding {
        tools: &tools,
        gate: Some(&b),
        created_by: Some(ann.name.as_str()),
        created_by_id: Some(ann.bot_id),
        ..binding()
    };
    db.create("Bob", Some("/synthetic"), made).unwrap();
    let turn = db
        .begin(
            "Bob",
            "request",
            "work",
            true,
            &TurnOptions::default(),
            allow_provider,
        )
        .unwrap()
        .turn;
    let (item, call) = shell_call("fc_0", "s0", "git push");
    db.append(turn, vec![item], std::slice::from_ref(&call), None)
        .unwrap();
    let decision = |tag, allow| Decision {
        bot: "Bob",
        turn,
        call_id: "s0",
        request: 1,
        tag: Some(tag),
        allow,
        unreviewed: false,
        reason: Some("doubted"),
        by: Some("approver"),
    };
    db.ask(decision("a", false)).unwrap();
    db.ask(decision("b", false)).unwrap();
    let listed = db.approvals(Some("Bob"), None, 0, None, 64).unwrap();
    let tags: Vec<&Value> = listed["approvals"][0]["asked"]
        .as_array()
        .unwrap()
        .iter()
        .map(|asked| &asked["tag"])
        .collect();
    assert_eq!(tags, [&json!("a"), &json!("b")]);
    // The person answers one gate; the other's handoff still shows.
    db.answer(Decision {
        reason: None,
        by: Some("person"),
        ..decision("b", true)
    })
    .unwrap();
    let listed = db.approvals(Some("Bob"), None, 0, None, 64).unwrap();
    assert_eq!(
        listed["approvals"][0]["asked"],
        json!([{"tag":"a","reason":"doubted","by":"approver",
        "at_ms":listed["approvals"][0]["asked"][0]["at_ms"]}]),
        "{listed}"
    );
    db.answer(Decision {
        reason: None,
        by: Some("person"),
        ..decision("a", true)
    })
    .unwrap();
    assert!(matches!(
        db.approval_start(turn, &call, 0).unwrap(),
        Gated::Started
    ));
}

#[test]
fn schema_53_denial_counts_become_the_newest_of_a_window() {
    let path = std::env::temp_dir().join(format!("agent-schema53-{}.sqlite", std::process::id()));
    let _ = std::fs::remove_file(&path);
    let mut db = Database::initialize(Connection::open(&path).unwrap()).unwrap();
    let turn = gated_turn(&mut db, None);
    drop(db);
    let old = json!({"manual":{"in_row":2,"in_turn":3,"turn":turn},
        "auto":{"in_row":60,"in_turn":60,"turn":turn}});
    Connection::open(&path)
        .unwrap()
        .execute(
            "UPDATE bots SET denials=? WHERE name='Bob'",
            [format!("{old}")],
        )
        .unwrap();
    Connection::open(&path)
        .unwrap()
        .execute_batch("PRAGMA user_version=53;")
        .unwrap();
    let db = Database::initialize(Connection::open(&path).unwrap()).unwrap();
    let denials = db.inspect("Bob").unwrap().denials;
    assert_eq!(
        (
            denials["manual"].in_row,
            denials["manual"].window,
            denials["manual"].turn
        ),
        (2, 0b111, turn)
    );
    assert_eq!(
        denials["auto"].window.count_ones(),
        agent_runtime::store::DENIAL_WINDOW
    );
    drop(db);
    std::fs::remove_file(path).unwrap();
}

#[test]
fn schema_49_waiting_calls_were_not_handed_to_anyone() {
    let path = std::env::temp_dir().join(format!("agent-schema49-{}.sqlite", std::process::id()));
    let _ = std::fs::remove_file(&path);
    Database::initialize(Connection::open(&path).unwrap()).unwrap();
    Connection::open(&path)
        .unwrap()
        .execute_batch("ALTER TABLE approvals DROP COLUMN asked; PRAGMA user_version=49;")
        .unwrap();
    let mut db = Database::initialize(Connection::open(&path).unwrap()).unwrap();
    let turn = gated_turn(&mut db, None);
    let (item, call) = shell_call("fc_1", "s0", "git push");
    db.append(turn, vec![item], std::slice::from_ref(&call), None)
        .unwrap();
    db.ask(Decision {
        bot: "Bob",
        turn,
        call_id: "s0",
        request: 1,
        tag: None,
        allow: false,
        unreviewed: false,
        reason: None,
        by: Some("approver"),
    })
    .unwrap();
    let listed = db.approvals(Some("Bob"), None, 0, None, 64).unwrap();
    assert_eq!(listed["approvals"][0]["asked"][0]["tag"], "manual");
    drop(db);
    std::fs::remove_file(path).unwrap();
}

#[test]
fn schema_62_had_no_steps_and_no_paused_calls() {
    let path = std::env::temp_dir().join(format!("agent-schema62-{}.sqlite", std::process::id()));
    let _ = std::fs::remove_file(&path);
    Database::initialize(Connection::open(&path).unwrap()).unwrap();
    Connection::open(&path)
        .unwrap()
        .execute_batch(
            "DROP INDEX approvals_steps; DROP INDEX answers_paused;
             ALTER TABLE approvals DROP COLUMN step_of; ALTER TABLE answers DROP COLUMN paused_ms;
             PRAGMA user_version=62;",
        )
        .unwrap();
    drop(Database::initialize(Connection::open(&path).unwrap()).unwrap());
    let conn = Connection::open(&path).unwrap();
    let columns: i64 = conn
        .query_row(
            "SELECT (SELECT count(*) FROM pragma_table_info('approvals') WHERE name='step_of')
                  + (SELECT count(*) FROM pragma_table_info('answers') WHERE name='paused_ms')",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(columns, 2);
    drop(conn);
    std::fs::remove_file(path).unwrap();
}

#[test]
fn a_new_turn_starts_its_denials_from_none() {
    let mut db = db();
    let deny = |db: &mut Database, turn: i64, n: usize| {
        let (item, call) = shell_call(&format!("fc_{turn}_{n}"), &format!("s{n}"), "rm -rf build");
        db.append(turn, vec![item], std::slice::from_ref(&call), None)
            .unwrap();
        db.answer(Decision {
            bot: "Bob",
            turn,
            call_id: &call.call_id,
            request: 1,
            tag: None,
            allow: false,
            unreviewed: false,
            reason: Some("not that"),
            by: Some("test"),
        })
        .unwrap();
        assert!(matches!(
            db.approval_start(turn, &call, 0).unwrap(),
            Gated::Denied
        ));
    };
    // A turn denied often enough to trip the breaker, then ended.
    let first = gated_turn(&mut db, None);
    for n in 0..4 {
        deny(&mut db, first, n);
    }
    let denials = db.inspect("Bob").unwrap().denials;
    assert_eq!(
        (
            denials["manual"].in_row,
            denials["manual"].window.count_ones()
        ),
        (4, 4)
    );
    db.finish(first, None).unwrap();
    // The next turn's first call is seen with no denials, and a denial in
    // it counts from one, not on from the last turn's run.
    let next = db
        .begin(
            "Bob",
            "again",
            "more",
            true,
            &TurnOptions::default(),
            allow_provider,
        )
        .unwrap()
        .turn;
    let (item, call) = shell_call("fc_next", "n0", "ls");
    db.append(next, vec![item], std::slice::from_ref(&call), None)
        .unwrap();
    let listed = db.approvals(None, Some("manual"), 0, None, 64).unwrap();
    assert_eq!(
        listed["approvals"][0]["denials"],
        json!({"manual":{"in_row":0,"recent":0}})
    );
    allow(&mut db, next, "n0").unwrap();
    assert!(matches!(
        db.approval_start(next, &call, 0).unwrap(),
        Gated::Started
    ));
    deny(&mut db, next, 1);
    let denials = db.inspect("Bob").unwrap().denials;
    assert_eq!(
        (
            denials["manual"].in_row,
            denials["manual"].window.count_ones(),
            denials["manual"].turn
        ),
        (1, 1, next)
    );
}

#[test]
fn an_unreviewed_denial_counts_nothing_and_old_denials_leave_the_window() {
    let mut db = db();
    let turn = gated_turn(&mut db, None);
    let decide = |db: &mut Database, n: usize, allow: bool, unreviewed: bool| {
        let (item, call) = shell_call(&format!("fc_{n}"), &format!("s{n}"), "make");
        db.append(turn, vec![item], std::slice::from_ref(&call), None)
            .unwrap();
        db.answer(Decision {
            bot: "Bob",
            turn,
            call_id: &call.call_id,
            request: 1,
            tag: None,
            allow,
            unreviewed,
            reason: (!allow).then_some("no"),
            by: Some("test"),
        })
        .unwrap();
        db.approval_start(turn, &call, 0).unwrap();
    };
    decide(&mut db, 0, false, false);
    decide(&mut db, 1, false, false);
    // A review that failed is denied without counting.
    decide(&mut db, 2, false, true);
    let denials = db.inspect("Bob").unwrap().denials;
    assert_eq!(
        (
            denials["manual"].in_row,
            denials["manual"].window.count_ones()
        ),
        (2, 2)
    );
    // Allowed calls end the run, and once the window holds no denial the
    // count is gone and allowed calls write nothing.
    for n in 3..3 + agent_runtime::store::DENIAL_WINDOW as usize {
        decide(&mut db, n, true, false);
    }
    let denials = db.inspect("Bob").unwrap().denials;
    assert!(!denials.contains_key("manual"), "{denials:?}");
}

#[test]
fn history_pages_name_the_folder_each_turn_ran_in() {
    let mut db = db();
    db.create("Bob", Some("/synthetic/first"), binding())
        .unwrap();
    let first = db
        .begin(
            "Bob",
            "r1",
            "here",
            true,
            &TurnOptions::default(),
            allow_provider,
        )
        .unwrap()
        .turn;
    db.append(first, vec![assistant("wrote a.md")], &[], None)
        .unwrap();
    db.finish(first, None).unwrap();
    let moved = TurnOptions {
        workspace: Some("/synthetic/second".into()),
        ..TurnOptions::default()
    };
    let second = db
        .begin("Bob", "r2", "there", true, &moved, allow_provider)
        .unwrap()
        .turn;
    db.append(second, vec![assistant("wrote b.md")], &[], None)
        .unwrap();
    db.finish(second, None).unwrap();
    let back = TurnOptions {
        workspace: Some("/synthetic/first".into()),
        ..TurnOptions::default()
    };
    let third = db
        .begin("Bob", "r3", "back", true, &back, allow_provider)
        .unwrap()
        .turn;
    db.append(third, vec![assistant("wrote c.md")], &[], None)
        .unwrap();
    db.finish(third, None).unwrap();
    // Each folder is named once, with the turns that ran in it.
    let page = db.history_nodes("Bob", None, 400, None, false).unwrap();
    assert_eq!(
        page["workspaces"],
        json!([
            {"folder": "/synthetic/first", "turns": [first, third]},
            {"folder": "/synthetic/second", "turns": [second]},
        ])
    );
    // A page holds only the folders of the turns it lists.
    let newest = page["nodes"][0]["node"].as_i64().unwrap();
    let one = db
        .history_nodes("Bob", Some(newest), 1, None, false)
        .unwrap();
    assert_eq!(
        one["workspaces"],
        json!([{"folder": "/synthetic/first", "turns": [third]}])
    );
}

#[test]
fn history_items_name_who_sent_each_prompt_after_its_turns_are_gone() {
    let mut db = db();
    for bot in ["Bob", "Carol"] {
        db.create(bot, Some("/synthetic"), binding()).unwrap();
    }
    let person = db
        .begin(
            "Bob",
            "p1",
            "work",
            true,
            &TurnOptions::default(),
            allow_provider,
        )
        .unwrap()
        .turn;
    let bob = json!({"bot":"Bob","turn":person,"bot_id":1});
    let by_bob = |delivery| TurnOptions {
        delivery,
        from: Some(("Bob".into(), person)),
        ..TurnOptions::default()
    };
    let by_app = |delivery, origin: &str| TurnOptions {
        delivery,
        origin: Some(origin.into()),
        ..TurnOptions::default()
    };
    let started = db
        .begin(
            "Carol",
            "b1",
            "delegated work",
            true,
            &by_bob(Delivery::Reject),
            allow_provider,
        )
        .unwrap();
    let turn = started.turn;
    let accepted = &started.entry.unwrap()["data"];
    assert_eq!(
        (&accepted["from"], &accepted["origin"]),
        (&bob, &Value::Null)
    );
    // Short steers from Bob, from the app and from a person each keep
    // their own node; a queued message from the app starts later.
    db.begin(
        "Carol",
        "b2",
        "also the docs",
        true,
        &by_bob(Delivery::Steer),
        allow_provider,
    )
    .unwrap();
    db.begin(
        "Carol",
        "a1",
        "task update",
        true,
        &by_app(Delivery::Steer, "tasks"),
        allow_provider,
    )
    .unwrap();
    let steer = TurnOptions {
        delivery: Delivery::Steer,
        ..TurnOptions::default()
    };
    db.begin("Carol", "p2", "and the tests", true, &steer, allow_provider)
        .unwrap();
    let absorbed = db
        .absorb(
            turn,
            None,
            8 << 20,
            4096,
            ContextUsage::default(),
            false,
            true,
        )
        .unwrap();
    // Each `steered` event names the steer's turn and who sent its message.
    let taken: Vec<Value> = absorbed
        .entries
        .iter()
        .filter(|e| e["event"] == "steered")
        .map(|e| {
            json!([
                e["data"]["steer"].is_i64(),
                e["data"]["from"],
                e["data"]["origin"]
            ])
        })
        .collect();
    assert_eq!(
        taken,
        [
            json!([true, bob, null]),
            json!([true, null, "tasks"]),
            json!([true, null, null])
        ]
    );
    // Bob's next message waits in line, and Bob is gone before it starts:
    // it still names the identity that sent it. A scheduled one follows.
    let waiting = db
        .begin(
            "Carol",
            "b3",
            "then the changelog",
            true,
            &by_bob(Delivery::Queue),
            allow_provider,
        )
        .unwrap()
        .turn;
    let later = db
        .begin(
            "Carol",
            "s1",
            "nightly",
            true,
            &by_app(Delivery::Queue, "schedule"),
            allow_provider,
        )
        .unwrap()
        .turn;
    db.finish(person, None).unwrap();
    db.delete_bot("Bob").unwrap();
    db.append(turn, vec![assistant("done")], &[], None).unwrap();
    db.finish(turn, None).unwrap();
    let (accepted, _) = db.start(waiting, allow_provider).unwrap();
    assert_eq!(
        (&accepted["data"]["from"], &accepted["data"]["origin"]),
        (&bob, &Value::Null)
    );
    db.append(waiting, vec![assistant("noted")], &[], None)
        .unwrap();
    db.finish(waiting, None).unwrap();
    let (accepted, _) = db.start(later, allow_provider).unwrap();
    assert_eq!(
        (&accepted["data"]["from"], &accepted["data"]["origin"]),
        (&Value::Null, &json!("schedule"))
    );
    db.append(later, vec![assistant("ran")], &[], None).unwrap();
    db.finish(later, None).unwrap();
    let senders = |db: &Database, name: &str| -> Vec<Value> {
        let page = db.history_nodes(name, None, 400, None, false).unwrap();
        let ids: Vec<i64> = page["nodes"]
            .as_array()
            .unwrap()
            .iter()
            .map(|n| n["node"].as_i64().unwrap())
            .collect();
        db.history_items(name, &ids).unwrap()["items"]
            .as_array()
            .unwrap()
            .iter()
            .map(|n| {
                let content = &n["item"]["content"];
                json!([
                    content[0]["text"],
                    content[1]["text"],
                    n["from"],
                    n["origin"]
                ])
            })
            .collect()
    };
    // The model reads who sent each prompt no person wrote, after its words.
    let by = format!("[Written by the agent Bob in turn:Bob/{person}, not by a person.]");
    let expected = [
        json!(["ran", null, null, null]),
        json!(["nightly", "[Sent by schedule.]", null, "schedule"]),
        json!(["noted", null, null, null]),
        json!(["then the changelog", by, bob, null]),
        json!(["done", null, null, null]),
        json!(["and the tests", null, null, null]),
        json!(["task update", "[Sent by tasks.]", null, "tasks"]),
        json!(["also the docs", by, bob, null]),
        json!(["delegated work", by, bob, null]),
    ];
    assert_eq!(senders(&db, "Carol"), expected);
    // A fork shares the nodes; the sender outlives the source's turns and
    // names the identity it had then, not whoever takes the name next.
    db.fork("Carol", "Branch", Fork::default()).unwrap();
    db.delete_bot("Carol").unwrap();
    db.create("Bob", Some("/synthetic"), binding()).unwrap();
    assert_eq!(senders(&db, "Branch"), expected);
}

/// A turn's prompt, or with `reply` the reply it ended with, to quote.
fn quote(bot: &str, turn: i64, reply: bool) -> QuoteRef {
    QuoteRef {
        bot: bot.into(),
        turn,
        reply,
    }
}

#[test]
fn a_quote_copies_a_turns_words_with_who_wrote_them() {
    let mut db = db();
    db.create("Bob", Some("/synthetic"), binding()).unwrap();
    db.create("Carol", Some("/synthetic"), binding()).unwrap();
    // The daemon's own person asks Bob for something, and Bob replies.
    let asked = db
        .begin(
            "Bob",
            "p1",
            "ship it",
            true,
            &TurnOptions::default(),
            allow_provider,
        )
        .unwrap()
        .turn;
    db.append(asked, vec![assistant("shipped")], &[], None)
        .unwrap();
    db.finish(asked, None).unwrap();
    // A person the client names writes to Bob too.
    let by_ana = TurnOptions {
        person: Some("ana".into()),
        ..TurnOptions::default()
    };
    let started = db
        .begin("Bob", "a1", "use tabs", true, &by_ana, allow_provider)
        .unwrap();
    let ana = started.turn;
    let accepted = started.entry.unwrap();
    assert_eq!(accepted["data"]["person"], "ana");
    let node = accepted["data"]["node"].as_i64().unwrap();
    assert_eq!(
        read_item(&db, "Bob", node).unwrap()["content"],
        json!([{"type":"input_text","text":"use tabs"},
            {"type":"input_text","text":"[Written by ana.]"}])
    );
    db.append(ana, vec![assistant("tabs it is")], &[], None)
        .unwrap();
    db.finish(ana, None).unwrap();
    // Bob passes both on to Carol, quoting them rather than retyping.
    let relay = TurnOptions {
        from: Some(("Bob".into(), ana)),
        quotes: vec![
            quote("Bob", asked, false),
            quote("Bob", asked, true),
            quote("Bob", ana, false),
        ],
        ..TurnOptions::default()
    };
    let started = db
        .begin(
            "Carol",
            "r1",
            "the person approved this",
            true,
            &relay,
            allow_provider,
        )
        .unwrap();
    let turn = started.turn;
    let accepted = started.entry.unwrap();
    let node = accepted["data"]["node"].as_i64().unwrap();
    let by_bob = format!("[Written by the agent Bob in turn:Bob/{ana}, not by a person.]");
    assert_eq!(
        read_item(&db, "Carol", node).unwrap()["content"],
        json!([
            {"type":"input_text","text":"the person approved this"},
            {"type":"input_text","text":by_bob},
            {"type":"input_text","text":format!("[Quoted from turn:Bob/{asked}, written by a person:]")},
            {"type":"input_text","text":"ship it"},
            {"type":"input_text","text":format!("[Quoted from the reply of turn:Bob/{asked}, written by the agent Bob:]")},
            {"type":"input_text","text":"shipped"},
            {"type":"input_text","text":format!("[Quoted from turn:Bob/{ana}, written by ana:]")},
            {"type":"input_text","text":"use tabs"},
        ])
    );
    // Events and history name each quote's writer and where its words
    // are in the item, never the words again.
    let named = json!([
        {"bot":"Bob","turn":asked,"at":3},
        {"bot":"Bob","turn":asked,"reply":true,"from":{"bot":"Bob","turn":asked},"at":5},
        {"bot":"Bob","turn":ana,"person":"ana","at":7},
    ]);
    assert_eq!(accepted["data"]["quotes"], named);
    let items = db.history_items("Carol", &[node]).unwrap();
    assert_eq!(items["items"][0]["quotes"], named);
    // A program judging Carol's calls reads each quote's words with its
    // writer, even once Bob and its turns are gone.
    db.delete_bot("Bob").unwrap();
    let read = db.prompts("Carol", turn, 64 * 1024).unwrap();
    let prompt = &read["prompts"][0];
    assert_eq!(prompt["from"], json!({"bot":"Bob","turn":ana}));
    assert_eq!(
        prompt["quotes"],
        json!([
            {"bot":"Bob","turn":asked,"text":"ship it"},
            {"bot":"Bob","turn":asked,"reply":true,"from":{"bot":"Bob","turn":asked},"text":"shipped"},
            {"bot":"Bob","turn":ana,"person":"ana","text":"use tabs"},
        ])
    );
    // Each quote's record counts against the read's bytes: past them the
    // rest are left out, and the prompt reads as cut.
    let words = "the person approved this".len();
    let short = db
        .prompts("Carol", turn, words + 2 * Database::PROMPTS_ENTRY + 4)
        .unwrap();
    let prompt = &short["prompts"][0];
    assert_eq!(prompt["quotes"].as_array().unwrap().len(), 1);
    assert_eq!(
        (&prompt["quotes_more"], &prompt["truncated"]),
        (&json!(true), &json!(true))
    );
}

#[test]
fn a_prompt_s_quotes_read_back_beside_its_picture_each_where_it_is() {
    let mut db = db();
    db.create("Bob", Some("/synthetic"), binding()).unwrap();
    db.create("Carol", Some("/synthetic"), binding()).unwrap();
    // Words with a NUL in them read back whole.
    let words = "yes\u{0}; actually do not deploy";
    let asked = db
        .begin(
            "Bob",
            "p1",
            words,
            true,
            &TurnOptions::default(),
            allow_provider,
        )
        .unwrap()
        .turn;
    db.finish(asked, None).unwrap();
    // The same words quoted twice, beside a picture the read never needs.
    let shown = TurnOptions {
        quotes: vec![quote("Bob", asked, false), quote("Bob", asked, false)],
        images: vec![noise(256)],
        ..TurnOptions::default()
    };
    let turn = db
        .begin("Carol", "c1", "see", true, &shown, allow_provider)
        .unwrap()
        .turn;
    let read = db.prompts("Carol", turn, 64 * 1024).unwrap();
    let texts: Vec<_> = read["prompts"][0]["quotes"]
        .as_array()
        .unwrap()
        .iter()
        .map(|quote| quote["text"].clone())
        .collect();
    assert_eq!(texts, [json!(words), json!(words)]);
    // Cut by the read's bytes, they say so.
    let short = db
        .prompts("Carol", turn, "see".len() + Database::PROMPTS_ENTRY + 5)
        .unwrap();
    let quote = &short["prompts"][0]["quotes"][0];
    assert_eq!(
        (&quote["text"], &quote["truncated"]),
        (&json!("yes\u{0};"), &json!(true))
    );
}

#[test]
fn a_quoted_prompt_brings_its_own_quotes_with_their_first_writers() {
    let mut db = db();
    for name in ["Bob", "Carol", "Dave"] {
        db.create(name, Some("/synthetic"), binding()).unwrap();
    }
    let asked = db
        .begin(
            "Bob",
            "p1",
            "approved",
            true,
            &TurnOptions::default(),
            allow_provider,
        )
        .unwrap()
        .turn;
    db.finish(asked, None).unwrap();
    // Carol's message waits in line, so its quote is copied before its
    // item exists, and moves into the item when it starts.
    let busy = db
        .begin(
            "Carol",
            "c0",
            "busy",
            true,
            &TurnOptions::default(),
            allow_provider,
        )
        .unwrap()
        .turn;
    let relay = TurnOptions {
        origin: Some("lookout".into()),
        quotes: vec![quote("Bob", asked, false)],
        delivery: Delivery::Queue,
        ..TurnOptions::default()
    };
    let queued = db
        .begin(
            "Carol",
            "c1",
            "from your tasks",
            true,
            &relay,
            allow_provider,
        )
        .unwrap();
    assert_eq!(
        queued.entry.unwrap()["data"]["quotes"],
        json!([{"bot":"Bob","turn":asked}])
    );
    db.finish(busy, None).unwrap();
    let (accepted, _) = db.start(queued.turn, allow_provider).unwrap();
    assert_eq!(
        accepted["data"]["quotes"],
        json!([{"bot":"Bob","turn":asked,"at":3}])
    );
    let carol = queued.turn;
    let pass_on = TurnOptions {
        quotes: vec![quote("Carol", carol, false)],
        ..TurnOptions::default()
    };
    let started = db
        .begin("Dave", "d1", "see this", true, &pass_on, allow_provider)
        .unwrap();
    let node = started.entry.unwrap()["data"]["node"].as_i64().unwrap();
    assert_eq!(
        read_item(&db, "Dave", node).unwrap()["content"],
        json!([
            {"type":"input_text","text":"see this"},
            {"type":"input_text","text":format!("[Quoted from turn:Carol/{carol}, sent by lookout:]")},
            {"type":"input_text","text":"from your tasks"},
            {"type":"input_text","text":format!("[Quoted from turn:Bob/{asked}, written by a person:]")},
            {"type":"input_text","text":"approved"},
        ])
    );
    // A retry names the turns the request named, not those they brought.
    let again = db
        .begin("Dave", "d1", "see this", true, &pass_on, allow_provider)
        .unwrap();
    assert_eq!(again.turn, started.turn);
    // Quotes they bring count against the prompt's quotes: each is a part
    // of its own.
    let many = TurnOptions {
        quotes: vec![quote("Dave", started.turn, false); 30],
        ..TurnOptions::default()
    };
    let error = db
        .begin("Bob", "p2", "all of it", true, &many, allow_provider)
        .unwrap_err();
    assert_eq!(error.code, "quote_too_large");
}

#[test]
fn a_quote_names_a_turn_that_has_the_words() {
    let mut db = db();
    db.create("Bob", Some("/synthetic"), binding()).unwrap();
    db.create("Carol", Some("/synthetic"), binding()).unwrap();
    let running = db
        .begin(
            "Bob",
            "p1",
            "work",
            true,
            &TurnOptions::default(),
            allow_provider,
        )
        .unwrap()
        .turn;
    let steer = db
        .begin(
            "Bob",
            "p2",
            "and more",
            true,
            &TurnOptions {
                delivery: Delivery::Steer,
                ..TurnOptions::default()
            },
            allow_provider,
        )
        .unwrap()
        .turn;
    let send = |db: &mut Database, id: &str, quotes: Vec<QuoteRef>, prompt: &str| {
        let options = TurnOptions {
            quotes,
            delivery: Delivery::Queue,
            ..TurnOptions::default()
        };
        db.begin("Carol", id, prompt, true, &options, allow_provider)
            .map(|started| started.turn)
            .map_err(|error| error.code)
    };
    assert_eq!(
        send(&mut db, "q1", vec![quote("Bob", running + 99, false)], "x"),
        Err("quote_not_found".into())
    );
    assert_eq!(
        send(&mut db, "q2", vec![quote("Carol", running, false)], "x"),
        Err("quote_not_found".into())
    );
    // A running turn's words are there; the reply it ends with is not yet.
    assert!(send(&mut db, "q3", vec![quote("Bob", running, false)], "x").is_ok());
    assert_eq!(
        send(&mut db, "q4", vec![quote("Bob", running, true)], "x"),
        Err("quote_unfinished".into())
    );
    db.absorb(
        running,
        None,
        8 << 20,
        4096,
        ContextUsage::default(),
        false,
        true,
    )
    .unwrap();
    db.finish(running, None).unwrap();
    // A steer replied in the turn it joined; a turn that said nothing
    // has no reply.
    assert_eq!(
        send(&mut db, "q5", vec![quote("Bob", steer, true)], "x"),
        Err("quote_not_found".into())
    );
    assert_eq!(
        send(&mut db, "q6", vec![quote("Bob", running, true)], "x"),
        Err("quote_not_found".into())
    );
    assert!(send(&mut db, "q7", vec![quote("Bob", steer, false)], "x").is_ok());
    // The prompt and its quotes are bounded together, as a prompt alone.
    let words = "w".repeat(Database::QUOTED_BYTES - 3);
    assert_eq!(
        send(&mut db, "q8", vec![quote("Bob", running, false)], &words),
        Err("quote_too_large".into())
    );
    // A retry names the same turns, or it is another request.
    let first = send(&mut db, "q9", vec![quote("Bob", running, false)], "x").unwrap();
    assert_eq!(
        send(&mut db, "q9", vec![quote("Bob", running, false)], "x"),
        Ok(first)
    );
    assert_eq!(
        send(&mut db, "q9", vec![quote("Bob", steer, false)], "x"),
        Err("idempotency_conflict".into())
    );
}

#[test]
fn a_bot_that_hides_senders_reads_quoted_words_without_their_lines() {
    let mut db = db();
    db.create("Bob", Some("/synthetic"), binding()).unwrap();
    let mut quiet = binding();
    quiet.settings.hide_senders = true;
    db.create("Carol", Some("/synthetic"), quiet).unwrap();
    let asked = db
        .begin(
            "Bob",
            "p1",
            "ship it",
            true,
            &TurnOptions::default(),
            allow_provider,
        )
        .unwrap()
        .turn;
    let options = TurnOptions {
        person: Some("ana".into()),
        quotes: vec![quote("Bob", asked, false)],
        ..TurnOptions::default()
    };
    let started = db
        .begin("Carol", "c1", "see this", true, &options, allow_provider)
        .unwrap();
    let data = &started.entry.unwrap()["data"];
    assert_eq!(data["quotes"], json!([{"bot":"Bob","turn":asked,"at":1}]));
    let node = data["node"].as_i64().unwrap();
    assert_eq!(
        read_item(&db, "Carol", node).unwrap()["content"],
        json!([{"type":"input_text","text":"see this"},
            {"type":"input_text","text":"ship it"}])
    );
}

#[test]
fn schema_64_had_no_people_and_no_quotes() {
    let path = std::env::temp_dir().join(format!("agent-schema64-{}.sqlite", std::process::id()));
    let _ = std::fs::remove_file(&path);
    Database::initialize(Connection::open(&path).unwrap()).unwrap();
    Connection::open(&path)
        .unwrap()
        .execute_batch(
            "ALTER TABLE turns DROP COLUMN person; ALTER TABLE turns DROP COLUMN quotes;
             ALTER TABLE senders DROP COLUMN person; ALTER TABLE senders DROP COLUMN quotes;
             ALTER TABLE nodes DROP COLUMN written_ms; PRAGMA user_version=64;",
        )
        .unwrap();
    drop(Database::initialize(Connection::open(&path).unwrap()).unwrap());
    let conn = Connection::open(&path).unwrap();
    let columns: i64 = conn
        .query_row(
            "SELECT (SELECT count(*) FROM pragma_table_info('turns') WHERE name IN ('person','quotes'))
                  + (SELECT count(*) FROM pragma_table_info('senders') WHERE name IN ('person','quotes'))
                  + (SELECT count(*) FROM pragma_table_info('nodes') WHERE name='written_ms')",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(columns, 5);
    drop(conn);
    std::fs::remove_file(path).unwrap();
}

/// A turn of Bob's that says "Shall I deploy?" and then streams what says
/// nothing a person reads: reasoning, and content that is not parts.
fn ask_and_finish(db: &mut Database, key: &str) -> (i64, i64) {
    let turn = db
        .begin(
            "Bob",
            key,
            "go",
            true,
            &TurnOptions::default(),
            allow_provider,
        )
        .unwrap()
        .turn;
    let quiet = |item: Value| Bytes::from(serde_json::to_vec(&item).unwrap());
    let entries = db
        .append(
            turn,
            vec![
                assistant("Looking."),
                assistant("Shall I deploy?"),
                quiet(json!({"type":"reasoning","id":"rs_1","summary":[]})),
                quiet(json!({"type":"message","role":"assistant","content":"plain"})),
                assistant(" \n"),
            ],
            &[],
            None,
        )
        .unwrap();
    db.finish(turn, None).unwrap();
    (turn, entries[1]["data"]["node"].as_i64().unwrap())
}

#[test]
fn a_finished_turns_reply_outlives_retention() {
    let mut db = db();
    db.create("Bob", Some("/synthetic"), binding()).unwrap();
    db.create("Carol", Some("/synthetic"), binding()).unwrap();
    let (asked, _) = ask_and_finish(&mut db, "r1");
    let answered = db
        .begin(
            "Bob",
            "r2",
            "yes",
            true,
            &TurnOptions::default(),
            allow_provider,
        )
        .unwrap()
        .turn;
    db.append(answered, vec![assistant("Deployed.")], &[], None)
        .unwrap();
    db.finish(answered, None).unwrap();
    assert!(db.prune("Bob", 1).unwrap()["events"].as_i64().unwrap() > 0);
    // Its outcome is gone, as retention promises.
    assert_eq!(
        db.turn_outcome("Bob", asked).unwrap_err().code,
        "turn_result_pruned"
    );
    // The words a person's "yes" answered are still read with it.
    assert_eq!(
        db.prompts("Bob", answered, 64 * 1024).unwrap()["prompts"][0]["reply"]["text"],
        "Shall I deploy?"
    );
    // And the reply can still be quoted.
    let options = TurnOptions {
        quotes: vec![quote("Bob", asked, true)],
        ..TurnOptions::default()
    };
    let quoting = db
        .begin("Carol", "c1", "see this", true, &options, allow_provider)
        .unwrap()
        .turn;
    assert_eq!(
        db.prompts("Carol", quoting, 64 * 1024).unwrap()["prompts"][0]["quotes"][0]["text"],
        "Shall I deploy?"
    );
}

#[test]
fn schema_69_finds_each_finished_turns_reply() {
    let path = std::env::temp_dir().join(format!(
        "agent-schema69-reply-{}.sqlite",
        std::process::id()
    ));
    let _ = std::fs::remove_file(&path);
    let (asked, node, silent) = {
        let mut db = Database::initialize(Connection::open(&path).unwrap()).unwrap();
        db.create("Bob", Some("/synthetic"), binding()).unwrap();
        let (asked, node) = ask_and_finish(&mut db, "r1");
        let silent = db
            .begin(
                "Bob",
                "r2",
                "hush",
                true,
                &TurnOptions::default(),
                allow_provider,
            )
            .unwrap()
            .turn;
        db.finish(silent, None).unwrap();
        (asked, node, silent)
    };
    Connection::open(&path)
        .unwrap()
        .execute_batch("ALTER TABLE turns DROP COLUMN reply_node; PRAGMA user_version=69;")
        .unwrap();
    drop(Database::initialize(Connection::open(&path).unwrap()).unwrap());
    let conn = Connection::open(&path).unwrap();
    let reply = |turn: i64| -> Option<i64> {
        conn.query_row("SELECT reply_node FROM turns WHERE id=?", [turn], |r| {
            r.get(0)
        })
        .unwrap()
    };
    assert_eq!(reply(asked), Some(node));
    assert_eq!(reply(silent), None);
    drop(conn);
    std::fs::remove_file(path).unwrap();
}

#[test]
fn schema_70_finds_each_attached_message_from_its_event() {
    let path = std::env::temp_dir().join(format!(
        "agent-schema70-attached-{}.sqlite",
        std::process::id()
    ));
    let _ = std::fs::remove_file(&path);
    let nodes: Vec<i64> = {
        let mut db = Database::initialize(Connection::open(&path).unwrap()).unwrap();
        db.create("Bob", Some("/synthetic"), binding()).unwrap();
        for (request, origin) in [("n1", Some("commons")), ("n2", None)] {
            let attach = TurnOptions {
                delivery: Delivery::Attach,
                origin: origin.map(Into::into),
                ..TurnOptions::default()
            };
            db.begin("Bob", request, "fyi", true, &attach, allow_provider)
                .unwrap();
        }
        let turn = db
            .begin(
                "Bob",
                "r1",
                "work",
                true,
                &TurnOptions::default(),
                allow_provider,
            )
            .unwrap()
            .turn;
        let absorbed = db
            .absorb(
                turn,
                None,
                8 << 20,
                4096,
                ContextUsage::default(),
                false,
                true,
            )
            .unwrap();
        (absorbed.entries.iter())
            .filter(|e| e["event"] == "steered")
            .map(|e| e["data"]["node"].as_i64().unwrap())
            .collect()
    };
    assert_eq!(nodes.len(), 2);
    Connection::open(&path)
        .unwrap()
        .execute_batch("ALTER TABLE senders DROP COLUMN attached; PRAGMA user_version=70;")
        .unwrap();
    let db = Database::initialize(Connection::open(&path).unwrap()).unwrap();
    let read = db.history_items("Bob", &nodes).unwrap();
    for item in read["items"].as_array().unwrap() {
        assert_eq!(item["attached"], true, "{item}");
    }
    assert_eq!(read["items"][0]["origin"], "commons");
    drop(db);
    std::fs::remove_file(path).unwrap();
}

#[test]
fn events_keep_when_they_were_written_and_a_time_finds_its_cursor() {
    let path = std::env::temp_dir().join(format!("agent-written-{}.sqlite", std::process::id()));
    let _ = std::fs::remove_file(&path);
    let mut db = Database::initialize(Connection::open(&path).unwrap()).unwrap();
    // An empty store starts a follow from the beginning, whatever the time.
    assert_eq!(db.cursor_since(0).unwrap(), 0);
    assert_eq!(db.cursor_since(i64::MAX).unwrap(), 0);
    let before = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_millis() as i64;
    db.create("Bob", Some("/synthetic"), binding()).unwrap();
    for n in 0..6 {
        let request = format!("r{n}");
        let turn = db
            .begin(
                "Bob",
                &request,
                "hello",
                true,
                &TurnOptions::default(),
                allow_provider,
            )
            .unwrap()
            .turn;
        db.append(turn, vec![assistant("hi")], &[], None).unwrap();
        db.finish(turn, None).unwrap();
    }
    let page = db.events("Bob", 0, 256).unwrap();
    let events = page["events"].as_array().unwrap();
    assert!(events.len() > 12, "{page}");
    for event in events {
        assert!(event["written_ms"].as_i64().unwrap() >= before, "{event}");
    }
    let cursors: Vec<i64> = events
        .iter()
        .map(|e| e["cursor"].as_i64().unwrap())
        .collect();
    drop(db);
    // Times as an older store has them: the first events none, the rest
    // ten ms apart, with a gap in the cursors where a bot was deleted.
    let conn = Connection::open(&path).unwrap();
    conn.execute_batch(&format!(
        "UPDATE events SET written_ms=NULL WHERE id<{old};
         UPDATE events SET written_ms=id*10 WHERE id>={old};
         DELETE FROM events WHERE id IN ({a},{b});",
        old = cursors[3],
        a = cursors[7],
        b = cursors[8],
    ))
    .unwrap();
    drop(conn);
    let db = Database::initialize(Connection::open(&path).unwrap()).unwrap();
    let kept: Vec<(i64, Option<i64>)> = cursors
        .iter()
        .filter(|&&c| c != cursors[7] && c != cursors[8])
        .map(|&c| (c, (c >= cursors[3]).then_some(c * 10)))
        .collect();
    let last = *cursors.last().unwrap();
    for since in (0..=last * 10 + 20).step_by(5) {
        let first = kept
            .iter()
            .find(|(_, written)| written.is_some_and(|w| w >= since))
            .map_or(last, |(cursor, _)| cursor - 1);
        // Any cursor between the event before and the first is the same
        // start: a follow from it replays the same events.
        let got = db.cursor_since(since).unwrap();
        let skipped = |c: &i64| (got.min(first) + 1..=got.max(first)).contains(c);
        assert!(
            !kept.iter().any(|(c, _)| skipped(c)),
            "since {since}: {got}, not {first}"
        );
    }
    drop(db);
    std::fs::remove_file(path).unwrap();
}

#[test]
fn an_event_is_never_written_before_the_one_before_it() {
    let path = std::env::temp_dir().join(format!("agent-clock-{}.sqlite", std::process::id()));
    let _ = std::fs::remove_file(&path);
    let mut db = Database::initialize(Connection::open(&path).unwrap()).unwrap();
    db.create("Bob", Some("/synthetic"), binding()).unwrap();
    drop(db);
    // The last event written an hour ahead, as before the clock went back.
    let ahead = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_millis() as i64
        + 3_600_000;
    Connection::open(&path)
        .unwrap()
        .execute("UPDATE events SET written_ms=?", [ahead])
        .unwrap();
    let mut db = Database::initialize(Connection::open(&path).unwrap()).unwrap();
    db.create("Carol", Some("/synthetic"), binding()).unwrap();
    let page = db.events_after(0, 256).unwrap();
    let times: Vec<i64> = (page["events"].as_array().unwrap().iter())
        .map(|e| e["written_ms"].as_i64().unwrap())
        .collect();
    assert_eq!(times, [ahead, ahead], "{page}");
    assert_eq!(db.cursor_since(ahead).unwrap(), 0);
    drop(db);
    std::fs::remove_file(path).unwrap();
}

#[test]
fn schema_68_events_had_no_times() {
    let path = std::env::temp_dir().join(format!("agent-schema69-{}.sqlite", std::process::id()));
    let _ = std::fs::remove_file(&path);
    let mut db = Database::initialize(Connection::open(&path).unwrap()).unwrap();
    db.create("Bob", Some("/synthetic"), binding()).unwrap();
    drop(db);
    Connection::open(&path)
        .unwrap()
        .execute_batch("ALTER TABLE events DROP COLUMN written_ms; PRAGMA user_version=68;")
        .unwrap();
    let db = Database::initialize(Connection::open(&path).unwrap()).unwrap();
    let page = db.events("Bob", 0, 256).unwrap();
    let created = &page["events"][0];
    assert_eq!(created["event"], "created");
    assert!(created.get("written_ms").is_none(), "{created}");
    // An event with no time counts as older than any time asked for.
    assert_eq!(
        db.cursor_since(0).unwrap(),
        created["cursor"].as_i64().unwrap()
    );
    drop(db);
    std::fs::remove_file(path).unwrap();
}

#[test]
fn a_bot_that_hides_senders_reads_only_the_words() {
    let mut db = db();
    db.create("Bob", Some("/synthetic"), binding()).unwrap();
    let mut quiet = binding();
    quiet.settings.hide_senders = true;
    db.create("Carol", Some("/synthetic"), quiet).unwrap();
    let person = db
        .begin(
            "Bob",
            "p1",
            "work",
            true,
            &TurnOptions::default(),
            allow_provider,
        )
        .unwrap()
        .turn;
    let from_bob = TurnOptions {
        from: Some(("Bob".into(), person)),
        ..TurnOptions::default()
    };
    let started = db
        .begin(
            "Carol",
            "b1",
            "delegated work",
            true,
            &from_bob,
            allow_provider,
        )
        .unwrap();
    let node = started.entry.unwrap()["data"]["node"].as_i64().unwrap();
    let item = read_item(&db, "Carol", node).unwrap();
    assert_eq!(
        item["content"],
        json!([{"type": "input_text", "text": "delegated work"}])
    );
    assert_eq!(
        db.inspect("Carol").unwrap().settings.resolved()["hide_senders"],
        true
    );
}

#[test]
fn a_turn_started_from_the_line_names_its_author_again() {
    let mut db = db();
    let person = gated_turn(&mut db, None);
    db.create("Carol", Some("/synthetic"), binding()).unwrap();
    let by_bob = TurnOptions {
        from: Some(("Bob".into(), person)),
        delivery: Delivery::Queue,
        ..TurnOptions::default()
    };
    // No slot: the turn waits, and its `accepted` comes later from the line.
    let waiting = db
        .begin(
            "Carol",
            "r1",
            "delegated work",
            false,
            &by_bob,
            allow_provider,
        )
        .unwrap();
    assert_eq!(waiting.entry.unwrap()["event"], "queued");
    let (accepted, _) = db.start(waiting.turn, allow_provider).unwrap();
    assert_eq!(accepted["event"], "accepted");
    assert_eq!(
        accepted["data"]["from"],
        json!({"bot":"Bob","turn":person,"bot_id":1})
    );
}

#[test]
fn prompts_name_who_wrote_each_and_what_the_turn_ran() {
    let mut db = db();
    let person = gated_turn(&mut db, None);
    db.create("Carol", Some("/synthetic"), binding()).unwrap();
    // The author is declared, but it has to be a turn of the bot it names.
    let wrong = TurnOptions {
        from: Some(("Carol".into(), person)),
        ..TurnOptions::default()
    };
    let refused = db.begin("Carol", "r0", "x", true, &wrong, allow_provider);
    assert_eq!(refused.unwrap_err().code, "invalid_from");
    let by_bob = TurnOptions {
        from: Some(("Bob".into(), person)),
        ..TurnOptions::default()
    };
    let started = db
        .begin(
            "Carol",
            "r1",
            "delegated work",
            true,
            &by_bob,
            allow_provider,
        )
        .unwrap();
    let turn = started.turn;
    assert_eq!(
        started.entry.unwrap()["data"]["from"],
        json!({"bot":"Bob","turn":person,"bot_id":1})
    );
    // The same request from someone else is not a retry of it.
    let conflict = db.begin(
        "Carol",
        "r1",
        "delegated work",
        true,
        &TurnOptions::default(),
        allow_provider,
    );
    assert_eq!(conflict.unwrap_err().code, "idempotency_conflict");
    // A person steers the delegated turn.
    let steer = TurnOptions {
        delivery: Delivery::Steer,
        ..TurnOptions::default()
    };
    let steered = db
        .begin(
            "Carol",
            "r2",
            "also check the docs",
            true,
            &steer,
            allow_provider,
        )
        .unwrap()
        .turn;
    db.absorb(
        turn,
        None,
        8 << 20,
        4096,
        ContextUsage::default(),
        false,
        true,
    )
    .unwrap();
    // The steer named no folder, so it records the one its turn ran in,
    // which a later move does not rewrite.
    let row = |db: &Database, id: i64| {
        db.turns("Carol", 0, None, 10, false).unwrap()["turns"]
            .as_array()
            .unwrap()
            .iter()
            .find(|t| t["turn"] == id)
            .unwrap()["workspace"]
            .clone()
    };
    let ran_in = row(&db, turn);
    // One call fails, one is still running.
    let (items, round): (Vec<Bytes>, Vec<ToolCall>) = [("c1", "false"), ("c2", "sleep 5")]
        .iter()
        .enumerate()
        .map(|(i, (id, command))| shell_call(&format!("fc_{i}"), id, command))
        .unzip();
    db.append(turn, items, &round, None).unwrap();
    db.tool_start(turn, &round[0]).unwrap();
    let failed = Outcome {
        failed: true,
        files: Default::default(),
        ..result("exit 1")
    };
    let (_, completed) = db.tool_finish(turn, "c1", &failed).unwrap();
    assert_eq!(completed["data"]["failed"], true);
    db.tool_start(turn, &round[1]).unwrap();
    let read = db.prompts("Carol", turn, 64 * 1024).unwrap();
    assert_eq!(
        read["prompts"],
        json!([{"turn":turn,"text":"delegated work","from":{"bot":"Bob","turn":person}},
            {"turn":steered,"text":"also check the docs","steer":true}])
    );
    assert_eq!(
        read["calls"],
        json!([{"call_id":"c1","name":"shell","arguments":"{\"command\":\"false\"}",
                "arguments_truncated":false,"done":true,"failed":true},
            {"call_id":"c2","name":"shell","arguments":"{\"command\":\"sleep 5\"}",
                "arguments_truncated":false,"done":false}])
    );
    assert_eq!(
        (&read["workspace"], &read["earlier"], &read["more"]),
        (&json!("/synthetic"), &json!([]), &json!(false))
    );
    assert_eq!(
        (&read["prompts_more"], &read["calls_more"]),
        (&json!(false), &json!(false))
    );
    // The author's own turn is a person's words.
    let bob = db.prompts("Bob", person, 1024).unwrap();
    assert_eq!(bob["prompts"], json!([{"turn":person,"text":"work"}]));
    // A gated call cut short names the item holding its whole arguments.
    let (item, call) = shell_call("fc_b", "b1", "make deploy");
    db.append(person, vec![item], std::slice::from_ref(&call), None)
        .unwrap();
    allow(&mut db, person, "b1").unwrap();
    assert!(matches!(
        db.approval_start(person, &call, 0).unwrap(),
        Gated::Started
    ));
    let entry = Database::PROMPTS_ENTRY;
    let cut = db.prompts("Bob", person, entry + 10).unwrap();
    let listed = db.approvals(Some("Bob"), None, 0, None, 64).unwrap();
    assert_eq!(cut["calls"][0]["arguments"], "{\"command\"");
    assert_eq!(cut["calls"][0]["arguments_truncated"], true);
    assert!(cut["calls"][0]["node"].is_i64(), "{cut}");
    assert!(listed["approvals"].as_array().unwrap().is_empty());
    // Text past the budget is cut and marked; steers and calls past it are
    // left out.
    let cut = db.prompts("Carol", turn, 5).unwrap();
    assert_eq!(cut["prompts"][0]["text"], "deleg");
    assert_eq!(cut["prompts"][0]["truncated"], true);
    assert_eq!(cut["prompts"].as_array().unwrap().len(), 1);
    assert_eq!(cut["prompts_more"], true);
    assert_eq!(
        (&cut["calls"], &cut["calls_more"]),
        (&json!([]), &json!(true))
    );
    let cut = db.prompts("Carol", turn, 14 + entry + 3).unwrap();
    assert_eq!(cut["prompts"][1]["text"], "als");
    assert_eq!(cut["prompts_more"], false);
    // The calls have a budget of their own, which each call counts
    // against, so words that fill theirs leave the calls and a long turn
    // stays within it.
    let cut = db.prompts("Carol", turn, entry + 3).unwrap();
    assert_eq!(cut["prompts_more"], true);
    assert_eq!(cut["calls"].as_array().unwrap().len(), 1);
    assert_eq!(cut["calls"][0]["arguments"], "{\"c");
    assert_eq!(cut["calls"][0]["arguments_truncated"], true);
    assert_eq!(cut["calls_more"], true);
    // A cut call names the item holding its whole arguments, when gated.
    assert!(cut["calls"][0].get("node").is_none(), "Carol is not gated");
    // A later turn sees the earlier prompts newest first.
    db.tool_finish(turn, "c2", &result("done")).unwrap();
    db.finish(turn, None).unwrap();
    let next = db
        .begin(
            "Carol",
            "r3",
            "next",
            true,
            &TurnOptions::default(),
            allow_provider,
        )
        .unwrap()
        .turn;
    let read = db.prompts("Carol", next, 4 + entry + 3).unwrap();
    assert_eq!(
        read["earlier"],
        json!([{"turn":steered,"text":"als","truncated":true,"steer":true}])
    );
    assert_eq!(read["more"], true);
    let read = db.prompts("Carol", next, 1024).unwrap();
    assert_eq!(read["earlier"].as_array().unwrap().len(), 2);
    assert_eq!(read["more"], false);
    assert_eq!(
        db.prompts("Carol", person, 1024).unwrap_err().code,
        "turn_not_found"
    );
    assert_eq!(
        db.prompts("Nobody", 1, 1024).unwrap_err().code,
        "bot_not_found"
    );
    assert_eq!(
        db.prompts("Carol", next, 0).unwrap_err().code,
        "invalid_limit"
    );
    let moving = TurnOptions {
        workspace: Some("/synthetic/moved".into()),
        delivery: Delivery::Queue,
        ..TurnOptions::default()
    };
    db.begin("Carol", "r-move", "later", true, &moving, allow_provider)
        .unwrap();
    assert_eq!(row(&db, steered), ran_in);
    assert_ne!(ran_in, json!("/synthetic/moved"));
}

#[test]
fn schema_38_reads_every_stored_prompt_as_a_persons() {
    let path = std::env::temp_dir().join(format!("agent-authors-{}.sqlite", std::process::id()));
    let _ = std::fs::remove_file(&path);
    let turn = {
        let mut db = Database::initialize(Connection::open(&path).unwrap()).unwrap();
        gated_turn(&mut db, None)
    };
    Connection::open(&path)
        .unwrap()
        .execute_batch(
            "ALTER TABLE turns DROP COLUMN from_bot; ALTER TABLE turns DROP COLUMN from_turn;
             PRAGMA user_version=37;",
        )
        .unwrap();
    let db = Database::initialize(Connection::open(&path).unwrap()).unwrap();
    assert_eq!(
        db.prompts("Bob", turn, 1024).unwrap()["prompts"],
        json!([{"turn":turn,"text":"work"}])
    );
    drop(db);
    let version: i32 = Connection::open(&path)
        .unwrap()
        .query_row("PRAGMA user_version", [], |r| r.get(0))
        .unwrap();
    assert_eq!(version, Database::SCHEMA);
    std::fs::remove_file(path).unwrap();
}

#[test]
fn schema_40_keeps_each_bot_written_prompts_sender_with_its_node() {
    let path = std::env::temp_dir().join(format!("agent-senders-{}.sqlite", std::process::id()));
    let _ = std::fs::remove_file(&path);
    let (person, turn) = {
        let mut db = Database::initialize(Connection::open(&path).unwrap()).unwrap();
        let person = gated_turn(&mut db, None);
        db.create("Carol", Some("/synthetic"), binding()).unwrap();
        let by_bob = |delivery| TurnOptions {
            delivery,
            from: Some(("Bob".into(), person)),
            ..TurnOptions::default()
        };
        let turn = db
            .begin(
                "Carol",
                "b1",
                "work",
                true,
                &by_bob(Delivery::Reject),
                allow_provider,
            )
            .unwrap()
            .turn;
        for (id, text, options) in [
            ("b2", "also", by_bob(Delivery::Steer)),
            (
                "p1",
                "mine",
                TurnOptions {
                    delivery: Delivery::Steer,
                    ..TurnOptions::default()
                },
            ),
        ] {
            db.begin("Carol", id, text, true, &options, allow_provider)
                .unwrap();
        }
        db.absorb(
            turn,
            None,
            8 << 20,
            4096,
            ContextUsage::default(),
            false,
            true,
        )
        .unwrap();
        (person, turn)
    };
    // Before 41 a `steered` event named the steer's turn as `from`.
    Connection::open(&path)
        .unwrap()
        .execute_batch(
            "DROP TABLE senders; ALTER TABLE turns DROP COLUMN origin;
             ALTER TABLE turns DROP COLUMN from_id;
             UPDATE events SET data=json_object('from',json_extract(data,'$.steer'),
                'node',json_extract(data,'$.node')) WHERE kind='steered';
             UPDATE events SET data=json_remove(data,'$.from.bot_id') WHERE kind IN ('accepted','queued');
             PRAGMA user_version=40;",
        )
        .unwrap();
    let db = Database::initialize(Connection::open(&path).unwrap()).unwrap();
    let page = db.history_nodes("Carol", None, 400, None, false).unwrap();
    let ids: Vec<i64> = page["nodes"]
        .as_array()
        .unwrap()
        .iter()
        .map(|n| n["node"].as_i64().unwrap())
        .collect();
    let read = db.history_items("Carol", &ids).unwrap();
    let sent: Vec<Value> = read["items"]
        .as_array()
        .unwrap()
        .iter()
        .map(|n| json!([n["item"]["content"][0]["text"], n["from"]]))
        .collect();
    let bob = json!({"bot":"Bob","turn":person,"bot_id":1});
    assert_eq!(
        sent,
        [
            json!(["mine", null]),
            json!(["also", bob]),
            json!(["work", bob])
        ]
    );
    // Stored events name the sender as new ones do.
    let events: Vec<Value> = Connection::open(&path)
        .unwrap()
        .prepare("SELECT json_extract(data,'$.from') FROM events WHERE kind IN ('accepted','queued','steered') AND bot='Carol' ORDER BY id")
        .unwrap()
        .query_map([], |r| r.get::<_, Option<String>>(0))
        .unwrap()
        .map(|from| from.unwrap().map_or(Value::Null, |f| serde_json::from_str(&f).unwrap()))
        .collect();
    // accepted, the two steers queued, then each taken in.
    assert_eq!(
        events,
        [
            bob.clone(),
            bob.clone(),
            Value::Null,
            bob.clone(),
            Value::Null
        ]
    );
    // The turn still reads the steers it took in, now named as `steer`.
    let texts: Vec<Value> = db.prompts("Carol", turn, 1024).unwrap()["prompts"]
        .as_array()
        .unwrap()
        .iter()
        .map(|p| p["text"].clone())
        .collect();
    assert_eq!(texts, [json!("work"), json!("also"), json!("mine")]);
    drop(db);
    std::fs::remove_file(path).unwrap();
}

#[test]
fn schema_39_records_the_folder_each_earlier_turn_ran_in() {
    let path = std::env::temp_dir().join(format!("agent-folders-{}.sqlite", std::process::id()));
    let _ = std::fs::remove_file(&path);
    let (turn, steer) = {
        let mut db = Database::initialize(Connection::open(&path).unwrap()).unwrap();
        let turn = gated_turn(&mut db, None);
        db.create("Carol", None, binding()).unwrap();
        db.create("Dan", Some("/synthetic"), binding()).unwrap();
        let active = TurnOptions {
            workspace: Some("/synthetic/active".into()),
            model: Some("openai/active".into()),
            ..TurnOptions::default()
        };
        let first = db
            .begin("Dan", "first", "work", true, &active, allow_provider)
            .unwrap()
            .turn;
        let steering = TurnOptions {
            delivery: Delivery::Steer,
            ..TurnOptions::default()
        };
        let steer = db
            .begin("Dan", "steer", "also", true, &steering, allow_provider)
            .unwrap()
            .turn;
        db.absorb(
            first,
            None,
            8 << 20,
            4096,
            ContextUsage::default(),
            false,
            true,
        )
        .unwrap();
        (turn, steer)
    };
    // Before 39 a turn's folder never moved its bot, a turn that named no
    // folder stored none and read its bot's, and an absorbed steer that
    // named no model stored none either.
    Connection::open(&path)
        .unwrap()
        .execute_batch(&format!(
            "UPDATE bots SET workspace='/synthetic' WHERE name='Dan';
             UPDATE turns SET workspace=NULL WHERE id IN ({turn},{steer});
             UPDATE turns SET model=NULL WHERE id={steer}; PRAGMA user_version=38;"
        ))
        .unwrap();
    let mut db = Database::initialize(Connection::open(&path).unwrap()).unwrap();
    // The steer ran with the folder and model of the turn it joined.
    let listed = db.turns("Dan", 0, None, 10, false).unwrap();
    let row = listed["turns"]
        .as_array()
        .unwrap()
        .iter()
        .find(|t| t["turn"] == steer)
        .unwrap();
    assert_eq!(
        (&row["workspace"], &row["model"]),
        (&json!("/synthetic/active"), &json!("openai/active"))
    );
    let moving = TurnOptions {
        workspace: Some("/synthetic/moved".into()),
        delivery: Delivery::Queue,
        ..TurnOptions::default()
    };
    db.begin("Bob", "r-move", "later", true, &moving, allow_provider)
        .unwrap();
    let listed = db.turns("Bob", 0, None, 10, false).unwrap();
    let row = listed["turns"]
        .as_array()
        .unwrap()
        .iter()
        .find(|t| t["turn"] == turn)
        .unwrap();
    assert_eq!(row["workspace"], "/synthetic");
    drop(db);
    let version: i32 = Connection::open(&path)
        .unwrap()
        .query_row("PRAGMA user_version", [], |r| r.get(0))
        .unwrap();
    assert_eq!(version, Database::SCHEMA);
    std::fs::remove_file(path).unwrap();
}

#[test]
fn schema_40_gives_every_existing_bot_the_defaults_and_completes_parked_turns() {
    let path = std::env::temp_dir().join(format!("agent-settings-{}.sqlite", std::process::id()));
    let _ = std::fs::remove_file(&path);
    let turn = {
        let mut db = Database::initialize(Connection::open(&path).unwrap()).unwrap();
        db.create("Bob", Some("/synthetic"), binding()).unwrap();
        converse(&mut db, "Bob", 1);
        let turn = db
            .begin(
                "Bob",
                "r2",
                "work",
                true,
                &TurnOptions::default(),
                allow_provider,
            )
            .unwrap()
            .turn;
        let (item, w) = call("w");
        db.append(turn, vec![item], std::slice::from_ref(&w), None)
            .unwrap();
        db.tool_start(turn, &w).unwrap();
        db.suspend(
            turn,
            "w",
            &["turn:Nobody/1".into()],
            None,
            false,
            &[],
            None,
            None,
        )
        .unwrap();
        turn
    };
    // An earlier daemon parked it before these fields were recorded.
    Connection::open(&path)
        .unwrap()
        .execute_batch(
            "ALTER TABLE bots DROP COLUMN settings;
             UPDATE turns SET waiting=json_remove(waiting,'$.any','$.paced_since_ms',
                '$.call_attempts','$.call_spent_ms','$.compaction') WHERE waiting IS NOT NULL;
             PRAGMA user_version=39;",
        )
        .unwrap();
    let db = Database::initialize(Connection::open(&path).unwrap()).unwrap();
    assert_eq!(db.inspect("Bob").unwrap().settings, Settings::default());
    let parked = db.waiting_turns().unwrap();
    assert_eq!(parked.len(), 1);
    assert_eq!(
        (
            parked[0].turn,
            parked[0].any,
            parked[0].paced_since_ms,
            parked[0].call_attempts,
            parked[0].call_spent_ms,
            parked[0].compaction
        ),
        (turn, false, None, 0, 0, false)
    );
    drop(db);
    let version: i32 = Connection::open(&path)
        .unwrap()
        .query_row("PRAGMA user_version", [], |r| r.get(0))
        .unwrap();
    assert_eq!(version, Database::SCHEMA);
    std::fs::remove_file(path).unwrap();
}

#[test]
fn schema_41_turns_ran_at_their_bots_effort() {
    let path = std::env::temp_dir().join(format!("agent-effort-{}.sqlite", std::process::id()));
    let _ = std::fs::remove_file(&path);
    {
        let mut db = Database::initialize(Connection::open(&path).unwrap()).unwrap();
        db.create(
            "Bob",
            Some("/synthetic"),
            Binding {
                effort: Some("medium"),
                ..binding()
            },
        )
        .unwrap();
        let running = db
            .begin(
                "Bob",
                "r1",
                "work",
                true,
                &TurnOptions::default(),
                allow_provider,
            )
            .unwrap()
            .turn;
        db.begin(
            "Bob",
            "steer",
            "join",
            true,
            &TurnOptions {
                delivery: Delivery::Steer,
                ..TurnOptions::default()
            },
            allow_provider,
        )
        .unwrap();
        db.absorb(
            running,
            None,
            8 << 20,
            4096,
            ContextUsage::default(),
            false,
            true,
        )
        .unwrap();
        db.finish(running, None).unwrap();
    }
    Connection::open(&path)
        .unwrap()
        .execute_batch("ALTER TABLE turns DROP COLUMN reasoning; ALTER TABLE turns DROP COLUMN inherited_reasoning; DELETE FROM events; PRAGMA user_version=41;")
        .unwrap();
    let db = Database::initialize(Connection::open(&path).unwrap()).unwrap();
    assert_eq!(
        db.turns("Bob", 0, None, 10, false).unwrap()["turns"][0]["effort"],
        "medium"
    );
    assert_eq!(
        db.turns("Bob", 0, None, 10, false).unwrap()["turns"][1]["effort"],
        "medium"
    );
    drop(db);
    let version: i32 = Connection::open(&path)
        .unwrap()
        .query_row("PRAGMA user_version", [], |r| r.get(0))
        .unwrap();
    assert_eq!(version, Database::SCHEMA);
    std::fs::remove_file(path).unwrap();
}

#[test]
fn schema_44_turns_report_no_summary_time_and_count_it_after() {
    let path = std::env::temp_dir().join(format!("agent-summary-ms-{}.sqlite", std::process::id()));
    let _ = std::fs::remove_file(&path);
    let turn = {
        let mut db = Database::initialize(Connection::open(&path).unwrap()).unwrap();
        db.create("Bob", Some("/synthetic"), binding()).unwrap();
        db.begin(
            "Bob",
            "r1",
            "work",
            true,
            &TurnOptions::default(),
            allow_provider,
        )
        .unwrap()
        .turn
    };
    Connection::open(&path)
        .unwrap()
        .execute_batch("ALTER TABLE turns DROP COLUMN summary_ms; PRAGMA user_version=44;")
        .unwrap();
    let mut db = Database::initialize(Connection::open(&path).unwrap()).unwrap();
    assert_eq!(
        db.turns("Bob", 0, None, 10, false).unwrap()["turns"][0]["summary_ms"],
        0
    );
    db.note_pacing(turn, 0, 0, 1200).unwrap();
    db.note_pacing(turn, 0, 0, 300).unwrap();
    assert_eq!(
        db.turns("Bob", 0, None, 10, false).unwrap()["turns"][0]["summary_ms"],
        1500
    );
    drop(db);
    let version: i32 = Connection::open(&path)
        .unwrap()
        .query_row("PRAGMA user_version", [], |r| r.get(0))
        .unwrap();
    assert_eq!(version, Database::SCHEMA);
    std::fs::remove_file(path).unwrap();
}

#[test]
fn schema_42_backfills_absorbed_effort_or_rolls_back_when_history_is_missing() {
    for missing in [None, Some("event"), Some("turn")] {
        let path = std::env::temp_dir().join(format!(
            "agent-effort-backfill-{}-{}.sqlite",
            std::process::id(),
            missing.unwrap_or("complete")
        ));
        let _ = std::fs::remove_file(&path);
        let (running, first_steer, long_steer);
        {
            let mut db = Database::initialize(Connection::open(&path).unwrap()).unwrap();
            db.create(
                "Bob",
                Some("/synthetic"),
                Binding {
                    effort: Some("high"),
                    ..binding()
                },
            )
            .unwrap();
            running = db
                .begin(
                    "Bob",
                    "run",
                    "work",
                    true,
                    &TurnOptions {
                        effort: Some("low".into()),
                        ..TurnOptions::default()
                    },
                    allow_provider,
                )
                .unwrap()
                .turn;
            let steer = TurnOptions {
                delivery: Delivery::Steer,
                ..TurnOptions::default()
            };
            first_steer = db
                .begin("Bob", "short", "join", true, &steer, allow_provider)
                .unwrap()
                .turn;
            long_steer = db
                .begin(
                    "Bob",
                    "long",
                    &"x".repeat(4096),
                    true,
                    &steer,
                    allow_provider,
                )
                .unwrap()
                .turn;
            db.begin(
                "Bob",
                "explicit",
                "join",
                true,
                &TurnOptions {
                    effort: Some("low".into()),
                    ..steer.clone()
                },
                allow_provider,
            )
            .unwrap();
            assert_eq!(
                db.absorb(
                    running,
                    None,
                    8 << 20,
                    4096,
                    ContextUsage::default(),
                    false,
                    true
                )
                .unwrap()
                .outcomes
                .len(),
                3
            );
            db.finish(running, None).unwrap();
            // A source without an override is known too: its bot's default applies.
            let default = db
                .begin(
                    "Bob",
                    "default",
                    "work",
                    true,
                    &TurnOptions::default(),
                    allow_provider,
                )
                .unwrap()
                .turn;
            db.begin("Bob", "default-steer", "join", true, &steer, allow_provider)
                .unwrap();
            db.absorb(
                default,
                None,
                8 << 20,
                4096,
                ContextUsage::default(),
                false,
                true,
            )
            .unwrap();
            db.finish(default, None).unwrap();
        }
        let conn = Connection::open(&path).unwrap();
        conn.execute_batch(
            "ALTER TABLE turns DROP COLUMN inherited_reasoning; PRAGMA user_version=42;",
        )
        .unwrap();
        match missing {
            Some("event") => {
                conn.execute(
                    "DELETE FROM events WHERE turn=? AND kind='turn_finished'",
                    [long_steer],
                )
                .unwrap();
            }
            Some("turn") => {
                conn.execute("DELETE FROM retained_turns WHERE turn=?", [running])
                    .unwrap();
                conn.execute("DELETE FROM turns WHERE id=?", [running])
                    .unwrap();
            }
            _ => {}
        }
        drop(conn);
        let opened = Database::initialize(Connection::open(&path).unwrap());
        if missing.is_some() {
            let error = match opened {
                Ok(_) => panic!("missing history must not silently change effort"),
                Err(e) => e,
            };
            assert_eq!(error.code, "store_migration_effort_unavailable");
            let conn = Connection::open(&path).unwrap();
            assert_eq!(
                conn.query_row("PRAGMA user_version", [], |r| r.get::<_, i32>(0))
                    .unwrap(),
                42
            );
            assert!(!conn.query_row("SELECT EXISTS(SELECT 1 FROM pragma_table_info('turns') WHERE name='inherited_reasoning')", [], |r| r.get::<_, bool>(0)).unwrap());
        } else {
            let db = opened.unwrap();
            let listed = db.turns("Bob", 0, None, 10, false).unwrap();
            let levels: Vec<_> = listed["turns"]
                .as_array()
                .unwrap()
                .iter()
                .map(|t| t["effort"].as_str().unwrap())
                .collect();
            assert_eq!(levels, ["low", "low", "low", "low", "high", "high"]);
            drop(db);
            let mut db = Database::initialize(Connection::open(&path).unwrap()).unwrap();
            let retry = db
                .begin(
                    "Bob",
                    "short",
                    "join",
                    true,
                    &TurnOptions {
                        delivery: Delivery::Steer,
                        ..TurnOptions::default()
                    },
                    allow_provider,
                )
                .unwrap();
            assert_eq!(retry.turn, first_steer);
            assert!(!retry.fresh);
        }
        std::fs::remove_file(path).unwrap();
    }
}

#[test]
fn a_bot_keeps_its_settings_and_a_fork_copies_them() {
    let mut db = db();
    let settings = Settings {
        context_bytes: Some(4096),
        keep_turns: Some(1),
        keep_warm: Some(0),
        cache_ttl: Some(CacheTtl::Hour),
        speed: Some(agent_runtime::provider::Speed::Fast),
        ..Settings::default()
    };
    let (bob, _) = db
        .create(
            "Bob",
            Some("/synthetic"),
            Binding {
                settings: settings.clone(),
                ..binding()
            },
        )
        .unwrap();
    assert_eq!(bob.settings, settings);
    // Clients read every setting as it applies, defaults included.
    assert_eq!(
        serde_json::to_value(&bob).unwrap()["settings"],
        json!({"context_bytes":4096,"context_items":4096,"note_turns":48,"compact_at":75,
            "compact_keep":25,"compact_tokens":null,"keep_turns":1,"max_rounds":null,"approval_hold_ms":2000,"max_output_tokens":null,
            "keep_warm":0,"cache_ttl":"1h","speed":"fast","sandbox":null,"writable":[],"network":false,"outputs":null,"hide_senders":false,"unsandboxed":[],
            "unix_sockets":[]})
    );
    // Retention follows the bot's own setting.
    for n in 1..=3 {
        converse(&mut db, "Bob", n);
    }
    db.retain("Bob", None).unwrap();
    let finished = |db: &Database, bot: &str| {
        db.events(bot, 0, 256).unwrap()["events"]
            .as_array()
            .unwrap()
            .iter()
            .filter(|event| event["event"] == "turn_finished")
            .count()
    };
    assert_eq!(finished(&db, "Bob"), 1);
    // A fork is an exact copy, its settings included.
    let fork = db.fork("Bob", "Fork", Fork::default()).unwrap().0;
    assert_eq!(fork.settings, settings);
    // Back to standard, it holds no speed, as a bot that never chose one.
    use agent_runtime::provider::Speed;
    assert!(db.set_speed("Fork", None, Speed::Standard).unwrap());
    assert_eq!(db.inspect("Fork").unwrap().settings.speed, None);
    assert!(!db.set_speed("Fork", None, Speed::Standard).unwrap());
    assert!(db.set_speed("Fork", None, Speed::Ultrafast).unwrap());
    assert_eq!(
        serde_json::to_value(db.inspect("Fork").unwrap()).unwrap()["settings"]["speed"],
        "ultrafast"
    );
    // A bot that chose nothing follows the defaults and keeps every turn.
    db.create("Plain", Some("/synthetic"), binding()).unwrap();
    for n in 1..=2 {
        converse(&mut db, "Plain", n);
    }
    db.retain("Plain", None).unwrap();
    assert_eq!(finished(&db, "Plain"), 2);
    // A value out of range is refused and creates nothing.
    for settings in [
        Settings {
            context_bytes: Some(1023),
            ..Settings::default()
        },
        Settings {
            context_items: Some(1),
            ..Settings::default()
        },
        Settings {
            note_turns: Some(1025),
            ..Settings::default()
        },
        Settings {
            compact_at: Some(100),
            ..Settings::default()
        },
        Settings {
            compact_keep: Some(80),
            ..Settings::default()
        },
        Settings {
            keep_turns: Some(0),
            ..Settings::default()
        },
        Settings {
            approval_hold_ms: Some(3_600_001),
            ..Settings::default()
        },
        Settings {
            max_output_tokens: Some(0),
            ..Settings::default()
        },
        Settings {
            keep_warm: Some(300),
            ..Settings::default()
        },
    ] {
        let refused = db
            .create(
                "Bad",
                Some("/synthetic"),
                Binding {
                    settings: settings.clone(),
                    ..binding()
                },
            )
            .unwrap_err();
        assert_eq!(refused.code, "invalid_setting", "{settings:?}");
    }
    assert_eq!(db.inspect("Bad").unwrap_err().code, "bot_not_found");
}

#[test]
fn declared_tools_pass_to_created_bots_and_forks_and_a_forks_allow_narrows_them() {
    let mut db = db();
    let react = Declared {
        name: "react".into(),
        description: "React to a message.".into(),
        parameters: json!({"type":"object","properties":{"emoji":{"type":"string"}}}),
        result: "ok".into(),
        inherit: Inherit::All,
        answered_by: None,
        expire_ms: None,
        grammar: None,
        steps: false,
    };
    // The lead's own tool, which a bot it creates does not take.
    let resolve = Declared {
        name: "resolve".into(),
        description: "Resolve a task.".into(),
        parameters: json!({"type":"object"}),
        result: "ok".into(),
        inherit: Inherit::Generations(0),
        answered_by: None,
        expire_ms: None,
        grammar: None,
        steps: false,
    };
    let declared = [react.clone()];
    let leads = [react.clone(), resolve];
    let (lead, _) = db
        .create(
            "Lead",
            Some("/synthetic"),
            Binding {
                declared: Some(&leads),
                ..binding()
            },
        )
        .unwrap();
    assert_eq!(lead.declared, leads);
    assert_eq!(
        serde_json::to_value(&lead).unwrap()["declared"],
        json!([{"name":"react","description":"React to a message.",
            "parameters":{"type":"object","properties":{"emoji":{"type":"string"}}},"result":"ok"},
            {"name":"resolve","description":"Resolve a task.","parameters":{"type":"object"},
            "result":"ok","inherit":false}])
    );
    // A bot the lead creates takes its tools unless its client declares others.
    let child = |db: &mut Database, name: &str, declared: Option<&[Declared]>| {
        let mut b = binding();
        b.created_by = Some("Lead");
        b.created_by_id = Some(lead.bot_id);
        b.declared = declared;
        db.create(name, Some("/synthetic"), b).unwrap().0
    };
    // All but the lead's own.
    assert_eq!(child(&mut db, "Task", None).declared, declared);
    assert!(child(&mut db, "Own", Some(&[])).declared.is_empty());
    // A bot nobody created declares nothing it was not given.
    let plain = db.create("Plain", Some("/synthetic"), binding()).unwrap().0;
    assert!(plain.declared.is_empty() && !plain.may_call("react"));
    // Declaring again says whether anything changed.
    assert!(!db.set_declared("Lead", None, &leads).unwrap());
    assert!(db.set_declared("Plain", None, &declared).unwrap());
    assert!(db.inspect("Plain").unwrap().may_call("react"));
    assert!(db.set_declared("Plain", None, &[]).unwrap());
    assert!(db.inspect("Plain").unwrap().declared.is_empty());
    // A fork keeps them all, and its allowed list narrows them like the rest.
    converse(&mut db, "Lead", 1);
    let fork = db.fork("Lead", "Copy", Fork::default()).unwrap().0;
    assert_eq!(fork.declared, leads);
    assert!(fork.declared("react").is_some());
    let only_read = ["read".to_owned()];
    let narrowed = db
        .fork(
            "Lead",
            "Reader",
            Fork {
                allow: Some(&only_read),
                ..Fork::default()
            },
        )
        .unwrap()
        .0;
    assert!(narrowed.declared("react").is_none() && !narrowed.may_call("react"));
    let only_react = ["react".to_owned()];
    let reacting = db
        .fork(
            "Lead",
            "Reactor",
            Fork {
                allow: Some(&only_react),
                ..Fork::default()
            },
        )
        .unwrap()
        .0;
    assert!(reacting.declared("react").is_some() && !reacting.may_call("read"));
    let missing = ["wave".to_owned()];
    let refused = db
        .fork(
            "Lead",
            "Waver",
            Fork {
                allow: Some(&missing),
                ..Fork::default()
            },
        )
        .unwrap_err();
    assert_eq!(refused.code, "allow_not_in_source");
}

#[test]
fn a_tool_declared_for_some_generations_goes_that_far_down_and_no_further() {
    let mut db = db();
    let start = Declared {
        name: "start".into(),
        description: "Start a task.".into(),
        parameters: json!({"type":"object"}),
        result: "ok".into(),
        inherit: Inherit::Generations(2),
        answered_by: None,
        expire_ms: None,
        grammar: None,
        steps: false,
    };
    let (lead, _) = db
        .create(
            "Lead",
            Some("/synthetic"),
            Binding {
                declared: Some(std::slice::from_ref(&start)),
                ..binding()
            },
        )
        .unwrap();
    assert_eq!(
        serde_json::to_value(&lead).unwrap()["declared"][0]["inherit"],
        2
    );
    let made = |db: &mut Database, name: &str, by: &str, by_id: i64| {
        let mut b = binding();
        b.created_by = Some(by);
        b.created_by_id = Some(by_id);
        db.create(name, Some("/synthetic"), b).unwrap().0
    };
    // Each generation passes it on one fewer, the last as its own.
    let task = made(&mut db, "Task", "Lead", lead.bot_id);
    assert_eq!(task.declared[0].inherit, Inherit::Generations(1));
    let helper = made(&mut db, "Helper", "Task", task.bot_id);
    assert_eq!(helper.declared[0].inherit, Inherit::Generations(0));
    assert_eq!(
        serde_json::to_value(&helper).unwrap()["declared"][0]["inherit"],
        false
    );
    assert!(
        made(&mut db, "Deeper", "Helper", helper.bot_id)
            .declared
            .is_empty()
    );
    // A fork is a copy, not a generation: it keeps what its source has.
    converse(&mut db, "Task", 1);
    let copy = db.fork("Task", "Copy", Fork::default()).unwrap().0;
    assert_eq!(copy.declared[0].inherit, Inherit::Generations(1));
    // On the wire it is true, false or a count, and nothing else.
    let read = |inherit: Value| {
        serde_json::from_value::<Declared>(json!({"name":"t","description":"d",
            "parameters":{"type":"object"},"inherit":inherit}))
        .map(|tool| tool.inherit)
    };
    assert_eq!(read(json!(true)).unwrap(), Inherit::All);
    assert_eq!(read(json!(false)).unwrap(), Inherit::Generations(0));
    assert_eq!(read(json!(0)).unwrap(), Inherit::Generations(0));
    assert_eq!(read(json!(3)).unwrap(), Inherit::Generations(3));
    assert!(read(json!(-1)).is_err() && read(json!("all")).is_err());
}

#[test]
fn waiting_calls_page_past_held_answers_and_an_answer_out_of_time_is_refused() {
    let mut db = db();
    let start = Declared {
        name: "start".into(),
        description: "Start.".into(),
        parameters: json!({"type":"object"}),
        result: "ok".into(),
        answered_by: Some("sonny".into()),
        expire_ms: None,
        grammar: None,
        steps: false,
        inherit: Inherit::All,
    };
    db.create(
        "Lead",
        Some("/synthetic"),
        Binding {
            declared: Some(std::slice::from_ref(&start)),
            ..binding()
        },
    )
    .unwrap();
    let turn = db
        .begin(
            "Lead",
            "request",
            "work",
            true,
            &TurnOptions::default(),
            allow_provider,
        )
        .unwrap()
        .turn;
    let calls: Vec<ToolCall> = (1..=3)
        .map(|n| ToolCall {
            name: "start".into(),
            call_id: format!("c{n}"),
            arguments: "{}".into(),
            custom: false,
        })
        .collect();
    db.append(turn, vec![], &calls, None).unwrap();
    let mut expires = 0;
    for call in &calls {
        expires = db.tool_start(turn, call).unwrap()["data"]["expires_ms"]
            .as_u64()
            .unwrap();
    }
    // The first two are answered and held by the worker: a page of one
    // that reads only them still goes on to the third.
    for id in ["c1", "c2"] {
        db.answer_call("Lead", turn, id, result("started"), 0)
            .unwrap();
    }
    let page = db.tool_calls("sonny", 0, 1).unwrap();
    assert_eq!(page["calls"], json!([]));
    let after = page["next_after"].as_i64().unwrap();
    let page = db.tool_calls("sonny", after, 1).unwrap();
    assert_eq!(page["calls"][0]["call_id"], "c3");
    assert_eq!(page["next_after"], Value::Null);
    // Out of time, an answer is refused, however late the turn learns it.
    let Err(late) = db.answer_call("Lead", turn, "c3", result("late"), expires) else {
        panic!("an answer out of time was taken");
    };
    assert_eq!(
        (late.code.as_str(), late.detail.as_deref()),
        ("no_pending_call", Some("the call ran out of time"))
    );
}

#[test]
fn a_parked_answer_s_pictures_are_stored_once_its_result_is() {
    let path =
        std::env::temp_dir().join(format!("agent-parked-parts-{}.sqlite", std::process::id()));
    let _ = std::fs::remove_file(&path);
    let mut db = Database::initialize(Connection::open(&path).unwrap()).unwrap();
    let start = Declared {
        name: "start".into(),
        description: "Start.".into(),
        parameters: json!({"type":"object"}),
        result: "ok".into(),
        answered_by: Some("sonny".into()),
        expire_ms: None,
        grammar: None,
        steps: false,
        inherit: Inherit::All,
    };
    db.create(
        "Lead",
        Some("/synthetic"),
        Binding {
            declared: Some(std::slice::from_ref(&start)),
            ..binding()
        },
    )
    .unwrap();
    let turn = db
        .begin(
            "Lead",
            "request",
            "work",
            true,
            &TurnOptions::default(),
            allow_provider,
        )
        .unwrap()
        .turn;
    let call = ToolCall {
        name: "start".into(),
        call_id: "c1".into(),
        arguments: "{}".into(),
        custom: false,
    };
    db.append(turn, vec![], std::slice::from_ref(&call), None)
        .unwrap();
    db.tool_start(turn, &call).unwrap();
    assert!(
        db.suspend_answer(turn, "c1", &[], 0, None, None)
            .unwrap()
            .is_some()
    );
    let pictured = Outcome {
        parts: vec![agent_runtime::codec::Part::Image(
            agent_runtime::codec::Image {
                media_type: "image/png".into(),
                data: "A".repeat(4096),
            },
        )],
        ..result("a page")
    };
    db.answer_call("Lead", turn, "c1", pictured.clone(), 0)
        .unwrap();
    let kept = |path: &std::path::Path| -> Option<String> {
        Connection::open(path)
            .unwrap()
            .query_row("SELECT parts FROM answers WHERE call_id='c1'", [], |r| {
                r.get(0)
            })
            .unwrap()
    };
    assert!(kept(&path).is_some());
    db.resume(turn).unwrap();
    let agent_runtime::store::CallAnswer::Given(answer) = db.answer_take(turn, "c1", 0).unwrap()
    else {
        panic!("the parked answer was not taken");
    };
    assert_eq!(answer.parts, pictured.parts);
    db.tool_finish(turn, "c1", &answer).unwrap();
    assert_eq!(kept(&path), None);
    drop(db);
    std::fs::remove_file(path).unwrap();
}

#[test]
fn schema_52_bots_declare_no_tools() {
    let path = std::env::temp_dir().join(format!("agent-declared-{}.sqlite", std::process::id()));
    let _ = std::fs::remove_file(&path);
    {
        let mut db = Database::initialize(Connection::open(&path).unwrap()).unwrap();
        db.create("Bob", Some("/synthetic"), binding()).unwrap();
    }
    Connection::open(&path)
        .unwrap()
        .execute_batch("ALTER TABLE bots DROP COLUMN declared; PRAGMA user_version=52;")
        .unwrap();
    let db = Database::initialize(Connection::open(&path).unwrap()).unwrap();
    assert!(db.inspect("Bob").unwrap().declared.is_empty());
    drop(db);
    let version: i32 = Connection::open(&path)
        .unwrap()
        .query_row("PRAGMA user_version", [], |r| r.get(0))
        .unwrap();
    assert_eq!(version, Database::SCHEMA);
    std::fs::remove_file(path).unwrap();
}

#[test]
fn a_served_listing_pages_only_to_where_serving_began() {
    let mut db = db();
    let turn = gated_turn(&mut db, None);
    let (items, round): (Vec<Bytes>, Vec<ToolCall>) = ["s1", "s2"]
        .iter()
        .enumerate()
        .map(|(i, id)| shell_call(&format!("fc_{i}"), id, "ls"))
        .unzip();
    db.append(turn, items, &round, None).unwrap();
    let (_, page) = db.serve_approvals("manual", 1).unwrap();
    let ids = |page: &Value| -> Vec<String> {
        page["approvals"]
            .as_array()
            .unwrap()
            .iter()
            .map(|a| a["call_id"].as_str().unwrap().to_owned())
            .collect()
    };
    assert_eq!(ids(&page), ["s1"]);
    let (after, through) = (
        page["next_after"].as_i64().unwrap(),
        page["through"].as_i64(),
    );
    // A call planned once serving began is delivered to the holder, so the
    // listing's later pages stop short of it.
    let (item, call) = shell_call("fc_2", "s3", "ls");
    db.append(turn, vec![item], std::slice::from_ref(&call), None)
        .unwrap();
    let rest = db
        .approvals(None, Some("manual"), after, through, 64)
        .unwrap();
    assert_eq!(
        (ids(&rest), &rest["next_after"]),
        (vec!["s2".to_owned()], &Value::Null)
    );
    let unbounded = db.approvals(None, Some("manual"), after, None, 64).unwrap();
    assert_eq!(ids(&unbounded), ["s2", "s3"]);
}

#[test]
fn schema_35_starts_every_bot_with_no_denials() {
    let path = std::env::temp_dir().join(format!("agent-denials-{}.sqlite", std::process::id()));
    let _ = std::fs::remove_file(&path);
    {
        let mut db = Database::initialize(Connection::open(&path).unwrap()).unwrap();
        gated_turn(&mut db, None);
    }
    Connection::open(&path)
        .unwrap()
        .execute_batch("ALTER TABLE bots DROP COLUMN denials; PRAGMA user_version=34;")
        .unwrap();
    let db = Database::initialize(Connection::open(&path).unwrap()).unwrap();
    assert!(db.inspect("Bob").unwrap().denials.is_empty());
    let resumed = serde_json::to_value(db.inspect("Bob").unwrap()).unwrap();
    assert!(
        resumed.get("denials").is_none(),
        "none counted, none reported"
    );
    drop(db);
    let version: i32 = Connection::open(&path)
        .unwrap()
        .query_row("PRAGMA user_version", [], |r| r.get(0))
        .unwrap();
    assert_eq!(version, Database::SCHEMA);
    std::fs::remove_file(path).unwrap();
}

#[test]
fn a_group_waits_for_a_write_lock_another_connection_holds() {
    let path = std::env::temp_dir().join(format!("agent-group-lock-{}.sqlite", std::process::id()));
    let _ = std::fs::remove_file(&path);
    let mut db = Database::initialize(Connection::open(&path).unwrap()).unwrap();
    // The daemon's reader takes the write lock for a moment when it catches
    // the WAL header mid-update; any holder will do here.
    let other = Connection::open(&path).unwrap();
    other.execute_batch("BEGIN IMMEDIATE").unwrap();
    let holder = std::thread::spawn(move || {
        std::thread::sleep(std::time::Duration::from_millis(100));
        other.execute_batch("COMMIT").unwrap();
    });
    // A job that reads before it writes: past its first read, SQLite would
    // answer SQLITE_BUSY at once rather than wait.
    db.begin_group().unwrap();
    db.last_event_id().unwrap();
    db.create("Bob", Some("/synthetic"), binding()).unwrap();
    db.commit_group().unwrap();
    holder.join().unwrap();
    drop(db);
    let _ = std::fs::remove_file(&path);
}

#[test]
fn artifacts_are_scoped_to_the_owning_bot_and_lineage_checks_use_depth() {
    let mut db = db();
    db.create("Bob", Some("/synthetic"), binding()).unwrap();
    let turn = db
        .begin(
            "Bob",
            "request",
            "work",
            true,
            &TurnOptions::default(),
            allow_provider,
        )
        .unwrap()
        .turn;
    let call = ToolCall {
        custom: false,
        name: "shell".into(),
        call_id: "c1".into(),
        arguments: "{}".into(),
    };
    db.append(turn, vec![], std::slice::from_ref(&call), None)
        .unwrap();
    db.tool_start(turn, &call).unwrap();
    let outcome = Outcome {
        output: "preview".into(),
        artifacts: vec![
            ("stdout", b"full output".to_vec()),
            ("stderr", "\"\né🙂".repeat(100_000).into_bytes()),
        ],
        parts: Vec::new(),
        note: None,
        failed: false,
        files: Default::default(),
    };
    let (_, entry) = db.tool_finish(turn, "c1", &outcome).unwrap();
    assert_eq!(entry["data"]["artifacts"][0], "stdout");
    assert_eq!(
        db.artifact_page("Bob", turn, "c1", "stdout", 0, 64)
            .unwrap()["text"],
        "full output"
    );
    assert!(
        db.artifact_page("Bob", turn, "missing", "stdout", 0, 4)
            .is_err()
    );
    db.create("Other", Some("/synthetic"), binding()).unwrap();
    assert!(
        db.artifact_page("Other", turn, "c1", "stdout", 0, 4)
            .is_err()
    );
    assert!(
        db.artifact_page("Bob", turn, "c1", "missing", 0, 4)
            .is_err()
    );
    assert!(
        db.artifact_page("Bob", turn, "c1", "stdout", 12, 4)
            .is_err()
    );
    let mut offset = 0;
    let mut restored = String::new();
    loop {
        let page = db
            .artifact_page("Bob", turn, "c1", "stdout", offset, 4)
            .unwrap();
        restored.push_str(page["text"].as_str().unwrap());
        offset = page["next_offset"].as_u64().unwrap();
        if page["done"] == true {
            break;
        }
    }
    assert_eq!(restored, "full output");
    restored.clear();
    offset = 0;
    loop {
        let page = db
            .artifact_page("Bob", turn, "c1", "stderr", offset, 65535)
            .unwrap();
        assert!(
            agent_runtime::output::encoded_len(&page).unwrap()
                < agent_runtime::output::MAX_EVENT / 2
        );
        restored.push_str(page["text"].as_str().unwrap());
        assert!(page["next_offset"].as_u64().unwrap() > offset);
        offset = page["next_offset"].as_u64().unwrap();
        if page["done"] == true {
            break;
        }
    }
    assert_eq!(restored, "\"\né🙂".repeat(100_000));
    assert!(db.artifact_page("Bob", turn, "c1", "stderr", 3, 4).is_err());
    assert_eq!(
        db.artifact_page("Bob", turn, "c1", "stderr", offset, 4)
            .unwrap()["done"],
        true
    );
    let node = entry["data"]["node"].as_i64().unwrap();
    assert!(db.history_items("Bob", &[node]).is_ok());
    assert!(db.history_items("Other", &[node]).is_err());
    assert!(db.history_items("Bob", &[node + 1000]).is_err());
    // Retention empties the turn: the owner and a fork holding the output
    // node learn that, an unrelated bot and an unanswered call still do not.
    let checkpoint = db.finish(turn, None).unwrap().last().unwrap()["data"]["checkpoint"]
        .as_i64()
        .unwrap();
    db.fork(
        "Bob",
        "Fork",
        Fork {
            checkpoint: Some(checkpoint),
            ..Fork::default()
        },
    )
    .unwrap();
    let later = db
        .begin(
            "Bob",
            "later",
            "more",
            true,
            &TurnOptions::default(),
            allow_provider,
        )
        .unwrap()
        .turn;
    db.finish(later, None).unwrap();
    db.prune("Bob", 1).unwrap();
    for reader in ["Bob", "Fork"] {
        assert_eq!(
            db.artifact_page(reader, turn, "c1", "stdout", 0, 4)
                .unwrap_err()
                .code,
            "artifact_pruned"
        );
        assert_eq!(
            db.artifact_lines(reader, turn, "c1", "stdout", 1, 5)
                .unwrap_err()
                .code,
            "artifact_pruned"
        );
    }
    assert_eq!(
        db.artifact_page("Other", turn, "c1", "stdout", 0, 4)
            .unwrap_err()
            .code,
        "turn_not_found"
    );
    assert_eq!(
        db.artifact_page("Fork", turn, "c9", "stdout", 0, 4)
            .unwrap_err()
            .code,
        "turn_not_found"
    );
    assert_eq!(
        db.artifact_page("Bob", later, "c1", "stdout", 0, 4)
            .unwrap_err()
            .code,
        "artifact_not_found"
    );
}

#[test]
fn a_turn_runs_at_its_own_effort_or_its_bots() {
    let mut db = db();
    db.create(
        "Bob",
        Some("/synthetic"),
        Binding {
            effort: Some("high"),
            ..binding()
        },
    )
    .unwrap();
    let at = |level: &str| TurnOptions {
        effort: Some(level.into()),
        ..TurnOptions::default()
    };
    // Only the levels the bot's family takes, named as at creation.
    let refused = db
        .begin("Bob", "r0", "work", true, &at("max"), allow_provider)
        .unwrap_err();
    assert_eq!(refused.code, "invalid_effort");
    assert_eq!(
        refused.facts.unwrap()["efforts"],
        json!(["low", "medium", "high", "xhigh"])
    );
    let low = db
        .begin("Bob", "r1", "work", true, &at("low"), allow_provider)
        .unwrap();
    assert_eq!(low.entry.unwrap()["data"]["effort"], "low");
    assert_eq!(db.context(low.turn).unwrap().effort.as_deref(), Some("low"));
    // The level is part of the request: a retry must name the same one.
    assert!(
        !db.begin("Bob", "r1", "work", true, &at("low"), allow_provider)
            .unwrap()
            .fresh
    );
    assert_eq!(
        db.begin("Bob", "r1", "work", true, &at("xhigh"), allow_provider)
            .unwrap_err()
            .code,
        "idempotency_conflict"
    );
    let steer = |level: Option<&str>| TurnOptions {
        effort: level.map(Into::into),
        delivery: Delivery::Steer,
        ..TurnOptions::default()
    };
    // A steer at the turn's level, or at none, joins it and records the
    // level it ran at.
    for (id, level) in [("s0", None), ("s0b", Some("low"))] {
        db.begin("Bob", id, "join", true, &steer(level), allow_provider)
            .unwrap();
    }
    let joined = db
        .absorb(
            low.turn,
            None,
            8 << 20,
            4096,
            ContextUsage::default(),
            false,
            true,
        )
        .unwrap();
    assert_eq!(joined.outcomes.len(), 2);
    // A steer at another level waits for a turn at its own.
    db.begin(
        "Bob",
        "s1",
        "same",
        true,
        &steer(Some("xhigh")),
        allow_provider,
    )
    .unwrap();
    db.begin("Bob", "s2", "any", true, &steer(None), allow_provider)
        .unwrap();
    let absorbed = db
        .absorb(
            low.turn,
            None,
            8 << 20,
            4096,
            ContextUsage::default(),
            false,
            true,
        )
        .unwrap();
    assert!(absorbed.outcomes.is_empty());
    db.finish(low.turn, None).unwrap();
    // Work that names none runs at Bob's own.
    let plain = db
        .begin(
            "Bob",
            "r2",
            "work",
            true,
            &TurnOptions {
                delivery: Delivery::Queue,
                ..TurnOptions::default()
            },
            allow_provider,
        )
        .unwrap();
    assert_eq!(plain.entry.unwrap()["data"]["effort"], "high");
    let listed = db.turns("Bob", 0, None, 10, false).unwrap();
    let levels: Vec<&Value> = listed["turns"]
        .as_array()
        .unwrap()
        .iter()
        .map(|t| &t["effort"])
        .collect();
    assert_eq!(levels, ["low", "low", "low", "xhigh", "high", "high"]);
}

#[test]
fn absorbed_effort_does_not_change_the_submitted_request() {
    for size in [4, 4096] {
        for level in [None, Some("low")] {
            let mut db = db();
            db.create(
                "Bob",
                Some("/synthetic"),
                Binding {
                    effort: Some("high"),
                    ..binding()
                },
            )
            .unwrap();
            let running = db
                .begin(
                    "Bob",
                    "run",
                    "work",
                    true,
                    &TurnOptions {
                        effort: Some("low".into()),
                        ..TurnOptions::default()
                    },
                    allow_provider,
                )
                .unwrap()
                .turn;
            let prompt = "x".repeat(size);
            let options = TurnOptions {
                effort: level.map(Into::into),
                delivery: Delivery::Steer,
                expected_turn: Some(running),
                ..TurnOptions::default()
            };
            let steer = db
                .begin("Bob", "steer", &prompt, true, &options, allow_provider)
                .unwrap()
                .turn;
            assert!(
                !db.begin("Bob", "steer", &prompt, true, &options, allow_provider)
                    .unwrap()
                    .fresh
            );
            let absorbed = db
                .absorb(
                    running,
                    None,
                    8 << 20,
                    4096,
                    ContextUsage::default(),
                    false,
                    true,
                )
                .unwrap();
            assert_eq!(absorbed.outcomes.len(), 1);
            db.finish(running, None).unwrap();
            // Both the inline and shared-prompt paths report the effective effort,
            // but retries still compare the original omission or explicit level.
            assert_eq!(
                db.turns("Bob", 0, None, 10, false).unwrap()["turns"][1]["effort"],
                "low"
            );
            let retry = db
                .begin("Bob", "steer", &prompt, true, &options, allow_provider)
                .unwrap();
            assert_eq!(retry.turn, steer);
            assert!(!retry.fresh);
            for changed in [None, Some("low"), Some("high")]
                .into_iter()
                .filter(|v| *v != level)
            {
                let other = TurnOptions {
                    effort: changed.map(Into::into),
                    ..options.clone()
                };
                assert_eq!(
                    db.begin("Bob", "steer", &prompt, true, &other, allow_provider)
                        .unwrap_err()
                        .code,
                    "idempotency_conflict"
                );
            }
        }
    }
}

#[test]
fn the_next_turn_knows_the_effort_its_history_was_sent_at() {
    let mut db = db();
    db.create(
        "Bob",
        Some("/synthetic"),
        Binding {
            effort: Some("high"),
            ..binding()
        },
    )
    .unwrap();
    let mut previous = Vec::new();
    for (n, level) in [(1, Some("low")), (2, None), (3, None)] {
        let options = TurnOptions {
            effort: level.map(Into::into),
            ..TurnOptions::default()
        };
        let turn = db
            .begin(
                "Bob",
                &format!("r{n}"),
                "work",
                true,
                &options,
                allow_provider,
            )
            .unwrap()
            .turn;
        let context = db.context(turn).unwrap();
        previous.push(context.previous_call.map(|(_, effort)| effort));
        db.append(turn, vec![assistant(&format!("a{n}"))], &[], None)
            .unwrap();
        db.finish(turn, None).unwrap();
    }
    // A cache written at one effort is not one written at another.
    assert_eq!(
        previous,
        [None, Some(Some("low".into())), Some(Some("high".into()))]
    );
}

#[test]
fn turn_options_are_recorded_and_part_of_idempotency() {
    let mut db = db();
    db.create("Bob", Some("/synthetic/default"), binding())
        .unwrap();
    let options = TurnOptions {
        workspace: Some("/synthetic/elsewhere".into()),
        model: Some("openai/other-model".into()),
        effort: None,
        delivery: Delivery::Reject,
        expected_turn: None,
        from: None,
        origin: None,
        person: None,
        quotes: Vec::new(),
        budget_tokens: None,
        images: Vec::new(),
    };
    let started = db
        .begin("Bob", "r1", "work", true, &options, allow_provider)
        .unwrap();
    let accepted = started.entry.unwrap();
    assert_eq!(accepted["data"]["workspace"], "/synthetic/elsewhere");
    assert_eq!(accepted["data"]["model"], "openai/other-model");
    let context = db.context(started.turn).unwrap();
    assert_eq!(
        (context.workspace.as_str(), context.model.as_str()),
        ("/synthetic/elsewhere", "openai/other-model")
    );
    assert!(
        !db.begin("Bob", "r1", "work", true, &options, allow_provider)
            .unwrap()
            .fresh
    );
    // A retry that names no folder means wherever the first attempt went.
    let unnamed = TurnOptions {
        workspace: None,
        ..options.clone()
    };
    assert!(
        !db.begin("Bob", "r1", "work", true, &unnamed, allow_provider)
            .unwrap()
            .fresh
    );
    assert_eq!(
        db.begin(
            "Bob",
            "r1",
            "work",
            true,
            &TurnOptions::default(),
            allow_provider
        )
        .unwrap_err()
        .code,
        "idempotency_conflict"
    );
    db.finish(started.turn, None).unwrap();
    let plain = db
        .begin(
            "Bob",
            "r2",
            "work",
            true,
            &TurnOptions::default(),
            allow_provider,
        )
        .unwrap();
    // The earlier turn moved Bob; a turn that names no folder runs where Bob is.
    let context = db.context(plain.turn).unwrap();
    assert_eq!(
        (context.workspace.as_str(), context.model.as_str()),
        ("/synthetic/elsewhere", "openai/synthetic-model")
    );
    // Work queued before a move runs where the bot was; work after it runs
    // where the bot went.
    let queue = TurnOptions {
        delivery: Delivery::Queue,
        ..TurnOptions::default()
    };
    let before = db
        .begin("Bob", "r3", "before", true, &queue, allow_provider)
        .unwrap();
    let moving = TurnOptions {
        workspace: Some("/synthetic/third".into()),
        ..queue.clone()
    };
    let mover = db
        .begin("Bob", "r4", "move", true, &moving, allow_provider)
        .unwrap();
    let after = db
        .begin("Bob", "r5", "after", true, &queue, allow_provider)
        .unwrap();
    // A steer's folder never moves the bot, whether or not the steer runs.
    let steer = TurnOptions {
        workspace: Some("/synthetic/steer".into()),
        delivery: Delivery::Steer,
        ..TurnOptions::default()
    };
    db.begin("Bob", "r6", "aside", true, &steer, allow_provider)
        .unwrap();
    assert_eq!(
        db.inspect("Bob").unwrap().workspace.as_deref(),
        Some("/synthetic/third")
    );
    for (turn, folder) in [
        (plain.turn, None),
        (before.turn, Some("/synthetic/elsewhere")),
        (mover.turn, Some("/synthetic/third")),
        (after.turn, Some("/synthetic/third")),
    ] {
        if let Some(folder) = folder {
            db.start(turn, allow_provider).unwrap();
            assert_eq!(db.context(turn).unwrap().workspace, folder);
        }
        db.finish(turn, None).unwrap();
    }
}

#[test]
fn a_bot_without_a_default_workspace_needs_one_per_submission() {
    let mut db = db();
    let (bot, _) = db.create("Nomad", None, binding()).unwrap();
    assert!(bot.workspace.is_none());
    assert_eq!(
        db.begin(
            "Nomad",
            "r1",
            "work",
            true,
            &TurnOptions::default(),
            allow_provider
        )
        .unwrap_err()
        .code,
        "workspace_required"
    );
    let options = TurnOptions {
        workspace: Some("/synthetic/today".into()),
        model: None,
        effort: None,
        delivery: Delivery::Reject,
        expected_turn: None,
        from: None,
        origin: None,
        person: None,
        quotes: Vec::new(),
        budget_tokens: None,
        images: Vec::new(),
    };
    let turn = db
        .begin("Nomad", "r1", "work", true, &options, allow_provider)
        .unwrap()
        .turn;
    assert_eq!(db.context(turn).unwrap().workspace, "/synthetic/today");
}

#[test]
fn an_attached_message_needs_no_workspace_and_joins_the_turn_that_names_one() {
    let mut db = db();
    db.create("Nomad", None, binding()).unwrap();
    let attach = TurnOptions {
        delivery: Delivery::Attach,
        ..TurnOptions::default()
    };
    let note = db
        .begin("Nomad", "n", "fyi", true, &attach, allow_provider)
        .unwrap();
    assert_eq!(note.status, "attached");
    let options = TurnOptions {
        workspace: Some("/synthetic/today".into()),
        ..TurnOptions::default()
    };
    let turn = db
        .begin("Nomad", "r1", "work", true, &options, allow_provider)
        .unwrap()
        .turn;
    let absorbed = db
        .absorb(
            turn,
            None,
            8 << 20,
            4096,
            ContextUsage::default(),
            false,
            true,
        )
        .unwrap();
    assert_eq!(absorbed.outcomes[0].0, note.turn);
    assert_eq!(db.context(turn).unwrap().workspace, "/synthetic/today");
}

#[test]
fn an_attached_message_says_it_rode_along_where_a_steer_does_not() {
    let mut db = db();
    db.create("Nomad", Some("/synthetic/today"), binding())
        .unwrap();
    let by = |delivery, origin: Option<&str>| TurnOptions {
        delivery,
        origin: origin.map(Into::into),
        ..TurnOptions::default()
    };
    // Attached while idle: one from a client, one the person sent.
    db.begin(
        "Nomad",
        "n1",
        "a reply in the commons",
        true,
        &by(Delivery::Attach, Some("commons")),
        allow_provider,
    )
    .unwrap();
    db.begin(
        "Nomad",
        "n2",
        "👍",
        true,
        &by(Delivery::Attach, None),
        allow_provider,
    )
    .unwrap();
    let turn = db
        .begin(
            "Nomad",
            "r1",
            "work",
            true,
            &TurnOptions::default(),
            allow_provider,
        )
        .unwrap()
        .turn;
    db.begin(
        "Nomad",
        "s1",
        "task update",
        true,
        &by(Delivery::Steer, Some("tasks")),
        allow_provider,
    )
    .unwrap();
    let absorbed = db
        .absorb(
            turn,
            None,
            8 << 20,
            4096,
            ContextUsage::default(),
            false,
            true,
        )
        .unwrap();
    let steered: Vec<(Value, Value)> = (absorbed.entries.iter())
        .filter(|e| e["event"] == "steered")
        .map(|e| (e["data"]["origin"].clone(), e["data"]["attached"].clone()))
        .collect();
    assert_eq!(
        steered,
        [
            (json!("commons"), json!(true)),
            (Value::Null, json!(true)),
            (json!("tasks"), Value::Null),
        ]
    );
    // History says the same, with each message's node.
    let nodes: Vec<i64> = (absorbed.entries.iter())
        .filter(|e| e["event"] == "steered")
        .map(|e| e["data"]["node"].as_i64().unwrap())
        .collect();
    let read = db.history_items("Nomad", &nodes).unwrap();
    let attached: Vec<&Value> = (read["items"].as_array().unwrap().iter())
        .map(|item| &item["attached"])
        .collect();
    assert_eq!(attached, [&json!(true), &json!(true), &Value::Null]);
}

#[test]
fn retention_keeps_every_attached_message_that_waits() {
    let mut db = db();
    db.create("Bob", Some("/synthetic"), binding()).unwrap();
    let done = db
        .begin(
            "Bob",
            "r1",
            "work",
            true,
            &TurnOptions::default(),
            allow_provider,
        )
        .unwrap()
        .turn;
    db.finish(done, None).unwrap();
    let attach = TurnOptions {
        delivery: Delivery::Attach,
        ..TurnOptions::default()
    };
    for key in ["n1", "n2"] {
        db.begin("Bob", key, "fyi", true, &attach, allow_provider)
            .unwrap();
    }
    // The finished turn is the newest one kept; nothing before it exists.
    assert_eq!(db.prune("Bob", 1).unwrap()["events"], 0);
}

#[test]
fn bot_pages_obey_byte_budget_without_loading_instructions() {
    let mut db = db();
    let workspace = format!("/{}", "x".repeat(4000));
    let instructions = "i".repeat(65536);
    for n in 0..140 {
        let mut config = binding();
        config.instructions = &instructions;
        db.create(&format!("bot-{n:03}"), Some(&workspace), config)
            .unwrap();
    }
    let page = db.list(None, 256, None, false).unwrap();
    let bots = page["bots"].as_array().unwrap();
    assert!(!bots.is_empty() && bots.len() < 140);
    assert!(serde_json::to_vec(&page).unwrap().len() < 524288);
    assert!(bots.iter().all(|b| b.get("instructions").is_none()));
    let rest = db
        .list(page["next_after"].as_str(), 256, None, false)
        .unwrap();
    assert_eq!(bots.len() + rest["bots"].as_array().unwrap().len(), 140);
    assert!(rest["next_after"].is_null());
    assert_eq!(db.inspect("bot-000").unwrap().instructions, instructions);
}

#[test]
fn bot_pages_filter_by_name_pattern_and_activity() {
    let mut db = db();
    for name in [
        "alpha.lead",
        "alpha.worker",
        "beta.lead",
        "beta.worker",
        "home",
    ] {
        db.create(name, Some("/synthetic"), binding()).unwrap();
    }
    for name in ["alpha.worker", "beta.lead"] {
        db.begin(
            name,
            "r1",
            "work",
            true,
            &TurnOptions::default(),
            allow_provider,
        )
        .unwrap();
    }
    let names = |page: Value| -> Vec<String> {
        page["bots"]
            .as_array()
            .unwrap()
            .iter()
            .map(|b| b["name"].as_str().unwrap().to_owned())
            .collect()
    };
    assert_eq!(
        names(db.list(None, 64, Some("alpha.*"), false).unwrap()),
        ["alpha.lead", "alpha.worker"]
    );
    assert_eq!(
        names(db.list(None, 64, Some("*.lead"), false).unwrap()),
        ["alpha.lead", "beta.lead"]
    );
    assert_eq!(
        names(db.list(None, 64, None, true).unwrap()),
        ["alpha.worker", "beta.lead"]
    );
    assert_eq!(
        names(db.list(None, 64, Some("beta.*"), true).unwrap()),
        ["beta.lead"]
    );
    // Filtered pages continue from their cursor like any other.
    let page = db.list(None, 1, Some("*.lead"), false).unwrap();
    assert_eq!(page["next_after"], "alpha.lead");
    let rest = db
        .list(page["next_after"].as_str(), 1, Some("*.lead"), false)
        .unwrap();
    assert_eq!(names(rest.clone()), ["beta.lead"]);
    assert!(rest["next_after"].is_null());
    // GLOB is case-sensitive, like names.
    assert!(names(db.list(None, 64, Some("ALPHA.*"), false).unwrap()).is_empty());
    for bad in ["", &"*".repeat(257)] {
        assert_eq!(
            db.list(None, 64, Some(bad), false).unwrap_err().code,
            "invalid_bot_filter"
        );
    }
    // The active filter reads its own index, not every bot.
    let path = std::env::temp_dir().join(format!("agent-active-{}.sqlite", std::process::id()));
    let _ = std::fs::remove_file(&path);
    drop(Database::initialize(Connection::open(&path).unwrap()).unwrap());
    let plan: String = Connection::open(&path)
        .unwrap()
        .query_row(
            "EXPLAIN QUERY PLAN SELECT name FROM bots WHERE name > ?1 AND running_turn IS NOT NULL ORDER BY name LIMIT 2",
            [""],
            |r| r.get(3),
        )
        .unwrap();
    let _ = std::fs::remove_file(&path);
    assert!(plan.contains("bots_running"), "{plan}");
}

#[test]
fn turn_pages_read_newest_first_from_the_end() {
    let mut db = db();
    db.create("Bob", Some("/synthetic"), binding()).unwrap();
    for n in 1..=5 {
        converse(&mut db, "Bob", n);
    }
    let ids = |page: &Value| -> Vec<i64> {
        page["turns"]
            .as_array()
            .unwrap()
            .iter()
            .map(|t| t["turn"].as_i64().unwrap())
            .collect()
    };
    let all = ids(&db.turns("Bob", 0, None, 64, false).unwrap());
    let newest = db.turns("Bob", 0, None, 2, true).unwrap();
    assert_eq!(ids(&newest), [all[4], all[3]]);
    assert_eq!(newest["next_before"], all[3]);
    assert!(newest.get("next_after").is_none());
    let older = db
        .turns("Bob", 0, newest["next_before"].as_i64(), 2, true)
        .unwrap();
    assert_eq!(ids(&older), [all[2], all[1]]);
    let last = db
        .turns("Bob", 0, older["next_before"].as_i64(), 2, true)
        .unwrap();
    assert_eq!(ids(&last), [all[0]]);
    assert!(last["next_before"].is_null());
    // `after` and `before` bound either direction.
    assert_eq!(
        ids(&db.turns("Bob", all[0], Some(all[4]), 64, false).unwrap()),
        &all[1..4]
    );
    assert_eq!(
        db.turns("Bob", 0, Some(0), 2, true).unwrap_err().code,
        "invalid_turn_page"
    );
}

#[test]
fn budgets_count_tokens_and_turn_listings_carry_accounting() {
    use agent_runtime::provider::Usage;
    let mut db = db();
    let mut capped = binding();
    capped.budget_tokens = Some(150);
    db.create("Bob", Some("/synthetic"), capped).unwrap();
    let turn = db
        .begin(
            "Bob",
            "r1",
            "work",
            true,
            &TurnOptions::default(),
            allow_provider,
        )
        .unwrap()
        .turn;
    let usage = Usage {
        input_tokens: 100,
        output_tokens: 20,
        cached_input_tokens: 0,
        cache_write_tokens: 0,
        cache_write_1h_tokens: 0,
        sent_ms: 0,
        models: Vec::new(),
        served_model: String::new(),
        context_window: 0,
    };
    db.append(turn, vec![assistant("one")], &[], Some(&usage))
        .unwrap();
    assert_eq!(db.inspect("Bob").unwrap().tokens_used, 120);
    db.finish(turn, None).unwrap();
    // Below the cap, a second turn is admitted; its own call pushes past it.
    let second = db
        .begin(
            "Bob",
            "r2",
            "more",
            true,
            &TurnOptions::default(),
            allow_provider,
        )
        .unwrap()
        .turn;
    db.append(second, vec![assistant("two")], &[], Some(&usage))
        .unwrap();
    db.finish(second, None).unwrap();
    assert_eq!(
        db.begin(
            "Bob",
            "r3",
            "again",
            true,
            &TurnOptions::default(),
            allow_provider
        )
        .unwrap_err()
        .code,
        "budget_exhausted"
    );
    let page = db.turns("Bob", 0, None, 1, false).unwrap();
    let first = &page["turns"][0];
    assert_eq!(
        (
            first["turn"].as_i64(),
            first["input_tokens"].as_i64(),
            first["output_tokens"].as_i64()
        ),
        (Some(turn), Some(100), Some(20))
    );
    assert_eq!(first["status"], "completed");
    assert!(first["started_ms"].as_i64().unwrap() <= first["finished_ms"].as_i64().unwrap());
    assert_eq!(page["next_after"], turn);
    let rest = db.turns("Bob", turn, None, 64, false).unwrap();
    assert_eq!(rest["turns"].as_array().unwrap().len(), 1);
    assert!(rest["next_after"].is_null());
}

#[test]
fn tool_selection_migration_rejects_unknown_policy_without_changing_data() {
    let path =
        std::env::temp_dir().join(format!("agent-tool-migrate-{}.sqlite", std::process::id()));
    let _ = std::fs::remove_file(&path);
    {
        let mut db = Database::initialize(Connection::open(&path).unwrap()).unwrap();
        db.create("Bob", Some("/synthetic"), binding()).unwrap();
        converse(&mut db, "Bob", 1);
    }
    let conn = Connection::open(&path).unwrap();
    conn.execute_batch("ALTER TABLE bots DROP COLUMN tools; PRAGMA user_version=17;")
        .unwrap();
    let nodes: Vec<(i64, Vec<u8>)> = conn
        .prepare("SELECT id,item FROM nodes ORDER BY id")
        .unwrap()
        .query_map([], |r| Ok((r.get(0)?, r.get(1)?)))
        .unwrap()
        .collect::<rusqlite::Result<_>>()
        .unwrap();
    assert_eq!(
        Database::initialize(conn).err().unwrap().code,
        "store_migration_tools_unknown"
    );
    let conn = Connection::open(&path).unwrap();
    assert_eq!(
        conn.query_row("PRAGMA user_version", [], |r| r.get::<_, i32>(0))
            .unwrap(),
        17
    );
    assert!(
        !conn
            .query_row(
                "SELECT EXISTS(SELECT 1 FROM pragma_table_info('bots') WHERE name='tools')",
                [],
                |r| r.get::<_, bool>(0)
            )
            .unwrap()
    );
    assert_eq!(
        conn.query_row("SELECT name FROM bots", [], |r| r.get::<_, String>(0))
            .unwrap(),
        "Bob"
    );
    let retained: Vec<(i64, Vec<u8>)> = conn
        .prepare("SELECT id,item FROM nodes ORDER BY id")
        .unwrap()
        .query_map([], |r| Ok((r.get(0)?, r.get(1)?)))
        .unwrap()
        .collect::<rusqlite::Result<_>>()
        .unwrap();
    assert_eq!(retained, nodes);
    drop(conn);
    std::fs::remove_file(path).unwrap();

    // No existing bot means there is no policy to invent.
    let path = std::env::temp_dir().join(format!(
        "agent-empty-tool-migrate-{}.sqlite",
        std::process::id()
    ));
    let _ = std::fs::remove_file(&path);
    drop(Database::initialize(Connection::open(&path).unwrap()).unwrap());
    let conn = Connection::open(&path).unwrap();
    conn.execute_batch("ALTER TABLE bots DROP COLUMN tools; PRAGMA user_version=17;")
        .unwrap();
    let mut db = Database::initialize(conn).unwrap();
    let tools = vec!["echo".to_owned()];
    db.create(
        "New",
        Some("/synthetic"),
        Binding {
            tools: &tools,
            compaction_instructions: None,
            compaction_model: None,
            fallback: None,
            gate: None,
            ..binding()
        },
    )
    .unwrap();
    assert_eq!(db.inspect("New").unwrap().tools, tools);
    drop(db);
    std::fs::remove_file(path).unwrap();
}

#[test]
fn stores_carry_a_schema_version_and_migrate_older_ones_forward() {
    let conn = Connection::open_in_memory().unwrap();
    conn.execute_batch("CREATE TABLE bots(name TEXT PRIMARY KEY)")
        .unwrap();
    assert_eq!(
        Database::initialize(conn).err().unwrap().code,
        "store_schema_unsupported"
    );
    let conn = Connection::open_in_memory().unwrap();
    conn.pragma_update(None, "user_version", Database::SCHEMA + 1)
        .unwrap();
    assert_eq!(
        Database::initialize(conn).err().unwrap().code,
        "store_schema_newer"
    );
    let conn = Connection::open_in_memory().unwrap();
    let version: i32 = {
        let _db = Database::initialize(conn).unwrap();
        Database::SCHEMA
    };
    assert_eq!(version, Database::SCHEMA);
    // A version-6 store (no turn ordinals) is migrated forward at open:
    // ordinals are rebuilt from the accepted events, so windows and history
    // reads work on the old data.
    let path = std::env::temp_dir().join(format!("agent-migrate-{}.sqlite", std::process::id()));
    let _ = std::fs::remove_file(&path);
    {
        let mut db = Database::initialize(Connection::open(&path).unwrap()).unwrap();
        db.create("Bob", Some("/synthetic"), binding()).unwrap();
        for n in 1..=3 {
            converse(&mut db, "Bob", n);
        }
        db.fork(
            "Bob",
            "branch",
            Fork {
                workspace: Some("/synthetic"),
                ..Fork::default()
            },
        )
        .unwrap();
        converse(&mut db, "branch", 4);
        converse(&mut db, "Bob", 5);
    }
    {
        let conn = Connection::open(&path).unwrap();
        conn.execute_batch(
            "DROP TABLE retained_turns;
             DROP TABLE node_sequence; DROP TABLE turn_sequence; DROP INDEX nodes_turn_seq; DROP INDEX nodes_turn; DROP INDEX nodes_parent; DROP INDEX bots_head;
             DROP INDEX bots_context_start; ALTER TABLE bots DROP COLUMN pruned_cursor;
             ALTER TABLE nodes DROP COLUMN turn; ALTER TABLE nodes DROP COLUMN turn_seq;
             ALTER TABLE bots DROP COLUMN context_start;",
        )
        .unwrap();
        conn.pragma_update(None, "user_version", 6).unwrap();
    }
    // A late malformed event must roll back earlier streamed backfill as well
    // as the DDL, so repairing that event permits a clean retry.
    let conn = Connection::open(&path).unwrap();
    let (event, saved): (i64, String) = conn
        .query_row(
            "SELECT id,data FROM events WHERE kind='accepted' ORDER BY id DESC LIMIT 1",
            [],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .unwrap();
    conn.execute("UPDATE events SET data='{}' WHERE id=?", [event])
        .unwrap();
    assert_eq!(
        Database::initialize(conn).err().unwrap().code,
        "store_migration_invalid_event"
    );
    let conn = Connection::open(&path).unwrap();
    assert_eq!(
        conn.query_row("PRAGMA user_version", [], |r| r.get::<_, i32>(0))
            .unwrap(),
        6
    );
    assert_eq!(
        conn.query_row(
            "SELECT count(*) FROM pragma_table_info('nodes') WHERE name='turn_seq'",
            [],
            |r| r.get::<_, i64>(0)
        )
        .unwrap(),
        0
    );
    conn.execute(
        "UPDATE events SET data=? WHERE id=?",
        rusqlite::params![saved, event],
    )
    .unwrap();
    drop(conn);
    let mut db = Database::initialize(Connection::open(&path).unwrap()).unwrap();
    assert!(
        db.history_read("Bob", 3, 0, 1024).unwrap()["text"]
            .as_str()
            .unwrap()
            .contains("r3")
    );
    assert!(
        db.history_read("branch", 4, 0, 1024).unwrap()["text"]
            .as_str()
            .unwrap()
            .contains("p4")
    );
    assert!(
        db.history_read("Bob", 4, 0, 1024).unwrap()["text"]
            .as_str()
            .unwrap()
            .contains("p5")
    );
    let window = db.window("branch", i64::MAX, 3).unwrap().unwrap();
    assert_eq!((window.omitted_items, window.omitted_turns), (6, 3));
    drop(db);
    for suffix in ["", "-wal", "-shm"] {
        let _ = std::fs::remove_file(path.with_file_name(format!(
            "{}{suffix}",
            path.file_name().unwrap().to_string_lossy()
        )));
    }
}

#[test]
fn creation_and_fork_reject_incomplete_deleted_and_reused_creator_identities() {
    let mut db = db();
    let (creator, _) = db.create("Creator", Some("/synthetic"), binding()).unwrap();
    db.create("Source", Some("/synthetic"), binding()).unwrap();
    for reused in [false, true] {
        db.delete_bot("Creator").unwrap();
        if reused {
            db.create("Creator", Some("/synthetic"), binding()).unwrap();
        }
        for (name, id, code) in [
            (Some("Creator"), Some(creator.bot_id), "creator_not_found"),
            (Some("Creator"), None, "creator_identity_required"),
            (None, Some(creator.bot_id), "creator_identity_required"),
        ] {
            let before = db.list(None, 64, None, false).unwrap();
            let mut b = binding();
            b.created_by = name;
            b.created_by_id = id;
            assert_eq!(
                db.create("Child", Some("/synthetic"), b).unwrap_err().code,
                code
            );
            assert_eq!(
                db.fork(
                    "Source",
                    "Child",
                    Fork {
                        created_by: name,
                        created_by_id: id,
                        ..Fork::default()
                    }
                )
                .unwrap_err()
                .code,
                code
            );
            assert_eq!(db.list(None, 64, None, false).unwrap(), before);
        }
        if !reused {
            db.create("Creator", Some("/synthetic"), binding()).unwrap();
        }
    }
}

#[test]
fn lineage_pins_the_creator_identity_so_a_reused_name_is_a_stranger() {
    let mut db = db();
    let (first_a, _) = db.create("A", Some("/synthetic"), binding()).unwrap();
    let mut by_a = binding();
    by_a.created_by = Some("A");
    by_a.created_by_id = Some(first_a.bot_id);
    let (b, _) = db.create("B", Some("/synthetic"), by_a).unwrap();
    assert_eq!(b.created_by_id, Some(first_a.bot_id));
    db.delete_bot("A").unwrap();
    let (second_a, _) = db.create("A", Some("/synthetic"), binding()).unwrap();
    assert_ne!(second_a.bot_id, first_a.bot_id);
    let b = db.inspect("B").unwrap();
    assert_eq!(b.created_by.as_deref(), Some("A"));
    assert_eq!(
        b.created_by_id,
        Some(first_a.bot_id),
        "B still names the A that made it"
    );
    let page = db.list(None, 64, None, false).unwrap();
    let listed = page["bots"].as_array().unwrap();
    let b_row = listed.iter().find(|r| r["name"] == "B").unwrap();
    assert_eq!(b_row["created_by_id"], first_a.bot_id);
    assert_ne!(b_row["created_by_id"], second_a.bot_id);
}

#[test]
fn fork_lineage_pages_are_bounded_and_survive_source_deletion() {
    let mut db = db();
    db.create("source", Some("/synthetic"), binding()).unwrap();
    let turn = db
        .begin(
            "source",
            "r1",
            "prompt",
            true,
            &TurnOptions::default(),
            allow_provider,
        )
        .unwrap()
        .turn;
    db.append(turn, vec![assistant("answer")], &[], None)
        .unwrap();
    db.finish(turn, None).unwrap();
    let checkpoint = db.inspect("source").unwrap().head.unwrap();
    db.fork("source", "branch", Fork::default()).unwrap();
    let later = db
        .begin(
            "source",
            "r2",
            "later",
            true,
            &TurnOptions::default(),
            allow_provider,
        )
        .unwrap()
        .turn;
    db.append(later, vec![assistant("later answer")], &[], None)
        .unwrap();
    db.finish(later, None).unwrap();
    let unrelated = db.inspect("source").unwrap().head.unwrap();
    assert!(
        db.history_nodes("branch", Some(unrelated), 1, None, false)
            .is_err()
    );
    assert!(db.history_nodes("branch", None, 0, None, false).is_err());
    assert!(db.history_nodes("branch", None, 401, None, false).is_err());
    db.delete_bot("source").unwrap();
    let first = db
        .history_nodes("branch", Some(checkpoint), 1, None, false)
        .unwrap();
    assert_eq!(first["nodes"].as_array().unwrap().len(), 1);
    assert_eq!(first["nodes"][0]["node"], checkpoint);
    assert_eq!(first["nodes"][0]["turn"], turn);
    let older = first["next_from"].as_i64().unwrap();
    let forward = db
        .history_nodes("branch", Some(checkpoint), 1, Some(older), true)
        .unwrap();
    assert_eq!(forward["nodes"][0]["node"], older);
    assert_eq!(forward["nodes"][0]["turn"], turn);
    let forward = db
        .history_nodes(
            "branch",
            Some(checkpoint),
            1,
            forward["next_newer"].as_i64(),
            true,
        )
        .unwrap();
    assert_eq!(forward["nodes"][0]["node"], checkpoint);
    assert!(forward["next_newer"].is_null());
    let second = db
        .history_nodes("branch", Some(older), 1, None, false)
        .unwrap();
    assert_eq!(second["nodes"][0]["node"], older);
    assert_eq!(second["nodes"][0]["turn"], turn);
    assert!(second["next_from"].is_null());
    assert_eq!(read_item(&db, "branch", older).unwrap()["role"], "user");
    db.create("empty", None, binding()).unwrap();
    assert_eq!(
        db.history_nodes("empty", None, 10, None, false).unwrap()["nodes"],
        json!([])
    );
}

#[test]
fn fork_events_publish_the_persisted_workspace() {
    let mut db = db();
    db.create("source", Some("/source"), binding()).unwrap();
    // A fork without a folder of its own starts in its source's.
    for (name, workspace, kept) in [
        ("default", None, "/source"),
        ("explicit", Some("/branch"), "/branch"),
    ] {
        let (fork, event) = db
            .fork(
                "source",
                name,
                Fork {
                    workspace,
                    ..Fork::default()
                },
            )
            .unwrap();
        let event = event.unwrap();
        assert_eq!(fork.workspace.as_deref(), Some(kept));
        assert_eq!(event["data"]["workspace"], json!(kept));
        let replay = db.events(name, 0, 10).unwrap();
        assert_eq!(replay["events"][0]["data"]["workspace"], json!(kept));
    }
}

#[test]
fn forks_keep_the_binding_and_instructions_and_record_a_creator() {
    let mut db = db();
    let mut created = binding();
    created.instructions = "first text";
    let (parent, _) = db.create("Parent", Some("/synthetic"), binding()).unwrap();
    created.created_by = Some("Parent");
    created.created_by_id = Some(parent.bot_id);
    let (bot, event) = db.create("Bob", Some("/synthetic"), created).unwrap();
    let event = event.unwrap();
    assert_eq!(bot.created_by.as_deref(), Some("Parent"));
    assert_eq!(event["data"]["created_by"], "Parent");
    // The event carries the validated creator identity.
    assert_eq!(bot.created_by_id, Some(parent.bot_id));
    assert_eq!(event["data"]["status"], "idle");
    assert_eq!(event["data"]["provider"], "openai");
    assert_eq!(event["data"]["workspace"], "/synthetic");
    // A fork keeps the source's text and names its own creator.
    let (same, _) = db
        .fork(
            "Bob",
            "same",
            Fork {
                created_by: Some("Bob"),
                created_by_id: Some(bot.bot_id),
                ..Fork::default()
            },
        )
        .unwrap();
    assert_eq!(same.instructions, "first text");
    assert_eq!(same.created_by.as_deref(), Some("Bob"));
    assert_eq!(
        same.created_by_id,
        Some(bot.bot_id),
        "the creator's identity, not its name"
    );
    // Without a creator the fork records none; the source is untouched.
    let (changed, forked) = db.fork("Bob", "changed", Fork::default()).unwrap();
    assert_eq!(changed.instructions, "first text");
    assert_eq!(changed.created_by, None);
    assert_eq!(
        forked.unwrap()["data"]["created_by"],
        serde_json::Value::Null
    );
    assert_eq!(db.inspect("Bob").unwrap().instructions, "first text");
    // Listing carries the creator; a fork keeps the model and tools.
    let page = db.list(None, 64, None, false).unwrap();
    let listed: Vec<(&str, Option<&str>)> = page["bots"]
        .as_array()
        .unwrap()
        .iter()
        .map(|b| (b["name"].as_str().unwrap(), b["created_by"].as_str()))
        .collect();
    assert_eq!(
        listed,
        vec![
            ("Bob", Some("Parent")),
            ("Parent", None),
            ("changed", None),
            ("same", Some("Bob"))
        ]
    );
    assert_eq!(changed.model, bot.model);
}

#[test]
fn forks_start_at_any_answered_message_and_default_to_the_head() {
    let mut db = db();
    db.create("Bob", Some("/synthetic"), binding()).unwrap();
    let turn = db
        .begin(
            "Bob",
            "r1",
            "work",
            true,
            &TurnOptions::default(),
            allow_provider,
        )
        .unwrap()
        .turn;
    let call = ToolCall {
        custom: false,
        name: "echo".into(),
        call_id: "c1".into(),
        arguments: "{}".into(),
    };
    let call_item: Bytes = serde_json::to_vec(&json!({"type":"function_call","name":"echo",
        "call_id":"c1","arguments":"{}"}))
    .unwrap()
    .into();
    let planned = db
        .append(
            turn,
            vec![assistant("planning"), call_item],
            std::slice::from_ref(&call),
            None,
        )
        .unwrap();
    let mid = planned[1]["data"]["node"].as_i64().unwrap();
    // Between a planned call and its result there is an unanswered tool call.
    assert_eq!(
        db.fork(
            "Bob",
            "early",
            Fork {
                checkpoint: Some(mid),
                ..Fork::default()
            }
        )
        .unwrap_err()
        .code,
        "fork_point_has_open_tool_calls"
    );
    // While the turn runs, a default fork starts at its newest finished
    // round: before any round finishes, the turn's prompt.
    let (live, forked) = db.fork("Bob", "live", Fork { ..Fork::default() }).unwrap();
    assert_eq!(live.head, Some(mid - 2));
    assert_eq!(forked.unwrap()["data"]["node"], mid - 2);
    db.tool_start(turn, &call).unwrap();
    let (_, entry) = db.tool_finish(turn, "c1", &result("hi")).unwrap();
    let answered = entry["data"]["node"].as_i64().unwrap();
    let (branch, _) = db
        .fork(
            "Bob",
            "branch",
            Fork {
                checkpoint: Some(answered),
                ..Fork::default()
            },
        )
        .unwrap();
    assert_eq!(branch.head, Some(answered));
    assert_eq!(stored(&mut db, "branch").len(), 4);
    db.append(turn, vec![assistant("done")], &[], None).unwrap();
    db.finish(turn, None).unwrap();
    let (tip, _) = db.fork("Bob", "tip", Fork { ..Fork::default() }).unwrap();
    assert_eq!(tip.head, db.inspect("Bob").unwrap().head);
    assert_eq!(stored(&mut db, "tip").len(), 5);
    // Branches are independent of the source and of each other.
    let b = db
        .begin(
            "branch",
            "b",
            "go",
            true,
            &TurnOptions {
                workspace: Some("/synthetic/b".into()),
                model: None,
                effort: None,
                delivery: Delivery::Reject,
                expected_turn: None,
                from: None,
                origin: None,
                person: None,
                quotes: Vec::new(),
                budget_tokens: None,
                images: Vec::new(),
            },
            allow_provider,
        )
        .unwrap()
        .turn;
    db.append(b, vec![assistant("branch reply")], &[], None)
        .unwrap();
    db.finish(b, None).unwrap();
    assert_eq!(stored(&mut db, "branch").len(), 6);
    assert_eq!(stored(&mut db, "Bob").len(), 5);
    assert_eq!(stored(&mut db, "tip").len(), 5);
    // Historical reads honor the exact fork cut, even after both branches
    // acquire different turns with the same ordinal.
    converse(&mut db, "Bob", 2);
    let branch_first = db.history_read("branch", 1, 0, 65536).unwrap();
    assert_eq!(branch_first["items"], 4);
    assert!(!branch_first["text"].as_str().unwrap().contains("done"));
    assert_eq!(db.history_read("Bob", 1, 0, 65536).unwrap()["items"], 5);
    let branch_second = db.history_read("branch", 2, 0, 65536).unwrap();
    assert_eq!(branch_second["items"], 2);
    assert!(
        branch_second["text"]
            .as_str()
            .unwrap()
            .contains("branch reply")
    );
    assert!(!branch_second["text"].as_str().unwrap().contains("r2"));
    assert!(
        db.history_read("Bob", 2, 0, 65536).unwrap()["text"]
            .as_str()
            .unwrap()
            .contains("r2")
    );
    assert_eq!(
        db.history_read("Bob", i64::MAX, 0, 65536).unwrap_err().code,
        "turn_not_in_history"
    );
}

fn call(call_id: &str) -> (Bytes, ToolCall) {
    let item = serde_json::to_vec(&json!({"type":"function_call","name":"echo",
        "call_id":call_id,"arguments":"{}"}))
    .unwrap()
    .into();
    let call = ToolCall {
        custom: false,
        name: "echo".into(),
        call_id: call_id.into(),
        arguments: "{}".into(),
    };
    (item, call)
}
fn head(db: &Database, bot: &str) -> i64 {
    db.inspect(bot).unwrap().head.unwrap()
}
/// The node a default fork of a running bot starts at, read from the fork.
fn fork_point(db: &mut Database, source: &str, name: &str) -> Result<i64> {
    let (fork, _) = db.fork(source, name, Fork::default())?;
    Ok(fork.head.unwrap())
}

#[test]
fn a_running_turn_forks_at_its_newest_finished_round() {
    let mut db = db();
    db.create("Bob", Some("/synthetic"), binding()).unwrap();
    converse(&mut db, "Bob", 1);
    let turn = db
        .begin(
            "Bob",
            "r2",
            "work",
            true,
            &TurnOptions::default(),
            allow_provider,
        )
        .unwrap()
        .turn;
    let prompt = head(&db, "Bob");
    // A busy submission is told what is in the way and which requests get
    // past it, with the same facts for programs.
    let busy = db
        .begin(
            "Bob",
            "r3",
            "ask",
            true,
            &TurnOptions::default(),
            allow_provider,
        )
        .unwrap_err();
    assert_eq!(busy.code, "bot_busy");
    assert_eq!(
        busy.detail.unwrap(),
        format!(
            "turn {turn} is running; submit with delivery \"steer\" and expected_turn {turn} \
             to add this to it, or delivery \"queue\" to run it afterwards; to ask without \
             interrupting, fork this bot and submit to the fork"
        )
    );
    assert_eq!(
        busy.facts.map(|facts| Value::Object(*facts)),
        Some(json!({"running_turn":turn,"fork_point":prompt}))
    );
    assert_eq!(fork_point(&mut db, "Bob", "f0").unwrap(), prompt);

    // Two calls in one round: the round closes only at its last result.
    let ((a_item, a), (b_item, b)) = (call("a"), call("b"));
    db.append(turn, vec![a_item, b_item], &[a.clone(), b.clone()], None)
        .unwrap();
    assert_eq!(fork_point(&mut db, "Bob", "f1").unwrap(), prompt);
    db.tool_start(turn, &a).unwrap();
    db.tool_finish(turn, "a", &result("one")).unwrap();
    assert_eq!(fork_point(&mut db, "Bob", "f2").unwrap(), prompt);
    db.tool_start(turn, &b).unwrap();
    db.tool_finish(turn, "b", &result("two")).unwrap();
    let round = head(&db, "Bob");
    assert_eq!(fork_point(&mut db, "Bob", "f3").unwrap(), round);
    // The fork holds the whole round and can run on its own.
    assert_eq!(stored(&mut db, "f3").len(), stored(&mut db, "Bob").len());

    // The next model call is in flight: nothing new is closed yet.
    let (c_item, c) = call("c");
    db.append(
        turn,
        vec![assistant("again"), c_item],
        std::slice::from_ref(&c),
        None,
    )
    .unwrap();
    assert_eq!(fork_point(&mut db, "Bob", "f4").unwrap(), round);

    // Parked on a wait, the turn keeps its closed node.
    db.tool_start(turn, &c).unwrap();
    db.tool_finish(turn, "c", &result("three")).unwrap();
    let answered = head(&db, "Bob");
    let (w_item, w) = call("w");
    db.append(turn, vec![w_item], std::slice::from_ref(&w), None)
        .unwrap();
    db.tool_start(turn, &w).unwrap();
    db.suspend(
        turn,
        "w",
        &["turn:Nobody/1".into()],
        None,
        false,
        &[],
        None,
        None,
    )
    .unwrap();
    assert_eq!(fork_point(&mut db, "Bob", "f5").unwrap(), answered);

    // A forking bot's own fork: the round before the running one.
    db.fork("f5", "f5-self", Fork::default()).unwrap();
    assert_eq!(head(&db, "f5-self"), answered);

    // A final answer closes at once; a reasoning item left last does not.
    db.resume(turn).unwrap();
    db.tool_finish(turn, "w", &result("{}")).unwrap();
    let waited = head(&db, "Bob");
    let reasoning: Bytes = serde_json::to_vec(&json!({"type":"reasoning","id":"rs1",
        "encrypted_content":"synthetic","summary":[]}))
    .unwrap()
    .into();
    db.append(turn, vec![reasoning], &[], None).unwrap();
    assert_eq!(fork_point(&mut db, "Bob", "f6").unwrap(), waited);
    db.append(turn, vec![assistant("done")], &[], None).unwrap();
    let done = head(&db, "Bob");
    assert_eq!(fork_point(&mut db, "Bob", "f7").unwrap(), done);
    db.finish(turn, None).unwrap();
    // Idle again, a default fork takes the head as before.
    assert_eq!(fork_point(&mut db, "Bob", "f8").unwrap(), done);
}

#[test]
fn a_steer_closes_the_round_it_joins() {
    let mut db = db();
    db.create("Bob", Some("/synthetic"), binding()).unwrap();
    let turn = db
        .begin(
            "Bob",
            "r1",
            "work",
            true,
            &TurnOptions::default(),
            allow_provider,
        )
        .unwrap()
        .turn;
    let (item, a) = call("a");
    db.append(turn, vec![item], std::slice::from_ref(&a), None)
        .unwrap();
    db.tool_start(turn, &a).unwrap();
    db.tool_finish(turn, "a", &result("one")).unwrap();
    for n in 0..2 {
        db.begin(
            "Bob",
            &format!("s{n}"),
            &format!("steer {n}"),
            true,
            &TurnOptions {
                delivery: Delivery::Steer,
                ..TurnOptions::default()
            },
            allow_provider,
        )
        .unwrap();
    }
    db.absorb(
        turn,
        None,
        8 << 20,
        4096,
        ContextUsage::default(),
        false,
        true,
    )
    .unwrap();
    let steered = head(&db, "Bob");
    assert_eq!(fork_point(&mut db, "Bob", "fork").unwrap(), steered);
    let text = serde_json::to_string(&stored(&mut db, "fork")).unwrap();
    assert!(
        text.contains("steer 0") && text.contains("steer 1"),
        "{text}"
    );
}

#[test]
fn a_round_that_reuses_a_call_id_of_its_turn_is_refused_whole() {
    // A model that starts over after a steer may send the first round's call
    // id again; the turn's calls are keyed by id.
    let mut db = db();
    db.create("Bob", Some("/synthetic"), binding()).unwrap();
    let turn = db
        .begin(
            "Bob",
            "r1",
            "work",
            true,
            &TurnOptions::default(),
            allow_provider,
        )
        .unwrap()
        .turn;
    let (item, a) = call("a");
    db.append(turn, vec![item], std::slice::from_ref(&a), None)
        .unwrap();
    db.tool_start(turn, &a).unwrap();
    db.tool_finish(turn, "a", &result("one")).unwrap();
    db.begin(
        "Bob",
        "s",
        "steer",
        true,
        &TurnOptions {
            delivery: Delivery::Steer,
            ..TurnOptions::default()
        },
        allow_provider,
    )
    .unwrap();
    db.absorb(
        turn,
        None,
        8 << 20,
        4096,
        ContextUsage::default(),
        false,
        true,
    )
    .unwrap();
    let steered = head(&db, "Bob");
    let seen = db.events("Bob", 0, 256).unwrap()["events"]
        .as_array()
        .unwrap()
        .len();
    let (fresh, b) = call("b");
    let (again, a) = call("a");
    let refused = db
        .append(turn, vec![fresh, again], &[b, a], None)
        .unwrap_err();
    assert_eq!(refused.code, "invalid_tool_call_id");
    assert_eq!(head(&db, "Bob"), steered);
    let events = db.events("Bob", 0, 256).unwrap()["events"]
        .as_array()
        .unwrap()
        .len();
    assert_eq!(events, seen);
    // Nothing of the refused round was planned, so its fresh id stays free.
    let (item, b) = call("b");
    db.append(turn, vec![item], std::slice::from_ref(&b), None)
        .unwrap();
}

#[test]
fn a_fork_keeps_its_sources_window_start() {
    let mut db = db();
    db.create("Bob", Some("/synthetic"), binding()).unwrap();
    for n in 1..=12 {
        converse(&mut db, "Bob", n);
    }
    // A budget the history has outgrown, so the source's start has moved.
    let window = db.window("Bob", i64::MAX, 12).unwrap().unwrap();
    assert!(window.ids.len() <= 9);
    // It grows again past three quarters, where a fresh start would differ.
    converse(&mut db, "Bob", 13);
    assert!(db.window("Bob", i64::MAX, 12).unwrap().unwrap().ids.len() > 9);
    let turn = db
        .begin(
            "Bob",
            "r14",
            "work",
            true,
            &TurnOptions::default(),
            allow_provider,
        )
        .unwrap()
        .turn;
    let source = db.window("Bob", i64::MAX, 12).unwrap().unwrap();
    db.fork("Bob", "fork", Fork::default()).unwrap();
    let fork = db.window("fork", i64::MAX, 12).unwrap().unwrap();
    assert_eq!(
        fork.ids, source.ids,
        "the fork's request begins where the source's does"
    );
    db.append(turn, vec![assistant("done")], &[], None).unwrap();
    db.finish(turn, None).unwrap();
}

#[test]
fn a_fork_waits_on_its_own_commands_and_reads_only_delivered_output() {
    let mut db = db();
    db.create("Bob", Some("/synthetic"), binding()).unwrap();
    let turn = db
        .begin(
            "Bob",
            "r1",
            "work",
            true,
            &TurnOptions::default(),
            allow_provider,
        )
        .unwrap()
        .turn;
    let (item, bg) = call("bg");
    db.append(turn, vec![item], std::slice::from_ref(&bg), None)
        .unwrap();
    db.tool_start(turn, &bg).unwrap();
    let process = db.process_start(turn, "bg").unwrap();
    db.tool_finish(turn, "bg", &result(&format!("proc:{process}")))
        .unwrap();
    // The fork's history holds the handle, but the command is Bob's.
    db.fork("Bob", "early", Fork::default()).unwrap();
    let refused = |db: &Database, bot, id| db.unowned_process(bot, &[id]).unwrap().map(|e| e.code);
    assert_eq!(
        refused(&db, "early", process),
        Some("handle_unavailable".into())
    );
    assert_eq!(refused(&db, "Bob", process), None);
    // A command not started yet could be started by anyone before the wait attaches.
    assert_eq!(
        refused(&db, "Bob", process + 1),
        Some("unknown_handle".into())
    );
    db.process_finish(
        process,
        &json!({"stdout":"large"}),
        &[("stdout", b"synthetic output".to_vec())],
        &[],
    )
    .unwrap();
    // Its stored output is Bob's, and a branch's only once delivered there.
    assert!(db.artifact_page("Bob", turn, "bg", "stdout", 0, 64).is_ok());
    assert_eq!(
        db.artifact_page("early", turn, "bg", "stdout", 0, 64)
            .unwrap_err()
            .code,
        "turn_not_found"
    );
    let (item, w) = call("w");
    db.append(turn, vec![item], std::slice::from_ref(&w), None)
        .unwrap();
    db.tool_start(turn, &w).unwrap();
    let (_, entry) = db.tool_finish(turn, "w", &result("{}")).unwrap();
    let delivered = entry["data"]["node"].as_i64().unwrap();
    db.delivered(delivered, &[process]).unwrap();
    db.fork("Bob", "late", Fork::default()).unwrap();
    assert_eq!(head(&db, "late"), delivered);
    assert!(
        db.artifact_page("late", turn, "bg", "stdout", 0, 64)
            .is_ok()
    );
    assert_eq!(
        db.artifact_page("early", turn, "bg", "stdout", 0, 64)
            .unwrap_err()
            .code,
        "turn_not_found"
    );
}

#[test]
fn schema_36_ends_a_turn_in_flight_and_keeps_its_history() {
    let path = std::env::temp_dir().join(format!("agent-closed-{}.sqlite", std::process::id()));
    let _ = std::fs::remove_file(&path);
    let (turn, kept) = {
        let mut db = Database::initialize(Connection::open(&path).unwrap()).unwrap();
        db.create("Bob", Some("/synthetic"), binding()).unwrap();
        converse(&mut db, "Bob", 1);
        let turn = db
            .begin(
                "Bob",
                "r2",
                "work",
                true,
                &TurnOptions::default(),
                allow_provider,
            )
            .unwrap()
            .turn;
        // Parked, so the turn is still there when the store opens again.
        let (item, w) = call("w");
        db.append(turn, vec![item], std::slice::from_ref(&w), None)
            .unwrap();
        db.tool_start(turn, &w).unwrap();
        db.suspend(
            turn,
            "w",
            &["turn:Nobody/1".into()],
            None,
            false,
            &[],
            None,
            None,
        )
        .unwrap();
        (turn, stored(&mut db, "Bob"))
    };
    Connection::open(&path)
        .unwrap()
        .execute_batch(
            "ALTER TABLE bots DROP COLUMN closed; ALTER TABLE bots DROP COLUMN open_calls;
             ALTER TABLE processes DROP COLUMN delivered; PRAGMA user_version=35;",
        )
        .unwrap();
    // Its closed node cannot be read back, so the upgrade ends the turn
    // rather than guessing, and the bot keeps its whole history.
    let mut db = Database::initialize(Connection::open(&path).unwrap()).unwrap();
    assert_eq!(db.turn_status("Bob", turn).unwrap(), "interrupted");
    let bot = db.inspect("Bob").unwrap();
    assert_eq!(
        (bot.status.as_str(), bot.running_turn),
        ("interrupted", None)
    );
    let history = stored(&mut db, "Bob");
    assert_eq!(history[..kept.len()], kept[..]);
    // The parked call is answered, so a default fork starts at the head.
    assert_eq!(history.len(), kept.len() + 1);
    assert_eq!(
        fork_point(&mut db, "Bob", "after").unwrap(),
        head(&db, "Bob")
    );
    drop(db);
    std::fs::remove_file(path).unwrap();
}

#[test]
fn forks_preserve_reasoning_pairs_and_require_every_parallel_result() {
    let mut db = db();
    db.create("Bob", Some("/synthetic"), binding()).unwrap();
    let turn = db
        .begin("Bob", "r", "work", true, &TurnOptions::default(), |_, _| {
            Ok(())
        })
        .unwrap()
        .turn;
    let reasoning: Bytes = serde_json::to_vec(&json!({"type":"reasoning",
        "id":"rs_synthetic", "summary":[], "encrypted_content":"synthetic"}))
    .unwrap()
    .into();
    let calls: Vec<ToolCall> = ["c1", "c2"]
        .into_iter()
        .map(|id| ToolCall {
            custom: false,
            name: "echo".into(),
            call_id: id.into(),
            arguments: "{}".into(),
        })
        .collect();
    let mut items = vec![reasoning.clone()];
    for call in &calls {
        items.push(
            serde_json::to_vec(&json!({"type":"function_call","name":call.name,
            "call_id":call.call_id,"arguments":call.arguments}))
            .unwrap()
            .into(),
        );
    }
    let events = db.append(turn, items, &calls, None).unwrap();
    let reasoning_node = events[0]["data"]["node"].as_i64().unwrap();
    assert_eq!(
        db.fork(
            "Bob",
            "split",
            Fork {
                checkpoint: Some(reasoning_node),
                ..Fork::default()
            }
        )
        .unwrap_err()
        .code,
        "fork_point_splits_reasoning"
    );
    assert!(db.inspect("split").is_err());
    for (index, call) in calls.iter().enumerate() {
        db.tool_start(turn, call).unwrap();
        let (_, event) = db.tool_finish(turn, &call.call_id, &result("ok")).unwrap();
        let node = event["data"]["node"].as_i64().unwrap();
        if index == 0 {
            assert_eq!(
                db.fork(
                    "Bob",
                    "partial",
                    Fork {
                        checkpoint: Some(node),
                        ..Fork::default()
                    }
                )
                .unwrap_err()
                .code,
                "fork_point_has_open_tool_calls"
            );
        } else {
            db.fork(
                "Bob",
                "answered",
                Fork {
                    checkpoint: Some(node),
                    ..Fork::default()
                },
            )
            .unwrap();
            assert_eq!(
                serde_json::to_vec(&stored(&mut db, "answered")[1]).unwrap(),
                reasoning
            );
            // A validated intermediate checkpoint is also safe for another branch.
            db.fork("answered", "nested", Fork { ..Fork::default() })
                .unwrap();
        }
    }
    // A reasoning-only completion must not bypass the boundary check either.
    db.append(turn, vec![reasoning], &[], None).unwrap();
    db.finish(turn, None).unwrap();
    assert_eq!(
        db.fork("Bob", "unpaired_head", Fork { ..Fork::default() })
            .unwrap_err()
            .code,
        "fork_point_splits_reasoning"
    );
}

#[test]
fn anthropic_forks_check_the_whole_tool_batch_after_a_checkpoint() {
    let mut db = db();
    db.create(
        "Bob",
        Some("/synthetic"),
        Binding {
            family: Family::Anthropic,
            compaction_instructions: None,
            compaction_model: None,
            fallback: None,
            gate: None,
            ..binding()
        },
    )
    .unwrap();
    let first = db
        .begin(
            "Bob",
            "first",
            "hello",
            true,
            &TurnOptions::default(),
            allow_provider,
        )
        .unwrap()
        .turn;
    let reply =
        serde_json::to_vec(&json!({"role":"assistant", "content":[{"type":"text","text":"hi"}]}))
            .unwrap()
            .into();
    db.append(first, vec![reply], &[], None).unwrap();
    db.finish(first, None).unwrap();
    let turn = db
        .begin(
            "Bob",
            "tools",
            "work",
            true,
            &TurnOptions::default(),
            allow_provider,
        )
        .unwrap()
        .turn;
    let calls: Vec<ToolCall> = ["a", "b"]
        .into_iter()
        .map(|id| ToolCall {
            custom: false,
            name: "echo".into(),
            call_id: id.into(),
            arguments: "{}".into(),
        })
        .collect();
    let blocks: Vec<Value> = calls
        .iter()
        .map(|call| {
            json!({"type":"tool_use",
        "id":call.call_id,"name":call.name,"input":{}})
        })
        .collect();
    let item = serde_json::to_vec(&json!({"role":"assistant","content":blocks}))
        .unwrap()
        .into();
    db.append(turn, vec![item], &calls, None).unwrap();
    for (index, call) in calls.iter().enumerate() {
        db.tool_start(turn, call).unwrap();
        let (_, event) = db.tool_finish(turn, &call.call_id, &result("ok")).unwrap();
        let node = event["data"]["node"].as_i64().unwrap();
        if index == 0 {
            assert_eq!(
                db.fork(
                    "Bob",
                    "partial",
                    Fork {
                        checkpoint: Some(node),
                        ..Fork::default()
                    }
                )
                .unwrap_err()
                .code,
                "fork_point_has_open_tool_calls"
            );
        } else {
            db.fork(
                "Bob",
                "answered",
                Fork {
                    checkpoint: Some(node),
                    ..Fork::default()
                },
            )
            .unwrap();
            assert_eq!(stored(&mut db, "answered").len(), 6);
        }
    }
}

fn user(text: &str) -> Bytes {
    Family::Responses.user_item(text).unwrap().into()
}
/// One finished turn: the prompt then an assistant reply.
fn converse(db: &mut Database, bot: &str, n: usize) {
    let turn = db
        .begin(
            bot,
            &format!("r{n}"),
            &format!("p{n}"),
            true,
            &TurnOptions::default(),
            allow_provider,
        )
        .unwrap()
        .turn;
    db.append(turn, vec![assistant(&format!("r{n}"))], &[], None)
        .unwrap();
    db.finish(turn, None).unwrap();
}

#[test]
fn context_windows_start_at_turn_boundaries_and_move_with_hysteresis() {
    let mut db = db();
    db.create("Bob", Some("/synthetic"), binding()).unwrap();
    for n in 1..=10 {
        converse(&mut db, "Bob", n);
    }
    // Unbounded: every item, nothing omitted, and the byte count matches the
    // encoded items exactly (the request Content-Length depends on it).
    let all = db.window("Bob", i64::MAX, i64::MAX).unwrap().unwrap();
    assert_eq!(all.ids.len(), 20);
    assert_eq!((all.omitted_items, all.omitted_turns), (0, 0));
    let joined = db.items_by_ids(&all.ids, 0, 0).unwrap();
    assert_eq!(joined.len() as i64, all.item_bytes + 19);
    let parsed: Vec<Value> = serde_json::from_slice(&[b"[", &joined[..], b"]"].concat()).unwrap();
    assert_eq!(parsed[0]["content"][0]["text"], "p1");
    assert_eq!(parsed[19]["content"][0]["text"], "r10");
    // Five items allow two turns, but the start lands at the oldest turn
    // boundary within three quarters of the budget: only the newest turn.
    let bounded = db.window("Bob", i64::MAX, 5).unwrap().unwrap();
    assert_eq!(bounded.ids, all.ids[18..]);
    assert_eq!((bounded.omitted_items, bounded.omitted_turns), (18, 9));
    // The start is persisted and stays while the window still fits.
    converse(&mut db, "Bob", 11);
    let grown = db.window("Bob", i64::MAX, 5).unwrap().unwrap();
    assert_eq!(grown.ids[0], bounded.ids[0]);
    assert_eq!(grown.ids.len(), 4);
    // Overflow moves the start forward again, to a turn boundary.
    converse(&mut db, "Bob", 12);
    let moved = db.window("Bob", i64::MAX, 5).unwrap().unwrap();
    assert_eq!(moved.ids.len(), 2);
    assert_eq!((moved.omitted_items, moved.omitted_turns), (22, 11));
    // Bytes bound the same way; a turn that cannot fit alone is an error
    // rather than a silently truncated request.
    let small = db
        .window("Bob", all.item_bytes / 5, i64::MAX)
        .unwrap()
        .unwrap();
    assert!(small.ids.len() >= 2 && small.ids.len().is_multiple_of(2));
    assert_eq!(
        db.window("Bob", 10, i64::MAX).unwrap_err().code,
        "context_limit"
    );
    // A running turn's own items are always part of its window.
    let turn = db
        .begin(
            "Bob",
            "live",
            "p13",
            true,
            &TurnOptions::default(),
            allow_provider,
        )
        .unwrap()
        .turn;
    db.append(turn, vec![assistant("partial"), user("more")], &[], None)
        .unwrap();
    let live = db.window("Bob", i64::MAX, 5).unwrap().unwrap();
    assert_eq!(live.ids.len(), 5);
    assert_eq!(
        db.window("Bob", i64::MAX, 2).unwrap_err().code,
        "context_limit"
    );
    assert!(db.window("Nobody", i64::MAX, i64::MAX).is_err());
}

#[test]
fn history_reads_one_turn_by_ordinal_along_the_lineage() {
    let mut db = db();
    db.create("Bob", Some("/synthetic"), binding()).unwrap();
    db.create("Empty", Some("/synthetic"), binding()).unwrap();
    for n in 1..=3 {
        converse(&mut db, "Bob", n);
    }
    let first = db.history_read("Bob", 1, 0, 64 * 1024).unwrap();
    assert_eq!(first["turn"], 1);
    assert_eq!(first["items"], 2);
    assert_eq!(first["truncated"], false);
    let text = first["text"].as_str().unwrap();
    assert!(text.contains("p1") && text.contains("r1") && !text.contains("p2"));
    let last = db.history_read("Bob", 3, 0, 64 * 1024).unwrap();
    assert!(last["text"].as_str().unwrap().contains("r3"));
    let clipped = db.history_read("Bob", 2, 0, 12).unwrap();
    assert_eq!(clipped["truncated"], true);
    assert!(clipped["items"].as_i64().unwrap() < 2);
    assert_eq!(
        db.history_read("Bob", 4, 0, 1024).unwrap_err().code,
        "turn_not_in_history"
    );
    assert_eq!(
        db.history_read("Empty", 1, 0, 1024).unwrap_err().code,
        "turn_not_in_history"
    );
    // A fork shares the numbering of its source up to the fork point and
    // continues it; the source never sees the branch's turns.
    db.fork(
        "Bob",
        "branch",
        Fork {
            workspace: Some("/synthetic"),
            ..Fork::default()
        },
    )
    .unwrap();
    converse(&mut db, "branch", 4);
    assert!(
        db.history_read("branch", 1, 0, 1024).unwrap()["text"]
            .as_str()
            .unwrap()
            .contains("p1")
    );
    assert!(
        db.history_read("branch", 4, 0, 1024).unwrap()["text"]
            .as_str()
            .unwrap()
            .contains("p4")
    );
    assert_eq!(
        db.history_read("Bob", 4, 0, 1024).unwrap_err().code,
        "turn_not_in_history"
    );
    let window = db.window("branch", i64::MAX, 3).unwrap().unwrap();
    assert_eq!((window.omitted_items, window.omitted_turns), (6, 3));
}

/// A turn with `prompt` and one reply, left running unless `finish`.
fn searched_turn(db: &mut Database, bot: &str, key: &str, prompt: &str, reply: &str, finish: bool) {
    let turn = db
        .begin(
            bot,
            key,
            prompt,
            true,
            &TurnOptions::default(),
            allow_provider,
        )
        .unwrap()
        .turn;
    db.append(turn, vec![assistant(reply)], &[], None).unwrap();
    if finish {
        db.finish(turn, None).unwrap();
    }
}

#[test]
fn history_search_finds_text_newest_first_and_continues_before_a_turn() {
    let mut db = db();
    db.create("Bob", Some("/synthetic"), binding()).unwrap();
    for n in 1..=30 {
        let reply = if n % 2 == 0 {
            format!("the NEEDLE is {n}")
        } else {
            format!("nothing at {n}")
        };
        searched_turn(
            &mut db,
            "Bob",
            &format!("k{n}"),
            &format!("task {n}"),
            &reply,
            true,
        );
    }
    let found = db.history_search("Bob", "needle", None, false).unwrap();
    let turns: Vec<i64> = found["matches"]
        .as_array()
        .unwrap()
        .iter()
        .map(|m| m["turn"].as_i64().unwrap())
        .collect();
    assert_eq!(turns, (1..=15).rev().map(|n| n * 2).collect::<Vec<_>>());
    assert_eq!(found["done"], true);
    assert_eq!(found["next_before"], Value::Null);
    let first = &found["matches"][0];
    assert_eq!(first["type"], "message");
    assert!(first["text"].as_str().unwrap().contains("the NEEDLE is 30"));
    // A turn with more than one match lists each; past 20 the search ends
    // at a turn's start and names it to continue from.
    let found = db.history_search("Bob", "task 2", None, false).unwrap();
    let turns: Vec<i64> = found["matches"]
        .as_array()
        .unwrap()
        .iter()
        .map(|m| m["turn"].as_i64().unwrap())
        .collect();
    assert_eq!(turns, vec![29, 28, 27, 26, 25, 24, 23, 22, 21, 20, 2]);
    let found = db.history_search("Bob", "is", None, false).unwrap();
    assert_eq!(found["matches"].as_array().unwrap().len(), 20);
    assert_eq!(found["done"], false);
    assert_eq!(found["next_before"], 11);
    let older = db.history_search("Bob", "is", Some(11), false).unwrap();
    assert_eq!(older["matches"][0]["turn"], 10);
    assert_eq!(older["done"], true);
    assert_eq!(
        db.history_search("Bob", "is", Some(31), false)
            .unwrap_err()
            .code,
        "turn_not_in_history"
    );
    assert_eq!(
        db.history_search("Bob", "", None, false).unwrap_err().code,
        "invalid_history_search"
    );
    // A search of the caller's own history leaves out the turn it runs in,
    // which holds the call itself.
    searched_turn(
        &mut db,
        "Bob",
        "k31",
        "find the needle",
        "searching for needle",
        false,
    );
    let own = db.history_search("Bob", "needle", None, true).unwrap();
    assert_eq!(own["matches"][0]["turn"], 30);
    let other = db.history_search("Bob", "needle", None, false).unwrap();
    assert_eq!(other["matches"][0]["turn"], 31);
    assert_eq!(other["matches"][1]["turn"], 31);
}

#[test]
fn history_search_follows_the_lineage_of_a_fork() {
    let mut db = db();
    db.create("Bob", Some("/synthetic"), binding()).unwrap();
    searched_turn(&mut db, "Bob", "k1", "shared fact", "ok", true);
    db.fork(
        "Bob",
        "branch",
        Fork {
            workspace: Some("/synthetic"),
            ..Fork::default()
        },
    )
    .unwrap();
    searched_turn(&mut db, "branch", "k2", "branch fact", "ok", true);
    let found = db.history_search("branch", "fact", None, false).unwrap();
    let turns: Vec<i64> = found["matches"]
        .as_array()
        .unwrap()
        .iter()
        .map(|m| m["turn"].as_i64().unwrap())
        .collect();
    assert_eq!(turns, vec![2, 1]);
    // A multiline item is searched as history reads show it, compacted.
    let turn = db
        .begin(
            "branch",
            "k3",
            "multiline",
            true,
            &TurnOptions::default(),
            allow_provider,
        )
        .unwrap()
        .turn;
    let item = "{\n  \"type\": \"message\",\n  \"role\": \"assistant\",\n  \"content\": [{\"type\": \"output_text\", \"text\": \"spaced\"}]\n}";
    db.append(turn, vec![Bytes::from(item)], &[], None).unwrap();
    db.finish(turn, None).unwrap();
    let found = db
        .history_search("branch", r#""role":"assistant","content""#, None, false)
        .unwrap();
    assert_eq!(found["matches"][0]["turn"], 3);
    assert!(
        found["matches"][0]["text"]
            .as_str()
            .unwrap()
            .contains("spaced")
    );
    let source = db.history_search("Bob", "branch", None, false).unwrap();
    assert!(source["matches"].as_array().unwrap().is_empty());
    assert_eq!(
        db.history_search("nobody", "fact", None, false)
            .unwrap_err()
            .code,
        "bot_not_found"
    );
}

#[test]
fn history_search_of_a_pattern_searches_every_bot_it_matches_newest_first() {
    let mut db = db();
    db.create("p.lead", Some("/synthetic"), binding()).unwrap();
    db.create("p.task", Some("/synthetic"), binding()).unwrap();
    db.create("other", Some("/synthetic"), binding()).unwrap();
    searched_turn(&mut db, "p.lead", "k1", "plan the needle", "ok", true);
    searched_turn(&mut db, "p.task", "k1", "build", "needle built", true);
    searched_turn(&mut db, "other", "k1", "needle elsewhere", "ok", true);
    db.fork(
        "p.lead",
        "p.side",
        Fork {
            workspace: Some("/synthetic"),
            ..Fork::default()
        },
    )
    .unwrap();
    searched_turn(&mut db, "p.side", "k2", "side needle", "ok", true);
    searched_turn(&mut db, "p.lead", "k2", "nothing here", "ok", true);
    let found = db.history_search_bots("p.*", "needle", None, None).unwrap();
    let matches = found["matches"].as_array().unwrap();
    let bots: Vec<&str> = matches.iter().map(|m| m["bot"].as_str().unwrap()).collect();
    // Newest first by turn ID; the fork's shared first turn is listed once,
    // under the lead it came from, and `other` is not searched.
    assert_eq!(bots, vec!["p.side", "p.task", "p.lead"]);
    assert_eq!(matches[0]["turn"], 2);
    assert_eq!(matches[2]["turn"], 1);
    let handle = matches[1]["handle"].as_str().unwrap();
    assert!(handle.starts_with("turn:p.task/"), "{handle}");
    assert_eq!(found["done"], true);
    // The turn a search runs in is left out.
    let side: i64 = matches[0]["handle"]
        .as_str()
        .unwrap()
        .rsplit('/')
        .next()
        .unwrap()
        .parse()
        .unwrap();
    let skipped = db
        .history_search_bots("*", "needle", None, Some(side))
        .unwrap();
    let bots: Vec<&str> = skipped["matches"]
        .as_array()
        .unwrap()
        .iter()
        .map(|m| m["bot"].as_str().unwrap())
        .collect();
    assert_eq!(bots, vec!["other", "p.task", "p.lead"]);
    assert!(
        db.history_search_bots("nobody*", "needle", None, None)
            .unwrap()["matches"]
            .as_array()
            .unwrap()
            .is_empty()
    );
    assert_eq!(
        db.history_search_bots("*", "", None, None)
            .unwrap_err()
            .code,
        "invalid_history_search"
    );
}

#[test]
fn history_search_of_a_pattern_credits_a_fork_with_what_only_it_has() {
    let mut db = db();
    db.create("src", Some("/synthetic"), binding()).unwrap();
    let turn = db
        .begin(
            "src",
            "k1",
            "the prompt",
            true,
            &TurnOptions::default(),
            allow_provider,
        )
        .unwrap()
        .turn;
    let mut heads = Vec::new();
    for text in ["alpha", "between", "omega"] {
        db.append(turn, vec![assistant(text)], &[], None).unwrap();
        heads.push(db.inspect("src").unwrap().head.unwrap());
    }
    db.finish(turn, None).unwrap();
    // Two forks cut the same turn at different points; the source is not
    // searched.
    for (name, head) in [("f.one", heads[0]), ("f.two", heads[1])] {
        db.fork(
            "src",
            name,
            Fork {
                checkpoint: Some(head),
                workspace: Some("/synthetic"),
                ..Fork::default()
            },
        )
        .unwrap();
    }
    let bots = |query: &str| -> Vec<String> {
        db.history_search_bots("f.*", query, None, None).unwrap()["matches"]
            .as_array()
            .unwrap()
            .iter()
            .map(|m| m["bot"].as_str().unwrap().to_owned())
            .collect()
    };
    // What both have is listed once, under the older fork; what only the
    // later one has, under it, where its handle reads it.
    assert_eq!(bots("the prompt"), ["f.one"]);
    assert_eq!(bots("alpha"), ["f.one"]);
    assert_eq!(bots("between"), ["f.two"]);
    assert!(bots("omega").is_empty());
    let found = db
        .history_search_bots("f.*", "between", None, None)
        .unwrap();
    assert_eq!(found["matches"][0]["handle"], format!("turn:f.two/{turn}"));
    assert_eq!(found["matches"][0]["turn"], 1);
}

#[test]
fn history_search_of_a_pattern_goes_on_from_where_it_had_enough() {
    let mut db = db();
    db.create("a", Some("/synthetic"), binding()).unwrap();
    db.create("b", Some("/synthetic"), binding()).unwrap();
    for n in 1..=15 {
        for bot in ["a", "b"] {
            searched_turn(
                &mut db,
                bot,
                &format!("k{n}"),
                &format!("{bot} {n} zq"),
                "ok",
                true,
            );
        }
    }
    let found = db.history_search_bots("[ab]", "zq", None, None).unwrap();
    assert_eq!(found["matches"].as_array().unwrap().len(), 20);
    assert_eq!(found["done"], false);
    let next = found["next_before"].as_i64().unwrap();
    let rest = db
        .history_search_bots("[ab]", "zq", Some(next), None)
        .unwrap();
    assert_eq!(rest["matches"].as_array().unwrap().len(), 10);
    assert_eq!(rest["done"], true);
    assert_eq!(rest["matches"][9]["bot"], "a");
    assert_eq!(rest["matches"][9]["turn"], 1);
}

#[test]
fn history_preserves_content_beyond_the_preview() {
    let mut db = db();
    db.create("Bob", Some("/synthetic"), binding()).unwrap();
    let prompt = format!("{} final fact", "é🦀".repeat(1000));
    let turn = db
        .begin(
            "Bob",
            "long",
            &prompt,
            true,
            &TurnOptions::default(),
            allow_provider,
        )
        .unwrap()
        .turn;
    let reasoning: Bytes = serde_json::to_vec(&json!({"type":"reasoning","id":"rs_1",
        "summary":[{"type":"summary_text","text":"Résumé 🦀"}],
        "encrypted_content":"opaque-and-large".repeat(16384)}))
    .unwrap()
    .into();
    db.append(turn, vec![reasoning, assistant("done")], &[], None)
        .unwrap();
    db.finish(turn, None).unwrap();
    let window = db.window("Bob", i64::MAX, i64::MAX).unwrap().unwrap();
    let replay = db.items_by_ids(&window.ids, 0, 0).unwrap();
    let page = db.history_read("Bob", 1, 0, 65536).unwrap();
    assert!(page["text"].as_str().unwrap().contains("final fact"));
    // The reading view keeps reasoning summaries but excludes opaque state.
    assert!(!page["text"].as_str().unwrap().contains("encrypted_content"));
    assert_eq!(page["truncated"], false);
    let full = page["text"].as_str().unwrap();
    let mut joined = String::new();
    let mut offset = 0;
    loop {
        let page = db.history_read("Bob", 1, offset, 97).unwrap();
        let text = page["text"].as_str().unwrap();
        assert!(text.len() <= 97 && !text.is_empty());
        assert_eq!(page["offset"], offset);
        assert_eq!(page["next_offset"], offset + text.len() as u64);
        joined.push_str(text);
        offset = page["next_offset"].as_u64().unwrap();
        if page["done"] == true {
            assert_eq!(page["truncated"], false);
            break;
        }
        assert_eq!(page["truncated"], true);
    }
    assert_eq!(joined, full);
    let records: Vec<Value> = joined
        .lines()
        .map(|line| serde_json::from_str(line).unwrap())
        .collect();
    assert_eq!(records[0]["content"][0]["text"], prompt);
    assert_eq!(records.len(), 3);
    assert_eq!(records[1]["summary"][0]["text"], "Résumé 🦀");
    assert_eq!(records[1]["type"], "reasoning");
    assert_eq!(db.items_by_ids(&window.ids, 0, 0).unwrap(), replay);
    assert!(
        String::from_utf8(replay)
            .unwrap()
            .contains("opaque-and-large")
    );
    assert_eq!(db.history_read("Bob", 1, offset, 97).unwrap()["text"], "");
    assert_eq!(
        db.history_read("Bob", 1, offset + 1, 97).unwrap_err().code,
        "invalid_history_page"
    );
    // An offset past any history names that bound, not the page size.
    let far = db.history_read("Bob", 1, u64::MAX, 97).unwrap_err();
    assert_eq!(
        (far.code.as_str(), far.detail.as_deref()),
        (
            "invalid_history_page",
            Some(format!("offset may be up to {}", i64::MAX).as_str())
        )
    );
    let unicode_offset = full.find('é').unwrap() as u64 + 1;
    assert_eq!(
        db.history_read("Bob", 1, unicode_offset, 97)
            .unwrap_err()
            .code,
        "invalid_history_page"
    );
}

#[test]
fn history_normalizes_multiline_items_without_changing_fields_or_replay() {
    let mut db = db();
    for (name, family, item) in [
        (
            "Bob",
            Family::Responses,
            r#"{
                "type":"reasoning",
                "summary":[{"type":"summary_text","text":"é🦀\nnext"}],
                "extra":{"encrypted_content":"nested"}
            }"#,
        ),
        (
            "Alice",
            Family::Anthropic,
            r#"{
                "role":"assistant",
                "content":[
                    {"type":"thinking","thinking":"é🦀","signature":"signed"},
                    {"type":"redacted_thinking","data":"opaque"}
                ],
                "encrypted_content":"not-a-reasoning-item"
            }"#,
        ),
        (
            "Eve",
            Family::Responses,
            r#"{ "type":"message", "role":"assistant", "content":[{"type":"output_text","text":"one\né🦀"}] }"#,
        ),
    ] {
        db.create(
            name,
            Some("/synthetic"),
            Binding {
                family,
                compaction_instructions: None,
                compaction_model: None,
                fallback: None,
                gate: None,
                ..binding()
            },
        )
        .unwrap();
        let turn = db
            .begin(
                name,
                "r1",
                "prompt",
                true,
                &TurnOptions::default(),
                allow_provider,
            )
            .unwrap()
            .turn;
        db.append(turn, vec![Bytes::from_static(item.as_bytes())], &[], None)
            .unwrap();
        db.finish(turn, None).unwrap();
        let window = db.window(name, i64::MAX, i64::MAX).unwrap().unwrap();
        let replay = db.items_by_ids(&window.ids, 0, 0).unwrap();
        assert!(replay.ends_with(item.as_bytes()));
        let mut joined = String::new();
        let mut offset = 0;
        let mut completed_items = 0;
        loop {
            let page = db.history_read(name, 1, offset, 4).unwrap();
            completed_items += page["items"].as_u64().unwrap();
            joined.push_str(page["text"].as_str().unwrap());
            if page["done"] == true {
                break;
            }
            let next = page["next_offset"].as_u64().unwrap();
            assert!(next > offset);
            offset = next;
        }
        let records: Vec<Value> = joined
            .lines()
            .map(|line| serde_json::from_str(line).unwrap())
            .collect();
        assert_eq!(records.len(), 2);
        assert_eq!(completed_items, 2);
        assert_eq!(records[1], serde_json::from_str::<Value>(item).unwrap());
        if !item.contains(['\r', '\n']) {
            assert_eq!(joined.lines().nth(1).unwrap(), item);
        }
        assert_eq!(db.items_by_ids(&window.ids, 0, 0).unwrap(), replay);
    }
}

#[test]
fn deleting_a_bot_frees_only_its_exclusive_history() {
    let mut db = db();
    db.create("Bob", Some("/synthetic"), binding()).unwrap();
    for n in 1..=3 {
        converse(&mut db, "Bob", n);
    }
    db.fork(
        "Bob",
        "branch",
        Fork {
            workspace: Some("/synthetic"),
            ..Fork::default()
        },
    )
    .unwrap();
    converse(&mut db, "branch", 4);
    converse(&mut db, "Bob", 5);
    let before: i64 = db
        .window("Bob", i64::MAX, i64::MAX)
        .unwrap()
        .unwrap()
        .ids
        .len() as i64;
    assert_eq!(before, 8);
    // A running bot cannot be deleted.
    let turn = db
        .begin("Bob", "live", "p", true, &TurnOptions::default(), |_, _| {
            Ok(())
        })
        .unwrap()
        .turn;
    assert_eq!(db.delete_bot("Bob").unwrap_err().code, "bot_busy");
    db.append(turn, vec![assistant("r")], &[], None).unwrap();
    db.finish(turn, None).unwrap();
    // Deleting the source frees its suffix after the fork point (turn 5 and
    // the live turn: four nodes) and keeps the six the branch still reaches.
    let freed = db.delete_bot("Bob").unwrap();
    assert_eq!(freed["nodes"], 4);
    assert_eq!(freed["turns"], 5);
    assert!(db.inspect("Bob").is_err());
    assert_eq!(stored(&mut db, "branch").len(), 8);
    assert!(
        db.history_read("branch", 1, 0, 65536).unwrap()["text"]
            .as_str()
            .unwrap()
            .contains("p1")
    );
    // Deleting the last bot on a lineage frees everything.
    let freed = db.delete_bot("branch").unwrap();
    assert_eq!(freed["nodes"], 8);
    let nodes: i64 = db.events("branch", 0, 8).map(|_| 0).unwrap_or_else(|_| 0);
    assert_eq!(nodes, 0);
    assert_eq!(db.delete_bot("branch").unwrap_err().code, "bot_not_found");
}

#[test]
fn completion_retention_prunes_one_piece_of_the_oldest_turns() {
    let mut db = db();
    db.create("Bob", Some("/synthetic"), binding()).unwrap();
    for n in 1..=7 {
        converse(&mut db, "Bob", n);
    }
    // A backlog drains a piece per completion, so no one job rewrites a
    // long history: six turns past the one kept, three events each.
    let piece = Database::RETENTION_PIECE;
    assert_eq!(
        db.prune_except("Bob", 1, None).unwrap()["events"],
        3 * piece
    );
    assert_eq!(
        db.prune_except("Bob", 1, None).unwrap()["events"],
        3 * (6 - piece)
    );
    assert_eq!(db.prune_except("Bob", 1, None).unwrap()["events"], 0);
    let left = db.events("Bob", 0, 256).unwrap()["events"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|event| event["event"] == "turn_finished")
        .count();
    assert_eq!(left, 1);
}

#[test]
fn completion_retention_passes_turns_whose_processes_still_run() {
    let mut db = db();
    db.create("Bob", Some("/synthetic"), binding()).unwrap();
    let piece = Database::RETENTION_PIECE;
    let mut processes = Vec::new();
    for n in 0..piece {
        let turn = db
            .begin(
                "Bob",
                &format!("bg{n}"),
                "work",
                true,
                &TurnOptions::default(),
                allow_provider,
            )
            .unwrap()
            .turn;
        processes.push(db.process_start(turn, "bg").unwrap());
        db.append(turn, vec![assistant("launched")], &[], None)
            .unwrap();
        db.finish(turn, None).unwrap();
    }
    for n in 1..=3 {
        converse(&mut db, "Bob", n);
    }
    // The oldest piece launched processes that still run: pruning keeps
    // their rows, and the next pass must reach the two turns after them.
    assert!(
        db.prune_except("Bob", 1, None).unwrap()["events"]
            .as_i64()
            .unwrap()
            > 0
    );
    assert_eq!(db.prune_except("Bob", 1, None).unwrap()["events"], 3 * 2);
    assert_eq!(db.prune_except("Bob", 1, None).unwrap()["events"], 0);
    assert_eq!(db.running_processes().unwrap(), piece as i64);
    // A process that ends makes its turn prunable again.
    db.process_finish(processes[0], &json!({"stdout":"done"}), &[], &[])
        .unwrap();
    db.prune_except("Bob", 1, None).unwrap();
    assert!(db.process_result(processes[0]).unwrap().is_none());
}

#[test]
fn pruning_keeps_the_transcript_and_marks_the_replay_gap() {
    let mut db = db();
    db.create("Bob", Some("/synthetic"), binding()).unwrap();
    for n in 1..=5 {
        converse(&mut db, "Bob", n);
    }
    assert_eq!(db.prune("Bob", 0).unwrap_err().code, "invalid_retention");
    let full = db.events("Bob", 0, 256).unwrap();
    let first_turn = full["events"][1]["turn"].as_i64().unwrap();
    assert_eq!(
        db.turn_outcome("Bob", first_turn).unwrap().unwrap()["text"],
        "r1"
    );
    assert!(full.get("pruned_before").is_none());
    let pruned = db.prune("Bob", 2).unwrap();
    assert_eq!(
        db.turn_outcome("Bob", first_turn).unwrap_err().code,
        "turn_result_pruned"
    );
    // Turns 1 to 3: accepted, message, and turn_finished each (no usage here).
    assert_eq!(pruned["events"], 9);
    let cursor = pruned["pruned_cursor"].as_i64().unwrap();
    let page = db.events("Bob", 0, 256).unwrap();
    assert_eq!(page["pruned_before"], cursor);
    let kinds: Vec<&str> = page["events"]
        .as_array()
        .unwrap()
        .iter()
        .map(|e| e["event"].as_str().unwrap())
        .collect();
    assert_eq!(kinds[0], "created");
    assert_eq!(page["events"].as_array().unwrap().len(), 1 + 2 * 3);
    assert!(
        db.events("Bob", cursor, 256)
            .unwrap()
            .get("pruned_before")
            .is_none()
    );
    // The transcript is untouched: the window and history reads still see turn 1.
    assert_eq!(stored(&mut db, "Bob").len(), 10);
    assert!(
        db.history_read("Bob", 1, 0, 65536).unwrap()["text"]
            .as_str()
            .unwrap()
            .contains("p1")
    );
    assert_eq!(
        db.turns("Bob", 0, None, 64, false).unwrap()["turns"]
            .as_array()
            .unwrap()
            .len(),
        5
    );
    // Pruning again with a wider window changes nothing; a narrower one moves the floor.
    assert_eq!(db.prune("Bob", 3).unwrap()["events"], 0);
    assert_eq!(db.prune("Bob", 1).unwrap()["events"], 3);
}

#[test]
fn turn_identity_migrates_above_fork_retained_history() {
    let path =
        std::env::temp_dir().join(format!("agent-turn-identity-{}.sqlite", std::process::id()));
    let _ = std::fs::remove_file(&path);
    let last;
    {
        let mut db = Database::initialize(Connection::open(&path).unwrap()).unwrap();
        db.create("Bob", Some("/synthetic"), binding()).unwrap();
        converse(&mut db, "Bob", 1);
        converse(&mut db, "Bob", 2);
        last = db.turns("Bob", 0, None, 64, false).unwrap()["turns"][1]["turn"]
            .as_i64()
            .unwrap();
        db.fork(
            "Bob",
            "branch",
            Fork {
                workspace: Some("/synthetic"),
                ..Fork::default()
            },
        )
        .unwrap();
        db.delete_bot("Bob").unwrap();
    }
    {
        // Version 8 has no surviving turn rows, but the branch retains their
        // transcript markers. Migration must include those IDs in its floor.
        let conn = Connection::open(&path).unwrap();
        conn.execute_batch(
            "DROP TABLE retained_turns;
             DROP TABLE node_sequence; DROP TABLE turn_sequence; PRAGMA user_version=8;",
        )
        .unwrap();
        let mut db = Database::initialize(conn).unwrap();
        let turn = db
            .begin(
                "branch",
                "next",
                "work",
                true,
                &TurnOptions::default(),
                allow_provider,
            )
            .unwrap()
            .turn;
        assert!(turn > last);
        db.finish(turn, None).unwrap();
        assert!(
            db.history_read("branch", 1, 0, 1024).unwrap()["text"]
                .as_str()
                .unwrap()
                .contains("p1")
        );
    }
    for suffix in ["", "-wal", "-shm"] {
        let _ = std::fs::remove_file(format!("{}{suffix}", path.display()));
    }
}

#[test]
fn checkpoint_identity_survives_migration_deletion_and_restart() {
    let path =
        std::env::temp_dir().join(format!("agent-node-identity-{}.sqlite", std::process::id()));
    let _ = std::fs::remove_file(&path);
    let checkpoint;
    {
        let mut db = Database::initialize(Connection::open(&path).unwrap()).unwrap();
        db.create("Bob", Some("/synthetic"), binding()).unwrap();
        converse(&mut db, "Bob", 1);
        checkpoint = db.inspect("Bob").unwrap().head.unwrap();
    }
    {
        let conn = Connection::open(&path).unwrap();
        conn.execute_batch(
            "DROP TABLE retained_turns;
                            DROP TABLE IF EXISTS node_sequence; PRAGMA user_version=9;",
        )
        .unwrap();
        let mut db = Database::initialize(conn).unwrap();
        assert_eq!(
            read_item(&db, "Bob", checkpoint).unwrap()["content"][0]["text"],
            "r1"
        );
        db.delete_bot("Bob").unwrap();
    }
    {
        let mut db = Database::initialize(Connection::open(&path).unwrap()).unwrap();
        db.create("Bob", Some("/synthetic"), binding()).unwrap();
        converse(&mut db, "Bob", 2);
        assert!(db.inspect("Bob").unwrap().head.unwrap() > checkpoint);
        assert_eq!(
            read_item(&db, "Bob", checkpoint).unwrap_err().code,
            "item_not_in_bot_history"
        );
        assert_eq!(
            db.fork(
                "Bob",
                "stale",
                Fork {
                    checkpoint: Some(checkpoint),
                    ..Fork::default()
                }
            )
            .unwrap_err()
            .code,
            "node_not_in_source_history"
        );
        // Deleting the newest branch must also preserve its IDs while an
        // older bot survives and appends more messages.
        db.fork(
            "Bob",
            "newer",
            Fork {
                workspace: Some("/synthetic"),
                ..Fork::default()
            },
        )
        .unwrap();
        converse(&mut db, "newer", 3);
        let removed = db.inspect("newer").unwrap().head.unwrap();
        db.delete_bot("newer").unwrap();
        converse(&mut db, "Bob", 4);
        assert!(db.inspect("Bob").unwrap().head.unwrap() > removed);
        assert_eq!(
            read_item(&db, "Bob", removed).unwrap_err().code,
            "item_not_in_bot_history"
        );
    }
    for suffix in ["", "-wal", "-shm"] {
        let _ = std::fs::remove_file(format!("{}{suffix}", path.display()));
    }
}

#[test]
fn retention_keeps_running_processes_until_their_results_commit() {
    let mut db = db();
    db.create("Bob", Some("/synthetic"), binding()).unwrap();
    let turn = db
        .begin(
            "Bob",
            "bg",
            "work",
            true,
            &TurnOptions::default(),
            allow_provider,
        )
        .unwrap()
        .turn;
    let process = db.process_start(turn, "bg").unwrap();
    db.append(turn, vec![assistant("launched")], &[], None)
        .unwrap();
    db.finish(turn, None).unwrap();
    assert_eq!(db.delete_bot("Bob").unwrap_err().code, "bot_busy");
    converse(&mut db, "Bob", 2);
    db.prune("Bob", 1).unwrap();
    assert_eq!(db.running_processes().unwrap(), 1);
    db.process_finish(
        process,
        &json!({"stdout":"done"}),
        &[("stdout", b"full output".to_vec())],
        &[],
    )
    .unwrap();
    assert_eq!(
        db.process_result(process).unwrap().unwrap().1.unwrap()["stdout"],
        "done"
    );
    // Late artifacts and completed process rows are removed on the next prune.
    db.prune("Bob", 1).unwrap();
    assert!(db.process_result(process).unwrap().is_none());
    assert!(db.artifact_page("Bob", turn, "bg", "stdout", 0, 4).is_err());
    db.delete_bot("Bob").unwrap();
}

#[test]
fn retention_candidates_migrate_and_stay_scoped_to_their_bot() {
    let path = std::env::temp_dir().join(format!(
        "agent-retention-index-{}.sqlite",
        std::process::id()
    ));
    let _ = std::fs::remove_file(&path);
    {
        let mut db = Database::initialize(Connection::open(&path).unwrap()).unwrap();
        for bot in ["Alice", "Bob"] {
            db.create(bot, Some("/synthetic"), binding()).unwrap();
            for n in 1..=3 {
                converse(&mut db, bot, n);
            }
        }
        db.prune("Bob", 2).unwrap();
    }
    let conn = Connection::open(&path).unwrap();
    // Version 10 retains turn rows but has no operational-retention index.
    conn.execute_batch(
        "DROP TABLE retained_turns;
                        PRAGMA user_version=10;",
    )
    .unwrap();
    let mut db = Database::initialize(conn).unwrap();
    assert_eq!(
        Connection::open(&path)
            .unwrap()
            .query_row(
                "SELECT count(*) FROM retained_turns WHERE bot='Bob'",
                [],
                |r| r.get::<_, i64>(0)
            )
            .unwrap(),
        2,
    );
    let alice = db.events("Alice", 0, 256).unwrap();
    assert_eq!(db.prune("Bob", 1).unwrap()["events"], 3);
    assert_eq!(db.prune("Bob", 1).unwrap()["events"], 0);
    assert_eq!(db.events("Alice", 0, 256).unwrap(), alice);
    assert_eq!(stored(&mut db, "Bob").len(), 6);
    drop(db);
    let conn = Connection::open(&path).unwrap();
    let rows: Vec<(String, i64)> = conn
        .prepare("SELECT bot,count(*) FROM retained_turns GROUP BY bot ORDER BY bot")
        .unwrap()
        .query_map([], |r| Ok((r.get(0)?, r.get(1)?)))
        .unwrap()
        .collect::<rusqlite::Result<_>>()
        .unwrap();
    assert_eq!(rows, vec![("Alice".into(), 3), ("Bob".into(), 1)]);
    // A restart and a wider retention request cannot restore expired records.
    let mut db = Database::initialize(conn).unwrap();
    assert_eq!(db.prune("Bob", 3).unwrap()["events"], 0);
    converse(&mut db, "Bob", 4);
    assert_eq!(db.prune("Bob", 1).unwrap()["events"], 3);
    assert_eq!(db.events("Alice", 0, 256).unwrap(), alice);
    drop(db);
    for suffix in ["", "-wal", "-shm"] {
        let _ = std::fs::remove_file(format!("{}{suffix}", path.display()));
    }
}

#[test]
fn global_replay_gap_survives_deletion_restart_and_migration() {
    let path = std::env::temp_dir().join(format!("agent-global-gap-{}.sqlite", std::process::id()));
    let _ = std::fs::remove_file(&path);
    let high;
    {
        let mut db = Database::initialize(Connection::open(&path).unwrap()).unwrap();
        db.create("Bob", Some("/synthetic"), binding()).unwrap();
        converse(&mut db, "Bob", 1);
        converse(&mut db, "Bob", 2);
        db.prune("Bob", 1).unwrap();
        let gap = db.events("Bob", 0, 256).unwrap()["pruned_before"].clone();
        assert_eq!(db.events_after(0, 256).unwrap()["pruned_before"], gap);
        high = db.events_after(0, 256).unwrap()["next_cursor"]
            .as_i64()
            .unwrap();
        db.delete_bot("Bob").unwrap();
        let empty = db.events_after(0, 256).unwrap();
        assert_eq!(empty["events"], json!([]));
        assert_eq!(empty["pruned_before"], high);
    }
    for migrate in [false, true] {
        if migrate {
            let conn = Connection::open(&path).unwrap();
            conn.execute_batch("DROP TABLE event_retention; PRAGMA user_version=13;")
                .unwrap();
        }
        let db = Database::initialize(Connection::open(&path).unwrap()).unwrap();
        assert_eq!(db.events_after(0, 256).unwrap()["pruned_before"], high);
        assert!(
            db.events_after(high, 256)
                .unwrap()
                .get("pruned_before")
                .is_none()
        );
    }
    std::fs::remove_file(path).unwrap();
}

#[test]
fn global_gap_migration_finds_interior_holes_but_not_contiguous_events() {
    let path =
        std::env::temp_dir().join(format!("agent-interior-gap-{}.sqlite", std::process::id()));
    let _ = std::fs::remove_file(&path);
    let deleted;
    {
        let mut db = Database::initialize(Connection::open(&path).unwrap()).unwrap();
        let (_, first) = db.create("First", None, binding()).unwrap();
        let first = first.unwrap();
        let (_, second) = db.create("Second", None, binding()).unwrap();
        let second = second.unwrap();
        let (_, third) = db.create("Third", None, binding()).unwrap();
        assert!(first["cursor"].as_i64().unwrap() < second["cursor"].as_i64().unwrap());
        deleted = second["cursor"].as_i64().unwrap();
        assert_eq!(third.unwrap()["cursor"], deleted + 1);
    }
    for remove in [false, true] {
        {
            let mut db = Database::initialize(Connection::open(&path).unwrap()).unwrap();
            if remove {
                db.delete_bot("Second").unwrap();
            }
        }
        let conn = Connection::open(&path).unwrap();
        conn.execute_batch("DROP TABLE event_retention; PRAGMA user_version=13;")
            .unwrap();
        let db = Database::initialize(conn).unwrap();
        let page = db.events_after(0, 256).unwrap();
        if remove {
            assert_eq!(page["pruned_before"], deleted);
        } else {
            assert!(page.get("pruned_before").is_none());
        }
    }
    std::fs::remove_file(path).unwrap();
}

#[test]
fn queued_turns_wait_for_the_bot_and_steers_join_the_running_turn() {
    let mut db = db();
    db.create("Bob", Some("/synthetic"), binding()).unwrap();
    let queue = TurnOptions {
        delivery: Delivery::Queue,
        ..TurnOptions::default()
    };
    let steer = TurnOptions {
        delivery: Delivery::Steer,
        ..TurnOptions::default()
    };
    let first = db
        .begin(
            "Bob",
            "r1",
            "first",
            true,
            &TurnOptions::default(),
            allow_provider,
        )
        .unwrap();
    assert_eq!(first.status, "running");
    let second = db
        .begin("Bob", "r2", "second", true, &queue, allow_provider)
        .unwrap();
    assert_eq!(second.status, "queued");
    assert_eq!(second.entry.as_ref().unwrap()["event"], "queued");
    let third = db
        .begin("Bob", "r3", "third", true, &steer, allow_provider)
        .unwrap();
    assert_eq!(third.status, "queued");
    assert!(db.steers_waiting("Bob").unwrap());
    assert_eq!(
        db.begin(
            "Bob",
            "r4",
            "fourth",
            true,
            &TurnOptions::default(),
            allow_provider
        )
        .unwrap_err()
        .code,
        "bot_busy"
    );
    // A retry of a queued submission is a duplicate, not a second row.
    assert!(
        !db.begin("Bob", "r2", "second", true, &queue, allow_provider)
            .unwrap()
            .fresh
    );
    assert!(db.turn_outcome("Bob", second.turn).unwrap().is_none());

    // The boundary takes the steer, not the queued turn, and answers its waiters.
    let absorbed = db
        .absorb(
            first.turn,
            None,
            8 << 20,
            4096,
            ContextUsage::default(),
            false,
            true,
        )
        .unwrap();
    assert_eq!(absorbed.outcomes.len(), 1);
    let (steered, outcome) = &absorbed.outcomes[0];
    assert_eq!(*steered, third.turn);
    assert_eq!(outcome["status"], "steered");
    assert_eq!(outcome["into"], first.turn);
    let kinds: Vec<&str> = absorbed
        .entries
        .iter()
        .map(|e| e["event"].as_str().unwrap())
        .collect();
    assert_eq!(kinds, ["turn_finished", "steered"]);
    assert!(!db.steers_waiting("Bob").unwrap());
    let items = stored(&mut db, "Bob");
    assert_eq!(items.len(), 2);
    assert_eq!(items[1]["content"][0]["text"], "third");
    assert_eq!(db.turn_status("Bob", second.turn).unwrap(), "queued");
    assert!(
        db.absorb(
            first.turn,
            None,
            8 << 20,
            4096,
            ContextUsage::default(),
            false,
            true
        )
        .unwrap()
        .outcomes
        .is_empty()
    );

    // Finishing promotes the bot's oldest queued turn to ready; starting it
    // puts its prompt after the whole first turn.
    db.append(first.turn, vec![assistant("done")], &[], None)
        .unwrap();
    db.finish(first.turn, None).unwrap();
    assert_eq!(db.turn_status("Bob", second.turn).unwrap(), "ready");
    assert_eq!(db.next_ready().unwrap(), Some(("Bob".into(), second.turn)));
    assert_eq!(db.delete_bot("Bob").unwrap_err().code, "bot_busy");
    let (accepted, _) = db.start(second.turn, allow_provider).unwrap();
    assert_eq!(accepted["event"], "accepted");
    assert_eq!(db.inspect("Bob").unwrap().running_turn, Some(second.turn));
    assert_eq!(db.next_ready().unwrap(), None);
    let items = stored(&mut db, "Bob");
    assert_eq!(items[3]["content"][0]["text"], "second");
    assert_eq!(
        db.start(second.turn, allow_provider).unwrap_err().code,
        "stale_turn"
    );

    // A queued turn can be ended where it stands; a ready one hands its
    // place to the next in line.
    let fourth = db
        .begin("Bob", "r4", "fourth", false, &steer, allow_provider)
        .unwrap();
    let fifth = db
        .begin("Bob", "r5", "fifth", false, &queue, allow_provider)
        .unwrap();
    assert_eq!((fourth.status, fifth.status), ("queued", "queued"));
    assert!(db.steers_waiting("Bob").unwrap());
    db.append(second.turn, vec![assistant("done")], &[], None)
        .unwrap();
    db.finish(second.turn, None).unwrap();
    // The steer at the head of the line is promoted, so it is no longer
    // absorbable and leaves the count.
    assert_eq!(db.turn_status("Bob", fourth.turn).unwrap(), "ready");
    assert!(!db.steers_waiting("Bob").unwrap());
    let (entries, outcome) = db
        .end_queued(fourth.turn, &Error::new("cancelled"))
        .unwrap();
    assert_eq!(entries[0]["data"]["status"], "interrupted");
    assert_eq!(outcome["status"], "interrupted");
    assert_eq!(db.turn_status("Bob", fifth.turn).unwrap(), "ready");
    db.end_queued(fifth.turn, &Error::new("cancelled")).unwrap();
    assert_eq!(
        db.end_queued(fifth.turn, &Error::new("cancelled"))
            .unwrap_err()
            .code,
        "stale_turn"
    );
    assert_eq!(db.counts().unwrap().2, 0);
    db.delete_bot("Bob").unwrap();
}

#[test]
fn ready_turns_wait_for_a_slot_and_survive_interrupted_predecessors() {
    let mut db = db();
    db.create("Alice", Some("/synthetic"), binding()).unwrap();
    let queue = TurnOptions {
        delivery: Delivery::Queue,
        ..TurnOptions::default()
    };
    assert_eq!(
        db.begin(
            "Alice",
            "r0",
            "work",
            false,
            &TurnOptions::default(),
            allow_provider
        )
        .unwrap_err()
        .code,
        "active_agent_limit"
    );
    let waiting = db
        .begin("Alice", "r1", "work", false, &queue, allow_provider)
        .unwrap();
    assert_eq!(waiting.status, "ready");
    assert_eq!(waiting.entry.as_ref().unwrap()["data"]["status"], "ready");
    assert_eq!(db.counts().unwrap().2, 1);
    assert_eq!(
        db.next_ready().unwrap(),
        Some(("Alice".into(), waiting.turn))
    );
    db.start(waiting.turn, allow_provider).unwrap();

    // An interrupted predecessor closes its planned calls and releases queued work.
    db.create("Carol", Some("/synthetic"), binding()).unwrap();
    let first = db
        .begin(
            "Carol",
            "c1",
            "work",
            true,
            &TurnOptions::default(),
            allow_provider,
        )
        .unwrap();
    let second = db
        .begin("Carol", "c2", "more", true, &queue, allow_provider)
        .unwrap();
    let call = ToolCall {
        custom: false,
        name: "echo".into(),
        call_id: "call-1".into(),
        arguments: "{}".into(),
    };
    db.append(first.turn, vec![assistant("calling")], &[call], None)
        .unwrap();
    db.finish(first.turn, Some(&Error::new("process_interrupted")))
        .unwrap();
    assert_eq!(db.inspect("Carol").unwrap().status, "interrupted");
    assert_eq!(db.turn_status("Carol", second.turn).unwrap(), "ready");
    db.start(second.turn, allow_provider).unwrap();
    db.finish(second.turn, None).unwrap();
    assert!(
        db.begin("Carol", "c3", "again", true, &queue, allow_provider)
            .unwrap()
            .fresh
    );
}

#[test]
fn ready_work_cannot_be_overtaken_when_capacity_opens() {
    let mut db = db();
    db.create("Bob", Some("/synthetic"), binding()).unwrap();
    let queue = TurnOptions {
        delivery: Delivery::Queue,
        ..TurnOptions::default()
    };
    let first = db
        .begin("Bob", "a", "first", false, &queue, allow_provider)
        .unwrap();
    assert_eq!(
        db.begin(
            "Bob",
            "reject",
            "no",
            true,
            &TurnOptions::default(),
            allow_provider
        )
        .unwrap_err()
        .code,
        "bot_busy"
    );
    let next = db
        .begin("Bob", "b", "next", true, &queue, allow_provider)
        .unwrap();
    assert_eq!((first.status, next.status), ("ready", "queued"));
    db.start(first.turn, allow_provider).unwrap();
    db.finish(first.turn, None).unwrap();
    let last = db
        .begin("Bob", "c", "last", true, &queue, allow_provider)
        .unwrap();
    assert_eq!(last.status, "queued");
    assert_eq!(db.next_ready().unwrap(), Some(("Bob".into(), next.turn)));
}

#[test]
fn restart_keeps_one_ready_head_per_bot() {
    let path = std::env::temp_dir().join(format!("agent-ready-head-{}.sqlite", std::process::id()));
    let _ = std::fs::remove_file(&path);
    let queue = TurnOptions {
        delivery: Delivery::Queue,
        ..TurnOptions::default()
    };
    let (first, next);
    {
        let mut db = Database::initialize(Connection::open(&path).unwrap()).unwrap();
        db.create("Bob", Some("/synthetic"), binding()).unwrap();
        first = db
            .begin("Bob", "a", "first", false, &queue, allow_provider)
            .unwrap()
            .turn;
        next = db
            .begin("Bob", "b", "next", false, &queue, allow_provider)
            .unwrap()
            .turn;
    }
    {
        let mut db = Database::initialize(Connection::open(&path).unwrap()).unwrap();
        assert_eq!(db.turn_status("Bob", next).unwrap(), "queued");
        db.start(first, allow_provider).unwrap();
        assert_eq!(db.next_ready().unwrap(), None);
        db.finish(first, None).unwrap();
        assert_eq!(db.next_ready().unwrap(), Some(("Bob".into(), next)));
    }
    for suffix in ["", "-wal", "-shm"] {
        let _ = std::fs::remove_file(format!("{}{suffix}", path.display()));
    }
}

#[test]
fn retention_preserves_unfinished_turns_and_their_completed_prefix() {
    let mut db = db();
    db.create("Bob", Some("/synthetic"), binding()).unwrap();
    converse(&mut db, "Bob", 1);
    let first = db
        .begin(
            "Bob",
            "a",
            "active",
            true,
            &TurnOptions::default(),
            allow_provider,
        )
        .unwrap();
    let queue = TurnOptions {
        delivery: Delivery::Queue,
        ..TurnOptions::default()
    };
    let next = db
        .begin("Bob", "b", "next", true, &queue, allow_provider)
        .unwrap();
    let cancelled = db
        .begin("Bob", "c", "cancelled", true, &queue, allow_provider)
        .unwrap();
    db.end_queued(cancelled.turn, &Error::new("cancelled"))
        .unwrap();
    // A newer terminal row must not move retention past live tool intents.
    let call = ToolCall {
        custom: false,
        name: "echo".into(),
        call_id: "live".into(),
        arguments: "{}".into(),
    };
    db.append(first.turn, vec![], std::slice::from_ref(&call), None)
        .unwrap();
    db.tool_start(first.turn, &call).unwrap();
    assert_eq!(db.prune("Bob", 1).unwrap()["events"], 0);
    db.tool_finish(first.turn, "live", &result("done")).unwrap();
    db.finish(first.turn, None).unwrap();
    db.prune("Bob", 1).unwrap();
    assert_eq!(
        db.turn_outcome("Bob", first.turn).unwrap().unwrap()["status"],
        "completed"
    );
    assert_eq!(db.turn_status("Bob", next.turn).unwrap(), "ready");
    db.start(next.turn, allow_provider).unwrap();
    db.finish(next.turn, None).unwrap();
    assert!(db.prune("Bob", 1).unwrap()["events"].as_u64().unwrap() > 0);
}

#[test]
fn steers_preserve_explicit_overrides_and_do_not_overtake_a_deferred_steer() {
    let mut db = db();
    db.create("Bob", Some("/default"), binding()).unwrap();
    let active = TurnOptions {
        workspace: Some("/active".into()),
        model: Some("openai/active".into()),
        ..TurnOptions::default()
    };
    let first = db
        .begin("Bob", "first", "work", true, &active, allow_provider)
        .unwrap()
        .turn;
    let matching = TurnOptions {
        delivery: Delivery::Steer,
        ..active.clone()
    };
    let matched = db
        .begin("Bob", "match", "match", true, &matching, allow_provider)
        .unwrap()
        .turn;
    let moved = db
        .begin(
            "Bob",
            "move",
            "move",
            true,
            &TurnOptions {
                workspace: Some("/elsewhere".into()),
                ..matching.clone()
            },
            allow_provider,
        )
        .unwrap()
        .turn;
    let changed = db
        .begin(
            "Bob",
            "model",
            "model",
            true,
            &TurnOptions {
                model: Some("openai/other".into()),
                ..matching
            },
            allow_provider,
        )
        .unwrap()
        .turn;
    let inherited = db
        .begin(
            "Bob",
            "inherit",
            "inherit",
            true,
            &TurnOptions {
                delivery: Delivery::Steer,
                ..TurnOptions::default()
            },
            allow_provider,
        )
        .unwrap()
        .turn;
    let result = db
        .absorb(
            first,
            None,
            8 << 20,
            4096,
            ContextUsage::default(),
            false,
            true,
        )
        .unwrap();
    assert_eq!(
        result
            .outcomes
            .iter()
            .map(|(id, _)| *id)
            .collect::<Vec<_>>(),
        [matched]
    );
    assert!(
        db.absorb(
            first,
            None,
            8 << 20,
            4096,
            ContextUsage::default(),
            false,
            true
        )
        .unwrap()
        .outcomes
        .is_empty()
    );
    assert!(db.steers_waiting("Bob").unwrap());
    db.finish(first, None).unwrap();
    db.start(moved, allow_provider).unwrap();
    assert_eq!(db.context(moved).unwrap().workspace, "/elsewhere");
    assert!(
        db.absorb(
            moved,
            None,
            8 << 20,
            4096,
            ContextUsage::default(),
            false,
            true
        )
        .unwrap()
        .outcomes
        .is_empty()
    );
    db.finish(moved, None).unwrap();
    db.start(changed, allow_provider).unwrap();
    assert_eq!(db.context(changed).unwrap().model, "openai/other");
    assert_eq!(
        db.absorb(
            changed,
            None,
            8 << 20,
            4096,
            ContextUsage::default(),
            false,
            true
        )
        .unwrap()
        .outcomes[0]
            .0,
        inherited
    );
    // The steer named neither, so it records the model and folder it ran with.
    let listed = db.turns("Bob", 0, None, 10, false).unwrap();
    let row = listed["turns"]
        .as_array()
        .unwrap()
        .iter()
        .find(|t| t["turn"] == inherited)
        .unwrap();
    assert_eq!(
        (&row["model"], &row["workspace"]),
        (&json!("openai/other"), &json!("/active"))
    );
    assert!(!db.steers_waiting("Bob").unwrap());
}

#[test]
fn steer_batches_bound_count_and_utf8_bytes_without_losing_the_remainder() {
    for (prompt, count, batch) in [("small".into(), 70, 32), ("é".repeat(128 * 1024), 3, 1)] {
        let mut db = db();
        db.create("Bob", Some("/synthetic"), binding()).unwrap();
        let first = db
            .begin(
                "Bob",
                "first",
                "work",
                true,
                &TurnOptions::default(),
                allow_provider,
            )
            .unwrap()
            .turn;
        let options = TurnOptions {
            delivery: Delivery::Steer,
            ..TurnOptions::default()
        };
        let mut submitted = Vec::new();
        for n in 0..count {
            submitted.push(
                db.begin(
                    "Bob",
                    &n.to_string(),
                    &prompt,
                    true,
                    &options,
                    allow_provider,
                )
                .unwrap()
                .turn,
            );
        }
        let mut seen = Vec::new();
        let mut through = None;
        let mut late = None;
        while seen.len() < count {
            let absorbed = db
                .absorb(
                    first,
                    through,
                    8 << 20,
                    4096,
                    ContextUsage::default(),
                    false,
                    true,
                )
                .unwrap();
            through = absorbed.next_through;
            assert_eq!(absorbed.outcomes.len(), batch.min(count - seen.len()));
            seen.extend(absorbed.outcomes.into_iter().map(|(id, _)| id));
            if late.is_none() {
                late = Some(
                    db.begin("Bob", "late", "late", true, &options, allow_provider)
                        .unwrap()
                        .turn,
                );
            }
            assert!(db.steers_waiting("Bob").unwrap());
        }
        assert!(through.is_none());
        assert_eq!(seen, submitted);
        assert_eq!(db.turn_status("Bob", late.unwrap()).unwrap(), "queued");
        assert_eq!(
            db.absorb(
                first,
                None,
                8 << 20,
                4096,
                ContextUsage::default(),
                false,
                true
            )
            .unwrap()
            .outcomes[0]
                .0,
            late.unwrap()
        );
        assert!(
            db.absorb(
                first,
                None,
                8 << 20,
                4096,
                ContextUsage::default(),
                false,
                true
            )
            .unwrap()
            .outcomes
            .is_empty()
        );
    }
}

#[test]
fn the_worker_publishes_only_what_committed_in_commit_order() {
    use agent_runtime::store::Publication;
    let mut db = db();
    db.create("Bob", Some("/synthetic"), binding()).unwrap();
    db.create("Alice", Some("/synthetic"), binding()).unwrap();
    let mut watermark = 0;
    let mut seen = Vec::new();
    db.publish_since(&mut watermark, |p| {
        seen.push(p);
        true
    })
    .unwrap();
    assert!(watermark > 0);
    let kinds: Vec<&str> = seen
        .iter()
        .map(|p| match p {
            Publication::Event(e) => e["event"].as_str().unwrap(),
            Publication::Through(_) => "through",
            Publication::Approval(_) => "approval",
            Publication::Finished { .. } => "finished",
            Publication::Stepped(_) => "stepped",
        })
        .collect();
    assert_eq!(kinds, ["created", "created"]);
    // Nothing new: nothing published, watermark unchanged.
    let before = watermark;
    let mut none = 0;
    db.publish_since(&mut watermark, |_| {
        none += 1;
        true
    })
    .unwrap();
    assert_eq!((none, watermark), (0, before));
    // A job that fails after inserting an event leaves nothing to publish:
    // only committed rows are read.
    let queue = TurnOptions {
        delivery: Delivery::Queue,
        ..TurnOptions::default()
    };
    let first = db
        .begin(
            "Bob",
            "r1",
            "work",
            true,
            &TurnOptions::default(),
            |_, _| Ok(()),
        )
        .unwrap();
    assert!(
        db.begin("Bob", "r2", "more", true, &queue, |_, _| Err(Error::new(
            "provider_unavailable"
        )))
        .is_err()
    );
    let mut cursors = Vec::new();
    db.publish_since(&mut watermark, |p| {
        if let Publication::Event(e) = p {
            cursors.push(e["cursor"].as_i64().unwrap());
        }
        true
    })
    .unwrap();
    assert_eq!(
        cursors.len(),
        1,
        "the accepted event, and no trace of the refused submission"
    );
    // A finished turn publishes its events, then its outcome for waiters,
    // and a sink that stops reading stops publication without losing the
    // watermark's meaning.
    db.append(first.turn, vec![assistant("done")], &[], None)
        .unwrap();
    db.finish(first.turn, None).unwrap();
    db.announce("Bob", first.turn, json!({"status":"completed"}));
    let mut order = Vec::new();
    db.publish_since(&mut watermark, |p| {
        order.push(match p {
            Publication::Event(e) => e["event"].as_str().unwrap().to_owned(),
            Publication::Through(through) => format!("through:{through}"),
            Publication::Approval(a) => format!("approval:{}", a.cursor),
            Publication::Finished { turn, .. } => format!("finished:{turn}"),
            Publication::Stepped(approval) => format!("stepped:{approval}"),
        });
        true
    })
    .unwrap();
    assert_eq!(
        order,
        [
            "message",
            "turn_finished",
            format!("finished:{}", first.turn).as_str()
        ]
    );
    assert_eq!(watermark, db.last_event_id().unwrap());
}

#[test]
fn strict_steers_are_for_one_running_turn_or_nobody() {
    let mut db = db();
    db.create("Bob", Some("/synthetic"), binding()).unwrap();
    let first = db
        .begin(
            "Bob",
            "r1",
            "first",
            true,
            &TurnOptions::default(),
            |_, _| Ok(()),
        )
        .unwrap();
    let strict = |turn| TurnOptions {
        delivery: Delivery::Steer,
        expected_turn: Some(turn),
        ..TurnOptions::default()
    };
    // The wrong turn, or the wrong mode, is stale before anything is written.
    assert_eq!(
        db.begin("Bob", "s0", "no", true, &strict(first.turn + 1), |_, _| Ok(
            ()
        ))
        .unwrap_err()
        .code,
        "stale_turn"
    );
    let wrong_mode = TurnOptions {
        delivery: Delivery::Queue,
        expected_turn: Some(first.turn),
        ..TurnOptions::default()
    };
    assert_eq!(
        db.begin("Bob", "s0", "no", true, &wrong_mode, |_, _| Ok(()))
            .unwrap_err()
            .code,
        "stale_turn"
    );
    assert_eq!(
        db.turns("Bob", 0, None, 10, false).unwrap()["turns"]
            .as_array()
            .unwrap()
            .len(),
        1
    );
    // The right turn absorbs it like any steer.
    let hit = db
        .begin(
            "Bob",
            "s1",
            "correction",
            true,
            &strict(first.turn),
            |_, _| Ok(()),
        )
        .unwrap();
    assert_eq!(hit.status, "queued");
    let absorbed = db
        .absorb(
            first.turn,
            None,
            8 << 20,
            4096,
            ContextUsage::default(),
            false,
            true,
        )
        .unwrap();
    assert_eq!(absorbed.outcomes[0].0, hit.turn);
    // One that misses its boundary is never absorbed by the next turn and
    // never starts as new work.
    let late = db
        .begin(
            "Bob",
            "s2",
            "too late",
            true,
            &strict(first.turn),
            |_, _| Ok(()),
        )
        .unwrap();
    let plain = db
        .begin(
            "Bob",
            "r2",
            "next",
            true,
            &TurnOptions {
                delivery: Delivery::Queue,
                ..TurnOptions::default()
            },
            |_, _| Ok(()),
        )
        .unwrap();
    db.append(first.turn, vec![assistant("done")], &[], None)
        .unwrap();
    db.finish(first.turn, None).unwrap();
    // The strict steer is the head of the line and cannot start.
    assert_eq!(db.turn_status("Bob", late.turn).unwrap(), "ready");
    assert_eq!(
        db.start(late.turn, |_, _| Ok(())).unwrap_err().code,
        "stale_turn"
    );
    let (_, outcome) = db.end_queued(late.turn, &Error::new("stale_turn")).unwrap();
    assert_eq!(
        (outcome["status"].as_str(), outcome["error"].as_str()),
        (Some("failed"), Some("stale_turn"))
    );
    db.start(plain.turn, |_, _| Ok(())).unwrap();
    // A strict steer for the finished turn is not absorbed by the running one.
    assert_eq!(
        db.begin("Bob", "s3", "stale", true, &strict(first.turn), |_, _| Ok(
            ()
        ))
        .unwrap_err()
        .code,
        "stale_turn"
    );
    assert!(
        db.absorb(
            plain.turn,
            None,
            8 << 20,
            4096,
            ContextUsage::default(),
            false,
            true
        )
        .unwrap()
        .outcomes
        .is_empty()
    );
}

#[test]
fn cached_input_tokens_are_kept_per_turn_and_per_bot_with_their_ratio() {
    let mut db = db();
    db.create("Bob", Some("/synthetic"), binding()).unwrap();
    let turn = db
        .begin(
            "Bob",
            "r1",
            "work",
            true,
            &TurnOptions::default(),
            |_, _| Ok(()),
        )
        .unwrap()
        .turn;
    let cold = Usage {
        input_tokens: 100,
        output_tokens: 10,
        cached_input_tokens: 0,
        cache_write_tokens: 0,
        cache_write_1h_tokens: 0,
        sent_ms: 0,
        models: Vec::new(),
        served_model: String::new(),
        context_window: 0,
    };
    let warm = Usage {
        input_tokens: 300,
        output_tokens: 10,
        cached_input_tokens: 240,
        cache_write_tokens: 0,
        cache_write_1h_tokens: 0,
        sent_ms: 0,
        models: Vec::new(),
        served_model: String::new(),
        context_window: 0,
    };
    db.append(turn, vec![assistant("one")], &[], Some(&cold))
        .unwrap();
    db.append(turn, vec![assistant("two")], &[], Some(&warm))
        .unwrap();
    db.finish(turn, None).unwrap();
    let listed = db.turns("Bob", 0, None, 10, false).unwrap();
    let row = &listed["turns"][0];
    assert_eq!(
        (
            row["input_tokens"].as_i64(),
            row["cached_input_tokens"].as_i64()
        ),
        (Some(400), Some(240))
    );
    assert_eq!(row["cache_hit"], 0.6);
    let bot = db.inspect("Bob").unwrap();
    assert_eq!(
        (bot.input_tokens, bot.cached_input_tokens, bot.cache_hit),
        (400, 240, 0.6)
    );
    assert_eq!(bot.tokens_used, 420);
    let page = db.list(None, 10, None, false).unwrap();
    assert_eq!(page["bots"][0]["cache_hit"], 0.6);
    assert_eq!(agent_runtime::store::cache_hit(0, 0), 0.0);
    assert_eq!(agent_runtime::store::cache_hit(1, 3), 0.333);
}

#[test]
fn cache_migration_rebuilds_retained_usage_or_rolls_back_when_pruned() {
    for pruned in [false, true] {
        let path = std::env::temp_dir().join(format!(
            "agent-cache-migrate-{}-{pruned}.sqlite",
            std::process::id()
        ));
        let _ = std::fs::remove_file(&path);
        {
            let mut db = Database::initialize(Connection::open(&path).unwrap()).unwrap();
            db.create("Bob", Some("/synthetic"), binding()).unwrap();
            let first = db
                .begin(
                    "Bob",
                    "1",
                    "first",
                    true,
                    &TurnOptions::default(),
                    allow_provider,
                )
                .unwrap()
                .turn;
            for (input, cached) in [(100, 0), (300, 240)] {
                db.append(
                    first,
                    vec![assistant("answer")],
                    &[],
                    Some(&Usage {
                        input_tokens: input,
                        cached_input_tokens: cached,
                        cache_write_tokens: 0,
                        cache_write_1h_tokens: 0,
                        sent_ms: 0,
                        output_tokens: 10,
                        models: Vec::new(),
                        served_model: String::new(),
                        context_window: 0,
                    }),
                )
                .unwrap();
            }
            db.finish(first, None).unwrap();
            db.fork(
                "Bob",
                "Fork",
                Fork {
                    workspace: Some("/synthetic"),
                    ..Fork::default()
                },
            )
            .unwrap();
            let second = db
                .begin(
                    "Bob",
                    "2",
                    "second",
                    true,
                    &TurnOptions::default(),
                    allow_provider,
                )
                .unwrap()
                .turn;
            db.failed_usage(
                second,
                &Usage {
                    input_tokens: 100,
                    cached_input_tokens: 40,
                    cache_write_tokens: 0,
                    cache_write_1h_tokens: 0,
                    sent_ms: 0,
                    output_tokens: 10,
                    models: Vec::new(),
                    served_model: String::new(),
                    context_window: 0,
                },
            )
            .unwrap();
            db.finish(second, Some(&Error::new("provider_incomplete")))
                .unwrap();
            if pruned {
                db.prune("Bob", 1).unwrap();
            }
        }
        let conn = Connection::open(&path).unwrap();
        conn.execute_batch(
            "ALTER TABLE turns DROP COLUMN cached_input_tokens;
            ALTER TABLE bots DROP COLUMN input_tokens;
            ALTER TABLE bots DROP COLUMN cached_input_tokens; PRAGMA user_version=18;",
        )
        .unwrap();
        let events: i64 = conn
            .query_row("SELECT COUNT(*) FROM events", [], |r| r.get(0))
            .unwrap();
        if pruned {
            assert_eq!(
                Database::initialize(conn).err().unwrap().code,
                "store_migration_usage_unavailable"
            );
            let conn = Connection::open(&path).unwrap();
            assert_eq!(
                conn.query_row("PRAGMA user_version", [], |r| r.get::<_, i32>(0))
                    .unwrap(),
                18
            );
            assert_eq!(
                conn.query_row(
                    "SELECT COUNT(*) FROM pragma_table_info('bots') WHERE name='input_tokens'",
                    [],
                    |r| r.get::<_, i64>(0)
                )
                .unwrap(),
                0
            );
            assert_eq!(
                conn.query_row("SELECT COUNT(*) FROM events", [], |r| r.get::<_, i64>(0))
                    .unwrap(),
                events
            );
            assert_eq!(
                conn.query_row("SELECT tokens_used FROM bots WHERE name='Bob'", [], |r| r
                    .get::<_, i64>(
                    0
                ))
                .unwrap(),
                530
            );
        } else {
            let db = Database::initialize(conn).unwrap();
            let bot = db.inspect("Bob").unwrap();
            assert_eq!(
                (bot.input_tokens, bot.cached_input_tokens, bot.tokens_used),
                (500, 280, 530)
            );
            assert_eq!(bot.cache_hit, 0.56);
            let turns = db.turns("Bob", 0, None, 10, false).unwrap();
            assert_eq!(turns["turns"][0]["cached_input_tokens"], 240);
            assert_eq!(turns["turns"][1]["cached_input_tokens"], 40);
            let fork = db.inspect("Fork").unwrap();
            assert_eq!(
                (
                    fork.input_tokens,
                    fork.cached_input_tokens,
                    fork.tokens_used
                ),
                (0, 0, 0)
            );
            drop(db);
            let db = Database::initialize(Connection::open(&path).unwrap()).unwrap();
            assert_eq!(db.inspect("Bob").unwrap().cache_hit, 0.56);
        }
        std::fs::remove_file(path).unwrap();
    }
}

#[test]
fn schema_45_events_name_identities_bot_id() {
    let path = std::env::temp_dir().join(format!("agent-schema45-{}.sqlite", std::process::id()));
    let _ = std::fs::remove_file(&path);
    {
        let mut db = Database::initialize(Connection::open(&path).unwrap()).unwrap();
        db.create("Bob", Some("/synthetic"), binding()).unwrap();
        converse(&mut db, "Bob", 1);
    }
    // Before 46 a made bot's event named its identity `id`, and a sender's too.
    Connection::open(&path)
        .unwrap()
        .execute_batch(
            "UPDATE events SET data=json_set(json_remove(data,'$.bot_id'),'$.id',json_extract(data,'$.bot_id'))
             WHERE kind='created';
             UPDATE events SET data=json_set(data,'$.from',json_object('bot','Ann','turn',3,'id',7))
             WHERE kind='accepted';
             PRAGMA user_version=45;",
        )
        .unwrap();
    drop(Database::initialize(Connection::open(&path).unwrap()).unwrap());
    let conn = Connection::open(&path).unwrap();
    let data = |kind: &str| -> Value {
        let text: String = conn
            .query_row(
                "SELECT data FROM events WHERE kind=? ORDER BY id LIMIT 1",
                [kind],
                |r| r.get(0),
            )
            .unwrap();
        serde_json::from_str(&text).unwrap()
    };
    let created = data("created");
    assert_eq!((&created["bot_id"], created.get("id")), (&json!(1), None));
    assert_eq!(
        data("accepted")["from"],
        json!({"bot":"Ann","turn":3,"bot_id":7})
    );
    let version: i32 = conn
        .query_row("PRAGMA user_version", [], |r| r.get(0))
        .unwrap();
    assert_eq!(version, Database::SCHEMA);
    drop(conn);
    std::fs::remove_file(path).unwrap();
}

#[test]
fn schema_46_settings_keep_turns_as_prune_names_it() {
    let path = std::env::temp_dir().join(format!("agent-schema46-{}.sqlite", std::process::id()));
    let _ = std::fs::remove_file(&path);
    let settings = Settings {
        keep_turns: Some(1),
        ..Settings::default()
    };
    let made = || Binding {
        settings: settings.clone(),
        request_id: Some("once"),
        ..binding()
    };
    {
        let mut db = Database::initialize(Connection::open(&path).unwrap()).unwrap();
        db.create("Bob", Some("/synthetic"), made()).unwrap();
    }
    // Before 47 a bot's settings and its creation named retention `retain_turns`.
    Connection::open(&path)
        .unwrap()
        .execute_batch(
            "UPDATE bots SET settings='{\"retain_turns\":1}',
             creation=json_set(json_remove(creation,'$.request.settings.keep_turns'),'$.request.settings.retain_turns',1);
             PRAGMA user_version=46;",
        )
        .unwrap();
    let mut db = Database::initialize(Connection::open(&path).unwrap()).unwrap();
    // The bot reads its setting, and a resend of its creation is still the same request.
    let (again, event) = db.create("Bob", Some("/synthetic"), made()).unwrap();
    assert_eq!((again.settings, event), (settings, None));
    for n in 1..=3 {
        converse(&mut db, "Bob", n);
    }
    db.retain("Bob", None).unwrap();
    let finished = db.events("Bob", 0, 256).unwrap()["events"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|event| event["event"] == "turn_finished")
        .count();
    assert_eq!(finished, 1);
    drop(db);
    std::fs::remove_file(path).unwrap();
}

#[test]
fn schema_47_turns_take_their_own_token_cap() {
    let path = std::env::temp_dir().join(format!("agent-schema47-{}.sqlite", std::process::id()));
    let _ = std::fs::remove_file(&path);
    let usage = |input_tokens, output_tokens| Usage {
        input_tokens,
        output_tokens,
        cached_input_tokens: 0,
        cache_write_tokens: 0,
        cache_write_1h_tokens: 0,
        sent_ms: 0,
        models: Vec::new(),
        served_model: String::new(),
        context_window: 0,
    };
    {
        let mut db = Database::initialize(Connection::open(&path).unwrap()).unwrap();
        db.create("Bob", Some("/synthetic"), binding()).unwrap();
        let turn = db
            .begin(
                "Bob",
                "r1",
                "p1",
                true,
                &TurnOptions::default(),
                allow_provider,
            )
            .unwrap()
            .turn;
        db.append(turn, vec![assistant("one")], &[], Some(&usage(100, 20)))
            .unwrap();
        db.finish(turn, None).unwrap();
    }
    // Before 48 no turn had a cap of its own.
    Connection::open(&path)
        .unwrap()
        .execute_batch("ALTER TABLE turns DROP COLUMN budget_tokens; PRAGMA user_version=47;")
        .unwrap();
    let mut db = Database::initialize(Connection::open(&path).unwrap()).unwrap();
    let capped = TurnOptions {
        budget_tokens: Some(300),
        ..TurnOptions::default()
    };
    let turn = db
        .begin("Bob", "r2", "p2", true, &capped, allow_provider)
        .unwrap()
        .turn;
    // The cap counts from the bot's total when the turn began, however much
    // of it the turn has already spent when its engine reloads it.
    let check = |db: &Database| {
        let mut bot = db.inspect("Bob").unwrap();
        bot.turn_budget = db.context(turn).unwrap().budget;
        bot
    };
    db.append(turn, vec![assistant("two")], &[], Some(&usage(100, 20)))
        .unwrap();
    let bot = check(&db);
    let cap = TurnBudget {
        tokens: 300,
        until: 420,
    };
    assert_eq!((bot.turn_budget, bot.ceiling()), (Some(cap), Some(420)));
    assert!(bot.exhausted().is_none());
    db.append(turn, vec![assistant("three")], &[], Some(&usage(180, 20)))
        .unwrap();
    let error = check(&db).exhausted().unwrap();
    assert_eq!(error.code, "turn_budget_exhausted");
    assert_eq!(
        error.facts.map(|facts| Value::Object(*facts)),
        Some(json!({"turn_budget_tokens":300,"turn_tokens_used":320}))
    );
    db.finish(turn, None).unwrap();
    // A resend names the same cap; the earlier turn reports none.
    let other = TurnOptions {
        budget_tokens: Some(200),
        ..TurnOptions::default()
    };
    assert_eq!(
        db.begin("Bob", "r2", "p2", true, &other, allow_provider)
            .unwrap_err()
            .code,
        "idempotency_conflict"
    );
    let page = db.turns("Bob", 0, None, 256, false).unwrap();
    let caps: Vec<_> = page["turns"]
        .as_array()
        .unwrap()
        .iter()
        .map(|t| t["budget_tokens"].clone())
        .collect();
    assert_eq!(caps, [Value::Null, json!(300)]);
    drop(db);
    std::fs::remove_file(path).unwrap();
}

#[test]
fn bot_identities_are_assigned_in_creation_order_and_never_reused() {
    let path = std::env::temp_dir().join(format!("agent-identity-{}.sqlite", std::process::id()));
    let _ = std::fs::remove_file(&path);
    {
        let mut db = Database::initialize(Connection::open(&path).unwrap()).unwrap();
        let (bob, event) = db.create("Bob", Some("/synthetic"), binding()).unwrap();
        assert_eq!(
            (bob.bot_id, event.unwrap()["data"]["bot_id"].as_i64()),
            (1, Some(1))
        );
        converse(&mut db, "Bob", 1);
        let (fork, event) = db
            .fork(
                "Bob",
                "Fork",
                Fork {
                    workspace: Some("/synthetic"),
                    ..Fork::default()
                },
            )
            .unwrap();
        assert_eq!(
            (fork.bot_id, event.unwrap()["data"]["bot_id"].as_i64()),
            (2, Some(2))
        );
        assert_eq!(db.identity("Bob", Some(1)).unwrap(), 1);
        let stale = db.identity("Fork", Some(1)).unwrap_err();
        assert_eq!(stale.code, "bot_not_found");
        assert!(stale.detail.unwrap().contains("identity 2"));
        assert_eq!(
            db.identity("Nobody", None).unwrap_err().code,
            "bot_not_found"
        );
        db.delete_bot("Fork").unwrap();
        assert_eq!(
            db.create("Fork", Some("/synthetic"), binding())
                .unwrap()
                .0
                .bot_id,
            3
        );
    }
    // A schema-20 store numbers its bots in creation order once; a reset
    // version keeps the identities it already has.
    {
        let conn = Connection::open(&path).unwrap();
        conn.execute_batch(
            "DROP INDEX bots_id; ALTER TABLE bots DROP COLUMN id; DROP TABLE bot_sequence;
             PRAGMA user_version=20;",
        )
        .unwrap();
    }
    {
        let mut db = Database::initialize(Connection::open(&path).unwrap()).unwrap();
        assert_eq!(db.inspect("Bob").unwrap().bot_id, 1);
        assert_eq!(db.inspect("Fork").unwrap().bot_id, 2);
        assert_eq!(
            db.create("New", Some("/synthetic"), binding())
                .unwrap()
                .0
                .bot_id,
            3
        );
    }
    {
        let conn = Connection::open(&path).unwrap();
        conn.pragma_update(None, "user_version", 20).unwrap();
    }
    {
        let db = Database::initialize(Connection::open(&path).unwrap()).unwrap();
        assert_eq!(db.inspect("New").unwrap().bot_id, 3);
        assert_eq!(
            db.list(None, 8, None, false).unwrap()["bots"][0]["bot_id"],
            1
        );
    }
    std::fs::remove_file(path).unwrap();
}

#[test]
fn identity_migration_seeds_allocation_after_sparse_and_empty_stores() {
    for empty in [false, true] {
        let path = std::env::temp_dir().join(format!(
            "agent-identity-gaps-{}-{empty}.sqlite",
            std::process::id()
        ));
        let _ = std::fs::remove_file(&path);
        {
            let mut db = Database::initialize(Connection::open(&path).unwrap()).unwrap();
            if !empty {
                for name in ["First", "Deleted", "Last"] {
                    db.create(name, Some("/synthetic"), binding()).unwrap();
                }
                db.delete_bot("Deleted").unwrap();
            }
        }
        {
            let conn = Connection::open(&path).unwrap();
            conn.execute_batch(
                "DROP INDEX bots_id; ALTER TABLE bots DROP COLUMN id; DROP TABLE bot_sequence;
                 PRAGMA user_version=20;",
            )
            .unwrap();
        }
        let last;
        {
            let mut db = Database::initialize(Connection::open(&path).unwrap()).unwrap();
            let maximum = if empty {
                0
            } else {
                let first = db.inspect("First").unwrap().bot_id;
                let last = db.inspect("Last").unwrap().bot_id;
                assert!(first > 0 && last > first);
                last
            };
            let created = db.create("New", Some("/synthetic"), binding()).unwrap().0;
            assert!(created.bot_id > maximum);
            last = db
                .fork("New", "Fork", Fork { ..Fork::default() })
                .unwrap()
                .0
                .bot_id;
            assert!(last > created.bot_id);
            db.delete_bot("Fork").unwrap();
        }
        {
            let mut db = Database::initialize(Connection::open(&path).unwrap()).unwrap();
            assert!(db.create("AfterRestart", None, binding()).unwrap().0.bot_id > last);
        }
        std::fs::remove_file(path).unwrap();
    }
}

#[test]
fn deletion_runs_in_pieces_refuses_work_and_resumes_after_interruption() {
    let path =
        std::env::temp_dir().join(format!("agent-delete-pieces-{}.sqlite", std::process::id()));
    let _ = std::fs::remove_file(&path);
    {
        let mut db = Database::initialize(Connection::open(&path).unwrap()).unwrap();
        db.create("Bob", Some("/synthetic"), binding()).unwrap();
        for n in 1..=40 {
            converse(&mut db, "Bob", n);
        }
        db.fork(
            "Bob",
            "branch",
            Fork {
                workspace: Some("/synthetic"),
                ..Fork::default()
            },
        )
        .unwrap();
        for n in 41..=44 {
            converse(&mut db, "Bob", n);
        }
        // Pieces of 16 turns: records, then turn rows, then nodes, then the bot.
        let (id, first) = db.start_delete_bot("Bob", 16).unwrap();
        assert_eq!(
            (
                first["done"].as_bool(),
                first["events"].as_i64().unwrap() > 0
            ),
            (Some(false), true)
        );
        assert_eq!(db.inspect("Bob").unwrap().status, "deleting");
        assert_eq!(db.identity("Bob", None).unwrap_err().code, "bot_not_found");
        assert_eq!(
            db.begin(
                "Bob",
                "late",
                "p",
                true,
                &TurnOptions::default(),
                allow_provider
            )
            .unwrap_err()
            .code,
            "bot_not_found"
        );
        assert_eq!(
            db.begin(
                "Bob",
                "r1",
                "p1",
                true,
                &TurnOptions::default(),
                allow_provider
            )
            .unwrap_err()
            .code,
            "bot_not_found"
        );
        assert_eq!(
            db.fork("Bob", "late", Fork { ..Fork::default() })
                .unwrap_err()
                .code,
            "bot_not_found"
        );
        // The name is held, by the identity being deleted, until it is gone.
        let taken = db.create("Bob", None, binding()).unwrap_err();
        assert_eq!(
            (taken.code.as_str(), taken.facts.unwrap()["bot_id"].as_i64()),
            ("bot_exists", Some(id))
        );
        assert_eq!(db.delete_bot_piece("Bob", id, 16).unwrap()["done"], false);
    }
    // Reopening finishes the deletion; the branch keeps the shared prefix.
    {
        let mut db = Database::initialize(Connection::open(&path).unwrap()).unwrap();
        assert_eq!(db.inspect("Bob").unwrap_err().code, "bot_not_found");
        assert_eq!(stored(&mut db, "branch").len(), 80);
        assert!(
            db.history_read("branch", 40, 0, 65536).unwrap()["text"]
                .as_str()
                .unwrap()
                .contains("p40")
        );
    }
    let conn = Connection::open(&path).unwrap();
    for (table, column) in [
        ("turns", "bot"),
        ("events", "bot"),
        ("retained_turns", "bot"),
        ("checkpoints", "bot"),
    ] {
        let left: i64 = conn
            .query_row(
                &format!("SELECT count(*) FROM {table} WHERE {column}='Bob'"),
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(left, 0, "{table}");
    }
    // The branch's 80 nodes are all that survive: Bob's suffix of 8 is gone.
    let nodes: i64 = conn
        .query_row("SELECT count(*) FROM nodes", [], |r| r.get(0))
        .unwrap();
    assert_eq!(nodes, 80);
    drop(conn);
    std::fs::remove_file(path).unwrap();
}

#[test]
fn explicit_prune_pieces_cover_the_same_records_as_one_pass() {
    let mut db = db();
    db.create("Bob", Some("/synthetic"), binding()).unwrap();
    for n in 1..=40 {
        converse(&mut db, "Bob", n);
    }
    let id = db.inspect("Bob").unwrap().bot_id;
    let mut after = 0;
    let mut events = 0;
    let mut pieces = 0;
    loop {
        let piece = db.prune_piece("Bob", id, 2, after, 16).unwrap();
        events += piece["events"].as_i64().unwrap();
        pieces += 1;
        match piece["next_after"].as_i64() {
            Some(next) => after = next,
            None => break,
        }
    }
    // 38 turns of three events each, in three pieces of at most 16 turns.
    assert_eq!((events, pieces), (38 * 3, 3));
    assert_eq!(db.prune("Bob", 2).unwrap()["events"], 0);
    let page = db.events("Bob", 0, 256).unwrap();
    assert!(page["pruned_before"].as_i64().unwrap() > 0);
    assert_eq!(page["events"].as_array().unwrap().len(), 1 + 2 * 3);
}

#[test]
fn stale_deletion_piece_cannot_remove_a_replacement_bot() {
    let mut db = db();
    let original = db
        .create("Bob", Some("/synthetic"), binding())
        .unwrap()
        .0
        .bot_id;
    converse(&mut db, "Bob", 1);
    converse(&mut db, "Bob", 2);
    let (id, first) = db.start_delete_bot("Bob", 1).unwrap();
    assert_eq!(id, original);
    assert_eq!(first["done"], false);
    // Another deletion finishes while the first caller is between pieces.
    db.delete_bot("Bob").unwrap();
    let replacement = db
        .create("Bob", Some("/synthetic"), binding())
        .unwrap()
        .0
        .bot_id;
    assert_ne!(original, replacement);
    converse(&mut db, "Bob", 3);
    let history = stored(&mut db, "Bob");
    assert_eq!(
        db.delete_bot_piece("Bob", original, 1).unwrap_err().code,
        "bot_not_found"
    );
    assert_eq!(db.inspect("Bob").unwrap().bot_id, replacement);
    assert_eq!(db.inspect("Bob").unwrap().status, "completed");
    assert_eq!(stored(&mut db, "Bob"), history);
    assert!(db.delete_bot("Bob").is_ok());
}

#[test]
fn deletion_pieces_report_replay_gaps_until_the_bot_is_gone() {
    let mut db = db();
    db.create("Bob", Some("/synthetic"), binding()).unwrap();
    for n in 1..=10 {
        converse(&mut db, "Bob", n);
    }
    let original = db.events("Bob", 0, 256).unwrap();
    let first_cursor = original["events"][0]["cursor"].as_i64().unwrap();
    let last_cursor = original["next_cursor"].as_i64().unwrap();
    db.create("Other", None, binding()).unwrap();
    let other = db.events("Other", 0, 256).unwrap();
    let (id, mut piece) = db.start_delete_bot("Bob", 4).unwrap();
    while piece["done"] != true {
        let page = db.events("Bob", first_cursor, 256).unwrap();
        // Deletion reserves its whole event range as a conservative gap,
        // including the later piece that removes all remaining bot events.
        assert_eq!(page["pruned_before"], last_cursor);
        assert_eq!(
            db.events_after(first_cursor, 256).unwrap()["pruned_before"],
            last_cursor
        );
        assert!(
            db.events("Bob", last_cursor, 256)
                .unwrap()
                .get("pruned_before")
                .is_none()
        );
        assert_eq!(db.events("Other", 0, 256).unwrap(), other);
        piece = db.delete_bot_piece("Bob", id, 4).unwrap();
    }
    assert_eq!(db.events("Bob", 0, 256).unwrap_err().code, "bot_not_found");
}

#[test]
fn stale_prune_piece_preserves_replacement_records() {
    let mut db = db();
    let original = db
        .create("Bob", Some("/synthetic"), binding())
        .unwrap()
        .0
        .bot_id;
    for n in 1..=10 {
        converse(&mut db, "Bob", n);
    }
    let first = db.prune_piece("Bob", original, 1, 0, 4).unwrap();
    let after = first["next_after"].as_i64().unwrap();
    db.delete_bot("Bob").unwrap();
    let replacement = db
        .create("Bob", Some("/synthetic"), binding())
        .unwrap()
        .0
        .bot_id;
    assert_ne!(original, replacement);
    for n in 11..=12 {
        converse(&mut db, "Bob", n);
    }
    let events = db.events("Bob", 0, 256).unwrap();
    let history = stored(&mut db, "Bob");
    // Both a delayed first piece and a continuation must reject name reuse.
    for cursor in [0, after] {
        assert_eq!(
            db.prune_piece("Bob", original, 1, cursor, 4)
                .unwrap_err()
                .code,
            "bot_not_found"
        );
        assert_eq!(db.events("Bob", 0, 256).unwrap(), events);
        assert_eq!(stored(&mut db, "Bob"), history);
    }
    assert!(db.prune("Bob", 1).unwrap()["events"].as_i64().unwrap() > 0);
}

#[test]
fn lineage_pages_preserve_turns_and_visit_every_node_in_both_directions() {
    let mut db = db();
    db.create("source", Some("/synthetic"), binding()).unwrap();
    let mut expected = Vec::new();
    for i in 0..6 {
        let turn = db
            .begin(
                "source",
                &format!("r{i}"),
                "prompt",
                true,
                &TurnOptions::default(),
                allow_provider,
            )
            .unwrap()
            .turn;
        let user = db.inspect("source").unwrap().head.unwrap();
        db.append(turn, vec![assistant("answer")], &[], None)
            .unwrap();
        let reply = db.inspect("source").unwrap().head.unwrap();
        db.finish(turn, None).unwrap();
        expected.extend([(user, turn), (reply, turn)]);
    }
    db.fork("source", "branch", Fork::default()).unwrap();
    db.delete_bot("source").unwrap();
    let mut backward = Vec::new();
    let mut from = None;
    loop {
        let page = db.history_nodes("branch", from, 3, None, false).unwrap();
        backward.extend(
            page["nodes"]
                .as_array()
                .unwrap()
                .iter()
                .map(|n| (n["node"].as_i64().unwrap(), n["turn"].as_i64().unwrap())),
        );
        from = page["next_from"].as_i64();
        if from.is_none() {
            break;
        }
    }
    backward.reverse();
    assert_eq!(backward, expected);
    let mut forward = Vec::new();
    let mut min = None;
    loop {
        let page = db.history_nodes("branch", None, 3, min, true).unwrap();
        forward.extend(
            page["nodes"]
                .as_array()
                .unwrap()
                .iter()
                .rev()
                .map(|n| (n["node"].as_i64().unwrap(), n["turn"].as_i64().unwrap())),
        );
        min = page["next_newer"].as_i64();
        if min.is_none() {
            break;
        }
    }
    assert_eq!(forward, expected);
}

#[test]
fn absorption_leaves_steers_that_do_not_fit_the_context_queued() {
    // Bytes: a 4 KiB context keeps three quarters, 3,072 bytes, for the
    // running turn; its prompt item takes some, and two of three 1,000-byte
    // steers fit. Items: with room for two more items, two fit as well.
    // What the view sends ahead of the turn, a summary or notes, leaves
    // room for one. The whole budget takes one more.
    let reserved = |bytes, items| ContextUsage { bytes, items };
    for (context_bytes, context_items, ahead, fit) in [
        (4096usize, 4096usize, reserved(0, 0), 2),
        (8 << 20, 4, reserved(0, 0), 2),
        (4096, 4096, reserved(1100, 1), 1),
        (8 << 20, 4, reserved(0, 1), 1),
    ] {
        let mut db = db();
        db.create("Bob", Some("/synthetic"), binding()).unwrap();
        let first = db
            .begin(
                "Bob",
                "first",
                "work",
                true,
                &TurnOptions::default(),
                allow_provider,
            )
            .unwrap()
            .turn;
        let options = TurnOptions {
            delivery: Delivery::Steer,
            ..TurnOptions::default()
        };
        let steers: Vec<i64> = (0..3)
            .map(|n| {
                db.begin(
                    "Bob",
                    &n.to_string(),
                    &"s".repeat(1000),
                    true,
                    &options,
                    allow_provider,
                )
                .unwrap()
                .turn
            })
            .collect();
        let absorbed = db
            .absorb(
                first,
                None,
                context_bytes,
                context_items,
                ahead,
                false,
                true,
            )
            .unwrap();
        let taken: Vec<i64> = absorbed.outcomes.iter().map(|(id, _)| *id).collect();
        assert_eq!(taken, steers[..fit]);
        assert!(absorbed.capped && absorbed.next_through.is_none());
        // The next does not fit now and is not retried into a full turn.
        let again = db
            .absorb(
                first,
                None,
                context_bytes,
                context_items,
                ahead,
                false,
                true,
            )
            .unwrap();
        assert!(again.outcomes.is_empty() && again.capped);
        assert_eq!(db.turn_status("Bob", steers[fit]).unwrap(), "queued");
        // Against the whole budget, as when no summary made room, the
        // next one fits: the budget is the only reason.
        let whole = db
            .absorb(first, None, context_bytes, context_items, ahead, true, true)
            .unwrap();
        let taken: Vec<i64> = whole.outcomes.iter().map(|(id, _)| *id).collect();
        assert_eq!(taken, steers[fit..fit + 1]);
        assert_eq!(whole.capped, fit + 1 < steers.len());
    }
}

#[test]
fn absorption_counts_the_separator_of_every_item_the_request_sends() {
    // The request puts a comma between items, so a steer that fits the
    // whole budget by its bytes alone does not fit the request: the turn's
    // prompt, its reply, and the steer each take one byte more.
    for (spare, taken) in [(0, false), (2, false), (3, true)] {
        let mut db = db();
        db.create("Bob", Some("/synthetic"), binding()).unwrap();
        let options = TurnOptions::default();
        let first = db
            .begin("Bob", "first", "work", true, &options, allow_provider)
            .unwrap()
            .turn;
        // A round a summary could take, so the whole budget applies.
        db.append(first, vec![assistant(&"r".repeat(4000))], &[], None)
            .unwrap();
        let steer = db
            .begin(
                "Bob",
                "s",
                "steer",
                true,
                &TurnOptions {
                    delivery: Delivery::Steer,
                    ..TurnOptions::default()
                },
                allow_provider,
            )
            .unwrap()
            .turn;
        let (family, used, _) = db.turn_usage("Bob", first).unwrap();
        let steer_bytes = family.user_item("steer").unwrap().len();
        let budget = used + steer_bytes + spare;
        let absorbed = db
            .absorb(first, None, budget, 64, ContextUsage::default(), true, true)
            .unwrap();
        let ids: Vec<i64> = absorbed.outcomes.iter().map(|(id, _)| *id).collect();
        assert_eq!(ids, if taken { vec![steer] } else { vec![] }, "{spare}");
    }
}

#[test]
fn a_steer_goes_past_three_quarters_only_over_what_a_summary_can_take() {
    // The same budget and steer, and the same bytes in the turn: as a
    // round, which a summary can take once the model has answered past
    // it, the steer goes in against the whole budget; as the turn's
    // prompt, which no summary takes, it would leave the turn no room
    // after any summary, so it keeps to three quarters and stays queued.
    for (prompt, round, taken) in [
        ("work".to_owned(), "r".repeat(4000), true),
        ("w".repeat(4000), String::new(), false),
    ] {
        let mut db = db();
        db.create("Bob", Some("/synthetic"), binding()).unwrap();
        let first = db
            .begin(
                "Bob",
                "first",
                &prompt,
                true,
                &TurnOptions::default(),
                allow_provider,
            )
            .unwrap()
            .turn;
        if !round.is_empty() {
            db.append(first, vec![assistant(&round)], &[], None)
                .unwrap();
        }
        let steer = db
            .begin(
                "Bob",
                "s",
                "steer",
                true,
                &TurnOptions {
                    delivery: Delivery::Steer,
                    ..TurnOptions::default()
                },
                allow_provider,
            )
            .unwrap()
            .turn;
        let absorbed = db
            .absorb(first, None, 4400, 64, ContextUsage::default(), true, true)
            .unwrap();
        let ids: Vec<i64> = absorbed.outcomes.iter().map(|(id, _)| *id).collect();
        assert_eq!(ids, if taken { vec![steer] } else { vec![] }, "{taken}");
        assert_eq!(absorbed.capped, !taken);
    }
}

#[test]
fn turn_usage_counts_only_the_active_branch_including_absorbed_steers() {
    let mut db = db();
    db.create("Bob", Some("/synthetic"), binding()).unwrap();
    let old = db
        .begin(
            "Bob",
            "old",
            "old history",
            true,
            &TurnOptions::default(),
            allow_provider,
        )
        .unwrap()
        .turn;
    db.append(old, vec![assistant("old answer")], &[], None)
        .unwrap();
    db.finish(old, None).unwrap();
    db.fork(
        "Bob",
        "Alice",
        Fork {
            workspace: Some("/synthetic"),
            ..Fork::default()
        },
    )
    .unwrap();
    let options = TurnOptions::default();
    let bob = db
        .begin("Bob", "current", "bob", true, &options, allow_provider)
        .unwrap()
        .turn;
    let alice = db
        .begin("Alice", "current", "alice", true, &options, allow_provider)
        .unwrap()
        .turn;
    let answer = assistant("unicode: é🙂");
    db.append(bob, vec![answer.clone(); 64], &[], None).unwrap();
    let steer_options = TurnOptions {
        delivery: Delivery::Steer,
        ..options
    };
    db.begin(
        "Bob",
        "steer",
        "correction",
        true,
        &steer_options,
        allow_provider,
    )
    .unwrap();
    db.absorb(
        bob,
        None,
        8 << 20,
        4096,
        ContextUsage::default(),
        false,
        true,
    )
    .unwrap();
    let (_, bytes, count) = db.turn_usage("Bob", bob).unwrap();
    assert_eq!(count, 66);
    assert_eq!(
        bytes,
        Family::Responses.user_item("bob").unwrap().len()
            + 64 * answer.len()
            + Family::Responses.user_item("correction").unwrap().len()
    );
    let (_, bytes, count) = db.turn_usage("Alice", alice).unwrap();
    assert_eq!(
        (bytes, count),
        (Family::Responses.user_item("alice").unwrap().len(), 1)
    );
    assert_eq!(db.turn_usage("Alice", bob).unwrap_err().code, "stale_turn");
    db.finish(bob, None).unwrap();
    assert_eq!(db.turn_usage("Bob", bob).unwrap_err().code, "stale_turn");
}

#[test]
fn pending_counters_follow_every_transition_and_bound_admission() {
    let path = std::env::temp_dir().join(format!("agent-pending-{}.sqlite", std::process::id()));
    let _ = std::fs::remove_file(&path);
    let options = TurnOptions {
        delivery: Delivery::Queue,
        ..TurnOptions::default()
    };
    let steer = TurnOptions {
        delivery: Delivery::Steer,
        ..TurnOptions::default()
    };
    {
        let mut db = Database::initialize(Connection::open(&path).unwrap()).unwrap();
        db.create("Bob", Some("/synthetic"), binding()).unwrap();
        db.create("Ann", Some("/synthetic"), binding()).unwrap();
        assert_eq!(db.pending().unwrap(), (0, 0));
        let first = db
            .begin(
                "Bob",
                "r1",
                "first",
                true,
                &TurnOptions::default(),
                allow_provider,
            )
            .unwrap()
            .turn;
        // A running turn is not pending; queued ones count with their prompt bytes.
        assert_eq!(db.pending().unwrap(), (0, 0));
        let second = db
            .begin("Bob", "r2", "sécond", true, &options, allow_provider)
            .unwrap()
            .turn;
        let third = db
            .begin("Bob", "r3", "third", true, &steer, allow_provider)
            .unwrap()
            .turn;
        assert_eq!(db.pending().unwrap(), (2, 7 + 5));
        // A ready turn on another bot counts too; a duplicate does not.
        let ready = db
            .begin("Ann", "a1", "ready", false, &options, allow_provider)
            .unwrap();
        assert_eq!(ready.status, "ready");
        assert_eq!(db.pending().unwrap(), (3, 17));
        assert!(
            !db.begin("Ann", "a1", "ready", false, &options, allow_provider)
                .unwrap()
                .fresh
        );
        assert_eq!(db.pending().unwrap(), (3, 17));
        // Bounds: a count, then bytes; refusals write nothing.
        db.set_pending_limits(3, 0);
        let refused = db
            .begin("Ann", "a2", "more", false, &options, allow_provider)
            .unwrap_err();
        assert_eq!(refused.code, "pending_limit");
        db.set_pending_limits(0, 20);
        assert_eq!(
            db.begin("Ann", "a2", "four", false, &options, allow_provider)
                .unwrap_err()
                .code,
            "pending_limit"
        );
        assert_eq!(
            db.begin("Ann", "a2", "abc", false, &options, allow_provider)
                .unwrap()
                .status,
            "queued"
        );
        assert_eq!(db.pending().unwrap(), (4, 20));
        // A running submission is never refused by the pending bounds.
        db.create("Cid", Some("/synthetic"), binding()).unwrap();
        assert_eq!(
            db.begin("Cid", "c1", "now", true, &options, allow_provider)
                .unwrap()
                .status,
            "running"
        );
        db.set_pending_limits(0, 0);
        // Leaving: absorbed into the running turn, cancelled, started.
        assert_eq!(
            db.absorb(
                first,
                None,
                8 << 20,
                4096,
                ContextUsage::default(),
                false,
                true
            )
            .unwrap()
            .outcomes[0]
                .0,
            third
        );
        assert_eq!(db.pending().unwrap(), (3, 15));
        db.end_queued(second, &Error::new("cancelled")).unwrap();
        assert_eq!(db.pending().unwrap(), (2, 8));
        let (ann, turn) = db.next_ready().unwrap().unwrap();
        assert_eq!(ann, "Ann");
        db.start(turn, |_, _| Ok(())).unwrap();
        assert_eq!(db.pending().unwrap(), (1, 3));
        db.set_pending_limits(1, 0);
        assert_eq!(
            db.begin("Bob", "r4", "wait", true, &options, allow_provider)
                .unwrap_err()
                .code,
            "pending_limit"
        );
    }
    // The counters are recounted from the rows at open, after recovery has
    // ended the running turns.
    {
        let mut db = Database::initialize(Connection::open(&path).unwrap()).unwrap();
        assert_eq!(db.pending().unwrap(), (1, 3));
        // Recovery ended Bob's running turn; without a slot the next waits.
        db.begin("Bob", "r5", "again", false, &options, allow_provider)
            .unwrap();
        assert_eq!(db.pending().unwrap(), (2, 8));
    }
    std::fs::remove_file(path).unwrap();
}

#[test]
fn quoted_words_wait_with_their_turn_and_count_against_the_pending_bytes() {
    let path = std::env::temp_dir().join(format!(
        "agent-pending-quotes-{}.sqlite",
        std::process::id()
    ));
    let _ = std::fs::remove_file(&path);
    let quoting = |delivery| TurnOptions {
        delivery,
        quotes: vec![quote("Ann", 1, false)],
        ..TurnOptions::default()
    };
    let words = "x".repeat(4096);
    let waiting = {
        let mut db = Database::initialize(Connection::open(&path).unwrap()).unwrap();
        db.create("Ann", Some("/synthetic"), binding()).unwrap();
        db.create("Bob", Some("/synthetic"), binding()).unwrap();
        let said = db
            .begin(
                "Ann",
                "a1",
                &words,
                true,
                &TurnOptions::default(),
                allow_provider,
            )
            .unwrap()
            .turn;
        assert_eq!(said, 1);
        db.finish(said, None).unwrap();
        let running = db
            .begin(
                "Bob",
                "b1",
                "go",
                true,
                &TurnOptions::default(),
                allow_provider,
            )
            .unwrap()
            .turn;
        // An empty prompt quoting 4 KiB waits with those 4 KiB.
        db.begin(
            "Bob",
            "b2",
            "",
            true,
            &quoting(Delivery::Queue),
            allow_provider,
        )
        .unwrap();
        let (turns, bytes) = db.pending().unwrap();
        assert_eq!(turns, 1);
        assert!(bytes as usize > words.len(), "{bytes}");
        // The bound counts them too.
        db.set_pending_limits(0, bytes as usize + 1024);
        assert_eq!(
            db.begin(
                "Bob",
                "b3",
                "",
                true,
                &quoting(Delivery::Queue),
                allow_provider
            )
            .unwrap_err()
            .code,
            "pending_limit"
        );
        db.set_pending_limits(0, 0);
        // A steer quoting them leaves the count when it is taken in.
        db.begin(
            "Bob",
            "b4",
            "",
            true,
            &quoting(Delivery::Steer),
            allow_provider,
        )
        .unwrap();
        assert_eq!(db.pending().unwrap(), (2, 2 * bytes));
        db.absorb(
            running,
            None,
            8 << 20,
            4096,
            ContextUsage::default(),
            false,
            true,
        )
        .unwrap();
        assert_eq!(db.pending().unwrap(), (1, bytes));
        bytes
    };
    // Recounted at open: the waiting turn still holds its quoted words.
    let mut db = Database::initialize(Connection::open(&path).unwrap()).unwrap();
    assert_eq!(db.pending().unwrap(), (1, waiting));
    let (_, turn) = db.next_ready().unwrap().unwrap();
    db.start(turn, |_, _| Ok(())).unwrap();
    assert_eq!(db.pending().unwrap(), (0, 0));
    std::fs::remove_file(path).unwrap();
}

#[test]
fn one_absorption_takes_in_quoted_words_within_its_batch_bytes() {
    let mut db = db();
    db.create("Ann", Some("/synthetic"), binding()).unwrap();
    db.create("Bob", Some("/synthetic"), binding()).unwrap();
    let said = db
        .begin(
            "Ann",
            "a1",
            &"x".repeat(200 * 1024),
            true,
            &TurnOptions::default(),
            allow_provider,
        )
        .unwrap()
        .turn;
    db.finish(said, None).unwrap();
    let running = db
        .begin(
            "Bob",
            "b1",
            "go",
            true,
            &TurnOptions::default(),
            allow_provider,
        )
        .unwrap()
        .turn;
    // Two short steers quoting 200 KiB each: the first is always taken in,
    // the second waits for the next batch.
    let steer = TurnOptions {
        delivery: Delivery::Steer,
        quotes: vec![quote("Ann", said, false)],
        ..TurnOptions::default()
    };
    for id in ["s1", "s2"] {
        db.begin("Bob", id, "", true, &steer, allow_provider)
            .unwrap();
    }
    let absorb = |db: &mut Database, through| {
        db.absorb(
            running,
            through,
            8 << 20,
            4096,
            ContextUsage::default(),
            false,
            true,
        )
        .unwrap()
    };
    let absorbed = absorb(&mut db, None);
    assert_eq!(absorbed.outcomes.len(), 1);
    assert!(absorbed.next_through.is_some());
    let absorbed = absorb(&mut db, absorbed.next_through);
    assert_eq!(absorbed.outcomes.len(), 1);
    assert_eq!(db.pending().unwrap(), (0, 0));
}

#[test]
fn batched_history_items_validate_the_branch_and_bound_payloads() {
    let mut db = db();
    db.create("source", Some("/synthetic"), binding()).unwrap();
    let turn = db
        .begin(
            "source",
            "r1",
            "prompt",
            true,
            &TurnOptions::default(),
            allow_provider,
        )
        .unwrap()
        .turn;
    db.append(
        turn,
        vec![
            assistant(&"x".repeat(512 * 1024)),
            assistant("small"),
            assistant("last"),
        ],
        &[],
        None,
    )
    .unwrap();
    db.finish(turn, None).unwrap();
    db.fork("source", "branch", Fork::default()).unwrap();
    let refs = db.history_nodes("branch", None, 400, None, false).unwrap();
    let ids: Vec<i64> = refs["nodes"]
        .as_array()
        .unwrap()
        .iter()
        .map(|n| n["node"].as_i64().unwrap())
        .collect();
    let batch = db.history_items("branch", &ids).unwrap();
    for row in batch["items"].as_array().unwrap() {
        assert_eq!(
            row["item"],
            read_item(&db, "branch", row["node"].as_i64().unwrap()).unwrap()
        );
    }
    assert_eq!(batch["items"].as_array().unwrap().len(), ids.len());
    assert!(serde_json::to_vec(&batch).unwrap().len() < 1024 * 1024);
    assert!(db.history_items("branch", &[]).is_err());
    assert!(db.history_items("branch", &[ids[0]; 401]).is_err());
    assert!(db.history_items("branch", &[ids[0], ids[0]]).is_err());
    let other = db
        .begin(
            "source",
            "r2",
            "other branch",
            true,
            &TurnOptions::default(),
            allow_provider,
        )
        .unwrap()
        .turn;
    db.append(
        other,
        vec![
            assistant(&"y".repeat(512 * 1024)),
            assistant(&"z".repeat(512 * 1024)),
        ],
        &[],
        None,
    )
    .unwrap();
    db.finish(other, None).unwrap();
    let refs = db.history_nodes("source", None, 400, None, false).unwrap();
    let other_ids: Vec<i64> = refs["nodes"]
        .as_array()
        .unwrap()
        .iter()
        .map(|n| n["node"].as_i64().unwrap())
        .collect();
    assert!(db.history_items("branch", &[ids[0], other_ids[0]]).is_err());
    let batch = db.history_items("source", &other_ids).unwrap();
    assert_eq!(
        batch["items"].as_array().unwrap().len(),
        1,
        "stop before the second large item"
    );
    db.delete_bot("source").unwrap();
    assert_eq!(
        db.history_items("branch", &ids).unwrap()["items"]
            .as_array()
            .unwrap()
            .len(),
        ids.len()
    );
}

#[test]
fn the_context_note_lists_omitted_turns_newest_first_from_the_window_start() {
    let mut db = db();
    db.create("Bob", Some("/synthetic"), binding()).unwrap();
    for n in 1..=5 {
        converse(&mut db, "Bob", n);
    }
    // A prompt with a second line and one too long for the preview.
    let long = format!("{}\nsecond line", "w".repeat(300));
    let turn = db
        .begin(
            "Bob",
            "r6",
            &long,
            true,
            &TurnOptions::default(),
            allow_provider,
        )
        .unwrap()
        .turn;
    db.append(turn, vec![assistant("r6")], &[], None).unwrap();
    db.finish(turn, None).unwrap();
    converse(&mut db, "Bob", 7);
    // The window's start is turn 7's prompt node: everything before is omitted.
    let start: i64 = db.events("Bob", 0, 256).unwrap()["events"]
        .as_array()
        .unwrap()
        .iter()
        .rfind(|e| e["event"] == "accepted")
        .unwrap()["data"]["node"]
        .as_i64()
        .unwrap();
    let listed = db.omitted_turns(start, 3).unwrap();
    assert_eq!(listed.len(), 3);
    assert_eq!(
        (listed[0].0, listed[0].1.len()),
        (6, "w".repeat(120).len() + '…'.len_utf8())
    );
    assert!(listed[0].1.ends_with('…'));
    assert_eq!(listed[1], (5, "p5".into()));
    assert_eq!(listed[2], (4, "p4".into()));
    let all = db.omitted_turns(start, 48).unwrap();
    assert_eq!(
        all.iter().map(|(o, _)| *o).collect::<Vec<_>>(),
        vec![6, 5, 4, 3, 2, 1]
    );
    assert_eq!(all[5], (1, "p1".into()));
    // The first turn's window omits nothing.
    let first: i64 = db.events("Bob", 0, 256).unwrap()["events"]
        .as_array()
        .unwrap()
        .iter()
        .find(|e| e["event"] == "accepted")
        .unwrap()["data"]["node"]
        .as_i64()
        .unwrap();
    assert!(db.omitted_turns(first, 48).unwrap().is_empty());
}

#[test]
fn carry_forward_notes_are_versioned_by_result_node_and_forks_bind_by_checkpoint() {
    let mut db = db();
    db.create("Bob", Some("/synthetic"), binding()).unwrap();
    assert!(db.window("Bob", i64::MAX, i64::MAX).unwrap().is_none());
    let noted = |db: &mut Database, n: usize, text: &str| -> i64 {
        let turn = db
            .begin(
                "Bob",
                &format!("n{n}"),
                "work",
                true,
                &TurnOptions::default(),
                allow_provider,
            )
            .unwrap()
            .turn;
        let call = ToolCall {
            custom: false,
            name: "note".into(),
            call_id: format!("note-{n}"),
            arguments: json!({"text":text}).to_string(),
        };
        db.append(turn, vec![], std::slice::from_ref(&call), None)
            .unwrap();
        db.tool_start(turn, &call).unwrap();
        let outcome = Outcome {
            output: "{}".into(),
            artifacts: Vec::new(),
            parts: Vec::new(),
            note: Some(text.to_owned()),
            failed: false,
            files: Default::default(),
        };
        let (_, entry) = db.tool_finish(turn, &call.call_id, &outcome).unwrap();
        let version = entry["data"]["note"].as_i64().unwrap();
        db.append(turn, vec![assistant("ok")], &[], None).unwrap();
        db.finish(turn, None).unwrap();
        version
    };
    let first = noted(&mut db, 1, "rule: end files with the marker");
    assert_eq!(db.inspect("Bob").unwrap().note, Some(first));
    assert_eq!(
        db.window("Bob", i64::MAX, i64::MAX).unwrap().unwrap().note,
        Some((first, "rule: end files with the marker".into()))
    );
    // A fork at the current head carries the note; one at an earlier
    // checkpoint carries the version that existed there.
    let checkpoint = db.inspect("Bob").unwrap().head.unwrap();
    let second = noted(&mut db, 2, "rule, plus: tests must pass");
    assert!(second > first);
    assert_eq!(db.inspect("Bob").unwrap().note, Some(second));
    db.fork(
        "Bob",
        "Early",
        Fork {
            checkpoint: Some(checkpoint),
            workspace: Some("/synthetic"),
            budget_tokens: None,
            ..Fork::default()
        },
    )
    .unwrap();
    assert_eq!(db.inspect("Early").unwrap().note, Some(first));
    db.fork(
        "Bob",
        "Late",
        Fork {
            checkpoint: None,
            workspace: Some("/synthetic"),
            budget_tokens: None,
            ..Fork::default()
        },
    )
    .unwrap();
    assert_eq!(db.inspect("Late").unwrap().note, Some(second));
    // Clearing is a version too: the window shows nothing, a fork before it still sees the note.
    let cleared = noted(&mut db, 3, "");
    assert_eq!(db.inspect("Bob").unwrap().note, Some(cleared));
    assert_eq!(
        db.window("Bob", i64::MAX, i64::MAX).unwrap().unwrap().note,
        Some((cleared, String::new()))
    );
    // Deleting a fork frees its suffix; the shared prefix's notes stay for the others.
    db.delete_bot("Late").unwrap();
    assert_eq!(db.inspect("Early").unwrap().note, Some(first));
    assert_eq!(
        db.window("Early", i64::MAX, i64::MAX)
            .unwrap()
            .unwrap()
            .note
            .unwrap()
            .0,
        first
    );
    // Deleting the source frees its exclusive suffix, notes included, and leaves Early whole.
    db.delete_bot("Bob").unwrap();
    assert_eq!(
        db.window("Early", i64::MAX, i64::MAX)
            .unwrap()
            .unwrap()
            .note
            .unwrap()
            .0,
        first
    );
}

#[test]
fn fork_prompt_views_survive_source_deletion_and_reopen() {
    for family in [Family::Responses, Family::Anthropic] {
        let path = std::env::temp_dir().join(format!(
            "agent-fork-prompts-{}-{}.sqlite",
            std::process::id(),
            family.name()
        ));
        let prompts = [
            "Keep \"quotes\" and \\ paths.\nSecond line.",
            "Unicode: λ🦀",
            "",
            "latest",
        ];
        let expected;
        let cut;
        {
            let mut db = Database::initialize(Connection::open(&path).unwrap()).unwrap();
            db.create(
                "Bob",
                Some("/synthetic"),
                Binding {
                    family,
                    ..binding()
                },
            )
            .unwrap();
            for (n, prompt) in prompts.iter().enumerate() {
                let turn = db
                    .begin(
                        "Bob",
                        &n.to_string(),
                        prompt,
                        true,
                        &TurnOptions::default(),
                        allow_provider,
                    )
                    .unwrap()
                    .turn;
                let item = match family {
                    Family::Responses => assistant(&"answer ".repeat(100)),
                    Family::Anthropic => serde_json::to_vec(&json!({"role":"assistant","content":[{"type":"text","text":"answer ".repeat(100)}]})).unwrap().into(),
                };
                db.append(turn, vec![item], &[], None).unwrap();
                db.finish(turn, None).unwrap();
            }
            let plan = compaction_plan(&db, "Bob", 1, 4096, 256).unwrap().unwrap();
            assert_eq!(plan.covered, (1, 3));
            expected = prompts[..3]
                .iter()
                .enumerate()
                .map(|(n, p)| (n as i64 + 1, (*p).to_owned()))
                .collect::<Vec<_>>();
            assert_eq!(plan.prompts, expected);
            cut = plan.cut;
            db.fork(
                "Bob",
                "Alice",
                Fork {
                    checkpoint: None,
                    workspace: Some("/synthetic"),
                    budget_tokens: None,
                    ..Fork::default()
                },
            )
            .unwrap();
            db.delete_bot("Bob").unwrap();
        }
        {
            let mut db = Database::initialize(Connection::open(&path).unwrap()).unwrap();
            let plan = compaction_plan(&db, "Alice", 1, 4096, 256)
                .unwrap()
                .unwrap();
            assert_eq!(plan.covered, (1, 3));
            assert_eq!(plan.prompts, expected);
            assert_eq!(
                db.omitted_turns(cut, 3).unwrap(),
                vec![
                    (3, "".into()),
                    (2, prompts[1].into()),
                    (1, format!("{}…", prompts[0].lines().next().unwrap())),
                ]
            );
            db.compact(
                "Alice",
                &plan,
                "retained summary",
                None,
                0,
                agent_runtime::store::ContextUsage {
                    bytes: 4096,
                    items: 256,
                },
                Value::Null,
            )
            .unwrap();
            let view = db
                .window("Alice", 4096, 256)
                .unwrap()
                .unwrap()
                .compaction
                .unwrap();
            assert_eq!(view.covered, (1, 3));
            assert_eq!(view.prompts, expected);
            // Compaction leaves the original native items retrievable.
            let raw = db.items_by_ids(&plan.ids, 0, 0).unwrap();
            let items: Vec<Value> =
                serde_json::from_slice(&[b"[", &raw[..], b"]"].concat()).unwrap();
            assert_eq!(items.len(), 6);
            for (item, prompt) in items.iter().step_by(2).zip(prompts) {
                assert_eq!(item["content"][0]["text"], prompt);
            }
        }
        std::fs::remove_file(path).unwrap();
    }
}

#[test]
fn compaction_prompt_metadata_stays_bounded_across_planning_and_merging() {
    let mut db = db();
    db.create("Bob", Some("/synthetic"), binding()).unwrap();
    // Empty prompts are valid protocol input. Even their ordinals and empty
    // strings occupy memory and appear in the summary prefix.
    for n in 0..1800 {
        let turn = db
            .begin(
                "Bob",
                &n.to_string(),
                "",
                true,
                &TurnOptions::default(),
                allow_provider,
            )
            .unwrap()
            .turn;
        db.finish(turn, None).unwrap();
        if (n + 1) % 600 != 0 {
            continue;
        }
        let plan = compaction_plan(&db, "Bob", 1, i64::MAX, i64::MAX)
            .unwrap()
            .unwrap();
        let cost = |prompts: &[(i64, String)]| {
            prompts
                .iter()
                .map(|(_, text)| std::mem::size_of::<(i64, String)>() + text.len())
                .sum::<usize>()
        };
        assert!(cost(&plan.prompts) <= Database::COMPACTION_PROMPTS_BYTES);
        assert_eq!(plan.prompts.last().unwrap().0, n as i64);
        db.compact(
            "Bob",
            &plan,
            "summary",
            None,
            0,
            agent_runtime::store::ContextUsage {
                bytes: 4096,
                items: 256,
            },
            Value::Null,
        )
        .unwrap();
        let view = db
            .window("Bob", i64::MAX, i64::MAX)
            .unwrap()
            .unwrap()
            .compaction
            .unwrap();
        assert!(cost(&view.prompts) <= Database::COMPACTION_PROMPTS_BYTES);
        assert_eq!(view.prompts.first().unwrap(), &(1, String::new()));
        assert_eq!(view.prompts.last().unwrap(), &(n as i64, String::new()));
        assert_eq!(view.covered, (1, n as i64));
        // The omitted excerpt is only a view; the original empty prompt is
        // still present in its native history item.
        assert_eq!(db.history_read("Bob", 300, 0, 65536).unwrap()["items"], 1);
    }
}

#[test]
fn compaction_summarizes_older_turns_keeps_prompts_and_versions_bind_forks() {
    let mut db = db();
    db.create(
        "Bob",
        Some("/synthetic"),
        Binding {
            compaction_instructions: Some("Summarize."),
            ..binding()
        },
    )
    .unwrap();
    assert_eq!(
        db.inspect("Bob")
            .unwrap()
            .compaction_instructions
            .as_deref(),
        Some("Summarize.")
    );
    for n in 1..=3 {
        converse(&mut db, "Bob", n);
    }
    let checkpoint_early = db.inspect("Bob").unwrap().head.unwrap();
    for n in 4..=6 {
        converse(&mut db, "Bob", n);
    }
    let before = db.unsummarized_bytes("Bob").unwrap();
    assert!(before > 0);
    // Keep at least one byte verbatim: the cut lands at the newest turn's
    // prompt, and everything older is the span.
    let plan = compaction_plan(&db, "Bob", 1, i64::MAX, i64::MAX)
        .unwrap()
        .unwrap();
    assert_eq!(plan.covered, (1, 5));
    assert_eq!(
        plan.prompts
            .iter()
            .map(|(o, p)| (*o, p.as_str()))
            .collect::<Vec<_>>(),
        vec![(1, "p1"), (2, "p2"), (3, "p3"), (4, "p4"), (5, "p5")]
    );
    assert_eq!(plan.ids.len(), 10);
    assert_eq!(plan.ids.len(), plan.sizes.len());
    assert!(plan.previous_summary.is_none());
    let checkpoint_before = db.inspect("Bob").unwrap().head.unwrap();
    let event = db
        .compact(
            "Bob",
            &plan,
            "summary one",
            None,
            0,
            agent_runtime::store::ContextUsage {
                bytes: 4096,
                items: 256,
            },
            Value::Null,
        )
        .unwrap();
    assert_eq!(event["event"], "compacted");
    assert_eq!(event["data"]["covered_turns"], json!([1, 5]));
    let bot = db.inspect("Bob").unwrap();
    assert_eq!(bot.compaction, Some(checkpoint_before));
    // The window now starts at the cut and carries the view.
    let window = db.window("Bob", i64::MAX, i64::MAX).unwrap().unwrap();
    assert_eq!(window.omitted_turns, 5);
    let view = window.compaction.unwrap();
    assert_eq!(
        (view.version, view.summary.as_str(), view.covered),
        (checkpoint_before, "summary one", (1, 5))
    );
    assert_eq!(view.prompts.len(), 5);
    assert!(db.unsummarized_bytes("Bob").unwrap() < before);
    // Nothing older than the cut is left: no second compaction yet.
    assert!(
        compaction_plan(&db, "Bob", 1, i64::MAX, i64::MAX)
            .unwrap()
            .is_none()
    );
    // More turns, then a second compaction merges from the previous summary.
    for n in 7..=9 {
        converse(&mut db, "Bob", n);
    }
    let second_version = db.inspect("Bob").unwrap().head.unwrap();
    let second = compaction_plan(&db, "Bob", 1, i64::MAX, i64::MAX)
        .unwrap()
        .unwrap();
    assert_eq!(second.covered, (6, 8));
    assert_eq!(second.previous_summary.as_deref(), Some("summary one"));
    assert!(second.ids.len() < 10);
    db.compact(
        "Bob",
        &second,
        "summary two",
        None,
        0,
        agent_runtime::store::ContextUsage {
            bytes: 4096,
            items: 256,
        },
        Value::Null,
    )
    .unwrap();
    let view = db
        .window("Bob", i64::MAX, i64::MAX)
        .unwrap()
        .unwrap()
        .compaction
        .unwrap();
    // The second stands for everything since the first: its coverage starts
    // at turn 1 and the kept prompts carry over.
    assert_eq!(
        (view.summary.as_str(), view.covered),
        ("summary two", (1, 8))
    );
    assert_eq!(view.prompts.len(), 8);
    // Forks bind to the newest compaction whose cut is at or before their
    // checkpoint: its summary covers only turns the fork shares. Before the
    // first cut there is none; after it, the first; at the head, the second.
    db.fork(
        "Bob",
        "Early",
        Fork {
            checkpoint: Some(checkpoint_early),
            workspace: Some("/synthetic"),
            budget_tokens: None,
            ..Fork::default()
        },
    )
    .unwrap();
    assert_eq!(db.inspect("Early").unwrap().compaction, None);
    db.fork(
        "Bob",
        "Mid",
        Fork {
            checkpoint: Some(checkpoint_before),
            workspace: Some("/synthetic"),
            budget_tokens: None,
            ..Fork::default()
        },
    )
    .unwrap();
    assert_eq!(
        db.inspect("Mid").unwrap().compaction,
        Some(checkpoint_before)
    );
    assert_eq!(
        db.inspect("Early")
            .unwrap()
            .compaction_instructions
            .as_deref(),
        Some("Summarize.")
    );
    db.fork(
        "Bob",
        "Late",
        Fork {
            checkpoint: None,
            workspace: Some("/synthetic"),
            budget_tokens: None,
            ..Fork::default()
        },
    )
    .unwrap();
    assert_eq!(db.inspect("Late").unwrap().compaction, Some(second_version));
    // Deletion frees a fork's suffix and the source's exclusive versions,
    // leaving the fork that still points at one whole.
    db.delete_bot("Early").unwrap();
    db.delete_bot("Mid").unwrap();
    db.delete_bot("Bob").unwrap();
    assert_eq!(
        db.window("Late", i64::MAX, i64::MAX)
            .unwrap()
            .unwrap()
            .compaction
            .unwrap()
            .version,
        second_version
    );
    db.delete_bot("Late").unwrap();
}

#[test]
fn independent_branches_can_compact_the_same_cut_without_rewriting_each_other() {
    let mut db = db();
    db.create("Bob", Some("/synthetic"), binding()).unwrap();
    for n in 1..=5 {
        converse(&mut db, "Bob", n);
    }
    let shared = db.inspect("Bob").unwrap().head.unwrap();
    db.fork(
        "Bob",
        "Alice",
        Fork {
            checkpoint: None,
            workspace: Some("/synthetic"),
            budget_tokens: None,
            ..Fork::default()
        },
    )
    .unwrap();
    converse(&mut db, "Bob", 6);
    converse(&mut db, "Alice", 7);
    let p = compaction_plan(&db, "Bob", 250, 4096, 256)
        .unwrap()
        .unwrap();
    let q = compaction_plan(&db, "Alice", 250, 4096, 256)
        .unwrap()
        .unwrap();
    assert_eq!(p.cut, q.cut);
    db.compact(
        "Bob",
        &p,
        "Bob summary",
        None,
        0,
        agent_runtime::store::ContextUsage {
            bytes: 4096,
            items: 256,
        },
        Value::Null,
    )
    .unwrap();
    db.compact(
        "Alice",
        &q,
        "Alice summary",
        None,
        0,
        agent_runtime::store::ContextUsage {
            bytes: 4096,
            items: 256,
        },
        Value::Null,
    )
    .unwrap();
    let b = db.window("Bob", 4096, 256).unwrap().unwrap();
    let a = db.window("Alice", 4096, 256).unwrap().unwrap();
    assert_ne!(
        b.compaction.as_ref().unwrap().version,
        a.compaction.as_ref().unwrap().version
    );
    assert_eq!(b.compaction.as_ref().unwrap().summary, "Bob summary");
    assert_eq!(a.compaction.as_ref().unwrap().summary, "Alice summary");
    db.fork(
        "Bob",
        "Before",
        Fork {
            checkpoint: Some(shared),
            workspace: Some("/synthetic"),
            budget_tokens: None,
            ..Fork::default()
        },
    )
    .unwrap();
    assert!(db.inspect("Before").unwrap().compaction.is_none());
    db.fork(
        "Bob",
        "After",
        Fork {
            checkpoint: None,
            workspace: Some("/synthetic"),
            budget_tokens: None,
            ..Fork::default()
        },
    )
    .unwrap();
    assert_eq!(db.window("After", 4096, 256).unwrap().unwrap().ids, b.ids);
    db.delete_bot("Bob").unwrap();
    assert_eq!(
        db.window("After", 4096, 256)
            .unwrap()
            .unwrap()
            .compaction
            .unwrap()
            .summary,
        "Bob summary"
    );
}

#[test]
fn oversized_backlogs_are_summarized_oldest_first_in_bounded_spans() {
    let mut db = db();
    db.create("Bob", Some("/synthetic"), binding()).unwrap();
    for n in 1..=100 {
        converse(&mut db, "Bob", n);
    }
    let total = db.unsummarized_bytes("Bob").unwrap();
    let (max_bytes, max_items) = (1024, 4096);
    assert!(total > max_bytes);
    // Each step covers the oldest whole turns not yet summarized whose
    // summarizer request fits the budget, and one round later the next step
    // takes over from its cut.
    let (mut covered_to, mut steps) = (0, 0);
    loop {
        let plan = compaction_plan(&db, "Bob", 1, max_bytes, max_items)
            .unwrap()
            .unwrap();
        let bytes: i64 = plan.sizes.iter().map(|s| *s as i64).sum();
        if !plan.catch_up {
            // Caught up: the ordinary plan keeps its verbatim tail.
            let event = db
                .compact(
                    "Bob",
                    &plan,
                    "summary",
                    None,
                    0,
                    agent_runtime::store::ContextUsage {
                        bytes: 4096,
                        items: 256,
                    },
                    Value::Null,
                )
                .unwrap();
            assert_eq!(event["data"]["catch_up"], false);
            assert_eq!(plan.covered.0, covered_to + 1);
            break;
        }
        let (head_frame, tail_frame) = CompactionPlan::frame(
            Family::Responses,
            plan.previous_summary.as_deref(),
            plan.summary_bytes,
        )
        .unwrap();
        let request = bytes
            + plan.ids.len().saturating_sub(1) as i64
            + head_frame.len() as i64
            + tail_frame.len() as i64;
        assert!(
            request <= max_bytes,
            "step {steps} summarized {bytes} bytes"
        );
        assert!(
            bytes > max_bytes / 2,
            "step {steps} summarized only {bytes} bytes"
        );
        assert_eq!(plan.covered.0, covered_to + 1);
        assert_eq!(
            plan.prompts.first().unwrap().1,
            format!("p{}", covered_to + 1)
        );
        let event = db
            .compact(
                "Bob",
                &plan,
                &format!("summary {steps}"),
                None,
                0,
                agent_runtime::store::ContextUsage {
                    bytes: 4096,
                    items: 256,
                },
                Value::Null,
            )
            .unwrap();
        assert_eq!(event["data"]["catch_up"], true);
        assert_eq!(event["data"]["covered_turns"], json!([1, plan.covered.1]));
        covered_to = plan.covered.1;
        steps += 1;
        assert!(steps < 64, "catch-up does not converge");
        // Later summaries merge the earlier ones.
        converse(&mut db, "Bob", 100 + steps);
        assert_eq!(
            compaction_plan(&db, "Bob", 1, max_bytes, max_items)
                .unwrap()
                .unwrap()
                .previous_summary,
            Some(format!("summary {}", steps - 1))
        );
    }
    assert!(steps > 1);
    // The view covers every turn since the first; the transcript is intact.
    let view = db
        .window("Bob", max_bytes, max_items)
        .unwrap()
        .unwrap()
        .compaction
        .unwrap();
    assert_eq!(view.covered.0, 1);
    assert_eq!(
        db.window("Bob", i64::MAX, i64::MAX)
            .unwrap()
            .unwrap()
            .ids
            .len() as i64
            + 2 * (view.covered.1),
        2 * (100 + steps as i64)
    );
    assert_eq!(db.history_read("Bob", 1, 0, 65536).unwrap()["items"], 2);
}

#[test]
fn catch_up_is_bounded_by_items_and_rejects_a_turn_larger_than_the_budget() {
    let mut db = db();
    db.create("Bob", Some("/synthetic"), binding()).unwrap();
    for n in 1..=100 {
        converse(&mut db, "Bob", n);
    }
    // Items: two per turn, and the request to write takes one of 16.
    let plan = compaction_plan(&db, "Bob", 1, i64::MAX, 16)
        .unwrap()
        .unwrap();
    assert_eq!(plan.covered, (1, 7));
    assert_eq!(plan.ids.len(), 14);
    // The walk's pieces do not change the plan, whatever their size.
    for piece in [1, 7, 4096] {
        let Some(Planning::CatchUp(mut walk)) = db
            .compaction_plan("Bob", 1, i64::MAX, i64::MAX, 16, false)
            .unwrap()
        else {
            panic!("expected a catch-up walk");
        };
        while !walk.done() {
            db.catch_up_piece(&mut walk, piece).unwrap();
        }
        let split = db.catch_up_plan("Bob", walk).unwrap().unwrap();
        assert_eq!(
            (split.cut, &split.ids, &split.sizes, &split.prompts),
            (plan.cut, &plan.ids, &plan.sizes, &plan.prompts)
        );
    }
    // No whole turn fits: nothing is planned, nothing moves.
    assert_eq!(
        compaction_plan(&db, "Bob", 1, 16, 4096).unwrap_err().code,
        "compaction_span_limit"
    );
    assert_eq!(
        compaction_plan(&db, "Bob", 1, i64::MAX, 1)
            .unwrap_err()
            .code,
        "compaction_span_limit"
    );
    assert!(db.inspect("Bob").unwrap().compaction.is_none());
    assert_eq!(
        db.window("Bob", i64::MAX, i64::MAX)
            .unwrap()
            .unwrap()
            .ids
            .len(),
        200
    );
}

#[test]
fn compaction_cut_migrates_without_replacing_the_recorded_summary() {
    let path = std::env::temp_dir().join(format!(
        "agent-compaction-migrate-{}.sqlite",
        std::process::id()
    ));
    let _ = std::fs::remove_file(&path);
    let (version, cut);
    {
        let mut db = Database::initialize(Connection::open(&path).unwrap()).unwrap();
        db.create("Bob", Some("/synthetic"), binding()).unwrap();
        for n in 1..=6 {
            converse(&mut db, "Bob", n);
        }
        let plan = compaction_plan(&db, "Bob", 1, 4096, 256).unwrap().unwrap();
        cut = plan.cut;
        version = db.inspect("Bob").unwrap().head.unwrap();
        db.compact(
            "Bob",
            &plan,
            "retained summary",
            None,
            0,
            agent_runtime::store::ContextUsage {
                bytes: 4096,
                items: 256,
            },
            Value::Null,
        )
        .unwrap();
    }
    {
        let conn = Connection::open(&path).unwrap();
        conn.pragma_update(None, "foreign_keys", "OFF").unwrap();
        conn.execute("UPDATE compactions SET node=?", [cut])
            .unwrap();
        conn.execute("UPDATE bots SET compaction=?", [cut]).unwrap();
        conn.execute_batch("DROP INDEX compactions_cut; ALTER TABLE compactions DROP COLUMN cut; PRAGMA user_version=23;")
            .unwrap();
    }
    {
        let mut db = Database::initialize(Connection::open(&path).unwrap()).unwrap();
        let w = db.window("Bob", 4096, 256).unwrap().unwrap();
        assert_eq!(w.ids[0], cut);
        assert_eq!(w.compaction.unwrap().summary, "retained summary");
        converse(&mut db, "Bob", 7);
        let plan = compaction_plan(&db, "Bob", 1, 4096, 256).unwrap().unwrap();
        db.compact(
            "Bob",
            &plan,
            "new summary",
            None,
            0,
            agent_runtime::store::ContextUsage {
                bytes: 4096,
                items: 256,
            },
            Value::Null,
        )
        .unwrap();
        assert!(db.inspect("Bob").unwrap().compaction.unwrap() > version);
        db.delete_bot("Bob").unwrap();
    }
    std::fs::remove_file(path).unwrap();
}

#[test]
fn merged_schema_preserves_stores_from_both_published_branches() {
    for lineage in [true, false] {
        let path = std::env::temp_dir().join(format!(
            "agent-schema-join-{}-{lineage}.sqlite",
            std::process::id()
        ));
        let (id, parent_id, head, item);
        {
            let mut db = Database::initialize(Connection::open(&path).unwrap()).unwrap();
            let parent = db
                .create("Parent", Some("/synthetic"), binding())
                .unwrap()
                .0;
            parent_id = parent.bot_id;
            let mut b = binding();
            b.created_by = Some("Parent");
            b.created_by_id = Some(parent.bot_id);
            b.compaction_instructions = Some("preserve decisions");
            db.create("Bob", Some("/synthetic"), b).unwrap();
            converse(&mut db, "Bob", 1);
            let bot = db.inspect("Bob").unwrap();
            id = bot.bot_id;
            head = bot.head.unwrap();
            item = read_item(&db, "Bob", head).unwrap();
        }
        {
            let conn = rusqlite::Connection::open(&path).unwrap();
            if lineage {
                conn.execute_batch(
                    "DROP INDEX bots_note; DROP INDEX bots_compaction;
                    ALTER TABLE bots DROP COLUMN note;
                    ALTER TABLE bots DROP COLUMN compaction;
                    ALTER TABLE bots DROP COLUMN compaction_instructions;
                    ALTER TABLE bots DROP COLUMN compaction_model;
                    DROP TABLE compactions; DROP TABLE notes;
                    PRAGMA user_version=23;",
                )
                .unwrap();
            } else {
                conn.execute(
                    "INSERT INTO notes(node, text) VALUES (?, 'retained note')",
                    [head],
                )
                .unwrap();
                conn.execute("UPDATE bots SET note=? WHERE name='Bob'", [head])
                    .unwrap();
                conn.execute_batch(
                    "ALTER TABLE bots DROP COLUMN created_by;
                    ALTER TABLE bots DROP COLUMN created_by_id;
                    PRAGMA user_version=24;",
                )
                .unwrap();
            }
        }
        for _ in 0..2 {
            let mut db = Database::initialize(Connection::open(&path).unwrap()).unwrap();
            let bot = db.inspect("Bob").unwrap();
            assert_eq!((bot.bot_id, bot.head), (id, Some(head)));
            assert_eq!(read_item(&db, "Bob", head).unwrap(), item);
            if lineage {
                assert_eq!(bot.created_by.as_deref(), Some("Parent"));
                assert_eq!(bot.created_by_id, Some(parent_id));
                assert_eq!(bot.compaction_instructions, None);
            } else {
                assert_eq!(bot.created_by_id, None);
                assert_eq!(
                    bot.compaction_instructions.as_deref(),
                    Some("preserve decisions")
                );
                assert_eq!(
                    db.window("Bob", i64::MAX, i64::MAX).unwrap().unwrap().note,
                    Some((head, "retained note".into()))
                );
            }
            let fork = db.fork("Bob", "Fork", Fork::default()).unwrap().0;
            assert_eq!(fork.head, bot.head);
            assert_eq!(fork.compaction_instructions, bot.compaction_instructions);
            db.delete_bot("Fork").unwrap();
        }
        let conn = rusqlite::Connection::open(&path).unwrap();
        assert_eq!(
            conn.query_row("PRAGMA user_version", [], |r| r.get::<_, i32>(0))
                .unwrap(),
            Database::SCHEMA
        );
        drop(conn);
        std::fs::remove_file(path).unwrap();
    }
}

#[test]
fn oversized_history_items_do_not_hide_the_rest_of_the_batch() {
    let mut db = db();
    db.create("Bob", Some("/synthetic"), binding()).unwrap();
    let turn = db
        .begin(
            "Bob",
            "r1",
            "prompt",
            true,
            &TurnOptions::default(),
            allow_provider,
        )
        .unwrap()
        .turn;
    db.append(
        turn,
        vec![
            assistant("before"),
            assistant(&"x".repeat(2 * 1024 * 1024)),
            assistant("after"),
        ],
        &[],
        None,
    )
    .unwrap();
    db.finish(turn, None).unwrap();
    let refs = db.history_nodes("Bob", None, 400, None, false).unwrap();
    let ids: Vec<i64> = refs["nodes"]
        .as_array()
        .unwrap()
        .iter()
        .map(|n| n["node"].as_i64().unwrap())
        .collect();
    let batch = db.history_items("Bob", &ids).unwrap();
    let rows = batch["items"].as_array().unwrap();
    assert_eq!(rows.len(), ids.len());
    assert_eq!(
        rows.iter()
            .filter(|r| r["error"] == "item_too_large")
            .count(),
        1
    );
    for row in rows.iter().filter(|r| r["error"].is_null()) {
        assert_eq!(
            row["item"],
            read_item(&db, "Bob", row["node"].as_i64().unwrap()).unwrap()
        );
    }
    assert!(serde_json::to_vec(&batch).unwrap().len() < 768 * 1024);
}

const THINKING: &[u8] = br#"{"content":[{"type":"thinking","thinking":"plan","signature":"s"},{"type":"text","text":"answer"}],"role":"assistant"}"#;

#[test]
fn schema_27_migrates_cache_lineage_and_thinking_sizes() {
    let path =
        std::env::temp_dir().join(format!("agent-cache-lineage-{}.sqlite", std::process::id()));
    let _ = std::fs::remove_file(&path);
    {
        let mut db = Database::initialize(Connection::open(&path).unwrap()).unwrap();
        db.create("Bob", Some("/synthetic"), binding()).unwrap();
        converse(&mut db, "Bob", 1);
        let turn = db
            .begin(
                "Bob",
                "r2",
                "p2",
                true,
                &TurnOptions::default(),
                allow_provider,
            )
            .unwrap()
            .turn;
        db.append(turn, vec![Bytes::from_static(THINKING)], &[], None)
            .unwrap();
        db.finish(turn, None).unwrap();
    }
    {
        let conn = Connection::open(&path).unwrap();
        conn.execute_batch(
            "ALTER TABLE bots DROP COLUMN cache_bot; ALTER TABLE bots DROP COLUMN thinking_prefix;
             ALTER TABLE bots DROP COLUMN thinking_floor; ALTER TABLE nodes DROP COLUMN thinking;
             PRAGMA user_version=26;",
        )
        .unwrap();
    }
    let mut db = Database::initialize(Connection::open(&path).unwrap()).unwrap();
    // Stored thinking is measured once, so a request knows its stripped
    // length before reading, and reads it back without the block.
    let window = db.window("Bob", 1 << 20, 1024).unwrap().unwrap();
    let stripped = agent_runtime::store::thinking_bytes(THINKING);
    assert!(stripped > 0);
    assert_eq!(
        window.thinking.iter().map(|&t| t as usize).sum::<usize>(),
        stripped
    );
    let full = db.items_by_ids(&window.ids, 0, 0).unwrap();
    let sent = db.items_by_ids(&window.ids, i64::MAX, 0).unwrap();
    assert_eq!(full.len() - sent.len(), stripped);
    assert!(!String::from_utf8(sent).unwrap().contains("\"thinking\""));
    let bob = db.inspect("Bob").unwrap();
    assert_eq!((bob.cache_bot, bob.cache_bot()), (None, bob.bot_id));
    // A fork repeats its source's prefix, so it shares the source's cache,
    // and so does a fork of that fork.
    db.fork("Bob", "Alice", Fork::default()).unwrap();
    db.fork("Alice", "Ann", Fork::default()).unwrap();
    for name in ["Alice", "Ann"] {
        assert_eq!(db.inspect(name).unwrap().cache_bot(), bob.bot_id, "{name}");
    }
    drop(db);
    std::fs::remove_file(path).unwrap();
}

#[test]
fn a_store_keeps_its_identity_and_forks_inherit_a_fallback() {
    let dir = std::env::temp_dir().join(format!("agent-identity-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join("state.sqlite");
    let identity = {
        let mut db = Database::initialize(Connection::open(&path).unwrap()).unwrap();
        let identity = db.store_identity().unwrap();
        // None unless the client names one; named, forks keep it.
        let plain = db.create("plain", None, binding()).unwrap().0;
        assert_eq!(plain.fallback, None);
        let mut asked = binding();
        asked.fallback = Some("anthropic/claude-opus-4-8");
        let bot = db.create("bot", None, asked).unwrap().0;
        assert_eq!(bot.fallback.as_deref(), Some("anthropic/claude-opus-4-8"));
        assert_eq!(
            (db.fork("bot", "fork", Fork::default()).unwrap().0.fallback).as_deref(),
            Some("anthropic/claude-opus-4-8")
        );
        assert_eq!(
            db.fork("plain", "plain-fork", Fork::default())
                .unwrap()
                .0
                .fallback,
            None
        );
        identity
    };
    // The identity is the file's, not the process's or the open's.
    let db = Database::initialize(Connection::open(&path).unwrap()).unwrap();
    assert_eq!(db.store_identity().unwrap(), identity);
    let other = Database::initialize(Connection::open(dir.join("other.sqlite")).unwrap()).unwrap();
    assert_ne!(other.store_identity().unwrap(), identity);
    drop((db, other));
    std::fs::remove_dir_all(dir).unwrap();
}

/// One answered model round in a running turn: the model's call, then its
/// result. Returns the call's node and the result's.
fn exchange(db: &mut Database, turn: i64, call_id: &str, output: &str) -> (i64, i64) {
    let call = ToolCall {
        custom: false,
        name: "shell".into(),
        call_id: call_id.into(),
        arguments: "{}".into(),
    };
    let item = json!({"type":"function_call","call_id":call_id,"name":"shell","arguments":"{}"});
    let entries = db
        .append(
            turn,
            vec![serde_json::to_vec(&item).unwrap().into()],
            std::slice::from_ref(&call),
            None,
        )
        .unwrap();
    let asked = entries
        .iter()
        .find_map(|entry| entry["data"]["node"].as_i64())
        .unwrap();
    db.tool_start(turn, &call).unwrap();
    let (_, entry) = db.tool_finish(turn, call_id, &result(output)).unwrap();
    (asked, entry["data"]["node"].as_i64().unwrap())
}
fn lines(tag: usize, count: usize) -> String {
    (0..count)
        .map(|n| format!("result {tag} line {n}\n"))
        .collect()
}
/// A window's items as the runtime sends them, checked against the sizes
/// the window counted for them: each one's weight, and what its pictures
/// add beyond it.
fn sent(db: &Database, window: &agent_runtime::store::Window) -> Vec<Value> {
    let mut items = Vec::new();
    for ((id, size), pictured) in window.ids.iter().zip(&window.sizes).zip(&window.pictured) {
        let bytes = db.items_by_ids(&[*id], 0, window.elided).unwrap();
        assert_eq!(
            bytes.len() as i64,
            i64::from(*size) + i64::from(*pictured),
            "node {id}"
        );
        items.push(serde_json::from_slice(&bytes).unwrap());
    }
    let joined = db.items_by_ids(&window.ids, 0, window.elided).unwrap();
    let pictured: i64 = window.pictured.iter().map(|&p| i64::from(p)).sum();
    assert_eq!(
        joined.len() as i64 + 1,
        window.item_bytes + pictured + window.ids.len() as i64
    );
    items
}

/// A picture of `side` × `side` pixels of noise: large for its size, as
/// a photograph is.
fn noise(side: u32) -> agent_runtime::codec::Image {
    use base64::Engine;
    let mut state = 0x2545_f491_u32;
    let picture = image::RgbImage::from_fn(side, side, |_, _| {
        state ^= state << 13;
        state ^= state >> 17;
        state ^= state << 5;
        image::Rgb([state as u8, (state >> 8) as u8, (state >> 16) as u8])
    });
    let mut png = Vec::new();
    picture
        .write_to(&mut std::io::Cursor::new(&mut png), image::ImageFormat::Png)
        .unwrap();
    agent_runtime::codec::Image {
        media_type: "image/png".into(),
        data: base64::engine::general_purpose::STANDARD.encode(png),
    }
}

#[test]
fn a_picture_that_compresses_below_its_cost_counts_at_its_cost() {
    use base64::Engine;
    let mut db = db();
    db.create("Bob", Some("/synthetic"), binding()).unwrap();
    // A blank 1568 × 1568 PNG is a few KB, and still 1600 tokens.
    let mut png = Vec::new();
    image::GrayImage::new(1568, 1568)
        .write_with_encoder(image::codecs::png::PngEncoder::new_with_quality(
            &mut png,
            image::codecs::png::CompressionType::Best,
            image::codecs::png::FilterType::NoFilter,
        ))
        .unwrap();
    let blank = agent_runtime::codec::Image {
        media_type: "image/png".into(),
        data: base64::engine::general_purpose::STANDARD.encode(png),
    };
    assert!(blank.data.len() < blank.weight());
    db.begin(
        "Bob",
        "r1",
        "look",
        true,
        &TurnOptions {
            images: vec![blank.clone()],
            ..TurnOptions::default()
        },
        allow_provider,
    )
    .unwrap();
    let window = db.window("Bob", 64 << 10, 1024).unwrap().unwrap();
    assert!(window.item_bytes > 6400, "{}", window.item_bytes);
    assert!(window.pictured[0] < 0, "{:?}", window.pictured);
    sent(&db, &window);
}

#[test]
fn pictures_count_at_their_weight_and_go_whole() {
    let mut db = db();
    db.create("Bob", Some("/synthetic"), binding()).unwrap();
    // 300 × 300 is 120 tokens, which weigh 480 bytes; its PNG is 270 KB.
    let shot = noise(300);
    assert_eq!(shot.weight(), 480);
    assert!(shot.data.len() > 300_000);
    let turn = db
        .begin(
            "Bob",
            "r1",
            "look",
            true,
            &TurnOptions {
                images: vec![shot.clone()],
                ..TurnOptions::default()
            },
            allow_provider,
        )
        .unwrap()
        .turn;
    // A read of an image file: a result with a larger picture, whose 480
    // tokens weigh enough for its stub to save something.
    let photo = noise(600);
    let call = ToolCall {
        name: "read".into(),
        call_id: "c1".into(),
        arguments: "{}".into(),
        custom: false,
    };
    let item = json!({"type":"function_call","call_id":"c1","name":"read","arguments":"{}"});
    db.append(
        turn,
        vec![serde_json::to_vec(&item).unwrap().into()],
        std::slice::from_ref(&call),
        None,
    )
    .unwrap();
    db.tool_start(turn, &call).unwrap();
    let read = Outcome {
        parts: vec![agent_runtime::codec::Part::Image(photo.clone())],
        ..result("photo.png: a PNG image, 600×600; it follows")
    };
    db.tool_finish(turn, "c1", &read).unwrap();
    db.append(turn, vec![assistant("noise")], &[], None)
        .unwrap();
    // Pictures of 360 KB and 1.4 MB fit a 64 KB budget, and go whole.
    let window = db.window("Bob", 64 << 10, 1024).unwrap().unwrap();
    assert_eq!(window.ids.len(), 4);
    assert!(window.item_bytes < 8 << 10, "{}", window.item_bytes);
    assert_eq!(
        window.pictured.iter().filter(|&&p| p > 0).count(),
        2,
        "{:?}",
        window.pictured
    );
    let items = sent(&db, &window);
    let data =
        |image: &agent_runtime::codec::Image| format!("data:image/png;base64,{}", image.data);
    assert_eq!(items[0]["content"][1]["image_url"], data(&shot));
    assert_eq!(items[2]["output"][1]["image_url"], data(&photo));
    // A summary would send them whole too.
    let plan = compaction_plan(&db, "Bob", 1, 64 << 10, 256)
        .unwrap()
        .unwrap();
    let raw = db.items_by_ids(&plan.ids, 0, plan.elided).unwrap();
    let span: i64 = plan.sizes.iter().map(|&s| i64::from(s)).sum::<i64>()
        + plan.pictured.iter().map(|&p| i64::from(p)).sum::<i64>();
    assert_eq!(raw.len() as i64, span + plan.ids.len() as i64 - 1);
    // Elided, the result goes as its text, and its picture no longer adds
    // to what it sends.
    let elision = db.elision_plan("Bob", 1, 1).unwrap().unwrap();
    db.elide("Bob", &elision).unwrap();
    let elided = db.window("Bob", 64 << 10, 1024).unwrap().unwrap();
    assert_eq!(elided.pictured, [0, 0, 0, 0]);
    assert_eq!(elided.pictures, 0);
    let items = sent(&db, &elided);
    // So does the prompt, with a line on its picture.
    assert_eq!(
        items[0]["content"],
        json!([{"type":"input_text",
            "text":"look\n[1 picture(s) sent with this message, elided from this request]"}])
    );
    assert!(
        items[2]["output"]
            .as_str()
            .unwrap()
            .starts_with("[tool result's pictures elided")
    );
}

#[test]
fn answered_tool_results_go_as_stubs_below_a_versioned_elision_floor() {
    let path = std::env::temp_dir().join(format!("agent-elision-{}.sqlite", std::process::id()));
    let _ = std::fs::remove_file(&path);
    let mut db = Database::initialize(Connection::open(&path).unwrap()).unwrap();
    db.create("Bob", Some("/synthetic"), binding()).unwrap();
    let turn = db
        .begin(
            "Bob",
            "r1",
            "long task",
            true,
            &TurnOptions::default(),
            allow_provider,
        )
        .unwrap()
        .turn;
    let mut rounds = Vec::new();
    for n in 0..6 {
        rounds.push(exchange(&mut db, turn, &format!("c{n}"), &lines(n, 400)));
    }
    let before = db.window("Bob", 1 << 20, 1024).unwrap().unwrap();
    assert_eq!(before.elided, 0);
    let full = sent(&db, &before);
    // Keep about two results verbatim. The newest result is not answered
    // yet, so however little is kept, the floor stays below its call.
    let size = full[2].to_string().len() as i64;
    let plan = db.elision_plan("Bob", 2 * size, 1).unwrap().unwrap();
    assert!(plan.through < rounds[5].0);
    let tight = db.elision_plan("Bob", 1, 1).unwrap().unwrap();
    assert_eq!(tight.through, rounds[5].0);
    assert!(
        db.elision_plan("Bob", 2 * size, plan.saved_bytes + 1)
            .unwrap()
            .is_none()
    );
    let entry = db.elide("Bob", &plan).unwrap();
    assert_eq!(entry["event"], "elided");
    assert_eq!(entry["turn"], turn);
    assert_eq!(entry["data"]["through"], plan.through);
    assert_eq!(entry["data"]["results"], plan.results);
    // Moving the floor is forward only.
    assert_eq!(
        db.elide("Bob", &plan).unwrap_err().code,
        "elision_not_forward"
    );
    let after = db.window("Bob", 1 << 20, 1024).unwrap().unwrap();
    assert_eq!(after.ids, before.ids);
    assert_eq!(after.elided, plan.through);
    assert_eq!(before.item_bytes - after.item_bytes, plan.saved_bytes);
    assert_eq!(
        before.unsummarized.bytes - after.unsummarized.bytes,
        plan.saved_bytes as usize
    );
    let items = sent(&db, &after);
    let mut stubs = 0;
    for (item, id) in items.iter().zip(&after.ids) {
        let output = item["output"].as_str().unwrap_or("");
        if output.starts_with("[tool result elided") {
            stubs += 1;
            assert!(*id <= plan.through);
            // The same call id, the size, the reference, both ends.
            assert!(item["call_id"].as_str().unwrap().starts_with('c'));
            assert!(output.contains(&format!("artifact \"result/{id}\"")));
            let whole = db
                .result_lines("Bob", *id, 1, 5000, false, usize::MAX)
                .unwrap();
            assert!(whole.contains("line 399"), "{whole}");
            assert!(output.contains("line 0") && output.contains("line 399"));
        } else if item["type"] == "function_call_output" {
            assert!(*id > plan.through, "{id}");
        }
    }
    assert_eq!(stubs, plan.results);
    // The stored transcript itself is unchanged.
    assert_eq!(stored(&mut db, "Bob"), full);
    // A result that is not on the reader's lineage is not theirs to read.
    db.create("Other", Some("/synthetic"), binding()).unwrap();
    assert_eq!(
        db.result_lines("Other", rounds[0].1, 1, 10, false, usize::MAX)
            .unwrap_err()
            .code,
        "result_not_found"
    );
    assert_eq!(
        db.result_lines("Bob", rounds[0].0, 1, 10, false, usize::MAX)
            .unwrap_err()
            .code,
        "result_not_found"
    );
    // Forks see what the source saw at their checkpoint: before the move,
    // every result whole; after it, the same stubs.
    let early = rounds[4].1;
    db.fork(
        "Bob",
        "Early",
        Fork {
            checkpoint: Some(early),
            ..Fork::default()
        },
    )
    .unwrap();
    assert_eq!(db.inspect("Early").unwrap().elision, None);
    let window = db.window("Early", 1 << 20, 1024).unwrap().unwrap();
    assert_eq!(window.elided, 0);
    assert_eq!(sent(&db, &window), full[..window.ids.len()]);
    db.append(turn, vec![assistant("done")], &[], None).unwrap();
    let checkpoint = db.finish(turn, None).unwrap().last().unwrap()["data"]["checkpoint"]
        .as_i64()
        .unwrap();
    db.fork(
        "Bob",
        "Late",
        Fork {
            checkpoint: Some(checkpoint),
            ..Fork::default()
        },
    )
    .unwrap();
    let bob = db.inspect("Bob").unwrap();
    assert_eq!(db.inspect("Late").unwrap().elision, bob.elision);
    let late = db.window("Late", 1 << 20, 1024).unwrap().unwrap();
    assert_eq!(late.elided, plan.through);
    let source = db.window("Bob", 1 << 20, 1024).unwrap().unwrap();
    assert_eq!(sent(&db, &late), sent(&db, &source));
    // The version outlives its bot while a fork holds it, then goes with the
    // last holder; stubs go with their nodes.
    db.delete_bot("Bob").unwrap();
    assert_eq!(
        db.window("Late", 1 << 20, 1024).unwrap().unwrap().elided,
        plan.through
    );
    db.delete_bot("Late").unwrap();
    let count = |table: &str| -> i64 {
        Connection::open(&path)
            .unwrap()
            .query_row(&format!("SELECT count(*) FROM {table}"), [], |r| r.get(0))
            .unwrap()
    };
    assert_eq!(count("elisions"), 0);
    // Early keeps its own stubs, for results up to its checkpoint.
    assert_eq!(count("stubs"), 5);
    db.delete_bot("Early").unwrap();
    assert_eq!(count("stubs"), 0);
    drop(db);
    std::fs::remove_file(path).unwrap();
}

#[test]
fn a_current_turn_over_budget_fits_once_its_answered_results_are_elided() {
    for family in [Family::Responses, Family::Anthropic] {
        let mut db = db();
        let mut binding = binding();
        binding.family = family;
        db.create("Bob", Some("/synthetic"), binding).unwrap();
        converse(&mut db, "Bob", 1);
        let turn = db
            .begin(
                "Bob",
                "r2",
                "long task",
                true,
                &TurnOptions::default(),
                allow_provider,
            )
            .unwrap()
            .turn;
        for n in 0..8 {
            let call = ToolCall {
                custom: false,
                name: "shell".into(),
                call_id: format!("c{n}"),
                arguments: "{}".into(),
            };
            let item = match family {
                Family::Responses => {
                    json!({"type":"function_call","call_id":call.call_id,"name":"shell","arguments":"{}"})
                }
                Family::Anthropic => json!({"role":"assistant","content":[
                    {"type":"tool_use","id":call.call_id,"name":"shell","input":{}}]}),
            };
            db.append(
                turn,
                vec![serde_json::to_vec(&item).unwrap().into()],
                std::slice::from_ref(&call),
                None,
            )
            .unwrap();
            db.tool_start(turn, &call).unwrap();
            db.tool_finish(turn, &call.call_id, &result(&lines(n, 800)))
                .unwrap();
            // The runtime reads the window each round, which saves its start.
            if n == 0 {
                db.window("Bob", 64 << 10, 1024).unwrap().unwrap();
            }
        }
        // The turn alone exceeds 64 KiB.
        assert_eq!(
            db.window("Bob", 64 << 10, 1024).unwrap_err().code,
            "context_limit"
        );
        let plan = db.elision_plan("Bob", 16 << 10, 1).unwrap().unwrap();
        db.elide("Bob", &plan).unwrap();
        let window = db.window("Bob", 64 << 10, 1024).unwrap().unwrap();
        assert!(window.item_bytes + window.ids.len() as i64 - 1 <= 64 << 10);
        // With the stubs, the saved start fits again: the earlier turn stays.
        assert_eq!(window.omitted_turns, 0);
        let items = sent(&db, &window);
        let stubbed = items
            .iter()
            .filter(|item| item.to_string().contains("[tool result elided"))
            .count();
        assert_eq!(stubbed as i64, plan.results);
        // Admission for history reads and notes counts the stubs as well.
        let (_, bytes, items) = db.turn_usage("Bob", turn).unwrap();
        let current = &window.sizes[window.ids.len() - items..];
        assert_eq!(bytes, current.iter().map(|&b| b as usize).sum::<usize>());
    }
}

#[test]
fn a_result_on_one_line_reads_back_whole_in_pieces() {
    // A shell result is one JSON line, often longer than a read page.
    let mut db = db();
    db.create("Bob", Some("/synthetic"), binding()).unwrap();
    let turn = db
        .begin(
            "Bob",
            "r1",
            "task",
            true,
            &TurnOptions::default(),
            allow_provider,
        )
        .unwrap()
        .turn;
    let wide = format!("{}{}", "é".repeat(30_000), "z".repeat(50_000));
    let (_, node) = exchange(&mut db, turn, "wide", &format!("short\n{wide}\nend"));
    let mut read = Vec::new();
    let mut offset = 1;
    loop {
        let page = db
            .result_lines("Bob", node, offset, 5000, false, usize::MAX)
            .unwrap();
        assert!(page.len() < agent_runtime::tools::PREVIEW_BYTES);
        let mut next = None;
        for line in page.lines() {
            match line.split_once('\t') {
                Some((number, piece)) if number.trim().parse::<usize>().is_ok() => {
                    read.push(piece.to_owned())
                }
                _ => {
                    if let Some(n) = line.split("offset=").nth(1) {
                        next = Some(n.trim_end_matches(']').parse::<usize>().unwrap());
                    }
                }
            }
        }
        match next {
            Some(n) => offset = n,
            None => break,
        }
    }
    assert_eq!(read.first().unwrap(), "short");
    assert_eq!(read.last().unwrap(), "end");
    let pieces = &read[1..read.len() - 1];
    assert!(
        pieces
            .iter()
            .all(|p| p.len() <= agent_runtime::tools::PIECE_BYTES)
    );
    assert_eq!(pieces.concat(), wide);
}

#[test]
fn a_piece_wider_than_the_room_reads_in_part_under_its_own_number() {
    // Numbers stay those of whole pieces, so an offset names the same text
    // whatever room a later read has.
    let mut db = db();
    db.create("Bob", Some("/synthetic"), binding()).unwrap();
    let turn = db
        .begin(
            "Bob",
            "r1",
            "task",
            true,
            &TurnOptions::default(),
            allow_provider,
        )
        .unwrap()
        .turn;
    let wide = format!("{}{}", "é".repeat(3_000), "z".repeat(6_000));
    let (_, node) = exchange(&mut db, turn, "wide", &wide);
    let whole = db
        .result_lines("Bob", node, 2, 1, false, usize::MAX)
        .unwrap();
    let piece = whole.lines().next().unwrap().split_once('\t').unwrap().1;
    assert_eq!(piece.len(), agent_runtime::tools::PIECE_BYTES);
    let page = db.result_lines("Bob", node, 2, 5000, true, 1024).unwrap();
    assert!(page.len() <= 1024, "{page}");
    let mut lines = page.lines();
    let (number, part) = lines.next().unwrap().split_once('\t').unwrap();
    assert_eq!(number.trim(), "2");
    assert!(!part.is_empty() && part.len() < piece.len());
    assert!(piece.starts_with(part));
    assert!(lines.next().unwrap().contains("read offset=2 again"));
    assert!(lines.next().is_none());
    // Too little room for any of it is still an error.
    assert_eq!(
        db.result_lines("Bob", node, 2, 5000, true, 130)
            .unwrap_err()
            .code,
        "read_line_too_long"
    );
}

#[test]
fn a_bot_without_read_stores_no_stubs() {
    // A stub names a read the model could not make, so its results carry
    // no stub and no saving, and nothing is ever planned for elision.
    let path = std::env::temp_dir().join(format!("agent-no-read-{}.sqlite", std::process::id()));
    let _ = std::fs::remove_file(&path);
    let mut db = Database::initialize(Connection::open(&path).unwrap()).unwrap();
    let shell = vec!["shell".to_owned()];
    db.create(
        "Bob",
        Some("/synthetic"),
        Binding {
            tools: &shell,
            ..binding()
        },
    )
    .unwrap();
    let turn = db
        .begin(
            "Bob",
            "r1",
            "task",
            true,
            &TurnOptions::default(),
            allow_provider,
        )
        .unwrap()
        .turn;
    exchange(&mut db, turn, "c0", &lines(0, 400));
    exchange(&mut db, turn, "c1", &lines(1, 400));
    let stored = |sql: &str| -> i64 {
        Connection::open(&path)
            .unwrap()
            .query_row(sql, [], |r| r.get(0))
            .unwrap()
    };
    assert_eq!(stored("SELECT count(*) FROM stubs"), 0);
    assert_eq!(stored("SELECT count(*) FROM nodes WHERE elided>0"), 0);
    db.window("Bob", 1 << 20, 1 << 20).unwrap();
    assert!(db.elision_plan("Bob", 1, 1).unwrap().is_none());
    drop(db);
    std::fs::remove_file(path).unwrap();
}

#[test]
fn a_result_s_text_parts_elide_together() {
    // A short output then a long text part: the stub counts and quotes
    // the whole text, as a read of the result returns it.
    let path = std::env::temp_dir().join(format!("agent-parts-stub-{}.sqlite", std::process::id()));
    let _ = std::fs::remove_file(&path);
    let mut db = Database::initialize(Connection::open(&path).unwrap()).unwrap();
    db.create("Bob", Some("/synthetic"), binding()).unwrap();
    let turn = db
        .begin(
            "Bob",
            "r1",
            "task",
            true,
            &TurnOptions::default(),
            allow_provider,
        )
        .unwrap()
        .turn;
    let call = ToolCall {
        custom: false,
        name: "shell".into(),
        call_id: "c0".into(),
        arguments: "{}".into(),
    };
    let item = json!({"type":"function_call","call_id":"c0","name":"shell","arguments":"{}"});
    db.append(
        turn,
        vec![serde_json::to_vec(&item).unwrap().into()],
        std::slice::from_ref(&call),
        None,
    )
    .unwrap();
    db.tool_start(turn, &call).unwrap();
    let long = format!("{}the end", lines(0, 400));
    let outcome = Outcome {
        parts: vec![agent_runtime::codec::Part::Text { text: long.clone() }],
        ..result("intro")
    };
    db.tool_finish(turn, "c0", &outcome).unwrap();
    let stub: Vec<u8> = Connection::open(&path)
        .unwrap()
        .query_row("SELECT item FROM stubs", [], |r| r.get(0))
        .unwrap();
    let stub: Value = serde_json::from_slice(&stub).unwrap();
    let text = stub["output"].as_str().unwrap();
    let whole = format!("intro\n{long}");
    assert!(text.contains(&format!("{} bytes", whole.len())), "{text}");
    assert!(text.ends_with("the end"), "{text}");
    drop(db);
    std::fs::remove_file(path).unwrap();
}

#[test]
fn a_custom_tool_s_pictures_are_read_without_their_bytes() {
    let mut db = db();
    db.create("Bob", Some("/synthetic"), binding()).unwrap();
    let turn = db
        .begin(
            "Bob",
            "r1",
            "task",
            true,
            &TurnOptions::default(),
            allow_provider,
        )
        .unwrap()
        .turn;
    let call = ToolCall {
        custom: true,
        name: "shot".into(),
        call_id: "c0".into(),
        arguments: "page".into(),
    };
    let item = json!({"type":"custom_tool_call","call_id":"c0","name":"shot","input":"page"});
    db.append(
        turn,
        vec![serde_json::to_vec(&item).unwrap().into()],
        std::slice::from_ref(&call),
        None,
    )
    .unwrap();
    db.tool_start(turn, &call).unwrap();
    let data = "A".repeat(4096);
    let outcome = Outcome {
        parts: vec![agent_runtime::codec::Part::Image(
            agent_runtime::codec::Image {
                media_type: "image/png".into(),
                data: data.clone(),
            },
        )],
        ..result("a page")
    };
    let (_, entry) = db.tool_finish(turn, "c0", &outcome).unwrap();
    let read = read_item(&db, "Bob", entry["data"]["node"].as_i64().unwrap()).unwrap();
    assert_eq!(read["type"], "custom_tool_call_output");
    assert_eq!(
        read["output"][1],
        json!({"type":"input_image","image_url":"data:image/png;base64,","omitted_bytes":data.len()})
    );
}

#[test]
fn schema_29_adds_elision_without_rewriting_stored_results() {
    let path = std::env::temp_dir().join(format!("agent-stubs-{}.sqlite", std::process::id()));
    let _ = std::fs::remove_file(&path);
    let savings = |path: &std::path::Path| {
        let conn = Connection::open(path).unwrap();
        let nodes: Vec<(i64, i64, i64)> = conn
            .prepare("SELECT id,elided,total_elided FROM nodes ORDER BY id")
            .unwrap()
            .query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))
            .unwrap()
            .collect::<rusqlite::Result<_>>()
            .unwrap();
        let stubs: Vec<i64> = conn
            .prepare("SELECT node FROM stubs ORDER BY node")
            .unwrap()
            .query_map([], |r| r.get(0))
            .unwrap()
            .collect::<rusqlite::Result<_>>()
            .unwrap();
        (nodes, stubs)
    };
    let old = {
        let mut db = Database::initialize(Connection::open(&path).unwrap()).unwrap();
        db.create("Bob", Some("/synthetic"), binding()).unwrap();
        let turn = db
            .begin(
                "Bob",
                "r1",
                "task",
                true,
                &TurnOptions::default(),
                allow_provider,
            )
            .unwrap()
            .turn;
        exchange(&mut db, turn, "small", "short");
        exchange(&mut db, turn, "large", &lines(0, 400));
        db.append(turn, vec![assistant("done")], &[], None).unwrap();
        db.finish(turn, None).unwrap();
        drop(db);
        savings(&path).0.iter().map(|n| n.0).collect::<Vec<_>>()
    };
    {
        let conn = Connection::open(&path).unwrap();
        conn.execute_batch(
            "DROP INDEX bots_elision; ALTER TABLE bots DROP COLUMN elision;
             ALTER TABLE nodes DROP COLUMN elided; ALTER TABLE nodes DROP COLUMN total_elided;
             DROP TABLE stubs; DROP TABLE elisions; PRAGMA user_version=28;",
        )
        .unwrap();
    }
    // Opening reads no stored result: earlier results have no stub and no
    // saving, and are always sent whole.
    let mut db = Database::initialize(Connection::open(&path).unwrap()).unwrap();
    assert_eq!(db.inspect("Bob").unwrap().elision, None);
    let (nodes, stubs) = savings(&path);
    assert_eq!(nodes, old.iter().map(|&id| (id, 0, 0)).collect::<Vec<_>>());
    assert!(stubs.is_empty());
    // A result recorded after the migration gets its stub, and savings
    // accumulate from there.
    let turn = db
        .begin(
            "Bob",
            "r2",
            "more",
            true,
            &TurnOptions::default(),
            allow_provider,
        )
        .unwrap()
        .turn;
    exchange(&mut db, turn, "later", &lines(0, 400));
    exchange(&mut db, turn, "last", "short");
    let (nodes, stubs) = savings(&path);
    assert_eq!(stubs.len(), 1);
    let saved = nodes.iter().find(|n| n.0 == stubs[0]).unwrap().1;
    assert!(saved > 0);
    assert_eq!(nodes.last().unwrap().2, saved);
    assert!(nodes.windows(2).all(|w| w[1].2 == w[0].2 + w[1].1));
    // The floor moves over it and leaves the earlier result whole.
    db.window("Bob", 1 << 20, 1 << 20).unwrap();
    let plan = db.elision_plan("Bob", 1, 1).unwrap().unwrap();
    assert_eq!((plan.results, plan.saved_bytes), (1, saved));
    drop(db);
    std::fs::remove_file(path).unwrap();
}

#[test]
fn compaction_counts_and_summarizes_stubs_under_the_elision_floor() {
    let mut db = db();
    db.create("Bob", Some("/synthetic"), binding()).unwrap();
    let older = db
        .begin(
            "Bob",
            "r1",
            "p1",
            true,
            &TurnOptions::default(),
            allow_provider,
        )
        .unwrap()
        .turn;
    exchange(&mut db, older, "old", &lines(100, 800));
    db.append(older, vec![assistant("r1")], &[], None).unwrap();
    db.finish(older, None).unwrap();
    let turn = db
        .begin(
            "Bob",
            "r2",
            "long task",
            true,
            &TurnOptions::default(),
            allow_provider,
        )
        .unwrap()
        .turn;
    for n in 0..8 {
        exchange(&mut db, turn, &format!("c{n}"), &lines(n, 800));
        if n == 0 {
            db.window("Bob", i64::MAX, i64::MAX).unwrap().unwrap();
        }
    }
    let raw = db.unsummarized_bytes("Bob").unwrap();
    let plan = db.elision_plan("Bob", 16 << 10, 1).unwrap().unwrap();
    db.elide("Bob", &plan).unwrap();
    // Bytes not yet summarized count the stubs, one subtraction per node.
    let unsummarized = db.unsummarized_bytes("Bob").unwrap();
    assert_eq!(unsummarized, raw - plan.saved_bytes);
    let window = db.window("Bob", i64::MAX, i64::MAX).unwrap().unwrap();
    assert_eq!(
        window.unsummarized.bytes as i64, window.item_bytes,
        "the whole unsummarized span is in view"
    );
    // The raw span is over the summarizer's 64 KiB; as sent, it plans in
    // one step, the older turn with its result as the stub the model saw.
    let limit = agent_runtime::store::ContextUsage {
        bytes: 64 << 10,
        items: 256,
    };
    assert!(raw > limit.bytes as i64 && unsummarized < limit.bytes as i64);
    // Keeping the whole current turn cuts at its prompt.
    let (_, current, _) = db.turn_usage("Bob", turn).unwrap();
    let summarized = compaction_plan(&db, "Bob", current as i64, 64 << 10, 256)
        .unwrap()
        .unwrap();
    assert_eq!(summarized.pinned, None);
    assert!(!summarized.catch_up);
    assert_eq!(summarized.covered, (1, 1));
    assert_eq!(summarized.elided, window.elided);
    let items = db
        .items_by_ids(&summarized.ids, i64::MAX, summarized.elided)
        .unwrap();
    assert_eq!(
        items.len() + 1,
        summarized
            .sizes
            .iter()
            .map(|&s| s as usize + 1)
            .sum::<usize>()
    );
    let text = String::from_utf8(items).unwrap();
    assert!(text.contains("[tool result elided from this request"));
    assert!(!text.contains("result 100 line 400"));
    // The current turn fits only as sent; the new view is checked the same
    // way, so the summary installs.
    db.compact("Bob", &summarized, "summary", None, 0, limit, Value::Null)
        .unwrap();
    let window = db.window("Bob", 64 << 10, 256).unwrap().unwrap();
    assert_eq!(window.omitted_turns, 1);
    assert_eq!(window.unsummarized.bytes as i64, window.item_bytes);
    let (_, bytes, _) = db.turn_usage("Bob", turn).unwrap();
    assert_eq!(bytes as i64, window.item_bytes);
}

#[test]
fn an_elided_event_counts_what_the_move_takes_off_the_view() {
    // A cut summarizes a result the floor had not reached. The next move
    // stubs it too, since a floor covers everything below it, but it is
    // never sent again: the event counts the results and bytes the move
    // takes off the view.
    let mut db = db();
    db.create("Bob", Some("/synthetic"), binding()).unwrap();
    let turn = db
        .begin(
            "Bob",
            "r1",
            "long task",
            true,
            &TurnOptions::default(),
            allow_provider,
        )
        .unwrap()
        .turn;
    let mut calls = Vec::new();
    for n in 0..8 {
        calls.push(exchange(&mut db, turn, &format!("c{n}"), &lines(n, 800)));
        if n == 0 {
            db.window("Bob", i64::MAX, i64::MAX).unwrap().unwrap();
        }
    }
    let first = db.elision_plan("Bob", 40 << 10, 1).unwrap().unwrap();
    assert!(first.through < calls[6].1);
    db.elide("Bob", &first).unwrap();
    let plan = compaction_plan(&db, "Bob", 1, 64 << 10, 256)
        .unwrap()
        .unwrap();
    assert_eq!(plan.cut, calls[7].0);
    let limit = ContextUsage {
        bytes: 64 << 10,
        items: 256,
    };
    db.compact("Bob", &plan, "summary", None, 0, limit, Value::Null)
        .unwrap();
    for n in 8..10 {
        calls.push(exchange(&mut db, turn, &format!("c{n}"), &lines(n, 800)));
    }
    let before = db.window("Bob", i64::MAX, i64::MAX).unwrap().unwrap();
    let second = db.elision_plan("Bob", 1, 1).unwrap().unwrap();
    let event = db.elide("Bob", &second).unwrap();
    let after = db.window("Bob", i64::MAX, i64::MAX).unwrap().unwrap();
    // Results 7 and 8, answered in view; 6 went with the summary.
    assert_eq!(event["data"]["results"], 2);
    assert_eq!(
        event["data"]["saved_bytes"].as_i64().unwrap(),
        before.item_bytes - after.item_bytes
    );
    assert_eq!(after.ids, before.ids);
}

/// The request prefix a window's view renders, as JSON items.
fn prefix_items(window: &agent_runtime::store::Window) -> Vec<Value> {
    let prefix = window.prefix(&[], usize::MAX, false).unwrap().bytes;
    let prefix = &prefix[..prefix.len().saturating_sub(1)];
    serde_json::from_slice(&[b"[", prefix, b"]"].concat()).unwrap()
}

#[test]
fn a_cut_inside_the_running_turn_keeps_its_prompt_ahead_of_the_tail() {
    let mut db = db();
    db.create("Bob", Some("/synthetic"), binding()).unwrap();
    converse(&mut db, "Bob", 1);
    let turn = db
        .begin(
            "Bob",
            "r2",
            "long task",
            true,
            &TurnOptions::default(),
            allow_provider,
        )
        .unwrap()
        .turn;
    let prompt = db.inspect("Bob").unwrap().head.unwrap();
    let mut calls = Vec::new();
    for n in 0..6 {
        calls.push(exchange(&mut db, turn, &format!("c{n}"), &lines(n, 50)));
        if n == 0 {
            db.window("Bob", i64::MAX, i64::MAX).unwrap().unwrap();
        }
    }
    let limit = agent_runtime::store::ContextUsage {
        bytes: 64 << 10,
        items: 256,
    };
    // Keeping one byte cuts at the newest round that follows a result.
    let plan = compaction_plan(&db, "Bob", 1, 64 << 10, 256)
        .unwrap()
        .unwrap();
    assert_eq!((plan.cut, plan.pinned), (calls[5].0, Some(prompt)));
    assert_eq!(plan.covered, (1, 2));
    assert_eq!(
        plan.prompts
            .iter()
            .map(|(o, p)| (*o, p.as_str()))
            .collect::<Vec<_>>(),
        vec![(1, "p1"), (2, "long task")]
    );
    // The span ends with a completed exchange: every call has its result.
    assert_eq!(*plan.ids.last().unwrap(), calls[4].1);
    let span: Vec<Value> = {
        let items = db.items_by_ids(&plan.ids, i64::MAX, 0).unwrap();
        serde_json::from_slice(&[b"[", &items[..], b"]"].concat()).unwrap()
    };
    let ids = |kind: &str| -> Vec<String> {
        span.iter()
            .filter(|i| i["type"] == kind)
            .map(|i| i["call_id"].as_str().unwrap().to_owned())
            .collect()
    };
    assert_eq!(ids("function_call"), ids("function_call_output"));
    let event = db
        .compact("Bob", &plan, "summary one", None, 0, limit, Value::Null)
        .unwrap();
    assert_eq!(event["data"]["pinned"], json!(prompt));
    let first = db.inspect("Bob").unwrap().compaction.unwrap();
    // The turn's prompt goes whole, ahead of the tail from the cut.
    let window = db.window("Bob", 64 << 10, 256).unwrap().unwrap();
    assert_eq!(&window.ids[..2], &[prompt, calls[5].0]);
    assert_eq!(window.omitted_turns, 1);
    // Turn one's two messages, and all of turn two's before the cut but
    // its prompt.
    assert_eq!(window.omitted_items, 2);
    assert_eq!(window.omitted_in_turn as usize, plan.ids.len() - 3);
    let items = sent(&db, &window);
    assert_eq!(items[0]["content"][0]["text"], "long task");
    assert_eq!(window.unsummarized.bytes as i64, window.item_bytes);
    let (_, bytes, count) = db.turn_usage("Bob", turn).unwrap();
    assert_eq!((bytes as i64, count), (window.item_bytes, window.ids.len()));
    // The summary says it covers the start of this turn, and lists only
    // the earlier turns' prompts.
    let view = prefix_items(&window)[0]["content"][0]["text"]
        .as_str()
        .unwrap()
        .to_owned();
    assert!(view.starts_with(&format!(
        "[compaction summary, version {first}, covering turns 1 to 1 and the start of turn 2]"
    )));
    assert!(view.contains("\n1: p1") && !view.contains("long task"));
    assert_eq!(
        prefix_items(&window)[1]["content"][0]["text"],
        "[context note] 1 earlier turn(s) with 2 messages, and 10 earlier messages of turn 2, are not shown."
    );

    // A second cut in the same turn summarizes from the first, keeping
    // the same prompt in view.
    for n in 6..9 {
        calls.push(exchange(&mut db, turn, &format!("c{n}"), &lines(n, 50)));
    }
    let second = compaction_plan(&db, "Bob", 1, 64 << 10, 256)
        .unwrap()
        .unwrap();
    assert_eq!((second.cut, second.pinned), (calls[8].0, Some(prompt)));
    assert_eq!(second.covered, (2, 2));
    assert_eq!(second.ids.first(), Some(&calls[5].0));
    assert_eq!(second.previous_summary.as_deref(), Some("summary one"));
    db.compact("Bob", &second, "summary two", None, 0, limit, Value::Null)
        .unwrap();
    db.append(turn, vec![assistant("done")], &[], None).unwrap();
    db.finish(turn, None).unwrap();

    // The next turn's window still starts at the cut with its prompt, and
    // a cut at that turn's own prompt covers the earlier one whole.
    converse(&mut db, "Bob", 3);
    let window = db.window("Bob", 64 << 10, 256).unwrap().unwrap();
    assert_eq!(&window.ids[..2], &[prompt, calls[8].0]);
    let third = compaction_plan(&db, "Bob", 1, 64 << 10, 256)
        .unwrap()
        .unwrap();
    assert_eq!(third.pinned, None);
    assert_eq!(third.covered, (2, 2));
    db.compact("Bob", &third, "summary three", None, 0, limit, Value::Null)
        .unwrap();
    let window = db.window("Bob", 64 << 10, 256).unwrap().unwrap();
    assert_eq!(window.omitted_turns, 2);
    let view = prefix_items(&window)[0]["content"][0]["text"]
        .as_str()
        .unwrap()
        .to_owned();
    assert!(view.contains("covering turns 1 to 2]"));
    assert!(view.contains("\n1: p1\n2: long task"));

    // A fork from between the two cuts in turn two binds the first, and
    // sees the prompt ahead of its cut as the source did.
    db.fork(
        "Bob",
        "Branch",
        Fork {
            checkpoint: Some(calls[7].1),
            ..Fork::default()
        },
    )
    .unwrap();
    assert_eq!(db.inspect("Branch").unwrap().compaction, Some(first));
    let window = db.window("Branch", 64 << 10, 256).unwrap().unwrap();
    assert_eq!(&window.ids[..2], &[prompt, calls[5].0]);
    assert_eq!(*window.ids.last().unwrap(), calls[7].1);
}

/// A running turn of four small exchanges, then a steer absorbed after the
/// last result, which the model answers with one large exchange.
fn steered_turn(db: &mut Database) -> (i64, i64, Vec<(i64, i64)>) {
    db.create("Bob", Some("/synthetic"), binding()).unwrap();
    let turn = db
        .begin(
            "Bob",
            "r1",
            "long task",
            true,
            &TurnOptions::default(),
            allow_provider,
        )
        .unwrap()
        .turn;
    let prompt = db.inspect("Bob").unwrap().head.unwrap();
    let mut calls: Vec<(i64, i64)> = (0..4)
        .map(|n| exchange(db, turn, &format!("c{n}"), &lines(n, 50)))
        .collect();
    let steer = TurnOptions {
        delivery: Delivery::Steer,
        ..TurnOptions::default()
    };
    db.begin(
        "Bob",
        "s",
        "use the other table",
        true,
        &steer,
        allow_provider,
    )
    .unwrap();
    let absorbed = db
        .absorb(
            turn,
            None,
            8 << 20,
            4096,
            ContextUsage::default(),
            false,
            true,
        )
        .unwrap();
    let steered = absorbed.outcomes[0].1["node"].as_i64().unwrap();
    calls.push(exchange(db, turn, "c4", &lines(4, 400)));
    (prompt, steered, calls)
}

#[test]
fn a_cut_inside_a_turn_may_start_at_a_steer_that_follows_a_result() {
    // The model's next output follows the steer, not the result, so the
    // steer starts that round: the tail may begin with it, whole.
    let mut db = db();
    let (prompt, steered, calls) = steered_turn(&mut db);
    let plan = compaction_plan(&db, "Bob", 1, 64 << 10, 256)
        .unwrap()
        .unwrap();
    assert_eq!((plan.cut, plan.pinned), (steered, Some(prompt)));
    assert_eq!(plan.ids.last(), Some(&calls[3].1));
    db.compact(
        "Bob",
        &plan,
        "summary",
        None,
        0,
        ContextUsage {
            bytes: 64 << 10,
            items: 256,
        },
        Value::Null,
    )
    .unwrap();
    let window = db.window("Bob", 64 << 10, 256).unwrap().unwrap();
    assert_eq!(&window.ids[..3], &[prompt, steered, calls[4].0]);
    assert_eq!(
        sent(&db, &window)[1]["content"][0]["text"],
        "use the other table"
    );
}

#[test]
fn a_catch_up_step_may_end_before_a_steer_that_follows_a_result() {
    // The large result leaves no room for the step to reach the newest
    // round, so the step ends at the steer, keeping the prompt.
    let mut db = db();
    let (prompt, steered, calls) = steered_turn(&mut db);
    let Some(Planning::CatchUp(mut walk)) = db
        .compaction_plan("Bob", 1, i64::MAX, 8192, 256, false)
        .unwrap()
    else {
        panic!("expected a catch-up walk");
    };
    while !walk.done() {
        db.catch_up_piece(&mut walk, 16).unwrap();
    }
    let plan = db.catch_up_plan("Bob", walk).unwrap().unwrap();
    assert_eq!((plan.cut, plan.pinned), (steered, Some(prompt)));
    assert_eq!(plan.ids.last(), Some(&calls[3].1));
}

/// An Anthropic bot whose every reply thinks for 3,000 bytes and answers in
/// a few.
fn thinking_turns(db: &mut Database, turns: usize) {
    db.create(
        "Bob",
        Some("/synthetic"),
        Binding {
            family: Family::Anthropic,
            ..binding()
        },
    )
    .unwrap();
    for n in 1..=turns {
        let turn = db
            .begin(
                "Bob",
                &format!("r{n}"),
                &format!("p{n}"),
                true,
                &TurnOptions::default(),
                allow_provider,
            )
            .unwrap()
            .turn;
        let reply = json!({"role":"assistant","content":[
            {"type":"thinking","thinking":"t".repeat(3000),"signature":"s"},
            {"type":"text","text":format!("r{n}")}]});
        db.append(
            turn,
            vec![serde_json::to_vec(&reply).unwrap().into()],
            &[],
            None,
        )
        .unwrap();
        db.finish(turn, None).unwrap();
    }
}

#[test]
fn a_summary_span_is_sized_without_the_thinking_its_request_leaves_out() {
    // No reply fits 2 KiB with its thinking, and every one fits without it,
    // as the summarizer is sent them.
    for (turns, catch_up) in [(4, false), (40, true)] {
        let mut db = db();
        thinking_turns(&mut db, turns);
        let plan = compaction_plan(&db, "Bob", 1, 2048, 256)
            .unwrap()
            .expect("a span that fits once its thinking is left out");
        assert_eq!(plan.catch_up, catch_up);
        assert!(plan.ids.len() >= 4);
        let (head, tail) =
            CompactionPlan::frame(Family::Anthropic, None, plan.summary_bytes).unwrap();
        let sent = db.items_by_ids(&plan.ids, i64::MAX, 0).unwrap();
        assert!(!String::from_utf8_lossy(&sent).contains("\"thinking\""));
        assert!(head.len() + sent.len() + tail.len() <= 2048);
    }
}

/// Every step of a catch-up walk through a bot's backlog, recorded with
/// numbered summaries, one step per round as the runtime takes them; each
/// step must end at a completed exchange of the long turn and keep its
/// prompt.
fn catch_up_steps(
    db: &mut Database,
    turn: i64,
    turn_prompt: i64,
    calls: &mut Vec<(i64, i64)>,
    max_bytes: i64,
) -> Vec<CompactionPlan> {
    let mut steps: Vec<CompactionPlan> = Vec::new();
    while let Some(Planning::CatchUp(mut walk)) = db
        .compaction_plan("Bob", 1, i64::MAX, max_bytes, 256, false)
        .unwrap()
    {
        while !walk.done() {
            db.catch_up_piece(&mut walk, 16).unwrap();
        }
        let plan = db.catch_up_plan("Bob", walk).unwrap().unwrap();
        assert!(plan.catch_up);
        assert_eq!(plan.pinned, Some(turn_prompt));
        let round = calls
            .iter()
            .position(|(asked, _)| *asked == plan.cut)
            .expect("each step cuts at a round");
        // The span ends with the previous round's result, and every call
        // in it has its result.
        assert_eq!(plan.ids.last(), Some(&calls[round - 1].1));
        let span: Vec<Value> = {
            let items = db.items_by_ids(&plan.ids, i64::MAX, 0).unwrap();
            serde_json::from_slice(&[b"[", &items[..], b"]"].concat()).unwrap()
        };
        let ids = |kind: &str| -> Vec<String> {
            span.iter()
                .filter(|i| i["type"] == kind)
                .map(|i| i["call_id"].as_str().unwrap().to_owned())
                .collect()
        };
        assert_eq!(ids("function_call"), ids("function_call_output"));
        assert!(plan.sizes.iter().map(|&s| s as i64).sum::<i64>() <= max_bytes);
        if let Some(previous) = steps.last() {
            assert_eq!(plan.ids.first(), Some(&previous.cut));
            assert_eq!(
                plan.previous_summary,
                Some(format!("summary {}", steps.len() - 1))
            );
        }
        let event = db
            .compact(
                "Bob",
                &plan,
                &format!("summary {}", steps.len()),
                None,
                0,
                agent_runtime::store::ContextUsage {
                    bytes: max_bytes as usize,
                    items: 256,
                },
                Value::Null,
            )
            .unwrap();
        assert_eq!(
            (&event["data"]["catch_up"], &event["data"]["pinned"]),
            (&json!(true), &json!(turn_prompt))
        );
        steps.push(plan);
        assert!(steps.len() < 64, "catch-up does not converge");
        let n = calls.len();
        calls.push(exchange(db, turn, &format!("c{n}"), &lines(n, 50)));
    }
    steps
}

#[test]
fn catch_up_through_a_turn_larger_than_the_budget_cuts_at_its_rounds() {
    let mut db = db();
    db.create("Bob", Some("/synthetic"), binding()).unwrap();
    converse(&mut db, "Bob", 1);
    let turn = db
        .begin(
            "Bob",
            "r2",
            "long task",
            true,
            &TurnOptions::default(),
            allow_provider,
        )
        .unwrap()
        .turn;
    let prompt = db.inspect("Bob").unwrap().head.unwrap();
    let mut calls: Vec<(i64, i64)> = (0..40)
        .map(|n| exchange(&mut db, turn, &format!("c{n}"), &lines(n, 50)))
        .collect();
    let max_bytes = 8192;
    // The walk's pieces do not change the first step, whatever their size.
    let mut first = Vec::new();
    for piece in [1, 7, 4096] {
        let Some(Planning::CatchUp(mut walk)) = db
            .compaction_plan("Bob", 1, i64::MAX, max_bytes, 256, false)
            .unwrap()
        else {
            panic!("expected a catch-up walk");
        };
        while !walk.done() {
            db.catch_up_piece(&mut walk, piece).unwrap();
        }
        let plan = db.catch_up_plan("Bob", walk).unwrap().unwrap();
        first.push((plan.cut, plan.pinned, plan.ids, plan.covered, plan.prompts));
    }
    assert!(first.windows(2).all(|pair| pair[0] == pair[1]));
    // The first step takes turn one whole and the long turn's start.
    let (_, _, ids, covered, prompts) = &first[0];
    assert_eq!(*covered, (1, 2));
    assert_eq!(
        prompts
            .iter()
            .map(|(o, p)| (*o, p.as_str()))
            .collect::<Vec<_>>(),
        vec![(1, "p1"), (2, "long task")]
    );
    assert!(ids.contains(&prompt));

    // Steps walk through the turn a round boundary at a time, then the
    // rest fits one ordinary summary that cuts at the newest round.
    let steps = catch_up_steps(&mut db, turn, prompt, &mut calls, max_bytes);
    assert!(steps.len() > 1);
    assert!(steps[1..].iter().all(|step| step.covered == (2, 2)));
    let last = compaction_plan(&db, "Bob", 1, max_bytes, 256)
        .unwrap()
        .unwrap();
    assert!(!last.catch_up);
    assert_eq!(
        (last.cut, last.pinned),
        (calls.last().unwrap().0, Some(prompt))
    );
    assert_eq!(last.ids.first(), Some(&steps.last().unwrap().cut));
    // The view sends the prompt whole, then the tail from the last cut.
    let window = db.window("Bob", max_bytes, 256).unwrap().unwrap();
    assert_eq!(&window.ids[..2], &[prompt, steps.last().unwrap().cut]);
    // The transcript is intact.
    assert_eq!(
        db.history_read("Bob", 2, 0, 65536).unwrap()["items"],
        1 + 2 * calls.len()
    );
}

#[test]
fn catch_up_cuts_a_finished_turn_larger_than_the_budget_at_its_rounds() {
    // A finished turn outgrew a budget set after it ran: the walk cuts it
    // at its rounds, keeping its prompt, then goes on at turn boundaries.
    let mut db = db();
    db.create("Bob", Some("/synthetic"), binding()).unwrap();
    let turn = db
        .begin(
            "Bob",
            "r1",
            "long task",
            true,
            &TurnOptions::default(),
            allow_provider,
        )
        .unwrap()
        .turn;
    let prompt = db.inspect("Bob").unwrap().head.unwrap();
    let calls: Vec<(i64, i64)> = (0..40)
        .map(|n| exchange(&mut db, turn, &format!("c{n}"), &lines(n, 50)))
        .collect();
    db.append(turn, vec![assistant("done")], &[], None).unwrap();
    db.finish(turn, None).unwrap();
    for n in 2..=4 {
        converse(&mut db, "Bob", n);
    }
    let Some(Planning::CatchUp(mut walk)) = db
        .compaction_plan("Bob", 1, i64::MAX, 8192, 256, false)
        .unwrap()
    else {
        panic!("expected a catch-up walk");
    };
    while !walk.done() {
        db.catch_up_piece(&mut walk, 16).unwrap();
    }
    let plan = db.catch_up_plan("Bob", walk).unwrap().unwrap();
    assert_eq!(plan.pinned, Some(prompt));
    assert_eq!(plan.covered, (1, 1));
    assert!(calls[1..].iter().any(|(asked, _)| *asked == plan.cut));
    db.compact(
        "Bob",
        &plan,
        "summary 0",
        None,
        0,
        agent_runtime::store::ContextUsage {
            bytes: 8192,
            items: 256,
        },
        Value::Null,
    )
    .unwrap();
    // The view keeps the old turn's prompt ahead of the rest of that turn
    // and the turns after it.
    let window = db.window("Bob", i64::MAX, i64::MAX).unwrap().unwrap();
    assert_eq!(&window.ids[..2], &[prompt, plan.cut]);
    // Later steps, one a turn, carry on through the old turn and then at
    // turn boundaries until the rest fits.
    let mut cuts = vec![plan.cut];
    let mut n = 5;
    converse(&mut db, "Bob", n);
    while let Some(Planning::CatchUp(mut walk)) = db
        .compaction_plan("Bob", 1, i64::MAX, 8192, 256, false)
        .unwrap()
    {
        while !walk.done() {
            db.catch_up_piece(&mut walk, 16).unwrap();
        }
        let next = db.catch_up_plan("Bob", walk).unwrap().unwrap();
        assert_eq!(next.ids.first(), cuts.last());
        assert_eq!(next.covered.0, 1);
        cuts.push(next.cut);
        db.compact(
            "Bob",
            &next,
            "summary",
            None,
            0,
            agent_runtime::store::ContextUsage {
                bytes: 8192,
                items: 256,
            },
            Value::Null,
        )
        .unwrap();
        n += 1;
        converse(&mut db, "Bob", n);
        assert!(n < 64, "catch-up does not converge");
    }
    // The steps cut inside the old turn; the rest then fits one ordinary
    // summary, cut at the newest turn's prompt.
    assert!(cuts.len() > 1);
    assert!(
        cuts.iter()
            .all(|cut| calls.iter().any(|(asked, _)| asked == cut))
    );
    let last = compaction_plan(&db, "Bob", 1, 8192, 256).unwrap().unwrap();
    assert_eq!((last.pinned, last.covered), (None, (1, n as i64 - 1)));
    assert_eq!(last.ids.first(), cuts.last());
    assert_eq!(
        db.history_read("Bob", 1, 0, 65536).unwrap()["items"],
        1 + 2 * calls.len() + 1
    );
}

#[test]
fn schema_30_adds_the_prompt_a_cut_inside_a_turn_keeps() {
    let path = std::env::temp_dir().join(format!("agent-pinned-{}.sqlite", std::process::id()));
    let _ = std::fs::remove_file(&path);
    {
        let mut db = Database::initialize(Connection::open(&path).unwrap()).unwrap();
        db.create("Bob", Some("/synthetic"), binding()).unwrap();
        for n in 1..=6 {
            converse(&mut db, "Bob", n);
        }
        let plan = compaction_plan(&db, "Bob", 1, i64::MAX, i64::MAX)
            .unwrap()
            .unwrap();
        let limit = agent_runtime::store::ContextUsage {
            bytes: 4096,
            items: 256,
        };
        db.compact("Bob", &plan, "summary", None, 0, limit, Value::Null)
            .unwrap();
    }
    Connection::open(&path)
        .unwrap()
        .execute_batch(
            "DROP INDEX compactions_pinned; ALTER TABLE compactions DROP COLUMN pinned;
             PRAGMA user_version=29;",
        )
        .unwrap();
    // Every earlier cut is a turn's prompt: none keeps one.
    let mut db = Database::initialize(Connection::open(&path).unwrap()).unwrap();
    let pinned: Vec<Option<i64>> = Connection::open(&path)
        .unwrap()
        .prepare("SELECT pinned FROM compactions")
        .unwrap()
        .query_map([], |r| r.get(0))
        .unwrap()
        .collect::<rusqlite::Result<_>>()
        .unwrap();
    assert_eq!(pinned, vec![None]);
    let window = db.window("Bob", i64::MAX, i64::MAX).unwrap().unwrap();
    assert_eq!((window.omitted_turns, window.omitted_in_turn), (5, 0));
    drop(db);
    std::fs::remove_file(path).unwrap();
}

#[test]
fn a_stub_strips_thinking_from_itself_up_to_the_request_that_sent_it() {
    // Nodes below 10 lost their context; the first stub is node 20 and the
    // request that sent it ended at node 30.
    let one = Strip::from(10).and(20, 31);
    assert_eq!(
        one,
        Strip {
            below: 10,
            from: 20,
            to: 31
        }
    );
    assert_eq!(
        (0..33).filter(|id| !one.strips(*id)).collect::<Vec<_>>(),
        [(10..20).collect::<Vec<_>>(), vec![31, 32]].concat()
    );
    // A later stub inside the range, or past it, extends it to the new
    // request; the nodes between two ranges go with them.
    for first in [25, 35] {
        assert_eq!(
            one.and(first, 41),
            Strip {
                below: 10,
                from: 20,
                to: 41
            }
        );
    }
    // A stub at or below the floor strips everything up to the request.
    assert_eq!(one.and(10, 41), Strip::from(41));
    assert_eq!(Strip::default().and(1, 5).from, 1);
}

#[test]
fn schema_31_keeps_each_bots_thinking_and_forks_carry_a_strip_from_before_them() {
    let path = std::env::temp_dir().join(format!("agent-strip-{}.sqlite", std::process::id()));
    let _ = std::fs::remove_file(&path);
    {
        let mut db = Database::initialize(Connection::open(&path).unwrap()).unwrap();
        db.create("Bob", Some("/synthetic"), binding()).unwrap();
        for n in 1..=3 {
            converse(&mut db, "Bob", n);
        }
        db.set_thinking("Bob", 7, Strip::from(3), 0).unwrap();
    }
    Connection::open(&path)
        .unwrap()
        .execute_batch(
            "ALTER TABLE bots DROP COLUMN thinking_from; ALTER TABLE bots DROP COLUMN thinking_to;
             ALTER TABLE bots DROP COLUMN thinking_elided; PRAGMA user_version=30;",
        )
        .unwrap();
    let mut db = Database::initialize(Connection::open(&path).unwrap()).unwrap();
    let bob = db.inspect("Bob").unwrap();
    assert_eq!(
        (bob.thinking_prefix, bob.thinking, bob.thinking_elided),
        (Some(7), Strip::from(3), 0)
    );
    // A strip recorded by a request that sent the head carries to a fork
    // there, and not to one from before it.
    let head = bob.head.unwrap();
    let strip = Strip {
        below: 3,
        from: head - 2,
        to: head + 1,
    };
    db.set_thinking("Bob", 7, strip, head - 2).unwrap();
    for (name, checkpoint, carried) in [("Now", None, true), ("Before", Some(head - 2), false)] {
        db.fork(
            "Bob",
            name,
            Fork {
                checkpoint,
                ..Fork::default()
            },
        )
        .unwrap();
        let fork = db.inspect(name).unwrap();
        let expected = if carried {
            (Some(7), strip, head - 2)
        } else {
            (None, Strip::default(), 0)
        };
        assert_eq!(
            (fork.thinking_prefix, fork.thinking, fork.thinking_elided),
            expected
        );
    }
    drop(db);
    std::fs::remove_file(path).unwrap();
}

#[test]
fn schema_33_adds_approvals_with_no_gates_on_existing_bots() {
    let path = std::env::temp_dir().join(format!("agent-gates-{}.sqlite", std::process::id()));
    let _ = std::fs::remove_file(&path);
    {
        let mut db = Database::initialize(Connection::open(&path).unwrap()).unwrap();
        db.create("Bob", Some("/synthetic"), binding()).unwrap();
    }
    Connection::open(&path)
        .unwrap()
        .execute_batch(
            "DROP TABLE approval_tags; DROP TABLE approvals;
             ALTER TABLE bots DROP COLUMN gates; PRAGMA user_version=32;",
        )
        .unwrap();
    let mut db = Database::initialize(Connection::open(&path).unwrap()).unwrap();
    assert!(db.inspect("Bob").unwrap().gates.is_empty());
    // A bot created after it takes a gate, which announces its calls.
    let tools = ["shell".to_owned()];
    let gate = Gate {
        tag: "manual".into(),
        tools: tools.to_vec(),
        expire_ms: None,
    };
    db.create(
        "Ann",
        Some("/synthetic"),
        Binding {
            tools: &tools,
            gate: Some(&gate),
            ..binding()
        },
    )
    .unwrap();
    let turn = db
        .begin(
            "Ann",
            "request",
            "work",
            true,
            &TurnOptions::default(),
            allow_provider,
        )
        .unwrap()
        .turn;
    let (item, call) = shell_call("fc_1", "s1", "true");
    db.append(turn, vec![item], &[call], None).unwrap();
    let listed = db.approvals(None, Some("manual"), 0, None, 64).unwrap();
    assert_eq!(listed["approvals"][0]["call_id"], "s1");
    drop(db);
    let version: i32 = Connection::open(&path)
        .unwrap()
        .query_row("PRAGMA user_version", [], |r| r.get(0))
        .unwrap();
    assert_eq!(version, Database::SCHEMA);
    std::fs::remove_file(path).unwrap();
}

#[test]
fn schema_34_records_whether_a_call_or_a_summary_changed_the_view_last() {
    let path = std::env::temp_dir().join(format!("agent-sent-{}.sqlite", std::process::id()));
    let _ = std::fs::remove_file(&path);
    {
        let mut db = Database::initialize(Connection::open(&path).unwrap()).unwrap();
        db.create("Bob", Some("/synthetic"), binding()).unwrap();
        for n in 1..=6 {
            converse(&mut db, "Bob", n);
        }
    }
    Connection::open(&path)
        .unwrap()
        .execute_batch("ALTER TABLE turns DROP COLUMN view_sent; PRAGMA user_version=33;")
        .unwrap();
    let mut db = Database::initialize(Connection::open(&path).unwrap()).unwrap();
    let begin = |db: &mut Database, n: usize| {
        db.begin(
            "Bob",
            &format!("r{n}"),
            &format!("p{n}"),
            true,
            &TurnOptions::default(),
            allow_provider,
        )
        .unwrap()
        .turn
    };
    // A turn stored before records neither, so none is taken as having
    // sent the view the next turn starts from.
    let turn = begin(&mut db, 7);
    let context = db.context(turn).unwrap();
    assert_eq!((context.view_sent, context.previous_call), (None, None));
    // A call sends the view; a summary after it rewrites the view.
    db.append(turn, vec![assistant("r7")], &[], None).unwrap();
    assert_eq!(db.context(turn).unwrap().view_sent, Some(true));
    let plan = compaction_plan(&db, "Bob", 1, i64::MAX, i64::MAX)
        .unwrap()
        .unwrap();
    db.compact(
        "Bob",
        &plan,
        "summary",
        None,
        0,
        ContextUsage {
            bytes: 4096,
            items: 256,
        },
        Value::Null,
    )
    .unwrap();
    assert_eq!(db.context(turn).unwrap().view_sent, Some(false));
    db.finish(turn, None).unwrap();
    let next = begin(&mut db, 8);
    assert_eq!(db.context(next).unwrap().previous_call, None);
    db.append(next, vec![assistant("r8")], &[], None).unwrap();
    db.finish(next, None).unwrap();
    let after = begin(&mut db, 9);
    let context = db.context(after).unwrap();
    assert_eq!(context.previous_call, Some((context.model, None)));
    drop(db);
    let version: i32 = Connection::open(&path)
        .unwrap()
        .query_row("PRAGMA user_version", [], |r| r.get(0))
        .unwrap();
    assert_eq!(version, Database::SCHEMA);
    std::fs::remove_file(path).unwrap();
}

#[test]
fn schema_32_sums_thinking_from_the_nodes_stored_after_it() {
    let path = std::env::temp_dir().join(format!("agent-thought-{}.sqlite", std::process::id()));
    let _ = std::fs::remove_file(&path);
    {
        let mut db = Database::initialize(Connection::open(&path).unwrap()).unwrap();
        thinking_turns(&mut db, 1);
    }
    Connection::open(&path)
        .unwrap()
        .execute_batch("ALTER TABLE nodes DROP COLUMN total_thinking; PRAGMA user_version=31;")
        .unwrap();
    let mut db = Database::initialize(Connection::open(&path).unwrap()).unwrap();
    // Nodes stored before count none; each later node adds its own.
    let turn = db
        .begin(
            "Bob",
            "r2",
            "p2",
            true,
            &TurnOptions::default(),
            allow_provider,
        )
        .unwrap()
        .turn;
    db.append(turn, vec![Bytes::from_static(THINKING)], &[], None)
        .unwrap();
    db.finish(turn, None).unwrap();
    drop(db);
    let totals: Vec<i64> = Connection::open(&path)
        .unwrap()
        .prepare("SELECT total_thinking FROM nodes ORDER BY id")
        .unwrap()
        .query_map([], |r| r.get(0))
        .unwrap()
        .collect::<rusqlite::Result<_>>()
        .unwrap();
    let thought = agent_runtime::store::thinking_bytes(THINKING) as i64;
    assert_eq!(totals, [0, 0, 0, thought]);
    std::fs::remove_file(path).unwrap();
}

#[test]
fn lineage_checks_reject_a_node_on_another_branch_at_any_depth() {
    let mut db = db();
    db.create("Bob", Some("/synthetic"), binding()).unwrap();
    converse(&mut db, "Bob", 1);
    let shared = db.inspect("Bob").unwrap().head.unwrap();
    db.fork(
        "Bob",
        "Branch",
        Fork {
            checkpoint: Some(shared),
            workspace: Some("/synthetic"),
            ..Fork::default()
        },
    )
    .unwrap();
    converse(&mut db, "Bob", 2);
    for n in 3..6 {
        converse(&mut db, "Branch", n);
    }
    let bob = db.inspect("Bob").unwrap().head.unwrap();
    let branch = db.inspect("Branch").unwrap().head.unwrap();
    // Bob's head is shallower than the branch's: the walk down the branch
    // reaches that depth at another node. The branch's head is deeper than
    // anything on Bob's lineage.
    for (reader, node) in [("Branch", bob), ("Bob", branch)] {
        assert_eq!(
            read_item(&db, reader, node).unwrap_err().code,
            "item_not_in_bot_history"
        );
    }
    for reader in ["Bob", "Branch"] {
        assert_eq!(
            read_item(&db, reader, shared).unwrap()["content"][0]["text"],
            "r1"
        );
    }
    assert_eq!(
        read_item(&db, "Branch", branch).unwrap()["content"][0]["text"],
        "r5"
    );
}

#[test]
fn catch_up_steps_at_one_head_extend_the_summary_made_there() {
    let mut db = db();
    db.create("Bob", Some("/synthetic"), binding()).unwrap();
    converse(&mut db, "Bob", 1);
    let turn = db
        .begin(
            "Bob",
            "r2",
            "long task",
            true,
            &TurnOptions::default(),
            allow_provider,
        )
        .unwrap()
        .turn;
    let prompt = db.inspect("Bob").unwrap().head.unwrap();
    let calls: Vec<(i64, i64)> = (0..40)
        .map(|n| exchange(&mut db, turn, &format!("c{n}"), &lines(n, 50)))
        .collect();
    let head = db.inspect("Bob").unwrap().head;
    let max_bytes = 8192;
    let plan = |db: &Database, again: bool| match db
        .compaction_plan("Bob", 1, i64::MAX, max_bytes, 256, again)
        .unwrap()
    {
        None => None,
        Some(Planning::Plan(plan)) => Some(plan),
        Some(Planning::CatchUp(mut walk)) => {
            while !walk.done() {
                db.catch_up_piece(&mut walk, 16).unwrap();
            }
            db.catch_up_plan("Bob", walk).unwrap()
        }
    };
    // No model call answers between the steps, as when a round overflows
    // with a backlog several summarizer budgets long.
    let mut steps: Vec<(CompactionPlan, Value)> = Vec::new();
    while let Some(step) = plan(&db, !steps.is_empty()) {
        let event = db
            .compact(
                "Bob",
                &step,
                &format!("summary {}", steps.len()),
                None,
                0,
                agent_runtime::store::ContextUsage {
                    bytes: max_bytes as usize,
                    items: 256,
                },
                Value::Null,
            )
            .unwrap();
        if steps.is_empty() {
            // A resumed call at this head does not summarize again.
            assert!(plan(&db, false).is_none());
        } else {
            assert_eq!(
                step.previous_summary,
                Some(format!("summary {}", steps.len() - 1))
            );
            assert_eq!(step.ids.first(), Some(&steps.last().unwrap().0.cut));
        }
        let done = !step.catch_up;
        steps.push((step, event));
        assert!(steps.len() < 64, "catch-up does not converge");
        if done {
            break;
        }
    }
    assert!(steps.len() > 2);
    for (_, event) in &steps {
        assert_eq!(event["data"]["version"], json!(head));
        assert_eq!(event["data"]["previous"], Value::Null);
    }
    // The version at the head is the last step, cut at the newest round.
    let (last, _) = steps.last().unwrap();
    assert_eq!(
        (last.cut, last.pinned),
        (calls.last().unwrap().0, Some(prompt))
    );
    let bot = db.inspect("Bob").unwrap();
    assert_eq!(bot.compaction, head);
    let window = db.window("Bob", 1 << 20, 1024).unwrap().unwrap();
    assert_eq!(window.ids[..2], [prompt, last.cut]);
}

#[test]
fn a_version_a_fork_sees_is_not_extended_under_it() {
    let mut db = db();
    db.create("Bob", Some("/synthetic"), binding()).unwrap();
    let turn = db
        .begin(
            "Bob",
            "r1",
            "long task",
            true,
            &TurnOptions::default(),
            allow_provider,
        )
        .unwrap()
        .turn;
    for n in 0..40 {
        exchange(&mut db, turn, &format!("c{n}"), &lines(n, 100));
    }
    let head = db.inspect("Bob").unwrap().head;
    let limit = agent_runtime::store::ContextUsage {
        bytes: 8192,
        items: 256,
    };
    let plan = |db: &Database, again: bool| match db
        .compaction_plan("Bob", 1, i64::MAX, 8192, 256, again)
        .unwrap()
    {
        None => None,
        Some(Planning::Plan(plan)) => Some(plan),
        Some(Planning::CatchUp(mut walk)) => {
            while !walk.done() {
                db.catch_up_piece(&mut walk, 16).unwrap();
            }
            db.catch_up_plan("Bob", walk).unwrap()
        }
    };
    let first = plan(&db, false).unwrap();
    assert!(first.catch_up);
    db.compact("Bob", &first, "summary 0", None, 0, limit, Value::Null)
        .unwrap();
    let floor = db.elision_plan("Bob", 4096, 1).unwrap().unwrap();
    db.elide("Bob", &floor).unwrap();
    // The next step is planned; a fork at the head binds both versions
    // while its summary is being written.
    let second = plan(&db, true).unwrap();
    db.fork(
        "Bob",
        "Side",
        Fork {
            checkpoint: head,
            workspace: Some("/synthetic"),
            ..Fork::default()
        },
    )
    .unwrap();
    let seen = db.window("Side", 1 << 20, 1024).unwrap().unwrap();
    assert_eq!(
        db.compact("Bob", &second, "summary 1", None, 0, limit, Value::Null)
            .unwrap_err()
            .code,
        "compaction_version_shared"
    );
    assert!(plan(&db, true).is_none());
    let further = db.elision_plan("Bob", 1, 1).unwrap().unwrap();
    assert!(further.through > floor.through);
    assert_eq!(
        db.elide("Bob", &further).unwrap_err().code,
        "elision_version_shared"
    );
    let after = db.window("Side", 1 << 20, 1024).unwrap().unwrap();
    assert_eq!(after.ids, seen.ids);
    assert_eq!(after.elided, floor.through);
    assert_eq!(after.compaction.unwrap().summary, "summary 0");
}

#[test]
fn a_fork_narrows_what_it_may_call_and_never_widens() {
    let mut db = db();
    let tools = ["shell".to_owned(), "read".to_owned()];
    let gate = Gate {
        tag: "manual".into(),
        tools: vec!["shell".into(), "read".into()],
        expire_ms: None,
    };
    db.create(
        "Bob",
        Some("/synthetic"),
        Binding {
            tools: &tools,
            gate: Some(&gate),
            ..binding()
        },
    )
    .unwrap();
    converse(&mut db, "Bob", 1);
    let read = ["read".to_owned()];
    let fork = |allow| Fork {
        allow,
        workspace: Some("/synthetic"),
        ..Fork::default()
    };
    let (reader, forked) = db.fork("Bob", "Reader", fork(Some(&read))).unwrap();
    // Still shown every tool, so its requests keep its source's prefix.
    assert_eq!(
        (reader.tools.as_slice(), reader.callable()),
        (&tools[..], &read[..])
    );
    assert_eq!(forked.unwrap()["data"]["allowed"], json!(["read"]));
    // A call it may not make waits for no verdict: it is refused.
    let shell = ToolCall {
        name: "shell".into(),
        call_id: "c".into(),
        arguments: r#"{"command":"true"}"#.into(),
        custom: false,
    };
    let folder = std::path::Path::new("/synthetic");
    assert!(db.inspect("Bob").unwrap().gated(&shell, folder));
    assert!(!reader.gated(&shell, folder));
    let (plain, _) = db.fork("Reader", "Plain", Fork::default()).unwrap();
    assert_eq!(plain.allowed.as_deref(), Some(&read[..]));
    assert_eq!(
        db.fork("Reader", "Wide", fork(Some(&tools)))
            .unwrap_err()
            .code,
        "allow_not_in_source"
    );
    let (none, _) = db.fork("Reader", "None", fork(Some(&[]))).unwrap();
    assert_eq!(none.callable(), &[] as &[String]);
    let (all, _) = db.fork("Bob", "All", Fork::default()).unwrap();
    assert_eq!((all.allowed.as_deref(), all.callable()), (None, &tools[..]));
    let turn = db
        .begin(
            "Reader",
            "r",
            "work",
            true,
            &TurnOptions::default(),
            allow_provider,
        )
        .unwrap()
        .turn;
    let (item, call) = shell_call("fc_1", "s", "true");
    let entries = db.append(turn, vec![item], &[call], None).unwrap();
    assert!(entries.iter().all(|e| e["event"] != "approval_requested"));
    // In a round with a call it may make, only that one is announced.
    db.finish(turn, None).unwrap();
    let turn = db
        .begin(
            "Reader",
            "r2",
            "work",
            true,
            &TurnOptions::default(),
            allow_provider,
        )
        .unwrap()
        .turn;
    let (shell, s) = shell_call("fc_2", "s2", "true");
    let arguments = json!({"path":"x"}).to_string();
    let read = ToolCall {
        custom: false,
        name: "read".into(),
        call_id: "r2".into(),
        arguments: arguments.clone(),
    };
    let item = json!({"type":"function_call","id":"fc_3","call_id":"r2","name":"read","arguments":arguments});
    let entries = db
        .append(
            turn,
            vec![shell, serde_json::to_vec(&item).unwrap().into()],
            &[s, read],
            None,
        )
        .unwrap();
    let announced: Vec<&Value> = entries
        .iter()
        .filter(|e| e["event"] == "approval_requested")
        .flat_map(|e| e["data"]["calls"].as_array().unwrap())
        .collect();
    assert_eq!(announced.len(), 1);
    assert_eq!(announced[0]["name"], "read");
    let listed = db.list(None, 16, None, false).unwrap();
    let reader = listed["bots"]
        .as_array()
        .unwrap()
        .iter()
        .find(|b| b["name"] == "Reader")
        .unwrap();
    assert_eq!(reader["allowed"], json!(["read"]));
}

#[test]
fn schema_37_keeps_every_bot_its_tools() {
    let path = std::env::temp_dir().join(format!("agent-allowed-{}.sqlite", std::process::id()));
    let _ = std::fs::remove_file(&path);
    {
        let mut db = Database::initialize(Connection::open(&path).unwrap()).unwrap();
        db.create("Bob", Some("/synthetic"), binding()).unwrap();
    }
    Connection::open(&path)
        .unwrap()
        .execute_batch("ALTER TABLE bots DROP COLUMN allowed; PRAGMA user_version=36;")
        .unwrap();
    let db = Database::initialize(Connection::open(&path).unwrap()).unwrap();
    let bob = db.inspect("Bob").unwrap();
    assert_eq!((bob.allowed.as_deref(), bob.callable()), (None, &READ[..]));
    drop(db);
    let _ = std::fs::remove_file(&path);
}

#[test]
fn a_denied_call_closes_its_round() {
    let mut db = db();
    let turn = gated_turn(&mut db, None);
    let (item, call) = shell_call("fc_1", "s1", "true");
    db.append(turn, vec![item], std::slice::from_ref(&call), None)
        .unwrap();
    db.answer(Decision {
        bot: "Bob",
        turn,
        call_id: "s1",
        request: 1,
        tag: None,
        allow: false,
        unreviewed: false,
        reason: Some("no"),
        by: Some("test"),
    })
    .unwrap();
    assert!(matches!(
        db.approval_start(turn, &call, 0).unwrap(),
        Gated::Denied
    ));
    let denied = head(&db, "Bob");
    assert_eq!(fork_point(&mut db, "Bob", "after").unwrap(), denied);
}

#[test]
fn a_refused_call_is_read_after_the_words_it_came_after() {
    let mut db = db();
    let turn = gated_turn(&mut db, None);
    let refuse = |db: &mut Database, turn: i64, n: &str, command: &str| {
        let (item, call) = shell_call(&format!("fc_{n}"), n, command);
        db.append(turn, vec![item], std::slice::from_ref(&call), None)
            .unwrap();
        db.answer(Decision {
            bot: "Bob",
            turn,
            call_id: n,
            request: 1,
            tag: None,
            allow: false,
            unreviewed: false,
            reason: Some("no"),
            by: Some("test"),
        })
        .unwrap();
        assert!(matches!(
            db.approval_start(turn, &call, 0).unwrap(),
            Gated::Denied
        ));
        json!({"name":"shell","arguments":call.arguments})
    };
    let push = refuse(&mut db, turn, "s1", "git push origin main");
    // A person's steer, then another refusal: each refusal is listed with
    // the words it came after, so a go-ahead reads as following only the
    // refusals before it.
    let steer = TurnOptions {
        delivery: Delivery::Steer,
        ..TurnOptions::default()
    };
    let steered = db
        .begin("Bob", "r2", "yes", true, &steer, allow_provider)
        .unwrap()
        .turn;
    db.absorb(
        turn,
        None,
        8 << 20,
        4096,
        ContextUsage::default(),
        false,
        true,
    )
    .unwrap();
    let deploy = refuse(&mut db, turn, "s2", "make deploy");
    let read = db.prompts("Bob", turn, 64 * 1024).unwrap();
    assert_eq!(read["prompts"][0]["refused"], json!([push]));
    assert_eq!(read["prompts"][1]["turn"], steered);
    assert_eq!(read["prompts"][1]["refused"], json!([deploy]));
    db.finish(turn, None).unwrap();
    let next = db
        .begin(
            "Bob",
            "next",
            "approved",
            true,
            &TurnOptions::default(),
            allow_provider,
        )
        .unwrap()
        .turn;
    // An earlier turn's refusals sit with its prompt and steers the same way.
    let read = db.prompts("Bob", next, 64 * 1024).unwrap();
    assert!(read["prompts"][0].get("refused").is_none());
    assert_eq!(read["earlier"][0]["turn"], steered);
    assert_eq!(read["earlier"][0]["refused"], json!([deploy]));
    assert_eq!(read["earlier"][1]["refused"], json!([push]));
    // Cut to the budget like any text, after the words: the turn's prompt
    // and its steer, then its refusals in order.
    let entry = Database::PROMPTS_ENTRY;
    let budget = 8 + entry + 3 + entry + 4 + entry + 5;
    let cut = db.prompts("Bob", next, budget).unwrap();
    assert!(cut["earlier"][0].get("refused").is_none());
    let [first] = &cut["earlier"][1]["refused"].as_array().unwrap()[..] else {
        panic!("{}", cut["earlier"])
    };
    assert_eq!(first["arguments_truncated"], true);
    assert_eq!(first["arguments"].as_str().unwrap().len(), 5);
    // A call past its preview is marked cut, as a started call's is.
    let long = refuse(&mut db, next, "s3", &"x".repeat(4096));
    let previewed = db.prompts("Bob", next, 64 * 1024).unwrap();
    let [listed] = &previewed["prompts"][0]["refused"].as_array().unwrap()[..] else {
        panic!("{}", previewed["prompts"][0])
    };
    assert_eq!(listed["arguments_truncated"], true);
    assert_eq!(listed["arguments"].as_str().unwrap().len(), 2048);
    assert_eq!(listed["name"], long["name"]);
    // A refusal the turn then ran as it was, and finished, is no longer
    // listed: its go-ahead is used.
    refuse(&mut db, next, "s4", "make release");
    let (item, call) = shell_call("fc_s5", "s5", "make release");
    db.append(next, vec![item], std::slice::from_ref(&call), None)
        .unwrap();
    allow(&mut db, next, "s5").unwrap();
    assert!(matches!(
        db.approval_start(next, &call, 0).unwrap(),
        Gated::Started
    ));
    db.tool_finish(next, "s5", &result("released")).unwrap();
    let read = db.prompts("Bob", next, 64 * 1024).unwrap();
    let lengths: Vec<_> = read["prompts"][0]["refused"]
        .as_array()
        .unwrap()
        .iter()
        .map(|call| call["arguments"].as_str().unwrap().len())
        .collect();
    assert_eq!(lengths, [2048]);
}

#[test]
fn a_turn_reads_with_the_message_its_words_answer() {
    let mut db = db();
    db.create("Bob", Some("/synthetic"), binding()).unwrap();
    let begin = |db: &mut Database, key: &str| {
        db.begin(
            "Bob",
            key,
            "yes",
            true,
            &TurnOptions::default(),
            allow_provider,
        )
        .unwrap()
        .turn
    };
    let first = begin(&mut db, "r1");
    // A bot that never answered asks nothing.
    assert!(
        db.prompts("Bob", first, 64 * 1024).unwrap()["prompts"][0]
            .get("reply")
            .is_none()
    );
    let question = format!("{} Shall I run the tests?", "word ".repeat(1000));
    db.append(
        first,
        vec![assistant("Looking."), assistant(&question)],
        &[],
        None,
    )
    .unwrap();
    db.finish(first, None).unwrap();
    let second = begin(&mut db, "r2");
    // The last message, its end kept and starting at a whole word.
    let reply = &db.prompts("Bob", second, 64 * 1024).unwrap()["prompts"][0]["reply"];
    assert_eq!(reply["truncated"], true);
    let text = reply["text"].as_str().unwrap();
    assert!(text.ends_with("Shall I run the tests?") && text.starts_with("word "));
    assert!(text.len() <= 2048);
    // It counts against the words' budget like any text.
    let read = db.prompts("Bob", second, 3 + 12).unwrap();
    assert_eq!(read["prompts"][0]["reply"]["text"], "the tests?");
    db.finish(second, None).unwrap();
    // A secret is redacted before the end is kept, so a key whose opening
    // line falls outside it is not shown either.
    let third = begin(&mut db, "r3");
    let key = format!(
        "-----BEGIN PRIVATE KEY-----\n{}-----END PRIVATE KEY-----\nShall I use it?",
        "QUJDREVGR0hJSktMTU5PUFFSU1RVVldYWVo0MjQy\n".repeat(80)
    );
    db.append(third, vec![assistant(&key)], &[], None).unwrap();
    db.finish(third, None).unwrap();
    let fourth = begin(&mut db, "r4");
    let reply = &db.prompts("Bob", fourth, 64 * 1024).unwrap()["prompts"][0]["reply"];
    assert_eq!(reply["text"], "[secret: private key]\nShall I use it?");
    // A steer answers what the bot said since the words before it, and
    // only that.
    let steer = |db: &mut Database, key: &str| {
        let steer = TurnOptions {
            delivery: Delivery::Steer,
            ..TurnOptions::default()
        };
        db.begin("Bob", key, "yes", true, &steer, allow_provider)
            .unwrap();
        db.absorb(
            fourth,
            None,
            8 << 20,
            4096,
            ContextUsage::default(),
            false,
            true,
        )
        .unwrap();
    };
    // A call streamed after the question says nothing a person answers.
    let (call_item, call) = shell_call("fc_q", "q", "true");
    db.append(
        fourth,
        vec![assistant("Shall I deploy?"), call_item],
        std::slice::from_ref(&call),
        None,
    )
    .unwrap();
    steer(&mut db, "s1");
    steer(&mut db, "s2");
    let read = db.prompts("Bob", fourth, 64 * 1024).unwrap();
    assert_eq!(read["prompts"][1]["reply"]["text"], "Shall I deploy?");
    assert!(read["prompts"][2].get("reply").is_none());
}

#[test]
fn a_resent_keyed_creation_gets_its_bot_and_a_changed_one_names_the_field() {
    let mut db = db();
    let keyed = || Binding {
        request_id: Some("once"),
        ..binding()
    };
    let (bob, event) = db.create("Bob", Some("/synthetic"), keyed()).unwrap();
    assert!(event.is_some());
    // The bot has worked since; a resend still gets it, with no new event.
    converse(&mut db, "Bob", 1);
    let (again, event) = db.create("Bob", Some("/synthetic"), keyed()).unwrap();
    assert_eq!((again.bot_id, event), (bob.bot_id, None));
    assert_eq!(again.head, db.inspect("Bob").unwrap().head);
    let conflict = |db: &mut Database, binding: Binding<'_>, workspace: &str| {
        let error = db.create("Bob", Some(workspace), binding).unwrap_err();
        assert_eq!(error.code, "idempotency_conflict");
        error.facts.unwrap()["field"].clone()
    };
    let other = Binding {
        model: "other-model",
        ..keyed()
    };
    assert_eq!(conflict(&mut db, other, "/synthetic"), "model");
    assert_eq!(conflict(&mut db, keyed(), "/elsewhere"), "workspace");
    let told = Binding {
        instructions: "other text",
        ..keyed()
    };
    assert_eq!(conflict(&mut db, told, "/synthetic"), "instructions");
    // Another key, or none, is another creation of a taken name.
    for request_id in [Some("twice"), None] {
        let error = db
            .create(
                "Bob",
                Some("/synthetic"),
                Binding {
                    request_id,
                    ..binding()
                },
            )
            .unwrap_err();
        assert_eq!(error.code, "bot_exists");
    }
    // A bot made without a key is never mistaken for a resend.
    db.create("Plain", Some("/synthetic"), binding()).unwrap();
    let error = db.create("Plain", Some("/synthetic"), keyed()).unwrap_err();
    assert_eq!(error.code, "bot_exists");
    let created = db.events("Bob", 0, 10).unwrap()["events"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|e| e["event"] == "created")
        .count();
    assert_eq!(created, 1);
}

#[test]
fn a_resent_keyed_fork_gets_its_fork_wherever_the_source_moved() {
    let mut db = db();
    db.create("Bob", Some("/synthetic"), binding()).unwrap();
    converse(&mut db, "Bob", 1);
    let keyed = Fork {
        request_id: Some("branch"),
        ..Fork::default()
    };
    let (fork, event) = db.fork("Bob", "Side", keyed).unwrap();
    assert!(event.is_some());
    converse(&mut db, "Bob", 2);
    let (again, event) = db.fork("Bob", "Side", keyed).unwrap();
    assert_eq!(
        (again.bot_id, again.head, event),
        (fork.bot_id, fork.head, None)
    );
    let error = db
        .fork(
            "Bob",
            "Side",
            Fork {
                checkpoint: fork.head,
                ..keyed
            },
        )
        .unwrap_err();
    assert_eq!(error.code, "idempotency_conflict");
    assert_eq!(error.facts.unwrap()["field"], "checkpoint");
    // A bot created under the key is not a fork of it.
    let made = Binding {
        request_id: Some("branch"),
        ..binding()
    };
    db.create("Made", Some("/synthetic"), made).unwrap();
    let error = db.fork("Bob", "Made", keyed).unwrap_err();
    assert_eq!(error.facts.unwrap()["field"], "op");
    assert_eq!(
        db.fork("Bob", "Side", Fork::default()).unwrap_err().code,
        "bot_exists"
    );
}

#[test]
fn a_deleted_identity_is_gone_and_a_later_namesake_is_not() {
    let mut db = db();
    let (first, _) = db.create("Bob", Some("/synthetic"), binding()).unwrap();
    assert!(!db.gone("Bob", first.bot_id).unwrap());
    while db.delete_bot_piece("Bob", first.bot_id, 4).unwrap()["done"] != true {}
    let (second, _) = db.create("Bob", Some("/synthetic"), binding()).unwrap();
    let (alice, _) = db.create("Alice", Some("/synthetic"), binding()).unwrap();
    assert!(db.gone("Bob", first.bot_id).unwrap());
    assert!(!db.gone("Bob", second.bot_id).unwrap());
    // An identity never issued, another bot's, or one deleted under another
    // name is not this name's.
    for (name, id) in [
        ("Bob", alice.bot_id + 1),
        ("Other", second.bot_id),
        ("Bob", 0),
        ("Alice", first.bot_id),
        ("Other", first.bot_id),
    ] {
        assert_eq!(db.gone(name, id).unwrap_err().code, "bot_not_found");
    }
    // A keyed creation resent after its bot was deleted does not make it again.
    let keyed = || Binding {
        request_id: Some("once"),
        ..binding()
    };
    let (kay, _) = db.create("Kay", Some("/synthetic"), keyed()).unwrap();
    while db.delete_bot_piece("Kay", kay.bot_id, 4).unwrap()["done"] != true {}
    let error = db.create("Kay", Some("/synthetic"), keyed()).unwrap_err();
    assert_eq!(error.code, "bot_deleted");
    assert_eq!(error.facts.unwrap()["bot_id"], kay.bot_id);
    db.create("Kay", Some("/synthetic"), binding()).unwrap();
}

#[test]
fn schema_43_bots_were_made_without_a_key() {
    let path = std::env::temp_dir().join(format!("agent-creation-{}.sqlite", std::process::id()));
    let _ = std::fs::remove_file(&path);
    {
        let mut db = Database::initialize(Connection::open(&path).unwrap()).unwrap();
        db.create("Bob", Some("/synthetic"), binding()).unwrap();
    }
    Connection::open(&path)
        .unwrap()
        .execute_batch(
            "ALTER TABLE bots DROP COLUMN creation; DROP TABLE deleted_bots;
             PRAGMA user_version=43;",
        )
        .unwrap();
    let mut db = Database::initialize(Connection::open(&path).unwrap()).unwrap();
    let keyed = || Binding {
        request_id: Some("once"),
        ..binding()
    };
    assert_eq!(
        db.create("Bob", Some("/synthetic"), keyed())
            .unwrap_err()
            .code,
        "bot_exists"
    );
    db.create("Ann", Some("/synthetic"), keyed()).unwrap();
    assert!(
        db.create("Ann", Some("/synthetic"), keyed())
            .unwrap()
            .1
            .is_none()
    );
    drop(db);
    let version: i32 = Connection::open(&path)
        .unwrap()
        .query_row("PRAGMA user_version", [], |r| r.get(0))
        .unwrap();
    assert_eq!(version, Database::SCHEMA);
    std::fs::remove_file(path).unwrap();
}

#[test]
fn instructions_are_replaced_and_a_fork_keys_its_own_cache_after() {
    let mut db = db();
    let (made, _) = db.create("Ann", None, binding()).unwrap();
    let (fork, _) = db.fork("Ann", "Ben", Fork::default()).unwrap();
    assert_eq!(fork.cache_bot(), made.bot_id);
    db.set_thinking("Ben", 7, Strip::from(3), 0).unwrap();
    // The same text again changes nothing.
    assert!(!db.set_instructions("Ben", None, "test").unwrap());
    assert!(
        db.set_instructions("Ben", Some(fork.bot_id), "new rules")
            .unwrap()
    );
    let after = db.inspect("Ben").unwrap();
    assert_eq!(after.instructions, "new rules");
    assert_eq!(after.cache_bot(), fork.bot_id);
    // Thinking bound under the old instructions is withheld from now on.
    assert_eq!(after.thinking_prefix, None);
    assert_eq!(db.inspect("Ann").unwrap().instructions, "test");
    // Another identity under the name is refused.
    let refused = db
        .set_instructions("Ben", Some(made.bot_id), "x")
        .unwrap_err();
    assert_eq!(refused.code, "bot_not_found");
}

#[test]
fn a_bot_is_pointed_at_another_model_and_effort() {
    let mut db = db();
    db.create("Ann", None, binding()).unwrap();
    assert!(
        !db.set_model("Ann", None, "openai", "synthetic-model", None)
            .unwrap()
    );
    assert!(
        db.set_model("Ann", None, "openai", "other", Some("high"))
            .unwrap()
    );
    let ann = db.inspect("Ann").unwrap();
    assert_eq!(
        (ann.model.as_str(), ann.effort.as_deref()),
        ("other", Some("high"))
    );
    assert!(
        !db.set_model("Ann", None, "openai", "other", Some("high"))
            .unwrap()
    );
    assert!(db.set_model("Ann", None, "openai", "other", None).unwrap());
    assert_eq!(db.inspect("Ann").unwrap().effort, None);
}

#[test]
fn a_bot_changes_its_gate_even_one_another_bot_made() {
    let mut db = db();
    let ann = db.create("Ann", None, binding()).unwrap().0;
    let gate = |tool: &str| Gate {
        tag: "auto".into(),
        tools: vec![tool.into()],
        expire_ms: Some(45_000),
    };
    assert!(db.set_gate("Ann", None, Some(gate("read")), None).unwrap());
    assert_eq!(db.inspect("Ann").unwrap().gates, [gate("read")]);
    assert!(!db.set_gate("Ann", None, Some(gate("read")), None).unwrap());
    assert_eq!(
        db.set_gate("Ann", None, Some(gate("shell")), None)
            .unwrap_err()
            .code,
        "approve_not_in_tools"
    );
    assert!(db.set_gate("Ann", None, None, None).unwrap());
    assert!(db.inspect("Ann").unwrap().gates.is_empty());
    // A bot Ann makes takes her gate, and the person can still lift it.
    assert!(db.set_gate("Ann", None, Some(gate("read")), None).unwrap());
    db.create(
        "Bob",
        None,
        Binding {
            created_by: Some(&ann.name),
            created_by_id: Some(ann.bot_id),
            ..binding()
        },
    )
    .unwrap();
    assert_eq!(db.inspect("Bob").unwrap().gates, [gate("read")]);
    assert!(db.set_gate("Bob", None, None, None).unwrap());
    assert!(db.inspect("Bob").unwrap().gates.is_empty());
}

#[test]
fn turns_keep_the_model_they_ran_at_after_the_bot_moves() {
    let mut db = db();
    db.create("Bob", Some("/synthetic"), binding()).unwrap();
    converse(&mut db, "Bob", 1);
    assert!(
        db.set_model("Bob", None, "openai", "other", Some("high"))
            .unwrap()
    );
    let next = db
        .begin(
            "Bob",
            "r-next",
            "next",
            true,
            &TurnOptions::default(),
            allow_provider,
        )
        .unwrap()
        .turn;
    // The finished turn ran at the old model and effort; the new one runs
    // at the new.
    let listed = db.turns("Bob", 0, None, 10, false).unwrap();
    let rows = listed["turns"].as_array().unwrap();
    assert_eq!(
        (&rows[0]["model"], &rows[0]["effort"]),
        (&json!("openai/synthetic-model"), &Value::Null)
    );
    assert_eq!(
        (&rows[1]["model"], &rows[1]["effort"]),
        (&json!("openai/other"), &json!("high"))
    );
    // So it does not take the old model's cache as its own.
    let context = db.context(next).unwrap();
    assert_eq!(context.model, "openai/other");
    assert_eq!(
        context.previous_call,
        Some(("openai/synthetic-model".into(), None))
    );
}

#[test]
fn a_keyed_creation_resends_after_its_instructions_change() {
    let mut db = db();
    let made = || Binding {
        request_id: Some("once"),
        ..binding()
    };
    let (bot, _) = db.create("Bob", None, made()).unwrap();
    assert!(db.set_instructions("Bob", None, "new rules").unwrap());
    let (again, event) = db.create("Bob", None, made()).unwrap();
    assert_eq!((again.bot_id, event), (bot.bot_id, None));
    assert_eq!(again.instructions, "new rules");
    let changed = Binding {
        instructions: "new rules",
        ..made()
    };
    assert_eq!(
        db.create("Bob", None, changed).unwrap_err().code,
        "idempotency_conflict"
    );
}

#[test]
fn schema_48_creations_keep_the_instructions_they_were_sent_with() {
    let path = std::env::temp_dir().join(format!("agent-schema48-{}.sqlite", std::process::id()));
    let _ = std::fs::remove_file(&path);
    let made = || Binding {
        request_id: Some("once"),
        ..binding()
    };
    {
        let mut db = Database::initialize(Connection::open(&path).unwrap()).unwrap();
        db.create("Bob", Some("/synthetic"), made()).unwrap();
        converse(&mut db, "Bob", 1);
    }
    // Before 49 a creation compared the bot's own instructions, and a turn
    // read the bot's model.
    Connection::open(&path)
        .unwrap()
        .execute_batch(
            "UPDATE bots SET creation=json_remove(creation,'$.request.instructions');
             ALTER TABLE turns DROP COLUMN bot_model;
             ALTER TABLE turns DROP COLUMN bot_reasoning;
             PRAGMA user_version=48;",
        )
        .unwrap();
    let mut db = Database::initialize(Connection::open(&path).unwrap()).unwrap();
    assert!(db.set_instructions("Bob", None, "new rules").unwrap());
    assert!(db.set_model("Bob", None, "openai", "other", None).unwrap());
    let (_, event) = db.create("Bob", Some("/synthetic"), made()).unwrap();
    assert_eq!(event, None);
    let listed = db.turns("Bob", 0, None, 10, false).unwrap();
    assert_eq!(listed["turns"][0]["model"], "openai/synthetic-model");
    drop(db);
    let version: i32 = Connection::open(&path)
        .unwrap()
        .query_row("PRAGMA user_version", [], |r| r.get(0))
        .unwrap();
    assert_eq!(version, Database::SCHEMA);
    std::fs::remove_file(path).unwrap();
}

#[test]
fn an_expiring_fork_is_due_once_idle_that_long_and_never_while_a_turn_runs() {
    let mut db = db();
    db.create("Bob", Some("/synthetic"), binding()).unwrap();
    converse(&mut db, "Bob", 1);
    assert_eq!(db.next_expiry(0).unwrap(), (None, false));
    let before = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_millis() as i64;
    let (side, event) = db
        .fork(
            "Bob",
            "Bob.side",
            Fork {
                expire_ms: Some(900_000),
                request_id: Some("side"),
                ..Fork::default()
            },
        )
        .unwrap();
    assert_eq!(event.unwrap()["data"]["expire_ms"], 900_000);
    assert_eq!(side.expire_ms, Some(900_000));
    let at = side.expires_at.unwrap();
    assert!(at >= before + 900_000, "{at} from {before}");
    // Records show it; a source that never asked for one shows none.
    let listed = db.list(None, 10, None, false).unwrap();
    assert_eq!(listed["bots"][0]["name"], "Bob");
    assert!(listed["bots"][0].get("expires_at").is_none());
    assert_eq!(listed["bots"][1]["expires_at"], at);
    assert_eq!(listed["bots"][1]["expire_ms"], 900_000);
    assert!(
        serde_json::to_value(db.inspect("Bob").unwrap())
            .unwrap()
            .get("expire_ms")
            .is_none()
    );
    assert_eq!(db.next_expiry(at - 1).unwrap(), (Some(at), false));
    assert_eq!(db.next_expiry(at).unwrap(), (None, true));
    assert!(db.expired(at - 1, 16).unwrap().is_empty());
    assert_eq!(
        db.expired(at, 16).unwrap(),
        vec![("Bob.side".to_owned(), side.bot_id)]
    );
    // A turn running past its time keeps it; its end starts the time again.
    let turn = db
        .begin(
            "Bob.side",
            "q1",
            "what are you doing?",
            true,
            &TurnOptions::default(),
            allow_provider,
        )
        .unwrap()
        .turn;
    assert!(db.expired(i64::MAX, 16).unwrap().is_empty());
    db.append(turn, vec![assistant("fixing the cart")], &[], None)
        .unwrap();
    db.finish(turn, None).unwrap();
    let again = db.inspect("Bob.side").unwrap().expires_at.unwrap();
    assert!(again >= at, "{again} before {at}");
    // A resend gets the same fork; another time is another request.
    let (resent, event) = db
        .fork(
            "Bob",
            "Bob.side",
            Fork {
                expire_ms: Some(900_000),
                request_id: Some("side"),
                ..Fork::default()
            },
        )
        .unwrap();
    assert_eq!((resent.bot_id, event.is_none()), (side.bot_id, true));
    let error = db
        .fork(
            "Bob",
            "Bob.side",
            Fork {
                expire_ms: Some(60_000),
                request_id: Some("side"),
                ..Fork::default()
            },
        )
        .unwrap_err();
    assert_eq!(error.facts.unwrap()["field"], "expire_ms");
    // Deleting it leaves its source as it was.
    db.delete_bot("Bob.side").unwrap();
    assert_eq!(db.next_expiry(0).unwrap(), (None, false));
    assert!(db.expired(i64::MAX, 16).unwrap().is_empty());
    assert_eq!(db.inspect("Bob").unwrap().head, side.head);
}

#[test]
fn schema_50_bots_never_expire() {
    let path = std::env::temp_dir().join(format!("agent-schema50-{}.sqlite", std::process::id()));
    let _ = std::fs::remove_file(&path);
    let mut db = Database::initialize(Connection::open(&path).unwrap()).unwrap();
    db.create("Bob", Some("/synthetic"), binding()).unwrap();
    drop(db);
    Connection::open(&path)
        .unwrap()
        .execute_batch(
            "DROP INDEX bots_expiring; ALTER TABLE bots DROP COLUMN expire_ms;
             ALTER TABLE bots DROP COLUMN expires_at; PRAGMA user_version=50;",
        )
        .unwrap();
    let mut db = Database::initialize(Connection::open(&path).unwrap()).unwrap();
    let bob = db.inspect("Bob").unwrap();
    assert_eq!((bob.expire_ms, bob.expires_at), (None, None));
    assert_eq!(db.next_expiry(0).unwrap(), (None, false));
    converse(&mut db, "Bob", 1);
    let (side, _) = db
        .fork(
            "Bob",
            "Bob.side",
            Fork {
                expire_ms: Some(1000),
                ..Fork::default()
            },
        )
        .unwrap();
    assert_eq!(db.next_expiry(0).unwrap(), (side.expires_at, false));
    drop(db);
    std::fs::remove_file(path).unwrap();
}

#[test]
fn an_expired_fork_still_running_a_command_holds_back_no_other() {
    let mut db = db();
    db.create("Bob", Some("/synthetic"), binding()).unwrap();
    converse(&mut db, "Bob", 1);
    let expiring = Fork {
        expire_ms: Some(1000),
        ..Fork::default()
    };
    db.fork("Bob", "busy", expiring).unwrap();
    // Its turn leaves a command running, so `delete` would refuse it.
    let turn = db
        .begin(
            "busy",
            "r1",
            "work",
            true,
            &TurnOptions::default(),
            allow_provider,
        )
        .unwrap()
        .turn;
    let (item, bg) = call("bg");
    db.append(turn, vec![item], std::slice::from_ref(&bg), None)
        .unwrap();
    db.tool_start(turn, &bg).unwrap();
    let process = db.process_start(turn, "bg").unwrap();
    db.tool_finish(turn, "bg", &result(&format!("proc:{process}")))
        .unwrap();
    db.append(turn, vec![assistant("started")], &[], None)
        .unwrap();
    db.finish(turn, None).unwrap();
    let (idle, _) = db
        .fork(
            "Bob",
            "idle",
            Fork {
                expire_ms: Some(60_000),
                ..Fork::default()
            },
        )
        .unwrap();
    assert!(db.delete_bot("busy").is_err());
    // While the busy one is due, the idle one's time is still the next.
    let busy_at = db.inspect("busy").unwrap().expires_at.unwrap();
    assert_eq!(db.next_expiry(busy_at).unwrap(), (idle.expires_at, true));
    assert_eq!(
        db.expired(i64::MAX, 1).unwrap(),
        vec![("idle".to_owned(), idle.bot_id)]
    );
    db.process_finish(process, &json!({}), &[], &[]).unwrap();
    assert_eq!(db.expired(i64::MAX, 16).unwrap().len(), 2);
}

#[test]
fn steer_batches_bound_the_bytes_of_their_pictures() {
    let mut db = db();
    db.create("Bob", Some("/synthetic"), binding()).unwrap();
    let first = db
        .begin(
            "Bob",
            "first",
            "work",
            true,
            &TurnOptions::default(),
            allow_provider,
        )
        .unwrap()
        .turn;
    // Each picture is 3.2 MB of base64, so two fit a batch and the third
    // waits for the next.
    let shot = noise(900);
    assert!(shot.data.len() > 3 << 20);
    let mut submitted = Vec::new();
    for n in 0..3 {
        let options = TurnOptions {
            delivery: Delivery::Steer,
            images: vec![shot.clone()],
            ..TurnOptions::default()
        };
        submitted.push(
            db.begin(
                "Bob",
                &n.to_string(),
                "look",
                true,
                &options,
                allow_provider,
            )
            .unwrap()
            .turn,
        );
    }
    let absorb = |db: &mut agent_runtime::store::Database, through| {
        db.absorb(
            first,
            through,
            8 << 20,
            4096,
            ContextUsage::default(),
            false,
            true,
        )
        .unwrap()
    };
    let absorbed = absorb(&mut db, None);
    let seen: Vec<_> = absorbed.outcomes.iter().map(|(id, _)| *id).collect();
    assert_eq!(seen, submitted[..2]);
    let absorbed = absorb(&mut db, absorbed.next_through);
    assert_eq!(absorbed.outcomes[0].0, submitted[2]);
    assert!(absorbed.next_through.is_none());
}

#[test]
fn a_request_keeps_its_newest_pictures_and_older_ones_go_as_their_words() {
    let mut db = db();
    db.create("Bob", Some("/synthetic"), binding()).unwrap();
    let shot = noise(40);
    for n in 0..6 {
        let turn = db
            .begin(
                "Bob",
                &n.to_string(),
                &format!("look {n}"),
                true,
                &TurnOptions {
                    images: vec![shot.clone(); 20],
                    ..TurnOptions::default()
                },
                allow_provider,
            )
            .unwrap()
            .turn;
        db.append(turn, vec![assistant("seen")], &[], None).unwrap();
        db.finish(turn, None).unwrap();
    }
    let window = db.window("Bob", 8 << 20, 4096).unwrap().unwrap();
    assert_eq!(window.pictures, 120);
    sent(&db, &window);
    // The newest 50 stay, so the floor goes through the fourth prompt from
    // the end: two turns keep their pictures.
    let plan = db
        .picture_elision_plan("Bob", 50, i64::MAX)
        .unwrap()
        .unwrap();
    db.elide("Bob", &plan).unwrap();
    let window = db.window("Bob", 8 << 20, 4096).unwrap().unwrap();
    assert_eq!(window.pictures, 40);
    let items = sent(&db, &window);
    assert_eq!(
        items[0]["content"][0]["text"],
        "look 0\n[20 picture(s) sent with this message, elided from this request]"
    );
    assert_eq!(items[8]["content"].as_array().unwrap().len(), 21);
    // Their bytes beyond weight bound them too.
    let extra = i64::from(window.pictured[10]);
    assert!(extra > 0);
    let plan = db.picture_elision_plan("Bob", 50, extra).unwrap().unwrap();
    db.elide("Bob", &plan).unwrap();
    let window = db.window("Bob", 8 << 20, 4096).unwrap().unwrap();
    assert_eq!(window.pictures, 20);
    sent(&db, &window);
    // With nothing past the bound, the floor stays.
    assert!(
        db.picture_elision_plan("Bob", 50, i64::MAX)
            .unwrap()
            .is_none()
    );
}

#[test]
fn a_pinned_prompt_under_the_floor_counts_as_its_stub() {
    let mut db = db();
    db.create("Bob", Some("/synthetic"), binding()).unwrap();
    converse(&mut db, "Bob", 1);
    let turn = db
        .begin(
            "Bob",
            "r2",
            "long task",
            true,
            &TurnOptions {
                images: vec![noise(40); 20],
                ..TurnOptions::default()
            },
            allow_provider,
        )
        .unwrap()
        .turn;
    let prompt = db.inspect("Bob").unwrap().head.unwrap();
    for n in 0..6 {
        exchange(&mut db, turn, &format!("c{n}"), &lines(n, 50));
        if n == 0 {
            db.window("Bob", i64::MAX, i64::MAX).unwrap().unwrap();
        }
    }
    let plan = db
        .picture_elision_plan("Bob", 0, i64::MAX)
        .unwrap()
        .unwrap();
    db.elide("Bob", &plan).unwrap();
    let plan = compaction_plan(&db, "Bob", 1, 64 << 10, 256)
        .unwrap()
        .unwrap();
    assert_eq!(plan.pinned, Some(prompt));
    let limit = agent_runtime::store::ContextUsage {
        bytes: 64 << 10,
        items: 256,
    };
    db.compact("Bob", &plan, "summary", None, 0, limit, Value::Null)
        .unwrap();
    // The prompt goes ahead of the tail as its words, and the turn counts
    // it that way.
    let window = db.window("Bob", 64 << 10, 256).unwrap().unwrap();
    assert_eq!(window.ids[0], prompt);
    assert_eq!(window.pictures, 0);
    let items = sent(&db, &window);
    assert_eq!(
        items[0]["content"][0]["text"],
        "long task\n[20 picture(s) sent with this message, elided from this request]"
    );
    let (_, bytes, _) = db.turn_usage("Bob", turn).unwrap();
    assert_eq!(bytes as i64, window.item_bytes);
}

#[test]
fn a_cut_s_kept_pictures_count_toward_the_picture_bound() {
    let mut db = db();
    db.create("Bob", Some("/synthetic"), binding()).unwrap();
    converse(&mut db, "Bob", 1);
    let look = |delivery| TurnOptions {
        delivery,
        images: vec![noise(40); 20],
        ..TurnOptions::default()
    };
    let turn = db
        .begin(
            "Bob",
            "r2",
            "long task",
            true,
            &look(Delivery::default()),
            allow_provider,
        )
        .unwrap()
        .turn;
    let prompt = db.inspect("Bob").unwrap().head.unwrap();
    exchange(&mut db, turn, "c0", &lines(0, 50));
    db.window("Bob", i64::MAX, i64::MAX).unwrap().unwrap();
    db.begin(
        "Bob",
        "s",
        "this too",
        true,
        &look(Delivery::Steer),
        allow_provider,
    )
    .unwrap();
    db.absorb(
        turn,
        None,
        8 << 20,
        4096,
        ContextUsage::default(),
        false,
        true,
    )
    .unwrap();
    let steer = db.inspect("Bob").unwrap().head.unwrap();
    for n in 1..6 {
        exchange(&mut db, turn, &format!("c{n}"), &lines(n, 50));
    }
    let plan = compaction_plan(&db, "Bob", 1, 64 << 10, 256)
        .unwrap()
        .unwrap();
    assert_eq!(plan.pinned, Some(prompt));
    let limit = ContextUsage {
        bytes: 64 << 10,
        items: 256,
    };
    db.compact("Bob", &plan, "summary", None, 0, limit, Value::Null)
        .unwrap();
    let window = db.window("Bob", 64 << 10, 256).unwrap().unwrap();
    assert_eq!(&window.ids[..2], &[prompt, steer]);
    assert_eq!(window.pictures, 40);
    // The tail has none; the prompt's and the steer's are past the bound,
    // and any floor in the tail takes them.
    let plan = db
        .picture_elision_plan("Bob", 30, i64::MAX)
        .unwrap()
        .unwrap();
    db.elide("Bob", &plan).unwrap();
    let window = db.window("Bob", 64 << 10, 256).unwrap().unwrap();
    assert_eq!(window.pictures, 0);
    assert!(
        db.picture_elision_plan("Bob", 30, i64::MAX)
            .unwrap()
            .is_none()
    );
}

#[test]
fn usage_sums_turns_by_bot_model_and_bucket_from_where_each_started() {
    let path = std::env::temp_dir().join(format!("agent-usage-{}.sqlite", std::process::id()));
    let _ = std::fs::remove_file(&path);
    let mut db = Database::initialize(Connection::open(&path).unwrap()).unwrap();
    let spend = |input, cached, output| Usage {
        input_tokens: input,
        output_tokens: output,
        cached_input_tokens: cached,
        cache_write_tokens: 0,
        cache_write_1h_tokens: 0,
        sent_ms: 0,
        models: Vec::new(),
        served_model: String::new(),
        context_window: 0,
    };
    let hour = 3_600_000;
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_millis() as i64;
    let since = now - 3 * hour;
    // Each turn's spend and how long after `since` it started; the first
    // started before the window.
    let turns = [
        ("p.lead", "1", spend(500, 0, 50), -hour),
        ("p.lead", "2", spend(1000, 800, 100), 10),
        ("p.task", "1", spend(400, 300, 40), hour + 5),
        ("p.lead", "3", spend(200, 100, 20), hour + 10),
        ("q.lead", "1", spend(9000, 0, 900), 2 * hour),
    ];
    for bot in ["p.lead", "p.task", "q.lead"] {
        db.create(bot, Some("/synthetic"), binding()).unwrap();
    }
    let mut started = Vec::new();
    for (bot, request, usage, at) in &turns {
        // The task's turn is one a client started for the commons.
        let options = TurnOptions {
            origin: (*bot == "p.task").then(|| "commons".to_owned()),
            ..TurnOptions::default()
        };
        let turn = db
            .begin(bot, request, "work", true, &options, allow_provider)
            .unwrap()
            .turn;
        db.append(turn, vec![assistant("done")], &[], Some(usage))
            .unwrap();
        db.finish(turn, None).unwrap();
        started.push((turn, since + at));
    }
    let raw = Connection::open(&path).unwrap();
    for (turn, at) in started {
        raw.execute(
            "UPDATE turns SET started_ms=? WHERE id=?",
            rusqlite::params![at, turn],
        )
        .unwrap();
    }

    let usage = db.usage(Some("p.*"), since, Some(hour), 64).unwrap();
    assert_eq!(
        (
            usage["turns"].as_i64(),
            usage["input_tokens"].as_i64(),
            usage["cached_input_tokens"].as_i64(),
            usage["output_tokens"].as_i64()
        ),
        (Some(3), Some(1600), Some(1200), Some(160)),
        "the turn started before the window and the other project's are out"
    );
    let bots: Vec<_> = usage["bots"]
        .as_array()
        .unwrap()
        .iter()
        .map(|b| {
            (
                b["bot"].as_str().unwrap(),
                b["input_tokens"].as_i64().unwrap(),
            )
        })
        .collect();
    assert_eq!(
        bots,
        [("p.lead", 1200), ("p.task", 400)],
        "most spent first"
    );
    assert_eq!(usage["other_bots"], Value::Null);
    assert_eq!(usage["models"].as_array().unwrap().len(), 1);
    assert_eq!(usage["models"][0]["turns"], 3);
    let origins: Vec<_> = usage["origins"]
        .as_array()
        .unwrap()
        .iter()
        .map(|o| (o["origin"].clone(), o["input_tokens"].as_i64().unwrap()))
        .collect();
    assert_eq!(
        origins,
        [(Value::Null, 1200), (json!("commons"), 400)],
        "by the origin each turn was given, most spent first"
    );
    let series: Vec<_> = usage["series"]
        .as_array()
        .unwrap()
        .iter()
        .map(|s| {
            (
                s["at_ms"].as_i64().unwrap(),
                s["input_tokens"].as_i64().unwrap(),
            )
        })
        .collect();
    assert_eq!(series, [(since, 1000), (since + hour, 600)]);

    // Every bot, the one that spent most listed and the rest summed.
    let usage = db.usage(None, 0, None, 1).unwrap();
    assert_eq!(usage["turns"], 5);
    assert_eq!(usage["bots"][0]["bot"], "q.lead");
    assert_eq!(usage["other_bots"]["bots"], 2);
    assert_eq!(usage["other_bots"]["input_tokens"], 2100);
    assert!(usage.get("series").is_none());

    for (since, bucket) in [
        (0, Some(hour)),
        (since, Some(0)),
        (now - 2000 * hour, Some(hour)),
    ] {
        assert_eq!(
            db.usage(None, since, bucket, 64).unwrap_err().code,
            "invalid_usage_window"
        );
    }
    assert_eq!(
        db.usage(Some(""), 0, None, 64).unwrap_err().code,
        "invalid_bot_filter"
    );
    drop(raw);
    let _ = std::fs::remove_file(&path);
}

#[test]
fn a_waiting_step_ended_in_a_group_that_fails_is_told_of_once_one_commits() {
    let mut db = db();
    let run = Declared {
        name: "run".into(),
        description: "Run.".into(),
        parameters: json!({"type":"object"}),
        result: "ok".into(),
        answered_by: Some("sonny".into()),
        expire_ms: None,
        grammar: None,
        steps: true,
        inherit: Inherit::All,
    };
    let gate = Gate {
        tag: "manual".into(),
        tools: vec!["run".into()],
        expire_ms: None,
    };
    db.create(
        "Lead",
        Some("/synthetic"),
        Binding {
            declared: Some(std::slice::from_ref(&run)),
            gate: Some(&gate),
            ..binding()
        },
    )
    .unwrap();
    let turn = db
        .begin(
            "Lead",
            "request",
            "work",
            true,
            &TurnOptions::default(),
            allow_provider,
        )
        .unwrap()
        .turn;
    let call = ToolCall {
        name: "run".into(),
        call_id: "c1".into(),
        arguments: "{}".into(),
        custom: false,
    };
    db.append(turn, vec![], std::slice::from_ref(&call), None)
        .unwrap();
    db.tool_start(turn, &call).unwrap();
    use agent_runtime::store::{Publication, Step};
    let Step::Pending { approval, .. } = db
        .step_announce("Lead", turn, "c1", "1", "send", json!({}), 0)
        .unwrap()
        .step
    else {
        panic!("a gated step was decided at once");
    };
    let mut watermark = db.last_event_id().unwrap();
    let mut stepped = |db: &mut Database| {
        let mut told = Vec::new();
        db.publish_since(&mut watermark, |p| {
            if let Publication::Stepped(approval) = p {
                told.push(approval);
            }
            true
        })
        .unwrap();
        told
    };
    // The call's answer ends the step, but its group fails to commit:
    // the step still waits, and nothing is told.
    db.begin_group().unwrap();
    db.answer_call("Lead", turn, "c1", result("done"), 0)
        .unwrap();
    db.abandon_group().unwrap();
    assert!(stepped(&mut db).is_empty());
    // Answered again and committed, the step is told of.
    db.begin_group().unwrap();
    db.answer_call("Lead", turn, "c1", result("done"), 0)
        .unwrap();
    db.commit_group().unwrap();
    assert_eq!(stepped(&mut db), [approval]);
}

#[test]
fn each_item_says_when_it_joined_the_history() {
    let mut db = db();
    db.create("Bob", Some("/synthetic"), binding()).unwrap();
    let before = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_millis() as i64;
    let turn = db
        .begin(
            "Bob",
            "r1",
            "hello",
            true,
            &TurnOptions::default(),
            allow_provider,
        )
        .unwrap()
        .turn;
    db.append(turn, vec![assistant("hi")], &[], None).unwrap();
    db.finish(turn, None).unwrap();
    let page = db.history_nodes("Bob", None, 400, None, false).unwrap();
    let nodes: Vec<i64> = (page["nodes"].as_array().unwrap().iter())
        .map(|n| n["node"].as_i64().unwrap())
        .collect();
    let items = db.history_items("Bob", &nodes).unwrap();
    for (listed, read) in page["nodes"]
        .as_array()
        .unwrap()
        .iter()
        .zip(items["items"].as_array().unwrap())
    {
        let written = listed["written_ms"].as_i64().unwrap();
        assert!(written >= before, "{listed}");
        assert_eq!(read["written_ms"], written);
    }
}
