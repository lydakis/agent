//! Triggers: an agent woken with a message it wrote for itself, or one
//! written for it, when something happens: a time comes, a file is written,
//! a repository's HEAD moves, or someone fires it by name. The system's own
//! scheduler watches (launchd on a Mac, systemd's user manager on Linux),
//! so a trigger fires with Sonny closed, and once on waking for times the
//! machine slept through. The daemon has no clock or watcher of its own,
//! and no process of a trigger runs between its fires.
//!
//! A trigger is one job: on a Mac a LaunchAgent,
//! `~/Library/LaunchAgents/LABEL.plist`; on Linux a user service,
//! `~/.config/systemd/user/LABEL.service`, with the timer and path units
//! that start it written beside it from its own `X-` sections. The job runs
//! this executable with `--trigger-fire` and everything the fire needs as
//! its arguments: the plist or service is its definition. Below, launchd
//! names either scheduler and the plist either definition, since the two
//! keep a job alike. What its fires did and leave for the next (messages
//! sent, the agent it started, the commit it saw) is
//! `~/.agent/triggers/NAME.json`. A fire messages the agent the trigger was
//! made for, pinned by its id, or starts the agent it names the first time
//! and messages that one after; never a working agent, for a repeating
//! trigger: that time is skipped. A deleted agent takes its triggers with
//! it.
//!
//! `~/.agent/trigger`, a script Sonny writes, is how agents and people
//! add, list, fire and remove them.
use agent_client::Client;
use serde_json::{Value, json};
use std::collections::HashMap;
use std::{
    path::{Path, PathBuf},
    time::Duration,
};

/// What `~/.agent/trigger` runs: `sonny trigger ARGS`.
pub const VERB: &str = "trigger";
/// What a trigger's job runs.
pub const FIRE_FLAG: &str = "--trigger-fire";
/// Every trigger's launchd label and systemd unit name starts with this;
/// the rest is its name.
const LABEL: &str = "me.lydakis.agent.trigger.";
/// A message is a reminder of what to do, not a document.
const MAX_MESSAGE: usize = 16 * 1024;
/// Calendar entries one trigger may expand to.
const MAX_ENTRIES: usize = 1024;
/// launchd's calendar has no year (Sonny writes systemd's without one too),
/// so a one-off time must fall within one.
const MAX_AHEAD: i64 = 364 * 24 * 3600;
/// How far from its time a one-off's fire may be and still be its own: a
/// time zone changed since it was made moves launchd's clock by up to a day.
const SLACK: i64 = 2 * 24 * 3600;
/// A one-off's calendar entry comes again a year later; a fire this late
/// is that, not a wake after a long sleep.
const STALE: i64 = 182 * 24 * 3600;
/// How long an `--if` command may run before its fire is skipped.
const GATE_TIMEOUT: Duration = Duration::from_secs(60);
/// How long a fire waits for the turn whose answer goes to `--reply-to`:
/// the longest wait the daemon takes.
const REPLY_WAIT_MS: u64 = 86_400_000;
/// How long `fire` waits for launchd's run of the job to say what came of
/// it: its gate's whole time, and then some for the send.
const FIRE_WAIT: Duration = Duration::from_secs(90);
pub const USAGE: &str = "usage: trigger add [--name NAME] [WHEN] [--bot NAME | --start NAME --model PROVIDER/MODEL [--effort LEVEL]] [--reply-to BOT] [--if CMD] [--runs N] [--turn-budget-tokens N] [--replace] -- MESSAGE\n         WHEN: --every N{m,h,d} | --in N{m,h} | --at 'YYYY-MM-DD HH:MM' | --cron 'MIN HOUR DAY MONTH WEEKDAY' | --file PATH | --commit REPO; none: only fire runs it\n       trigger ls [--after NAME]\n       trigger show NAME\n       trigger fire NAME\n       trigger rm NAME";

/// What runs a trigger's job, and the definition it reads.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Scheduler {
    /// A LaunchAgent plist, loaded with `launchctl`.
    Launchd,
    /// A user service with its timer and path units, through `systemctl --user`.
    Systemd,
}

impl Scheduler {
    /// This system's: launchd on a Mac, systemd elsewhere.
    pub fn here() -> Self {
        match cfg!(target_os = "macos") {
            true => Self::Launchd,
            false => Self::Systemd,
        }
    }
    /// How a definition's file name ends.
    fn suffix(self) -> &'static str {
        match self {
            Self::Launchd => ".plist",
            Self::Systemd => ".service",
        }
    }
}

/// Where triggers live: `jobs` holds their definitions (the LaunchAgents
/// folder, or systemd's user unit folder), and `~/.agent/triggers` what
/// each one's fires did.
#[derive(Clone)]
pub struct Places {
    pub jobs: PathBuf,
    pub state: PathBuf,
    pub scheduler: Scheduler,
}

impl Places {
    pub fn home() -> Result<Self, String> {
        let home = PathBuf::from(std::env::var_os("HOME").ok_or("no HOME for triggers")?);
        let scheduler = Scheduler::here();
        Ok(Self {
            jobs: match scheduler {
                Scheduler::Launchd => home.join("Library/LaunchAgents"),
                // Where the user manager reads units it is not given a path for.
                Scheduler::Systemd => xdg("XDG_CONFIG_HOME", &home, ".config").join("systemd/user"),
            },
            state: home.join(".agent/triggers"),
            scheduler,
        })
    }
    /// The trigger's definition: its plist, or its service.
    fn job(&self, name: &str) -> PathBuf {
        self.jobs
            .join(format!("{LABEL}{name}{}", self.scheduler.suffix()))
    }
    fn last(&self, name: &str) -> PathBuf {
        self.state.join(format!("{name}.json"))
    }
    /// The trigger's asks: launchd runs its job while any is there, one run
    /// at a time, and again after one that ends with an ask still there.
    fn asks(&self, name: &str) -> PathBuf {
        self.state.join(format!("{name}.asks"))
    }
    /// The ask a fire took out of the queue, until it is done with it.
    fn taking(&self, name: &str) -> PathBuf {
        taking(&self.asks(name))
    }
    /// The turn an ask from `fire NAME` sent, for the `fire` waiting on it:
    /// beside the trigger's files, so a trigger that ends with that send
    /// does not take it. `fire` removes it, and Sonny's start one left.
    fn receipt(&self, name: &str, file: &str) -> PathBuf {
        self.state.join(format!(".fired.{name}.{file}"))
    }
}

/// An XDG base folder: its variable when set to a full path, else its
/// place under HOME.
fn xdg(key: &str, home: &Path, under: &str) -> PathBuf {
    std::env::var_os(key)
        .map(PathBuf::from)
        .filter(|dir| dir.is_absolute())
        .unwrap_or_else(|| home.join(under))
}

/// What the scheduler is asked to do. Tests stand in for it.
pub enum Job<'a> {
    Load(&'a Path),
    Unload(&'a str),
}

pub type Loader<'a> = &'a dyn Fn(Job) -> Result<(), String>;

/// This system's scheduler.
pub fn schedule(what: Job) -> Result<(), String> {
    match Scheduler::here() {
        Scheduler::Launchd => launchctl(what),
        Scheduler::Systemd => systemctl(what),
    }
}

/// `launchctl` in the logged-in user's domain.
fn launchctl(what: Job) -> Result<(), String> {
    if !cfg!(target_os = "macos") {
        return Err("triggers_unsupported: launchd is macOS's".into());
    }
    // SAFETY: getuid takes no arguments and cannot fail.
    let domain = format!("gui/{}", unsafe { libc::getuid() });
    let unloading = matches!(what, Job::Unload(_));
    let args = match what {
        Job::Load(path) => vec![
            "bootstrap".into(),
            domain,
            path.to_string_lossy().into_owned(),
        ],
        Job::Unload(label) => vec!["bootout".into(), format!("{domain}/{label}")],
    };
    let out = std::process::Command::new("/bin/launchctl")
        .args(&args)
        .stdin(std::process::Stdio::null())
        .output()
        .map_err(|e| format!("launchctl: {e}"))?;
    if out.status.success() {
        return Ok(());
    }
    // bootout's ESRCH and "no such service": nothing by that label is loaded.
    let missing = unloading && matches!(out.status.code(), Some(3 | 113));
    Err(format!(
        "{}launchctl {}: {}",
        if missing { NOT_LOADED } else { "" },
        args[0],
        String::from_utf8_lossy(&out.stderr).trim()
    ))
}

/// The units systemd starts a trigger's service with, by the section of
/// the service that says what each holds: a timer for its times, which
/// catches up once on a time missed while the machine slept or was off,
/// and a path unit for its watched path and its asks.
const STARTERS: [(&str, &str, &str); 2] = [
    ("timer", "[X-Timer]", "timers.target"),
    ("path", "[X-Path]", "paths.target"),
];

/// systemd's user manager, through `systemctl --user`. A load writes the
/// service's timer and path units beside it from its `X-` sections and
/// starts them, enabled so the manager starts them again at the next login;
/// an unload stops them and the service, and removes them. The service
/// file itself is the definition, and stays the trigger code's to write
/// and remove. A fire ending its own trigger is not stopped: it ends on its
/// own, as nothing starts it again.
fn systemctl(what: Job) -> Result<(), String> {
    if !cfg!(target_os = "linux") {
        return Err("triggers_unsupported: systemd is Linux's".into());
    }
    let manager = UserManager::find();
    match what {
        Job::Load(path) => {
            let manager = manager?;
            let text = read_record(path)?;
            let dir = path.parent().unwrap_or(Path::new("."));
            let label = path
                .file_name()
                .and_then(|f| f.to_str())
                .and_then(|f| f.strip_suffix(".service"))
                .ok_or_else(|| format!("{}: not a service", path.display()))?;
            let mut units = Vec::new();
            for (kind, section, target) in STARTERS {
                let unit = format!("{label}.{kind}");
                match section_of(&text, section) {
                    Some(body) => {
                        replace(&dir.join(&unit), &starter(label, kind, body, target))?;
                        units.push(unit);
                    }
                    None => forget(&dir.join(&unit))?,
                }
            }
            let mut enable = vec!["enable", "--now"];
            enable.extend(units.iter().map(String::as_str));
            let loaded = manager
                .run(&["daemon-reload"])
                .and_then(|()| manager.run(&enable));
            if loaded.is_err() {
                let _ = unload_units(&manager, dir, label);
            }
            loaded
        }
        Job::Unload(label) => {
            let Some(dir) = Places::home().ok().map(|p| p.jobs) else {
                return Err("no HOME for triggers".into());
            };
            match manager {
                Ok(manager) => unload_units(&manager, &dir, label),
                // Nothing of it can be loaded with no manager to load it in.
                Err(_) if !starters(&dir, label).iter().any(|p| p.exists()) => {
                    Err(format!("{NOT_LOADED}no user manager"))
                }
                Err(error) => Err(error),
            }
        }
    }
}

/// A trigger's timer and path units, and the links that enable them.
fn starters(dir: &Path, label: &str) -> Vec<PathBuf> {
    STARTERS
        .iter()
        .flat_map(|(kind, _, target)| {
            let unit = format!("{label}.{kind}");
            [
                dir.join(&unit),
                dir.join(format!("{target}.wants")).join(unit),
            ]
        })
        .collect()
}

/// Stop a trigger's units and remove its timer and path units, whatever is
/// left of them; not loaded when none was there.
fn unload_units(manager: &UserManager, dir: &Path, label: &str) -> Result<(), String> {
    let files = starters(dir, label);
    let here = files.iter().any(|p| std::fs::symlink_metadata(p).is_ok());
    // The timer and path first, in the same stop, so neither starts the
    // service again; one not loaded (5) stops the rest all the same.
    let mut units: Vec<String> = STARTERS
        .iter()
        .map(|(kind, ..)| format!("{label}.{kind}"))
        .collect();
    if !running_as(label) {
        units.push(format!("{label}.service"));
    }
    let mut stop = vec!["stop"];
    stop.extend(units.iter().map(String::as_str));
    manager.run_allowing(&stop, &[5])?;
    for file in &files {
        forget(file)?;
    }
    // A timer's last time, which a timer of the name made again would
    // otherwise catch up from.
    if let Some(home) = std::env::var_os("HOME") {
        let stamps = xdg("XDG_DATA_HOME", Path::new(&home), ".local/share");
        forget(&stamps.join(format!("systemd/timers/stamp-{label}.timer")))?;
    }
    manager.run(&["daemon-reload"])?;
    match here {
        true => Ok(()),
        false => Err(format!("{NOT_LOADED}{label}")),
    }
}

/// A timer or path unit, from its section of the service.
fn starter(label: &str, kind: &str, body: &str, target: &str) -> String {
    let name = match kind {
        "timer" => "Timer",
        _ => "Path",
    };
    format!(
        "# Sonny writes this from {label}.service, and removes it with it.\n[{name}]\n{}\n\n[Install]\nWantedBy={target}\n",
        body.trim_end()
    )
}

/// The body of a unit's `[NAME]` section, as Sonny writes them.
fn section_of<'a>(text: &'a str, section: &str) -> Option<&'a str> {
    let at = text.find(&format!("\n{section}\n"))? + section.len() + 2;
    let rest = &text[at..];
    Some(&rest[..rest.find("\n[").map_or(rest.len(), |end| end + 1)])
}

/// Whether this process is the trigger's service: a fire ending its trigger.
fn running_as(label: &str) -> bool {
    let me = format!("/{label}.service");
    std::fs::read_to_string("/proc/self/cgroup")
        .is_ok_and(|groups| groups.lines().any(|line| line.ends_with(&me)))
}

/// The logged-in user's systemd manager, found by its runtime folder as
/// `systemctl --user` finds it.
struct UserManager {
    runtime: PathBuf,
}

impl UserManager {
    fn find() -> Result<Self, String> {
        let runtime = std::env::var_os("XDG_RUNTIME_DIR")
            .map(PathBuf::from)
            .unwrap_or_else(|| {
                // SAFETY: getuid takes no arguments and cannot fail.
                PathBuf::from(format!("/run/user/{}", unsafe { libc::getuid() }))
            });
        match runtime.join("systemd/private").exists() {
            true => Ok(Self { runtime }),
            false => Err(format!(
                "triggers_unsupported: triggers on Linux use systemd's user manager, which is not running here ({} has no systemd/private)",
                runtime.display()
            )),
        }
    }
    fn run(&self, args: &[&str]) -> Result<(), String> {
        self.run_allowing(args, &[])
    }
    /// `systemctl --user ARGS`, also fine when it exits with one of `fine`.
    fn run_allowing(&self, args: &[&str], fine: &[i32]) -> Result<(), String> {
        let out = std::process::Command::new("systemctl")
            .arg("--user")
            .args(args)
            .env("XDG_RUNTIME_DIR", &self.runtime)
            .stdin(std::process::Stdio::null())
            .output()
            .map_err(|e| format!("systemctl: {e}"))?;
        match out.status.success() || out.status.code().is_some_and(|c| fine.contains(&c)) {
            true => Ok(()),
            false => Err(format!(
                "systemctl {}: {}",
                args[0],
                String::from_utf8_lossy(&out.stderr).trim()
            )),
        }
    }
}

/// How a loader says the label it was asked to unload is not loaded.
const NOT_LOADED: &str = "not_loaded: ";

/// Unload a label; one launchd does not have is already unloaded.
fn unload(label: &str, jobs: Loader) -> Result<(), String> {
    match jobs(Job::Unload(label)) {
        Err(error) if !error.starts_with(NOT_LOADED) => Err(error),
        _ => Ok(()),
    }
}

/// Which daemon a trigger reaches: its store, which a fire may start the
/// daemon of, and the socket that daemon listens on, when it was given one.
/// A socket without its store is reached but never started.
#[derive(Debug, Clone, PartialEq)]
pub struct Daemon {
    pub store: Option<PathBuf>,
    pub socket: Option<PathBuf>,
}

impl Daemon {
    /// The daemon of the shell the trigger is made from, found as the CLI
    /// finds it. An agent's shell has both its daemon's store and socket.
    pub(crate) fn current() -> Result<Self, String> {
        // A launchd job has no working folder: paths are kept absolute.
        let absolute = |p: PathBuf| std::path::absolute(&p).unwrap_or(p);
        let socket = std::env::var_os("AGENT_SOCKET")
            .map(PathBuf::from)
            .map(absolute);
        let store = std::env::var_os("AGENT_STORE")
            .map(PathBuf::from)
            .map(absolute)
            .or_else(|| {
                socket
                    .is_none()
                    .then(|| std::env::var_os("HOME"))
                    .flatten()
                    .map(|h| PathBuf::from(h).join(".agent/state.sqlite"))
            });
        if store.is_none() && socket.is_none() {
            return Err("no AGENT_STORE or HOME to find the daemon".into());
        }
        Ok(Self { store, socket })
    }
    pub(crate) fn socket(&self) -> Result<PathBuf, String> {
        match (&self.socket, &self.store) {
            (Some(socket), _) => Ok(socket.clone()),
            (None, Some(store)) => {
                agent_client::socket::default_socket(store).map_err(|e| e.to_string())
            }
            (None, None) => Err("no --store or --socket".into()),
        }
    }
}

/// Whom a trigger messages.
#[derive(Debug, Clone, PartialEq)]
pub enum Target {
    /// An agent that existed when the trigger was made, pinned by its id.
    Bot { name: String, id: i64 },
    /// An agent the first fire starts in the trigger's folder, made by the
    /// agent that added the trigger, when one did; later fires message it.
    Start {
        name: String,
        model: String,
        effort: Option<String>,
        by: Option<(String, i64)>,
    },
}

/// One trigger, as its plist's arguments carry it.
#[derive(Debug, Clone, PartialEq)]
pub struct Trigger {
    pub name: String,
    pub generation: String,
    pub not_before: Option<i64>,
    pub target: Target,
    /// How it was asked for, to show: `every 30m`, `file /a/b`, `fire`.
    pub when: String,
    /// A one-off's time; it fires once, then removes itself.
    pub at: Option<i64>,
    /// The repository whose HEAD it follows; a fire for any other write to
    /// its HEAD log sends nothing.
    pub commit: Option<PathBuf>,
    /// Where `--if` runs and `--start` starts its agent: the folder `add`
    /// ran in.
    pub dir: Option<PathBuf>,
    /// Who gets the turn's answer, pinned by id.
    pub reply_to: Option<(String, i64)>,
    /// `sh -c` this first; anything but exit 0 skips the fire.
    pub gate: Option<String>,
    /// Ends once this many messages went out.
    pub runs: Option<u64>,
    /// The most input plus output tokens each fire's turn may spend.
    pub turn_budget_tokens: Option<u64>,
    /// The path `--file` watches, which each fire checks is still not one
    /// every fire writes.
    pub file: Option<PathBuf>,
    pub daemon: Daemon,
    /// The store lineage its daemon announced when it was made; a daemon
    /// on that socket serving another store is not its daemon.
    pub store_id: String,
    pub message: String,
}

impl Trigger {
    /// The agent it messages.
    fn bot(&self) -> &str {
        match &self.target {
            Target::Bot { name, .. } | Target::Start { name, .. } => name,
        }
    }

    /// The fire's arguments, after the executable.
    fn args(&self) -> Vec<String> {
        let mut args = vec![
            FIRE_FLAG.into(),
            "--name".into(),
            self.name.clone(),
            "--generation".into(),
            self.generation.clone(),
        ];
        let mut pair = |flag: &str, value: String| args.extend([flag.to_owned(), value]);
        match &self.target {
            Target::Bot { name, id } => {
                pair("--bot", name.clone());
                pair("--bot-id", id.to_string());
            }
            Target::Start {
                name,
                model,
                effort,
                by,
            } => {
                pair("--start", name.clone());
                pair("--model", model.clone());
                if let Some(effort) = effort {
                    pair("--effort", effort.clone());
                }
                if let Some((by, id)) = by {
                    pair("--by", by.clone());
                    pair("--by-id", id.to_string());
                }
            }
        }
        pair("--when", self.when.clone());
        if let Some(first) = self.not_before {
            pair("--not-before", first.to_string());
        }
        if let Some(at) = self.at {
            pair("--at", at.to_string());
        }
        let path = |p: &PathBuf| p.to_string_lossy().into_owned();
        if let Some(repo) = &self.commit {
            pair("--commit", path(repo));
        }
        if let Some(dir) = &self.dir {
            pair("--dir", path(dir));
        }
        if let Some((bot, id)) = &self.reply_to {
            pair("--reply-to", bot.clone());
            pair("--reply-to-id", id.to_string());
        }
        if let Some(gate) = &self.gate {
            pair("--if", gate.clone());
        }
        if let Some(runs) = self.runs {
            pair("--runs", runs.to_string());
        }
        if let Some(tokens) = self.turn_budget_tokens {
            pair("--turn-budget-tokens", tokens.to_string());
        }
        if let Some(file) = &self.file {
            pair("--file", path(file));
        }
        if let Some(store) = &self.daemon.store {
            pair("--store", path(store));
        }
        if let Some(socket) = &self.daemon.socket {
            pair("--socket", path(socket));
        }
        pair("--store-id", self.store_id.clone());
        args.extend(["--".into(), self.message.clone()]);
        args
    }

    /// Read back from a fire's arguments, after the flag.
    fn parse(args: &[String]) -> Result<Self, String> {
        let bad = |what: &str| format!("invalid_trigger: {what}");
        let mut v = std::collections::HashMap::<&str, &String>::new();
        let mut iter = args.iter();
        let message = loop {
            let Some(flag) = iter.next() else {
                return Err(bad("no message"));
            };
            if flag == "--" {
                break iter.cloned().collect::<Vec<_>>().join(" ");
            }
            let value = iter
                .next()
                .ok_or_else(|| bad(&format!("{flag} needs a value")))?;
            const FLAGS: [&str; 22] = [
                "--name",
                "--generation",
                "--bot",
                "--bot-id",
                "--start",
                "--model",
                "--effort",
                "--by",
                "--by-id",
                "--when",
                "--not-before",
                "--at",
                "--commit",
                "--dir",
                "--reply-to",
                "--reply-to-id",
                "--if",
                "--runs",
                "--turn-budget-tokens",
                "--file",
                "--store",
                "--socket",
            ];
            let known = FLAGS
                .iter()
                .chain(&["--store-id"])
                .find(|f| **f == flag)
                .ok_or_else(|| bad(flag))?;
            v.insert(*known, value);
        };
        let text = |flag: &str| v.get(flag).map(|s| s.to_string());
        let need = |flag: &str| text(flag).ok_or_else(|| bad(&format!("no {flag}")));
        let number = |flag: &str| -> Result<Option<i64>, String> {
            text(flag)
                .map(|s| s.parse().map_err(|_| bad(flag)))
                .transpose()
        };
        let pinned = |flag: &str, id: &str| -> Result<Option<(String, i64)>, String> {
            match (text(flag), number(id)?) {
                (Some(name), Some(id)) => Ok(Some((name, id))),
                (None, None) => Ok(None),
                _ => Err(bad(&format!("{flag} goes with {id}"))),
            }
        };
        let target = match (pinned("--bot", "--bot-id")?, text("--start")) {
            (Some((name, id)), None) => Target::Bot { name, id },
            (None, Some(name)) => Target::Start {
                name,
                model: need("--model")?,
                effort: text("--effort"),
                by: pinned("--by", "--by-id")?,
            },
            _ => return Err(bad("one of --bot or --start")),
        };
        let daemon = Daemon {
            store: text("--store").map(PathBuf::from),
            socket: text("--socket").map(PathBuf::from),
        };
        if daemon.store.is_none() && daemon.socket.is_none() {
            return Err(bad("no --store or --socket"));
        }
        Ok(Self {
            name: need("--name")?,
            generation: need("--generation")?,
            not_before: number("--not-before")?,
            target,
            when: need("--when")?,
            at: number("--at")?,
            commit: text("--commit").map(PathBuf::from),
            dir: text("--dir").map(PathBuf::from),
            reply_to: pinned("--reply-to", "--reply-to-id")?,
            gate: text("--if"),
            runs: number("--runs")?.map(|n| n.max(1) as u64),
            turn_budget_tokens: number("--turn-budget-tokens")?.map(|n| n.max(1) as u64),
            file: text("--file").map(PathBuf::from),
            daemon,
            store_id: need("--store-id")?,
            message,
        })
    }

    /// Its row, with what its fires left in `state`: the last outcome,
    /// messages sent, and the id of the agent it started.
    fn json(&self, state: &Value) -> Value {
        let (bot_id, start) = match &self.target {
            Target::Bot { id, .. } => (json!(id), Value::Null),
            Target::Start { model, effort, .. } => (
                state["started_id"].clone(),
                json!({"model": model, "effort": effort}),
            ),
        };
        json!({"name": self.name, "generation": self.generation, "when": self.when,
            "bot": self.bot(), "bot_id": bot_id, "start": start,
            "reply_to": self.reply_to.as_ref().map(|r| &r.0), "if": self.gate, "runs": self.runs,
            "turn_budget_tokens": self.turn_budget_tokens,
            "sent": state["sent"].as_u64().unwrap_or(0), "message": self.message,
            "once": self.at.is_some(), "last": state["last"], "checked": state["checked"]})
    }

    /// The first field another trigger of its name differs in, to name in
    /// `trigger_exists`. Its generation and first time are when it was
    /// made, not what it is.
    fn differs(&self, other: &Self) -> Option<&'static str> {
        let (target, mine) = match (&self.target, &other.target) {
            (Target::Bot { .. }, Target::Bot { .. }) => ("bot", true),
            (Target::Start { .. }, Target::Start { .. }) => ("start", true),
            _ => ("bot", false),
        };
        [
            ("when", self.when == other.when),
            (target, mine && self.target == other.target),
            ("message", self.message == other.message),
            ("reply_to", self.reply_to == other.reply_to),
            ("if", self.gate == other.gate),
            ("runs", self.runs == other.runs),
            (
                "turn_budget_tokens",
                self.turn_budget_tokens == other.turn_budget_tokens,
            ),
            ("commit", self.commit == other.commit),
            ("dir", self.dir == other.dir),
            ("daemon", self.daemon == other.daemon),
            ("store_id", self.store_id == other.store_id),
        ]
        .into_iter()
        .find(|(_, same)| !same)
        .map(|(field, _)| field)
    }
}

/// A local wall-clock time.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Local {
    pub year: i32,
    pub month: u8,
    pub day: u8,
    pub hour: u8,
    pub minute: u8,
}

pub(crate) fn local(epoch: i64) -> Local {
    // SAFETY: localtime_r fills the zeroed struct it is given and nothing else.
    let tm = unsafe {
        let mut tm: libc::tm = std::mem::zeroed();
        libc::localtime_r(&(epoch as libc::time_t), &mut tm);
        tm
    };
    Local {
        year: tm.tm_year + 1900,
        month: (tm.tm_mon + 1) as u8,
        day: tm.tm_mday as u8,
        hour: tm.tm_hour as u8,
        minute: tm.tm_min as u8,
    }
}

/// The time a local wall-clock time names, when it names one.
fn epoch_of(at: Local) -> Option<i64> {
    // SAFETY: mktime reads and normalizes the struct it is given.
    let epoch = unsafe {
        let mut tm: libc::tm = std::mem::zeroed();
        tm.tm_year = at.year - 1900;
        tm.tm_mon = at.month as i32 - 1;
        tm.tm_mday = at.day as i32;
        tm.tm_hour = at.hour as i32;
        tm.tm_min = at.minute as i32;
        tm.tm_isdst = -1;
        libc::mktime(&mut tm)
    };
    (epoch != -1 && local(epoch as i64) == at).then_some(epoch as i64)
}

fn now() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_secs() as i64)
}

/// One `StartCalendarInterval` entry; a field left out matches any value.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Entry {
    pub month: Option<u8>,
    pub day: Option<u8>,
    pub weekday: Option<u8>,
    pub hour: Option<u8>,
    pub minute: Option<u8>,
}

/// When, as asked for: how to show it, launchd's entries, a one-off's
/// time, and the path launchd watches.
#[derive(Debug, PartialEq)]
pub struct When {
    pub text: String,
    pub entries: Vec<Entry>,
    pub not_before: Option<i64>,
    pub at: Option<i64>,
    pub watch: Option<PathBuf>,
}

fn amount(value: &str) -> Option<(u32, char)> {
    let unit = value.chars().last()?;
    let n = value[..value.len() - unit.len_utf8()].parse().ok()?;
    (n > 0).then_some((n, unit))
}

/// `--every N{m,h,d}`: counted from the next whole minute, so the first
/// time is one interval after it. Minutes divide an hour and hours a day, which is what a calendar
/// can repeat; anything else is a `--cron`.
pub fn every(value: &str, now: i64) -> Result<When, String> {
    let bad = || {
        format!(
            "invalid_every: {value}: minutes that divide 60, hours that divide 24, or 1d; use --cron for others"
        )
    };
    let (n, unit) = amount(value).ok_or_else(bad)?;
    // From the next whole minute: launchd keeps minutes, and a time never comes early.
    let from = local(now + (60 - now.rem_euclid(60)) % 60);
    let entries = match unit {
        'm' if 60 % n == 0 => (0..60 / n)
            .map(|k| Entry {
                minute: Some(((from.minute as u32 % n) + k * n) as u8),
                ..Entry::default()
            })
            .collect(),
        'h' if 24 % n == 0 => (0..24 / n)
            .map(|k| Entry {
                hour: Some(((from.hour as u32 % n) + k * n) as u8),
                minute: Some(from.minute),
                ..Entry::default()
            })
            .collect(),
        'd' if n == 1 => vec![Entry {
            hour: Some(from.hour),
            minute: Some(from.minute),
            ..Entry::default()
        }],
        _ => return Err(bad()),
    };
    Ok(When {
        text: format!("every {value}"),
        not_before: Some(
            now + (60 - now.rem_euclid(60)) % 60
                + i64::from(n)
                    * match unit {
                        'm' => 60,
                        'h' => 3600,
                        _ => 86400,
                    },
        ),
        entries,
        at: None,
        watch: None,
    })
}

/// A one-off at `epoch`, whole minutes.
fn once(epoch: i64, now: i64) -> Result<When, String> {
    if epoch <= now {
        return Err("invalid_at: that time has passed".into());
    }
    if epoch - now > MAX_AHEAD {
        return Err("invalid_at: a one-off must be within a year".into());
    }
    let at = local(epoch);
    Ok(When {
        not_before: None,
        text: format!(
            "at {}-{:02}-{:02} {:02}:{:02}",
            at.year, at.month, at.day, at.hour, at.minute
        ),
        entries: vec![Entry {
            month: Some(at.month),
            day: Some(at.day),
            hour: Some(at.hour),
            minute: Some(at.minute),
            weekday: None,
        }],
        at: Some(epoch - epoch % 60),
        watch: None,
    })
}

/// `--in N{m,h}`: a one-off that long from now.
pub fn after(value: &str, now: i64) -> Result<When, String> {
    let bad = || format!("invalid_in: {value}: N minutes (m) or hours (h)");
    let (n, unit) = amount(value).ok_or_else(bad)?;
    let seconds = match unit {
        'm' => n as i64 * 60,
        'h' => n as i64 * 3600,
        _ => return Err(bad()),
    };
    // launchd keeps whole minutes: the next one at or after the delay, never before it.
    let epoch = now + seconds;
    once(epoch + (60 - epoch.rem_euclid(60)) % 60, now)
}

/// `--at 'YYYY-MM-DD HH:MM'`, local time.
pub fn at(value: &str, now: i64) -> Result<When, String> {
    let bad = || format!("invalid_at: {value}: YYYY-MM-DD HH:MM");
    let (date, time) = value.trim().split_once([' ', 'T']).ok_or_else(bad)?;
    let parts =
        |text: &str, sep| -> Option<Vec<u32>> { text.split(sep).map(|p| p.parse().ok()).collect() };
    let (Some(date), Some(time)) = (parts(date, '-'), parts(time, ':')) else {
        return Err(bad());
    };
    let (&[year, month, day], &[hour, minute]) = (&date[..], &time[..]) else {
        return Err(bad());
    };
    let small = |v: u32, high: u32| (v <= high).then_some(v as u8).ok_or_else(bad);
    let wanted = Local {
        year: i32::try_from(year).map_err(|_| bad())?,
        month: small(month, 12)?,
        day: small(day, 31)?,
        hour: small(hour, 23)?,
        minute: small(minute, 59)?,
    };
    let epoch = epoch_of(wanted).ok_or_else(bad)?;
    once(epoch, now)
}

/// One cron field: `*`, or values, ranges and steps separated by commas.
/// `None` is any value.
fn field(text: &str, low: u8, high: u8) -> Result<Option<Vec<u8>>, String> {
    if text == "*" {
        return Ok(None);
    }
    let bad = || format!("invalid_cron: {text}");
    let mut values = Vec::new();
    for part in text.split(',') {
        let (range, step) = match part.split_once('/') {
            Some((range, step)) => (
                range,
                step.parse::<u8>().ok().filter(|s| *s > 0).ok_or_else(bad)?,
            ),
            None => (part, 1),
        };
        let (from, to) = match range {
            "*" => (low, high),
            _ => match range.split_once('-') {
                Some((a, b)) => (a.parse().map_err(|_| bad())?, b.parse().map_err(|_| bad())?),
                None => {
                    let v = range.parse().map_err(|_| bad())?;
                    (v, if part.contains('/') { high } else { v })
                }
            },
        };
        if from < low || to > high || from > to {
            return Err(bad());
        }
        values.extend((from..=to).step_by(step as usize));
    }
    values.sort_unstable();
    values.dedup();
    Ok(Some(values))
}

/// `--cron 'MIN HOUR DAY MONTH WEEKDAY'`, as cron reads it: a time matches
/// when either the day or the weekday does, when both are given.
pub fn cron(expression: &str) -> Result<When, String> {
    let parts: Vec<&str> = expression.split_whitespace().collect();
    let [minute, hour, day, month, weekday] = parts[..] else {
        return Err(format!("invalid_cron: {expression}: five fields"));
    };
    let minutes = field(minute, 0, 59)?;
    let hours = field(hour, 0, 23)?;
    let days = field(day, 1, 31)?;
    let months = field(month, 1, 12)?;
    // 7 is Sunday as well as 0.
    let weekdays = field(weekday, 0, 7)?.map(|w| {
        let mut w: Vec<u8> = w.into_iter().map(|d| d % 7).collect();
        w.sort_unstable();
        w.dedup();
        w
    });
    let any = |values: &Option<Vec<u8>>| match values {
        None => vec![None],
        Some(values) => values.iter().copied().map(Some).collect(),
    };
    // Counted before any is made: a broad expression is refused, not built.
    let count = |f: &Option<Vec<u8>>| f.as_ref().map_or(1, Vec::len);
    let each = count(&months) * count(&hours) * count(&minutes);
    let needed = match (&days, &weekdays) {
        (Some(_), Some(_)) => each * (count(&days) + count(&weekdays)),
        _ => each * count(&days) * count(&weekdays),
    };
    if needed > MAX_ENTRIES {
        return Err(format!(
            "invalid_cron: {expression} needs more than {MAX_ENTRIES} calendar entries; narrow it"
        ));
    }
    let product = |days: &Option<Vec<u8>>, weekdays: &Option<Vec<u8>>| {
        let mut out = Vec::new();
        for month in any(&months) {
            for day in any(days) {
                for weekday in any(weekdays) {
                    for hour in any(&hours) {
                        for minute in any(&minutes) {
                            out.push(Entry {
                                month,
                                day,
                                weekday,
                                hour,
                                minute,
                            });
                        }
                    }
                }
            }
        }
        out
    };
    let entries = match (&days, &weekdays) {
        (Some(_), Some(_)) => {
            let mut both = product(&days, &None);
            both.extend(product(&None, &weekdays));
            both
        }
        _ => product(&days, &weekdays),
    };
    Ok(When {
        text: format!("cron {expression}"),
        not_before: None,
        entries,
        at: None,
        watch: None,
    })
}

fn escape(text: &str) -> String {
    text.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
        .replace('\'', "&apos;")
}

fn unescape(text: &str) -> String {
    text.replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&quot;", "\"")
        .replace("&apos;", "'")
        .replace("&amp;", "&")
}

/// `--file PATH`: a write to the file, or a file added to or removed from
/// it when it is a folder. It need not exist yet.
pub fn file(value: &str) -> Result<When, String> {
    let path = std::path::absolute(value).map_err(|e| format!("invalid_file: {value}: {e}"))?;
    watchable(&path).map_err(|why| format!("invalid_file: {value}: {why}"))?;
    Ok(When {
        text: format!("file {}", path.display()),
        entries: Vec::new(),
        not_before: None,
        at: None,
        watch: Some(path),
    })
}

/// A path a scheduler can watch: systemd reads a unit's path to the end of
/// its line, trimmed, and a line ending in a backslash goes on into the
/// next, so one with a control character or ending in a space or a
/// backslash is refused on every system alike.
fn watchable(path: &Path) -> Result<(), &'static str> {
    let text = path.to_string_lossy();
    let end = text.ends_with(|c: char| c.is_whitespace() || c == '\\');
    match text.chars().any(char::is_control) || end {
        true => Err(
            "a path with a control character, or ending in a space or a backslash, cannot be watched",
        ),
        false => Ok(()),
    }
}

/// git about `repo` alone: a hook's `GIT_DIR` and the like would name
/// another repository than the one launchd's fire, which has none, reads.
fn git(repo: &Path, args: &[&str]) -> Option<String> {
    let mut command = std::process::Command::new("git");
    for key in [
        "GIT_DIR",
        "GIT_WORK_TREE",
        "GIT_COMMON_DIR",
        "GIT_INDEX_FILE",
        "GIT_OBJECT_DIRECTORY",
        "GIT_ALTERNATE_OBJECT_DIRECTORIES",
        "GIT_NAMESPACE",
    ] {
        command.env_remove(key);
    }
    let out = command
        .arg("-C")
        .arg(repo)
        .args(args)
        .stdin(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .output()
        .ok()?;
    // Only the newline git ends its answer with goes: a path may end in
    // spaces.
    out.status.success().then(|| {
        let text = String::from_utf8_lossy(&out.stdout);
        text.strip_suffix('\n').unwrap_or(&text).to_owned()
    })
}

/// Where a repository's HEAD was when the trigger last looked, and when.
#[derive(Debug, Default, Clone, PartialEq)]
struct Seen {
    head: Option<String>,
    at: Option<i64>,
}

/// Where HEAD is now, and whether a commit was made since `since`: one
/// HEAD now reaches that the HEAD then did not, committed after the trigger
/// looked. A checkout, a reset, a rebase's start and end or a fast-forward
/// to commits that were there already reach none; a commit, an amend, a
/// merge, a cherry-pick, a rebase that rewrote commits, or a pull of work
/// committed since do. One git run for HEAD and one for the question.
fn moved(repo: &Path, since: &Seen) -> (bool, Seen) {
    // The time first: a commit HEAD does not name yet is made after it.
    let at = Some(self::now());
    let now = Seen {
        head: head(repo),
        at,
    };
    let news = match (&since.head, &now.head) {
        (_, None) => false,
        (then, now) if then == now => false,
        (then, Some(head)) => made_since(repo, then.as_deref(), head, since.at),
    };
    (news, now)
}

/// Whether `head` reaches a commit `then` does not, committed after `at`.
/// A `then` the repository no longer has (made again) reaches nothing; a
/// question git cannot answer is news.
fn made_since(repo: &Path, then: Option<&str>, head: &str, at: Option<i64>) -> bool {
    // git's dates are whole seconds: one in the second it looked counts.
    let since = at.map(|at| format!("--since=@{at}"));
    let mut args = vec!["rev-list", "-n1"];
    args.extend(since.as_deref());
    args.push(head);
    let known =
        then.filter(|then| git(repo, &["cat-file", "-e", &format!("{then}^{{commit}}")]).is_some());
    if let Some(then) = known {
        args.extend(["--not", then]);
    }
    git(repo, &args).is_none_or(|found| !found.is_empty())
}

/// The commit a repository's HEAD names, when it names one.
fn head(repo: &Path) -> Option<String> {
    git(repo, &["rev-parse", "--verify", "-q", "HEAD"])
}

/// `--commit REPO`: its HEAD moves to another commit. git writes the HEAD
/// log, in the repository's own git folder (a worktree has its own), for
/// every move, so launchd watches that; the fire sends only when the commit
/// is not the one the trigger last saw.
pub fn commit(value: &str) -> Result<(When, PathBuf), String> {
    let repo = std::path::absolute(value).map_err(|e| format!("invalid_commit: {value}: {e}"))?;
    let git_dir = git(&repo, &["rev-parse", "--absolute-git-dir"])
        .ok_or_else(|| format!("invalid_commit: {value}: not a git repository"))?;
    let log = PathBuf::from(git_dir).join("logs/HEAD");
    watchable(&log).map_err(|why| format!("invalid_commit: {value}: {why}"))?;
    // git keeps the HEAD log only where core.logAllRefUpdates says so, which
    // a bare repository does not by default: there it would never fire.
    let logs = match git(&repo, &["config", "core.logAllRefUpdates"]) {
        Some(set) => !matches!(
            set.to_ascii_lowercase().as_str(),
            "false" | "no" | "off" | "0" | ""
        ),
        None => git(&repo, &["rev-parse", "--is-bare-repository"]).as_deref() != Some("true"),
    };
    if !logs {
        return Err(format!(
            "invalid_commit: {value}: git keeps no HEAD log here; git -C {} config core.logAllRefUpdates true",
            repo.display()
        ));
    }
    Ok((
        When {
            text: format!("commit {}", repo.display()),
            entries: Vec::new(),
            not_before: None,
            at: None,
            watch: Some(log),
        },
        repo,
    ))
}

/// A trigger's definition, as its scheduler reads it.
pub fn definition(
    scheduler: Scheduler,
    app: &Path,
    trigger: &Trigger,
    when: &When,
    asks: &Path,
    environment: &[(&str, String)],
) -> String {
    match scheduler {
        Scheduler::Launchd => plist(app, trigger, when, asks, environment),
        Scheduler::Systemd => unit(app, trigger, when, asks, environment),
    }
}

/// Whether a definition is a plist, by how it starts, which no message
/// can change; else it is a unit.
fn is_plist(text: &str) -> bool {
    text.trim_start().starts_with('<')
}

/// The LaunchAgent for a trigger. `environment` is what the fire starts
/// with besides launchd's own: the shell whose login environment starts a
/// daemon with your keys, as Sonny starts one. Without calendar entries
/// or a watched path, launchd runs it only when asked (`fire`): an ask is a
/// file in `asks`, which launchd watches as a queue.
pub fn plist(
    app: &Path,
    trigger: &Trigger,
    when: &When,
    asks: &Path,
    environment: &[(&str, String)],
) -> String {
    let mut out = String::from(
        "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n<!DOCTYPE plist PUBLIC \"-//Apple//DTD PLIST 1.0//EN\" \"http://www.apple.com/DTDs/PropertyList-1.0.dtd\">\n<plist version=\"1.0\">\n<dict>\n",
    );
    let string = |s: &str| format!("<string>{}</string>", escape(s));
    out += &format!(
        "  <key>Label</key>\n  {}\n",
        string(&format!("{LABEL}{}", trigger.name))
    );
    out += "  <key>ProgramArguments</key>\n  <array>\n";
    for arg in std::iter::once(app.to_string_lossy().into_owned()).chain(trigger.args()) {
        out += &format!("    {}\n", string(&arg));
    }
    out += "  </array>\n";
    if !when.entries.is_empty() {
        out += "  <key>StartCalendarInterval</key>\n  <array>\n";
        for entry in &when.entries {
            out += "    <dict>";
            for (key, value) in [
                ("Month", entry.month),
                ("Day", entry.day),
                ("Weekday", entry.weekday),
                ("Hour", entry.hour),
                ("Minute", entry.minute),
            ] {
                if let Some(value) = value {
                    out += &format!("<key>{key}</key><integer>{value}</integer>");
                }
            }
            out += "</dict>\n";
        }
        out += "  </array>\n";
    }
    if let Some(path) = &when.watch {
        out += &format!(
            "  <key>WatchPaths</key>\n  <array>\n    {}\n  </array>\n",
            string(&path.to_string_lossy())
        );
    }
    out += &environment_section(environment);
    out += "  <key>ProcessType</key>\n  <string>Background</string>\n</dict>\n</plist>\n";
    queued(&out, asks)
}

/// Whether a definition runs its fires with this environment and no other.
fn runs_with(text: &str, environment: &[(&str, String)]) -> bool {
    match (environment, is_plist(text)) {
        ([], true) => !text.contains("<key>EnvironmentVariables</key>"),
        ([], false) => !text.contains("\nEnvironment="),
        (_, true) => text.contains(&environment_section(environment)),
        (_, false) => text.contains(&unit_environment(environment)),
    }
}

/// The environment a fire runs with, as its plist says it.
fn environment_section(environment: &[(&str, String)]) -> String {
    if environment.is_empty() {
        return String::new();
    }
    let mut out = String::from("  <key>EnvironmentVariables</key>\n  <dict>\n");
    for (key, value) in environment {
        out += &format!("    <key>{key}</key><string>{}</string>\n", escape(value));
    }
    out + "  </dict>\n"
}

/// A plist with `asks` as its queue: launchd runs the job while a file is
/// there, and runs it again when it ends with one still there. The ask a
/// fire took waits beside it (`NAME.taking`) until the fire is done with
/// it, so a fire cut short is run again for it too.
fn queued(text: &str, asks: &Path) -> String {
    let key = format!(
        "  <key>QueueDirectories</key>\n  <array>\n    <string>{}</string>\n    <string>{}</string>\n  </array>\n",
        escape(&asks.to_string_lossy()),
        escape(&taking(asks).to_string_lossy())
    );
    match text.rfind("</dict>") {
        Some(at) if !text.contains("<key>QueueDirectories</key>") => {
            format!("{}{key}{}", &text[..at], &text[at..])
        }
        _ => text.to_owned(),
    }
}

/// The folder beside a queue that holds the ask a fire took.
fn taking(asks: &Path) -> PathBuf {
    asks.with_extension("taking")
}

/// Make a trigger's queue folders, which launchd watches.
fn queues(asks: &Path) -> Result<(), String> {
    [asks.to_owned(), taking(asks)].iter().try_for_each(|dir| {
        std::fs::create_dir_all(dir).map_err(|e| format!("{}: {e}", dir.display()))
    })
}

/// A definition's program arguments, as Sonny writes them.
fn program(text: &str) -> Option<Vec<String>> {
    if !is_plist(text) {
        let line = text.lines().find_map(|l| l.strip_prefix("ExecStart="))?;
        return words(line);
    }
    let rest = &text[text.find("<key>ProgramArguments</key>")?..];
    let array = &rest[rest.find("<array>")? + "<array>".len()..rest.find("</array>")?];
    let mut args = Vec::new();
    let mut at = array;
    while let Some(start) = at.find("<string>") {
        let body = &at[start + "<string>".len()..];
        let end = body.find("</string>")?;
        args.push(unescape(&body[..end]));
        at = &body[end + "</string>".len()..];
    }
    Some(args)
}

/// The path a definition has its scheduler watch, as Sonny writes it.
fn watched(text: &str) -> Option<PathBuf> {
    if !is_plist(text) {
        let line = section_of(text, "[X-Path]")?
            .lines()
            .find_map(|l| l.strip_prefix("PathChanged="))?;
        return Some(PathBuf::from(line.replace("%%", "%")));
    }
    let rest = &text[text.find("<key>WatchPaths</key>")?..];
    let body = &rest[rest.find("<string>")? + "<string>".len()..];
    Some(PathBuf::from(unescape(&body[..body.find("</string>")?])))
}

/// The trigger a definition runs, and the Sonny it runs it with.
fn read_job(text: &str) -> Option<(Trigger, PathBuf)> {
    let args = program(text)?;
    let (app, rest) = args.split_first()?;
    let trigger = Trigger::parse(rest.strip_prefix(&[FIRE_FLAG.to_owned()])?).ok()?;
    Some((trigger, PathBuf::from(app)))
}

/// The definition with the Sonny it runs moved from `was` to `app`.
fn with_app(text: &str, was: &Path, app: &Path) -> String {
    let (was, app) = (was.to_string_lossy(), app.to_string_lossy());
    match is_plist(text) {
        true => text.replacen(
            &format!("<string>{}</string>", escape(&was)),
            &format!("<string>{}</string>", escape(&app)),
            1,
        ),
        false => text.replacen(
            &format!("ExecStart={}", quoted(&was, true)),
            &format!("ExecStart={}", quoted(&app, true)),
            1,
        ),
    }
}

/// The systemd service for a trigger, which runs its fire, and in sections
/// systemd itself skips (`X-`), what its timer and path units hold, which a
/// load writes beside it: the definition is this one file, as a plist is.
/// The timer has its times, and catches up once on one missed while the
/// machine slept or was off; the path unit its watched path, and its asks
/// as a queue: it starts the service while `asks` holds a file, and again
/// when the service ends with one still there. `environment` is as for a
/// plist.
pub fn unit(
    app: &Path,
    trigger: &Trigger,
    when: &When,
    asks: &Path,
    environment: &[(&str, String)],
) -> String {
    let name = &trigger.name;
    let path = |p: &Path| p.to_string_lossy().replace('%', "%%");
    let command: Vec<String> = std::iter::once(app.to_string_lossy().into_owned())
        .chain(trigger.args())
        .map(|arg| quoted(&arg, true))
        .collect();
    let mut out = format!(
        "# Sonny's trigger {name}: `trigger rm {name}` removes it.\n[Unit]\nDescription=Sonny trigger {name}\n# Each start takes one ask: a burst of them, or of writes, is no reason to stop.\nStartLimitIntervalSec=0\n\n[Service]\nType=oneshot\nExecStart={}\n",
        command.join(" ")
    );
    out += &unit_environment(environment);
    if !when.entries.is_empty() {
        out += "\n[X-Timer]\n";
        for entry in &when.entries {
            out += &format!("OnCalendar={}\n", on_calendar(entry));
        }
        out += "Persistent=true\nAccuracySec=1s\n";
    }
    out += "\n[X-Path]\n";
    if let Some(watch) = &when.watch {
        out += &format!("PathChanged={}\n", path(watch));
    }
    out += &format!(
        "DirectoryNotEmpty={}\nDirectoryNotEmpty={}\n",
        path(asks),
        path(&taking(asks))
    );
    out
}

/// The environment a fire runs with, as its unit says it.
fn unit_environment(environment: &[(&str, String)]) -> String {
    environment
        .iter()
        .map(|(key, value)| format!("Environment={}\n", quoted(&format!("{key}={value}"), false)))
        .collect()
}

/// One calendar entry as an `OnCalendar=` time; a field left out is any.
fn on_calendar(entry: &Entry) -> String {
    let two = |v: Option<u8>| v.map_or("*".to_owned(), |v| format!("{v:02}"));
    let weekday = entry.weekday.map_or(String::new(), |w| {
        format!(
            "{} ",
            ["Sun", "Mon", "Tue", "Wed", "Thu", "Fri", "Sat"][w as usize % 7]
        )
    });
    format!(
        "{weekday}*-{}-{} {}:{}:00",
        two(entry.month),
        two(entry.day),
        two(entry.hour),
        two(entry.minute)
    )
}

/// One word of a unit's command or environment, as systemd reads it back:
/// C escapes inside double quotes, `%` doubled for its specifiers and, in a
/// command, `$` doubled for its variables.
fn quoted(text: &str, command: bool) -> String {
    let mut out = String::from("\"");
    for c in text.chars() {
        match c {
            '"' => out += "\\\"",
            '\\' => out += "\\\\",
            '\n' => out += "\\n",
            '\t' => out += "\\t",
            '\r' => out += "\\r",
            '%' => out += "%%",
            '$' if command => out += "$$",
            c => out.push(c),
        }
    }
    out + "\""
}

/// The words of a command `quoted` wrote.
fn words(line: &str) -> Option<Vec<String>> {
    let mut words = Vec::new();
    let mut chars = line.chars();
    loop {
        match chars.find(|c| *c != ' ') {
            None => return Some(words),
            Some('"') => {}
            Some(_) => return None,
        }
        let mut word = String::new();
        loop {
            match chars.next()? {
                '"' => break,
                '\\' => word.push(match chars.next()? {
                    'n' => '\n',
                    't' => '\t',
                    'r' => '\r',
                    c => c,
                }),
                // Doubled: `%%` and `$$`.
                c @ ('%' | '$') => {
                    chars.next().filter(|next| *next == c)?;
                    word.push(c);
                }
                c => word.push(c),
            }
        }
        words.push(word);
    }
}

/// A plist's trigger and app, or why it could not be read.
type Read = Result<(Trigger, PathBuf), String>;

const PAGE_SIZE: usize = 64;
// Shared by plist writes and record reads; oversized definitions never replace a job.
const MAX_RECORD: u64 = 128 * 1024;

fn read_record(path: &Path) -> Result<String, String> {
    use std::io::Read;
    let mut text = String::new();
    std::fs::File::open(path)
        .and_then(|file| file.take(MAX_RECORD + 1).read_to_string(&mut text))
        .map_err(|e| format!("unreadable: {e}"))?;
    if text.len() as u64 > MAX_RECORD {
        return Err("unreadable: trigger record too large".into());
    }
    Ok(text)
}

fn read_trigger(path: &Path) -> Read {
    read_record(path).and_then(|text| {
        read_job(&text).ok_or_else(|| "unreadable: not a trigger's definition".into())
    })
}

/// Walk one plist at a time when refreshing Sonny's path.
fn read_all(places: &Places) -> impl Iterator<Item = (String, Read)> {
    let suffix = places.scheduler.suffix();
    std::fs::read_dir(&places.jobs)
        .into_iter()
        .flatten()
        .flatten()
        .filter_map(move |e| {
            let file = e.file_name().into_string().ok()?;
            let name = file.strip_prefix(LABEL)?.strip_suffix(suffix)?.to_owned();
            Some((name, read_trigger(&e.path())))
        })
}

fn triggers(places: &Places) -> Vec<(Trigger, PathBuf)> {
    read_all(places).filter_map(|(_, r)| r.ok()).collect()
}

fn read_json(path: &Path) -> Option<Value> {
    read_record(path)
        .ok()
        .and_then(|s| serde_json::from_str::<Value>(&s).ok())
}

/// What this trigger's fires left: its state file, when it is this
/// trigger's and not an earlier one's of the same name.
fn state(places: &Places, trigger: &Trigger) -> Value {
    read_json(&places.last(&trigger.name))
        .filter(|r| r["generation"] == trigger.generation)
        .unwrap_or(Value::Null)
}

/// A bounded page in name order. Directory scans retain only the next
/// PAGE_SIZE + 1 names; only that page's messages and results are read.
pub fn list(places: &Places, after: Option<&str>) -> Value {
    let mut names = std::collections::BTreeSet::new();
    for (dir, prefix, suffix) in [
        (&places.jobs, LABEL, places.scheduler.suffix()),
        (&places.state, "", ".json"),
    ] {
        for e in std::fs::read_dir(dir).into_iter().flatten().flatten() {
            let file = e.file_name();
            let Some(name) = file
                .to_str()
                .and_then(|f| f.strip_prefix(prefix))
                .and_then(|f| f.strip_suffix(suffix))
            else {
                continue;
            };
            if valid_name(name).is_err() || after.is_some_and(|a| name <= a) {
                continue;
            }
            names.insert(name.to_owned());
            if names.len() > PAGE_SIZE + 1 {
                names.pop_last();
            }
        }
    }
    let more = names.len() > PAGE_SIZE;
    if more {
        names.pop_last();
    }
    let next = more.then(|| names.last().cloned()).flatten();
    let rows: Vec<_> = names.into_iter().map(|name| row(places, &name)).collect();
    json!({"triggers": rows, "next_after": next})
}

/// Every trigger there now that was made on the daemon whose store is
/// `store_id`, in name order: its row, as `list` shows it, and the agents
/// it is for: the one it messages, the one that added it to start that
/// one, and the one its answers go to.
pub fn on_store(places: &Places, store_id: &str) -> Vec<(Value, Vec<String>)> {
    let mut found: Vec<(Value, Vec<String>)> = triggers(places)
        .into_iter()
        .filter(|(t, _)| t.store_id == store_id)
        .map(|(t, _)| {
            let mut row = t.json(&state(places, &t));
            if t.at.is_some_and(|at| now() > at + SLACK) {
                row["missed"] = json!(true);
            }
            let mut bots = vec![t.bot().to_owned()];
            if let Target::Start {
                by: Some((by, _)), ..
            } = &t.target
            {
                bots.push(by.clone());
            }
            if let Some((to, _)) = &t.reply_to {
                bots.push(to.clone());
            }
            (row, bots)
        })
        .collect();
    found.sort_by(|a, b| a.0["name"].as_str().cmp(&b.0["name"].as_str()));
    found
}

/// One trigger's row, as `list` shows it; none when no trigger of that
/// name is there, or one that ended has been removed.
pub fn one(places: &Places, name: &str) -> Option<Value> {
    (valid_name(name).is_ok() && (places.job(name).exists() || places.last(name).exists()))
        .then(|| row(places, name))
}

fn row(places: &Places, name: &str) -> Value {
    let path = places.job(name);
    if !path.exists() {
        let mut row = read_json(&places.last(name))
            .filter(|r| r["name"] == name)
            .unwrap_or_else(|| json!({"name": name, "problem": "unreadable: result"}));
        row["ended"] = json!(true);
        return row;
    }
    match read_trigger(&path) {
        Ok((t, _)) => {
            let mut row = t.json(&state(places, &t));
            row["ended"] = json!(false);
            if t.at.is_some_and(|at| now() > at + SLACK) {
                row["missed"] = json!(true);
            }
            row
        }
        Err(problem) => json!({"name": name, "ended": false, "problem": problem}),
    }
}

/// Write a file whole beside its place, then rename it there.
fn replace(path: &Path, text: &str) -> Result<(), String> {
    replace_mode(path, text.as_bytes(), 0o644)
}

pub(crate) use agent_client::files::replace_mode;

/// A name is part of a launchd label and a file name.
fn valid_name(name: &str) -> Result<(), String> {
    let ok = !name.is_empty()
        && name.len() <= 128
        && !name.starts_with('.')
        && name
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'-' | b'_'));
    ok.then_some(())
        .ok_or_else(|| format!("invalid_name: {name}: letters, digits, '.', '-' and '_'"))
}

/// Write the trigger's plist and load it. A trigger of that name already
/// there is left as it is: the same one is returned, to say so, and another
/// is `trigger_exists`, naming the first field that differs.
pub fn install(
    places: &Places,
    app: &Path,
    trigger: &Trigger,
    when: &When,
    environment: &[(&str, String)],
    replace: bool,
    launchd: Loader,
) -> Result<Option<Trigger>, String> {
    valid_name(&trigger.name)?;
    let _lock = Lock::take(places)?;
    // macOS folders ignore case: `Build` would overwrite `build`'s files,
    // its plist or an ended one's last result.
    let taken = |dir: &Path, file: &str| {
        std::fs::read_dir(dir)
            .into_iter()
            .flatten()
            .flatten()
            .filter_map(|e| e.file_name().into_string().ok())
            .find(|f| f != file && f.eq_ignore_ascii_case(file))
    };
    let suffix = places.scheduler.suffix();
    let taken = taken(&places.jobs, &format!("{LABEL}{}{suffix}", trigger.name))
        .map(|f| f[LABEL.len()..f.len() - suffix.len()].to_owned())
        .or_else(|| {
            taken(&places.state, &format!("{}.json", trigger.name))
                .map(|f| f[..f.len() - ".json".len()].to_owned())
        });
    if let Some(other) = taken {
        return Err(format!(
            "name_taken: {}: trigger {other} differs only in case; pass --name",
            trigger.name
        ));
    }
    unfinished(places, &trigger.name, launchd)?;
    let path = places.job(&trigger.name);
    match std::fs::read_to_string(&path) {
        Ok(text) => {
            let (there, there_app) = read_job(&text).ok_or_else(|| {
                format!(
                    "trigger_exists: definition: {} is there but cannot be read; rm it first",
                    trigger.name
                )
            })?;
            return match there.differs(trigger) {
                None => {
                    // The same definition can watch another path: a
                    // repository made again at its path has another git
                    // folder. launchd is made to watch the one it is now,
                    // and to run it with the environment it is added with now.
                    if watched(&text) != when.watch || !runs_with(&text, environment) {
                        let asks = places.asks(&there.name);
                        let want = definition(
                            places.scheduler,
                            &there_app,
                            &there,
                            when,
                            &asks,
                            environment,
                        );
                        swap(
                            &path,
                            &format!("{LABEL}{}", there.name),
                            Some(&text),
                            &want,
                            launchd,
                        )?;
                    }
                    Ok(Some(there))
                }
                Some(_) if replace => {
                    // Another definition under the name: what its fires
                    // did stays on its row, its count of sends starts again,
                    // and an agent it started is the one it messages, while
                    // the store it started it in is the one it sends to.
                    let durable = state(places, &there);
                    let mut kept = Kept {
                        sent: 0,
                        ..Kept::of(&durable)
                    };
                    let same_start = there.store_id == trigger.store_id
                        && matches!((&there.target, &trigger.target),
                            (Target::Start { name: a, .. }, Target::Start { name: b, .. }) if a == b);
                    if !same_start {
                        kept.started_id = None;
                    }
                    if there.commit != trigger.commit {
                        kept.seen = Seen::default();
                    }
                    let asks = places.asks(&trigger.name);
                    queues(&asks)?;
                    let want = definition(places.scheduler, app, trigger, when, &asks, environment);
                    let label = format!("{LABEL}{}", trigger.name);
                    swap(&path, &label, Some(&text), &want, launchd)?;
                    // Without what it keeps the new one would start over:
                    // the old one goes back in.
                    if let Err(error) = write_row(places, trigger, &durable["last"], &kept) {
                        let _ = swap(&path, &label, Some(&want), &text, launchd);
                        return Err(error);
                    }
                    Ok(None)
                }
                Some(field) => Err(format!(
                    "trigger_exists: {field}: {} is a trigger with another {field}; pass --replace, rm it first, or pass another --name",
                    trigger.name
                )),
            };
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
        Err(e) => return Err(format!("{}: {e}", path.display())),
    }
    let asks = places.asks(&trigger.name);
    queues(&asks)?;
    swap(
        &path,
        &format!("{LABEL}{}", trigger.name),
        None,
        &definition(places.scheduler, app, trigger, when, &asks, environment),
        launchd,
    )?;
    Ok(None)
}

/// Put `text` in the plist at `path` and load it in place of `old`: when
/// launchd will not unload the old job nothing changes, and when the new
/// one cannot be written or loaded the old one is written back and loaded.
fn swap(
    path: &Path,
    label: &str,
    old: Option<&str>,
    text: &str,
    launchd: Loader,
) -> Result<(), String> {
    // Check the serialized bytes once, before touching the job or its files.
    if text.len() as u64 > MAX_RECORD {
        return Err(format!(
            "invalid_trigger: serialized plist exceeds {MAX_RECORD} bytes; narrow --cron or shorten the message"
        ));
    }
    unload(label, launchd)?;
    let loaded = replace(path, text).and_then(|()| launchd(Job::Load(path)));
    if let Err(error) = loaded {
        match old {
            Some(old) => {
                if replace(path, old).is_ok() {
                    let _ = launchd(Job::Load(path));
                }
            }
            None => {
                let _ = forget(path);
            }
        }
        return Err(error);
    }
    Ok(())
}

/// Held while a trigger's files or launchd's job change, so `add`, `rm`,
/// a fire ending its trigger and Sonny's refresh never interleave. The
/// lock goes with the process, also when a fire's own unload ends it.
struct Lock {
    _held: std::fs::File,
}

impl Lock {
    fn take(places: &Places) -> Result<Self, String> {
        std::fs::create_dir_all(&places.state)
            .map_err(|e| format!("{}: {e}", places.state.display()))?;
        let path = places.state.join(".lock");
        let file = std::fs::OpenOptions::new()
            .create(true)
            .truncate(false)
            .write(true)
            .open(&path)
            .map_err(|e| format!("{}: {e}", path.display()))?;
        use std::os::fd::AsRawFd;
        // SAFETY: flock on a descriptor this struct owns.
        if unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX) } != 0 {
            return Err(format!(
                "{}: {}",
                path.display(),
                std::io::Error::last_os_error()
            ));
        }
        Ok(Self { _held: file })
    }
}

/// Delete a file for good: gone from its folder once that folder is synced.
/// One already gone is fine.
fn forget(path: &Path) -> Result<(), String> {
    let gone = match std::fs::remove_file(path) {
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        gone => gone,
    };
    gone.and_then(|()| match path.parent() {
        Some(dir) => std::fs::File::open(dir).and_then(|d| d.sync_all()),
        None => Ok(()),
    })
    .map_err(|e| format!("{}: {e}", path.display()))
}

/// Remove a trigger, whatever is left of it: its plist, its last result,
/// and launchd's job, also one loaded without its plist.
pub fn remove(places: &Places, name: &str, launchd: Loader) -> Result<(), String> {
    valid_name(name)?;
    let _lock = Lock::take(places)?;
    match retire(places, name, false, launchd)? {
        true => Ok(()),
        false => Err(format!("trigger_not_found: {name}")),
    }
}

/// `fire NAME`: an ask, which launchd runs the trigger's job for. While a
/// fire of it runs launchd starts no other; it runs the job again once that
/// one is done, for the ask it left. The ask's name, which what comes of
/// it carries.
pub fn fire_now(places: &Places, name: &str) -> Result<String, String> {
    valid_name(name)?;
    let _lock = Lock::take(places)?;
    if !stored(&places.job(name)) {
        return Err(format!("trigger_not_found: {name}"));
    }
    ask(places, name, Ask::Fire)
}

/// What came of the fire for ask `file`, once launchd's run of the job says
/// within `wait`: the turn it sent and its handle, or why none went. Still
/// `pending` past that, which a busy Mac or a slow `--if` can be.
fn fired(places: &Places, name: &str, file: &str, wait: Duration) -> Value {
    let until = std::time::Instant::now() + wait;
    let came = |outcome: &Value| {
        let mut came = json!({"name": name, "fired": true});
        for key in ["outcome", "turn", "handle", "detail"] {
            if !outcome[key].is_null() {
                came[key] = outcome[key].clone();
            }
        }
        came
    };
    let receipt = places.receipt(name, file);
    let came = loop {
        let row = one(places, name).unwrap_or(Value::Null);
        if row["last"]["ask"] == file {
            break came(&row["last"]);
        }
        if row["checked"]["ask"] == file && row["checked"]["gate"] == "no" {
            break came(&json!({"outcome": "declined", "detail": row["checked"]["detail"]}));
        }
        // What came of it, also while its fire waits for the answer
        // `--reply-to` passes on, after it ended the trigger, row and all, or
        // once another fire's result is the row's.
        let received = std::fs::read(&receipt)
            .ok()
            .and_then(|bytes| serde_json::from_slice::<Value>(&bytes).ok());
        if let Some(received) = received.filter(|r| r["outcome"].is_string()) {
            break came(&received);
        }
        if std::time::Instant::now() > until {
            break json!({"name": name, "fired": true, "pending": true});
        }
        std::thread::sleep(Duration::from_millis(100));
    };
    let _ = std::fs::remove_file(&receipt);
    came
}

/// What an ask asks of the fire launchd runs for it.
#[derive(Debug, Clone, Copy, PartialEq)]
enum Ask {
    /// Send, whatever the time or path says (`fire NAME`).
    Fire,
    /// Look as launchd would, and send only what is news: `add` asks so for
    /// a commit made while launchd began to watch, which woke nothing.
    Wake,
}

impl Ask {
    /// How its file's name starts.
    fn prefix(self) -> &'static str {
        match self {
            Ask::Fire => "",
            Ask::Wake => "wake.",
        }
    }
    /// Whether it asks for a message whatever the time or path says.
    fn sends(self) -> bool {
        matches!(self, Ask::Fire)
    }
    fn of(file: &str) -> Self {
        match file.starts_with(Ask::Wake.prefix()) {
            true => Ask::Wake,
            false => Ask::Fire,
        }
    }
}

/// Ask for a fire: a file in the trigger's queue, written whole beside it
/// first, so launchd never starts the job for half of one. Called under the
/// lock, with the plist there.
fn ask(places: &Places, name: &str, kind: Ask) -> Result<String, String> {
    static ASKS: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let file = format!(
        "{}{}.{}.{}",
        kind.prefix(),
        now(),
        std::process::id(),
        ASKS.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
    );
    let asks = places.asks(name);
    std::fs::create_dir_all(&asks).map_err(|e| format!("{}: {e}", asks.display()))?;
    let beside = places.state.join(format!(".ask.{name}.{file}"));
    replace(&beside, "")?;
    std::fs::rename(&beside, asks.join(&file))
        .and_then(|()| std::fs::File::open(&asks)?.sync_all())
        .map_err(|e| {
            let _ = std::fs::remove_file(&beside);
            format!("{}: {e}", asks.display())
        })?;
    Ok(file)
}

/// An ask a fire took: out of the queue launchd watches, in the trigger's
/// `NAME.taking` folder until the fire is done with it.
#[derive(Debug)]
struct Asked {
    kind: Ask,
    path: PathBuf,
    receipt: PathBuf,
}

impl Asked {
    /// Its file's name, which no other ask has.
    fn name(&self) -> String {
        self.path
            .file_name()
            .map(|f| f.to_string_lossy().into_owned())
            .unwrap_or_default()
    }
}

/// The ask this fire is for: one a fire took and was cut short before it
/// was done with, else the oldest in the queue, moved out of it. Each ask is
/// one fire, and launchd runs the job again while any is left. One that
/// cannot be moved out would have launchd run the job for ever: an error,
/// which stops the trigger.
fn take_ask(places: &Places, trigger: &Trigger) -> Result<Option<Asked>, String> {
    let name = &trigger.name;
    // A queue that cannot be read is not an empty one: launchd would run
    // the job for it again and again.
    let oldest = |dir: &Path| -> Result<Option<(String, PathBuf)>, String> {
        let entries = match std::fs::read_dir(dir) {
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            entries => entries,
        };
        let stuck = |e: std::io::Error| format!("asks_stuck: {}: {e}", dir.display());
        let mut oldest: Option<(String, PathBuf)> = None;
        for entry in entries.map_err(stuck)? {
            let entry = entry.map_err(stuck)?;
            let found = (
                entry.file_name().to_string_lossy().into_owned(),
                entry.path(),
            );
            if oldest.as_ref().is_none_or(|first| found < *first) {
                oldest = Some(found);
            }
        }
        Ok(oldest)
    };
    let taking = places.taking(name);
    if let Some((file, path)) = oldest(&taking)? {
        return Ok(Some(Asked {
            kind: Ask::of(&file),
            receipt: places.receipt(name, &file),
            path,
        }));
    }
    let asks = places.asks(name);
    if let Some((file, path)) = oldest(&asks)? {
        let kind = Ask::of(&file);
        let to = taking.join(&file);
        std::fs::create_dir_all(&taking)
            .and_then(|()| std::fs::rename(&path, &to))
            .and_then(|()| std::fs::File::open(&asks)?.sync_all())
            .and_then(|()| std::fs::File::open(&taking)?.sync_all())
            .map_err(|e| format!("asks_stuck: {}: {e}", path.display()))?;
        return Ok(Some(Asked {
            kind,
            receipt: places.receipt(name, &file),
            path: to,
        }));
    }
    Ok(None)
}

/// The fire is done with its ask. One that cannot go would have launchd
/// run the job for it for ever; one already gone went with the trigger this
/// fire ended (systemd lets the fire finish).
fn done_with(asked: &Asked) -> Result<(), String> {
    match std::fs::symlink_metadata(&asked.path) {
        Ok(meta) if meta.is_dir() => std::fs::remove_dir_all(&asked.path),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
        _ => std::fs::remove_file(&asked.path),
    }
    .map_err(|e| format!("asks_stuck: {}: {e}", asked.path.display()))
}

/// What a fire leaves for the next: messages sent, the agent it started,
/// and the commit it saw.
#[derive(Debug, Default, Clone, PartialEq)]
struct Kept {
    sent: u64,
    started_id: Option<i64>,
    seen: Seen,
    /// What its `--if` said last, also when it said no and nothing went.
    checked: Value,
}

impl Kept {
    fn of(state: &Value) -> Self {
        Self {
            sent: state["sent"].as_u64().unwrap_or(0),
            started_id: state["started_id"].as_i64(),
            seen: Seen {
                head: state["head"].as_str().map(str::to_owned),
                at: state["seen_at"].as_i64(),
            },
            checked: state["checked"].clone(),
        }
    }
}

/// What a fire did goes on disk, and a trigger that is over ends: both
/// under the lock, and only while the plist is still this trigger's. One
/// replaced or removed while its message went out is left as it now is; a
/// job left loaded after its plist went (an end cut short) is unloaded.
/// Whether that is all done: false when what it did is not on disk, or the
/// trigger it ends is still there.
fn settle(
    places: &Places,
    trigger: &Trigger,
    outcome: &Value,
    kept: &Kept,
    launchd: Loader,
) -> bool {
    let log = |error: String| {
        eprintln!("{}", error_json(&error));
        false
    };
    received(places, trigger, outcome);
    let _lock = match Lock::take(places) {
        Ok(lock) => lock,
        Err(error) => return log(error),
    };
    let path = places.job(&trigger.name);
    let text = match std::fs::read_to_string(&path) {
        Ok(text) => text,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            // Under the lock no `add` is between writing and loading one.
            return retire(places, &trigger.name, true, launchd).map_or_else(log, |_| true);
        }
        Err(e) => return log(format!("{}: {e}", path.display())),
    };
    if read_job(&text).is_none_or(|(now, _)| now != *trigger) {
        return true;
    }
    let sent = outcome["outcome"] == "sent";
    let kept = Kept {
        sent: kept.sent + u64::from(sent),
        ..kept.clone()
    };
    let recorded = record_last(places, trigger, outcome, &kept);
    if let Err(error) = &recorded {
        log(error.clone());
    }
    let over = trigger.at.is_some()
        || outcome["outcome"] == "gone"
        || outcome["gone"] == true
        || outcome["reply"]["gone"] == true
        || trigger.runs.is_some_and(|runs| kept.sent >= runs);
    // One that did not deliver ends only once why is on disk; else its plist
    // stays, listed.
    // An answer that did not get through keeps its row, saying so.
    let kept_row = !sent || outcome["reply"]["outcome"] == "failed";
    if over && (sent || recorded.is_ok()) {
        return retire(places, &trigger.name, kept_row, launchd).map_or_else(log, |_| true);
    }
    recorded.is_ok()
}

/// What came of a `fire NAME`'s ask, in its receipt for that `fire`: the
/// trigger's row says only its latest, which another fire can write first.
fn received(places: &Places, trigger: &Trigger, outcome: &Value) {
    if let Some(file) = outcome["ask"].as_str().filter(|f| Ask::of(f) == Ask::Fire) {
        let _ = std::fs::write(places.receipt(&trigger.name, file), outcome.to_string());
    }
}

/// The one way a trigger goes, by `rm` or by its own end, under the lock:
/// its files first, then launchd's job, whose unload ends a fire that ends
/// its own trigger. `keep` leaves its last result, so an end nobody asked
/// for still shows, and why. Its files are set aside by rename, whatever
/// their size or contents, and deleted once launchd has let the job go; a
/// job launchd will not unload gets them back, so it stays listed for `rm`:
/// with its plist gone it would load again at the next login. What an
/// earlier retire of the name set aside is finished with the rest: a fire's
/// own unload ends it before it deletes them. Whether any of it was there.
/// A folder that ignores case finds `build`'s files for `Build`, whose
/// label launchd does not have: only the name as stored is that trigger,
/// and launchd's labels keep their case.
fn retire(places: &Places, name: &str, keep: bool, launchd: Loader) -> Result<bool, String> {
    let mut aside: Vec<(PathBuf, PathBuf)> = Vec::new();
    // A file comes back only to a place nothing has taken since.
    let back = |aside: &[(PathBuf, PathBuf)]| {
        for (path, by) in aside {
            if !stored(path)
                && let Err(error) = renamed(by, path)
            {
                eprintln!("{}", error_json(&error));
            }
        }
    };
    // Its asks go with it, and come back with the rest.
    let [plist, last, asks, taking] = its_files(places, name);
    for (path, go) in [
        (&plist, true),
        (&last, !keep),
        (&asks, true),
        (&taking, true),
    ] {
        let by = tomb(path);
        if !(go && stored(path)) {
            if stored(&by) {
                aside.push((path.clone(), by));
            }
            continue;
        }
        if let Err(e) = std::fs::rename(path, &by) {
            back(&aside);
            return Err(format!("{}: {e}", path.display()));
        }
        aside.push((path.clone(), by));
        if let Err(error) = synced(path) {
            back(&aside);
            return Err(error);
        }
    }
    let here = !aside.is_empty();
    let unloaded = match launchd(Job::Unload(&format!("{LABEL}{name}"))) {
        Ok(()) => Ok(true),
        Err(error) if error.starts_with(NOT_LOADED) => Ok(here),
        Err(error) => {
            back(&aside);
            return Err(error);
        }
    };
    for (_, by) in &aside {
        let gone = match std::fs::symlink_metadata(by) {
            Ok(meta) if meta.is_dir() => std::fs::remove_dir_all(by)
                .map_err(|e| format!("{}: {e}", by.display()))
                .and_then(|()| synced(by)),
            _ => forget(by),
        };
        if let Err(error) = gone {
            eprintln!("{}", error_json(&error));
        }
    }
    unloaded
}

/// What a trigger has on disk: its plist, last result, asks and the one
/// being taken.
fn its_files(places: &Places, name: &str) -> [PathBuf; 4] {
    [
        places.job(name),
        places.last(name),
        places.asks(name),
        places.taking(name),
    ]
}

/// Whether a file is there by this name as stored: a folder that ignores
/// case finds `build`'s for `Build`.
fn stored(file: &Path) -> bool {
    let want = file.file_name();
    file.parent()
        .and_then(|dir| std::fs::read_dir(dir).ok())
        .into_iter()
        .flatten()
        .flatten()
        .any(|e| Some(e.file_name().as_os_str()) == want)
}

/// Where a retiring file waits for launchd to let its job go.
fn tomb(path: &Path) -> PathBuf {
    let file = path.file_name().unwrap_or_default().to_string_lossy();
    path.with_file_name(format!(".{file}.retiring"))
}

/// At Sonny's start, under the lock, finish what was cut short: a retire
/// a fire's own unload ended before it deleted what it set aside, and a
/// write that died before its rename, whose temporary nothing else would
/// remove. Every write here is under the lock, so no other is under way. A
/// name whose plist is back is a trigger added since, and keeps what it
/// has.
pub fn finish(places: &Places, launchd: Loader) {
    let Ok(_lock) = Lock::take(places) else {
        return;
    };
    let log = |error: String| eprintln!("{}", error_json(&error));
    let dots = |dir: &Path| {
        std::fs::read_dir(dir)
            .into_iter()
            .flatten()
            .flatten()
            .filter_map(|e| e.file_name().into_string().ok())
            .filter_map(|f| f.strip_prefix('.').map(str::to_owned))
            .collect::<Vec<_>>()
    };
    let mut cut = Vec::new();
    let ours = dots(&places.jobs)
        .into_iter()
        .filter(|f| f.starts_with(LABEL))
        .map(|f| (places.jobs.join(format!(".{f}")), f));
    let state = dots(&places.state)
        .into_iter()
        .filter(|f| f != "lock")
        .map(|f| (places.state.join(format!(".{f}")), f));
    for (path, file) in ours.chain(state) {
        // `.NAME.EXT.retiring`, or `.LABELNAME.plist.retiring`.
        match file
            .strip_suffix(".retiring")
            .and_then(|f| f.rsplit_once('.'))
        {
            Some((name, _)) => cut.push(name.strip_prefix(LABEL).unwrap_or(name).to_owned()),
            None => {
                if let Err(error) = forget(&path) {
                    log(error);
                }
            }
        }
    }
    cut.sort();
    cut.dedup();
    for name in cut.iter().filter(|name| valid_name(name).is_ok()) {
        if let Err(error) = unfinished(places, name, launchd) {
            log(error);
        }
    }
}

/// Under the lock, finish a retire of `name` that was cut short, before
/// anything else is done with the name.
fn unfinished(places: &Places, name: &str, launchd: Loader) -> Result<(), String> {
    let files = its_files(places, name);
    if files[0].exists() || !files.iter().any(|path| tomb(path).exists()) {
        return Ok(());
    }
    retire(places, name, true, launchd).map(|_| ())
}

/// Rename a file, durably: its folder is synced after.
fn renamed(from: &Path, to: &Path) -> Result<(), String> {
    std::fs::rename(from, to)
        .map_err(|e| format!("{}: {e}", from.display()))
        .and_then(|()| synced(to))
}

/// Sync the folder a file was renamed in.
fn synced(path: &Path) -> Result<(), String> {
    match path.parent() {
        Some(dir) => std::fs::File::open(dir).and_then(|d| d.sync_all()),
        None => Ok(()),
    }
    .map_err(|e| format!("{}: {e}", path.display()))
}

/// The app moves when it is updated, so it writes its path into every
/// trigger again when it starts from somewhere else. One that fails keeps
/// the old path, so the next start tries it again.
pub fn refresh(places: &Places, app: &Path, launchd: Loader) {
    for (trigger, _) in read_all(places).filter_map(|(_, r)| r.ok()) {
        let Ok(_lock) = Lock::take(places) else {
            return;
        };
        // Read again under the lock: an `rm`, `add` or fire may have come first.
        let path = places.job(&trigger.name);
        let Ok(text) = std::fs::read_to_string(&path) else {
            continue;
        };
        let Some(was) = program(&text).and_then(|args| args.into_iter().next()) else {
            continue;
        };
        let was = PathBuf::from(was);
        if was == app {
            continue;
        }
        let moved = with_app(&text, &was, app);
        let _ = swap(
            &path,
            &format!("{LABEL}{}", trigger.name),
            Some(&text),
            &moved,
            launchd,
        );
    }
}

/// A trigger made before triggers named their store by its lineage names
/// it by its file's identity, which a replaced file does not keep: each
/// that names `identity`, the daemon's now, names `lineage` from here on.
pub fn restamp(places: &Places, identity: &str, lineage: &str, launchd: Loader) {
    for (trigger, _) in read_all(places).filter_map(|(_, r)| r.ok()) {
        if trigger.store_id != identity {
            continue;
        }
        let Ok(_lock) = Lock::take(places) else {
            return;
        };
        // Read again under the lock: an `rm`, `add` or fire may have come first.
        let path = places.job(&trigger.name);
        let Ok(text) = std::fs::read_to_string(&path) else {
            continue;
        };
        let stamped = match is_plist(&text) {
            true => text.replacen(
                &format!("<string>{}</string>", escape(identity)),
                &format!("<string>{}</string>", escape(lineage)),
                1,
            ),
            false => {
                let flag = quoted("--store-id", true);
                text.replacen(
                    &format!("{flag} {}", quoted(identity, true)),
                    &format!("{flag} {}", quoted(lineage, true)),
                    1,
                )
            }
        };
        if stamped == text {
            continue;
        }
        let _ = swap(
            &path,
            &format!("{LABEL}{}", trigger.name),
            Some(&text),
            &stamped,
            launchd,
        );
    }
}

/// Once Sonny knows its daemon: [`restamp`] the triggers made on its store.
pub fn restamp_at_start(identity: Option<&str>, lineage: Option<&str>) {
    let (Ok(places), Some(identity), Some(lineage)) = (Places::home(), identity, lineage) else {
        return;
    };
    let (identity, lineage) = (identity.to_owned(), lineage.to_owned());
    std::thread::spawn(move || restamp(&places, &identity, &lineage, &schedule));
}

/// What `trigger add` was asked.
#[derive(Debug, PartialEq)]
struct Add {
    name: Option<String>,
    when: When,
    commit: Option<PathBuf>,
    bot: Option<String>,
    start: Option<(String, String, Option<String>)>,
    reply_to: Option<String>,
    gate: Option<String>,
    runs: Option<u64>,
    turn_budget_tokens: Option<u64>,
    replace: bool,
    message: String,
}

fn parse_add(args: &[String], now: i64) -> Result<Add, String> {
    let bad = |what: String| format!("invalid_trigger: {what}\n{USAGE}");
    let mut v = std::collections::HashMap::<String, String>::new();
    let (mut when, mut commit, mut replace) = (None, None, false);
    let mut iter = args.iter();
    let message = loop {
        let Some(flag) = iter.next() else {
            return Err(bad("a message goes after --".into()));
        };
        if flag == "--" {
            break iter.cloned().collect::<Vec<_>>().join(" ");
        }
        if flag == "--replace" {
            replace = true;
            continue;
        }
        let value = iter
            .next()
            .ok_or_else(|| bad(format!("{flag} needs a value")))?;
        let asked = match flag.as_str() {
            "--every" => every(value, now)?,
            "--in" => after(value, now)?,
            "--at" => at(value, now)?,
            "--cron" => cron(value)?,
            "--file" => file(value)?,
            "--commit" => {
                let (when, repo) = self::commit(value)?;
                commit = Some(repo);
                when
            }
            "--name"
            | "--bot"
            | "--start"
            | "--model"
            | "--effort"
            | "--reply-to"
            | "--if"
            | "--runs"
            | "--turn-budget-tokens" => {
                if v.insert(flag.clone(), value.clone()).is_some() {
                    return Err(bad(format!("{flag} once")));
                }
                continue;
            }
            other => return Err(bad(other.to_owned())),
        };
        if when.replace(asked).is_some() {
            return Err(bad(
                "one of --every, --in, --at, --cron, --file or --commit".into(),
            ));
        }
    };
    let mut take = |flag: &str| v.remove(flag);
    let (bot, start, model, effort) = (
        take("--bot"),
        take("--start"),
        take("--model"),
        take("--effort"),
    );
    let start = match (start, model, effort) {
        (Some(_), None, _) => return Err(bad("--start needs --model".into())),
        (Some(name), Some(model), effort) => Some((name, model, effort)),
        (None, None, None) => None,
        (None, ..) => return Err(bad("--model and --effort go with --start".into())),
    };
    if bot.is_some() && start.is_some() {
        return Err(bad("one of --bot or --start".into()));
    }
    let mut counted = |flag: &str| {
        take(flag)
            // As high as its plist reads back.
            .map(|n| n.parse::<i64>().ok().filter(|n| *n > 0).map(|n| n as u64))
            .map(|n| n.ok_or_else(|| bad(format!("{flag} takes a count from 1 to {}", i64::MAX))))
            .transpose()
    };
    let runs = counted("--runs")?;
    let turn_budget_tokens = counted("--turn-budget-tokens")?;
    if message.trim().is_empty() {
        return Err(bad("a message goes after --".into()));
    }
    if message.len() > MAX_MESSAGE {
        return Err(format!(
            "invalid_trigger: a message is at most {MAX_MESSAGE} bytes"
        ));
    }
    // XML 1.0 has no place for other control characters.
    let control = |s: &str| {
        s.chars()
            .any(|c| c.is_control() && !matches!(c, '\n' | '\t' | '\r'))
    };
    let gate = take("--if");
    if control(&message) || gate.as_deref().is_some_and(control) {
        return Err("invalid_trigger: the message or --if has control characters".into());
    }
    Ok(Add {
        name: take("--name"),
        when: when.unwrap_or(When {
            text: "fire".into(),
            entries: Vec::new(),
            not_before: None,
            at: None,
            watch: None,
        }),
        commit,
        bot,
        start,
        reply_to: take("--reply-to"),
        gate,
        runs,
        turn_budget_tokens,
        replace,
        message,
    })
}

/// What a trigger's job starts with besides launchd's own: the shell whose
/// login environment starts a daemon with your keys, as Sonny starts one.
fn environment(scheduler: Scheduler) -> Vec<(&'static str, String)> {
    // PATH too: an `--if` command runs with the adder's, as typed there.
    let mut keys = vec!["HOME", "SHELL", "PATH"];
    // Where systemd's units and timer stamps are, which the fire finds as
    // `add` did.
    if scheduler == Scheduler::Systemd {
        keys.extend(["XDG_CONFIG_HOME", "XDG_DATA_HOME"]);
    }
    keys.into_iter()
        .filter_map(|key| std::env::var(key).ok().map(|v| (key, v)))
        .collect()
}

pub(crate) fn runtime() -> Result<tokio::runtime::Runtime, String> {
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .map_err(|e| e.to_string())
}

/// A daemon's error as this module's: `CODE: detail`, its code kept for
/// software to branch on.
fn coded(error: agent_client::Error) -> String {
    format!("{}: {}", error.code, error.detail.unwrap_or_default())
}

/// One error shape, `{"error": CODE, "detail": ...}`, from the
/// `CODE: detail` this module's errors are; `trigger_exists` also names the
/// `field` that differs.
pub(crate) fn error_json(message: &str) -> Value {
    let (code, detail) = match message.split_once(": ") {
        Some((code, detail))
            if !code.is_empty() && code.bytes().all(|b| b.is_ascii_lowercase() || b == b'_') =>
        {
            (code, detail)
        }
        _ => ("trigger_failed", message),
    };
    match detail.split_once(": ") {
        Some((field, detail)) if code == "trigger_exists" => {
            json!({"error": code, "field": field, "detail": detail})
        }
        _ => json!({"error": code, "detail": detail}),
    }
}

/// At Sonny's start, off its way: finish what a fire cut short, and point
/// every trigger at this Sonny when it moved.
pub fn at_start() {
    let (Ok(places), Some(sonny)) = (Places::home(), crate::skills::sonny_path()) else {
        return;
    };
    std::thread::spawn(move || {
        finish(&places, &schedule);
        refresh(&places, &sonny, &schedule);
    });
}

/// `sonny trigger add|ls|show|fire|rm`, as `~/.agent/trigger` runs it.
pub fn cli(args: &[String]) -> i32 {
    let done = match Places::home().and_then(|places| match args.split_first() {
        Some((verb, rest)) if verb == "ls" => match rest {
            [] => Ok(list(&places, None)),
            [flag, name] if flag == "--after" => Ok(list(&places, Some(name))),
            _ => Err(format!("usage: {USAGE}")),
        },
        Some((verb, [name])) if verb == "rm" => {
            remove(&places, name, &schedule).map(|()| json!({"removed": name}))
        }
        Some((verb, [name])) if verb == "show" => Ok(one(&places, name).unwrap_or(Value::Null)),
        Some((verb, [name])) if verb == "fire" => {
            fire_now(&places, name).map(|file| fired(&places, name, &file, FIRE_WAIT))
        }
        Some((verb, rest)) if verb == "add" => add(&places, rest),
        _ => Err(format!("usage: {USAGE}")),
    }) {
        Ok(value) => {
            println!("{value}");
            return 0;
        }
        Err(error) => error,
    };
    eprintln!("{}", error_json(&done));
    1
}

/// A `triggers` call: `add` one, or `remove`, `fire` or `show` one by
/// name; with none, every trigger. Run as `add` and the rest run from an
/// agent's shell, for the calling agent.
pub fn tool(arguments: &Value) -> Result<Value, String> {
    let places = Places::home()?;
    let named = |key: &str| arguments[key].as_str().filter(|name| !name.is_empty());
    if let Some(asked) = arguments.get("add") {
        return add(&places, &add_args(asked));
    }
    if let Some(name) = named("remove") {
        return remove(&places, name, &schedule).map(|()| json!({"removed": name}));
    }
    if let Some(name) = named("fire") {
        return fire_now(&places, name).map(|file| fired(&places, name, &file, FIRE_WAIT));
    }
    if let Some(name) = named("show") {
        return one(&places, name).ok_or_else(|| format!("trigger_not_found: {name}"));
    }
    Ok(list(&places, None))
}

/// The `add` arguments a `triggers` call's `add` names.
fn add_args(asked: &Value) -> Vec<String> {
    let mut args = Vec::new();
    for (key, flag) in [
        ("name", "--name"),
        ("every", "--every"),
        ("in", "--in"),
        ("at", "--at"),
        ("cron", "--cron"),
        ("file", "--file"),
        ("commit", "--commit"),
        ("bot", "--bot"),
        ("reply_to", "--reply-to"),
        ("if", "--if"),
    ] {
        if let Some(value) = asked[key].as_str() {
            args.extend([flag.to_owned(), value.to_owned()]);
        }
    }
    let start = &asked["start"];
    for (key, flag) in [
        ("name", "--start"),
        ("model", "--model"),
        ("effort", "--effort"),
    ] {
        if let Some(value) = start[key].as_str() {
            args.extend([flag.to_owned(), value.to_owned()]);
        }
    }
    for (key, flag) in [
        ("runs", "--runs"),
        ("turn_budget_tokens", "--turn-budget-tokens"),
    ] {
        if let Some(count) = asked[key].as_u64() {
            args.extend([flag.to_owned(), count.to_string()]);
        }
    }
    if asked["replace"] == true {
        args.push("--replace".to_owned());
    }
    let message = asked["message"].as_str().unwrap_or_default();
    args.extend(["--".to_owned(), message.to_owned()]);
    args
}

/// A trigger that watched the triggers' own folder would fire itself: each
/// fire writes its result there; so would one watching its daemon's store,
/// which each message it sends writes, or that store's `-wal` and `-shm`.
/// Folders on a Mac ignore case, and a link is followed as far as the path
/// exists.
fn watches_itself(places: &Places, store: Option<&Path>, watch: &Path) -> Result<(), String> {
    let real = |path: &Path| {
        let mut at = path.to_path_buf();
        let mut rest = Vec::new();
        loop {
            if let Ok(found) = at.canonicalize() {
                return rest
                    .iter()
                    .rev()
                    .fold(found, |p: PathBuf, part: &std::ffi::OsString| p.join(part));
            }
            match (at.file_name().map(|f| f.to_owned()), at.parent()) {
                (Some(part), Some(up)) => {
                    rest.push(part);
                    at = up.to_path_buf();
                }
                _ => return path.to_path_buf(),
            }
        }
    };
    let lower = |path: PathBuf| path.to_string_lossy().to_lowercase();
    let (real_watch, state) = (lower(real(watch)), lower(real(&places.state)));
    if real_watch == state || real_watch.starts_with(&format!("{state}/")) {
        return Err(format!(
            "invalid_file: {}: the triggers' own folder changes on every fire; watch another path",
            watch.display()
        ));
    }
    if let Some(store) = store.map(|s| lower(real(s)))
        && (real_watch == store || real_watch.starts_with(&format!("{store}-")))
    {
        return Err(format!(
            "invalid_file: {}: its daemon's store changes with every message; watch another path",
            watch.display()
        ));
    }
    // A hard link is the store's file by another name: the same file.
    use std::os::unix::fs::MetadataExt;
    let same = |other: &Path| match (std::fs::metadata(watch), std::fs::metadata(other)) {
        (Ok(a), Ok(b)) => (a.dev(), a.ino()) == (b.dev(), b.ino()),
        _ => false,
    };
    if let Some(store) = store {
        let name = store.as_os_str().to_owned();
        let with = |suffix: &str| {
            let mut name = name.clone();
            name.push(suffix);
            PathBuf::from(name)
        };
        if [with(""), with("-wal"), with("-shm")]
            .iter()
            .any(|f| same(f))
        {
            return Err(format!(
                "invalid_file: {}: it is its daemon's store by another name, which changes with every message; watch another path",
                watch.display()
            ));
        }
    }
    Ok(())
}

/// An agent's id by its name, now.
async fn bot_id(client: &Client, name: &str) -> Result<i64, String> {
    let record = client
        .request("inspect", json!({"bot": name}))
        .await
        .map_err(coded)?;
    record["bot_id"]
        .as_i64()
        .ok_or_else(|| "the daemon named no bot id".into())
}

fn add(places: &Places, args: &[String]) -> Result<Value, String> {
    let asked = parse_add(args, now())?;
    let env = |key: &str| std::env::var(key).ok().filter(|v| !v.is_empty());
    // The shell's own agent, when it is an agent's shell.
    let shell = match (env("AGENT_BOT"), env("AGENT_BOT_ID")) {
        (Some(bot), Some(id)) => Some((
            bot,
            id.parse::<i64>()
                .map_err(|_| "invalid_trigger: AGENT_BOT_ID is not an id")?,
        )),
        _ => None,
    };
    let daemon = Daemon::current()?;
    if let Some(watch) = &asked.when.watch {
        // A watched file could be the store, which every send writes: one
        // whose store is not known cannot be told apart from it.
        if asked.commit.is_none() && daemon.store.is_none() {
            return Err(
                "invalid_file: the daemon's store is not known (AGENT_SOCKET without AGENT_STORE), so --file cannot be checked against it"
                    .into(),
            );
        }
        watches_itself(places, daemon.store.as_deref(), watch)?;
    }
    let socket = daemon.socket()?;
    let name = asked
        .name
        .clone()
        .or_else(|| asked.start.as_ref().map(|s| s.0.clone()))
        .or_else(|| asked.bot.clone())
        .or_else(|| shell.as_ref().map(|s| s.0.clone()))
        .ok_or("invalid_trigger: --bot NAME or --start NAME, or run it from an agent's shell")?;
    let (target, reply_to, store_id) = runtime()?.block_on(async {
        let client = connect(&socket, &daemon).await?;
        let resolved = async {
            let target = match (&asked.bot, &asked.start, &shell) {
                (Some(bot), ..) => Target::Bot {
                    name: bot.clone(),
                    id: bot_id(&client, bot).await?,
                },
                (None, Some((start, model, effort)), _) => {
                    // Only a name no agent has: the fire makes it. Once it
                    // has, the same `add` again is that trigger.
                    if !places.job(&name).exists() {
                        match bot_id(&client, start).await {
                            Ok(_) => {
                                return Err(format!(
                                    "bot_exists: {start} is an agent already; message it with --bot {start}"
                                ));
                            }
                            Err(error) if error.starts_with("bot_not_found") => {}
                            Err(error) => return Err(error),
                        }
                    }
                    // The agent it is shown under must still be the shell's.
                    if let Some((bot, id)) = &shell
                        && bot_id(&client, bot).await? != *id
                    {
                        return Err(
                            "bot_not_found: the shell's bot identity no longer exists".into()
                        );
                    }
                    Target::Start {
                        name: start.clone(),
                        model: model.clone(),
                        effort: effort.clone(),
                        by: shell.clone(),
                    }
                }
                (None, None, Some((bot, id))) => {
                    // The trigger is pinned to this identity, which must still exist.
                    if bot_id(&client, bot).await? != *id {
                        return Err(
                            "bot_not_found: the shell's bot identity no longer exists".into()
                        );
                    }
                    Target::Bot {
                        name: bot.clone(),
                        id: *id,
                    }
                }
                (None, None, None) => {
                    return Err("invalid_trigger: --bot NAME or --start NAME, or run it from an agent's shell".into());
                }
            };
            let reply_to = match &asked.reply_to {
                Some(bot) => Some((bot.clone(), bot_id(&client, bot).await?)),
                None => None,
            };
            let store_id = client
                .store()
                .map(str::to_owned)
                .ok_or_else(|| "the daemon announced no store identity".to_owned())?;
            Ok::<_, String>((target, reply_to, store_id))
        }
        .await;
        client.close().await;
        resolved
    })?;
    let needs_dir = asked.gate.is_some() || asked.start.is_some();
    let trigger = Trigger {
        generation: format!(
            "{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map_err(|e| e.to_string())?
                .as_nanos()
        ),
        not_before: asked.when.not_before,
        name,
        target,
        when: asked.when.text.clone(),
        at: asked.when.at,
        file: asked
            .commit
            .is_none()
            .then(|| asked.when.watch.clone())
            .flatten(),
        commit: asked.commit,
        dir: needs_dir
            .then(std::env::current_dir)
            .transpose()
            .map_err(|e| e.to_string())?,
        reply_to,
        gate: asked.gate,
        runs: asked.runs,
        turn_budget_tokens: asked.turn_budget_tokens,
        daemon,
        store_id,
        message: asked.message,
    };
    // Where HEAD is now is seen, a repository with no commit yet too: only
    // a commit after it fires. It is read before launchd watches; one made
    // before launchd did is asked for.
    let seen = trigger
        .commit
        .as_deref()
        .map(|repo| moved(repo, &Seen::default()).1);
    let watching = std::fs::read_to_string(places.job(&trigger.name))
        .ok()
        .and_then(|text| watched(&text));
    let keeps_seen = asked.replace && keeps_seen(places, &trigger);
    let app = crate::skills::sonny_path().ok_or("no path to this Sonny")?;
    let environment = environment(places.scheduler);
    match install(
        places,
        &app,
        &trigger,
        &asked.when,
        &environment,
        asked.replace,
        &schedule,
    )? {
        Some(there) => {
            // The same repository made again has another git folder, and
            // launchd watches that one now.
            if let Some(seen) = seen.filter(|_| watching != asked.when.watch) {
                baseline(places, &there, &seen, false)?;
            }
            let mut row = there.json(&state(places, &there));
            row["duplicate"] = json!(true);
            Ok(row)
        }
        None => {
            // A trigger without its baseline would take the commit there for
            // news, so it goes.
            if let Some(seen) = seen.filter(|_| !keeps_seen)
                && let Err(error) = baseline(places, &trigger, &seen, !asked.replace)
            {
                let _ = remove(places, &trigger.name, &schedule);
                return Err(error);
            }
            Ok(trigger.json(&state(places, &trigger)))
        }
    }
}

/// Whether the trigger `replace` puts in a name's place keeps where the one
/// there last looked: one on the same repository, to which a commit since
/// is still news.
fn keeps_seen(places: &Places, trigger: &Trigger) -> bool {
    std::fs::read_to_string(places.job(&trigger.name))
        .ok()
        .and_then(|text| read_job(&text))
        .is_some_and(|(there, _)| {
            there.commit == trigger.commit && !state(places, &there)["seen_at"].is_null()
        })
}

/// What a fire did, kept for `ls` to show and the next fire to read.
/// It is the trigger's whole row, which Settings still shows once the
/// trigger has ended on its own.
fn record_last(
    places: &Places,
    trigger: &Trigger,
    outcome: &Value,
    kept: &Kept,
) -> Result<(), String> {
    let mut outcome = outcome.clone();
    if !outcome.is_null() {
        outcome["fired_ms"] = json!(now() * 1000);
    }
    write_row(places, trigger, &outcome, kept)
}

fn write_row(places: &Places, trigger: &Trigger, last: &Value, kept: &Kept) -> Result<(), String> {
    let mut row = trigger.json(&json!({"last": last, "sent": kept.sent,
        "started_id": kept.started_id}));
    row["started_id"] = json!(kept.started_id);
    row["head"] = json!(kept.seen.head);
    row["seen_at"] = json!(kept.seen.at);
    row["checked"] = kept.checked.clone();
    replace(&places.last(&trigger.name), &row.to_string())
}

/// Where a commit trigger's repository is now becomes what its next fire
/// compares with, its last result kept: at `add`, unless a fire wrote one
/// first (`fresh`), whenever `add` finds the repository made again, whose
/// log has nothing to do with the last one, and after a fire that found no
/// news or whose `--if` said no to it. A commit made while
/// launchd began to watch woke nothing, so it is asked for. Under the lock,
/// and only while the plist is this trigger's.
fn baseline(places: &Places, trigger: &Trigger, seen: &Seen, fresh: bool) -> Result<(), String> {
    let _lock = Lock::take(places)?;
    let durable = state(places, trigger);
    if (fresh && !durable.is_null()) || !ours(places, trigger) {
        return Ok(());
    }
    let kept = Kept {
        seen: seen.clone(),
        ..Kept::of(&durable)
    };
    write_row(places, trigger, &durable["last"], &kept)?;
    let repo = trigger.commit.as_deref().unwrap_or(Path::new(""));
    match moved(repo, seen).0 {
        true => ask(places, &trigger.name, Ask::Wake).map(drop),
        false => Ok(()),
    }
}

/// `YYYY-MM-DD HH:MM`, local.
fn stamp(epoch: i64) -> String {
    let t = local(epoch);
    format!(
        "{}-{:02}-{:02} {:02}:{:02}",
        t.year, t.month, t.day, t.hour, t.minute
    )
}

/// `sh -c CMD` in the trigger's folder, on its daemon (`AGENT_SOCKET`,
/// `AGENT_STORE`), given `GATE_TIMEOUT`: whether it said go, and why not when
/// it said no; why it said nothing when it could not run or ran past its
/// time, which is the fire failing, not a no.
fn gate(command: &str, dir: Option<&Path>, daemon: &Daemon) -> Result<Result<(), String>, String> {
    let mut sh = std::process::Command::new("/bin/sh");
    for (key, value) in [
        ("AGENT_SOCKET", &daemon.socket),
        ("AGENT_STORE", &daemon.store),
    ] {
        match value {
            Some(value) => sh.env(key, value),
            None => sh.env_remove(key),
        };
    }
    sh.args(["-c", command])
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null());
    if let Some(dir) = dir {
        sh.current_dir(dir);
    }
    // Its own process group: whatever it started goes with it, whatever it
    // says, so nothing it forked outlives the check.
    use std::os::unix::process::CommandExt;
    sh.process_group(0);
    let mut child = sh.spawn().map_err(|e| format!("--if: {e}"))?;
    let group = child.id() as libc::pid_t;
    // SAFETY: a signal to the process group this function made.
    let end_group = || unsafe {
        libc::kill(-group, libc::SIGKILL);
    };
    let until = std::time::Instant::now() + GATE_TIMEOUT;
    let said = loop {
        match child.try_wait() {
            Err(e) => break Err(format!("--if: {e}")),
            Ok(Some(status)) if status.success() => break Ok(Ok(())),
            Ok(Some(status)) => break Ok(Err(format!("--if: {status}"))),
            Ok(None) if std::time::Instant::now() > until => {
                end_group();
                let _ = child.wait();
                break Err(format!("--if: ran past {}s", GATE_TIMEOUT.as_secs()));
            }
            Ok(None) => std::thread::sleep(Duration::from_millis(20)),
        }
    };
    end_group();
    said
}

/// The first fire of a `--start` trigger: the agent is made as Sonny
/// makes one, with its folder's composed policy, the default tools and
/// Sonny's own, and shown under the agent that added the trigger. Its request id is the
/// trigger's: a fire cut short after the daemon made it asks again and gets
/// the same agent, and a name anyone else took fails, so a trigger never
/// takes over an agent it did not make.
async fn start(client: &Client, trigger: &Trigger) -> Result<i64, String> {
    let Target::Start {
        name,
        model,
        effort,
        by,
    } = &trigger.target
    else {
        return Err("invalid_trigger: not a --start trigger".into());
    };
    let dir = trigger
        .dir
        .as_ref()
        .ok_or("invalid_trigger: --start has no folder")?;
    let instructions = crate::project::plain(dir)?;
    let config = agent_client::config::load()?;
    let mut settings =
        agent_client::policy::cache_ttl(model).map_or(json!({}), |ttl| json!({"cache_ttl": ttl}));
    if config.hide_senders {
        settings["hide_senders"] = json!(true);
    }
    let made = client
        .request(
            "create",
            json!({"bot": name, "workspace": dir, "model": model, "effort": effort,
                "instructions": instructions,
                "compaction_instructions": agent_client::policy::DEFAULT_COMPACTION_INSTRUCTIONS,
                "tools": agent_client::default_tools(client.providers(), model, config.web_search),
                // The memory tool, and `exec` while MCP servers are on, so
                // an agent no one opens reaches the person's chat, mail
                // and the like as one they open does.
                "declare": crate::declared::tools(false),
                "created_by": by.as_ref().map(|(bot, _)| bot),
                "created_by_id": by.as_ref().map(|(_, id)| id),
                "settings": settings,
                "request_id": format!("trigger-{}", trigger.generation)}),
        )
        .await
        .map_err(coded)?;
    made["bot_id"]
        .as_i64()
        .ok_or_else(|| "create: no bot id".into())
}

/// Give the agent a `--start` trigger made before its agents had Sonny's
/// tools those tools, as [`start`] gives them now; the lookout keeps them
/// up to date from then on. One that has any declared tools is left as it
/// is, and a failure leaves it to the next fire.
async fn adopt(client: &Client, name: &str, id: i64) {
    let Ok(mut record) = client
        .request("inspect", json!({"bot": name, "bot_id": id}))
        .await
    else {
        return;
    };
    record["name"] = json!(name);
    record["bot_id"] = json!(id);
    if let Some(fields) = crate::declared::adopt(&record) {
        let _ = client.request("configure", Value::Object(fields)).await;
    }
}

/// What came of a fire whose agent could not be made. Pinned by id, an
/// agent that added the trigger and is gone never comes back, nor does the
/// agent this trigger made once it is deleted: the trigger ends, as for an
/// answer whose agent is gone.
fn unmade(error: String) -> Value {
    let mut failed = json!({"outcome": "failed", "detail": error});
    if error.starts_with("creator_not_found") || error.starts_with("bot_deleted") {
        failed["gone"] = json!(true);
    }
    failed
}

/// How a turn a trigger sent says so in its `request_id`, by the trigger's
/// generation, which has no `_` (a request id is a name's characters, so
/// the trigger's own name could not be told apart), so a turn shows which
/// trigger sent it.
fn sent_by(trigger: &Trigger) -> String {
    format!("trigger_{}_", trigger.generation)
}

/// Send the fire's message: a new turn when the agent is resting; a working
/// agent, or one with work waiting, skips this time of a repeating trigger
/// and gets any other's after its work. A deleted agent's trigger goes. A
/// `--start` trigger's first fire makes its agent, whose id `kept` takes.
async fn deliver(
    client: &Client,
    places: &Places,
    trigger: &Trigger,
    kept: &mut Kept,
    why: &str,
    ask: Option<&Asked>,
) -> Value {
    // An ask keeps the request sent for it, before it goes: a fire cut
    // short, or the trigger that replaced this one, sends it again as it
    // was, and its answer to where it was to go under the same request id,
    // which the daemon takes once.
    if let Some((sent, sender)) = ask.and_then(|ask| sent_for(ask, trigger)) {
        let bot = sent["bot"].as_str().unwrap_or_default().to_owned();
        return submit(client, &sender, &bot, sent, false, ask).await;
    }
    let prompt = format!(
        "[trigger {} · {} · {why}]\n{}",
        trigger.name,
        stamp(now()),
        trigger.message
    );
    let (bot, id, started) = match (&trigger.target, kept.started_id) {
        (Target::Bot { name, id }, _) => (name, *id, false),
        (Target::Start { name, .. }, Some(id)) => {
            adopt(client, name, id).await;
            (name, id, false)
        }
        (Target::Start { name, .. }, None) => match start(client, trigger).await {
            Ok(id) => (name, id, true),
            Err(error) => return unmade(error),
        },
    };
    if started {
        kept.started_id = Some(id);
        // On its row at once: a trigger that replaces this one while its
        // answer is on its way messages the agent this one made.
        made(places, trigger, id);
    }
    // A new agent is resting, so its first message never waits.
    let asked = ask.is_some_and(|a| a.kind.sends());
    let delivery = if trigger.at.is_some() || asked || started {
        "queue"
    } else {
        "reject"
    };
    let asking = json!({"bot": bot, "bot_id": id, "request_id": request_id(trigger, id, ask),
        "prompt": prompt, "delivery": delivery, "origin": "trigger",
        "budget_tokens": trigger.turn_budget_tokens});
    if let Some(ask) = ask {
        let sent = json!({"store_id": trigger.store_id, "generation": trigger.generation,
            "reply_to": trigger.reply_to, "submit": asking});
        // On disk before it goes, or it does not go: an ask that lost it
        // would send another request.
        if let Err(error) = keep_sent(places, trigger, ask, &sent) {
            return json!({"outcome": "failed", "detail": error});
        }
    }
    submit(client, trigger, bot, asking, started, ask).await
}

/// Write what an ask sends into it, durably: beside the queue first, where
/// a fire taking asks does not look, then renamed into its place.
fn keep_sent(places: &Places, trigger: &Trigger, ask: &Asked, sent: &Value) -> Result<(), String> {
    let beside = places
        .state
        .join(format!(".sent.{}.{}", trigger.name, ask.name()));
    replace(&beside, &sent.to_string())?;
    let taking = ask.path.parent().unwrap_or(Path::new("."));
    std::fs::rename(&beside, &ask.path)
        .and_then(|()| std::fs::File::open(taking)?.sync_all())
        .map_err(|e| {
            let _ = std::fs::remove_file(&beside);
            format!("{}: {e}", ask.path.display())
        })
}

/// The request an ask sent before, to send again, and the trigger that sent
/// it, whose generation and `--reply-to` its answer goes under: only to the
/// store it went to, since one that replaced its trigger on another store
/// sends its own.
fn sent_for(ask: &Asked, trigger: &Trigger) -> Option<(Value, Trigger)> {
    let sent = std::fs::read(&ask.path)
        .ok()
        .and_then(|bytes| serde_json::from_slice::<Value>(&bytes).ok())
        .filter(|sent| sent["store_id"] == trigger.store_id.as_str())?;
    let submit = &sent["submit"];
    if !submit["request_id"].is_string() || !submit["bot"].is_string() {
        return None;
    }
    let sender = Trigger {
        generation: sent["generation"].as_str()?.to_owned(),
        reply_to: serde_json::from_value(sent["reply_to"].clone()).ok()?,
        ..trigger.clone()
    };
    Some((submit.clone(), sender))
}

/// Send a fire's turn, and its answer on to `--reply-to`.
async fn submit(
    client: &Client,
    trigger: &Trigger,
    bot: &str,
    asking: Value,
    started: bool,
    ask: Option<&Asked>,
) -> Value {
    let id = asking["bot_id"].as_i64().unwrap_or_default();
    let submitted = client.request("submit", asking).await;
    let outcome = match submitted {
        Ok(turn) => {
            let mut sent = json!({"outcome": "sent", "turn": turn["turn"],
                "handle": format!("turn:{bot}/{}", turn["turn"])});
            if started {
                sent["started"] = json!(true);
            }
            // A `fire NAME` waiting for this one finds its turn in its
            // receipt, also while an answer for `--reply-to` takes its time
            // and after a trigger this send ends has gone.
            if let Some(ask) = ask.filter(|a| a.kind == Ask::Fire) {
                let _ = std::fs::write(&ask.receipt, sent.to_string());
            }
            sent
        }
        Err(error) if error.code == "bot_busy" || error.code == "active_agent_limit" => {
            json!({"outcome": "skipped", "detail": error.to_string()})
        }
        Err(error) if error.code == "bot_not_found" => {
            json!({"outcome": "gone", "detail": error.to_string()})
        }
        Err(error) => json!({"outcome": "failed", "detail": error.to_string()}),
    };
    if matches!(outcome["outcome"].as_str(), Some("skipped" | "failed")) {
        missed(client, trigger, (bot, id), &outcome).await;
    }
    reply(client, trigger, bot, outcome).await
}

/// A fire that sent nothing tells the agent its answer goes to, else the
/// agent it messages, without starting a turn: it reads it in its next
/// model call, also the one it is busy with, instead of hearing nothing.
async fn missed(client: &Client, trigger: &Trigger, (bot, id): (&str, i64), outcome: &Value) {
    let (to, to_id) = match &trigger.reply_to {
        Some((to, to_id)) => (to.as_str(), *to_id),
        None => (bot, id),
    };
    let detail = outcome["detail"].as_str().unwrap_or("no reason given");
    let prompt = format!(
        "[trigger {} · {} · {} not sent to {bot}: {detail}]",
        trigger.name,
        stamp(now()),
        trigger.when
    );
    let request = format!(
        "{}missed.{to_id}.{}.{}.{}",
        sent_by(trigger),
        now(),
        std::process::id(),
        sends()
    );
    // Telling is a courtesy: what came of the fire is its row's either way.
    let _ = client
        .request(
            "submit",
            json!({"bot": to, "bot_id": to_id, "request_id": request, "prompt": prompt,
                "delivery": "attach", "origin": "trigger"}),
        )
        .await;
}

/// A fire's request id, which says its trigger sent it (`sent_by`). A fire
/// cut short is run again for its ask, which sends the same request: the
/// daemon takes it once. With `--reply-to` it ends `.to.ID`, the agent its
/// answer goes to: the lookout tells a lead of its task's turn only
/// when the answer does not reach it already.
fn request_id(trigger: &Trigger, id: i64, ask: Option<&Asked>) -> String {
    let base = match ask {
        Some(ask) => format!("{}{id}.{}", sent_by(trigger), ask.name()),
        None => format!(
            "{}{id}.{}.{}.{}",
            sent_by(trigger),
            now(),
            std::process::id(),
            sends()
        ),
    };
    match &trigger.reply_to {
        Some((_, to)) => format!("{base}.to.{to}"),
        None => base,
    }
}

/// With `--reply-to`, wait for the turn a fire sent and queue its answer
/// to that agent; what came of it is the outcome's `reply`.
async fn reply(client: &Client, trigger: &Trigger, bot: &str, mut outcome: Value) -> Value {
    let (Some((to, to_id)), Some(turn)) = (&trigger.reply_to, outcome["turn"].as_i64()) else {
        return outcome;
    };
    let handle = format!("turn:{bot}/{turn}");
    let waited = client
        .request(
            "wait",
            json!({"handles": [handle], "timeout_ms": REPLY_WAIT_MS}),
        )
        .await;
    let result = match waited {
        Ok(waited) => waited["results"][&handle].clone(),
        Err(error) => {
            outcome["reply"] = json!({"outcome": "failed", "detail": error.to_string()});
            return outcome;
        }
    };
    if result["pending"] == true || result.is_null() {
        outcome["reply"] =
            json!({"outcome": "failed", "detail": "the turn did not end within a day"});
        return outcome;
    }
    let status = result["status"].as_str().unwrap_or("ended");
    let answer = answer(&result);
    // The daemon hands a long answer over cut short, and says so.
    let cut = if result["text_truncated"] == true {
        " · cut short"
    } else {
        ""
    };
    let prompt = format!(
        "[trigger {} · {} · {bot} turn {turn} {status}{cut}]\n{answer}",
        trigger.name,
        stamp(now())
    );
    let sent = client
        .request(
            "submit",
            json!({"bot": to, "bot_id": to_id,
                "request_id": format!("{}reply.{to_id}.{turn}", sent_by(trigger)),
                "prompt": prompt, "delivery": "queue", "from": {"bot": bot, "turn": turn}}),
        )
        .await;
    outcome["reply"] = match sent {
        Ok(sent) => json!({"outcome": "sent", "bot": to, "turn": sent["turn"]}),
        // Its request id is the turn's own: another prompt under it is this
        // answer, passed on by a fire cut short before it settled.
        Err(error) if error.code == "idempotency_conflict" => {
            json!({"outcome": "sent", "bot": to})
        }
        // Pinned by id, it is never there again: the trigger ends.
        Err(error) if error.code == "bot_not_found" => {
            json!({"outcome": "failed", "bot": to, "gone": true, "detail": error.to_string()})
        }
        Err(error) => json!({"outcome": "failed", "bot": to, "detail": error.to_string()}),
    };
    outcome
}

/// What a turn's `wait` result says: its text, else its error. A turn that
/// ended saying nothing passes on nothing, not "null".
/// The turn the last fire of `row` sent, by handle, when it sent one.
pub fn sent(row: &Value) -> Option<String> {
    let last = &row["last"];
    (last["outcome"] == "sent")
        .then(|| last["handle"].as_str().map(str::to_owned))
        .flatten()
}

/// How the turns of `handles` ended, read from the daemon at once: each
/// one that ended, by handle, as `wait` gives it. A turn still running,
/// or a daemon that does not answer, is left out.
pub async fn ended(client: &Client, handles: Vec<String>) -> HashMap<String, Value> {
    let mut found = HashMap::new();
    // As many as one `wait` takes at a time.
    for some in handles.chunks(64) {
        let Ok(waited) = client
            .request("wait", json!({"handles": some, "timeout_ms": 0}))
            .await
        else {
            continue;
        };
        found.extend(
            (waited["results"].as_object().into_iter().flatten())
                .filter(|(_, result)| result["pending"] != true && result["status"].is_string())
                .map(|(handle, result)| (handle.clone(), result.clone())),
        );
    }
    found
}

/// `row`, its `last` taking how the turn its fire sent ended, from
/// `ended`: its `turn_status`, and `turn_error` when it did not complete,
/// so a fire that went out and whose turn then failed, as when the machine
/// was offline or a login expired, reads so wherever its row is shown.
pub fn with_turn(row: &mut Value, ended: &HashMap<String, Value>) {
    let Some(result) = sent(row).and_then(|handle| ended.get(&handle)) else {
        return;
    };
    let status = result["status"].clone();
    if status != "completed" {
        let error = answer(&json!({"error": result["error"]}));
        row["last"]["turn_error"] = json!(match result["detail"].as_str() {
            Some(detail) if !error.is_empty() => format!("{error}: {detail}"),
            Some(detail) => detail.to_owned(),
            None => error,
        });
    }
    row["last"]["turn_status"] = status;
}

/// [`with_turn`] for each of `rows`, reading their turns from the daemon.
pub async fn with_turns(client: &Client, rows: &mut [Value]) {
    let found = ended(client, rows.iter().filter_map(sent).collect()).await;
    for row in rows {
        with_turn(row, &found);
    }
}

fn answer(result: &Value) -> String {
    match (result["text"].as_str(), &result["error"]) {
        (Some(text), _) if !text.is_empty() => text.to_owned(),
        (_, Value::Null) => String::new(),
        (_, Value::String(error)) => error.clone(),
        (_, error) => error.to_string(),
    }
}

/// `APP --trigger-fire ...`, run by launchd.
pub fn fire_cli(args: &[String]) -> i32 {
    let (trigger, places) = match Trigger::parse(args).and_then(|t| Ok((t, Places::home()?))) {
        Ok(found) => found,
        Err(error) => {
            eprintln!("{}", error_json(&error));
            return 1;
        }
    };
    let asked = match take_ask(&places, &trigger) {
        Ok(asked) => asked,
        Err(error) => {
            stops(&places, &trigger, error, &schedule);
            return 1;
        }
    };
    fire(&places, &trigger, asked.as_ref());
    if let Some(Err(error)) = asked.as_ref().map(done_with) {
        stops(&places, &trigger, error, &schedule);
        return 1;
    }
    0
}

/// A trigger that cannot go on ends, saying why: its row stays.
fn stops(places: &Places, trigger: &Trigger, detail: String, launchd: Loader) {
    let log = |error: String| eprintln!("{}", error_json(&error));
    log(detail.clone());
    let _lock = match Lock::take(places) {
        Ok(lock) => lock,
        Err(error) => return log(error),
    };
    if !ours(places, trigger) {
        return;
    }
    let failed = json!({"outcome": "failed", "detail": detail});
    let kept = Kept::of(&state(places, trigger));
    if let Err(error) = record_last(places, trigger, &failed, &kept)
        .and_then(|()| retire(places, &trigger.name, true, launchd).map(drop))
    {
        log(error);
    }
}

/// This process's sends so far: each send is its own request.
fn sends() -> u64 {
    static SENT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    SENT.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
}

/// One fire: send the message when it is time, or when it was asked for;
/// none of launchd's own runs is asked for.
fn fire(places: &Places, trigger: &Trigger, ask: Option<&Asked>) {
    let asked = ask.is_some_and(|a| a.kind.sends());
    // What came of an ask names it: a `fire NAME` finds its own, and a
    // fire run again for one already settled knows it.
    let named = |mut outcome: Value| {
        if let Some(ask) = ask {
            outcome["ask"] = json!(ask.name());
        }
        outcome
    };
    // A link moved since `add` can make its path one every fire writes: it
    // would fire itself for ever. It ends saying why, in the one write its
    // end makes before launchd lets the job go.
    if let Some(file) = &trigger.file
        && let Err(error) = watches_itself(places, trigger.daemon.store.as_deref(), file)
    {
        return stops(places, trigger, error, &schedule);
    }
    let now = now();
    let durable = state(places, trigger);
    let mut kept = Kept::of(&durable);
    // An ask that sent its request is past its time, its commit and its
    // `--if`: what is left is sending it again, which the daemon takes once,
    // and passing its answer on.
    if let Some(ask) = ask.filter(|a| sent_for(a, trigger).is_some()) {
        // Settled already, and cut short before it was done with its ask:
        // its send is counted.
        if durable["last"]["ask"] == ask.name() {
            return;
        }
        let outcome = with_daemon(trigger, async |client| {
            deliver(client, places, trigger, &mut kept, "fired", Some(ask)).await
        });
        let (Ok(outcome) | Err(outcome)) = outcome;
        settle(places, trigger, &named(outcome), &kept, &schedule);
        return;
    }
    // Its last run went out but its end did not (launchd would not unload
    // it): it ends now, sending nothing more.
    if trigger.runs.is_some_and(|runs| kept.sent >= runs) {
        let last = &durable["last"];
        return ends(
            places,
            trigger,
            last["outcome"] != "sent" || last["reply"]["outcome"] == "failed",
        );
    }
    if !asked {
        // A one-off's calendar entry has no year: the same date a year early
        // is not its time. A day's slack keeps its time when the Mac's time
        // zone changed since it was made, which launchd follows and `at`
        // does not.
        if trigger.at.is_some_and(|at| now < at - SLACK) {
            return;
        }
        if trigger.not_before.is_some_and(|first| now < first) {
            return;
        }
        // Months late is the entry's next year: the Mac was off at its time,
        // or its end was cut short. It is not sent; if it is still listed, it
        // ends saying so.
        if trigger.at.is_some_and(|at| now > at + STALE) {
            let missed = json!({"outcome": "missed", "detail": "its time passed long ago"});
            settle(places, trigger, &missed, &kept, &schedule);
            return;
        }
    }
    let mut why = match asked {
        true => "fired".to_owned(),
        false => trigger.when.clone(),
    };
    // Any write to the HEAD log wakes it; only another commit is news.
    if let Some(repo) = &trigger.commit {
        let (news, now) = moved(repo, &kept.seen);
        if !asked && !news {
            // Moves read once are not read again: the next look starts
            // past them.
            if now.head.is_some() && now != kept.seen {
                looked(places, trigger, &now);
            }
            return;
        }
        // A HEAD that cannot be read now keeps the last one seen.
        if let Some(sha) = &now.head {
            why = format!("{why} at {}", &sha[..sha.len().min(12)]);
            kept.seen = now;
        }
    }
    // A gate that says no costs this process and nothing else, and its row
    // says when it last looked; a one-off had its one time, and ends saying
    // so. One that could not say is a failed fire, shown as one.
    let said = trigger
        .gate
        .as_deref()
        .map(|c| gate(c, trigger.dir.as_deref(), &trigger.daemon));
    let checked = |gate: &str, detail: Option<&String>| {
        named(json!({"checked_ms": now * 1000, "gate": gate, "detail": detail}))
    };
    match said {
        None => {}
        Some(Ok(Ok(()))) => kept.checked = checked("go", None),
        Some(Ok(Err(no))) => {
            kept.checked = checked("no", Some(&no));
            if trigger.at.is_some() {
                let declined = named(json!({"outcome": "declined", "detail": no}));
                settle(places, trigger, &declined, &kept, &schedule);
                return;
            }
            noted(places, trigger, &kept);
            received(
                places,
                trigger,
                &named(json!({"outcome": "declined", "detail": no})),
            );
            if trigger.commit.is_some() {
                // The commit it said no to is not news again.
                looked(places, trigger, &kept.seen);
            }
            return;
        }
        Some(Err(broken)) => {
            kept.checked = checked("failed", Some(&broken));
            let failed = named(json!({"outcome": "failed", "detail": broken}));
            settle(places, trigger, &failed, &kept, &schedule);
            return;
        }
    }
    let outcome = with_daemon(trigger, async |client| {
        deliver(client, places, trigger, &mut kept, &why, ask).await
    });
    let (Ok(outcome) | Err(outcome)) = outcome;
    settle(places, trigger, &named(outcome), &kept, &schedule);
}

/// A repeating trigger's `--if` that said no: its row says so, its last
/// result kept. Under the lock, and only while the plist is this trigger's.
fn noted(places: &Places, trigger: &Trigger, kept: &Kept) {
    let written = Lock::take(places).and_then(|_lock| match ours(places, trigger) {
        true => write_row(places, trigger, &state(places, trigger)["last"], kept),
        false => Ok(()),
    });
    if let Err(error) = written {
        eprintln!("{}", error_json(&error));
    }
}

/// The agent a `--start` trigger made, on its row before the fire is done.
/// Under the lock, and only while the plist is this trigger's.
fn made(places: &Places, trigger: &Trigger, id: i64) {
    let written = Lock::take(places).and_then(|_lock| {
        let durable = state(places, trigger);
        let kept = Kept {
            started_id: Some(id),
            ..Kept::of(&durable)
        };
        match ours(places, trigger) {
            true => write_row(places, trigger, &durable["last"], &kept),
            false => Ok(()),
        }
    });
    if let Err(error) = written {
        eprintln!("{}", error_json(&error));
    }
}

/// A commit trigger's look that sent nothing is where its next one starts.
fn looked(places: &Places, trigger: &Trigger, seen: &Seen) {
    if let Err(error) = baseline(places, trigger, seen, false) {
        eprintln!("{}", error_json(&error));
    }
}

/// `f` with the trigger's daemon; why not, as a failed outcome, when it
/// cannot be reached or serves another store.
fn with_daemon<T>(trigger: &Trigger, f: impl AsyncFnOnce(&Client) -> T) -> Result<T, Value> {
    let failed = |detail: String| json!({"outcome": "failed", "detail": detail});
    let runtime = runtime().map_err(failed)?;
    runtime.block_on(async {
        let socket = trigger.daemon.socket().map_err(failed)?;
        let client = connect(&socket, &trigger.daemon).await.map_err(failed)?;
        // The socket may now be another store's daemon's; its bot ids are its own.
        if client.store() != Some(trigger.store_id.as_str()) {
            client.close().await;
            return Err(failed(format!(
                "store_mismatch: the daemon at {} serves another store",
                socket.display()
            )));
        }
        let outcome = f(&client).await;
        client.close().await;
        Ok(outcome)
    })
}

/// Whether the plist is still this trigger's. Read under the lock.
fn ours(places: &Places, trigger: &Trigger) -> bool {
    std::fs::read_to_string(places.job(&trigger.name))
        .is_ok_and(|text| read_job(&text).is_some_and(|(now, _)| now == *trigger))
}

/// The trigger ends, under the lock, while the plist is still its own.
fn ends(places: &Places, trigger: &Trigger, keep: bool) {
    let log = |error: String| eprintln!("{}", error_json(&error));
    let _lock = match Lock::take(places) {
        Ok(lock) => lock,
        Err(error) => return log(error),
    };
    if ours(places, trigger)
        && let Err(error) = retire(places, &trigger.name, keep, &schedule)
    {
        log(error);
    }
}

/// The daemon, started the way Sonny starts one when none answers and
/// the trigger names its store, on the socket the trigger was made with,
/// with the login shell's environment, since launchd's has no provider keys.
pub(crate) async fn connect(
    socket: &Path,
    daemon: &Daemon,
) -> Result<std::sync::Arc<Client>, String> {
    match Client::connect(socket).await {
        Ok((client, _events)) => Ok(client),
        Err(error) if error.code == "daemon_unavailable" => {
            let Some(store) = &daemon.store else {
                return Err(coded(error));
            };
            crate::start_daemon(store, socket, true)
                .map_err(|e| format!("daemon_unavailable: {e}"))?;
            let (client, _events) = Client::connect(socket).await.map_err(coded)?;
            Ok(client)
        }
        Err(error) => Err(coded(error)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::RefCell;

    /// A time on a known local clock.
    fn clock(y: i32, mo: u8, d: u8, h: u8, mi: u8) -> i64 {
        epoch_of(Local {
            year: y,
            month: mo,
            day: d,
            hour: h,
            minute: mi,
        })
        .unwrap()
    }

    fn minutes(when: &When) -> Vec<u8> {
        when.entries.iter().map(|e| e.minute.unwrap()).collect()
    }

    #[test]
    fn every_counts_from_the_next_whole_minute() {
        let now = clock(2026, 9, 28, 23, 52) + 30;
        // Half a minute in: 23:53 is the start, never 23:52 already gone.
        let half = every("30m", now).unwrap();
        assert_eq!(minutes(&half), [23, 53]);
        assert_eq!(half.text, "every 30m");
        assert!(
            half.entries
                .iter()
                .all(|e| e.hour.is_none() && e.day.is_none())
        );
        let two = every("2h", now).unwrap();
        assert_eq!(
            two.entries
                .iter()
                .map(|e| e.hour.unwrap())
                .collect::<Vec<_>>(),
            [1, 3, 5, 7, 9, 11, 13, 15, 17, 19, 21, 23]
        );
        assert!(two.entries.iter().all(|e| e.minute == Some(53)));
        let daily = every("1d", now).unwrap();
        assert_eq!(
            daily.entries,
            [Entry {
                hour: Some(23),
                minute: Some(53),
                ..Entry::default()
            }]
        );
        for bad in ["7m", "5h", "2d", "0m", "m", "30s", "x"] {
            assert!(
                every(bad, now).unwrap_err().starts_with("invalid_every"),
                "{bad}"
            );
        }
    }

    #[test]
    fn a_one_off_is_a_dated_entry_within_a_year() {
        let now = clock(2026, 9, 28, 23, 52);
        let soon = after("45m", now).unwrap();
        assert_eq!(soon.text, "at 2026-09-29 00:37");
        assert_eq!(
            soon.entries,
            [Entry {
                month: Some(9),
                day: Some(29),
                hour: Some(0),
                minute: Some(37),
                weekday: None
            }]
        );
        assert_eq!(soon.at, Some(clock(2026, 9, 29, 0, 37)));
        let morning = at("2026-09-29 09:07", now).unwrap();
        assert_eq!(morning.at, Some(clock(2026, 9, 29, 9, 7)));
        assert_eq!(at("2026-09-29T09:07", now).unwrap(), morning);
        assert!(at("2026-09-28 09:00", now).unwrap_err().contains("passed"));
        assert!(
            at("2027-10-01 09:00", now)
                .unwrap_err()
                .contains("within a year")
        );
        assert!(at("2026-02-30 09:00", now).is_err());
        assert!(at("tomorrow", now).is_err());
        assert!(after("2d", now).is_err());
        // A part out of range is refused, not wrapped; nothing after the minute.
        assert!(at("2027-257-29 09:07", now).is_err());
        assert!(at("2026-10-01 09:07:99", now).is_err());
        assert!(at("2026-10-01-5 09:07", now).is_err());
        // A delay is never cut short to the start of its minute.
        let late = after("1m", now + 59).unwrap();
        assert_eq!(late.at, Some(clock(2026, 9, 28, 23, 54)));
    }

    #[test]
    fn cron_expands_to_calendar_entries() {
        let weekdays = cron("7 9 * * 1-5").unwrap();
        assert_eq!(weekdays.entries.len(), 5);
        assert_eq!(
            weekdays.entries[0],
            Entry {
                weekday: Some(1),
                hour: Some(9),
                minute: Some(7),
                ..Entry::default()
            }
        );
        assert_eq!(minutes(&cron("*/15 * * * *").unwrap()), [0, 15, 30, 45]);
        assert_eq!(minutes(&cron("5/20 * * * *").unwrap()), [5, 25, 45]);
        assert_eq!(cron("0 0 * * 7").unwrap().entries[0].weekday, Some(0));
        // Day or weekday, as cron reads both.
        let either = cron("0 12 1 * 1").unwrap();
        assert_eq!(either.entries.len(), 2);
        assert_eq!(
            (either.entries[0].day, either.entries[0].weekday),
            (Some(1), None)
        );
        assert_eq!(
            (either.entries[1].day, either.entries[1].weekday),
            (None, Some(1))
        );
        assert!(cron("* * * *").is_err());
        assert!(cron("60 * * * *").is_err());
        assert!(cron("* * * * * *").is_err());
        assert!(cron("5-1 * * * *").is_err());
        assert!(cron("* * * * *").is_ok());
        assert!(cron("0-59 0-23 * * *").unwrap_err().contains("narrow"));
        // Refused before it is built: this one would be 656,208 entries.
        assert!(
            cron("0-59 0-23 1-31 1-12 0-6")
                .unwrap_err()
                .contains("narrow")
        );
    }

    fn trigger() -> Trigger {
        Trigger {
            generation: "test-1".into(),
            not_before: None,
            name: "p.fix-login".into(),
            target: Target::Bot {
                name: "p.fix-login".into(),
                id: 42,
            },
            when: "every 30m".into(),
            at: None,
            commit: None,
            dir: None,
            reply_to: None,
            gate: None,
            runs: None,
            turn_budget_tokens: None,
            file: None,
            daemon: Daemon {
                store: Some("/Users/a/.agent/state.sqlite".into()),
                socket: Some("/tmp/s".into()),
            },
            store_id: "00ab".into(),
            message: "Check the PR's CI & reviews; <fix> what's \"red\".\nThen say so.".into(),
        }
    }

    /// Only `fire` runs it.
    fn fired() -> When {
        When {
            text: "fire".into(),
            entries: Vec::new(),
            not_before: None,
            at: None,
            watch: None,
        }
    }

    #[test]
    fn a_plist_carries_everything_a_fire_needs() {
        let s = trigger();
        let when = every("30m", clock(2026, 9, 28, 23, 52)).unwrap();
        let text = plist(
            Path::new("/Applications/Agent.app/Contents/MacOS/agent-app"),
            &s,
            &when,
            Path::new("/H/.agent/triggers/p.fix-login.asks"),
            &[("SHELL", "/bin/zsh".into())],
        );
        assert!(text.contains(
            "<key>QueueDirectories</key>\n  <array>\n    <string>/H/.agent/triggers/p.fix-login.asks</string>\n    <string>/H/.agent/triggers/p.fix-login.taking</string>"
        ));
        assert_eq!(queued(&text, Path::new("/other")), text);
        assert!(
            text.contains(
                "<key>Label</key>\n  <string>me.lydakis.agent.trigger.p.fix-login</string>"
            )
        );
        assert!(text.contains("<dict><key>Minute</key><integer>22</integer></dict>"));
        assert!(text.contains("<key>SHELL</key><string>/bin/zsh</string>"));
        assert!(!text.contains("WatchPaths"));
        let args = program(&text).unwrap();
        assert_eq!(args[0], "/Applications/Agent.app/Contents/MacOS/agent-app");
        assert_eq!(args[1], FIRE_FLAG);
        assert_eq!(Trigger::parse(&args[2..]).unwrap(), s);
        let once = Trigger {
            at: Some(1_790_000_000),
            daemon: Daemon {
                store: None,
                socket: Some("/tmp/s".into()),
            },
            ..s.clone()
        };
        assert_eq!(Trigger::parse(&once.args()[1..]).unwrap(), once);
        // Everything else a trigger may carry comes back as it went.
        let full = Trigger {
            target: Target::Start {
                name: "p.review".into(),
                model: "anthropic/m".into(),
                effort: Some("high".into()),
                by: Some(("p.lead".into(), 7)),
            },
            when: "commit /r".into(),
            commit: Some("/r".into()),
            dir: Some("/r/sub dir".into()),
            reply_to: Some(("p.lead".into(), 7)),
            gate: Some("test -n \"$(git status --porcelain)\" -- x".into()),
            runs: Some(3),
            turn_budget_tokens: Some(50_000),
            ..s.clone()
        };
        let watched = When {
            watch: Some("/r/.git/logs/HEAD".into()),
            ..fired()
        };
        let text = plist(Path::new("/A/app"), &full, &watched, Path::new("/q"), &[]);
        assert!(
            text.contains(
                "<key>WatchPaths</key>\n  <array>\n    <string>/r/.git/logs/HEAD</string>"
            )
        );
        assert!(!text.contains("StartCalendarInterval"));
        assert_eq!(read_job(&text).unwrap().0, full);
        // Only `fire` runs one with neither.
        let text = plist(Path::new("/A/app"), &s, &fired(), Path::new("/q"), &[]);
        assert!(!text.contains("StartCalendarInterval") && !text.contains("WatchPaths"));
        for bad in [
            vec!["--bot", "x"],
            vec!["--start", "x"],
            vec![
                "--bot", "x", "--bot-id", "1", "--start", "y", "--model", "m",
            ],
        ] {
            let mut args: Vec<String> = s.args()[1..].to_vec();
            let at = args.iter().position(|a| a == "--bot").unwrap();
            args.drain(at..at + 4);
            args.splice(0..0, bad.iter().map(|a| a.to_string()));
            assert!(Trigger::parse(&args).is_err(), "{bad:?}");
        }
    }

    #[test]
    fn a_unit_carries_everything_a_fire_needs() {
        let s = Trigger {
            // A message about a plist leaves the unit a unit.
            message: "100% sure: $HOME is \"home\" \\ not\tthere;\nThen say so. <key>ProgramArguments</key>".into(),
            ..trigger()
        };
        let when = every("30m", clock(2026, 9, 28, 23, 52)).unwrap();
        let app = Path::new("/opt/Sonny Dir/sonny");
        let environment = [
            ("SHELL", "/bin/zsh".to_owned()),
            ("PATH", "/a%b:$P".to_owned()),
        ];
        let asks = Path::new("/H/.agent/triggers/p.fix-login.asks");
        let text = unit(app, &s, &when, asks, &environment);
        assert!(text.contains("[Service]\nType=oneshot\nExecStart=\"/opt/Sonny Dir/sonny\" \"--trigger-fire\" \"--name\" \"p.fix-login\""));
        assert!(text.contains("\nStartLimitIntervalSec=0\n"));
        // systemd's own specifiers and variables are doubled; escapes in quotes.
        assert!(text.contains(r#""100%% sure: $$HOME is \"home\" \\ not\tthere;\nThen say so. <key>ProgramArguments</key>""#));
        assert!(text.contains("Environment=\"SHELL=/bin/zsh\"\nEnvironment=\"PATH=/a%%b:$P\"\n"));
        assert!(text.contains(
            "\n[X-Timer]\nOnCalendar=*-*-* *:22:00\nOnCalendar=*-*-* *:52:00\nPersistent=true\nAccuracySec=1s\n"
        ));
        assert!(text.ends_with(
            "\n[X-Path]\nDirectoryNotEmpty=/H/.agent/triggers/p.fix-login.asks\nDirectoryNotEmpty=/H/.agent/triggers/p.fix-login.taking\n"
        ));
        assert_eq!(watched(&text), None);
        let args = program(&text).unwrap();
        assert_eq!(
            (args[0].as_str(), args[1].as_str()),
            (app.to_str().unwrap(), FIRE_FLAG)
        );
        assert_eq!(read_job(&text).unwrap(), (s.clone(), app.to_owned()));
        assert!(runs_with(&text, &environment) && !runs_with(&text, &[]));
        let bare = unit(app, &s, &fired(), asks, &[]);
        assert!(runs_with(&bare, &[]) && !runs_with(&bare, &environment));
        // Only an ask starts one with no times and no path.
        assert!(!bare.contains("[X-Timer]") && !bare.contains("PathChanged="));
        // What the timer and path units are, as a load writes them.
        let label = format!("{LABEL}{}", s.name);
        assert_eq!(
            starter(
                &label,
                "timer",
                section_of(&text, "[X-Timer]").unwrap(),
                "timers.target"
            ),
            format!(
                "# Sonny writes this from {label}.service, and removes it with it.\n[Timer]\nOnCalendar=*-*-* *:22:00\nOnCalendar=*-*-* *:52:00\nPersistent=true\nAccuracySec=1s\n\n[Install]\nWantedBy=timers.target\n"
            )
        );
        assert_eq!(section_of(&bare, "[X-Timer]"), None);
        // A watched path comes back as it went, and the app as it moved.
        let watching = When {
            watch: Some("/r/50% done/.git/logs/HEAD".into()),
            ..fired()
        };
        let text = unit(app, &s, &watching, asks, &[]);
        assert!(text.contains("\n[X-Path]\nPathChanged=/r/50%% done/.git/logs/HEAD\n"));
        assert_eq!(watched(&text), watching.watch);
        let moved = with_app(&text, app, Path::new("/new/100%/sonny"));
        assert_eq!(
            read_job(&moved).unwrap(),
            (s, PathBuf::from("/new/100%/sonny"))
        );
    }

    #[test]
    fn calendar_entries_read_as_systemd_times() {
        let weekdays: Vec<String> = cron("0 9 * * 1-5")
            .unwrap()
            .entries
            .iter()
            .map(on_calendar)
            .collect();
        assert_eq!(weekdays.first().unwrap(), "Mon *-*-* 09:00:00");
        assert_eq!(weekdays.last().unwrap(), "Fri *-*-* 09:00:00");
        assert_eq!(
            on_calendar(&cron("5 * 25 12 *").unwrap().entries[0]),
            "*-12-25 *:05:00"
        );
        assert_eq!(
            on_calendar(&cron("0 0 * * 7").unwrap().entries[0]),
            "Sun *-*-* 00:00:00"
        );
        let once = after("90m", clock(2026, 9, 28, 23, 52)).unwrap();
        assert_eq!(on_calendar(&once.entries[0]), "*-09-29 01:22:00");
    }

    #[test]
    fn a_unit_s_words_read_back_as_they_were_quoted() {
        let tricky = [
            "", "a b", "\"q\"", "\\", "%%", "$$x", ";", "é\n\t\r", "-- x",
        ];
        let line: Vec<String> = tricky.iter().map(|w| quoted(w, true)).collect();
        assert_eq!(words(&line.join(" ")).unwrap(), tricky);
        // Only what Sonny quoted reads back.
        assert_eq!(words("\"a\" b"), None);
        assert_eq!(words("\"open"), None);
        assert_eq!(words("\"50%\""), None);
    }

    #[test]
    fn a_path_systemd_cannot_hold_is_refused() {
        for bad in [
            "/tmp/a\nExecStartPre=/bin/true",
            "/tmp/a ",
            "/tmp/\u{7}",
            "/tmp/a\\",
        ] {
            assert!(
                file(bad).unwrap_err().starts_with("invalid_file:"),
                "{bad:?}"
            );
        }
        assert!(file("/tmp/a b/c").is_ok());
    }

    #[test]
    fn systemd_keeps_triggers_as_launchd_does() {
        let w = World::systemd("systemd");
        let s = trigger();
        assert_eq!(w.install(&s).unwrap(), None);
        let path = w.places.job(&s.name);
        assert_eq!(
            path,
            w.places.jobs.join(format!("{LABEL}p.fix-login.service"))
        );
        assert_eq!(w.state(&s.name), (true, true, false));
        assert!(
            w.plist(&s.name)
                .contains("\n[X-Timer]\nOnCalendar=*-*-* 09:07:00\n")
        );
        assert_eq!(w.rows()[0]["name"], "p.fix-login");
        // The same again is that trigger; another of the name differing only
        // in case is refused, as on a Mac.
        assert_eq!(
            w.install(&Trigger {
                generation: "test-2".into(),
                ..s.clone()
            })
            .unwrap(),
            Some(s.clone())
        );
        let other = Trigger {
            name: "P.fix-login".into(),
            ..s.clone()
        };
        assert!(w.install(&other).unwrap_err().starts_with("name_taken:"));
        // A fire's ask, and a move of the app, as for a plist.
        fire_now(&w.places, &s.name).unwrap();
        refresh(&w.places, Path::new("/B/sonny"), &|c| w.fake.call(c));
        assert_eq!(
            read_job(&w.plist(&s.name)).unwrap().1,
            PathBuf::from("/B/sonny")
        );
        // A retire cut short is finished at the next start.
        std::fs::rename(&path, tomb(&path)).unwrap();
        finish(&w.places, &|c| w.fake.call(c));
        assert_eq!(w.state(&s.name), (false, false, false));
        assert!(!tomb(&path).exists());
        assert_eq!(w.install(&s).unwrap(), None);
        w.remove(&s.name).unwrap();
        assert_eq!(w.state(&s.name), (false, false, false));
        assert!(w.rows().is_empty());
        assert!(
            w.remove(&s.name)
                .unwrap_err()
                .starts_with("trigger_not_found")
        );
    }

    #[test]
    fn a_unit_s_sections_are_found_whole() {
        let text = "[Service]\nA=1\n\n[X-Timer]\nOnCalendar=x\nPersistent=true\n\n[X-Path]\nPathChanged=/p\n";
        assert_eq!(
            section_of(text, "[X-Timer]"),
            Some("OnCalendar=x\nPersistent=true\n\n")
        );
        assert_eq!(section_of(text, "[X-Path]"), Some("PathChanged=/p\n"));
        assert_eq!(section_of(text, "[X-None]"), None);
    }

    /// launchd as triggers see it: which labels are loaded, which were run,
    /// and what it refuses.
    #[derive(Default)]
    struct Fake {
        loaded: RefCell<std::collections::BTreeSet<String>>,
        refuse_load: std::cell::Cell<bool>,
        refuse_unload: std::cell::Cell<bool>,
        unloads: std::cell::Cell<usize>,
    }

    impl Fake {
        fn call(&self, what: Job) -> Result<(), String> {
            match what {
                Job::Load(path) => {
                    if self.refuse_load.get() {
                        return Err("launchctl bootstrap: refused".into());
                    }
                    let label = path.file_stem().unwrap().to_string_lossy().into_owned();
                    self.loaded
                        .borrow_mut()
                        .insert(label)
                        .then_some(())
                        .ok_or_else(|| "launchctl bootstrap: already loaded".into())
                }
                Job::Unload(label) => {
                    self.unloads.set(self.unloads.get() + 1);
                    if self.refuse_unload.get() {
                        return Err("launchctl bootout: busy".into());
                    }
                    self.loaded
                        .borrow_mut()
                        .remove(label)
                        .then_some(())
                        .ok_or_else(|| format!("{NOT_LOADED}launchctl bootout: no such process"))
                }
            }
        }
    }

    struct World {
        root: PathBuf,
        places: Places,
        fake: Fake,
    }

    impl World {
        fn new(tag: &str) -> Self {
            Self::with(tag, Scheduler::Launchd)
        }
        /// Triggers kept by systemd: services in its unit folder.
        fn systemd(tag: &str) -> Self {
            Self::with(tag, Scheduler::Systemd)
        }
        fn with(tag: &str, scheduler: Scheduler) -> Self {
            let root = std::env::temp_dir().join(format!("agent-app-{tag}-{}", std::process::id()));
            let _ = std::fs::remove_dir_all(&root);
            let places = Places {
                jobs: root.join(match scheduler {
                    Scheduler::Launchd => "LaunchAgents",
                    Scheduler::Systemd => "systemd/user",
                }),
                state: root.join(".agent/triggers"),
                scheduler,
            };
            Self {
                root,
                places,
                fake: Fake::default(),
            }
        }
        fn install(&self, s: &Trigger) -> Result<Option<Trigger>, String> {
            let when = every("1d", clock(2026, 9, 28, 9, 7)).unwrap();
            install(
                &self.places,
                Path::new("/A/agent-app"),
                s,
                &when,
                &[],
                false,
                &|w| self.fake.call(w),
            )
        }
        fn settle(&self, s: &Trigger, outcome: Value) {
            let kept = Kept::of(&state(&self.places, s));
            settle(&self.places, s, &outcome, &kept, &|w| self.fake.call(w));
        }
        fn remove(&self, name: &str) -> Result<(), String> {
            remove(&self.places, name, &|w| self.fake.call(w))
        }
        /// Its plist, launchd's copy, and its last result.
        fn state(&self, name: &str) -> (bool, bool, bool) {
            (
                self.places.job(name).exists(),
                self.fake
                    .loaded
                    .borrow()
                    .contains(&format!("{LABEL}{name}")),
                self.places.last(name).exists(),
            )
        }
        fn plist(&self, name: &str) -> String {
            std::fs::read_to_string(self.places.job(name)).unwrap()
        }
        fn rows(&self) -> Vec<Value> {
            list(&self.places, None)["triggers"]
                .as_array()
                .unwrap()
                .clone()
        }
    }

    impl Drop for World {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.root);
        }
    }

    #[test]
    fn a_store_s_triggers_name_every_agent_they_are_for() {
        let w = World::new("on-store");
        let started = Trigger {
            name: "p.digest".into(),
            target: Target::Start {
                name: "p.digest".into(),
                model: "openai/gpt".into(),
                effort: None,
                by: Some(("p.lead".into(), 7)),
            },
            reply_to: Some(("p.lead".into(), 7)),
            ..trigger()
        };
        let elsewhere = Trigger {
            name: "q.other".into(),
            store_id: "another-store".into(),
            ..trigger()
        };
        for t in [&trigger(), &started, &elsewhere] {
            w.install(t).unwrap();
        }
        let found = on_store(&w.places, "00ab");
        let named: Vec<(&str, &[String])> = found
            .iter()
            .map(|(row, bots)| (row["name"].as_str().unwrap(), bots.as_slice()))
            .collect();
        assert_eq!(
            named,
            [
                (
                    "p.digest",
                    &["p.digest".into(), "p.lead".into(), "p.lead".into()][..]
                ),
                ("p.fix-login", &["p.fix-login".into()][..]),
            ]
        );
        assert_eq!(found[1].0["when"], "every 30m");
    }

    #[test]
    fn a_trigger_made_by_its_store_file_s_identity_names_its_lineage_instead() {
        for w in [World::new("restamp"), World::systemd("restamp-unit")] {
            restamps(w);
        }
    }

    fn restamps(w: World) {
        let elsewhere = Trigger {
            name: "q.other".into(),
            store_id: "another-store".into(),
            ..trigger()
        };
        for t in [&trigger(), &elsewhere] {
            w.install(t).unwrap();
        }
        let unloads = w.fake.unloads.get();
        restamp(&w.places, "00ab", "0123456789abcdef", &|x| w.fake.call(x));
        let mut ids: Vec<(String, String)> = triggers(&w.places)
            .into_iter()
            .map(|(t, _)| (t.name, t.store_id))
            .collect();
        ids.sort();
        assert_eq!(
            ids,
            [
                ("p.fix-login".into(), "0123456789abcdef".into()),
                ("q.other".into(), "another-store".into())
            ]
        );
        // Reloaded once; again it changes nothing.
        assert_eq!(w.fake.unloads.get(), unloads + 1);
        restamp(&w.places, "00ab", "0123456789abcdef", &|x| w.fake.call(x));
        assert_eq!(w.fake.unloads.get(), unloads + 1);
    }

    fn once() -> Trigger {
        Trigger {
            name: "p.once".into(),
            when: "in 2h".into(),
            at: Some(1_790_000_000),
            ..trigger()
        }
    }

    #[test]
    fn add_again_is_the_same_trigger_and_rm_removes_through_launchd() {
        let w = World::new("add");
        let s = trigger();
        assert_eq!(w.install(&s).unwrap(), None);
        // The same again changes nothing and says it was there; its result stays.
        record_last(
            &w.places,
            &s,
            &json!({"outcome": "skipped"}),
            &Kept::default(),
        )
        .unwrap();
        let unloads = w.fake.unloads.get();
        let again = Trigger {
            generation: "test-2".into(),
            not_before: Some(1),
            ..s.clone()
        };
        assert_eq!(w.install(&again).unwrap(), Some(s.clone()));
        assert_eq!(w.fake.unloads.get(), unloads);
        assert_eq!(w.rows()[0]["last"]["outcome"], "skipped");
        // Added again with another environment, its fires run with that one.
        let when = every("1d", clock(2026, 9, 28, 9, 7)).unwrap();
        let path = [("PATH", "/opt/new/bin".to_owned())];
        let add = |environment: &[(&str, String)]| {
            install(
                &w.places,
                Path::new("/A/agent-app"),
                &again,
                &when,
                environment,
                false,
                &|c| w.fake.call(c),
            )
        };
        assert_eq!(add(&path).unwrap(), Some(s.clone()));
        assert!(
            w.plist(&s.name)
                .contains("<key>PATH</key><string>/opt/new/bin</string>")
        );
        assert_eq!(w.fake.unloads.get(), unloads + 1);
        assert_eq!(add(&path).unwrap(), Some(s.clone()));
        assert_eq!(w.fake.unloads.get(), unloads + 1);
        assert_eq!(add(&[]).unwrap(), Some(s.clone()));
        assert!(!w.plist(&s.name).contains("EnvironmentVariables"));
        assert_eq!(w.rows()[0]["last"]["outcome"], "skipped");
        // Another under its name is refused, naming what differs.
        let other = Trigger {
            message: "something else".into(),
            ..s.clone()
        };
        let refused = w.install(&other).unwrap_err();
        assert_eq!(
            error_json(&refused),
            json!({"error": "trigger_exists", "field": "message",
                "detail": "p.fix-login is a trigger with another message; pass --replace, rm it first, or pass another --name"})
        );
        let to_start = Trigger {
            target: Target::Start {
                name: "p.fix-login".into(),
                model: "m".into(),
                effort: None,
                by: None,
            },
            ..s.clone()
        };
        assert!(
            w.install(&to_start)
                .unwrap_err()
                .starts_with("trigger_exists: bot:")
        );
        assert_eq!(
            w.plist(&s.name),
            plist(
                Path::new("/A/agent-app"),
                &s,
                &every("1d", clock(2026, 9, 28, 9, 7)).unwrap(),
                &w.places.asks(&s.name),
                &[]
            )
        );
        // A move launchd refuses to load keeps the old path, to be tried again.
        let refused = |what: Job| match what {
            Job::Load(path) if std::fs::read_to_string(path).unwrap().contains("/B/") => {
                Err("launchctl bootstrap: refused".to_owned())
            }
            what => w.fake.call(what),
        };
        refresh(&w.places, Path::new("/B/agent-app"), &refused);
        assert_eq!(triggers(&w.places)[0].1, Path::new("/A/agent-app"));
        // A name differing only in case would share its files on macOS.
        let cased = Trigger {
            name: "P.Fix-Login".into(),
            ..trigger()
        };
        let taken = w.install(&cased).unwrap_err();
        assert!(
            taken.starts_with("name_taken: P.Fix-Login: trigger p.fix-login"),
            "{taken}"
        );
        // Nor does `rm` of that other case reach it, even where the folder would alias it.
        assert!(
            w.remove("P.Fix-Login")
                .unwrap_err()
                .starts_with("trigger_not_found")
        );
        // A move of Sonny is written into every trigger and reloaded.
        let unloads = w.fake.unloads.get();
        refresh(&w.places, Path::new("/B/agent-app"), &|x| w.fake.call(x));
        assert_eq!(triggers(&w.places)[0].1, Path::new("/B/agent-app"));
        assert_eq!(triggers(&w.places)[0].0, s);
        assert_eq!(w.fake.unloads.get(), unloads + 1);
        refresh(&w.places, Path::new("/B/agent-app"), &|x| w.fake.call(x));
        assert_eq!(
            w.fake.unloads.get(),
            unloads + 1,
            "an unmoved app changes nothing"
        );
        w.remove(&s.name).unwrap();
        assert_eq!(w.rows(), Vec::<Value>::new());
        assert_eq!(w.state(&s.name), (false, false, false));
        let bad = Trigger {
            name: "../x".into(),
            ..trigger()
        };
        assert!(w.install(&bad).is_err());
    }

    #[test]
    fn oversized_plists_are_refused_before_installing_a_trigger() {
        let w = World::new("record-limit");
        let broad = cron("0-7 0-7 1-4 1-4 *").unwrap();
        let s = Trigger {
            when: broad.text.clone(),
            ..trigger()
        };
        let app = Path::new("/A/agent-app");
        assert!(plist(app, &s, &broad, Path::new("/q"), &[]).len() as u64 > MAX_RECORD);
        let refused = install(&w.places, app, &s, &broad, &[], false, &|what| {
            w.fake.call(what)
        });
        assert!(refused.unwrap_err().starts_with("invalid_trigger:"));
        assert_eq!(w.state(&s.name), (false, false, false));
        assert_eq!(w.fake.unloads.get(), 0);
        // A large readable plist, including XML expansion, still lists and
        // follows an app move. The limit is serialized bytes, not message bytes.
        let when = cron("0-7 0-7 1-2 1-4 *").unwrap();
        let kept = Trigger {
            when: when.text.clone(),
            message: "&".repeat(MAX_MESSAGE / 2),
            ..s.clone()
        };
        install(&w.places, app, &kept, &when, &[], false, &|what| {
            w.fake.call(what)
        })
        .unwrap();
        let text = w.plist(&s.name);
        assert!(text.len() as u64 > MAX_RECORD * 9 / 10);
        assert!(text.len() as u64 <= MAX_RECORD);
        assert_eq!(w.rows()[0]["message"], kept.message);
        refresh(&w.places, Path::new("/B/agent-app"), &|what| {
            w.fake.call(what)
        });
        assert_eq!(
            read_job(&w.plist(&s.name)).unwrap().1,
            Path::new("/B/agent-app")
        );
    }

    #[test]
    fn pages_bound_messages_and_keep_results_with_their_generation() {
        let w = World::new("pages");
        let s = trigger();
        w.install(&s).unwrap();
        let sent = Kept {
            sent: 1,
            ..Kept::default()
        };
        record_last(&w.places, &s, &json!({"outcome": "sent", "turn": 7}), &sent).unwrap();
        assert_eq!(
            (&w.rows()[0]["last"]["turn"], &w.rows()[0]["sent"]),
            (&json!(7), &json!(1))
        );
        // Another of its name made since (an `rm`, then an `add`) does not
        // take its result; the first one put back sees it again.
        let changed = Trigger {
            generation: "test-next".into(),
            ..s.clone()
        };
        std::fs::write(
            w.places.job(&s.name),
            plist(
                Path::new("/A/app"),
                &changed,
                &fired(),
                Path::new("/q"),
                &[],
            ),
        )
        .unwrap();
        assert!(w.rows()[0]["last"].is_null());
        assert_eq!(w.rows()[0]["sent"], 0);
        std::fs::write(
            w.places.job(&s.name),
            plist(Path::new("/A/app"), &s, &fired(), Path::new("/q"), &[]),
        )
        .unwrap();
        assert_eq!(w.rows()[0]["last"]["turn"], 7);
        for i in 0..PAGE_SIZE + 2 {
            let item = Trigger {
                name: format!("task-{i:03}"),
                message: "x".repeat(MAX_MESSAGE),
                ..s.clone()
            };
            std::fs::write(
                w.places.job(&item.name),
                plist(Path::new("/A/app"), &item, &fired(), Path::new("/q"), &[]),
            )
            .unwrap();
        }
        let first = list(&w.places, None);
        assert_eq!(first["triggers"].as_array().unwrap().len(), PAGE_SIZE);
        let second = list(&w.places, first["next_after"].as_str());
        assert_eq!(second["triggers"].as_array().unwrap().len(), 3);
        assert!(second["next_after"].is_null());
        let before = first["next_after"].as_str().unwrap();
        assert!(
            second["triggers"]
                .as_array()
                .unwrap()
                .iter()
                .all(|r| r["name"].as_str().unwrap() > before)
        );
        // One row by name, past the first page too; none for a name not there.
        assert_eq!(one(&w.places, "task-065").unwrap(), second["triggers"][2]);
        assert!(one(&w.places, "task-999").is_none());
        assert!(one(&w.places, "../x").is_none());
    }

    #[test]
    fn recurring_triggers_wait_a_full_interval_before_the_first_fire() {
        let now = clock(2026, 9, 28, 23, 52) + 30;
        for (value, seconds) in [("30m", 1800), ("2h", 7200), ("1d", 86400)] {
            let when = every(value, now).unwrap();
            assert_eq!(when.not_before, Some(now + 30 + seconds));
            let s = Trigger {
                not_before: when.not_before,
                ..trigger()
            };
            assert_eq!(
                Trigger::parse(&s.args()[1..]).unwrap().not_before,
                when.not_before
            );
        }
    }

    #[test]
    fn creating_leaves_a_trigger_whole_or_nothing() {
        let w = World::new("create");
        let s = trigger();
        // A load launchd refuses leaves nothing.
        w.fake.refuse_load.set(true);
        assert!(w.install(&s).unwrap_err().contains("refused"));
        assert_eq!(w.state(&s.name), (false, false, false));
        w.fake.refuse_load.set(false);
        w.install(&s).unwrap();
        assert_eq!(w.state(&s.name), (true, true, false));
        // A plist there that cannot be read is not replaced, nor unloaded.
        let odd = Trigger {
            name: "p.odd".into(),
            ..trigger()
        };
        std::fs::create_dir_all(w.places.job(&odd.name).join("x")).unwrap();
        let unloads = w.fake.unloads.get();
        assert!(w.install(&odd).is_err());
        assert_eq!(w.fake.unloads.get(), unloads);
        // It is listed, saying why.
        let row = w.rows().into_iter().find(|r| r["name"] == "p.odd").unwrap();
        assert!(row["problem"].as_str().unwrap().starts_with("unreadable"));
        std::fs::remove_dir_all(w.places.job(&odd.name)).unwrap();
        std::fs::write(w.places.job(&odd.name), "not a plist").unwrap();
        assert!(
            w.install(&odd)
                .unwrap_err()
                .starts_with("trigger_exists: definition:")
        );
        std::fs::remove_file(w.places.job(&odd.name)).unwrap();
        // Nor may a name differing only in case take an ended one's result.
        record_last(
            &w.places,
            &once(),
            &json!({"outcome": "failed"}),
            &Kept::default(),
        )
        .unwrap();
        let cased = Trigger {
            name: "P.Once".into(),
            ..trigger()
        };
        assert!(w.install(&cased).unwrap_err().starts_with("name_taken"));
        // A creation over a job loaded without its plist replaces that job.
        w.fake
            .loaded
            .borrow_mut()
            .insert(format!("{LABEL}{}", odd.name));
        w.install(&odd).unwrap();
        assert_eq!(w.state(&odd.name), (true, true, false));
    }

    #[test]
    fn a_fire_ends_only_the_trigger_it_ran_for() {
        let w = World::new("fire");
        let one = once();
        // Delivered: nothing is left.
        w.install(&one).unwrap();
        w.settle(&one, json!({"outcome": "sent", "turn": 2}));
        assert_eq!(w.state(&one.name), (false, false, false));
        // Not delivered: it ends, and its row says why until it is removed.
        w.install(&one).unwrap();
        w.settle(
            &one,
            json!({"outcome": "failed", "detail": "daemon_unavailable"}),
        );
        assert_eq!(w.state(&one.name), (false, false, true));
        let listed = w.rows();
        assert_eq!(listed.len(), 1);
        assert_eq!(
            (&listed[0]["ended"], &listed[0]["last"]["outcome"]),
            (&json!(true), &json!("failed"))
        );
        assert_eq!(listed[0]["message"], one.message);
        w.remove(&one.name).unwrap();
        assert_eq!(w.rows(), Vec::<Value>::new());
        // Replaced while its message went out: the new one is left alone.
        w.install(&one).unwrap();
        let replacement = Trigger {
            message: "a new reminder".into(),
            ..once()
        };
        w.remove(&one.name).unwrap();
        w.install(&replacement).unwrap();
        w.settle(&one, json!({"outcome": "sent", "turn": 3}));
        assert_eq!(w.state(&one.name), (true, true, false));
        assert_eq!(triggers(&w.places)[0].0, replacement);
        // An unload launchd refuses puts the plist back, listed for `rm`.
        w.fake.refuse_unload.set(true);
        w.settle(&replacement, json!({"outcome": "sent", "turn": 4}));
        w.fake.refuse_unload.set(false);
        assert_eq!(w.state(&one.name), (true, true, true));
        assert_eq!(triggers(&w.places)[0].0, replacement);
        assert_eq!(w.rows()[0]["last"]["turn"], 4);
        // A job left loaded without its plist (an end cut short) is unloaded
        // by its next fire, which records nothing over the result there was.
        std::fs::remove_file(w.places.job(&one.name)).unwrap();
        w.settle(&replacement, json!({"outcome": "missed"}));
        assert_eq!(w.state(&one.name), (false, false, true));
        assert_eq!(w.rows()[0]["last"]["turn"], 4);
        w.remove(&one.name).unwrap();
        // A result that cannot be set aside keeps its job loaded, its plist
        // and itself.
        let stuck = w.places.job("p.stuck");
        std::fs::write(&stuck, "plist").unwrap();
        std::fs::write(w.places.last("p.stuck"), "{}").unwrap();
        let blocked = w.places.state.join(".p.stuck.json.retiring");
        std::fs::create_dir_all(blocked.join("x")).unwrap();
        w.fake.loaded.borrow_mut().insert(format!("{LABEL}p.stuck"));
        assert!(retire(&w.places, "p.stuck", false, &|x| w.fake.call(x)).is_err());
        assert!(w.fake.loaded.borrow().contains(&format!("{LABEL}p.stuck")));
        assert_eq!(std::fs::read_to_string(&stuck).unwrap(), "plist");
        assert!(w.places.last("p.stuck").exists());
        std::fs::remove_dir_all(&blocked).unwrap();
        std::fs::remove_file(&stuck).unwrap();
        std::fs::remove_file(w.places.last("p.stuck")).unwrap();
        // A repeating one ends only when its agent is gone, keeping why.
        let s = trigger();
        w.install(&s).unwrap();
        w.settle(&s, json!({"outcome": "skipped"}));
        assert_eq!(w.state(&s.name), (true, true, true));
        w.settle(&s, json!({"outcome": "gone", "detail": "bot_not_found"}));
        assert_eq!(w.state(&s.name), (false, false, true));
        // So does one whose answers go to an agent that is gone.
        w.install(&s).unwrap();
        let lost = json!({"outcome": "failed", "bot": "p.lead", "gone": true});
        w.settle(&s, json!({"outcome": "sent", "turn": 2, "reply": lost}));
        assert_eq!(w.state(&s.name), (false, false, true));
        assert_eq!(w.rows()[0]["last"]["reply"]["gone"], true);
        // And one that starts its agent once the agent that added it is gone;
        // any other agent it could not make is tried again next time.
        w.install(&s).unwrap();
        w.settle(&s, unmade("bot_exists: p.review".into()));
        assert_eq!(w.state(&s.name), (true, true, true));
        w.settle(&s, unmade("creator_not_found: ".into()));
        assert_eq!(w.state(&s.name), (false, false, true));
        let row = &w.rows()[0];
        assert_eq!(
            (&row["ended"], &row["last"]["outcome"]),
            (&json!(true), &json!("failed"))
        );
        assert!(
            row["last"]["detail"]
                .as_str()
                .unwrap()
                .starts_with("creator_not_found")
        );
        w.install(&s).unwrap();
        w.settle(
            &s,
            unmade("bot_deleted: the p.review this request made".into()),
        );
        assert_eq!(w.state(&s.name), (false, false, true));
    }

    #[test]
    fn runs_end_a_trigger_after_that_many_messages_and_keep_what_it_started() {
        let w = World::new("runs");
        let s = Trigger {
            runs: Some(2),
            target: Target::Start {
                name: "p.review".into(),
                model: "m".into(),
                effort: None,
                by: None,
            },
            ..trigger()
        };
        w.install(&s).unwrap();
        // The first fire started its agent; later ones find its id.
        let started = Kept {
            started_id: Some(9),
            ..Kept::default()
        };
        settle(
            &w.places,
            &s,
            &json!({"outcome": "sent", "turn": 1, "started": true}),
            &started,
            &|x| w.fake.call(x),
        );
        assert_eq!(
            Kept::of(&state(&w.places, &s)),
            Kept {
                sent: 1,
                started_id: Some(9),
                ..Kept::default()
            }
        );
        assert_eq!(
            (&w.rows()[0]["bot"], &w.rows()[0]["bot_id"]),
            (&json!("p.review"), &json!(9))
        );
        // A skipped time is not a run.
        w.settle(&s, json!({"outcome": "skipped"}));
        assert_eq!(w.state(&s.name), (true, true, true));
        w.settle(&s, json!({"outcome": "sent", "turn": 5}));
        assert_eq!(w.state(&s.name), (false, false, false));
    }

    #[test]
    fn fire_asks_launchd_to_run_the_job_and_its_fire_knows() {
        let w = World::new("fire-now");
        let s = trigger();
        assert!(
            fire_now(&w.places, &s.name)
                .unwrap_err()
                .starts_with("trigger_not_found")
        );
        w.install(&s).unwrap();
        let asks = w.places.asks(&s.name);
        assert!(w.plist(&s.name).contains(&format!(
            "<key>QueueDirectories</key>\n  <array>\n    <string>{}</string>",
            asks.display()
        )));
        let take = |places: &Places| {
            let asked = take_ask(places, &s).unwrap()?;
            done_with(&asked).unwrap();
            Some(asked.kind)
        };
        assert_eq!(take(&w.places), None, "launchd's own fire");
        // Each ask is one fire, oldest first; launchd runs the job again
        // while any is left.
        for _ in 0..2 {
            let file = fire_now(&w.places, &s.name).unwrap();
            assert!(asks.join(file).exists());
        }
        ask(&w.places, &s.name, Ask::Wake).unwrap();
        assert_eq!(std::fs::read_dir(&asks).unwrap().count(), 3);
        assert_eq!(take(&w.places), Some(Ask::Fire));
        assert_eq!(take(&w.places), Some(Ask::Fire));
        assert_eq!(take(&w.places), Some(Ask::Wake));
        assert_eq!(take(&w.places), None);
        // One a fire took and did not finish with is the next fire's.
        fire_now(&w.places, &s.name).unwrap();
        let cut = take_ask(&w.places, &s).unwrap().unwrap();
        assert_eq!(std::fs::read_dir(&asks).unwrap().count(), 0);
        assert_eq!(take_ask(&w.places, &s).unwrap().unwrap().path, cut.path);
        done_with(&cut).unwrap();
        // Whatever is in the queue goes out of it, a folder too.
        std::fs::create_dir_all(asks.join("x/y")).unwrap();
        assert_eq!(take(&w.places), Some(Ask::Fire));
        assert_eq!(std::fs::read_dir(&asks).unwrap().count(), 0);
        // Nothing half written is ever in the queue.
        let beside = std::fs::read_dir(&w.places.state)
            .unwrap()
            .flatten()
            .filter(|e| e.file_name().to_string_lossy().starts_with('.'))
            .filter(|e| e.file_name() != ".lock")
            .count();
        assert_eq!(beside, 0);
        // A trigger that goes takes its asks with it, once launchd lets it
        // go: until then they stay.
        fire_now(&w.places, &s.name).unwrap();
        w.fake.refuse_unload.set(true);
        assert!(w.remove(&s.name).is_err());
        assert_eq!(std::fs::read_dir(&asks).unwrap().count(), 1);
        w.fake.refuse_unload.set(false);
        w.remove(&s.name).unwrap();
        assert!(!asks.exists());
        assert!(
            std::fs::read_dir(&w.places.state)
                .unwrap()
                .flatten()
                .all(|e| !e.file_name().to_string_lossy().ends_with(".retiring"))
        );
    }

    #[test]
    fn fire_says_what_came_of_its_own_ask() {
        let w = World::new("fire-came");
        let s = trigger();
        w.install(&s).unwrap();
        let wait = |file: &str| super::fired(&w.places, &s.name, file, Duration::ZERO);
        // launchd has not run the job yet.
        let first = fire_now(&w.places, &s.name).unwrap();
        assert_eq!(
            wait(&first),
            json!({"name": s.name, "fired": true, "pending": true})
        );
        // Sent, its answer still on its way to `--reply-to`, or the trigger
        // it ended gone: its receipt says, once.
        let sent = json!({"outcome": "sent", "turn": 7, "handle": "turn:p.fix-login/7"});
        let receipt = w.places.receipt(&s.name, &first);
        std::fs::write(&receipt, sent.to_string()).unwrap();
        assert_eq!(
            wait(&first),
            json!({"name": s.name, "fired": true, "outcome": "sent", "turn": 7,
                "handle": "turn:p.fix-login/7"})
        );
        assert!(!receipt.exists());
        let ending = fire_now(&w.places, &s.name).unwrap();
        std::fs::write(w.places.receipt(&s.name, &ending), sent.to_string()).unwrap();
        w.remove(&s.name).unwrap();
        assert_eq!(wait(&ending)["handle"], "turn:p.fix-login/7");
        w.install(&s).unwrap();
        // Settled: the row says, for this ask and no other.
        let second = fire_now(&w.places, &s.name).unwrap();
        w.settle(
            &s,
            json!({"outcome": "skipped", "detail": "bot_busy", "ask": second}),
        );
        assert_eq!(
            wait(&second),
            json!({"name": s.name, "fired": true, "outcome": "skipped", "detail": "bot_busy"})
        );
        assert_eq!(wait("another")["pending"], true);
        // Two settled before the first one's `fire` looked: each finds its own.
        let (one, two) = (
            fire_now(&w.places, &s.name).unwrap(),
            fire_now(&w.places, &s.name).unwrap(),
        );
        w.settle(
            &s,
            json!({"outcome": "skipped", "detail": "bot_busy", "ask": one}),
        );
        w.settle(
            &s,
            json!({"outcome": "failed", "detail": "down", "ask": two}),
        );
        assert_eq!(wait(&one)["outcome"], "skipped");
        assert_eq!(wait(&two)["outcome"], "failed");
        // Its `--if` said no: nothing went, and the row's check says why.
        let third = fire_now(&w.places, &s.name).unwrap();
        let kept = Kept {
            checked: json!({"checked_ms": 1, "gate": "no", "detail": "--if: exit status: 1",
                "ask": third}),
            ..Kept::of(&state(&w.places, &s))
        };
        noted(&w.places, &s, &kept);
        assert_eq!(
            wait(&third),
            json!({"name": s.name, "fired": true, "outcome": "declined",
                "detail": "--if: exit status: 1"})
        );
    }

    #[test]
    fn a_gate_that_says_no_shows_on_the_row_and_keeps_the_last_result() {
        let w = World::new("gate-noted");
        let s = trigger();
        w.install(&s).unwrap();
        w.settle(&s, json!({"outcome": "sent", "turn": 5}));
        let kept = Kept {
            checked: json!({"checked_ms": 1000, "gate": "no", "detail": "--if: exit status: 1"}),
            ..Kept::of(&state(&w.places, &s))
        };
        noted(&w.places, &s, &kept);
        let row = &w.rows()[0];
        assert_eq!(
            (&row["last"]["outcome"], &row["sent"]),
            (&json!("sent"), &json!(1))
        );
        assert_eq!(row["checked"]["gate"], "no");
        // Another trigger under the name since: its row is not this one's.
        let other = Trigger {
            generation: "test-2".into(),
            ..s.clone()
        };
        std::fs::remove_file(w.places.job(&s.name)).unwrap();
        w.install(&other).unwrap();
        noted(&w.places, &s, &kept);
        assert!(w.rows()[0]["checked"].is_null());
    }

    #[test]
    fn replace_keeps_what_the_trigger_did() {
        let w = World::new("replace");
        let s = Trigger {
            commit: Some("/repo".into()),
            ..trigger()
        };
        w.install(&s).unwrap();
        let seen = Seen {
            head: Some("abc".into()),
            at: Some(3),
        };
        let kept = Kept {
            seen: seen.clone(),
            ..Kept::default()
        };
        settle(
            &w.places,
            &s,
            &json!({"outcome": "sent", "turn": 5}),
            &kept,
            &|x| w.fake.call(x),
        );
        let other = Trigger {
            generation: "test-2".into(),
            message: "something else".into(),
            ..s.clone()
        };
        let when = every("1d", clock(2026, 9, 28, 9, 7)).unwrap();
        let app = Path::new("/A/agent-app");
        let add = |t: &Trigger, replace| {
            install(&w.places, app, t, &when, &[], replace, &|x| w.fake.call(x))
        };
        assert!(
            add(&other, false)
                .unwrap_err()
                .starts_with("trigger_exists")
        );
        assert!(keeps_seen(&w.places, &other), "a commit since is news");
        assert_eq!(add(&other, true).unwrap(), None);
        assert!(w.plist(&s.name).contains("something else"));
        let row = &w.rows()[0];
        assert_eq!(row["generation"], "test-2");
        assert_eq!(row["message"], "something else");
        assert_eq!(row["last"]["outcome"], "sent");
        assert_eq!(row["sent"], 0, "its count starts again");
        assert_eq!(Kept::of(&state(&w.places, &other)).seen, seen);
        // Another repository is not where the last one was.
        let moved = Trigger {
            generation: "test-3".into(),
            commit: Some("/elsewhere".into()),
            ..other.clone()
        };
        assert!(!keeps_seen(&w.places, &moved));
        add(&moved, true).unwrap();
        assert_eq!(Kept::of(&state(&w.places, &moved)).seen, Seen::default());
        assert_eq!(w.rows()[0]["last"]["outcome"], "sent");
        // The agent it started is its own only on the store it started it in.
        let starts = Trigger {
            generation: "test-4".into(),
            target: Target::Start {
                name: "p.review".into(),
                model: "m".into(),
                effort: None,
                by: None,
            },
            ..moved.clone()
        };
        add(&starts, true).unwrap();
        let started = Kept {
            started_id: Some(9),
            ..Kept::of(&state(&w.places, &starts))
        };
        settle(
            &w.places,
            &starts,
            &json!({"outcome": "sent", "turn": 1}),
            &started,
            &|x| w.fake.call(x),
        );
        let reworded = Trigger {
            generation: "test-5".into(),
            message: "reworded".into(),
            ..starts.clone()
        };
        add(&reworded, true).unwrap();
        assert_eq!(Kept::of(&state(&w.places, &reworded)).started_id, Some(9));
        let elsewhere = Trigger {
            generation: "test-6".into(),
            store_id: "another-store".into(),
            ..reworded.clone()
        };
        add(&elsewhere, true).unwrap();
        assert_eq!(Kept::of(&state(&w.places, &elsewhere)).started_id, None);
        // One it made goes on its row before its fire is done, so a
        // replacement made while its answer is on its way messages it.
        made(&w.places, &elsewhere, 11);
        let again = Trigger {
            generation: "test-7".into(),
            message: "again".into(),
            ..elsewhere.clone()
        };
        add(&again, true).unwrap();
        assert_eq!(Kept::of(&state(&w.places, &again)).started_id, Some(11));
        assert_eq!(w.rows()[0]["last"]["outcome"], "sent");
    }

    #[test]
    fn a_replace_whose_row_cannot_be_written_puts_the_old_trigger_back() {
        let w = World::new("replace-unwritten");
        let s = trigger();
        w.install(&s).unwrap();
        // A row nothing can be written over.
        let row = w.places.last(&s.name);
        let _ = std::fs::remove_file(&row);
        std::fs::create_dir_all(row.join("x")).unwrap();
        let other = Trigger {
            generation: "test-2".into(),
            message: "something else".into(),
            ..s.clone()
        };
        let when = every("1d", clock(2026, 9, 28, 9, 7)).unwrap();
        let replaced = install(
            &w.places,
            Path::new("/A/agent-app"),
            &other,
            &when,
            &[],
            true,
            &|x| w.fake.call(x),
        );
        assert!(replaced.is_err());
        let back = read_job(&w.plist(&s.name)).unwrap().0;
        assert_eq!(back, s);
    }

    #[test]
    fn a_queue_a_fire_cannot_take_from_stops_its_trigger() {
        let w = World::new("stuck-asks");
        let s = trigger();
        w.install(&s).unwrap();
        // A taking folder that is a file: nothing can be moved into it.
        std::fs::remove_dir(w.places.taking(&s.name)).unwrap();
        std::fs::write(w.places.taking(&s.name), "").unwrap();
        fire_now(&w.places, &s.name).unwrap();
        let stuck = take_ask(&w.places, &s).unwrap_err();
        assert!(stuck.starts_with("asks_stuck:"), "{stuck}");
        stops(&w.places, &s, stuck, &|x| w.fake.call(x));
        assert_eq!(w.state(&s.name), (false, false, true));
        let row = &w.rows()[0];
        assert_eq!(row["last"]["outcome"], "failed");
        assert!(
            row["last"]["detail"]
                .as_str()
                .unwrap()
                .starts_with("asks_stuck:")
        );
    }

    #[test]
    fn an_answer_is_the_turns_text_else_its_error_else_nothing() {
        assert_eq!(answer(&json!({"text": "done", "error": null})), "done");
        assert_eq!(answer(&json!({"text": "", "error": null})), "");
        assert_eq!(answer(&json!({"text": ""})), "");
        assert_eq!(
            answer(&json!({"text": "", "error": "rate limited"})),
            "rate limited"
        );
        assert_eq!(answer(&json!({"error": {"code": "x"}})), r#"{"code":"x"}"#);
    }

    #[test]
    fn a_gate_says_no_with_any_exit_but_zero() {
        let none = Daemon {
            store: None,
            socket: None,
        };
        let gate = |command: &str, dir: Option<&Path>| super::gate(command, dir, &none);
        assert_eq!(gate("true", None), Ok(Ok(())));
        // It asks its trigger's daemon.
        let daemon = Daemon {
            store: Some("/s/state.sqlite".into()),
            socket: None,
        };
        let on = "test \"$AGENT_STORE\" = /s/state.sqlite && test -z \"$AGENT_SOCKET\"";
        assert_eq!(super::gate(on, None, &daemon), Ok(Ok(())));
        assert!(gate("exit 3", None).unwrap().unwrap_err().contains('3'));
        // One that cannot run says nothing: that is no answer, not a no.
        assert!(gate("true", Some(Path::new("/nonexistent/agent-gate"))).is_err());
        // What it started in the background ends with it.
        let pid = std::env::temp_dir().join(format!("agent-app-gate-{}", std::process::id()));
        let command = format!("sleep 30 & echo $! > {}", pid.display());
        assert_eq!(gate(&command, None), Ok(Ok(())));
        let pid: libc::pid_t = std::fs::read_to_string(&pid)
            .unwrap()
            .trim()
            .parse()
            .unwrap();
        std::thread::sleep(Duration::from_millis(50));
        // SAFETY: signal 0 only asks whether the process is there.
        let alive = unsafe { libc::kill(pid, 0) } == 0
            && std::fs::read_to_string(format!("/proc/{pid}/stat"))
                .map_or(true, |stat| !stat.contains(") Z "));
        assert!(!alive, "the gate's background sleep outlived it");
        let dir = std::env::temp_dir();
        assert!(
            gate(
                "test \"$(pwd -P)\" = \"$(cd \"$0\" && pwd -P)\"",
                Some(&dir)
            )
            .unwrap()
            .is_err()
        );
        assert!(
            gate(
                &format!(
                    "cd {} && test \"$(pwd -P)\" = \"$(cd {} && pwd -P)\"",
                    dir.display(),
                    dir.display()
                ),
                Some(&dir)
            ) == Ok(Ok(()))
        );
    }

    #[test]
    fn a_file_trigger_cannot_watch_the_triggers_own_folder() {
        let w = World::new("own-state");
        std::fs::create_dir_all(&w.places.state).unwrap();
        for own in [
            w.places.state.clone(),
            w.places.last("p.x"),
            w.root.join(".agent/Triggers/p.x.json"),
            w.root.join(".agent/./triggers/new/file"),
        ] {
            let refused = watches_itself(&w.places, None, &own).unwrap_err();
            assert!(refused.starts_with("invalid_file:"), "{own:?}: {refused}");
        }
        std::os::unix::fs::symlink(&w.places.state, w.root.join("link")).unwrap();
        assert!(watches_itself(&w.places, None, &w.root.join("link/p.x.json")).is_err());
        for other in [w.root.join(".agent"), w.root.join(".agent/triggers-not")] {
            assert_eq!(watches_itself(&w.places, None, &other), Ok(()), "{other:?}");
        }
        // Nor its daemon's store, or what SQLite keeps beside it.
        let store = w.root.join(".agent/state.sqlite");
        for own in ["state.sqlite", "state.sqlite-wal", "State.sqlite-shm"] {
            let refused = watches_itself(&w.places, Some(&store), &w.root.join(".agent").join(own));
            assert!(refused.unwrap_err().contains("store"), "{own}");
        }
        assert_eq!(
            watches_itself(
                &w.places,
                Some(&store),
                &w.root.join(".agent/state.sqlite.bak")
            ),
            Ok(())
        );
        // Nor the store's file by another name.
        std::fs::write(store.with_extension("sqlite-wal"), "").unwrap();
        let alias = w.root.join("elsewhere");
        std::fs::hard_link(store.with_extension("sqlite-wal"), &alias).unwrap();
        assert!(
            watches_itself(&w.places, Some(&store), &alias)
                .unwrap_err()
                .contains("store")
        );
    }

    #[test]
    fn a_commit_trigger_watches_the_head_log_and_names_the_commit() {
        let root = std::env::temp_dir().join(format!("agent-app-commit-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).unwrap();
        let git = |args: &[&str]| {
            assert!(
                std::process::Command::new("git")
                    .arg("-C")
                    .arg(&root)
                    .args(["-c", "user.name=t", "-c", "user.email=t@example.com"])
                    .args(args)
                    .output()
                    .unwrap()
                    .status
                    .success(),
                "{args:?}"
            );
        };
        git(&["init", "-q"]);
        assert!(commit(root.to_str().unwrap()).is_ok());
        assert_eq!(head(&root), None, "no commit yet");
        git(&["commit", "-q", "--allow-empty", "-m", "one"]);
        let (when, repo) = commit(root.to_str().unwrap()).unwrap();
        assert_eq!(repo, root);
        assert_eq!(when.text, format!("commit {}", root.display()));
        let log = when.watch.clone().unwrap();
        assert!(log.ends_with(".git/logs/HEAD") && log.exists());
        let first = head(&root).unwrap();
        // A long title in a legacy encoding: its log is not UTF-8.
        let message = root.join("message");
        let mut title = b"caf\xe9 ".to_vec();
        title.extend([b'x'; 4096]);
        std::fs::write(&message, title).unwrap();
        git(&[
            "-c",
            "i18n.commitEncoding=ISO-8859-1",
            "commit",
            "-q",
            "--allow-empty",
            "-F",
            message.to_str().unwrap(),
        ]);
        let two = head(&root);
        assert_ne!(two.as_deref().unwrap(), first);
        let (news, at_two) = moved(&root, &Seen::default());
        assert!(news && at_two.head == two && at_two.at.is_some());
        assert!(!moved(&root, &at_two).0);
        // Checking out a commit that was there is a move, not a commit.
        git(&["checkout", "-q", &first]);
        assert!(!moved(&root, &at_two).0);
        let at_first = moved(&root, &at_two).1;
        // A commit, then back, then to it again: it is one HEAD reaches
        // now and did not then.
        git(&["commit", "-q", "--allow-empty", "-m", "three"]);
        let three = head(&root).unwrap();
        git(&["reset", "-q", "--hard", &first]);
        assert!(!moved(&root, &at_first).0, "back where it was");
        git(&["checkout", "-q", &three]);
        assert!(moved(&root, &at_first).0);
        // A fast-forward to commits that were there before the look moves
        // HEAD and makes none.
        git(&["checkout", "-q", "-B", "main", &first]);
        let at_main = moved(&root, &at_first).1;
        let looked = Seen {
            at: at_main.at.map(|at| at + 2),
            ..at_main.clone()
        };
        git(&["merge", "-q", "--ff-only", &three]);
        assert!(!moved(&root, &looked).0);
        // A HEAD the repository no longer has (made again) excludes nothing:
        // what HEAD reaches is news when it was committed since the last look.
        let gone = Seen {
            head: Some("0".repeat(40)),
            ..at_first.clone()
        };
        assert!(moved(&root, &gone).0);
        let gone_after = Seen {
            at: looked.at,
            ..gone
        };
        assert!(!moved(&root, &gone_after).0);
        assert!(
            commit(root.join("nope").to_str().unwrap())
                .unwrap_err()
                .starts_with("invalid_commit")
        );
        // Where git keeps no HEAD log, nothing would ever fire it.
        git(&["config", "core.logAllRefUpdates", "false"]);
        let none = commit(root.to_str().unwrap()).unwrap_err();
        assert!(none.contains("git keeps no HEAD log here"), "{none}");
        git(&["config", "core.logAllRefUpdates", "always"]);
        assert!(commit(root.to_str().unwrap()).is_ok());
        let bare = root.join("bare.git");
        git(&["init", "-q", "--bare", bare.to_str().unwrap()]);
        assert!(commit(bare.to_str().unwrap()).is_err());
        // A repository made again at its path is watched where it is now.
        let w = World::new("commit-again");
        let s = Trigger {
            when: when.text.clone(),
            commit: Some(root.clone()),
            ..trigger()
        };
        let app = Path::new("/A/agent-app");
        install(&w.places, app, &s, &when, &[], false, &|x| w.fake.call(x)).unwrap();
        let moved = When {
            watch: Some(root.join("elsewhere/logs/HEAD")),
            ..commit(root.to_str().unwrap()).unwrap().0
        };
        let again = install(&w.places, app, &s, &moved, &[], false, &|x| w.fake.call(x));
        assert_eq!(again.unwrap(), Some(s.clone()));
        assert_eq!(watched(&w.plist(&s.name)), moved.watch);
        assert_eq!(w.state(&s.name), (true, true, false));
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn removal_reaches_whatever_is_left_of_a_trigger() {
        let w = World::new("remove");
        let s = trigger();
        w.install(&s).unwrap();
        record_last(&w.places, &s, &json!({"outcome": "sent"}), &Kept::default()).unwrap();
        // An unload launchd refuses keeps everything, to be tried again.
        w.fake.refuse_unload.set(true);
        assert!(w.remove(&s.name).unwrap_err().contains("busy"));
        assert_eq!(w.state(&s.name), (true, true, true));
        w.fake.refuse_unload.set(false);
        w.remove(&s.name).unwrap();
        assert_eq!(w.state(&s.name), (false, false, false));
        assert!(
            w.remove(&s.name)
                .unwrap_err()
                .starts_with("trigger_not_found")
        );
        // A plist launchd no longer has, as after a failed reload.
        w.install(&s).unwrap();
        w.fake.loaded.borrow_mut().clear();
        w.remove(&s.name).unwrap();
        assert_eq!(w.state(&s.name), (false, false, false));
        // A job loaded without its plist.
        w.fake
            .loaded
            .borrow_mut()
            .insert(format!("{LABEL}{}", s.name));
        w.remove(&s.name).unwrap();
        assert_eq!(w.state(&s.name), (false, false, false));
        // A one-off still there two days after its time is listed as missed.
        let late = Trigger {
            at: Some(now() - 3 * 24 * 3600),
            ..once()
        };
        w.install(&late).unwrap();
        assert_eq!(w.rows()[0]["missed"], true);
        // Its fire the next year does not send it; it ends, saying so.
        w.remove(&late.name).unwrap();
        let stale = Trigger {
            at: Some(now() - STALE - 3600),
            ..once()
        };
        w.install(&stale).unwrap();
        w.settle(
            &stale,
            json!({"outcome": "missed", "detail": "its time passed long ago"}),
        );
        assert_eq!(w.state(&stale.name), (false, false, true));
        assert_eq!(w.rows()[0]["last"]["outcome"], "missed");
    }

    #[test]
    fn a_failed_rm_puts_back_files_that_are_not_text() {
        let w = World::new("rm-bytes");
        let s = trigger();
        w.install(&s).unwrap();
        let odd = b"\xff\xfe not text";
        std::fs::write(w.places.last(&s.name), odd).unwrap();
        w.fake.refuse_unload.set(true);
        assert!(w.remove(&s.name).is_err());
        assert_eq!(std::fs::read(w.places.last(&s.name)).unwrap(), odd);
        assert_eq!(w.state(&s.name), (true, true, true));
        // Once launchd lets it go, nothing is left aside.
        w.fake.refuse_unload.set(false);
        w.remove(&s.name).unwrap();
        let left = |dir: &Path| {
            std::fs::read_dir(dir)
                .unwrap()
                .flatten()
                .any(|e| e.file_name().to_string_lossy().ends_with(".retiring"))
        };
        assert!(!left(&w.places.jobs) && !left(&w.places.state));
    }

    #[test]
    fn an_end_its_own_unload_cut_short_is_finished_later() {
        let w = World::new("cut-short");
        let s = trigger();
        let left = |dir: &Path| {
            std::fs::read_dir(dir)
                .unwrap()
                .flatten()
                .any(|e| e.file_name().to_string_lossy().ends_with(".retiring"))
        };
        // What a fire leaves when its unload ends it: its files set aside,
        // its job unloaded, or not yet when it died before.
        let cut = |loaded: bool| {
            w.install(&s).unwrap();
            std::fs::write(w.places.last(&s.name), "{}").unwrap();
            for path in [w.places.job(&s.name), w.places.last(&s.name)] {
                std::fs::rename(&path, tomb(&path)).unwrap();
            }
            if !loaded {
                w.fake
                    .loaded
                    .borrow_mut()
                    .remove(&format!("{LABEL}{}", s.name));
            }
        };
        for loaded in [false, true] {
            cut(loaded);
            finish(&w.places, &|x| w.fake.call(x));
            assert_eq!(w.state(&s.name), (false, false, false));
            assert!(!left(&w.places.jobs) && !left(&w.places.state));
        }
        // Adding the name again finishes it first; the new one keeps its own.
        cut(true);
        w.install(&s).unwrap();
        assert_eq!(w.state(&s.name), (true, true, false));
        assert!(!left(&w.places.jobs) && !left(&w.places.state));
        // The app's start leaves a trigger added since alone, and removes
        // what writes that died before their rename left.
        std::fs::write(tomb(&w.places.last(&s.name)), "{}").unwrap();
        let temporaries = [
            w.places.state.join(format!(".{}.json.4242", s.name)),
            w.places.jobs.join(format!(".{LABEL}{}.plist.4242", s.name)),
        ];
        for path in &temporaries {
            std::fs::write(path, "half").unwrap();
        }
        finish(&w.places, &|x| w.fake.call(x));
        assert_eq!(w.state(&s.name), (true, true, false));
        assert!(temporaries.iter().all(|path| !path.exists()));
        assert!(w.places.state.join(".lock").exists());
    }

    #[test]
    fn errors_have_one_shape() {
        assert_eq!(
            error_json("trigger_not_found: x"),
            json!({"error": "trigger_not_found", "detail": "x"})
        );
        assert_eq!(
            error_json("/a/b: no such file"),
            json!({"error": "trigger_failed", "detail": "/a/b: no such file"})
        );
        assert_eq!(error_json("usage: trigger ls")["error"], "usage");
        // A daemon's code is kept, with or without a detail.
        let mut missing = agent_client::Error::new("bot_not_found");
        assert_eq!(
            error_json(&coded(missing.clone())),
            json!({"error": "bot_not_found", "detail": ""})
        );
        missing.detail = Some("p.x".into());
        assert_eq!(
            error_json(&coded(missing)),
            json!({"error": "bot_not_found", "detail": "p.x"})
        );
    }

    #[test]
    fn a_triggers_call_adds_as_add_does() {
        let now = clock(2026, 9, 28, 23, 52);
        let asked = json!({"name": "pr", "cron": "0 9 * * 1", "start": {"name": "p.r", "model": "a/m",
            "effort": "high"}, "reply_to": "p.lead", "if": "test -e x", "runs": 3,
            "turn_budget_tokens": 50000, "replace": true, "message": "-- check the PR --"});
        let add = parse_add(&add_args(&asked), now).unwrap();
        assert_eq!(add.name.as_deref(), Some("pr"));
        assert_eq!(add.when.text, "cron 0 9 * * 1");
        assert_eq!(
            add.start,
            Some(("p.r".into(), "a/m".into(), Some("high".into())))
        );
        assert_eq!(
            (
                add.reply_to.as_deref(),
                add.gate.as_deref(),
                add.runs,
                add.turn_budget_tokens,
                add.replace
            ),
            (
                Some("p.lead"),
                Some("test -e x"),
                Some(3),
                Some(50_000),
                true
            )
        );
        // The message is the message, flags and all.
        assert_eq!(add.message, "-- check the PR --");
        let mine = parse_add(&add_args(&json!({"every": "30m", "message": "go"})), now).unwrap();
        assert_eq!(
            (mine.bot, mine.start, mine.when.text.as_str()),
            (None, None, "every 30m")
        );
        assert!(parse_add(&add_args(&json!({"every": "7m", "message": "go"})), now).is_err());
    }

    #[test]
    fn add_takes_one_when_one_target_and_a_message() {
        let now = clock(2026, 9, 28, 23, 52);
        let words = |s: &str| s.split(' ').map(str::to_owned).collect::<Vec<_>>();
        let asked = parse_add(&words("--every 30m --bot p.x -- check the PR"), now).unwrap();
        assert_eq!(
            (asked.bot.as_deref(), asked.message.as_str()),
            (Some("p.x"), "check the PR")
        );
        let started = parse_add(
            &words(
                "--start p.r --model a/m --effort high --reply-to p.lead --runs 3 --turn-budget-tokens 50000 --if true -- go",
            ),
            now,
        )
        .unwrap();
        assert_eq!(
            started.start,
            Some(("p.r".into(), "a/m".into(), Some("high".into())))
        );
        assert_eq!(
            (
                started.reply_to.as_deref(),
                started.runs,
                started.turn_budget_tokens,
                started.when.text.as_str()
            ),
            (Some("p.lead"), Some(3), Some(50_000), "fire")
        );
        let watched = parse_add(&words("--file notes.md -- x"), now).unwrap();
        assert_eq!(
            watched.when.watch,
            Some(std::path::absolute("notes.md").unwrap())
        );
        for bad in [
            "--count 3 -- x",
            "--turn-end a -- x",
            "--every 30m --in 5m -- x",
            "--every 30m --file x -- x",
            "--every 30m",
            "--every 30m --",
            "--start p.r -- x",
            "--model a/m -- x",
            "--bot p.x --start p.r --model a/m -- x",
            "--runs 0 -- x",
            "--runs 9223372036854775808 -- x",
            "--bot a --bot b -- x",
            "--wat x -- x",
        ] {
            assert!(
                parse_add(&words(bad), now)
                    .unwrap_err()
                    .starts_with("invalid_trigger:"),
                "{bad}"
            );
        }
        assert!(
            parse_add(&words("-- x"), now).is_ok(),
            "fire only, for the shell's agent"
        );
        assert!(
            parse_add(
                &["--in".into(), "5m".into(), "--".into(), "a\u{7}b".into()],
                now
            )
            .is_err()
        );
    }
}
