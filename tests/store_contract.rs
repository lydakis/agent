use agent_runtime::{provider::ToolCall, store::Database};
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

#[test]
fn historical_fork_and_exact_resume_preserve_independent_lineage() {
    let mut db = db();
    assert!(db.inspect("missing").is_err());
    db.create("Bob", "/synthetic/bob").unwrap();
    let first = db.begin("Bob", "r1", "first", true).unwrap().turn;
    db.append(first, vec![assistant("answer one")], &[])
        .unwrap();
    let checkpoint = db.finish(first, None).unwrap()["data"]["checkpoint"]
        .as_i64()
        .unwrap();
    let second = db.begin("Bob", "r2", "second", true).unwrap().turn;
    db.append(second, vec![assistant("answer two")], &[])
        .unwrap();
    db.finish(second, None).unwrap();
    db.fork("Bob", checkpoint, "Alternative", "/synthetic/alternative")
        .unwrap();
    assert_eq!(db.load("Alternative").unwrap().len(), 2);
    assert_eq!(db.load("Bob").unwrap().len(), 4);
    assert!(db.fork("Bob", checkpoint - 1, "bad", "/synthetic").is_err());
    let alt = db
        .begin("Alternative", "r1", "different", true)
        .unwrap()
        .turn;
    db.append(alt, vec![assistant("another direction")], &[])
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
    db.create("Bob", "/synthetic").unwrap();
    let started = db.begin("Bob", "same", "work", true).unwrap();
    assert!(started.fresh);
    let retry = db.begin("Bob", "same", "work", true).unwrap();
    assert!(!retry.fresh);
    assert_eq!(started.turn, retry.turn);
    assert!(db.begin("Bob", "same", "different", true).is_err());
    assert!(db.begin("Bob", "other", "work", true).is_err());
    db.append(started.turn, vec![assistant("done")], &[])
        .unwrap();
    db.finish(started.turn, None).unwrap();
    assert!(!db.begin("Bob", "same", "work", true).unwrap().fresh);
    assert_eq!(db.load("Bob").unwrap().len(), 2);
    assert!(
        db.append(started.turn, vec![assistant("late")], &[])
            .is_err()
    );
}

#[test]
fn admission_reconciles_retries_without_accepting_fresh_work_at_capacity() {
    let mut db = db();
    db.create("Bob", "/synthetic").unwrap();
    db.create("Other", "/synthetic").unwrap();
    let turn = db.begin("Bob", "same", "work", true).unwrap().turn;
    let retry = db.begin("Bob", "same", "work", false).unwrap();
    assert!(!retry.fresh);
    assert_eq!(retry.turn, turn);
    assert_eq!(
        db.begin("Bob", "same", "changed", false).err().unwrap().0,
        "idempotency_conflict"
    );
    assert_eq!(
        db.begin("Other", "fresh", "work", false).err().unwrap().0,
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
    assert!(db.begin("Other", "fresh", "work", true).unwrap().fresh);
}

#[test]
fn unrecorded_tool_outcomes_block_automatic_reexecution() {
    let mut db = db();
    db.create("Bob", "/synthetic").unwrap();
    let turn = db.begin("Bob", "request", "work", true).unwrap().turn;
    let call = ToolCall {
        name: "echo".into(),
        call_id: "c1".into(),
        arguments: r#"{"text":"hi"}"#.into(),
    };
    db.append(turn, vec![], &[call]).unwrap();
    db.tool_start(turn, "c1", "echo").unwrap();
    assert_eq!(
        db.finish(turn, Some("process_interrupted")).unwrap()["data"]["status"],
        "uncertain"
    );
    assert!(db.begin("Bob", "retry", "work", true).is_err());
    assert_eq!(db.inspect("Bob").unwrap().status, "uncertain");
}

#[test]
fn tool_results_and_cursor_events_commit_together() {
    let mut db = db();
    db.create("Bob", "/synthetic").unwrap();
    let turn = db.begin("Bob", "request", "work", true).unwrap().turn;
    let call = ToolCall {
        name: "echo".into(),
        call_id: "c1".into(),
        arguments: "{}".into(),
    };
    db.append(turn, vec![], &[call]).unwrap();
    assert!(db.tool_finish(turn, "c1", "too early").is_err());
    db.tool_start(turn, "c1", "echo").unwrap();
    db.tool_finish(turn, "c1", "hi").unwrap();
    assert!(db.tool_start(turn, "c1", "echo").is_err());
    db.append(turn, vec![assistant("done")], &[]).unwrap();
    assert_eq!(
        db.finish(turn, None).unwrap()["data"]["status"],
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
