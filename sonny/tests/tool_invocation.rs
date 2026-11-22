//! `sonny tool` acts as the agent its invocation names, never as the one
//! its process's variables name: the tool host gives the caller with each
//! call, as Codex hands each tool its invocation.
use std::path::Path;

/// What `sonny tool live` answers, run in `folder` with `home`, given
/// `invocation` on stdin and `AGENT_BOT` set to `env_bot`.
fn live(
    home: &Path,
    folder: &Path,
    invocation: serde_json::Value,
    env_bot: Option<&str>,
) -> serde_json::Value {
    use std::io::Write;
    let mut command = std::process::Command::new(env!("CARGO_BIN_EXE_sonny"));
    command
        .args(["tool", "live"])
        .current_dir(folder)
        .env("HOME", home)
        .env("AGENT_TEST_HOME", home)
        .env_remove("AGENT_STORE")
        .env_remove("AGENT_SOCKET")
        .env_remove("XDG_CONFIG_HOME")
        .env_remove("XDG_STATE_HOME")
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped());
    match env_bot {
        Some(bot) => command.env("AGENT_BOT", bot),
        None => command.env_remove("AGENT_BOT"),
    };
    let mut child = command.spawn().unwrap();
    let given = serde_json::json!({"invocation": invocation, "arguments": {"markdown": "plain"}});
    (child.stdin.take().unwrap())
        .write_all(given.to_string().as_bytes())
        .unwrap();
    let ran = child.wait_with_output().unwrap();
    assert!(ran.status.success(), "{ran:?}");
    serde_json::from_slice(&ran.stdout).unwrap()
}

#[test]
fn a_tool_call_is_the_invocation_s_agent_s_and_never_the_shell_s() {
    let home = std::env::temp_dir().join(format!("sonny-tool-invocation-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&home);
    let work = home.join("work");
    std::fs::create_dir_all(&work).unwrap();
    let named = serde_json::json!({"bot": "Reader", "pwd": &work, "folder": std::fs::canonicalize(&work).unwrap()});
    // Named in the invocation alone, the call is answered.
    let said = live(&home, &work, named, None);
    assert!(said["output"].is_string(), "{said}");
    // A shell's AGENT_BOT does not stand in for an invocation that names
    // no agent.
    let unnamed =
        serde_json::json!({"pwd": &work, "folder": std::fs::canonicalize(&work).unwrap()});
    let said = live(&home, &work, unnamed, Some("Reader"));
    assert_eq!(said["error"]["code"], "no_caller", "{said}");
    std::fs::remove_dir_all(&home).unwrap();
}
