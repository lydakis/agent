//! `sonny mcp exec`, as a lookout source's command runs it: a script
//! against the person's MCP servers whose calls all go down one
//! connection to each, so a server that keeps a cursor per session pages.
use std::io::{BufRead, Write};
use std::path::Path;
use std::process::{Command, Stdio};

/// A log server that knows each cursor only in the session it gave it,
/// on a free port: it, and its URL.
fn log_server(home: &Path) -> (std::process::Child, String) {
    let mut child = Command::new("python3")
        .arg(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/../tests/mcp_http_server.py"
        ))
        .env("MCP_SESSIONS", home.join("sessions"))
        .stdout(Stdio::piped())
        .spawn()
        .unwrap();
    let mut port = String::new();
    std::io::BufReader::new(child.stdout.take().unwrap())
        .read_line(&mut port)
        .unwrap();
    (child, format!("http://127.0.0.1:{}/mcp", port.trim()))
}

/// `sonny mcp exec -` under `home`, as a source's `sh -c` runs it, with
/// `script` on stdin.
fn exec(home: &Path, script: &str) -> std::process::Output {
    let mut child = Command::new("sh")
        .args(["-c", &format!("{} mcp exec -", env!("CARGO_BIN_EXE_sonny"))])
        .env("HOME", home)
        .env("AGENT_TEST_HOME", home)
        .env_remove("XDG_CONFIG_HOME")
        .env_remove("XDG_STATE_HOME")
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

#[test]
fn a_source_s_script_pages_a_server_s_cursors_on_one_session() {
    let home = std::env::temp_dir().join(format!("sonny-mcp-exec-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&home);
    std::fs::create_dir_all(home.join(".agent")).unwrap();
    let (mut server, url) = log_server(&home);
    std::fs::write(
        home.join(".agent/config.toml"),
        format!("[sonny.mcp_servers.log]\nurl = \"{url}\"\n"),
    )
    .unwrap();
    // Each page an event, a line as it comes; every page past the first
    // asks with the cursor the one before it gave.
    let paging = r#"let cursor;
do {
  const r = await tools.mcp__log__page(cursor ? {cursor} : {});
  if (r.isError) throw new Error(r.content[0].text);
  text(JSON.stringify({id: `page-${r.structuredContent.page}`, text: `page ${r.structuredContent.page}`}));
  cursor = r.structuredContent.cursor;
} while (cursor);"#;
    let ran = exec(&home, paging);
    assert!(ran.status.success(), "{ran:?}");
    let lines: Vec<serde_json::Value> = String::from_utf8(ran.stdout)
        .unwrap()
        .lines()
        .map(|line| serde_json::from_str(line).unwrap())
        .collect();
    let ids: Vec<&str> = lines.iter().map(|l| l["id"].as_str().unwrap()).collect();
    assert_eq!(ids, ["page-1", "page-2", "page-3", "page-4", "page-5"]);
    let sessions = std::fs::read_to_string(home.join("sessions")).unwrap();
    assert_eq!(sessions.lines().count(), 1, "{sessions}");
    // A script that fails fails the command, saying why on stderr, with
    // what it printed before printed.
    let ran = exec(&home, "text('before');\nthrow new Error('no more');");
    assert!(!ran.status.success(), "{ran:?}");
    assert_eq!(String::from_utf8_lossy(&ran.stdout), "before\n");
    let said = String::from_utf8_lossy(&ran.stderr);
    assert!(
        said.contains("Script failed") && said.contains("no more"),
        "{said}"
    );
    let _ = server.kill();
    let _ = std::fs::remove_dir_all(&home);
}
