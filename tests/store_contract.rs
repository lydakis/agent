use agent_runtime::{
    Error, Result,
    codec::Family,
    provider::{ToolCall, Usage},
    store::{Binding, Bot, Database, Delivery, TurnOptions},
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
    }
}
/// Every stored item of a bot, through the same window the runtime streams.
fn stored(db: &mut Database, name: &str) -> Vec<Value> {
    let Some(window) = db.window(name, i64::MAX, i64::MAX).unwrap() else {
        return Vec::new();
    };
    let joined = db.items_by_ids(&window.ids).unwrap();
    serde_json::from_slice(&[b"[", &joined[..], b"]"].concat()).unwrap()
}
fn result(output: &str) -> Outcome {
    Outcome {
        output: output.into(),
        artifacts: Vec::new(),
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
    db.fork("Bob", Some(checkpoint), "Alternative", None, None)
        .unwrap();
    assert_eq!(stored(&mut db, "Alternative").len(), 2);
    assert_eq!(stored(&mut db, "Bob").len(), 4);
    assert!(
        db.fork("Bob", Some(checkpoint + 1000), "bad", None, None)
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
        for error in ["cancelled", "process_interrupted", "provider_failed"] {
            let mut db = db();
            db.create(
                "Bob",
                Some("/synthetic"),
                Binding {
                    family,
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
    db.fork("Bob", Some(checkpoint), "Fork", None, None)
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
        db.fork("Bob", None, "branch", Some("/synthetic"), None)
            .unwrap();
        converse(&mut db, "branch", 4);
        converse(&mut db, "Bob", 5);
    }
    {
        let conn = Connection::open(&path).unwrap();
        conn.execute_batch(
            "DROP TABLE retained_turns;
             DROP TABLE node_sequence; DROP TABLE turn_sequence; DROP INDEX nodes_turn_seq; DROP INDEX nodes_parent; DROP INDEX bots_head;
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
        db.fork("Bob", Some(mid), "early", None, None)
            .unwrap_err()
            .code,
        "fork_point_has_open_tool_calls"
    );
    // While the turn runs, forking the moving head is refused; an explicit answered node is fine.
    assert_eq!(
        db.fork("Bob", None, "live", None, None).unwrap_err().code,
        "bot_busy"
    );
    db.tool_start(turn, &call).unwrap();
    let (_, entry) = db.tool_finish(turn, "c1", &result("hi")).unwrap();
    let answered = entry["data"]["node"].as_i64().unwrap();
    let (branch, _) = db
        .fork("Bob", Some(answered), "branch", None, None)
        .unwrap();
    assert_eq!(branch.head, Some(answered));
    assert_eq!(stored(&mut db, "branch").len(), 4);
    db.append(turn, vec![assistant("done")], &[], None).unwrap();
    db.finish(turn, None).unwrap();
    let (tip, _) = db.fork("Bob", None, "tip", None, None).unwrap();
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
        db.fork("Bob", Some(reasoning_node), "split", None, None)
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
                db.fork("Bob", Some(node), "partial", None, None)
                    .unwrap_err()
                    .code,
                "fork_point_has_open_tool_calls"
            );
        } else {
            db.fork("Bob", Some(node), "answered", None, None).unwrap();
            assert_eq!(
                serde_json::to_vec(&stored(&mut db, "answered")[1]).unwrap(),
                reasoning
            );
            // A validated intermediate checkpoint is also safe for another branch.
            db.fork("answered", None, "nested", None, None).unwrap();
        }
    }
    // A reasoning-only completion must not bypass the boundary check either.
    db.append(turn, vec![reasoning], &[], None).unwrap();
    db.finish(turn, None).unwrap();
    assert_eq!(
        db.fork("Bob", None, "unpaired_head", None, None)
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
                db.fork("Bob", Some(node), "partial", None, None)
                    .unwrap_err()
                    .code,
                "fork_point_has_open_tool_calls"
            );
        } else {
            db.fork("Bob", Some(node), "answered", None, None).unwrap();
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
    let joined = db.items_by_ids(&all.ids).unwrap();
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
    db.fork("Bob", None, "branch", Some("/synthetic"), None)
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
    let replay = db.items_by_ids(&window.ids).unwrap();
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
    assert_eq!(db.items_by_ids(&window.ids).unwrap(), replay);
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
        let replay = db.items_by_ids(&window.ids).unwrap();
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
        assert_eq!(db.items_by_ids(&window.ids).unwrap(), replay);
    }
}

#[test]
fn deleting_a_bot_frees_only_its_exclusive_history() {
    let mut db = db();
    db.create("Bob", Some("/synthetic"), binding()).unwrap();
    for n in 1..=3 {
        converse(&mut db, "Bob", n);
    }
    db.fork("Bob", None, "branch", Some("/synthetic"), None)
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
        db.fork("Bob", None, "branch", Some("/synthetic"), None)
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
            db.fork("Bob", Some(checkpoint), "stale", None, None)
                .unwrap_err()
                .code,
            "node_not_in_source_history"
        );
        // Deleting the newest branch must also preserve its IDs while an
        // older bot survives and appends more messages.
        db.fork("Bob", None, "newer", Some("/synthetic"), None)
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
    let absorbed = db.absorb(first.turn, None).unwrap();
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
    assert!(db.absorb(first.turn, None).unwrap().outcomes.is_empty());

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
    let result = db.absorb(first, None).unwrap();
    assert_eq!(
        result
            .outcomes
            .iter()
            .map(|(id, _)| *id)
            .collect::<Vec<_>>(),
        [matched]
    );
    assert!(db.absorb(first, None).unwrap().outcomes.is_empty());
    assert!(db.steers_waiting("Bob").unwrap());
    db.finish(first, None).unwrap();
    db.start(moved, allow_provider).unwrap();
    assert_eq!(db.context(moved).unwrap().workspace, "/elsewhere");
    assert!(db.absorb(moved, None).unwrap().outcomes.is_empty());
    db.finish(moved, None).unwrap();
    db.start(changed, allow_provider).unwrap();
    assert_eq!(db.context(changed).unwrap().model, "openai/other");
    assert_eq!(db.absorb(changed, None).unwrap().outcomes[0].0, inherited);
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
            let absorbed = db.absorb(first, through).unwrap();
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
        assert_eq!(db.absorb(first, None).unwrap().outcomes[0].0, late.unwrap());
        assert!(db.absorb(first, None).unwrap().outcomes.is_empty());
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
    let absorbed = db.absorb(first.turn, None).unwrap();
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
    assert!(db.absorb(plain.turn, None).unwrap().outcomes.is_empty());
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
    };
    let warm = Usage {
        input_tokens: 300,
        output_tokens: 10,
        cached_input_tokens: 240,
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
                        output_tokens: 10,
                    }),
                )
                .unwrap();
            }
            db.finish(first, None).unwrap();
            db.fork("Bob", None, "Fork", Some("/synthetic"), None)
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
                    output_tokens: 10,
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
            .fork("Bob", None, "Fork", Some("/synthetic"), None)
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
            last = db.fork("New", None, "Fork", None, None).unwrap().0.id;
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
