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

/// `home` with `config` as its Sonny config.
fn home_with(name: &str, config: &str) -> std::path::PathBuf {
    let home = std::env::temp_dir().join(format!("sonny-mcp-exec-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&home);
    std::fs::create_dir_all(home.join(".agent")).unwrap();
    std::fs::write(home.join(".agent/config.toml"), config).unwrap();
    home
}

/// `sonny mcp exec -` under `home`, `script` on stdin, in a process
/// group of its own, as the lookout starts a source's command.
fn started(home: &Path, script: &str, stdout: Stdio) -> std::process::Child {
    use std::os::unix::process::CommandExt;
    let mut child = Command::new(env!("CARGO_BIN_EXE_sonny"))
        .args(["mcp", "exec", "-"])
        .env("HOME", home)
        .env("AGENT_TEST_HOME", home)
        .env_remove("XDG_CONFIG_HOME")
        .env_remove("XDG_STATE_HOME")
        .stdin(Stdio::piped())
        .stdout(stdout)
        .stderr(Stdio::piped())
        .process_group(0)
        .spawn()
        .unwrap();
    (child.stdin.take().unwrap())
        .write_all(script.as_bytes())
        .unwrap();
    child
}

#[test]
fn a_script_whose_output_cannot_go_out_fails() {
    let home = home_with("closed", "");
    // Its stdout's reader gone before it says a word.
    let (reader, writer) = std::io::pipe().unwrap();
    drop(reader);
    let child = started(&home, "text('one');\ntext('two');", Stdio::from(writer));
    let ran = child.wait_with_output().unwrap();
    assert!(!ran.status.success(), "{ran:?}");
    let said = String::from_utf8_lossy(&ran.stderr);
    assert!(said.contains("its output could not be written"), "{said}");
    // So does one that catches the error and goes on.
    let (reader, writer) = std::io::pipe().unwrap();
    drop(reader);
    let child = started(
        &home,
        "try { text('one'); } catch (e) {}",
        Stdio::from(writer),
    );
    let ran = child.wait_with_output().unwrap();
    assert!(!ran.status.success(), "{ran:?}");
    let _ = std::fs::remove_dir_all(&home);
}

#[test]
fn a_stdio_server_stops_with_the_command_s_process_group() {
    let dir = std::env::temp_dir().join(format!("sonny-mcp-exec-group-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let starts = dir.join("starts");
    let config = format!(
        "[sonny.mcp_servers.test]\ncommand = \"python3\"\nargs = [\"{}\"]\nenv = {{ MCP_STARTS = \"{}\" }}\n",
        concat!(env!("CARGO_MANIFEST_DIR"), "/../tests/mcp_server.py"),
        starts.display()
    );
    let home = home_with("group", &config);
    let never = dir.join("never");
    let child = started(
        &home,
        &format!(
            "await tools.mcp__test__slow({{until: {:?}}});",
            never.display().to_string()
        ),
        Stdio::null(),
    );
    let started_at = std::time::Instant::now();
    let server = loop {
        if let Some(pid) = std::fs::read_to_string(&starts)
            .ok()
            .and_then(|pids| pids.lines().next().map(str::to_owned))
        {
            break pid;
        }
        assert!(
            started_at.elapsed().as_secs() < 30,
            "the server never started"
        );
        std::thread::sleep(std::time::Duration::from_millis(20));
    };
    // In the command's group, as the lookout's SIGTERM to a source's group
    // reaches every process of it.
    let pgid = |pid: &str| {
        let out = Command::new("ps")
            .args(["-o", "pgid=", "-p", pid])
            .output()
            .unwrap();
        String::from_utf8_lossy(&out.stdout).trim().to_owned()
    };
    let group = child.id().to_string();
    assert_eq!(pgid(&server), group);
    Command::new("kill")
        .args(["-TERM", "--", &format!("-{group}")])
        .status()
        .unwrap();
    let _ = child.wait_with_output();
    let gone_at = std::time::Instant::now();
    while Command::new("kill")
        .args(["-0", &server])
        .stderr(Stdio::null())
        .status()
        .unwrap()
        .success()
    {
        assert!(
            gone_at.elapsed().as_secs() < 10,
            "the server outlived its command"
        );
        std::thread::sleep(std::time::Duration::from_millis(20));
    }
    let _ = std::fs::remove_dir_all(&home);
    let _ = std::fs::remove_dir_all(&dir);
}
