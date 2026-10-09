//! Getting a daemon when none answers. A packaged app carries `agent` beside
//! its own executable and asks it for one with `agent start`, so the app
//! starts a daemon exactly as a CLI command would: same provider detection,
//! same log beside the store, same process group that outlives the window.
//!
//! A window opened from the Dock or Finder inherits launchd's environment,
//! not the user's shell, so provider keys exported in a shell profile would be
//! missing. `agent start` therefore runs with the environment of the user's
//! login shell, read once per app run; if that cannot be read it runs with the
//! app's own. Keys kept out of shell profiles go in `~/.agent/env`, `KEY=VALUE`
//! lines readable only by their owner, which the app adds on top: the file is
//! the app's, and neither the CLI nor the daemon reads it. The app's default
//! model comes from the same place when its own environment has none.
use std::{
    ffi::OsString,
    path::{Path, PathBuf},
    process::Stdio,
    time::{Duration, Instant},
};
use tokio::{process::Command, sync::OnceCell};

/// The CLI bounds its own startup at 10 s; the shell gets the rest.
const START_TIMEOUT: Duration = Duration::from_secs(30);
const SHELL_TIMEOUT: Duration = Duration::from_secs(10);
/// A failed start is not repeated on every two-second reattach: its reason
/// stands until this has passed.
const RETRY_AFTER: Duration = Duration::from_secs(30);
const MARKER: &str = "__agent_app_environment__";
/// Keys, not documents: bounds the read that starts every daemon.
pub const MAX_ENV_FILE: u64 = 64 * 1024;
const MAX_SHELL_OUTPUT: u64 = 1024 * 1024;

static LOGIN: OnceCell<Option<Vec<(OsString, OsString)>>> = OnceCell::const_new();

pub async fn login() -> Option<&'static [(OsString, OsString)]> {
    LOGIN.get_or_init(login_environment).await.as_deref()
}

/// The `agent` shipped beside this executable, if there is one.
pub fn bundled() -> Option<PathBuf> {
    let agent = std::env::current_exe().ok()?.parent()?.join("agent");
    agent.is_file().then_some(agent)
}

/// The last start that failed, and why, so a reattach loop reports it
/// instead of starting shells.
#[derive(Default)]
pub struct Starts {
    failed: Option<(Instant, String)>,
}

impl Starts {
    /// Start the store's daemon, on `socket` when it listens somewhere other
    /// than the store's own rendezvous.
    pub async fn start(
        &mut self,
        agent: &Path,
        store: &Path,
        socket: Option<&Path>,
    ) -> Result<(), String> {
        if let Some((at, reason)) = &self.failed
            && at.elapsed() < RETRY_AFTER
        {
            return Err(reason.clone());
        }
        let result = start(agent, store, socket).await;
        self.failed = result.as_ref().err().map(|e| (Instant::now(), e.clone()));
        result
    }
    /// Settings changed: the last failure's reason may no longer stand.
    pub fn forget(&mut self) {
        self.failed = None;
    }
}

/// Stop the store's daemon with `agent shutdown`, which returns once the
/// process is gone, so the next start is a new daemon, not the draining one.
/// Turns still running are cancelled; none running is no daemon to stop.
pub async fn stop(agent: &Path, store: &Path) -> Result<(), String> {
    let mut command = Command::new(agent);
    command
        .arg("shutdown")
        .arg("--store")
        .arg(store)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    let output = tokio::time::timeout(START_TIMEOUT, command.output())
        .await
        .map_err(|_| "daemon_stop_timeout: agent shutdown did not return".to_owned())?
        .map_err(|e| format!("daemon_stop_failed: {}: {e}", agent.display()))?;
    let reason = cli_reason(&output.stderr);
    match reason {
        _ if output.status.success() => Ok(()),
        Some(reason) if reason.starts_with("daemon_unavailable") => Ok(()),
        Some(reason) => Err(reason),
        None => Err(format!(
            "daemon_stop_failed: agent shutdown exited with {}",
            output.status
        )),
    }
}

/// A protocol mismatch as the page acts on it: `daemon_older` when the
/// daemon announced an older protocol than this app's, `daemon_newer` when
/// it announced a newer one, from the error's facts rather than its text.
pub fn age(error: &agent_client::Error) -> String {
    let announced = (error.facts.as_ref())
        .and_then(|facts| facts.get("protocol"))
        .and_then(|p| p.as_u64());
    match announced {
        Some(p) if p < agent_client::PROTOCOL => format!(
            "daemon_older: the daemon speaks protocol {p}, this app {}",
            agent_client::PROTOCOL
        ),
        Some(p) if p > agent_client::PROTOCOL => format!(
            "daemon_newer: the daemon speaks protocol {p}, this app {}; update the app",
            agent_client::PROTOCOL
        ),
        _ => error.to_string(),
    }
}

/// How long an older daemon gets to end its turns and close its store.
const REPLACE_TIMEOUT: Duration = Duration::from_secs(30);

/// Stop the daemon at `socket` when it speaks an older protocol than this
/// app, as after an upgrade, so the next attach starts the bundled one. It
/// gets SIGTERM, which every protocol honours: running turns end as
/// interrupted and the store keeps every chat. A newer daemon is left alone,
/// and so is one this app can already talk to.
pub async fn replace_older(socket: &Path) -> Result<(), String> {
    let facts = match agent_client::Client::connect(socket).await {
        Ok((client, _events)) => {
            client.close().await;
            return Ok(());
        }
        Err(error) if error.code == "daemon_unavailable" => return Ok(()),
        Err(error) if error.code == "daemon_protocol_mismatch" => error.facts.unwrap_or_default(),
        Err(error) => return Err(error.to_string()),
    };
    // Only a daemon that greeted as one and says it is older is signalled; a
    // listener that did not greet as a daemon fails to connect otherwise.
    match facts.get("protocol").and_then(|p| p.as_u64()) {
        Some(protocol) if protocol < agent_client::PROTOCOL => {}
        protocol => {
            return Err(format!(
                "daemon_newer: the daemon speaks protocol {}, this app {}; update the app",
                protocol.map_or("unknown".into(), |p| p.to_string()),
                agent_client::PROTOCOL
            ));
        }
    }
    let pid = (facts.get("pid").and_then(|p| p.as_i64()))
        .and_then(|pid| libc::pid_t::try_from(pid).ok())
        .filter(|pid| *pid > 1)
        .ok_or(
            "daemon_pid_unknown: the daemon did not say its process; stop it with its own agent",
        )?;
    // SAFETY: a signal to the process the daemon named as itself.
    if unsafe { libc::kill(pid, libc::SIGTERM) } != 0 {
        return Err(format!(
            "daemon_stop_failed: {}",
            std::io::Error::last_os_error()
        ));
    }
    let deadline = Instant::now() + REPLACE_TIMEOUT;
    while running(pid) {
        if Instant::now() > deadline {
            return Err("daemon_stop_timeout: the older daemon did not exit".into());
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    Ok(())
}

/// Whether a process is still there, as the CLI's shutdown waits on it: an
/// exited one nobody has reaped yet (a zombie, where `/proc` says so) has
/// closed its store and counts as gone.
fn running(pid: libc::pid_t) -> bool {
    // SAFETY: signal 0 only asks whether the process still exists.
    let exists = unsafe { libc::kill(pid, 0) } == 0
        || std::io::Error::last_os_error().raw_os_error() == Some(libc::EPERM);
    exists && !exited(pid)
}

/// A process that exited but that its parent has not reaped yet: it has
/// closed its store, so it counts as gone.
#[cfg(target_os = "linux")]
fn exited(pid: libc::pid_t) -> bool {
    std::fs::read_to_string(format!("/proc/{pid}/stat")).is_ok_and(|stat| {
        stat.rsplit_once(") ")
            .is_some_and(|(_, state)| state.starts_with('Z'))
    })
}

#[cfg(target_os = "macos")]
fn exited(pid: libc::pid_t) -> bool {
    // SAFETY: plain data the kernel fills in, at most `size` bytes of it.
    let mut info: libc::proc_bsdinfo = unsafe { std::mem::zeroed() };
    let size = std::mem::size_of::<libc::proc_bsdinfo>() as libc::c_int;
    // SAFETY: `info` is `size` bytes the call may write, and outlives it.
    let got =
        unsafe { libc::proc_pidinfo(pid, libc::PROC_PIDTBSDINFO, 0, (&raw mut info).cast(), size) };
    got == size && info.pbi_status == libc::SZOMB
}

#[cfg(not(any(target_os = "linux", target_os = "macos")))]
fn exited(_pid: libc::pid_t) -> bool {
    false
}

/// The CLI's failure, one JSON object on stderr, as `CODE: DETAIL`.
fn cli_reason(stderr: &[u8]) -> Option<String> {
    String::from_utf8_lossy(stderr)
        .lines()
        .rev()
        .find_map(|line| {
            let failure: serde_json::Value = serde_json::from_str(line).ok()?;
            let code = failure["error"].as_str()?;
            Some(match failure["detail"].as_str() {
                Some(detail) => format!("{code}: {detail}"),
                None => code.to_owned(),
            })
        })
}

async fn start(agent: &Path, store: &Path, socket: Option<&Path>) -> Result<(), String> {
    let mut command = Command::new(agent);
    if let Some(environment) = login().await {
        command.env_clear().envs(environment.iter().cloned());
    }
    if let Some(file) = env_file() {
        apply(&mut command, read_env_file(&file)?);
    }
    command
        .arg("start")
        .arg("--store")
        .arg(store)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    if let Some(socket) = socket {
        command.arg("--socket").arg(socket);
    }
    let output = tokio::time::timeout(START_TIMEOUT, command.output())
        .await
        .map_err(|_| "daemon_start_timeout: agent start did not return".to_owned())?
        .map_err(|e| format!("daemon_start_failed: {}: {e}", agent.display()))?;
    if output.status.success() {
        return Ok(());
    }
    let reason = cli_reason(&output.stderr).unwrap_or_else(|| {
        format!(
            "daemon_start_failed: agent start exited with {}",
            output.status
        )
    });
    // Nothing to run yet: the page's setup is the answer, not the CLI's flags.
    Err(if reason.starts_with("usage: no provider") {
        "no_provider: connect a provider in Settings".to_owned()
    } else {
        reason
    })
}

/// The env file's settings over the command's environment. An empty value
/// unsets what the login shell set: Settings writes one to clear it.
fn apply(command: &mut Command, pairs: Vec<(String, String)>) {
    for (key, value) in pairs {
        match value.is_empty() {
            true => command.env_remove(key),
            false => command.env(key, value),
        };
    }
}

pub fn env_file() -> Option<PathBuf> {
    std::env::var_os("HOME").map(|home| PathBuf::from(home).join(".agent/env"))
}

/// `KEY=VALUE` lines, `export` optional, `#` comments, and values optionally
/// in matching quotes. A missing file is empty. One others can read, or a line
/// that is not an assignment, is refused by path and line, never by value.
pub fn read_env_file(path: &Path) -> Result<Vec<(String, String)>, String> {
    use std::{io::Read, os::unix::fs::PermissionsExt};
    let unreadable =
        |error: std::io::Error| format!("env_file_unreadable: {}: {error}", path.display());
    // Checked before opening: opening a FIFO for reading would block.
    let metadata = match std::fs::metadata(path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(error) => return Err(unreadable(error)),
    };
    if !metadata.is_file() || metadata.len() > MAX_ENV_FILE {
        return Err(format!(
            "env_file_invalid: {} must be a regular file of at most {MAX_ENV_FILE} bytes",
            path.display()
        ));
    }
    let mut text = String::new();
    std::fs::File::open(path)
        .and_then(|file| file.take(MAX_ENV_FILE + 1).read_to_string(&mut text))
        .map_err(unreadable)?;
    if text.len() as u64 > MAX_ENV_FILE {
        return Err(format!(
            "env_file_invalid: {} must be a regular file of at most {MAX_ENV_FILE} bytes",
            path.display()
        ));
    }
    let mode = metadata.permissions().mode();
    if mode & 0o077 != 0 {
        return Err(format!(
            "env_file_permissions: {} is readable by others; chmod 600 it",
            path.display()
        ));
    }
    let mut environment = Vec::new();
    for (index, line) in text.lines().enumerate() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let line = line.strip_prefix("export ").unwrap_or(line).trim_start();
        let valid = |key: &str| {
            key.chars()
                .next()
                .is_some_and(|c| c.is_ascii_alphabetic() || c == '_')
                && key.chars().all(|c| c.is_ascii_alphanumeric() || c == '_')
        };
        let Some((key, value)) = line
            .split_once('=')
            .map(|(key, value)| (key.trim_end(), value))
            .filter(|(key, _)| valid(key))
        else {
            return Err(format!(
                "env_file_invalid: {} line {}: expected KEY=VALUE",
                path.display(),
                index + 1
            ));
        };
        let value = value.trim();
        let value = ['"', '\'']
            .iter()
            .find_map(|quote| {
                value
                    .strip_prefix(*quote)
                    .and_then(|inner| inner.strip_suffix(*quote))
            })
            .unwrap_or(value);
        environment.push((key.to_owned(), value.to_owned()));
    }
    Ok(environment)
}

/// The environment an interactive login shell ends up with, where people
/// export provider keys. Anything the profile prints comes before the marker;
/// a value may contain anything, the marker included.
async fn login_environment() -> Option<Vec<(OsString, OsString)>> {
    use tokio::io::AsyncReadExt;
    let shell = std::env::var_os("SHELL").filter(|shell| !shell.is_empty())?;
    let mut child = Command::new(shell)
        .args(["-l", "-i", "-c"])
        .arg(format!("echo {MARKER}; /usr/bin/env -0"))
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .kill_on_drop(true)
        .spawn()
        .ok()?;
    let mut stdout = child.stdout.take()?.take(MAX_SHELL_OUTPUT + 1);
    let mut output = Vec::new();
    let read = async {
        stdout.read_to_end(&mut output).await.ok()?;
        child.wait().await.ok()
    };
    // Bounded in time and in bytes: a noisy profile is not an environment.
    let status = tokio::time::timeout(SHELL_TIMEOUT, read).await.ok()??;
    if !status.success() || output.len() as u64 > MAX_SHELL_OUTPUT {
        return None;
    }
    parse_environment(&output)
}

fn parse_environment(stdout: &[u8]) -> Option<Vec<(OsString, OsString)>> {
    use std::os::unix::ffi::OsStrExt;
    let marker = format!("{MARKER}\n");
    let start = stdout
        .windows(marker.len())
        .position(|window| window == marker.as_bytes())?
        + marker.len();
    let environment: Vec<_> = stdout[start..]
        .split(|byte| *byte == 0)
        .filter_map(|entry| {
            let at = entry.iter().position(|byte| *byte == b'=')?;
            (at > 0).then(|| {
                (
                    std::ffi::OsStr::from_bytes(&entry[..at]).to_owned(),
                    std::ffi::OsStr::from_bytes(&entry[at + 1..]).to_owned(),
                )
            })
        })
        .collect();
    (!environment.is_empty()).then_some(environment)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A daemon at `socket` announcing `protocol` and a stand-in process as
    /// its own; the returned channel says when that process has exited.
    fn announcing(
        socket: &Path,
        protocol: u64,
    ) -> (u32, std::sync::mpsc::Receiver<std::process::ExitStatus>) {
        greeting(socket, "ready", protocol)
    }

    /// A listener whose first line is `event` with a protocol and a process.
    fn greeting(
        socket: &Path,
        event: &'static str,
        protocol: u64,
    ) -> (u32, std::sync::mpsc::Receiver<std::process::ExitStatus>) {
        use std::io::Write;
        let mut process = std::process::Command::new("sleep")
            .arg("30")
            .spawn()
            .unwrap();
        let pid = process.id();
        let (exited, exit) = std::sync::mpsc::channel();
        // Reaped as it exits, so it does not linger as a zombie.
        std::thread::spawn(move || exited.send(process.wait().unwrap()));
        let _ = std::fs::remove_file(socket);
        let listener = std::os::unix::net::UnixListener::bind(socket).unwrap();
        std::thread::spawn(move || {
            for stream in listener.incoming() {
                let Ok(mut stream) = stream else { return };
                let ready = serde_json::json!({"event": event, "protocol": protocol, "pid": pid});
                let _ = writeln!(stream, "{ready}");
            }
        });
        (pid, exit)
    }

    #[test]
    fn a_mismatch_is_aged_by_the_protocol_the_daemon_announced() {
        let mismatch = |protocol: Option<u64>| {
            let mut error = agent_client::Error::with("daemon_protocol_mismatch", "any wording");
            error.facts = protocol
                .map(|p| Box::new(serde_json::Map::from_iter([("protocol".into(), p.into())])));
            age(&error)
        };
        let now = agent_client::PROTOCOL;
        assert!(mismatch(Some(now - 1)).starts_with("daemon_older:"));
        assert!(mismatch(Some(now + 1)).starts_with("daemon_newer:"));
        assert!(mismatch(None).starts_with("daemon_protocol_mismatch"));
    }

    #[test]
    fn an_older_daemon_is_stopped_and_a_newer_one_left_alone() {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        let dir = std::env::temp_dir().join(format!("agent-replace-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let older = dir.join("older.sock");
        let (_, exit) = announcing(&older, agent_client::PROTOCOL - 1);
        runtime.block_on(replace_older(&older)).unwrap();
        assert!(exit.recv_timeout(Duration::from_secs(5)).is_ok());
        let newer = dir.join("newer.sock");
        let (pid, exit) = announcing(&newer, agent_client::PROTOCOL + 1);
        let refused = runtime.block_on(replace_older(&newer)).unwrap_err();
        assert!(refused.starts_with("daemon_newer"), "{refused}");
        assert!(exit.recv_timeout(Duration::from_millis(200)).is_err());
        // SAFETY: the stand-in this test started.
        unsafe { libc::kill(pid as libc::pid_t, libc::SIGKILL) };
        // A listener that did not greet as a daemon is not signalled, whatever
        // protocol it names.
        for protocol in [agent_client::PROTOCOL - 1, agent_client::PROTOCOL] {
            let foreign = dir.join(format!("foreign-{protocol}.sock"));
            let (pid, exit) = greeting(&foreign, "hello", protocol);
            let refused = runtime.block_on(replace_older(&foreign)).unwrap_err();
            assert!(refused.starts_with("daemon_greeting_invalid"), "{refused}");
            assert!(exit.recv_timeout(Duration::from_millis(200)).is_err());
            // SAFETY: the stand-in this test started.
            unsafe { libc::kill(pid as libc::pid_t, libc::SIGKILL) };
        }
        // Nothing listening is nothing to replace.
        runtime
            .block_on(replace_older(&dir.join("none.sock")))
            .unwrap();
        std::fs::remove_dir_all(dir).unwrap();
    }

    /// An exited daemon its parent has not reaped has closed its store: the
    /// wait for it ends rather than timing out.
    #[cfg(target_os = "linux")]
    #[test]
    fn an_exited_daemon_nobody_reaped_is_gone() {
        let mut process = std::process::Command::new("sleep")
            .arg("30")
            .spawn()
            .unwrap();
        let pid = process.id() as libc::pid_t;
        assert!(running(pid));
        // SAFETY: the stand-in this test started.
        unsafe { libc::kill(pid, libc::SIGKILL) };
        let deadline = Instant::now() + Duration::from_secs(5);
        while running(pid) && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(10));
        }
        assert!(!running(pid), "a zombie counts as exited");
        process.wait().unwrap();
    }

    #[test]
    fn the_environment_is_read_after_whatever_the_profile_printed() {
        let stdout = format!("Welcome!\n{MARKER}\nHOME=/h\0KEY=a=b\n{MARKER}\n\0=bad\0junk\0");
        let environment = parse_environment(stdout.as_bytes()).unwrap();
        assert_eq!(
            environment,
            vec![
                ("HOME".into(), "/h".into()),
                ("KEY".into(), format!("a=b\n{MARKER}\n").into())
            ]
        );
        assert!(parse_environment(b"no marker\0HOME=/h\0").is_none());
        assert!(parse_environment(format!("{MARKER}\n").as_bytes()).is_none());
    }

    #[test]
    fn the_env_file_takes_assignments_and_refuses_anything_else() {
        use std::os::unix::fs::PermissionsExt;
        let root = std::env::temp_dir().join(format!("agent-app-env-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).unwrap();
        let file = root.join("env");
        assert_eq!(read_env_file(&file).unwrap(), vec![]);
        std::fs::write(
            &file,
            "# keys\n\nOPENAI_API_KEY=sk-1\nexport ANTHROPIC_API_KEY = \"a b\"\nX='q'\nEMPTY=\nURL=a=b\n",
        )
        .unwrap();
        std::fs::set_permissions(&file, std::fs::Permissions::from_mode(0o600)).unwrap();
        let pairs = |list: &[(&str, &str)]| {
            list.iter()
                .map(|(k, v)| (k.to_string(), v.to_string()))
                .collect::<Vec<_>>()
        };
        assert_eq!(
            read_env_file(&file).unwrap(),
            pairs(&[
                ("OPENAI_API_KEY", "sk-1"),
                ("ANTHROPIC_API_KEY", "a b"),
                ("X", "q"),
                ("EMPTY", ""),
                ("URL", "a=b"),
            ])
        );
        std::fs::write(&file, "OK=1\nsk-secret\n").unwrap();
        let error = read_env_file(&file).unwrap_err();
        assert!(
            error.starts_with("env_file_invalid: ")
                && error.ends_with("line 2: expected KEY=VALUE"),
            "{error}"
        );
        assert!(!error.contains("sk-secret"));
        std::fs::write(&file, "1X=1\n").unwrap();
        assert!(read_env_file(&file).is_err());
        std::fs::write(&file, "X=".repeat(40_000)).unwrap();
        assert!(
            read_env_file(&file)
                .unwrap_err()
                .starts_with("env_file_invalid: ")
        );
        std::fs::remove_file(&file).unwrap();
        std::fs::create_dir(&file).unwrap();
        assert!(
            read_env_file(&file)
                .unwrap_err()
                .starts_with("env_file_invalid: ")
        );
        std::fs::remove_dir(&file).unwrap();
        std::fs::write(&file, "OK=1\n").unwrap();
        std::fs::set_permissions(&file, std::fs::Permissions::from_mode(0o644)).unwrap();
        assert!(
            read_env_file(&file)
                .unwrap_err()
                .starts_with("env_file_permissions: ")
        );
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn an_empty_setting_unsets_the_login_shells() {
        let mut command = Command::new("agent");
        command.env("AWS_PROFILE", "shell");
        let pair = |key: &str, value: &str| (key.to_owned(), value.to_owned());
        apply(
            &mut command,
            vec![pair("AWS_PROFILE", ""), pair("AWS_REGION", "eu-west-1")],
        );
        let set: Vec<_> = command.as_std().get_envs().collect();
        assert!(set.contains(&(std::ffi::OsStr::new("AWS_PROFILE"), None)));
        assert!(set.contains(&(
            std::ffi::OsStr::new("AWS_REGION"),
            Some(std::ffi::OsStr::new("eu-west-1"))
        )));
    }

    /// An executable written by a child process. Had this process written
    /// it, another test's child could inherit the open file between its fork
    /// and exec, and Linux refuses to run a file open for writing ("Text file
    /// busy").
    fn script(path: &Path, text: &str) {
        use std::io::Write;
        let _ = std::fs::remove_file(path);
        let mut writer = std::process::Command::new("/bin/sh")
            .args(["-c", "cat > \"$0\" && chmod 755 \"$0\""])
            .arg(path)
            .stdin(Stdio::piped())
            .spawn()
            .unwrap();
        writer
            .stdin
            .take()
            .unwrap()
            .write_all(text.as_bytes())
            .unwrap();
        assert!(writer.wait().unwrap().success());
    }

    #[tokio::test]
    async fn a_failed_start_reports_the_cli_reason_and_is_not_repeated_at_once() {
        let root = std::env::temp_dir().join(format!("agent-app-start-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).unwrap();
        let agent = root.join("agent");
        let calls = root.join("calls");
        script(
            &agent,
            &format!(
                "#!/bin/sh\necho started >> '{}'\necho '{{\"error\":\"usage\",\"detail\":\"no provider\"}}' >&2\nexit 2\n",
                calls.display()
            ),
        );
        let mut starts = Starts::default();
        let store = root.join("state.sqlite");
        let reason = "no_provider: connect a provider in Settings";
        assert_eq!(
            starts.start(&agent, &store, None).await.unwrap_err(),
            reason
        );
        assert_eq!(
            starts.start(&agent, &store, None).await.unwrap_err(),
            reason
        );
        assert_eq!(std::fs::read_to_string(&calls).unwrap(), "started\n");
        // Changed settings try again at once.
        starts.forget();
        assert_eq!(
            starts.start(&agent, &store, None).await.unwrap_err(),
            reason
        );
        assert_eq!(
            std::fs::read_to_string(&calls).unwrap(),
            "started\nstarted\n"
        );
        std::fs::remove_dir_all(root).unwrap();
    }

    #[tokio::test]
    async fn stopping_no_daemon_is_done_and_any_other_failure_is_its_reason() {
        let root = std::env::temp_dir().join(format!("agent-app-stop-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).unwrap();
        let agent = root.join("agent");
        let store = root.join("state.sqlite");
        for (said, stopped) in [
            ("", Ok(())),
            (
                r#"echo '{"error":"daemon_unavailable"}' >&2; exit 1"#,
                Ok(()),
            ),
            (
                r#"echo '{"error":"daemon_shutdown_timeout","detail":"42"}' >&2; exit 1"#,
                Err("daemon_shutdown_timeout: 42".to_owned()),
            ),
        ] {
            script(
                &agent,
                &format!("#!/bin/sh\n[ \"$1 $2\" = 'shutdown --store' ] || exit 9\n{said}\n"),
            );
            assert_eq!(stop(&agent, &store).await, stopped);
        }
        std::fs::remove_dir_all(root).unwrap();
    }
}
