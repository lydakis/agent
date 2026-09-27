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
const MAX_ENV_FILE: u64 = 64 * 1024;
const MAX_SHELL_OUTPUT: u64 = 1024 * 1024;

static LOGIN: OnceCell<Option<Vec<(OsString, OsString)>>> = OnceCell::const_new();

async fn login() -> Option<&'static [(OsString, OsString)]> {
    LOGIN.get_or_init(login_environment).await.as_deref()
}

/// `AGENT_MODEL` as a daemon this app starts would see it: `~/.agent/env`
/// over the login shell.
pub async fn model() -> Option<String> {
    let file = env_file().and_then(|file| read_env_file(&file).ok());
    pick_model(file.as_deref(), login().await)
}

fn pick_model(
    file: Option<&[(String, String)]>,
    login: Option<&[(OsString, OsString)]>,
) -> Option<String> {
    let model =
        match file.and_then(|pairs| pairs.iter().rev().find(|(key, _)| key == "AGENT_MODEL")) {
            Some((_, value)) => value.clone(),
            None => login?
                .iter()
                .find(|(key, _)| key == "AGENT_MODEL")?
                .1
                .to_str()?
                .to_owned(),
        };
    (!model.is_empty()).then_some(model)
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
    pub async fn start(&mut self, agent: &Path, store: &Path) -> Result<(), String> {
        if let Some((at, reason)) = &self.failed
            && at.elapsed() < RETRY_AFTER
        {
            return Err(reason.clone());
        }
        let result = start(agent, store).await;
        self.failed = result.as_ref().err().map(|e| (Instant::now(), e.clone()));
        result
    }
}

async fn start(agent: &Path, store: &Path) -> Result<(), String> {
    let mut command = Command::new(agent);
    if let Some(environment) = login().await {
        command.env_clear().envs(environment.iter().cloned());
    }
    if let Some(file) = env_file() {
        command.envs(read_env_file(&file)?);
    }
    command
        .arg("start")
        .arg("--store")
        .arg(store)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    let output = tokio::time::timeout(START_TIMEOUT, command.output())
        .await
        .map_err(|_| "daemon_start_timeout: agent start did not return".to_owned())?
        .map_err(|e| format!("daemon_start_failed: {}: {e}", agent.display()))?;
    if output.status.success() {
        return Ok(());
    }
    // The CLI's error line, `agent: CODE: DETAIL`, is the reason to show.
    let stderr = String::from_utf8_lossy(&output.stderr);
    let reason = stderr
        .lines()
        .rev()
        .find_map(|line| line.strip_prefix("agent: "))
        .map(str::to_owned)
        .unwrap_or_else(|| {
            format!(
                "daemon_start_failed: agent start exited with {}",
                output.status
            )
        });
    Err(if reason.starts_with("usage: no provider") {
        format!("{reason}; or put KEY=VALUE lines in ~/.agent/env")
    } else {
        reason
    })
}

fn env_file() -> Option<PathBuf> {
    std::env::var_os("HOME").map(|home| PathBuf::from(home).join(".agent/env"))
}

/// `KEY=VALUE` lines, `export` optional, `#` comments, and values optionally
/// in matching quotes. A missing file is empty. One others can read, or a line
/// that is not an assignment, is refused by path and line, never by value.
fn read_env_file(path: &Path) -> Result<Vec<(String, String)>, String> {
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
    fn the_model_is_the_env_files_then_the_login_shells() {
        let file = |model: &str| vec![("AGENT_MODEL".to_owned(), model.to_owned())];
        let login = vec![(
            OsString::from("AGENT_MODEL"),
            OsString::from("openai/login"),
        )];
        assert_eq!(
            pick_model(Some(&file("anthropic/file")), Some(&login)).as_deref(),
            Some("anthropic/file")
        );
        assert_eq!(
            pick_model(Some(&[]), Some(&login)).as_deref(),
            Some("openai/login")
        );
        assert_eq!(pick_model(None, None), None);
        // An empty assignment in the file means no model, as it would for the daemon.
        assert_eq!(pick_model(Some(&file("")), Some(&login)), None);
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

    #[tokio::test]
    async fn a_failed_start_reports_the_cli_reason_and_is_not_repeated_at_once() {
        let root = std::env::temp_dir().join(format!("agent-app-start-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).unwrap();
        let agent = root.join("agent");
        let calls = root.join("calls");
        std::fs::write(
            &agent,
            format!(
                "#!/bin/sh\necho started >> '{}'\necho 'agent: usage: no provider' >&2\nexit 2\n",
                calls.display()
            ),
        )
        .unwrap();
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&agent, std::fs::Permissions::from_mode(0o755)).unwrap();
        let mut starts = Starts::default();
        let store = root.join("state.sqlite");
        let reason = "usage: no provider; or put KEY=VALUE lines in ~/.agent/env";
        assert_eq!(starts.start(&agent, &store).await.unwrap_err(), reason);
        assert_eq!(starts.start(&agent, &store).await.unwrap_err(), reason);
        assert_eq!(std::fs::read_to_string(&calls).unwrap(), "started\n");
        std::fs::remove_dir_all(root).unwrap();
    }
}
