//! Triggers: an agent woken with a message it wrote for itself, or one
//! written for it, when something happens: a time comes, a file is written,
//! a repository's HEAD moves, an agent ends a turn, or someone fires it by
//! name. launchd watches, so a trigger fires with the app closed, and once
//! on waking for times the Mac slept through. The daemon has no clock or
//! watcher of its own. No process of a trigger runs between its fires but
//! one watcher, while any trigger waits on turn ends: it follows those
//! agents' events and starts their triggers' fires (`watch`).
//!
//! A trigger is one LaunchAgent, `~/Library/LaunchAgents/LABEL.plist`,
//! which runs this executable with `--trigger-fire` and everything the fire
//! needs as its arguments: the plist is its definition. What its fires did
//! and leave for the next (messages sent, the agent it started, the commit
//! it saw) is `~/.agent/triggers/NAME.json`. A fire messages the agent the
//! trigger was made for, pinned by its id, or starts the agent it names the
//! first time and messages that one after; never a working agent, for a
//! repeating trigger: that time is skipped. A deleted agent takes its
//! triggers with it.
//!
//! `~/.agent/trigger`, a script the app writes, is how agents and people
//! add, list, fire and remove them.
use agent_client::Client;
use serde_json::{Value, json};
use std::{
    io::Write,
    path::{Path, PathBuf},
    time::Duration,
};

mod watch;
pub use watch::watch_cli;
use watch::{newest_cursor, read_watched, rewatch, save_watched, unwatch};

pub const FLAG: &str = "--trigger";
pub const FIRE_FLAG: &str = "--trigger-fire";
/// What the turn-end watcher's job runs.
pub const WATCH_FLAG: &str = "--trigger-watch";
/// Every trigger's launchd label starts with this; the rest is its name.
const LABEL: &str = "me.lydakis.agent.trigger.";
/// The one watcher's job; not a trigger's label, which ends in `.`.
const WATCH_LABEL: &str = "me.lydakis.agent.trigger-watch";
/// A message is a reminder of what to do, not a document.
const MAX_MESSAGE: usize = 16 * 1024;
/// Calendar entries one trigger may expand to.
const MAX_ENTRIES: usize = 1024;
/// launchd's calendar has no year, so a one-off time must fall within one.
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
const USAGE: &str = "usage: trigger add [--name NAME] [WHEN] [--bot NAME | --start NAME --model PROVIDER/MODEL [--effort LEVEL]] [--reply-to BOT] [--if CMD] [--runs N] -- MESSAGE\n         WHEN: --every N{m,h,d} | --in N{m,h} | --at 'YYYY-MM-DD HH:MM' | --cron 'MIN HOUR DAY MONTH WEEKDAY' | --file PATH | --commit REPO | --turn-end BOT [--count N]; none: only fire runs it\n       trigger ls [--after NAME]\n       trigger fire NAME\n       trigger rm NAME";

/// Where triggers live: the LaunchAgents folder holds their plists, and
/// `~/.agent/triggers` what each one's fires did.
#[derive(Clone)]
pub struct Places {
    pub agents: PathBuf,
    pub state: PathBuf,
}

impl Places {
    pub fn home() -> Result<Self, String> {
        let home = PathBuf::from(std::env::var_os("HOME").ok_or("no HOME for triggers")?);
        Ok(Self {
            agents: home.join("Library/LaunchAgents"),
            state: home.join(".agent/triggers"),
        })
    }
    fn plist(&self, name: &str) -> PathBuf {
        self.agents.join(format!("{LABEL}{name}.plist"))
    }
    fn last(&self, name: &str) -> PathBuf {
        self.state.join(format!("{name}.json"))
    }
    /// The trigger's asks: launchd runs its job while any is there, one run
    /// at a time, and again after one that ends with an ask still there.
    fn asks(&self, name: &str) -> PathBuf {
        self.state.join(format!("{name}.asks"))
    }
    /// Where the watcher is in a turn-end trigger's agent's events.
    fn watched(&self, name: &str) -> PathBuf {
        self.state.join(format!("{name}.watch"))
    }
    fn watcher(&self) -> PathBuf {
        self.agents.join(format!("{WATCH_LABEL}.plist"))
    }
    /// The ask a fire took out of the queue, until it is done with it.
    fn taking(&self, name: &str) -> PathBuf {
        taking(&self.asks(name))
    }
}

/// What launchd is asked to do. Tests stand in for it.
pub enum Launchd<'a> {
    Load(&'a Path),
    Unload(&'a str),
    /// Stop a loaded job's process and run it again.
    Restart(&'a str),
}

pub type Loader<'a> = &'a dyn Fn(Launchd) -> Result<(), String>;

/// `launchctl` in the logged-in user's domain. Other systems have no
/// launchd, and triggers are refused there.
pub fn launchctl(what: Launchd) -> Result<(), String> {
    if !cfg!(target_os = "macos") {
        return Err("triggers_unsupported: triggers use launchd, which only macOS has".into());
    }
    let domain = format!("gui/{}", unsafe { libc::getuid() });
    let unloading = matches!(what, Launchd::Unload(_));
    let args = match what {
        Launchd::Load(path) => vec![
            "bootstrap".into(),
            domain,
            path.to_string_lossy().into_owned(),
        ],
        Launchd::Unload(label) => vec!["bootout".into(), format!("{domain}/{label}")],
        Launchd::Restart(label) => {
            vec!["kickstart".into(), "-k".into(), format!("{domain}/{label}")]
        }
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

/// How a loader says the label it was asked to unload is not loaded.
const NOT_LOADED: &str = "not_loaded: ";

/// Unload a label; one launchd does not have is already unloaded.
fn unload(label: &str, launchd: Loader) -> Result<(), String> {
    match launchd(Launchd::Unload(label)) {
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
    fn current() -> Result<Self, String> {
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
    fn socket(&self) -> Result<PathBuf, String> {
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
    /// The agent whose turn ends it waits on, pinned by id, and every how
    /// many of them it fires.
    pub turn_end: Option<(String, i64)>,
    pub count: Option<u64>,
    /// The path `--file` watches, which each fire checks is still not one
    /// every fire writes.
    pub file: Option<PathBuf>,
    pub daemon: Daemon,
    /// The store identity its daemon announced when it was made; a daemon
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
        if let Some((bot, id)) = &self.turn_end {
            pair("--turn-end", bot.clone());
            pair("--turn-end-id", id.to_string());
        }
        if let Some(count) = self.count {
            pair("--count", count.to_string());
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
            const FLAGS: [&str; 24] = [
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
                "--turn-end",
                "--turn-end-id",
                "--count",
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
            turn_end: pinned("--turn-end", "--turn-end-id")?,
            count: number("--count")?.map(|n| n.max(1) as u64),
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
            "sent": state["sent"].as_u64().unwrap_or(0), "message": self.message,
            "once": self.at.is_some(), "last": state["last"]})
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
            ("turn_end", self.turn_end == other.turn_end),
            ("count", self.count == other.count),
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
    Ok(When {
        text: format!("file {}", path.display()),
        entries: Vec::new(),
        not_before: None,
        at: None,
        watch: Some(path),
    })
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
            watch: Some(PathBuf::from(git_dir).join("logs/HEAD")),
        },
        repo,
    ))
}

/// The LaunchAgent for a trigger. `environment` is what the fire starts
/// with besides launchd's own: the shell whose login environment starts a
/// daemon with your keys, as the app starts one. Without calendar entries
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

/// Whether a plist runs its fires with this environment and no other.
fn runs_with(text: &str, environment: &[(&str, String)]) -> bool {
    match environment {
        [] => !text.contains("<key>EnvironmentVariables</key>"),
        _ => text.contains(&environment_section(environment)),
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

/// A plist's program arguments, as the app writes them.
fn program(text: &str) -> Option<Vec<String>> {
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

/// The path a plist has launchd watch, as the app writes it.
fn watched(text: &str) -> Option<PathBuf> {
    let rest = &text[text.find("<key>WatchPaths</key>")?..];
    let body = &rest[rest.find("<string>")? + "<string>".len()..];
    Some(PathBuf::from(unescape(&body[..body.find("</string>")?])))
}

/// The trigger a plist runs, and the app it runs it with.
fn read_plist(text: &str) -> Option<(Trigger, PathBuf)> {
    let args = program(text)?;
    let (app, rest) = args.split_first()?;
    let trigger = Trigger::parse(rest.strip_prefix(&[FIRE_FLAG.to_owned()])?).ok()?;
    Some((trigger, PathBuf::from(app)))
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
        read_plist(&text).ok_or_else(|| "unreadable: not a trigger's plist".into())
    })
}

/// Walk one plist at a time when refreshing the app path.
fn read_all(places: &Places) -> impl Iterator<Item = (String, Read)> {
    std::fs::read_dir(&places.agents)
        .into_iter()
        .flatten()
        .flatten()
        .filter_map(|e| {
            let file = e.file_name().into_string().ok()?;
            let name = file.strip_prefix(LABEL)?.strip_suffix(".plist")?.to_owned();
            Some((name, read_trigger(&e.path())))
        })
}

#[cfg(test)]
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
        (&places.agents, LABEL, ".plist"),
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

/// One trigger's row, as `list` shows it; none when no trigger of that
/// name is there, or one that ended has been removed.
pub fn one(places: &Places, name: &str) -> Option<Value> {
    (valid_name(name).is_ok() && (places.plist(name).exists() || places.last(name).exists()))
        .then(|| row(places, name))
}

fn row(places: &Places, name: &str) -> Value {
    let path = places.plist(name);
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

/// `replace` with the file's mode set from creation, so the new name never
/// has any other.
pub(crate) fn replace_mode(path: &Path, text: &[u8], mode: u32) -> Result<(), String> {
    use std::os::unix::fs::OpenOptionsExt;
    let dir = path.parent().ok_or("no folder")?;
    std::fs::create_dir_all(dir).map_err(|e| format!("{}: {e}", dir.display()))?;
    let temporary = dir.join(format!(
        ".{}.{}",
        path.file_name().unwrap_or_default().to_string_lossy(),
        std::process::id()
    ));
    let written = (|| {
        let mut file = std::fs::OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .mode(mode)
            .open(&temporary)?;
        // An old temporary of this pid may carry another mode.
        std::fs::set_permissions(
            &temporary,
            std::os::unix::fs::PermissionsExt::from_mode(mode),
        )?;
        file.write_all(text)?;
        file.sync_all()?;
        std::fs::rename(&temporary, path)?;
        // The new name is durable only once its folder is.
        std::fs::File::open(dir)?.sync_all()
    })();
    if written.is_err() {
        let _ = std::fs::remove_file(&temporary);
    }
    written.map_err(|e| format!("{}: {e}", path.display()))
}

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
    let taken = taken(&places.agents, &format!("{LABEL}{}.plist", trigger.name))
        .map(|f| f[LABEL.len()..f.len() - ".plist".len()].to_owned())
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
    let path = places.plist(&trigger.name);
    match std::fs::read_to_string(&path) {
        Ok(text) => {
            let (there, there_app) = read_plist(&text).ok_or_else(|| {
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
                        let want = plist(&there_app, &there, when, &asks, environment);
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
                Some(field) => Err(format!(
                    "trigger_exists: {field}: {} is a trigger with another {field}; rm it first or pass another --name",
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
        &plist(app, trigger, when, &asks, environment),
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
    let loaded = replace(path, text).and_then(|()| launchd(Launchd::Load(path)));
    if let Err(error) = loaded {
        match old {
            Some(old) => {
                if replace(path, old).is_ok() {
                    let _ = launchd(Launchd::Load(path));
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
/// a fire ending its trigger and the app's refresh never interleave. The
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
/// one is done, for the ask it left.
pub fn fire_now(places: &Places, name: &str) -> Result<Value, String> {
    valid_name(name)?;
    let _lock = Lock::take(places)?;
    if !stored(&places.plist(name)) {
        return Err(format!("trigger_not_found: {name}"));
    }
    ask(places, name, Ask::Fire)?;
    Ok(json!({"name": name, "fired": true}))
}

/// What an ask asks of the fire launchd runs for it.
#[derive(Debug, Clone, Copy, PartialEq)]
enum Ask {
    /// Send, whatever the time or path says (`fire NAME`).
    Fire,
    /// Look as launchd would, and send only what is news: `add` asks so for
    /// a commit made while launchd began to watch, which woke nothing.
    Wake,
    /// Send for a turn end the watcher counted, at this place in its
    /// agent's events (`ask_turn`).
    Turn(i64),
}

/// How a turn end's ask starts, before its place in the agent's events.
const TURN: &str = "turn.";

impl Ask {
    /// How its file's name starts.
    fn prefix(self) -> &'static str {
        match self {
            Ask::Fire => "",
            Ask::Wake => "wake.",
            Ask::Turn(_) => TURN,
        }
    }
    /// Whether it asks for a message whatever the time or path says.
    fn sends(self) -> bool {
        matches!(self, Ask::Fire | Ask::Turn(_))
    }
    fn of(file: &str) -> Self {
        match file.strip_prefix(TURN).map(str::parse) {
            Some(Ok(cursor)) => Ask::Turn(cursor),
            _ if file.starts_with(Ask::Wake.prefix()) => Ask::Wake,
            _ => Ask::Fire,
        }
    }
}

/// Ask for a fire: a file in the trigger's queue, written whole beside it
/// first, so launchd never starts the job for half of one. Called under the
/// lock, with the plist there. A `fire NAME` ask says no more; the
/// watcher's turn-end asks say which turn ended (`ask_turn`).
fn ask(places: &Places, name: &str, kind: Ask) -> Result<(), String> {
    static ASKS: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let file = format!(
        "{}{}.{}.{}",
        kind.prefix(),
        now(),
        std::process::id(),
        ASKS.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
    );
    ask_as(places, name, &file, "")
}

/// The watcher's ask for a turn end, named by where the turn end is in its
/// agent's events: made again, it is the same ask, and one a fire already
/// took for its message is passed over (`take_ask`).
fn ask_turn(places: &Places, name: &str, cursor: i64, why: &str) -> Result<(), String> {
    ask_as(places, name, &format!("{TURN}{cursor:020}"), why)
}

fn ask_as(places: &Places, name: &str, file: &str, why: &str) -> Result<(), String> {
    let asks = places.asks(name);
    std::fs::create_dir_all(&asks).map_err(|e| format!("{}: {e}", asks.display()))?;
    let beside = places.state.join(format!(".ask.{name}.{file}"));
    replace(&beside, why)?;
    std::fs::rename(&beside, asks.join(file))
        .and_then(|()| std::fs::File::open(&asks)?.sync_all())
        .map_err(|e| {
            let _ = std::fs::remove_file(&beside);
            format!("{}: {e}", asks.display())
        })
}

/// An ask a fire took: out of the queue launchd watches, in the trigger's
/// `NAME.taking` folder until the fire is done with it.
#[derive(Debug)]
struct Asked {
    kind: Ask,
    /// Why it was asked, for a turn end: which turn ended.
    why: String,
    path: PathBuf,
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
        // A turn end asked again while a fire holds it is that ask.
        let again = places.asks(name).join(&file);
        if again.exists() {
            forget(&again).map_err(|e| format!("asks_stuck: {e}"))?;
        }
        return Ok(Some(Asked {
            kind: Ask::of(&file),
            why: why(&path),
            path,
        }));
    }
    let kept = Kept::of(&state(places, trigger));
    let asks = places.asks(name);
    while let Some((file, path)) = oldest(&asks)? {
        let kind = Ask::of(&file);
        // A turn end a fire already sent for, asked again by a watcher that
        // stopped before it saved its place.
        if let Ask::Turn(cursor) = kind
            && kept.turn.is_some_and(|sent| cursor <= sent)
        {
            forget(&path).map_err(|e| format!("asks_stuck: {e}"))?;
            continue;
        }
        let to = taking.join(&file);
        std::fs::create_dir_all(&taking)
            .and_then(|()| std::fs::rename(&path, &to))
            .and_then(|()| std::fs::File::open(&asks)?.sync_all())
            .and_then(|()| std::fs::File::open(&taking)?.sync_all())
            .map_err(|e| format!("asks_stuck: {}: {e}", path.display()))?;
        return Ok(Some(Asked {
            kind,
            why: why(&to),
            path: to,
        }));
    }
    Ok(None)
}

/// What an ask's file says.
fn why(path: &Path) -> String {
    std::fs::read_to_string(path)
        .unwrap_or_default()
        .trim()
        .to_owned()
}

/// The fire is done with its ask. One that cannot go would have launchd
/// run the job for it for ever.
fn done_with(asked: &Asked) -> Result<(), String> {
    match std::fs::symlink_metadata(&asked.path) {
        Ok(meta) if meta.is_dir() => std::fs::remove_dir_all(&asked.path),
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
    /// The newest turn end a message was sent for, by its place in its
    /// agent's events.
    turn: Option<i64>,
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
            turn: state["turn"].as_i64(),
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
    let _lock = match Lock::take(places) {
        Ok(lock) => lock,
        Err(error) => return log(error),
    };
    let path = places.plist(&trigger.name);
    let text = match std::fs::read_to_string(&path) {
        Ok(text) => text,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            // Under the lock no `add` is between writing and loading one.
            return retire(places, &trigger.name, true, launchd).map_or_else(log, |_| true);
        }
        Err(e) => return log(format!("{}: {e}", path.display())),
    };
    if read_plist(&text).is_none_or(|(now, _)| now != *trigger) {
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
    // Its asks and where the watcher was in its agent's events go with it,
    // and come back with the rest.
    let [plist, last, asks, taking, cursor] = its_files(places, name);
    // A turn-end trigger's, also when its plist cannot be read or an end
    // cut short set it aside.
    let watched = [&cursor, &tomb(&cursor)]
        .into_iter()
        .any(|path| stored(path))
        || [&plist, &tomb(&plist)]
            .into_iter()
            .any(|path| read_trigger(path).is_ok_and(|(t, _)| t.turn_end.is_some()));
    for (path, go) in [
        (&plist, true),
        (&last, !keep),
        (&asks, true),
        (&taking, true),
        (&cursor, true),
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
    let unloaded = match launchd(Launchd::Unload(&format!("{LABEL}{name}"))) {
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
    // Then the watcher reads the turn-end triggers left, or goes with the
    // last: last, since the watcher may be what is retiring this one, and
    // its restart ends it.
    if watched && let Err(error) = unwatch(places, launchd) {
        eprintln!("{}", error_json(&error));
    }
    unloaded
}

/// What a trigger has on disk: its plist, last result, asks, the one being
/// taken, and where the watcher was in its agent's events.
fn its_files(places: &Places, name: &str) -> [PathBuf; 5] {
    [
        places.plist(name),
        places.last(name),
        places.asks(name),
        places.taking(name),
        places.watched(name),
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

/// At the app's start, under the lock, finish what was cut short: a retire
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
    let ours = dots(&places.agents)
        .into_iter()
        .filter(|f| f.starts_with(LABEL))
        .map(|f| (places.agents.join(format!(".{f}")), f));
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
        let path = places.plist(&trigger.name);
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
        let moved = text.replacen(
            &format!("<string>{}</string>", escape(&was.to_string_lossy())),
            &format!("<string>{}</string>", escape(&app.to_string_lossy())),
            1,
        );
        let _ = swap(
            &path,
            &format!("{LABEL}{}", trigger.name),
            Some(&text),
            &moved,
            launchd,
        );
    }
    // The watcher's job runs the app too.
    if let Err(error) = rewatch(places, app, &environment(), false, launchd) {
        eprintln!("{}", error_json(&error));
    }
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
    turn_end: Option<String>,
    count: Option<u64>,
    message: String,
}

fn parse_add(args: &[String], now: i64) -> Result<Add, String> {
    let bad = |what: String| format!("invalid_trigger: {what}\n{USAGE}");
    let mut v = std::collections::HashMap::<String, String>::new();
    let (mut when, mut commit, mut turn_end) = (None, None, None);
    let mut iter = args.iter();
    let message = loop {
        let Some(flag) = iter.next() else {
            return Err(bad("a message goes after --".into()));
        };
        if flag == "--" {
            break iter.cloned().collect::<Vec<_>>().join(" ");
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
            "--turn-end" => {
                turn_end = Some(value.clone());
                // Its text, once --count is known.
                When {
                    text: String::new(),
                    entries: Vec::new(),
                    not_before: None,
                    at: None,
                    watch: None,
                }
            }
            "--name" | "--bot" | "--start" | "--model" | "--effort" | "--reply-to" | "--if"
            | "--runs" | "--count" => {
                if v.insert(flag.clone(), value.clone()).is_some() {
                    return Err(bad(format!("{flag} once")));
                }
                continue;
            }
            other => return Err(bad(other.to_owned())),
        };
        if when.replace(asked).is_some() {
            return Err(bad(
                "one of --every, --in, --at, --cron, --file, --commit or --turn-end".into(),
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
    let count = counted("--count")?;
    if let (Some(when), Some(bot)) = (when.as_mut(), &turn_end) {
        when.text = match count {
            None | Some(1) => format!("turn end of {bot}"),
            Some(n) => format!("every {n} turns of {bot}"),
        };
    } else if count.is_some() {
        return Err(bad("--count goes with --turn-end".into()));
    }
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
        turn_end,
        count: count.filter(|n| *n > 1),
        message,
    })
}

/// What a trigger's job starts with besides launchd's own: the shell whose
/// login environment starts a daemon with your keys, as the app starts one.
fn environment() -> Vec<(&'static str, String)> {
    // PATH too: an `--if` command runs with the adder's, as typed there.
    ["HOME", "SHELL", "PATH"]
        .into_iter()
        .filter_map(|key| std::env::var(key).ok().map(|v| (key, v)))
        .collect()
}

fn runtime() -> Result<tokio::runtime::Runtime, String> {
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
fn error_json(message: &str) -> Value {
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

/// `APP --trigger add|ls|fire|rm`, from `~/.agent/trigger`.
pub fn cli(args: &[String]) -> i32 {
    let done = match Places::home().and_then(|places| match args.split_first() {
        Some((verb, rest)) if verb == "ls" => match rest {
            [] => Ok(list(&places, None)),
            [flag, name] if flag == "--after" => Ok(list(&places, Some(name))),
            _ => Err(format!("usage: {USAGE}")),
        },
        Some((verb, [name])) if verb == "rm" => {
            remove(&places, name, &launchctl).map(|()| json!({"removed": name}))
        }
        Some((verb, [name])) if verb == "fire" => fire_now(&places, name),
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
        .request("resume", json!({"bot": name}))
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
    let (target, reply_to, turn_end, store_id) = runtime()?.block_on(async {
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
                    if !places.plist(&name).exists() {
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
            // Only turns that end from now on count.
            let turn_end = match &asked.turn_end {
                Some(bot) => Some((
                    bot.clone(),
                    bot_id(&client, bot).await?,
                    newest_cursor(&client, bot).await?,
                )),
                None => None,
            };
            let store_id = client
                .store()
                .map(str::to_owned)
                .ok_or_else(|| "the daemon announced no store identity".to_owned())?;
            Ok::<_, String>((target, reply_to, turn_end, store_id))
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
        turn_end: turn_end.as_ref().map(|(bot, id, _)| (bot.clone(), *id)),
        count: asked.count,
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
    let watching = std::fs::read_to_string(places.plist(&trigger.name))
        .ok()
        .and_then(|text| watched(&text));
    let app = std::env::current_exe().map_err(|e| e.to_string())?;
    let environment = environment();
    match install(
        places,
        &app,
        &trigger,
        &asked.when,
        &environment,
        &launchctl,
    )? {
        Some(there) => {
            // The same repository made again has another git folder, and
            // launchd watches that one now.
            if let Some(seen) = seen.filter(|_| watching != asked.when.watch) {
                baseline(places, &there, &seen, false)?;
            }
            // An add that failed after its plist went in left the watcher
            // without it: the same add again makes it whole.
            if let (Some(_), Some((_, _, cursor))) = (&there.turn_end, turn_end) {
                start_watch(places, &there, cursor)?;
                rewatch(places, &app, &environment, true, &launchctl)?;
            }
            let mut row = there.json(&state(places, &there));
            row["duplicate"] = json!(true);
            Ok(row)
        }
        None => {
            // A trigger without its baseline would take the commit there for
            // news, so it goes.
            if let Some(seen) = seen
                && let Err(error) = baseline(places, &trigger, &seen, true)
            {
                let _ = remove(places, &trigger.name, &launchctl);
                return Err(error);
            }
            if let Some((_, _, cursor)) = turn_end {
                start_watch(places, &trigger, cursor)?;
                rewatch(places, &app, &environment, true, &launchctl)?;
            }
            Ok(trigger.json(&state(places, &trigger)))
        }
    }
}

/// Where the watcher starts on a turn-end trigger `add` installed, under
/// the lock `rm` takes, and only while it is still that trigger: an `rm`
/// that came between leaves nothing to watch, and the add says so. One it
/// has a place for keeps it.
fn start_watch(places: &Places, trigger: &Trigger, cursor: i64) -> Result<(), String> {
    let _lock = Lock::take(places)?;
    if !ours(places, trigger) {
        return Err(format!(
            "trigger_not_found: {} was removed while it was added",
            trigger.name
        ));
    }
    match read_watched(places, trigger)? {
        Some(_) => Ok(()),
        None => save_watched(
            places,
            trigger,
            watch::Place {
                cursor,
                count: 0,
                from: cursor,
            },
        ),
    }
}

/// What a fire did, kept for the app to show and the next fire to read.
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
    if let Some(turn) = kept.turn {
        row["turn"] = json!(turn);
    }
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
        true => ask(places, &trigger.name, Ask::Wake),
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

/// `sh -c CMD` in the trigger's folder, given `GATE_TIMEOUT`: whether it
/// said go, and why not when it said no; why it said nothing when it could
/// not run or ran past its time, which is the fire failing, not a no.
fn gate(command: &str, dir: Option<&Path>) -> Result<Result<(), String>, String> {
    let mut sh = std::process::Command::new("/bin/sh");
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

/// The first fire of a `--start` trigger: the agent is made as the app
/// makes one, with its folder's composed policy and the default tools, and
/// shown under the agent that added the trigger. Its request id is the
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
    let policy = crate::compose(dir, None, None)?;
    let made = client
        .request(
            "create",
            json!({"bot": name, "workspace": dir, "model": model, "effort": effort,
                "instructions": policy["instructions"],
                "compaction_instructions": policy["compaction_instructions"],
                "tools": crate::TOOLS,
                "created_by": by.as_ref().map(|(bot, _)| bot),
                "created_by_id": by.as_ref().map(|(_, id)| id),
                "request_id": format!("trigger-{}", trigger.generation)}),
        )
        .await
        .map_err(coded)?;
    made["bot_id"]
        .as_i64()
        .ok_or_else(|| "create: no bot id".into())
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
/// the trigger's own name could not be told apart): the watcher counts no
/// turn end of a turn its own trigger sent, so a trigger never wakes itself.
fn sent_by(trigger: &Trigger) -> String {
    format!("trigger_{}_", trigger.generation)
}

/// Send the fire's message: a new turn when the agent is resting; a working
/// agent, or one with work waiting, skips this time of a repeating trigger
/// and gets any other's after its work. A deleted agent's trigger goes. A
/// `--start` trigger's first fire makes its agent, whose id `kept` takes.
async fn deliver(
    client: &Client,
    trigger: &Trigger,
    kept: &mut Kept,
    why: &str,
    ask: Option<&Asked>,
) -> Value {
    let prompt = format!(
        "[trigger {} · {} · {why}]\n{}",
        trigger.name,
        stamp(now()),
        trigger.message
    );
    let (bot, id, started) = match (&trigger.target, kept.started_id) {
        (Target::Bot { name, id }, _) => (name, *id, false),
        (Target::Start { name, .. }, Some(id)) => (name, id, false),
        (Target::Start { name, .. }, None) => match start(client, trigger).await {
            Ok(id) => (name, id, true),
            Err(error) => return unmade(error),
        },
    };
    if started {
        kept.started_id = Some(id);
    }
    // A new agent is resting, so its first message never waits.
    let asked = ask.is_some_and(|a| a.kind.sends());
    let delivery = if trigger.at.is_some() || asked || started {
        "queue"
    } else {
        "reject"
    };
    let submitted = client
        .request(
            "submit",
            json!({"bot": bot, "bot_id": id, "request_id": request_id(trigger, id, ask),
                "prompt": prompt, "delivery": delivery, "origin": "trigger"}),
        )
        .await;
    let outcome = match submitted {
        Ok(turn) if started => json!({"outcome": "sent", "turn": turn["turn"], "started": true}),
        Ok(turn) => json!({"outcome": "sent", "turn": turn["turn"]}),
        Err(error) if error.code == "bot_busy" || error.code == "active_agent_limit" => {
            json!({"outcome": "skipped", "detail": error.to_string()})
        }
        Err(error) if error.code == "bot_not_found" => {
            json!({"outcome": "gone", "detail": error.to_string()})
        }
        Err(error) => json!({"outcome": "failed", "detail": error.to_string()}),
    };
    reply(client, trigger, bot, outcome).await
}

/// A fire's request id, which says its trigger sent it (`sent_by`). A fire
/// cut short is run again for its ask, which sends the same request: the
/// daemon takes it once. With `--reply-to` it ends `.to.ID`, the agent its
/// answer goes to: the app tells a coordinator of its task's turn only
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
            stops(&places, &trigger, error, &launchctl);
            return 1;
        }
    };
    fire(&places, &trigger, asked.as_ref());
    if let Some(Err(error)) = asked.as_ref().map(done_with) {
        stops(&places, &trigger, error, &launchctl);
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
    // A link moved since `add` can make its path one every fire writes: it
    // would fire itself for ever. It ends saying why, in the one write its
    // end makes before launchd lets the job go.
    if let Some(file) = &trigger.file
        && let Err(error) = watches_itself(places, trigger.daemon.store.as_deref(), file)
    {
        return stops(places, trigger, error, &launchctl);
    }
    let now = now();
    let durable = state(places, trigger);
    let mut kept = Kept::of(&durable);
    // The turn end this fire is for goes on disk with what it did, so the
    // same ask made again is passed over.
    if let Some(Ask::Turn(cursor)) = ask.map(|a| a.kind) {
        kept.turn = Some(cursor);
    }
    // Only the watcher, or `fire`, has news of a turn end.
    if trigger.turn_end.is_some() && !asked {
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
            settle(places, trigger, &missed, &kept, &launchctl);
            return;
        }
    }
    let mut why = match ask {
        Some(ask) if !ask.why.is_empty() => ask.why.clone(),
        _ if asked => "fired".to_owned(),
        _ => trigger.when.clone(),
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
    // A gate that says no costs this process and nothing else; a one-off
    // had its one time, and ends saying so. One that could not say is a
    // failed fire, shown as one.
    match trigger
        .gate
        .as_deref()
        .map(|c| gate(c, trigger.dir.as_deref()))
    {
        None | Some(Ok(Ok(()))) => {}
        Some(Ok(Err(no))) => {
            if trigger.at.is_some() {
                let declined = json!({"outcome": "declined", "detail": no});
                settle(places, trigger, &declined, &kept, &launchctl);
            } else if trigger.commit.is_some() {
                // The commit it said no to is not news again.
                looked(places, trigger, &kept.seen);
            }
            return;
        }
        Some(Err(broken)) => {
            let failed = json!({"outcome": "failed", "detail": broken});
            settle(places, trigger, &failed, &kept, &launchctl);
            return;
        }
    }
    let outcome = with_daemon(trigger, async |client| {
        deliver(client, trigger, &mut kept, &why, ask).await
    });
    let (Ok(outcome) | Err(outcome)) = outcome;
    settle(places, trigger, &outcome, &kept, &launchctl);
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
    std::fs::read_to_string(places.plist(&trigger.name))
        .is_ok_and(|text| read_plist(&text).is_some_and(|(now, _)| now == *trigger))
}

/// The trigger ends, under the lock, while the plist is still its own.
fn ends(places: &Places, trigger: &Trigger, keep: bool) {
    let log = |error: String| eprintln!("{}", error_json(&error));
    let _lock = match Lock::take(places) {
        Ok(lock) => lock,
        Err(error) => return log(error),
    };
    if ours(places, trigger)
        && let Err(error) = retire(places, &trigger.name, keep, &launchctl)
    {
        log(error);
    }
}

/// The daemon, started the way the app starts one when none answers and
/// the trigger names its store, on the socket the trigger was made with.
async fn connect(socket: &Path, daemon: &Daemon) -> Result<std::sync::Arc<Client>, String> {
    match Client::connect(socket).await {
        Ok((client, _events)) => Ok(client),
        Err(error) if error.code == "daemon_unavailable" => {
            let (Some(store), Some(agent)) = (&daemon.store, crate::daemon::bundled()) else {
                return Err(coded(error));
            };
            crate::daemon::Starts::default()
                .start(&agent, store, daemon.socket.as_deref())
                .await?;
            let (client, _events) = Client::connect(socket).await.map_err(coded)?;
            Ok(client)
        }
        Err(error) => Err(coded(error)),
    }
}

/// `~/.agent/trigger`, written again whenever the app starts from
/// somewhere else.
pub fn write_script(home: &Path, app: &Path) -> Result<(), String> {
    let quote = |p: &Path| format!("'{}'", p.to_string_lossy().replace('\'', r"'\''"));
    let usage = USAGE.replace('\n', "\n# ");
    let text = format!("#!/bin/sh\n# {usage}\nexec {} {FLAG} \"$@\"\n", quote(app));
    let path = home.join("trigger");
    use std::os::unix::fs::PermissionsExt;
    let runnable = std::fs::metadata(&path).is_ok_and(|m| m.permissions().mode() & 0o777 == 0o755);
    if runnable && std::fs::read_to_string(&path).is_ok_and(|have| have == text) {
        return Ok(());
    }
    replace_mode(&path, text.as_bytes(), 0o755)
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
            turn_end: None,
            count: None,
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
            turn_end: Some(("p.task".into(), 9)),
            count: Some(20),
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
        assert_eq!(read_plist(&text).unwrap().0, full);
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

    /// launchd as triggers see it: which labels are loaded, which were run,
    /// and what it refuses.
    #[derive(Default)]
    struct Fake {
        loaded: RefCell<std::collections::BTreeSet<String>>,
        started: RefCell<Vec<String>>,
        refuse_load: std::cell::Cell<bool>,
        refuse_unload: std::cell::Cell<bool>,
        unloads: std::cell::Cell<usize>,
    }

    impl Fake {
        fn call(&self, what: Launchd) -> Result<(), String> {
            match what {
                Launchd::Load(path) => {
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
                Launchd::Unload(label) => {
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
                Launchd::Restart(label) => {
                    if !self.loaded.borrow().contains(label) {
                        return Err("launchctl kickstart: no such service".into());
                    }
                    self.started.borrow_mut().push(label.to_owned());
                    Ok(())
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
            let root = std::env::temp_dir().join(format!("agent-app-{tag}-{}", std::process::id()));
            let _ = std::fs::remove_dir_all(&root);
            let places = Places {
                agents: root.join("LaunchAgents"),
                state: root.join(".agent/triggers"),
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
                self.places.plist(name).exists(),
                self.fake
                    .loaded
                    .borrow()
                    .contains(&format!("{LABEL}{name}")),
                self.places.last(name).exists(),
            )
        }
        fn plist(&self, name: &str) -> String {
            std::fs::read_to_string(self.places.plist(name)).unwrap()
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
                "detail": "p.fix-login is a trigger with another message; rm it first or pass another --name"})
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
        let refused = |what: Launchd| match what {
            Launchd::Load(path) if std::fs::read_to_string(path).unwrap().contains("/B/") => {
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
        // A move of the app is written into every trigger and reloaded.
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
    fn the_script_is_runnable_even_when_its_text_was_already_there() {
        use std::os::unix::fs::PermissionsExt;
        let root = std::env::temp_dir().join(format!("agent-app-script-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        let app = Path::new("/A/agent-app");
        write_script(&root, app).unwrap();
        let path = root.join("trigger");
        let mode = |p: &Path| std::fs::metadata(p).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode(&path), 0o755);
        // Its text survived a crash before its mode did.
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644)).unwrap();
        write_script(&root, app).unwrap();
        assert_eq!(mode(&path), 0o755);
        std::fs::remove_dir_all(root).unwrap();
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
        let refused = install(&w.places, app, &s, &broad, &[], &|what| w.fake.call(what));
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
        install(&w.places, app, &kept, &when, &[], &|what| w.fake.call(what)).unwrap();
        let text = w.plist(&s.name);
        assert!(text.len() as u64 > MAX_RECORD * 9 / 10);
        assert!(text.len() as u64 <= MAX_RECORD);
        assert_eq!(w.rows()[0]["message"], kept.message);
        refresh(&w.places, Path::new("/B/agent-app"), &|what| {
            w.fake.call(what)
        });
        assert_eq!(
            read_plist(&w.plist(&s.name)).unwrap().1,
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
            w.places.plist(&s.name),
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
            w.places.plist(&s.name),
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
                w.places.plist(&item.name),
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
        std::fs::create_dir_all(w.places.plist(&odd.name).join("x")).unwrap();
        let unloads = w.fake.unloads.get();
        assert!(w.install(&odd).is_err());
        assert_eq!(w.fake.unloads.get(), unloads);
        // It is listed, saying why.
        let row = w.rows().into_iter().find(|r| r["name"] == "p.odd").unwrap();
        assert!(row["problem"].as_str().unwrap().starts_with("unreadable"));
        std::fs::remove_dir_all(w.places.plist(&odd.name)).unwrap();
        std::fs::write(w.places.plist(&odd.name), "not a plist").unwrap();
        assert!(
            w.install(&odd)
                .unwrap_err()
                .starts_with("trigger_exists: definition:")
        );
        std::fs::remove_file(w.places.plist(&odd.name)).unwrap();
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
        std::fs::remove_file(w.places.plist(&one.name)).unwrap();
        w.settle(&replacement, json!({"outcome": "missed"}));
        assert_eq!(w.state(&one.name), (false, false, true));
        assert_eq!(w.rows()[0]["last"]["turn"], 4);
        w.remove(&one.name).unwrap();
        // A result that cannot be set aside keeps its job loaded, its plist
        // and itself.
        let stuck = w.places.plist("p.stuck");
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
            assert_eq!(
                fire_now(&w.places, &s.name).unwrap(),
                json!({"name": s.name, "fired": true})
            );
        }
        ask(&w.places, &s.name, Ask::Wake).unwrap();
        assert_eq!(std::fs::read_dir(&asks).unwrap().count(), 3);
        assert_eq!(take(&w.places), Some(Ask::Fire));
        assert_eq!(take(&w.places), Some(Ask::Fire));
        assert_eq!(take(&w.places), Some(Ask::Wake));
        assert_eq!(take(&w.places), None);
        // A turn end's ask says which turn ended. The same turn end asked
        // again is the same ask, and one at or before the turn end a message
        // was begun for is passed over.
        ask_turn(&w.places, &s.name, 7, "turn end of p: turn:p/3 completed").unwrap();
        ask_turn(&w.places, &s.name, 9, "turn end of p: turn:p/4 completed").unwrap();
        ask_turn(&w.places, &s.name, 9, "turn end of p: turn:p/4 completed").unwrap();
        assert_eq!(std::fs::read_dir(&asks).unwrap().count(), 2);
        let turn = take_ask(&w.places, &s).unwrap().unwrap();
        assert_eq!(
            (turn.kind, turn.why.as_str()),
            (Ask::Turn(7), "turn end of p: turn:p/3 completed")
        );
        done_with(&turn).unwrap();
        let kept = Kept {
            turn: Some(9),
            ..Kept::default()
        };
        record_last(&w.places, &s, &Value::Null, &kept).unwrap();
        ask_turn(&w.places, &s.name, 8, "late").unwrap();
        assert_eq!(take(&w.places), None);
        assert_eq!(std::fs::read_dir(&asks).unwrap().count(), 0);
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
    fn an_add_an_rm_came_between_starts_no_watch() {
        let w = World::new("start-watch");
        let mut s = trigger();
        s.turn_end = Some(("p.task".into(), 5));
        assert!(
            start_watch(&w.places, &s, 9)
                .unwrap_err()
                .starts_with("trigger_not_found")
        );
        assert!(!w.places.watched(&s.name).exists());
        w.install(&s).unwrap();
        start_watch(&w.places, &s, 9).unwrap();
        // The same add again keeps the place the watcher reached.
        let place = watch::Place {
            cursor: 12,
            count: 1,
            from: 10,
        };
        watch::save_watched(&w.places, &s, place).unwrap();
        start_watch(&w.places, &s, 9).unwrap();
        assert_eq!(watch::read_watched(&w.places, &s), Ok(Some(place)));
    }

    #[test]
    fn a_gate_says_no_with_any_exit_but_zero() {
        assert_eq!(gate("true", None), Ok(Ok(())));
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
        install(&w.places, app, &s, &when, &[], &|x| w.fake.call(x)).unwrap();
        let moved = When {
            watch: Some(root.join("elsewhere/logs/HEAD")),
            ..commit(root.to_str().unwrap()).unwrap().0
        };
        let again = install(&w.places, app, &s, &moved, &[], &|x| w.fake.call(x));
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
        assert!(!left(&w.places.agents) && !left(&w.places.state));
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
            for path in [w.places.plist(&s.name), w.places.last(&s.name)] {
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
            assert!(!left(&w.places.agents) && !left(&w.places.state));
        }
        // Adding the name again finishes it first; the new one keeps its own.
        cut(true);
        w.install(&s).unwrap();
        assert_eq!(w.state(&s.name), (true, true, false));
        assert!(!left(&w.places.agents) && !left(&w.places.state));
        // The app's start leaves a trigger added since alone, and removes
        // what writes that died before their rename left.
        std::fs::write(tomb(&w.places.last(&s.name)), "{}").unwrap();
        let temporaries = [
            w.places.state.join(format!(".{}.json.4242", s.name)),
            w.places
                .agents
                .join(format!(".{LABEL}{}.plist.4242", s.name)),
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
                "--start p.r --model a/m --effort high --reply-to p.lead --runs 3 --if true -- go",
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
                started.when.text.as_str()
            ),
            (Some("p.lead"), Some(3), "fire")
        );
        let watched = parse_add(&words("--file notes.md -- x"), now).unwrap();
        assert_eq!(
            watched.when.watch,
            Some(std::path::absolute("notes.md").unwrap())
        );
        let ends = parse_add(&words("--count 20 --turn-end Home -- note it"), now).unwrap();
        assert_eq!(
            (
                ends.turn_end.as_deref(),
                ends.count,
                ends.when.text.as_str()
            ),
            (Some("Home"), Some(20), "every 20 turns of Home")
        );
        assert!(ends.when.entries.is_empty() && ends.when.watch.is_none());
        let each = parse_add(&words("--turn-end p.task --count 1 -- x"), now).unwrap();
        assert_eq!(
            (each.count, each.when.text.as_str()),
            (None, "turn end of p.task")
        );
        for bad in [
            "--count 3 -- x",
            "--every 30m --count 3 -- x",
            "--turn-end a --count 0 -- x",
            "--turn-end a --file x -- x",
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
