use agent_runtime::{
    Error,
    codec::Family,
    provider::ToolCall,
    store::{Binding, Database, TurnOptions},
    tools::Outcome,
};
use bytes::Bytes;
use rusqlite::Connection;
use serde_json::{Value, json};

fn assistant(text: &str) -> Bytes {
    serde_json::to_vec(&json!({"type":"message","role":"assistant",
        "content":[{"type":"output_text","text":text}]}))
    .unwrap()
    .into()
}
fn db() -> Database {
    Database::initialize(Connection::open_in_memory().unwrap(), "test").unwrap()
}
fn binding() -> Binding<'static> {
    Binding {
        provider: "openai",
        family: Family::Responses,
        model: "synthetic-model",
        instructions: "test",
        reasoning: None,
        budget_tokens: None,
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
        .begin("Bob", "r1", "first", true, &TurnOptions::default())
        .unwrap()
        .turn;
    db.append(first, vec![assistant("answer one")], &[], None)
        .unwrap();
    let checkpoint = db.finish(first, None).unwrap().last().unwrap()["data"]["checkpoint"]
        .as_i64()
        .unwrap();
    let second = db
        .begin("Bob", "r2", "second", true, &TurnOptions::default())
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
            &TurnOptions::default()
        )
        .unwrap_err()
        .code,
        "workspace_required"
    );
    let branch = TurnOptions {
        workspace: Some("/synthetic/alternative".into()),
        model: None,
    };
    let alt = db
        .begin("Alternative", "r1", "different", true, &branch)
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
        .begin("Bob", "same", "work", true, &TurnOptions::default())
        .unwrap();
    assert!(started.fresh);
    let retry = db
        .begin("Bob", "same", "work", true, &TurnOptions::default())
        .unwrap();
    assert!(!retry.fresh);
    assert_eq!(started.turn, retry.turn);
    assert!(
        db.begin("Bob", "same", "different", true, &TurnOptions::default())
            .is_err()
    );
    assert!(
        db.begin("Bob", "other", "work", true, &TurnOptions::default())
            .is_err()
    );
    db.append(started.turn, vec![assistant("done")], &[], None)
        .unwrap();
    db.finish(started.turn, None).unwrap();
    assert!(
        !db.begin("Bob", "same", "work", true, &TurnOptions::default())
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
        .begin("Bob", "same", "work", true, &TurnOptions::default())
        .unwrap()
        .turn;
    let retry = db
        .begin("Bob", "same", "work", false, &TurnOptions::default())
        .unwrap();
    assert!(!retry.fresh);
    assert_eq!(retry.turn, turn);
    assert_eq!(
        db.begin("Bob", "same", "changed", false, &TurnOptions::default())
            .err()
            .unwrap()
            .code,
        "idempotency_conflict"
    );
    assert_eq!(
        db.begin("Other", "fresh", "work", false, &TurnOptions::default())
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
        db.begin("Other", "fresh", "work", true, &TurnOptions::default())
            .unwrap()
            .fresh
    );
}

#[test]
fn unrecorded_tool_outcomes_block_automatic_reexecution() {
    let mut db = db();
    db.create("Bob", Some("/synthetic"), binding()).unwrap();
    let turn = db
        .begin("Bob", "request", "work", true, &TurnOptions::default())
        .unwrap()
        .turn;
    let call = ToolCall {
        name: "echo".into(),
        call_id: "c1".into(),
        arguments: r#"{"text":"hi"}"#.into(),
    };
    db.append(turn, vec![], std::slice::from_ref(&call), None)
        .unwrap();
    db.tool_start(turn, &call).unwrap();
    assert_eq!(
        db.finish(turn, Some(&Error::new("process_interrupted")))
            .unwrap()
            .last()
            .unwrap()["data"]["status"],
        "uncertain"
    );
    assert!(
        db.begin("Bob", "retry", "work", true, &TurnOptions::default())
            .is_err()
    );
    assert_eq!(db.inspect("Bob").unwrap().status, "uncertain");
}

#[test]
fn tool_results_and_cursor_events_commit_together() {
    let mut db = db();
    db.create("Bob", Some("/synthetic"), binding()).unwrap();
    let turn = db
        .begin("Bob", "request", "work", true, &TurnOptions::default())
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
        .begin("Bob", "request", "work", true, &TurnOptions::default())
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
}

#[test]
fn turn_options_are_recorded_and_part_of_idempotency() {
    let mut db = db();
    db.create("Bob", Some("/synthetic/default"), binding())
        .unwrap();
    let options = TurnOptions {
        workspace: Some("/synthetic/elsewhere".into()),
        model: Some("openai/other-model".into()),
    };
    let started = db.begin("Bob", "r1", "work", true, &options).unwrap();
    let accepted = started.entry.unwrap();
    assert_eq!(accepted["data"]["workspace"], "/synthetic/elsewhere");
    assert_eq!(accepted["data"]["model"], "openai/other-model");
    let context = db.context(started.turn).unwrap();
    assert_eq!(
        (context.workspace.as_str(), context.model.as_str()),
        ("/synthetic/elsewhere", "openai/other-model")
    );
    assert!(!db.begin("Bob", "r1", "work", true, &options).unwrap().fresh);
    assert_eq!(
        db.begin("Bob", "r1", "work", true, &TurnOptions::default())
            .unwrap_err()
            .code,
        "idempotency_conflict"
    );
    db.finish(started.turn, None).unwrap();
    let plain = db
        .begin("Bob", "r2", "work", true, &TurnOptions::default())
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
    let bot = db.create("Nomad", None, binding()).unwrap();
    assert!(bot.workspace.is_none());
    assert_eq!(
        db.begin("Nomad", "r1", "work", true, &TurnOptions::default())
            .unwrap_err()
            .code,
        "workspace_required"
    );
    let options = TurnOptions {
        workspace: Some("/synthetic/today".into()),
        model: None,
    };
    let turn = db
        .begin("Nomad", "r1", "work", true, &options)
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
        .begin("Bob", "r1", "work", true, &TurnOptions::default())
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
        .begin("Bob", "r2", "more", true, &TurnOptions::default())
        .unwrap()
        .turn;
    db.append(second, vec![assistant("two")], &[], Some(&usage))
        .unwrap();
    db.finish(second, None).unwrap();
    assert_eq!(
        db.begin("Bob", "r3", "again", true, &TurnOptions::default())
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
fn stores_carry_a_schema_version_and_migrate_older_ones_forward() {
    let conn = Connection::open_in_memory().unwrap();
    conn.execute_batch("CREATE TABLE bots(name TEXT PRIMARY KEY)")
        .unwrap();
    assert_eq!(
        Database::initialize(conn, "test").err().unwrap().code,
        "store_schema_unsupported"
    );
    let conn = Connection::open_in_memory().unwrap();
    conn.pragma_update(None, "user_version", Database::SCHEMA + 1)
        .unwrap();
    assert_eq!(
        Database::initialize(conn, "test").err().unwrap().code,
        "store_schema_newer"
    );
    let conn = Connection::open_in_memory().unwrap();
    let version: i32 = {
        let _db = Database::initialize(conn, "test").unwrap();
        Database::SCHEMA
    };
    assert_eq!(version, Database::SCHEMA);
    // A version-6 store (no turn ordinals) is migrated forward at open:
    // ordinals are rebuilt from the accepted events, so windows and history
    // reads work on the old data.
    let path = std::env::temp_dir().join(format!("agent-migrate-{}.sqlite", std::process::id()));
    let _ = std::fs::remove_file(&path);
    {
        let mut db = Database::initialize(Connection::open(&path).unwrap(), "test").unwrap();
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
        Database::initialize(conn, "test").err().unwrap().code,
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
    let mut db = Database::initialize(Connection::open(&path).unwrap(), "test").unwrap();
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
        .begin("Bob", "r1", "work", true, &TurnOptions::default())
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
    let branch = db
        .fork("Bob", Some(answered), "branch", None, None)
        .unwrap();
    assert_eq!(branch.head, Some(answered));
    assert_eq!(stored(&mut db, "branch").len(), 4);
    db.append(turn, vec![assistant("done")], &[], None).unwrap();
    db.finish(turn, None).unwrap();
    let tip = db.fork("Bob", None, "tip", None, None).unwrap();
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
            },
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
        .begin("Bob", "r", "work", true, &TurnOptions::default())
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
        .begin("Bob", "first", "hello", true, &TurnOptions::default())
        .unwrap()
        .turn;
    let reply =
        serde_json::to_vec(&json!({"role":"assistant", "content":[{"type":"text","text":"hi"}]}))
            .unwrap()
            .into();
    db.append(first, vec![reply], &[], None).unwrap();
    db.finish(first, None).unwrap();
    let turn = db
        .begin("Bob", "tools", "work", true, &TurnOptions::default())
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
        .begin("Bob", "live", "p13", true, &TurnOptions::default())
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
        .begin("Bob", "long", &prompt, true, &TurnOptions::default())
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
            .begin(name, "r1", "prompt", true, &TurnOptions::default())
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
        .begin("Bob", "live", "p", true, &TurnOptions::default())
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
        let mut db = Database::initialize(Connection::open(&path).unwrap(), "test").unwrap();
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
        let mut db = Database::initialize(conn, "test").unwrap();
        let turn = db
            .begin("branch", "next", "work", true, &TurnOptions::default())
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
        let mut db = Database::initialize(Connection::open(&path).unwrap(), "test").unwrap();
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
        let mut db = Database::initialize(conn, "test").unwrap();
        assert_eq!(
            db.item("Bob", checkpoint).unwrap()["content"][0]["text"],
            "r1"
        );
        db.delete_bot("Bob").unwrap();
    }
    {
        let mut db = Database::initialize(Connection::open(&path).unwrap(), "test").unwrap();
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
        .begin("Bob", "bg", "work", true, &TurnOptions::default())
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
        let mut db = Database::initialize(Connection::open(&path).unwrap(), "test").unwrap();
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
    let mut db = Database::initialize(conn, "test").unwrap();
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
    let mut db = Database::initialize(conn, "test").unwrap();
    assert_eq!(db.prune("Bob", 3).unwrap()["events"], 0);
    converse(&mut db, "Bob", 4);
    assert_eq!(db.prune("Bob", 1).unwrap()["events"], 3);
    assert_eq!(db.events("Alice", 0, 256).unwrap(), alice);
    drop(db);
    for suffix in ["", "-wal", "-shm"] {
        let _ = std::fs::remove_file(format!("{}{suffix}", path.display()));
    }
}
