use agent_runtime::{
    Error,
    codec::Family,
    provider::ToolCall,
    store::{Binding, Database, TurnOptions},
    tools::Outcome,
};
use bytes::Bytes;
use rusqlite::Connection;
use serde_json::json;

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
    }
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
    db.fork("Bob", checkpoint, "Alternative", None).unwrap();
    assert_eq!(db.load("Alternative").unwrap().len(), 2);
    assert_eq!(db.load("Bob").unwrap().len(), 4);
    assert!(db.fork("Bob", checkpoint - 1, "bad", None).is_err());
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
    assert_ne!(
        db.load("Alternative").unwrap().items(),
        db.load("Bob").unwrap().items()
    );
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
    assert_eq!(db.load("Bob").unwrap().len(), 2);
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
