//! Getting a daemon when none answers. A packaged app carries `agent` beside
//! its own executable and asks it for one with `agent start`, so the app
//! starts a daemon exactly as a CLI command would: same provider detection,
//! same log beside the store, same process group that outlives the window.
//!
//! A window opened from the Dock or Finder inherits launchd's environment,
//! not the user's shell, so provider keys exported in a shell profile would be
//! missing. `agent start` therefore runs with the environment of the user's
//! login shell, read once per start; if that cannot be read it runs with the
//! app's own.
use std::{
    ffi::OsString,
    path::{Path, PathBuf},
    process::Stdio,
    time::{Duration, Instant},
};
use tokio::process::Command;

/// The CLI bounds its own startup at 10 s; the shell gets the rest.
const START_TIMEOUT: Duration = Duration::from_secs(30);
const SHELL_TIMEOUT: Duration = Duration::from_secs(10);
/// A failed start is not repeated on every two-second reattach: its reason
/// stands until this has passed.
const RETRY_AFTER: Duration = Duration::from_secs(30);
const MARKER: &str = "__agent_app_environment__";

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
    if let Some(environment) = login_environment().await {
        command.env_clear().envs(environment);
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
    Err(stderr
        .lines()
        .rev()
        .find_map(|line| line.strip_prefix("agent: "))
        .map(str::to_owned)
        .unwrap_or_else(|| {
            format!(
                "daemon_start_failed: agent start exited with {}",
                output.status
            )
        }))
}

/// The environment an interactive login shell ends up with, where people
/// export provider keys. Anything the profile prints comes before the marker;
/// a value may contain anything, the marker included.
async fn login_environment() -> Option<Vec<(OsString, OsString)>> {
    let shell = std::env::var_os("SHELL").filter(|shell| !shell.is_empty())?;
    let output = tokio::time::timeout(
        SHELL_TIMEOUT,
        Command::new(shell)
            .args(["-l", "-i", "-c"])
            .arg(format!("echo {MARKER}; /usr/bin/env -0"))
            .stdin(Stdio::null())
            .stderr(Stdio::null())
            .kill_on_drop(true)
            .output(),
    )
    .await
    .ok()?
    .ok()?;
    output
        .status
        .success()
        .then(|| parse_environment(&output.stdout))
        .flatten()
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
        assert_eq!(
            starts.start(&agent, &store).await.unwrap_err(),
            "usage: no provider"
        );
        assert_eq!(
            starts.start(&agent, &store).await.unwrap_err(),
            "usage: no provider"
        );
        assert_eq!(std::fs::read_to_string(&calls).unwrap(), "started\n");
        std::fs::remove_dir_all(root).unwrap();
    }
}
