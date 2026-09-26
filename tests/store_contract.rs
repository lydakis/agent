use agent_runtime::{
    Error, Result,
    codec::Family,
    provider::{ToolCall, Usage},
    store::{Binding, Bot, CompactionPlan, Database, Delivery, Fork, Planning, TurnOptions},
    tools::Outcome,
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
fn binding() -> Binding<'static> {
    Binding {
        provider: "openai",
        family: Family::Responses,
        model: "synthetic-model",
        instructions: "test",
        reasoning: None,
        budget_tokens: None,
        tools: &[],
        created_by: None,
        created_by_id: None,
        compaction_instructions: None,
        compaction_model: None,
        fallbacks: false,
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
    match db.compaction_plan(name, keep, i64::MAX, max_bytes, max_items)? {
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
        note: None,
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
    // The fork carries no default directory; each of its turns names one.
    assert_eq!(
        db.begin(
            "Alternative",
            "r1",
            "different",
            true,
            &TurnOptions::default(),
            allow_provider
        )
        .unwrap_err()
        .code,
        "workspace_required"
    );
    let branch = TurnOptions {
        workspace: Some("/synthetic/alternative".into()),
        model: None,
        delivery: Delivery::Reject,
        expected_turn: None,
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
        db.item("Bob", db.inspect("Alternative").unwrap().head.unwrap())
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
    assert_eq!(
        db.begin(
            "Bob",
            "same",
            "changed",
            false,
            &TurnOptions::default(),
            allow_provider
        )
        .err()
        .unwrap()
        .code,
        "idempotency_conflict"
    );
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
                    fallbacks: false,
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
                    fallbacks: false,
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
                name: "write".into(),
                call_id: "c1".into(),
                arguments: "{}".into(),
            };
            // Keep a completed call in the same turn. A pruned migration
            // must repair only the unanswered suffix, not duplicate results.
            for id in ["done", "c1"] {
                let call = ToolCall {
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
        note: None,
    };
    let (_, entry) = db.tool_finish(turn, "c1", &outcome).unwrap();
    assert_eq!(entry["data"]["artifacts"][0], "stdout");
    assert_eq!(
        db.artifact("Bob", turn, "c1").unwrap()["stdout"],
        "full output"
    );
    assert!(db.artifact("Bob", turn, "missing").is_err());
    db.create("Other", Some("/synthetic"), binding()).unwrap();
    assert!(db.artifact("Other", turn, "c1").is_err());
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
    assert!(db.item("Bob", node).is_ok());
    assert!(db.item("Other", node).is_err());
    assert!(db.item("Bob", node + 1000).is_err());
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
            db.artifact(reader, turn, "c1").unwrap_err().code,
            "artifact_pruned"
        );
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
        db.artifact("Other", turn, "c1").unwrap_err().code,
        "turn_not_found"
    );
    assert_eq!(
        db.artifact("Fork", turn, "c9").unwrap_err().code,
        "turn_not_found"
    );
    assert_eq!(
        db.artifact("Bob", later, "c1").unwrap_err().code,
        "artifact_not_found"
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
        delivery: Delivery::Reject,
        expected_turn: None,
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
    let context = db.context(plain.turn).unwrap();
    assert_eq!(
        (context.workspace.as_str(), context.model.as_str()),
        ("/synthetic/default", "openai/synthetic-model")
    );
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
        delivery: Delivery::Reject,
        expected_turn: None,
    };
    let turn = db
        .begin("Nomad", "r1", "work", true, &options, allow_provider)
        .unwrap()
        .turn;
    assert_eq!(db.context(turn).unwrap().workspace, "/synthetic/today");
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
    let page = db.list(None, 256).unwrap();
    let bots = page["bots"].as_array().unwrap();
    assert!(!bots.is_empty() && bots.len() < 140);
    assert!(serde_json::to_vec(&page).unwrap().len() < 524288);
    assert!(bots.iter().all(|b| b.get("instructions").is_none()));
    let rest = db.list(page["next_after"].as_str(), 256).unwrap();
    assert_eq!(bots.len() + rest["bots"].as_array().unwrap().len(), 140);
    assert!(rest["next_after"].is_null());
    assert_eq!(db.inspect("bot-000").unwrap().instructions, instructions);
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
    let page = db.turns("Bob", 0, 1).unwrap();
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
    let rest = db.turns("Bob", turn, 64).unwrap();
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
            fallbacks: false,
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
            (Some("Creator"), Some(creator.id), "creator_not_found"),
            (Some("Creator"), None, "creator_identity_required"),
            (None, Some(creator.id), "creator_identity_required"),
        ] {
            let before = db.list(None, 64).unwrap();
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
            assert_eq!(db.list(None, 64).unwrap(), before);
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
    by_a.created_by_id = Some(first_a.id);
    let (b, _) = db.create("B", Some("/synthetic"), by_a).unwrap();
    assert_eq!(b.created_by_id, Some(first_a.id));
    db.delete_bot("A").unwrap();
    let (second_a, _) = db.create("A", Some("/synthetic"), binding()).unwrap();
    assert_ne!(second_a.id, first_a.id);
    let b = db.inspect("B").unwrap();
    assert_eq!(b.created_by.as_deref(), Some("A"));
    assert_eq!(
        b.created_by_id,
        Some(first_a.id),
        "B still names the A that made it"
    );
    let page = db.list(None, 64).unwrap();
    let listed = page["bots"].as_array().unwrap();
    let b_row = listed.iter().find(|r| r["name"] == "B").unwrap();
    assert_eq!(b_row["created_by_id"], first_a.id);
    assert_ne!(b_row["created_by_id"], second_a.id);
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
    assert_eq!(db.item("branch", older).unwrap()["role"], "user");
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
    for (name, workspace) in [("default", None), ("explicit", Some("/branch"))] {
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
        assert_eq!(fork.workspace.as_deref(), workspace);
        assert_eq!(event["data"]["workspace"], json!(workspace));
        let replay = db.events(name, 0, 10).unwrap();
        assert_eq!(replay["events"][0]["data"]["workspace"], json!(workspace));
    }
}

#[test]
fn forks_keep_the_binding_and_instructions_and_record_a_creator() {
    let mut db = db();
    let mut created = binding();
    created.instructions = "first text";
    let (parent, _) = db.create("Parent", Some("/synthetic"), binding()).unwrap();
    created.created_by = Some("Parent");
    created.created_by_id = Some(parent.id);
    let (bot, event) = db.create("Bob", Some("/synthetic"), created).unwrap();
    assert_eq!(bot.created_by.as_deref(), Some("Parent"));
    assert_eq!(event["data"]["created_by"], "Parent");
    // The event carries the validated creator identity.
    assert_eq!(bot.created_by_id, Some(parent.id));
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
                created_by_id: Some(bot.id),
                ..Fork::default()
            },
        )
        .unwrap();
    assert_eq!(same.instructions, "first text");
    assert_eq!(same.created_by.as_deref(), Some("Bob"));
    assert_eq!(
        same.created_by_id,
        Some(bot.id),
        "the creator's identity, not its name"
    );
    // Without a creator the fork records none; the source is untouched.
    let (changed, forked) = db.fork("Bob", "changed", Fork::default()).unwrap();
    assert_eq!(changed.instructions, "first text");
    assert_eq!(changed.created_by, None);
    assert_eq!(forked["data"]["created_by"], serde_json::Value::Null);
    assert_eq!(db.inspect("Bob").unwrap().instructions, "first text");
    // Listing carries the creator; a fork keeps the model and tools.
    let page = db.list(None, 64).unwrap();
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
    // While the turn runs, forking the moving head is refused; an explicit answered node is fine.
    assert_eq!(
        db.fork("Bob", "live", Fork { ..Fork::default() })
            .unwrap_err()
            .code,
        "bot_busy"
    );
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
                delivery: Delivery::Reject,
                expected_turn: None,
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
            fallbacks: false,
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
                fallbacks: false,
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
        db.turns("Bob", 0, 64).unwrap()["turns"]
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
        last = db.turns("Bob", 0, 64).unwrap()["turns"][1]["turn"]
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
            db.item("Bob", checkpoint).unwrap()["content"][0]["text"],
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
            db.item("Bob", checkpoint).unwrap_err().code,
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
            db.item("Bob", removed).unwrap_err().code,
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
        let (_, second) = db.create("Second", None, binding()).unwrap();
        let (_, third) = db.create("Third", None, binding()).unwrap();
        assert!(first["cursor"].as_i64().unwrap() < second["cursor"].as_i64().unwrap());
        deleted = second["cursor"].as_i64().unwrap();
        assert_eq!(third["cursor"], deleted + 1);
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
    let absorbed = db.absorb(first.turn, None, 8 << 20, 4096).unwrap();
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
        db.absorb(first.turn, None, 8 << 20, 4096)
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
    let result = db.absorb(first, None, 8 << 20, 4096).unwrap();
    assert_eq!(
        result
            .outcomes
            .iter()
            .map(|(id, _)| *id)
            .collect::<Vec<_>>(),
        [matched]
    );
    assert!(
        db.absorb(first, None, 8 << 20, 4096)
            .unwrap()
            .outcomes
            .is_empty()
    );
    assert!(db.steers_waiting("Bob").unwrap());
    db.finish(first, None).unwrap();
    db.start(moved, allow_provider).unwrap();
    assert_eq!(db.context(moved).unwrap().workspace, "/elsewhere");
    assert!(
        db.absorb(moved, None, 8 << 20, 4096)
            .unwrap()
            .outcomes
            .is_empty()
    );
    db.finish(moved, None).unwrap();
    db.start(changed, allow_provider).unwrap();
    assert_eq!(db.context(changed).unwrap().model, "openai/other");
    assert_eq!(
        db.absorb(changed, None, 8 << 20, 4096).unwrap().outcomes[0].0,
        inherited
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
            let absorbed = db.absorb(first, through, 8 << 20, 4096).unwrap();
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
            db.absorb(first, None, 8 << 20, 4096).unwrap().outcomes[0].0,
            late.unwrap()
        );
        assert!(
            db.absorb(first, None, 8 << 20, 4096)
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
            Publication::Finished { .. } => "finished",
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
            Publication::Finished { turn, .. } => format!("finished:{turn}"),
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
        db.turns("Bob", 0, 10).unwrap()["turns"]
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
    let absorbed = db.absorb(first.turn, None, 8 << 20, 4096).unwrap();
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
        db.absorb(plain.turn, None, 8 << 20, 4096)
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
    };
    let warm = Usage {
        input_tokens: 300,
        output_tokens: 10,
        cached_input_tokens: 240,
        cache_write_tokens: 0,
        cache_write_1h_tokens: 0,
        sent_ms: 0,
        models: Vec::new(),
    };
    db.append(turn, vec![assistant("one")], &[], Some(&cold))
        .unwrap();
    db.append(turn, vec![assistant("two")], &[], Some(&warm))
        .unwrap();
    db.finish(turn, None).unwrap();
    let listed = db.turns("Bob", 0, 10).unwrap();
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
    let page = db.list(None, 10).unwrap();
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
            let turns = db.turns("Bob", 0, 10).unwrap();
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
fn bot_identities_are_assigned_in_creation_order_and_never_reused() {
    let path = std::env::temp_dir().join(format!("agent-identity-{}.sqlite", std::process::id()));
    let _ = std::fs::remove_file(&path);
    {
        let mut db = Database::initialize(Connection::open(&path).unwrap()).unwrap();
        let (bob, event) = db.create("Bob", Some("/synthetic"), binding()).unwrap();
        assert_eq!((bob.id, event["data"]["id"].as_i64()), (1, Some(1)));
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
        assert_eq!((fork.id, event["data"]["id"].as_i64()), (2, Some(2)));
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
                .id,
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
        assert_eq!(db.inspect("Bob").unwrap().id, 1);
        assert_eq!(db.inspect("Fork").unwrap().id, 2);
        assert_eq!(
            db.create("New", Some("/synthetic"), binding())
                .unwrap()
                .0
                .id,
            3
        );
    }
    {
        let conn = Connection::open(&path).unwrap();
        conn.pragma_update(None, "user_version", 20).unwrap();
    }
    {
        let db = Database::initialize(Connection::open(&path).unwrap()).unwrap();
        assert_eq!(db.inspect("New").unwrap().id, 3);
        assert_eq!(db.list(None, 8).unwrap()["bots"][0]["id"], 1);
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
                let first = db.inspect("First").unwrap().id;
                let last = db.inspect("Last").unwrap().id;
                assert!(first > 0 && last > first);
                last
            };
            let created = db.create("New", Some("/synthetic"), binding()).unwrap().0;
            assert!(created.id > maximum);
            last = db
                .fork("New", "Fork", Fork { ..Fork::default() })
                .unwrap()
                .0
                .id;
            assert!(last > created.id);
            db.delete_bot("Fork").unwrap();
        }
        {
            let mut db = Database::initialize(Connection::open(&path).unwrap()).unwrap();
            assert!(db.create("AfterRestart", None, binding()).unwrap().0.id > last);
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
        assert_eq!(
            db.create("Bob", None, binding()).unwrap_err().code,
            "bot_exists"
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
    let id = db.inspect("Bob").unwrap().id;
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
        .id;
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
        .id;
    assert_ne!(original, replacement);
    converse(&mut db, "Bob", 3);
    let history = stored(&mut db, "Bob");
    assert_eq!(
        db.delete_bot_piece("Bob", original, 1).unwrap_err().code,
        "bot_not_found"
    );
    assert_eq!(db.inspect("Bob").unwrap().id, replacement);
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
        .id;
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
        .id;
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
    for (context_bytes, context_items) in [(4096usize, 4096usize), (8 << 20, 4)] {
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
            .absorb(first, None, context_bytes, context_items)
            .unwrap();
        let taken: Vec<i64> = absorbed.outcomes.iter().map(|(id, _)| *id).collect();
        assert_eq!(taken, steers[..2]);
        assert!(absorbed.next_through.is_none());
        // The third does not fit now and is not retried into a full turn.
        assert!(
            db.absorb(first, None, context_bytes, context_items)
                .unwrap()
                .outcomes
                .is_empty()
        );
        assert_eq!(db.turn_status("Bob", steers[2]).unwrap(), "queued");
        // With room it would have been taken: the budget is the only reason.
        assert_eq!(
            db.absorb(first, None, 8 << 20, 4096).unwrap().outcomes[0].0,
            steers[2]
        );
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
    db.absorb(bob, None, 8 << 20, 4096).unwrap();
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
            db.absorb(first, None, 8 << 20, 4096).unwrap().outcomes[0].0,
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
            db.item("branch", row["node"].as_i64().unwrap()).unwrap()
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
            note: Some(text.to_owned()),
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
            .compaction_plan("Bob", 1, i64::MAX, i64::MAX, 16)
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
            parent_id = parent.id;
            let mut b = binding();
            b.created_by = Some("Parent");
            b.created_by_id = Some(parent.id);
            b.compaction_instructions = Some("preserve decisions");
            db.create("Bob", Some("/synthetic"), b).unwrap();
            converse(&mut db, "Bob", 1);
            let bot = db.inspect("Bob").unwrap();
            id = bot.id;
            head = bot.head.unwrap();
            item = db.item("Bob", head).unwrap();
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
            assert_eq!((bot.id, bot.head), (id, Some(head)));
            assert_eq!(db.item("Bob", head).unwrap(), item);
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
            db.item("Bob", row["node"].as_i64().unwrap()).unwrap()
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
    assert_eq!((bob.cache_bot, bob.cache_bot()), (None, bob.id));
    // A fork repeats its source's prefix, so it shares the source's cache,
    // and so does a fork of that fork.
    db.fork("Bob", "Alice", Fork::default()).unwrap();
    db.fork("Alice", "Ann", Fork::default()).unwrap();
    for name in ["Alice", "Ann"] {
        assert_eq!(db.inspect(name).unwrap().cache_bot(), bob.id, "{name}");
    }
    drop(db);
    std::fs::remove_file(path).unwrap();
}

#[test]
fn a_store_keeps_its_identity_and_forks_inherit_fallbacks() {
    let dir = std::env::temp_dir().join(format!("agent-identity-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join("state.sqlite");
    let identity = {
        let mut db = Database::initialize(Connection::open(&path).unwrap()).unwrap();
        let identity = db.store_identity().unwrap();
        // Off unless the client asks; asked for, forks keep it.
        let plain = db.create("plain", None, binding()).unwrap().0;
        assert!(!plain.fallbacks);
        let mut asked = binding();
        asked.fallbacks = true;
        let bot = db.create("bot", None, asked).unwrap().0;
        assert!(bot.fallbacks);
        assert!(db.fork("bot", "fork", Fork::default()).unwrap().0.fallbacks);
        assert!(
            !db.fork("plain", "plain-fork", Fork::default())
                .unwrap()
                .0
                .fallbacks
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
/// the window counted for them.
fn sent(db: &Database, window: &agent_runtime::store::Window) -> Vec<Value> {
    let mut items = Vec::new();
    for (id, size) in window.ids.iter().zip(&window.sizes) {
        let bytes = db.items_by_ids(&[*id], 0, window.elided).unwrap();
        assert_eq!(bytes.len(), *size as usize, "node {id}");
        items.push(serde_json::from_slice(&bytes).unwrap());
    }
    let joined = db.items_by_ids(&window.ids, 0, window.elided).unwrap();
    assert_eq!(
        joined.len() as i64 + 1,
        window.item_bytes + window.ids.len() as i64
    );
    items
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
    // Moving the floor is forward only, once per head.
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
            let whole = db.result_lines("Bob", *id, 1, 5000).unwrap();
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
        db.result_lines("Other", rounds[0].1, 1, 10)
            .unwrap_err()
            .code,
        "result_not_found"
    );
    assert_eq!(
        db.result_lines("Bob", rounds[0].0, 1, 10).unwrap_err().code,
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
        let page = db.result_lines("Bob", node, offset, 5000).unwrap();
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
    let summarized = compaction_plan(&db, "Bob", 1, 64 << 10, 256)
        .unwrap()
        .unwrap();
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
    db.compact("Bob", &summarized, "summary", None, 0, limit)
        .unwrap();
    let window = db.window("Bob", 64 << 10, 256).unwrap().unwrap();
    assert_eq!(window.omitted_turns, 1);
    assert_eq!(window.unsummarized.bytes as i64, window.item_bytes);
    let (_, bytes, _) = db.turn_usage("Bob", turn).unwrap();
    assert_eq!(bytes as i64, window.item_bytes);
}
