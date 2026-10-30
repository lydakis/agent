//! `sonny.start_at_boot`: the daemon started when this machine starts,
//! for a box whose agents should keep working after a restart without
//! anyone opening Sonny. Off by default. On, Sonny keeps a job that runs
//! `sonny boot`, which starts the daemon as a trigger's fire does: a
//! LaunchAgent run at load (at login on a Mac), or a systemd user service
//! wanted by the user manager's default target, which starts at boot once
//! the user lingers. The daemon brings back the companions it keeps.
use std::path::{Path, PathBuf};

const LABEL: &str = "me.lydakis.agent.boot";

/// The job's file, where the scheduler reads it.
fn job() -> Result<(crate::trigger::Scheduler, PathBuf), String> {
    let jobs = crate::trigger::Places::home()?;
    let file = match jobs.scheduler {
        crate::trigger::Scheduler::Launchd => format!("{LABEL}.plist"),
        crate::trigger::Scheduler::Systemd => format!("{LABEL}.service"),
    };
    Ok((jobs.scheduler, jobs.jobs.join(file)))
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

/// The setting made so: the job set up when `on`, or taken away. Says
/// what came of it.
pub fn apply(on: bool) -> Result<String, String> {
    let (scheduler, file) = job()?;
    if !on {
        if !file.exists() {
            return Ok("the daemon does not start at boot".into());
        }
        unload(scheduler)?;
        std::fs::remove_file(&file).map_err(|e| e.to_string())?;
        return Ok("the daemon no longer starts at boot".into());
    }
    let sonny = crate::skills::sonny_path().ok_or("boot_failed: Sonny's own path is unknown")?;
    let text = definition(scheduler, &sonny, &crate::trigger::environment(scheduler));
    if let Some(dir) = file.parent() {
        std::fs::create_dir_all(dir).map_err(|e| e.to_string())?;
    }
    if scheduler == crate::trigger::Scheduler::Launchd {
        // bootstrap refuses a label already loaded.
        let _ = unload(scheduler);
    }
    std::fs::write(&file, text).map_err(|e| e.to_string())?;
    if let Err(error) = load(scheduler, &file) {
        let _ = std::fs::remove_file(&file);
        return Err(error);
    }
    Ok(match scheduler {
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

/// At Sonny's start: the job as the setting says, rewritten when Sonny
/// has moved, so an upgrade keeps it working.
pub fn at_start() {
    let Some(on) = agent_client::config::path()
        .and_then(|path| agent_client::config::read(&path).ok())
        .map(|config| config.sonny.start_at_boot)
    else {
        return;
    };
    let (Ok((scheduler, file)), Some(sonny)) = (job(), crate::skills::sonny_path()) else {
        return;
    };
    let text = std::fs::read_to_string(&file).ok();
    let current = text
        .as_deref()
        .is_some_and(|text| text.contains(&program(scheduler, &sonny)));
    if on != text.is_some() || (on && !current) {
        std::thread::spawn(move || {
            let _ = apply(on);
        });
    }
}

/// How the job names Sonny, which `at_start` looks for.
fn program(scheduler: crate::trigger::Scheduler, sonny: &Path) -> String {
    let sonny = sonny.to_string_lossy();
    match scheduler {
        crate::trigger::Scheduler::Launchd => {
            format!("<string>{}</string>", crate::trigger::escape(&sonny))
        }
        crate::trigger::Scheduler::Systemd => {
            format!("ExecStart={} boot", crate::trigger::quoted(&sonny, true))
        }
    }
}

/// The job: `sonny boot` once, as the scheduler starts. It runs with
/// `environment` (HOME, SHELL, PATH as triggers keep them), so the daemon
/// starts with the login shell's environment and its provider keys.
fn definition(
    scheduler: crate::trigger::Scheduler,
    sonny: &Path,
    environment: &[(&str, String)],
) -> String {
    match scheduler {
        crate::trigger::Scheduler::Launchd => format!(
            "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n<!DOCTYPE plist PUBLIC \"-//Apple//DTD PLIST 1.0//EN\" \"http://www.apple.com/DTDs/PropertyList-1.0.dtd\">\n<plist version=\"1.0\">\n<dict>\n  <key>Label</key>\n  <string>{LABEL}</string>\n  <key>ProgramArguments</key>\n  <array>\n    {}\n    <string>boot</string>\n  </array>\n  <key>RunAtLoad</key>\n  <true/>\n{}</dict>\n</plist>\n",
            program(scheduler, sonny),
            crate::trigger::environment_section(environment)
        ),
        // A oneshot: `sonny boot` starts the daemon in a scope of its own,
        // so it outlives the service, as a trigger's fire does.
        crate::trigger::Scheduler::Systemd => format!(
            "# Sonny's start of the daemon at boot: sonny.start_at_boot in ~/.agent/config.toml.\n[Unit]\nDescription=Agent daemon at boot\n\n[Service]\nType=oneshot\n{}\n{}\n[Install]\nWantedBy=default.target\n",
            program(scheduler, sonny),
            crate::trigger::unit_environment(environment)
        ),
    }
}

fn load(scheduler: crate::trigger::Scheduler, file: &Path) -> Result<(), String> {
    match scheduler {
        crate::trigger::Scheduler::Launchd => {
            crate::trigger::schedule(crate::trigger::Job::Load(file))
        }
        crate::trigger::Scheduler::Systemd => {
            let manager = crate::trigger::UserManager::find()?;
            manager.run(&["daemon-reload"])?;
            manager.run(&["enable", &format!("{LABEL}.service")])
        }
    }
}

fn unload(scheduler: crate::trigger::Scheduler) -> Result<(), String> {
    match scheduler {
        crate::trigger::Scheduler::Launchd => {
            match crate::trigger::schedule(crate::trigger::Job::Unload(LABEL)) {
                Err(error) if !error.starts_with(crate::trigger::NOT_LOADED) => Err(error),
                _ => Ok(()),
            }
        }
        // Disabling leaves the daemon running: it is in a scope of its own.
        crate::trigger::Scheduler::Systemd => match crate::trigger::UserManager::find() {
            Ok(manager) => manager.run(&["disable", &format!("{LABEL}.service")]),
            Err(_) => Ok(()),
        },
    }
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::trigger::Scheduler;

    #[test]
    fn the_job_starts_the_daemon_once_as_the_scheduler_starts() {
        let environment = [("HOME", "/h".to_owned()), ("SHELL", "/bin/zsh".to_owned())];
        let sonny = Path::new("/opt/Sonny Dir/sonny");
        let plist = definition(Scheduler::Launchd, sonny, &environment);
        assert!(plist.contains("<string>/opt/Sonny Dir/sonny</string>\n    <string>boot</string>"));
        assert!(plist.contains("<key>RunAtLoad</key>\n  <true/>"));
        assert!(plist.contains("<key>SHELL</key>"));
        assert!(
            !plist.contains("ProcessType"),
            "the daemon is no background job"
        );
        assert!(plist.contains(&program(Scheduler::Launchd, sonny)));
        let unit = definition(Scheduler::Systemd, sonny, &environment);
        assert!(unit.contains("Type=oneshot\nExecStart=\"/opt/Sonny Dir/sonny\" boot\n"));
        assert!(unit.contains("[Install]\nWantedBy=default.target\n"));
        assert!(unit.contains("SHELL=/bin/zsh"), "{unit}");
        assert!(unit.contains(&program(Scheduler::Systemd, sonny)));
    }
}
