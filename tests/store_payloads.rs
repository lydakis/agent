use agent_runtime::{
    codec::Family,
    store::{Binding, Database, Delivery, Fork, TurnOptions},
};
use rusqlite::{Connection, params};

fn binding(family: Family) -> Binding<'static> {
    Binding {
        provider: "test",
        family,
        model: "test",
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

#[test]
fn shared_prompts_survive_queue_steer_restart_and_source_deletion() {
    for family in [Family::Responses, Family::Anthropic] {
        let path = std::env::temp_dir().join(format!(
            "agent-payload-{}-{}.sqlite",
            std::process::id(),
            family.name()
        ));
        let _ = std::fs::remove_file(&path);
        let prompt = "é\0🙂 exact\n".repeat(1024);
        let plain = TurnOptions::default();
        let queue = TurnOptions {
            delivery: Delivery::Queue,
            ..Default::default()
        };
        let steer = TurnOptions {
            delivery: Delivery::Steer,
            ..Default::default()
        };
        let (queued, first);
        {
            let mut db = Database::initialize(Connection::open(&path).unwrap()).unwrap();
            db.create("bot", Some("/synthetic"), binding(family))
                .unwrap();
            first = db
                .begin("bot", "first", &prompt, true, &plain, |_, _| Ok(()))
                .unwrap()
                .turn;
            queued = db
                .begin("bot", "queued", &prompt, true, &queue, |_, _| Ok(()))
                .unwrap()
                .turn;
            db.begin("bot", "steer", &prompt, true, &steer, |_, _| Ok(()))
                .unwrap();
            assert_eq!(
                db.absorb(first, None, 8 << 20, 4096)
                    .unwrap()
                    .outcomes
                    .len(),
                1
            );
            for (id, options) in [("first", &plain), ("queued", &queue), ("steer", &steer)] {
                assert!(
                    !db.begin("bot", id, &prompt, true, options, |_, _| Ok(()))
                        .unwrap()
                        .fresh
                );
                assert!(
                    db.begin("bot", id, "changed", true, options, |_, _| Ok(()))
                        .is_err()
                );
            }
            let conn = Connection::open(&path).unwrap();
            let bytes: i64 = conn
                .query_row(
                    "SELECT sum(length(CAST(prompt AS BLOB))) FROM turns",
                    [],
                    |r| r.get(0),
                )
                .unwrap();
            // Only the not-yet-started turn needs an inline prompt copy.
            assert_eq!(bytes, prompt.len() as i64);
        }
        {
            let mut db = Database::initialize(Connection::open(&path).unwrap()).unwrap();
            assert!(
                !db.begin("bot", "first", &prompt, true, &plain, |_, _| Ok(()))
                    .unwrap()
                    .fresh
            );
            assert_eq!(db.pending().unwrap(), (1, prompt.len() as i64));
            db.start(queued, |_, _| Ok(())).unwrap();
            assert_eq!(db.pending().unwrap(), (0, 0));
            db.finish(queued, None).unwrap();
            db.fork("bot", "fork", Fork::default()).unwrap();
            let before = db.window("fork", i64::MAX, i64::MAX).unwrap().unwrap();
            let bytes = db.items_by_ids(&before.ids, 0).unwrap();
            db.delete_bot("bot").unwrap();
            assert_eq!(db.items_by_ids(&before.ids, 0).unwrap(), bytes);
            let decoded: serde_json::Value =
                serde_json::from_slice(&[b"[", &bytes[..], b"]"].concat()).unwrap();
            assert_eq!(decoded[0]["content"][0]["text"], prompt);
            db.delete_bot("fork").unwrap();
            let conn = Connection::open(&path).unwrap();
            assert_eq!(
                conn.query_row("SELECT count(*) FROM nodes", [], |r| r.get::<_, i64>(0))
                    .unwrap(),
                0
            );
        }
        std::fs::remove_file(path).unwrap();
    }
}

#[test]
fn old_prompt_copies_migrate_without_touching_queued_work() {
    let prompt = "é\0exact".repeat(1024);
    let path = std::env::temp_dir().join(format!(
        "agent-prompt-migrate-{}.sqlite",
        std::process::id()
    ));
    let _ = std::fs::remove_file(&path);
    {
        let mut db = Database::initialize(Connection::open(&path).unwrap()).unwrap();
        db.create("bot", Some("/synthetic"), binding(Family::Responses))
            .unwrap();
        db.begin(
            "bot",
            "first",
            &prompt,
            true,
            &TurnOptions::default(),
            |_, _| Ok(()),
        )
        .unwrap();
        db.begin(
            "bot",
            "queued",
            "pending",
            true,
            &TurnOptions {
                delivery: Delivery::Queue,
                ..Default::default()
            },
            |_, _| Ok(()),
        )
        .unwrap();
    }
    {
        let conn = Connection::open(&path).unwrap();
        conn.execute(
            "UPDATE turns SET prompt=? WHERE request_id='first'",
            params![&prompt],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO artifacts(turn,call_id,stream,data) VALUES (1,'legacy','stdout',?)",
            params![b"legacy bytes".as_slice()],
        )
        .unwrap();
        conn.execute_batch("DROP INDEX IF EXISTS turns_prompt_node; ALTER TABLE turns DROP COLUMN prompt_node; ALTER TABLE artifacts DROP COLUMN raw_bytes; PRAGMA user_version=25;").unwrap();
    }
    let mut db = Database::initialize(Connection::open(&path).unwrap()).unwrap();
    assert!(
        !db.begin(
            "bot",
            "first",
            &prompt,
            true,
            &TurnOptions::default(),
            |_, _| Ok(())
        )
        .unwrap()
        .fresh
    );
    assert_eq!(db.pending().unwrap(), (1, 7));
    assert_eq!(
        db.artifact("bot", 1, "legacy").unwrap()["stdout"],
        "legacy bytes"
    );
    let conn = Connection::open(&path).unwrap();
    assert_eq!(
        conn.query_row(
            "SELECT sum(length(CAST(prompt AS BLOB))) FROM turns",
            [],
            |r| r.get::<_, i64>(0)
        )
        .unwrap(),
        7
    );
    drop(db);
    drop(conn);
    std::fs::remove_file(path).unwrap();
}

#[test]
fn large_artifacts_remain_exact_after_reopen_and_background_completion() {
    use agent_runtime::{provider::ToolCall, tools::Outcome};
    let path = std::env::temp_dir().join(format!(
        "agent-artifact-codec-{}.sqlite",
        std::process::id()
    ));
    let _ = std::fs::remove_file(&path);
    let data = "é\0🙂 record\n".repeat(60000);
    let turn;
    {
        let mut db = Database::initialize(Connection::open(&path).unwrap()).unwrap();
        db.create("bot", Some("/synthetic"), binding(Family::Responses))
            .unwrap();
        turn = db
            .begin(
                "bot",
                "first",
                "work",
                true,
                &TurnOptions::default(),
                |_, _| Ok(()),
            )
            .unwrap()
            .turn;
        let call = ToolCall {
            name: "shell".into(),
            call_id: "call".into(),
            arguments: "{}".into(),
        };
        db.append(turn, vec![], std::slice::from_ref(&call), None)
            .unwrap();
        db.tool_start(turn, &call).unwrap();
        db.tool_finish(
            turn,
            "call",
            &Outcome {
                output: "preview".into(),
                artifacts: vec![("stdout", data.as_bytes().to_vec())],
                note: None,
            },
        )
        .unwrap();
        let process = db.process_start(turn, "background").unwrap();
        db.process_finish(
            process,
            &serde_json::json!({"success":true}),
            &[("stderr", data.as_bytes().to_vec())],
        )
        .unwrap();
        db.finish(turn, None).unwrap();
        let conn = Connection::open(&path).unwrap();
        let bytes: i64 = conn
            .query_row("SELECT sum(length(data)) FROM artifacts", [], |r| r.get(0))
            .unwrap();
        assert!(
            bytes < data.len() as i64 / 2,
            "physical payload bytes: {bytes}"
        );
    }
    {
        let mut db = Database::initialize(Connection::open(&path).unwrap()).unwrap();
        assert_eq!(db.artifact("bot", turn, "call").unwrap()["stdout"], data);
        assert_eq!(
            db.artifact("bot", turn, "background").unwrap()["stderr"],
            data
        );
        let mut offset = 0;
        let mut reconstructed = String::new();
        loop {
            let page = db
                .artifact_page("bot", turn, "call", "stdout", offset, 16385)
                .unwrap();
            reconstructed.push_str(page["text"].as_str().unwrap());
            offset = page["next_offset"].as_u64().unwrap();
            if page["done"] == true {
                break;
            }
        }
        assert_eq!(reconstructed, data);
        let page = db
            .artifact_page("bot", turn, "call", "stdout", data.len() as u64, 4)
            .unwrap();
        assert_eq!(page["text"], "");
        assert!(
            db.artifact_page("bot", turn, "call", "stdout", 1, 4)
                .is_err()
        );
        assert_eq!(
            db.artifact_lines("bot", turn, "call", "stdout", 4, 2)
                .unwrap(),
            "     4\té\0🙂 record\n     5\té\0🙂 record\n[showing lines 4-5 of 60000]\n"
        );
        let next = db
            .begin("bot", "next", "", true, &TurnOptions::default(), |_, _| {
                Ok(())
            })
            .unwrap()
            .turn;
        db.finish(next, None).unwrap();
        db.prune("bot", 1).unwrap();
        assert_eq!(
            db.artifact("bot", turn, "call").unwrap_err().code,
            "artifact_pruned"
        );
    }
    std::fs::remove_file(path).unwrap();
}
