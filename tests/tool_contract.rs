#![cfg(unix)]
// The libc calls signal or inspect processes these tests started.
#![allow(clippy::undocumented_unsafe_blocks)]
use agent_runtime::tools::{Credentials, Prepared, Registry};
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
    let output: Value = serde_json::from_str(
        &tools
            .execute(prepared, &cwd, &[], None)
            .await
            .unwrap()
            .output,
    )
    .unwrap();
    assert_eq!(output["stdout"], "out");
    assert_eq!(output["stderr"], "err");
    assert_eq!(output["exit_code"], 7);
    assert!(output["wall_time_ms"].is_u64());
    // No command has a terminal, and what the caller sets still wins.
    let prepared = tools
        .prepare(
            "shell",
            &json!({"command":"printf '%s %s %s %s %s %s' \"$TERM\" \"$NO_COLOR\" \"$PAGER\" \"$GIT_PAGER\" \"$GH_PAGER\" \"$AI_AGENT\""})
                .to_string(),
        )
        .unwrap();
    let env = [("PAGER".to_owned(), "less".to_owned())];
    let output: Value = serde_json::from_str(
        &tools
            .execute(prepared, &cwd, &env, None)
            .await
            .unwrap()
            .output,
    )
    .unwrap();
    assert_eq!(output["stdout"], "dumb 1 less cat cat agent");
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
            tools.execute(prepared, &cwd, &[], None),
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
    // Past 1 MiB a stream keeps both ends, and the command runs to its end.
    let overflow = run(
        "head -c 3000000 /dev/zero | tr '\\0' x; echo; echo done",
        1500,
    )
    .await
    .unwrap()
    .unwrap();
    let output: Value = serde_json::from_str(&overflow.output).unwrap();
    assert_eq!(output["exit_code"], 0);
    assert_eq!(output["success"], true);
    let shown = output["stdout"].as_str().unwrap();
    assert!(shown.starts_with("xxxx"));
    assert!(shown.ends_with("x\ndone\n"));
    assert!(shown.contains("more past 1 MiB were not kept"));
    let (stream, kept) = &overflow.artifacts[0];
    assert_eq!(*stream, "stdout");
    assert!(kept.len() <= agent_runtime::tools::ARTIFACT_BYTES);
    assert!(String::from_utf8_lossy(kept).contains("bytes dropped: past 1 MiB"));
}

#[tokio::test]
async fn known_credential_text_is_redacted_before_tool_result_serialization() {
    let sentinel = "synthetic-\"test-value";
    let credentials = Credentials::default();
    credentials.set("AGENT_TEST_FAKE_KEY", sentinel);
    let tools = Registry::new("echo,shell")
        .unwrap()
        .with_credentials(credentials);
    let echo = tools
        .prepare("echo", &json!({"text":sentinel}).to_string())
        .unwrap();
    assert!(
        tools
            .execute(echo, &std::env::temp_dir(), &[], None)
            .await
            .unwrap()
            .output
            == "[REDACTED]"
    );
    let shell = tools.prepare("shell", &json!({"command": "printf '%s' 'synthetic-\"test-value'; printf '%s' 'synthetic-\"test-value' >&2"}).to_string()).unwrap();
    let output = tools
        .execute(shell, &std::env::temp_dir(), &[], None)
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
        .execute(binary, &std::env::temp_dir(), &[], None)
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
            .execute(echo, &std::env::temp_dir(), &[], None)
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
            .execute(write, &dir, &[], None)
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
    let shown = tools.execute(read, &dir, &[], None).await.unwrap().output;
    assert!(shown.starts_with("     2\ttwo\n"), "{shown}");
    assert!(shown.contains("showing lines 2-2 of 3"));
    let ambiguous = tools
        .prepare(
            "edit",
            &json!({"path":"src/lib.rs","old":"\n","new":" "}).to_string(),
        )
        .unwrap();
    assert_eq!(
        tools
            .execute(ambiguous, &dir, &[], None)
            .await
            .unwrap_err()
            .code,
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
            .execute(edit, &dir, &[], None)
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
        tools
            .execute(oversized, &dir, &[], None)
            .await
            .unwrap_err()
            .code,
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
    tools.execute(rewrite, &dir, &[], None).await.unwrap();
    assert_eq!(
        std::fs::read_to_string(dir.join("src/lib.rs")).unwrap(),
        "short"
    );
    assert!(dir.join("alias.rs").is_symlink());
    let missing = tools
        .prepare("read", &json!({"path":"nope.txt"}).to_string())
        .unwrap();
    assert_eq!(
        tools
            .execute(missing, &dir, &[], None)
            .await
            .unwrap_err()
            .code,
        "file_unreadable"
    );
    // Oversized shell output keeps a preview and retains the full stream.
    let big = tools
        .prepare(
            "shell",
            &json!({"command":"head -c 200000 /dev/zero | tr '\\0' 'y'"}).to_string(),
        )
        .unwrap();
    let outcome = tools.execute(big, &dir, &[], None).await.unwrap();
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
    let error = tools.execute(read(1), &dir, &[], None).await.unwrap_err();
    assert_eq!(error.code, "read_line_too_long");
    assert!(error.detail.unwrap().contains("line 1"));
    assert!(
        tools
            .execute(read(2), &dir, &[], None)
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
    let page = tools
        .execute(read(1), &dir, &[], None)
        .await
        .unwrap()
        .output;
    assert!(page.starts_with("     1\tok\n"));
    assert!(page.contains("offset=2"));
    assert_eq!(
        tools
            .execute(read(2), &dir, &[], None)
            .await
            .unwrap_err()
            .code,
        "read_line_too_long"
    );
    assert!(
        tools
            .execute(read(3), &dir, &[], None)
            .await
            .unwrap()
            .output
            .contains("tail")
    );
    std::fs::write(dir.join("data"), "abcdefgh\n".repeat(5000)).unwrap();
    let page = tools
        .prepare("read", &json!({"path":"data","limit":5000}).to_string())
        .unwrap();
    let output = tools.execute(page, &dir, &[], None).await.unwrap().output;
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
    let output: Value = serde_json::from_str(
        &tools
            .execute(prepared, &dir, &[], None)
            .await
            .unwrap()
            .output,
    )
    .unwrap();
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
                .execute(detach("exec sleep 30"), &dir, &[], None)
                .await
                .unwrap()
                .output,
        )
        .unwrap();
        pids.push(output["pid"].as_i64().unwrap() as i32);
    }
    // Two running: the bound refuses a third, as a tool error the model sees.
    let error = tools
        .execute(detach("true"), &dir, &[], None)
        .await
        .unwrap_err();
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
        .execute(detach("true"), &dir, &[], None)
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
            None,
        )
        .await
        .unwrap();
    }
    unsafe {
        libc::kill(pids[1], libc::SIGKILL);
    }
    std::fs::remove_dir_all(dir).unwrap();
}

/// Whether sandboxes run here. CI sets AGENT_TEST_SANDBOX, where one that
/// can't is a failure; elsewhere the test says why it skipped.
#[cfg(any(target_os = "linux", target_os = "macos"))]
fn sandboxes() -> bool {
    match agent_runtime::sandbox::available() {
        Ok(()) => true,
        Err(error) if std::env::var_os("AGENT_TEST_SANDBOX").is_some() => {
            panic!("no sandbox: {error:?}")
        }
        Err(error) => {
            eprintln!("skipped: {error:?}");
            false
        }
    }
}

/// A sandboxed command writes its roots and nothing else, keeps `.git`
/// and the store read-only, can't make a missing `.git`, and has no
/// network; a confined write or edit is held to the same roots.
#[cfg(target_os = "linux")]
#[tokio::test]
async fn a_sandboxed_command_writes_only_its_roots() {
    use agent_runtime::sandbox::{Daemon, Mode, Policy};
    use std::sync::Arc;
    if !sandboxes() {
        return;
    }
    // Outside the temporary directory, which a sandbox may write.
    let base = std::fs::canonicalize(env!("CARGO_TARGET_TMPDIR"))
        .unwrap()
        .join(format!("agent-sandbox-{}", std::process::id()));
    let (dir, other, repo, fresh) = (
        base.join("work"),
        base.join("other"),
        base.join("repo"),
        base.join("fresh"),
    );
    for folder in [&dir, &other, &repo.join(".git"), &fresh] {
        std::fs::create_dir_all(folder).unwrap();
    }
    std::fs::write(dir.join("store"), "db").unwrap();
    let daemon = Daemon {
        store: Some(dir.join("store")),
        helper: Some(env!("CARGO_BIN_EXE_agent").into()),
        ..protecting()
    };
    let writable = [
        repo.to_str().unwrap().to_owned(),
        fresh.to_str().unwrap().to_owned(),
    ];
    let policy = Arc::new(
        Policy::new(
            Some(Mode::WorkspaceWrite),
            Some(&dir),
            &writable,
            false,
            &daemon,
        )
        .unwrap()
        .unwrap(),
    );
    let tools = Registry::new("shell,write,edit,apply_patch").unwrap();
    let run = |command: String| {
        let tools = tools.clone();
        let (dir, policy) = (dir.clone(), policy.clone());
        async move {
            let prepared = tools
                .prepare("shell", &json!({"command":command}).to_string())
                .unwrap();
            let outcome = tools
                .execute(prepared, &dir, &[], Some(policy))
                .await
                .unwrap();
            serde_json::from_str::<Value>(&outcome.output).unwrap()
        }
    };
    // Its folder, a named one and the temporary directory take writes,
    // from where the command started.
    let output = run(format!(
        "echo a > made && echo b > {repo}/b && echo c > /tmp/agent-sandbox-tmp-{pid} && pwd",
        repo = repo.display(),
        pid = std::process::id()
    ))
    .await;
    assert_eq!(output["exit_code"], 0, "{output}");
    assert_eq!(
        output["stdout"].as_str().unwrap().trim(),
        dir.to_str().unwrap()
    );
    assert!(dir.join("made").exists() && repo.join("b").exists());
    std::fs::remove_file(format!("/tmp/agent-sandbox-tmp-{}", std::process::id())).unwrap();
    // Anything else, git's folder and the store stay as they were.
    for target in [
        other.join("x"),
        repo.join(".git/config"),
        dir.join("store"),
        fresh.join(".git"),
    ] {
        let output = run(format!("mkdir {0} || echo x > {0}", target.display())).await;
        assert_ne!(output["exit_code"], 0, "{} {output}", target.display());
    }
    assert!(!other.join("x").exists() && !repo.join(".git/config").exists());
    assert_eq!(std::fs::read_to_string(dir.join("store")).unwrap(), "db");
    // The placeholder that kept `.git` from being made goes with the
    // command, off the runtime thread.
    for _ in 0..100 {
        if !fresh.join(".git").exists() && !dir.join(".git").exists() {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }
    assert!(!fresh.join(".git").exists() && !dir.join(".git").exists());
    // Only loopback: no network.
    let output = run("tail -n +3 /proc/net/dev | cut -d: -f1".into()).await;
    assert_eq!(output["stdout"].as_str().unwrap().trim(), "lo");
    // A confined write or edit is held to the same roots.
    let write = |path: &str| {
        tools
            .prepare("write", &json!({"path":path,"content":"x"}).to_string())
            .unwrap()
    };
    tools
        .execute(write("notes/a.md"), &dir, &[], Some(policy.clone()))
        .await
        .unwrap();
    for denied in [
        other.join("a").to_str().unwrap(),
        repo.join(".git/HEAD").to_str().unwrap(),
        "store",
    ] {
        let error = tools
            .execute(write(denied), &dir, &[], Some(policy.clone()))
            .await
            .unwrap_err();
        assert_eq!(error.code, "write_denied", "{denied}");
    }
    std::os::unix::fs::symlink(&other, dir.join("out")).unwrap();
    let edit = tools
        .prepare(
            "edit",
            &json!({"path":"out/a","old":"x","new":"y"}).to_string(),
        )
        .unwrap();
    let error = tools
        .execute(edit, &dir, &[], Some(policy.clone()))
        .await
        .unwrap_err();
    assert_eq!(error.code, "write_denied");
    // A link to a file that doesn't exist yet is followed to where a write
    // through it would land.
    std::os::unix::fs::symlink(other.join("new"), dir.join("dangling")).unwrap();
    let error = tools
        .execute(write("dangling"), &dir, &[], Some(policy.clone()))
        .await
        .unwrap_err();
    assert_eq!(error.code, "write_denied");
    assert!(!other.join("new").exists());
    // So is a patch, every file it changes checked before any is written.
    let patch = |paths: &[&str]| {
        let mut input = String::from("*** Begin Patch\n");
        for path in paths {
            input.push_str(&format!("*** Add File: {path}\n+x\n"));
        }
        input.push_str("*** End Patch");
        tools
            .prepare("apply_patch", &json!({ "input": input }).to_string())
            .unwrap()
    };
    let outside = other.join("b");
    let error = tools
        .execute(
            patch(&["notes/b.md", outside.to_str().unwrap()]),
            &dir,
            &[],
            Some(policy.clone()),
        )
        .await
        .unwrap_err();
    assert_eq!(error.code, "write_denied");
    assert!(!dir.join("notes/b.md").exists() && !outside.exists());
    tools
        .execute(patch(&["notes/b.md"]), &dir, &[], Some(policy.clone()))
        .await
        .unwrap();
    assert_eq!(
        std::fs::read_to_string(dir.join("notes/b.md")).unwrap(),
        "x\n"
    );
    std::fs::remove_dir_all(base).unwrap();
}

/// The daemon's socket is out of reach from inside a sandbox, network on
/// or off, as Codex keeps its own.
#[cfg(target_os = "linux")]
#[tokio::test]
async fn a_sandboxed_command_cannot_reach_the_daemon_socket() {
    use agent_runtime::sandbox::{Daemon, Mode, Policy};
    if !sandboxes() {
        return;
    }
    let base = std::fs::canonicalize(env!("CARGO_TARGET_TMPDIR"))
        .unwrap()
        .join(format!("agent-socket-{}", std::process::id()));
    std::fs::create_dir_all(&base).unwrap();
    let socket = base.join("agent.sock");
    let _listener = std::os::unix::net::UnixListener::bind(&socket).unwrap();
    let tools = Registry::new("shell").unwrap();
    let connect = format!(
        "python3 -c 'import socket; socket.socket(socket.AF_UNIX).connect(\"{}\")'",
        socket.display()
    );
    let run = |sandbox: Option<Policy>| {
        let tools = tools.clone();
        let (base, connect) = (base.clone(), connect.clone());
        async move {
            let prepared = tools
                .prepare("shell", &json!({"command": connect}).to_string())
                .unwrap();
            let output = tools
                .execute(prepared, &base, &[], sandbox.map(std::sync::Arc::new))
                .await
                .unwrap()
                .output;
            serde_json::from_str::<Value>(&output).unwrap()["exit_code"].clone()
        }
    };
    assert_eq!(run(None).await, 0);
    for network in [false, true] {
        let daemon = Daemon {
            socket: Some(socket.clone()),
            ..protecting()
        };
        let policy = Policy::new(
            Some(Mode::WorkspaceWrite),
            Some(&base),
            &[],
            network,
            &daemon,
        )
        .unwrap()
        .unwrap();
        assert_ne!(run(Some(policy.clone())).await, 0, "network {network}");
        // With the bot's own socket, laid out as the daemon lays them out,
        // the daemon's path reaches that one.
        let folder = base.join(format!("agent.sock.d-{network}"));
        std::fs::create_dir_all(&folder).unwrap();
        let own = folder.join("1");
        let listener = std::os::unix::net::UnixListener::bind(&own).unwrap();
        listener.set_nonblocking(true).unwrap();
        let policy = Policy {
            capability: Some(own),
            ..policy
        };
        assert_eq!(run(Some(policy)).await, 0, "network {network}");
        assert!(listener.accept().is_ok());
    }
    std::fs::remove_dir_all(base).unwrap();
}

/// On macOS a sandboxed command reaches only the Unix sockets its settings
/// name, network on or off: not the daemon's, even inside its folder, nor
/// one in its folder, which could be a hard link to any socket.
#[cfg(target_os = "macos")]
#[tokio::test]
async fn a_sandboxed_command_reaches_only_the_sockets_it_names() {
    use agent_runtime::sandbox::{Daemon, Mode, Policy};
    if !sandboxes() {
        return;
    }
    let base = std::fs::canonicalize(env!("CARGO_TARGET_TMPDIR"))
        .unwrap()
        .join(format!("agent-sockets-{}", std::process::id()));
    let (workspace, outside) = (base.join("ws"), base.join("out"));
    std::fs::create_dir_all(workspace.join(".local")).unwrap();
    std::fs::create_dir_all(outside.join("named")).unwrap();
    let socket = workspace.join(".local/agent.sock");
    let listen = |path: &std::path::Path| std::os::unix::net::UnixListener::bind(path).unwrap();
    let paths = [
        socket.clone(),
        workspace.join("own.sock"),
        outside.join("other.sock"),
        outside.join("one.sock"),
        outside.join("named/in.sock"),
    ];
    let _listeners: Vec<_> = paths.iter().map(|path| listen(path)).collect();
    let tools = Registry::new("shell").unwrap();
    let reaches = |policy: Policy, path: std::path::PathBuf| {
        let tools = tools.clone();
        let workspace = workspace.clone();
        async move {
            let connect = format!(
                "python3 -c 'import socket; socket.socket(socket.AF_UNIX).connect(\"{}\")'",
                path.display()
            );
            let prepared = tools
                .prepare("shell", &json!({"command": connect}).to_string())
                .unwrap();
            let output = tools
                .execute(prepared, &workspace, &[], Some(std::sync::Arc::new(policy)))
                .await
                .unwrap()
                .output;
            serde_json::from_str::<Value>(&output).unwrap()["exit_code"] == 0
        }
    };
    let named = [
        paths[3].display().to_string(),
        outside.join("named").display().to_string(),
    ];
    for network in [false, true] {
        let daemon = Daemon {
            socket: Some(socket.clone()),
            ..protecting()
        };
        let policy = Policy::new(
            Some(Mode::WorkspaceWrite),
            Some(&workspace),
            &[],
            network,
            &daemon,
        )
        .unwrap()
        .unwrap()
        .with_sockets(&named);
        let reached = [
            reaches(policy.clone(), paths[0].clone()).await,
            reaches(policy.clone(), paths[1].clone()).await,
            reaches(policy.clone(), paths[2].clone()).await,
            reaches(policy.clone(), paths[3].clone()).await,
            reaches(policy.clone(), paths[4].clone()).await,
        ];
        assert_eq!(
            reached,
            [false, false, false, true, true],
            "network {network}"
        );
    }
    std::fs::remove_dir_all(base).unwrap();
}

/// A folder a bot may write that something swapped for a link since is
/// refused, never bound where the link points: a bot's creator may write
/// the folder above it. Each confined call builds its policy anew.
#[cfg(target_os = "linux")]
#[tokio::test]
async fn a_folder_swapped_for_a_link_is_not_followed() {
    use agent_runtime::sandbox::{Mode, Policy};
    if !sandboxes() {
        return;
    }
    let base = std::fs::canonicalize(env!("CARGO_TARGET_TMPDIR"))
        .unwrap()
        .join(format!("agent-swap-{}", std::process::id()));
    let (given, outside) = (base.join("creator/child"), base.join("outside"));
    std::fs::create_dir_all(&given).unwrap();
    std::fs::create_dir_all(&outside).unwrap();
    let policy = |workspace: &std::path::Path| {
        Policy::new(
            Some(Mode::WorkspaceWrite),
            Some(workspace),
            &[],
            false,
            &protecting(),
        )
    };
    assert!(policy(&given).is_ok());
    std::fs::rename(&given, base.join("creator/moved")).unwrap();
    std::os::unix::fs::symlink(&outside, &given).unwrap();
    let error = policy(&given).unwrap_err();
    assert_eq!(error.code, "sandbox_unavailable");
    std::fs::remove_dir_all(base).unwrap();
}

/// Read-only writes nothing, not even the temporary directory.
#[cfg(target_os = "linux")]
#[tokio::test]
async fn a_read_only_sandbox_writes_nothing() {
    use agent_runtime::sandbox::{Mode, Policy};
    if !sandboxes() {
        return;
    }
    let dir = std::fs::canonicalize(env!("CARGO_TARGET_TMPDIR")).unwrap();
    let policy = Policy::new(Some(Mode::ReadOnly), Some(&dir), &[], true, &protecting())
        .unwrap()
        .unwrap();
    assert!(!policy.network);
    let tools = Registry::new("shell").unwrap();
    let marker = format!("agent-read-only-{}", std::process::id());
    let prepared = tools
        .prepare(
            "shell",
            &json!({"command":format!("cat /etc/hostname >/dev/null && echo x > {marker} || echo x > /tmp/{marker}")})
                .to_string(),
        )
        .unwrap();
    let output: Value = serde_json::from_str(
        &tools
            .execute(
                prepared,
                &dir,
                &[],
                Some(std::sync::Arc::new(policy.clone())),
            )
            .await
            .unwrap()
            .output,
    )
    .unwrap();
    assert_ne!(output["exit_code"], 0, "{output}");
    assert!(!dir.join(&marker).exists());
    assert!(!std::path::Path::new("/tmp").join(&marker).exists());
    // The write a confined write tool runs is held by the kernel as well,
    // whatever the daemon checked before it.
    let target = dir.join(&marker);
    let mut write = agent_runtime::sandbox::command(
        &policy,
        env!("CARGO_BIN_EXE_agent"),
        &[agent_runtime::sandbox::WRITE, target.to_str().unwrap()],
        false,
    )
    .unwrap();
    let written = write
        .stdin(std::process::Stdio::null())
        .output()
        .await
        .unwrap();
    assert!(!written.status.success());
    let reported: Value = serde_json::from_slice(&written.stderr).unwrap();
    assert_eq!(reported["error"], "write_denied", "{reported}");
    assert!(!target.exists());
}

/// Serve each connection one canned response, by the path asked for.
async fn page_server(pages: Vec<(&'static str, &'static str, Vec<u8>)>) -> String {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    tokio::spawn(async move {
        loop {
            let (mut socket, _) = listener.accept().await.unwrap();
            let mut request = vec![0; 4096];
            let read = socket.read(&mut request).await.unwrap();
            let request = String::from_utf8_lossy(&request[..read]);
            let path = request.split_whitespace().nth(1).unwrap_or("/").to_owned();
            let (status, kind, body) = pages
                .iter()
                .find(|(page, _, _)| page.split(' ').next() == Some(path.as_str()))
                .map(|(page, kind, body)| (page.split_once(' ').unwrap().1, *kind, body.clone()))
                .unwrap_or(("404 Not Found", "text/plain", b"missing".to_vec()));
            let head = format!(
                "HTTP/1.1 {status}\r\ncontent-type: {kind}\r\ncontent-length: {}\r\nconnection: close\r\n\r\n",
                body.len()
            );
            let _ = socket.write_all(head.as_bytes()).await;
            let _ = socket.write_all(&body).await;
        }
    });
    format!("http://{address}")
}

#[tokio::test]
async fn web_fetch_reads_a_page_as_markdown_and_keeps_a_long_one() {
    let long = format!("<p>{}</p><p>the end</p>", "word ".repeat(30_000));
    let base = page_server(vec![
        (
            "/page 200 OK",
            "text/html; charset=utf-8",
            b"<html><head><title>Hello</title></head><body><h1>Hi</h1><a href=\"/next\">next</a></body></html>".to_vec(),
        ),
        ("/long 200 OK", "text/html", long.into_bytes()),
        ("/picture 200 OK", "image/png", b"\x89PNG".to_vec()),
    ])
    .await;
    let tools = Registry::new("web_fetch").unwrap();
    let cwd = std::env::temp_dir();
    let fetch = |path: &str| {
        tools
            .prepare(
                "web_fetch",
                &json!({"url":format!("{base}{path}")}).to_string(),
            )
            .unwrap()
    };
    let outcome = tools
        .execute(fetch("/page"), &cwd, &[], None)
        .await
        .unwrap();
    let output: Value = serde_json::from_str(&outcome.output).unwrap();
    assert!(!outcome.failed);
    assert_eq!(output["status"], 200);
    assert_eq!(output["title"], "Hello");
    assert_eq!(output["text"], format!("# Hi\n\n[next]({base}/next)\n"));
    assert_eq!(output["url"], format!("{base}/page"));
    // A long page shows its start and end and keeps the whole to read.
    let outcome = tools
        .execute(fetch("/long"), &cwd, &[], None)
        .await
        .unwrap();
    let output: Value = serde_json::from_str(&outcome.output).unwrap();
    let text = output["text"].as_str().unwrap();
    assert!(
        text.len() < 70_000 && text.ends_with("the end\n"),
        "{}",
        text.len()
    );
    assert_eq!(outcome.artifacts.len(), 1);
    assert_eq!(outcome.artifacts[0].0, "text");
    assert!(outcome.artifacts[0].1.len() > 140_000);
    // A missing page is a failed result with what the server said.
    let outcome = tools
        .execute(fetch("/gone"), &cwd, &[], None)
        .await
        .unwrap();
    assert!(outcome.failed);
    assert_eq!(
        serde_json::from_str::<Value>(&outcome.output).unwrap()["text"],
        "missing"
    );
    let error = tools
        .execute(fetch("/picture"), &cwd, &[], None)
        .await
        .unwrap_err();
    assert_eq!(error.code, "web_fetch_not_text");
    assert_eq!(
        tools
            .prepare("web_fetch", r#"{"url":"file:///etc/passwd"}"#)
            .err()
            .unwrap()
            .code,
        "invalid_url"
    );
}

#[tokio::test]
async fn a_terminal_takes_typed_input_and_reports_its_end() {
    let tools = Registry::all().unwrap();
    let cwd = std::env::temp_dir();
    // A terminal is the runtime's to start, and runs until it ends.
    assert!(matches!(
        tools.prepare("shell", r#"{"command":"cat","tty":true}"#),
        Ok(Prepared::Terminal { yield_ms: 1000, .. })
    ));
    for args in [
        json!({"command":"cat","tty":true,"background":true}),
        json!({"command":"cat","tty":true,"timeout_ms":5}),
        json!({"command":"cat","tty":true,"sandbox_permissions":"require_escalated"}),
        json!({"command":"cat","yield_time_ms":5}),
    ] {
        let error = tools.prepare("shell", &args.to_string()).err().unwrap();
        assert_eq!(error.code, "invalid_shell_arguments", "{args}");
    }
    for args in [
        json!({"handle":"turn:a/1"}),
        json!({"handle":"proc:01"}),
        json!({"handle":"proc:1","chars":"x","kill":true}),
    ] {
        let error = tools
            .prepare("write_stdin", &args.to_string())
            .err()
            .unwrap();
        assert_eq!(error.code, "invalid_tool_arguments", "{args}");
    }

    let (done, end) = tokio::sync::oneshot::channel();
    let started = tools
        .terminal(
            1,
            1,
            "test -t 0 && echo tty; read -p 'name? ' name; echo \"hi $name\"; exit 3".into(),
            cwd.clone(),
            2000,
            Vec::new(),
            None,
            done,
        )
        .await
        .unwrap();
    let started: Value = serde_json::from_str(&started.output).unwrap();
    assert_eq!(started["handle"], "proc:1");
    assert_eq!(started["running"], true);
    assert_eq!(started["output"], "tty\nname? ");
    // Only its own bot types into it.
    let error = tools
        .write_stdin(2, 1, "x\n", None, false, None)
        .await
        .err()
        .unwrap();
    assert_eq!(error.code, "terminal_not_running");
    let typed = tools
        .write_stdin(1, 1, "bob\n", Some(5000), false, None)
        .await
        .unwrap();
    assert!(typed.failed);
    let typed: Value = serde_json::from_str(&typed.output).unwrap();
    assert_eq!(typed["output"], "bob\nhi bob\n");
    assert_eq!(typed["exit_code"], 3);
    assert!(typed.get("running").is_none());
    // Its handle reports the end too.
    let end: Value = serde_json::from_str(&end.await.unwrap().unwrap().output).unwrap();
    assert_eq!(end["exit_code"], 3);
    assert!(end.get("handle").is_none());
    let error = tools
        .write_stdin(1, 1, "", None, false, None)
        .await
        .err()
        .unwrap();
    assert_eq!(error.code, "terminal_not_running");

    // Ctrl-C reaches the program, as at a terminal; kill ends what ignores it.
    let (done, _end) = tokio::sync::oneshot::channel();
    tools
        .terminal(1, 2, "cat".into(), cwd.clone(), 0, Vec::new(), None, done)
        .await
        .unwrap();
    let interrupted = tools
        .write_stdin(1, 2, "\u{3}", Some(5000), false, None)
        .await
        .unwrap();
    let interrupted: Value = serde_json::from_str(&interrupted.output).unwrap();
    assert_eq!(interrupted["signal"], libc::SIGINT);
    let (done, _end) = tokio::sync::oneshot::channel();
    tools
        .terminal(
            1,
            3,
            "trap '' INT; sleep 60".into(),
            cwd.clone(),
            0,
            Vec::new(),
            None,
            done,
        )
        .await
        .unwrap();
    // Input typed after its bot's sandbox changed would run outside the
    // new one: the terminal ends instead.
    let confined = std::sync::Arc::new(agent_runtime::sandbox::Policy {
        roots: Vec::new(),
        network: false,
        socket: None,
        capability: None,
        helper: None,
        sockets: Vec::new(),
    });
    let error = tools
        .write_stdin(1, 3, "x\n", None, false, Some(confined))
        .await
        .err()
        .unwrap();
    assert_eq!(error.code, "terminal_sandbox_changed");
    let (done, _end) = tokio::sync::oneshot::channel();
    tools
        .terminal(
            1,
            4,
            "trap '' INT; sleep 60".into(),
            cwd.clone(),
            0,
            Vec::new(),
            None,
            done,
        )
        .await
        .unwrap();
    let killed = tools.write_stdin(1, 4, "", None, true, None).await.unwrap();
    let killed: Value = serde_json::from_str(&killed.output).unwrap();
    assert_eq!(killed["signal"], libc::SIGKILL);
}

/// The daemon as the CLI starts it, protecting what its client names.
#[cfg(any(target_os = "linux", target_os = "macos"))]
fn protecting() -> agent_runtime::sandbox::Daemon {
    agent_runtime::sandbox::Daemon {
        protected: agent_client::PROTECTED.map(String::from).to_vec(),
        protected_existing: agent_client::PROTECTED_EXISTING.map(String::from).to_vec(),
        ..Default::default()
    }
}
