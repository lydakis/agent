//! Companions: commands a client asks the daemon to keep running for as
//! long as it runs, such as a client's own helpers that agents wait on.
//! The daemon knows nothing of what they are. Each is kept in the store
//! by name, started once the daemon listens, started again with backoff
//! when it exits, and stopped when the daemon stops, so it lives as long
//! as the daemon whoever started it.
use agent_runtime::{Error, Result, fail_with};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::{
    collections::{BTreeMap, HashMap},
    path::{Path, PathBuf},
    sync::{Arc, Mutex},
    time::{Duration, Instant},
};
use tokio::{sync::watch, task::JoinHandle};

/// The first wait before a companion that exited starts again, doubled
/// each time it exits again soon, up to `BACKOFF_MAX`.
const BACKOFF: Duration = Duration::from_secs(1);
const BACKOFF_MAX: Duration = Duration::from_secs(60);
/// A run this long counts as healthy: the next wait starts over.
const HEALTHY: Duration = Duration::from_secs(60);
/// How long a stopped companion has after SIGTERM before SIGKILL.
const GRACE: Duration = Duration::from_secs(5);
const NAME_BYTES: usize = 64;

/// What a companion runs: a command and its arguments, as `exec` takes
/// them, with variables over the daemon's own and a folder to run in.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Spec {
    pub command: String,
    #[serde(default)]
    pub args: Vec<String>,
    #[serde(default)]
    pub env: BTreeMap<String, String>,
    pub cwd: Option<String>,
}

/// A companion's add request: its name and what it runs.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Added {
    pub name: String,
    pub command: String,
    #[serde(default)]
    pub args: Vec<String>,
    #[serde(default)]
    pub env: BTreeMap<String, String>,
    pub cwd: Option<String>,
}

impl Added {
    pub fn split(self) -> (String, Spec) {
        let spec = Spec {
            command: self.command,
            args: self.args,
            env: self.env,
            cwd: self.cwd,
        };
        (self.name, spec)
    }
}

impl Spec {
    pub fn check(&self, name: &str) -> Result<()> {
        let refuse = |why: String| fail_with("invalid_companion", format!("{name}: {why}"));
        if name.is_empty()
            || name.len() > NAME_BYTES
            || name.starts_with('.')
            || !name
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b"._-".contains(&b))
        {
            return fail_with(
                "invalid_companion",
                format!(
                    "a companion's name is 1 to {NAME_BYTES} letters, digits, '.', '_' or '-', not starting with '.'"
                ),
            );
        }
        if self.command.is_empty() {
            return refuse("command is empty".into());
        }
        if let Some(key) = self
            .env
            .keys()
            .find(|key| key.is_empty() || key.contains(['=', '\0']))
        {
            return refuse(format!("{key:?} is not a variable name"));
        }
        if self
            .cwd
            .as_deref()
            .is_some_and(|cwd| !Path::new(cwd).is_absolute())
        {
            return refuse("cwd must be an absolute path".into());
        }
        let nul = |text: &String| text.contains('\0');
        if nul(&self.command) || self.args.iter().any(nul) || self.env.values().any(nul) {
            return refuse("a NUL byte".into());
        }
        Ok(())
    }
}

/// How a companion is doing, as `companions` lists it.
#[derive(Default)]
struct Status {
    pid: Option<u32>,
    started_ms: Option<i64>,
    /// Starts after the first.
    restarts: u64,
    last_exit: Option<Value>,
}

struct Kept {
    spec: Spec,
    status: Arc<Mutex<Status>>,
    stop: watch::Sender<bool>,
    task: JoinHandle<()>,
}

/// The companions this daemon keeps, by name.
pub struct Companions {
    /// The variables every companion gets over the daemon's own: where
    /// the daemon is.
    environment: Vec<(String, String)>,
    /// Where each companion's output goes, `STORE.companions/NAME.log`.
    logs: PathBuf,
    /// Whether they run: only a daemon on a socket has one to reach it on.
    runs: bool,
    kept: HashMap<String, Kept>,
}

impl Companions {
    pub fn new(store: &Path, environment: Vec<(String, String)>, runs: bool) -> Self {
        let mut logs = store.as_os_str().to_owned();
        logs.push(".companions");
        Self {
            environment,
            logs: logs.into(),
            runs,
            kept: HashMap::new(),
        }
    }

    /// Keep `spec` running under `name`: a new one starts, one whose spec
    /// changed starts again once the old run has stopped, and the same
    /// spec again changes nothing.
    pub fn keep(&mut self, name: &str, spec: Spec) {
        if self.kept.get(name).is_some_and(|kept| kept.spec == spec) {
            return;
        }
        let before = self.kept.remove(name).map(|kept| {
            let _ = kept.stop.send(true);
            kept.task
        });
        let status = Arc::new(Mutex::new(Status::default()));
        let (stop, stopped) = watch::channel(false);
        let run = Run {
            name: name.to_owned(),
            spec: spec.clone(),
            environment: self.environment.clone(),
            log: self.logs.join(format!("{name}.log")),
            status: status.clone(),
        };
        let runs = self.runs;
        let task = tokio::spawn(async move {
            if let Some(before) = before {
                let _ = before.await;
            }
            if runs {
                run.keep(stopped).await;
            }
        });
        self.kept.insert(
            name.to_owned(),
            Kept {
                spec,
                status,
                stop,
                task,
            },
        );
    }

    /// Stop `name`'s companion; whether one was kept.
    pub fn drop_one(&mut self, name: &str) -> bool {
        self.kept
            .remove(name)
            .map(|kept| kept.stop.send(true))
            .is_some()
    }

    /// Stop every companion and wait for them to exit.
    pub async fn stop(&mut self) {
        let tasks: Vec<_> = self
            .kept
            .drain()
            .map(|(_, kept)| {
                let _ = kept.stop.send(true);
                kept.task
            })
            .collect();
        futures_util::future::join_all(tasks).await;
    }

    /// Whether `pid` is a companion's process: its sessions keep no daemon
    /// from going idle.
    pub fn runs_pid(&self, pid: u32) -> bool {
        self.kept
            .values()
            .any(|kept| kept.status.lock().unwrap_or_else(|e| e.into_inner()).pid == Some(pid))
    }

    /// Each companion, by name, with what it runs and how it is doing.
    pub fn listed(&self) -> Value {
        let mut names: Vec<_> = self.kept.keys().collect();
        names.sort();
        let listed: Vec<Value> = names
            .into_iter()
            .map(|name| self.shown(name).expect("kept"))
            .collect();
        json!(listed)
    }

    pub fn shown(&self, name: &str) -> Option<Value> {
        let kept = self.kept.get(name)?;
        let status = kept.status.lock().unwrap_or_else(|e| e.into_inner());
        let mut shown = json!({"name": name, "command": kept.spec.command,
            "args": kept.spec.args, "env": kept.spec.env, "cwd": kept.spec.cwd,
            "running": status.pid.is_some(), "pid": status.pid,
            "started_ms": status.started_ms, "restarts": status.restarts,
            "last_exit": status.last_exit});
        if !self.runs {
            shown["running"] = json!(false);
        }
        Some(shown)
    }
}

/// One companion's runs, one after another.
struct Run {
    name: String,
    spec: Spec,
    environment: Vec<(String, String)>,
    log: PathBuf,
    status: Arc<Mutex<Status>>,
}

impl Run {
    fn status(&self) -> std::sync::MutexGuard<'_, Status> {
        self.status.lock().unwrap_or_else(|e| e.into_inner())
    }

    async fn keep(self, mut stop: watch::Receiver<bool>) {
        let mut backoff = BACKOFF;
        let mut first = true;
        while !*stop.borrow() {
            if !first {
                self.status().restarts += 1;
            }
            first = false;
            let started = Instant::now();
            let exit = match self.spawn() {
                Ok(mut child) => {
                    {
                        let mut status = self.status();
                        status.pid = child.id();
                        status.started_ms = Some(epoch_ms());
                    }
                    let exit = tokio::select! {
                        exit = child.wait() => exited(exit),
                        _ = stop.changed() => {
                            terminate(&mut child).await;
                            self.status().pid = None;
                            return;
                        }
                    };
                    self.status().pid = None;
                    exit
                }
                Err(error) => json!({"error": "spawn_failed", "detail": error}),
            };
            let mut exit = exit;
            exit["at_ms"] = json!(epoch_ms());
            self.status().last_exit = Some(exit);
            if started.elapsed() >= HEALTHY {
                backoff = BACKOFF;
            }
            tokio::select! {
                _ = tokio::time::sleep(backoff) => {}
                _ = stop.changed() => return,
            }
            backoff = (backoff * 2).min(BACKOFF_MAX);
        }
    }

    fn spawn(&self) -> std::result::Result<tokio::process::Child, String> {
        std::fs::create_dir_all(self.log.parent().expect("in its folder"))
            .map_err(|e| e.to_string())?;
        let log = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&self.log)
            .map_err(|e| format!("{}: {e}", self.log.display()))?;
        let mut command = tokio::process::Command::new(&self.spec.command);
        command
            .args(&self.spec.args)
            .envs(self.environment.iter().map(|(k, v)| (k, v)))
            .env("AGENT_COMPANION", &self.name)
            .envs(&self.spec.env)
            .stdin(std::process::Stdio::null())
            .stdout(log.try_clone().map_err(|e| e.to_string())?)
            .stderr(log)
            // Its own group, so stopping it stops what it started too.
            .process_group(0)
            .kill_on_drop(true);
        if let Some(cwd) = &self.spec.cwd {
            command.current_dir(cwd);
        }
        command
            .spawn()
            .map_err(|e| format!("{}: {e}", self.spec.command))
    }
}

fn exited(exit: std::io::Result<std::process::ExitStatus>) -> Value {
    use std::os::unix::process::ExitStatusExt;
    match exit {
        Ok(status) => json!({"code": status.code(), "signal": status.signal()}),
        Err(error) => json!({"error": "wait_failed", "detail": error.to_string()}),
    }
}

/// SIGTERM to its group, then SIGKILL once its grace is up.
async fn terminate(child: &mut tokio::process::Child) {
    let Some(pid) = child.id() else { return };
    let group = -(pid as i32);
    // SAFETY: a signal to the process group this companion leads.
    unsafe { libc::kill(group, libc::SIGTERM) };
    if tokio::time::timeout(GRACE, child.wait()).await.is_err() {
        // SAFETY: as above.
        unsafe { libc::kill(group, libc::SIGKILL) };
        let _ = child.wait().await;
    }
}

fn epoch_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_millis() as i64)
}

/// The spec a stored row holds.
pub fn stored(name: &str, spec: &str) -> Result<Spec> {
    serde_json::from_str(spec).map_err(|_| Error::with("store_companion_invalid", name.to_owned()))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn spec(command: &str, args: &[&str]) -> Spec {
        Spec {
            command: command.into(),
            args: args.iter().map(|a| a.to_string()).collect(),
            env: BTreeMap::new(),
            cwd: None,
        }
    }

    #[test]
    fn a_companion_s_name_and_spec_are_checked() {
        assert!(spec("sonny", &["lookout"]).check("lookout").is_ok());
        for name in ["", ".hidden", "a b", &"x".repeat(65)] {
            assert_eq!(
                spec("sonny", &[]).check(name).unwrap_err().code,
                "invalid_companion"
            );
        }
        assert!(spec("", &[]).check("x").is_err());
        let mut relative = spec("sonny", &[]);
        relative.cwd = Some("here".into());
        assert!(relative.check("x").is_err());
        let mut bad = spec("sonny", &[]);
        bad.env.insert("A=B".into(), "1".into());
        assert!(bad.check("x").is_err());
    }

    async fn until(what: impl Fn() -> bool) {
        let deadline = Instant::now() + Duration::from_secs(10);
        while !what() {
            assert!(Instant::now() < deadline, "timed out");
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    }

    #[tokio::test]
    async fn a_companion_runs_again_when_it_exits_and_stops_with_the_daemon() {
        let dir = std::env::temp_dir().join(format!("companions-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let store = dir.join("state.sqlite");
        let mut companions =
            Companions::new(&store, vec![("AGENT_SOCKET".into(), "/s".into())], true);
        let marks = dir.join("marks");
        let script = format!(
            "echo \"$AGENT_COMPANION $AGENT_SOCKET\" >> {}; exit 3",
            marks.display()
        );
        companions.keep("once", spec("sh", &["-c", &script]));
        until(|| std::fs::read_to_string(&marks).is_ok_and(|text| text.lines().count() >= 2)).await;
        assert_eq!(
            std::fs::read_to_string(&marks).unwrap().lines().next(),
            Some("once /s")
        );
        let shown = companions.shown("once").unwrap();
        assert!(shown["restarts"].as_u64().unwrap() >= 1, "{shown}");
        assert_eq!(shown["last_exit"]["code"], 3);
        // The same spec again keeps the run it has.
        companions.keep("long", spec("sleep", &["30"]));
        until(|| companions.shown("long").unwrap()["running"] == true).await;
        let pid = companions.shown("long").unwrap()["pid"].as_u64().unwrap() as u32;
        assert!(companions.runs_pid(pid));
        companions.keep("long", spec("sleep", &["30"]));
        assert_eq!(companions.shown("long").unwrap()["pid"], json!(pid));
        companions.stop().await;
        // SAFETY: signal 0 only asks whether it lives.
        assert_ne!(unsafe { libc::kill(pid as i32, 0) }, 0);
        assert_eq!(companions.listed(), json!([]));
        let _ = std::fs::remove_dir_all(&dir);
    }
}
