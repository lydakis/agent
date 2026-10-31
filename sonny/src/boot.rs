//! `sonny.start_at_boot`: the daemon started when this machine starts,
//! for a box whose agents should keep working after a restart without
//! anyone opening Sonny. Off by default. On, Sonny's scheduler, which the
//! system starts at login (a LaunchAgent run at load on a Mac, a systemd
//! user service wanted by the user manager's default target, which starts
//! at boot once the user lingers), runs `sonny boot` as it starts, which
//! starts the daemon as a trigger's fire does. The daemon brings back the
//! companions it keeps.
use std::path::Path;

/// The job each setting had of its own before the scheduler ran it, which
/// Sonny's start lets go of.
pub(crate) const OLD_LABEL: &str = "me.lydakis.agent.boot";

/// Whether the setting is on, as config.toml says now.
pub fn on() -> bool {
    agent_client::config::path()
        .and_then(|path| agent_client::config::read(&path).ok())
        .is_some_and(|config| config.sonny.start_at_boot)
}

/// `sonny boot`, the job's run: the daemon of the default store, started
/// unless one runs.
pub fn run() -> i32 {
    let started = crate::daemon_paths(None, None).and_then(|(store, socket)| {
        crate::start_daemon(&store, &socket, true).map_err(|e| e.to_string())
    });
    match started {
        Ok(()) => 0,
        Err(error) => {
            eprintln!(
                "{}",
                serde_json::json!({"error": "boot_failed", "detail": error})
            );
            1
        }
    }
}

/// The setting made so: on, the scheduler set up, which reads it as it
/// starts. Says what came of it.
pub fn apply(on: bool) -> Result<String, String> {
    if !on {
        return Ok("the daemon no longer starts at boot".into());
    }
    crate::scheduler::ensure()?;
    Ok(match crate::trigger::Scheduler::here() {
        crate::trigger::Scheduler::Launchd => "the daemon starts when you log in".into(),
        crate::trigger::Scheduler::Systemd => match linger() {
            Ok(()) => "the daemon starts when this machine boots".into(),
            Err(error) => format!(
                "the daemon starts when you log in; to start it at boot, `sudo loginctl enable-linger {}` ({error})",
                user()
            ),
        },
    })
}

/// At Sonny's start: the job of its own an earlier Sonny kept let go of,
/// and the scheduler running while the setting is on.
pub fn at_start() {
    std::thread::spawn(|| {
        if on()
            && let Err(error) = crate::scheduler::ring()
        {
            eprintln!("{}", crate::trigger::error_json(&error));
        }
    });
}

fn user() -> String {
    std::env::var("USER")
        .or_else(|_| std::env::var("LOGNAME"))
        .unwrap_or_else(|_| "$USER".into())
}

/// The user's manager kept running without a login, so it starts at boot.
fn linger() -> Result<(), String> {
    if Path::new("/var/lib/systemd/linger").join(user()).exists() {
        return Ok(());
    }
    let out = std::process::Command::new("loginctl")
        .arg("enable-linger")
        .stdin(std::process::Stdio::null())
        .output()
        .map_err(|e| format!("loginctl: {e}"))?;
    match out.status.success() {
        true => Ok(()),
        false => Err(String::from_utf8_lossy(&out.stderr).trim().to_owned()),
    }
}
