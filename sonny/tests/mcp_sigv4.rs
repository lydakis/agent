//! MCP servers behind AWS IAM: every request Sonny sends one, for its tools
//! and for its events, carries an AWS Signature Version 4 that a stand-in
//! server checks as API Gateway does, signed with keys from the AWS chain
//! (here a stand-in AWS CLI), which Sonny resolves again before they expire
//! and never prints.
use std::io::{BufRead, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

const KEYS: &str = r#"{"AKIDONE": "secret-one", "AKIDTWO": "secret-two"}"#;

/// A scratch home whose config names the server at `url` as `ledger`,
/// signed for `region`.
fn home(name: &str) -> PathBuf {
    let home = std::env::temp_dir().join(format!("sonny-sigv4-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&home);
    std::fs::create_dir_all(home.join(".agent")).unwrap();
    home
}

fn configure(home: &Path, url: &str, region: &str) {
    std::fs::write(
        home.join(".agent/config.toml"),
        format!(
            "[sonny.mcp_servers.ledger]\nurl = \"{url}\"\nsigv4 = \"execute-api\"\nregion = \"{region}\"\n"
        ),
    )
    .unwrap();
}

/// `text` as ~/.agent/keys/env under `home`, readable by its owner alone.
fn keep(home: &Path, text: &str) {
    use std::os::unix::fs::OpenOptionsExt;
    let dir = home.join(".agent/keys");
    std::fs::create_dir_all(&dir).unwrap();
    let mut file = std::fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .mode(0o600)
        .open(dir.join("env"))
        .unwrap();
    file.write_all(text.as_bytes()).unwrap();
}

/// The signature-checking server on a free port, logging each request to
/// `home/signed`: it, and its URL.
fn server(home: &Path) -> (Child, String) {
    let mut child = Command::new("python3")
        .arg(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/../tests/mcp_sigv4_server.py"
        ))
        .env("MCP_SIGV4", "execute-api/us-east-1")
        .env("MCP_KEYS", KEYS)
        .env("MCP_SIGNED", home.join("signed"))
        .stdout(Stdio::piped())
        .spawn()
        .unwrap();
    let mut port = String::new();
    std::io::BufReader::new(child.stdout.take().unwrap())
        .read_line(&mut port)
        .unwrap();
    (child, format!("http://127.0.0.1:{}/mcp", port.trim()))
}

/// Sonny under `home`, with the stand-in AWS CLI first on PATH giving
/// `keys` in turn, and no AWS keys or choices of its own in the
/// environment, so the CLI is what it asks.
fn sonny(home: &Path, keys: &str) -> Command {
    let path = format!(
        "{}:{}",
        concat!(env!("CARGO_MANIFEST_DIR"), "/../tests/fake_aws"),
        std::env::var("PATH").unwrap_or_default()
    );
    let mut command = Command::new(env!("CARGO_BIN_EXE_sonny"));
    command
        .env("HOME", home)
        .env("AGENT_TEST_HOME", home)
        .env("PATH", path)
        .env("FAKE_AWS_KEYS", keys)
        .env("FAKE_AWS_CALLS", home.join("aws-calls"))
        .env_remove("XDG_CONFIG_HOME")
        .env_remove("XDG_STATE_HOME");
    for name in [
        "AWS_ACCESS_KEY_ID",
        "AWS_SECRET_ACCESS_KEY",
        "AWS_SESSION_TOKEN",
        "AWS_PROFILE",
        "AWS_DEFAULT_PROFILE",
        "AWS_REGION",
        "AWS_DEFAULT_REGION",
    ] {
        command.env_remove(name);
    }
    command
}

/// `sonny mcp exec -` running `script`.
fn exec(home: &Path, keys: &str, script: &str) -> std::process::Output {
    let mut child = sonny(home, keys)
        .args(["mcp", "exec", "-"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    (child.stdin.take().unwrap())
        .write_all(script.as_bytes())
        .unwrap();
    child.wait_with_output().unwrap()
}

/// What the server logged: each request's HTTP method, JSON-RPC method,
/// signing key id and whether its signature held.
fn signed(home: &Path) -> Vec<serde_json::Value> {
    std::fs::read_to_string(home.join("signed"))
        .unwrap_or_default()
        .lines()
        .map(|line| serde_json::from_str(line).unwrap())
        .collect()
}

#[test]
fn tools_are_signed_and_keys_are_resolved_again_before_they_expire() {
    let home = home("tools");
    let (mut server, url) = server(&home);
    configure(&home, &url, "us-east-1");
    // The first keys are within five minutes of expiring a second from
    // now. The CLI is asked at most every ten seconds, so once that has
    // passed, a call signs with them while the CLI is asked again behind
    // it, and the calls after that sign with the new ones.
    let keys = r#"[["AKIDONE", "secret-one", 302], ["AKIDTWO", "secret-two", 3600]]"#;
    // The profile setup keeps in ~/.agent/keys/env stands over the
    // environment's, as it does for the daemon.
    keep(&home, "AWS_PROFILE=ledger-reader\n");
    let script = r#"text((await tools.mcp__ledger__echo({text: 'one'})).content[0].text);
await tools.mcp__ledger__wait({ms: 10500});
await tools.mcp__ledger__echo({text: 'two'});
await tools.mcp__ledger__wait({ms: 1500});
text((await tools.mcp__ledger__echo({text: 'three'})).content[0].text);"#;
    let ran = exec(&home, keys, script);
    assert!(ran.status.success(), "{ran:?}");
    assert_eq!(String::from_utf8_lossy(&ran.stdout), "one\nthree\n");
    let signed = signed(&home);
    assert!(
        signed.iter().all(|request| request["verified"] == true),
        "{signed:?}"
    );
    let methods: Vec<&str> = signed.iter().filter_map(|r| r["method"].as_str()).collect();
    assert!(
        methods.contains(&"initialize") && methods.contains(&"tools/call"),
        "{methods:?}"
    );
    assert_eq!(signed.first().unwrap()["key"], "AKIDONE", "{signed:?}");
    assert_eq!(signed.last().unwrap()["key"], "AKIDTWO", "{signed:?}");
    // Asked twice in all: once to start, once as the first keys neared
    // their end, however many requests were signed.
    let calls = std::fs::read_to_string(home.join("aws-calls")).unwrap();
    assert_eq!(
        calls.lines().collect::<Vec<_>>(),
        ["configure export-credentials --format process profile=ledger-reader"; 2]
    );
    let _ = server.kill();
    let _ = std::fs::remove_dir_all(&home);
}

#[test]
fn events_are_listed_and_polled_signed() {
    let home = home("events");
    let (mut server, url) = server(&home);
    configure(&home, &url, "us-east-1");
    let source = home.join("source");
    std::fs::create_dir_all(&source).unwrap();
    let wants = home.join("wants.json");
    std::fs::write(&wants, r#"{"follows": [{"topics": ["deploy.done"]}]}"#).unwrap();
    let mut child = sonny(&home, r#"[["AKIDONE", "secret-one", 3600]]"#)
        .args(["mcp", "events", "ledger"])
        .env("SONNY_SOURCE_DIR", &source)
        .env("SONNY_SOURCE_WANTS", &wants)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    // The deploy comes on the poll after the first, a second later.
    let (lines, read) = std::sync::mpsc::channel();
    let stdout = child.stdout.take().unwrap();
    std::thread::spawn(move || {
        for line in std::io::BufReader::new(stdout).lines() {
            let Ok(line) = line else { break };
            if lines.send(line).is_err() {
                break;
            }
        }
    });
    let deadline = Instant::now() + Duration::from_secs(30);
    let event = loop {
        let left = deadline.saturating_duration_since(Instant::now());
        let line = read.recv_timeout(left).expect("no deploy event");
        let line: serde_json::Value = serde_json::from_str(&line).unwrap();
        if line["topic"] == "deploy.done" && line["data"]["stage"] == "prod" {
            break line;
        }
    };
    let _ = child.kill();
    let _ = child.wait();
    assert_eq!(event["topic"], "deploy.done");
    let signed = signed(&home);
    let methods: Vec<&str> = signed.iter().filter_map(|r| r["method"].as_str()).collect();
    assert!(methods.contains(&"events/list"), "{methods:?}");
    assert!(
        methods.iter().filter(|m| **m == "events/poll").count() >= 2,
        "{methods:?}"
    );
    assert!(
        signed
            .iter()
            .all(|request| request["verified"] == true && request["key"] == "AKIDONE"),
        "{signed:?}"
    );
    let _ = server.kill();
    let _ = std::fs::remove_dir_all(&home);
}

/// The region signed for is checked: signed for another, the server
/// refuses, and Sonny says so without a word of the keys.
#[test]
fn a_signature_for_another_region_is_refused_and_no_key_is_shown() {
    let home = home("region");
    let (mut server, url) = server(&home);
    configure(&home, &url, "eu-west-1");
    let ran = exec(
        &home,
        r#"[["AKIDONE", "secret-one", 3600]]"#,
        "text((await tools.mcp__ledger__echo({text: 'one'})).content[0].text);",
    );
    assert!(!ran.status.success(), "{ran:?}");
    let signed = signed(&home);
    assert!(!signed.is_empty());
    assert!(
        signed.iter().all(|request| request["verified"] == false),
        "{signed:?}"
    );
    let said = format!(
        "{}{}",
        String::from_utf8_lossy(&ran.stdout),
        String::from_utf8_lossy(&ran.stderr)
    );
    assert!(said.contains("403"), "{said}");
    for secret in ["secret-one", "token-AKIDONE"] {
        assert!(!said.contains(secret), "{said}");
    }
    let _ = server.kill();
    let _ = std::fs::remove_dir_all(&home);
}
