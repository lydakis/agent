#![cfg(unix)]
use agent_runtime::tools::{Credentials, Registry};
use serde_json::{Value, json};

#[tokio::test]
async fn shell_preserves_exit_status_and_separate_outputs() {
    let tools = Registry::new("echo,shell").unwrap();
    let cwd = std::env::temp_dir();
    let prepared = tools
        .prepare(
            "shell",
            &json!({"command":"printf out; printf err >&2; exit 7","timeout_ms":1000}).to_string(),
        )
        .unwrap();
    let output: Value =
        serde_json::from_str(&tools.execute(prepared, &cwd, &[]).await.unwrap().output).unwrap();
    assert_eq!(output["stdout"], "out");
    assert_eq!(output["stderr"], "err");
    assert_eq!(output["exit_code"], 7);
    assert!(
        Registry::new("echo")
            .unwrap()
            .prepare("shell", r#"{"command":"true"}"#)
            .is_err()
    );
    assert!(
        tools
            .prepare("shell", r#"{"command":"true","timeout_ms":0}"#)
            .is_err()
    );
}

#[tokio::test]
async fn shell_timeout_and_output_overflow_are_bounded() {
    let tools = Registry::new("echo,shell").unwrap();
    let cwd = std::env::temp_dir();
    let run = |command: &str, timeout: u64| {
        let prepared = tools
            .prepare(
                "shell",
                &json!({"command":command,"timeout_ms":timeout}).to_string(),
            )
            .unwrap();
        tokio::time::timeout(
            std::time::Duration::from_secs(2),
            tools.execute(prepared, &cwd, &[]),
        )
    };
    // A timed-out command still shows what it wrote before it was killed.
    let output: Value = serde_json::from_str(
        &run("printf before; printf warn >&2; sleep 10", 300)
            .await
            .unwrap()
            .unwrap()
            .output,
    )
    .unwrap();
    assert_eq!(output["stdout"], "before");
    assert_eq!(output["stderr"], "warn");
    assert_eq!(output["timed_out"], true);
    assert_eq!(output["success"], false);
    assert!(output["exit_code"].is_null());
    let overflow = run("yes x", 1000).await.unwrap().unwrap_err();
    assert_eq!(overflow.code, "shell_output_limit");
}

#[tokio::test]
async fn known_credential_text_is_redacted_before_tool_result_serialization() {
    let sentinel = "synthetic-\"test-value";
    let tools = Registry::new("echo,shell")
        .unwrap()
        .exclude_credential("AGENT_TEST_FAKE_KEY", sentinel);
    let echo = tools
        .prepare("echo", &json!({"text":sentinel}).to_string())
        .unwrap();
    assert!(
        tools
            .execute(echo, &std::env::temp_dir(), &[])
            .await
            .unwrap()
            .output
            == "[REDACTED]"
    );
    let shell = tools.prepare("shell", &json!({"command": "printf '%s' 'synthetic-\"test-value'; printf '%s' 'synthetic-\"test-value' >&2"}).to_string()).unwrap();
    let output = tools
        .execute(shell, &std::env::temp_dir(), &[])
        .await
        .unwrap()
        .output;
    let result: Value = serde_json::from_str(&output).unwrap();
    assert!(result["stdout"] == "[REDACTED]");
    assert!(result["stderr"] == "[REDACTED]");
    let binary = tools
        .prepare(
            "shell",
            &json!({"command":"printf '\\377'; printf 'synthetic-\"test-value'"}).to_string(),
        )
        .unwrap();
    let outcome = tools
        .execute(binary, &std::env::temp_dir(), &[])
        .await
        .unwrap();
    let result: Value = serde_json::from_str(&outcome.output).unwrap();
    assert_eq!(result["stdout"], "\u{fffd}[REDACTED]");
}

#[tokio::test]
async fn rotated_overlapping_credentials_are_fully_redacted() {
    let credentials = Credentials::default();
    credentials.set("AGENT_TEST_FAKE_KEY", "synthetic-prefix");
    credentials.set("AGENT_OTHER_FAKE_KEY", "other-secret");
    credentials.set("AGENT_TEST_FAKE_KEY", "synthetic-prefix-rotated");
    let tools = Registry::new("echo")
        .unwrap()
        .with_credentials(credentials.clone());
    let echo = tools
        .prepare(
            "echo",
            &json!({"text":"synthetic-prefix-rotated"}).to_string(),
        )
        .unwrap();
    assert_eq!(
        tools
            .execute(echo, &std::env::temp_dir(), &[])
            .await
            .unwrap()
            .output,
        "[REDACTED]"
    );
    assert_eq!(
        credentials.names(),
        ["AGENT_TEST_FAKE_KEY", "AGENT_OTHER_FAKE_KEY"]
    );
}

#[tokio::test]
async fn file_tools_read_write_and_edit_within_the_workspace() {
    let tools = Registry::new("read,write,edit,shell").unwrap();
    let dir = std::env::temp_dir().join(format!("agent-file-tools-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let write = tools
        .prepare(
            "write",
            &json!({"path":"src/lib.rs","content":"one\ntwo\nthree\n"}).to_string(),
        )
        .unwrap();
    assert!(
        tools
            .execute(write, &dir, &[])
            .await
            .unwrap()
            .output
            .contains("wrote")
    );
    std::os::unix::fs::symlink("src/lib.rs", dir.join("alias.rs")).unwrap();
    let read = tools
        .prepare(
            "read",
            &json!({"path":"alias.rs","offset":2,"limit":1}).to_string(),
        )
        .unwrap();
    let shown = tools.execute(read, &dir, &[]).await.unwrap().output;
    assert!(shown.starts_with("     2\ttwo\n"), "{shown}");
    assert!(shown.contains("showing lines 2-2 of 3"));
    let ambiguous = tools
        .prepare(
            "edit",
            &json!({"path":"src/lib.rs","old":"\n","new":" "}).to_string(),
        )
        .unwrap();
    assert_eq!(
        tools.execute(ambiguous, &dir, &[]).await.unwrap_err().code,
        "edit_target_ambiguous"
    );
    let edit = tools
        .prepare(
            "edit",
            &json!({"path":"alias.rs","old":"two","new":"2"}).to_string(),
        )
        .unwrap();
    assert!(
        tools
            .execute(edit, &dir, &[])
            .await
            .unwrap()
            .output
            .contains("replaced 1")
    );
    assert_eq!(
        std::fs::read_to_string(dir.join("src/lib.rs")).unwrap(),
        "one\n2\nthree\n"
    );
    // Expansion must be rejected without mutating the file. A separate
    // allocation probe verifies that rejection precedes building the output.
    let oversized = tools
        .prepare(
            "edit",
            &json!({"path":"src/lib.rs",
        "old":"\n","new":"x".repeat(1024*1024),"replace_all":true})
            .to_string(),
        )
        .unwrap();
    // Three replacements fit; five replacements exceed the file bound.
    std::fs::write(dir.join("src/lib.rs"), "a\nb\nc\nd\ne\n").unwrap();
    assert_eq!(
        tools.execute(oversized, &dir, &[]).await.unwrap_err().code,
        "tool_output_limit"
    );
    assert_eq!(
        std::fs::read_to_string(dir.join("src/lib.rs")).unwrap(),
        "a\nb\nc\nd\ne\n"
    );
    let rewrite = tools
        .prepare(
            "write",
            &json!({"path":"alias.rs","content":"short"}).to_string(),
        )
        .unwrap();
    tools.execute(rewrite, &dir, &[]).await.unwrap();
    assert_eq!(
        std::fs::read_to_string(dir.join("src/lib.rs")).unwrap(),
        "short"
    );
    assert!(dir.join("alias.rs").is_symlink());
    let missing = tools
        .prepare("read", &json!({"path":"nope.txt"}).to_string())
        .unwrap();
    assert_eq!(
        tools.execute(missing, &dir, &[]).await.unwrap_err().code,
        "file_unreadable"
    );
    // Oversized shell output keeps a preview and retains the full stream.
    let big = tools
        .prepare(
            "shell",
            &json!({"command":"head -c 200000 /dev/zero | tr '\\0' 'y'"}).to_string(),
        )
        .unwrap();
    let outcome = tools.execute(big, &dir, &[]).await.unwrap();
    let result: Value = serde_json::from_str(&outcome.output).unwrap();
    assert!(result["stdout"].as_str().unwrap().contains("bytes omitted"));
    assert_eq!(outcome.artifacts[0].0, "stdout");
    assert_eq!(outcome.artifacts[0].1.len(), 200000);
    std::fs::remove_dir_all(dir).unwrap();
}

#[tokio::test]
async fn read_distinguishes_long_lines_from_eof_and_keeps_pages_bounded() {
    let tools = Registry::new("read").unwrap();
    let dir = std::env::temp_dir().join(format!("agent-read-lines-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let read = |offset| {
        tools
            .prepare("read", &json!({"path":"data","offset":offset}).to_string())
            .unwrap()
    };
    std::fs::write(dir.join("data"), "x".repeat(80_000)).unwrap();
    let error = tools.execute(read(1), &dir, &[]).await.unwrap_err();
    assert_eq!(error.code, "read_line_too_long");
    assert!(error.detail.unwrap().contains("line 1"));
    assert!(
        tools
            .execute(read(2), &dir, &[])
            .await
            .unwrap()
            .output
            .contains("past the last line 1")
    );
    std::fs::write(
        dir.join("data"),
        format!("ok\n{}\ntail\n", "x".repeat(80_000)),
    )
    .unwrap();
    let page = tools.execute(read(1), &dir, &[]).await.unwrap().output;
    assert!(page.starts_with("     1\tok\n"));
    assert!(page.contains("offset=2"));
    assert_eq!(
        tools.execute(read(2), &dir, &[]).await.unwrap_err().code,
        "read_line_too_long"
    );
    assert!(
        tools
            .execute(read(3), &dir, &[])
            .await
            .unwrap()
            .output
            .contains("tail")
    );
    std::fs::write(dir.join("data"), "abcdefgh\n".repeat(5000)).unwrap();
    let page = tools
        .prepare("read", &json!({"path":"data","limit":5000}).to_string())
        .unwrap();
    let output = tools.execute(page, &dir, &[]).await.unwrap().output;
    assert!(output.len() <= agent_runtime::tools::PREVIEW_BYTES);
    assert!(output.contains("continue with offset="));
    std::fs::remove_dir_all(dir).unwrap();
}

#[tokio::test]
async fn detached_commands_run_in_their_own_session_and_write_only_where_told() {
    let tools = Registry::new("echo,shell").unwrap();
    assert!(
        tools
            .prepare(
                "shell",
                r#"{"command":"true","detach":true,"background":true}"#
            )
            .is_err()
    );
    let dir = std::env::temp_dir().join(format!("agent-detach-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let prepared = tools
        .prepare(
            "shell",
            &json!({"command":"echo lost; echo up > up.log; exec sleep 30","detach":true})
                .to_string(),
        )
        .unwrap();
    let output: Value =
        serde_json::from_str(&tools.execute(prepared, &dir, &[]).await.unwrap().output).unwrap();
    assert_eq!(output.as_object().unwrap().len(), 2);
    let pid = output["pid"].as_i64().unwrap() as i32;
    // Its own session, so no group kill of the calling command reaches it.
    assert_eq!(unsafe { libc::getsid(pid) }, pid);
    let mut logged = String::new();
    for _ in 0..100 {
        logged = std::fs::read_to_string(dir.join("up.log")).unwrap_or_default();
        if !logged.is_empty() {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }
    assert_eq!(logged, "up\n");
    assert_eq!(unsafe { libc::kill(pid, 0) }, 0);
    unsafe {
        libc::kill(pid, libc::SIGKILL);
    }
    std::fs::remove_dir_all(dir).unwrap();
}

#[tokio::test]
async fn detached_commands_are_bounded_and_reaped() {
    let tools = Registry::new("shell").unwrap().with_detached_budget(2);
    let dir = std::env::temp_dir().join(format!("agent-detach-bound-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let detach = |command: &str| {
        tools
            .prepare(
                "shell",
                &json!({"command":command,"detach":true}).to_string(),
            )
            .unwrap()
    };
    let mut pids = Vec::new();
    for _ in 0..2 {
        let output: Value = serde_json::from_str(
            &tools
                .execute(detach("exec sleep 30"), &dir, &[])
                .await
                .unwrap()
                .output,
        )
        .unwrap();
        pids.push(output["pid"].as_i64().unwrap() as i32);
    }
    // Two running: the bound refuses a third, as a tool error the model sees.
    let error = tools.execute(detach("true"), &dir, &[]).await.unwrap_err();
    assert_eq!(error.code, "detached_limit");
    assert!(
        error
            .detail
            .as_deref()
            .unwrap()
            .contains("--max-detached 2"),
        "{error:?}"
    );
    // One exits: its waiter reaps it without another detach, and releases
    // its place. The test must not reap the child on the runtime's behalf.
    unsafe {
        libc::kill(pids[0], libc::SIGKILL);
    }
    for _ in 0..100 {
        if unsafe { libc::kill(pids[0], 0) } != 0 {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }
    assert_ne!(unsafe { libc::kill(pids[0], 0) }, 0);
    let mut status = 0;
    assert_eq!(
        unsafe { libc::waitpid(pids[0], &mut status, libc::WNOHANG) },
        -1
    );
    let output = tools
        .execute(detach("true"), &dir, &[])
        .await
        .unwrap()
        .output;
    assert!(output.contains("\"detached\":true"), "{output}");
    // Unbounded when asked.
    let free = Registry::new("shell").unwrap().with_detached_budget(0);
    for _ in 0..3 {
        free.execute(
            free.prepare(
                "shell",
                &json!({"command":"true","detach":true}).to_string(),
            )
            .unwrap(),
            &dir,
            &[],
        )
        .await
        .unwrap();
    }
    unsafe {
        libc::kill(pids[1], libc::SIGKILL);
    }
    std::fs::remove_dir_all(dir).unwrap();
}
