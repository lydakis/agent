//! Triggers: an agent woken with a message it wrote for itself, or one
//! written for it, when something happens: a time comes, a file is written,
//! a repository's HEAD moves, or someone fires it by name. Sonny's
//! scheduler watches ([`crate::scheduler`]): one job of the system's own
//! (a LaunchAgent on a Mac, a systemd user service on Linux) that Sonny sets
//! up once and never changes, so a trigger fires with Sonny closed, and
//! once on waking for times the machine slept through, while adding,
//! changing or removing one is a file and nothing the system registers.
//! The daemon has no clock or watcher of its own.
//!
//! A trigger is one definition, `~/.agent/triggers/jobs/NAME.json`: the
//! program its fire runs (this executable with `--trigger-fire` and
//! everything the fire needs as its arguments), the environment it runs
//! with, its calendar entries and the path it watches. The scheduler runs
//! it as launchd runs a job: one fire of a trigger at a time, again while
//! its queue of asks holds one. What its fires did and leave for the next
//! (messages sent, the agent it started, the commit it saw) is
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
/// How each trigger's own launchd label and systemd unit name started,
/// before triggers moved to Sonny's scheduler: what [`migrate`] looks for.
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

/// The system's own scheduler, which keeps Sonny's one job.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Scheduler {
    /// A LaunchAgent plist, loaded with `launchctl`.
    Launchd,
    /// A user service, through `systemctl --user`.
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
    /// How a job's file name ends.
    pub(crate) fn suffix(self) -> &'static str {
        match self {
            Self::Launchd => ".plist",
            Self::Systemd => ".service",
        }
    }
    /// The folder it reads jobs from under `home`: the LaunchAgents
    /// folder, or the user manager's unit folder.
    pub(crate) fn jobs(self, home: &Path) -> PathBuf {
        match self {
            Self::Launchd => home.join("Library/LaunchAgents"),
            // Where the user manager reads units it is not given a path for.
            Self::Systemd => xdg("XDG_CONFIG_HOME", home, ".config").join("systemd/user"),
        }
    }
}

/// Where triggers live, `~/.agent/triggers`: `jobs` holds their
/// definitions, and `state` what each one's fires did, their queues of
/// asks, and the scheduler's own files.
#[derive(Clone)]
pub struct Places {
    pub jobs: PathBuf,
    pub state: PathBuf,
}

impl Places {
    pub fn home() -> Result<Self, String> {
        let home = std::env::var_os("HOME").ok_or("no HOME for triggers")?;
        Ok(Self::under(Path::new(&home)))
    }
    /// Under the HOME `home`.
    pub(crate) fn under(home: &Path) -> Self {
        let state = home.join(".agent/triggers");
        Self {
            jobs: state.join("jobs"),
            state,
        }
    }
    /// The trigger's definition.
    pub(crate) fn job(&self, name: &str) -> PathBuf {
        self.jobs.join(format!("{name}.json"))
    }
    fn last(&self, name: &str) -> PathBuf {
        self.state.join(format!("{name}.json"))
    }
    /// The trigger's asks: the scheduler runs its fire while any is there,
    /// one fire at a time, and again after one that ends with an ask still
    /// there.
    pub(crate) fn asks(&self, name: &str) -> PathBuf {
        self.state.join(format!("{name}.asks"))
    }
    /// The ask a fire took out of the queue, until it is done with it.
    pub(crate) fn taking(&self, name: &str) -> PathBuf {
        taking(&self.asks(name))
    }
    /// The turn an ask from `fire NAME` sent, for the `fire` waiting on it:
    /// beside the trigger's files, so a trigger that ends with that send
    /// does not take it. `fire` removes it, and Sonny's start one left.
    fn receipt(&self, name: &str, file: &str) -> PathBuf {
        self.state.join(format!(".fired.{name}.{file}"))
    }
    /// The scheduler's doorbell, a FIFO it listens on: a byte written
    /// there has it look again now.
    pub(crate) fn doorbell(&self) -> PathBuf {
        self.state.join(".ring")
    }
    /// What a fire of the trigger holds a lock on while it runs: a plain
    /// file, since not every system locks a folder or a FIFO.
    pub(crate) fn running(&self, name: &str) -> PathBuf {
        self.locks().join(format!("{name}.lock"))
    }
    /// What the one scheduler of this HOME holds a lock on.
    pub(crate) fn scheduling(&self) -> PathBuf {
        self.locks().join("scheduler")
    }
    /// Up to when the scheduler looked at each trigger's calendar.
    pub(crate) fn looked(&self) -> PathBuf {
        self.locks().join("looked")
    }
    /// The scheduler's own folder, which [`finish`] leaves, so neither a
    /// lock nor a write of its own under way is taken from it.
    fn locks(&self) -> PathBuf {
        self.state.join(".scheduler")
    }
}

/// A HOME's tag in the labels of jobs Sonny gives the system: eight hex
/// digits of a 64-bit FNV-1a hash of its path, folded to 32 bits, with any
/// trailing `/` left out. launchd keeps one domain per user, and systemd
/// one manager, whatever HOME is: a test or eval HOME's job sits beside the
/// user's own, under a label of its own.
pub(crate) fn home_tag(home: &std::ffi::OsStr) -> String {
    use std::os::unix::ffi::OsStrExt;
    let bytes = home.as_bytes();
    let end = bytes
        .iter()
        .rposition(|&b| b != b'/')
        .map_or(1, |at| at + 1);
    let hash = bytes[..end.min(bytes.len())]
        .iter()
        .fold(0xcbf2_9ce4_8422_2325_u64, |hash, &b| {
            (hash ^ u64::from(b)).wrapping_mul(0x0100_0000_01b3)
        });
    format!("{:08x}", (hash ^ (hash >> 32)) as u32)
}

/// An XDG base folder: its variable when set to a full path, else its
/// place under HOME.
pub(crate) fn xdg(key: &str, home: &Path, under: &str) -> PathBuf {
    std::env::var_os(key)
        .map(PathBuf::from)
        .filter(|dir| dir.is_absolute())
        .unwrap_or_else(|| home.join(under))
}

/// What is told once a trigger's definition changed: Sonny's scheduler,
/// made sure of and woken ([`ring`]). An error is a scheduler that cannot
/// run here, and the change is undone. Tests stand in for it.
pub type Ring<'a> = &'a dyn Fn() -> Result<(), String>;

/// Sonny's scheduler for this HOME, set up when it is not and woken.
pub fn ring() -> Result<(), String> {
    crate::scheduler::ring()
}

/// Wake the scheduler of `places` when it runs: a byte on its doorbell.
/// Whether one was listening.
pub(crate) fn poke(places: &Places) -> bool {
    use std::io::Write;
    use std::os::unix::fs::OpenOptionsExt;
    // No reader is ENXIO at once, never a wait.
    std::fs::OpenOptions::new()
        .write(true)
        .custom_flags(libc::O_NONBLOCK)
        .open(places.doorbell())
        .and_then(|mut fifo| match fifo.write(&[1]) {
            // A full pipe already has the scheduler's attention.
            Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => Ok(1),
            done => done,
        })
        .is_ok()
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
pub(crate) fn epoch_of(at: Local) -> Option<i64> {
    mktime(at, -1)
}

/// Every moment a local time names, earliest first: two for a time the
/// clock goes through twice as daylight saving ends, none for one it
/// skips.
pub(crate) fn epochs_of(at: Local) -> Vec<i64> {
    let mut found: Vec<i64> = [0, 1]
        .into_iter()
        .filter_map(|dst| mktime(at, dst))
        .collect();
    found.sort_unstable();
    found.dedup();
    found
}

/// `at` as a moment, with daylight saving as `dst` says (-1: as the zone
/// has it), when it names that local time.
fn mktime(at: Local, dst: i32) -> Option<i64> {
    // SAFETY: mktime reads and normalizes the struct it is given.
    let epoch = unsafe {
        let mut tm: libc::tm = std::mem::zeroed();
        tm.tm_year = at.year - 1900;
        tm.tm_mon = at.month as i32 - 1;
        tm.tm_mday = at.day as i32;
        tm.tm_hour = at.hour as i32;
        tm.tm_min = at.minute as i32;
        tm.tm_isdst = dst;
        libc::mktime(&mut tm)
    };
    (epoch != -1 && local(epoch as i64) == at).then_some(epoch as i64)
}

pub(crate) fn now() -> i64 {
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

impl Entry {
    /// Its fields, `{"hour": 9, "minute": 0}`: one left out is any value.
    fn fields(&self) -> [(&'static str, Option<u8>); 5] {
        [
            ("month", self.month),
            ("day", self.day),
            ("weekday", self.weekday),
            ("hour", self.hour),
            ("minute", self.minute),
        ]
    }
    fn json(&self) -> Value {
        Value::Object(
            self.fields()
                .into_iter()
                .filter_map(|(key, value)| Some((key.to_owned(), json!(value?))))
                .collect(),
        )
    }
    fn of(value: &Value) -> Option<Self> {
        let field = |key: &str| match &value[key] {
            Value::Null => Some(None),
            v => v.as_u64().and_then(|n| u8::try_from(n).ok()).map(Some),
        };
        value.is_object().then_some(())?;
        Some(Self {
            month: field("month")?,
            day: field("day")?,
            weekday: field("weekday")?,
            hour: field("hour")?,
            minute: field("minute")?,
        })
    }
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

pub(crate) fn escape(text: &str) -> String {
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

/// A path a trigger can watch: one with a control character, or ending in
/// a space or a backslash, reads back wrong wherever it is shown or
/// quoted (a systemd unit, as triggers once were, trims it or runs it on
/// into the next line), so it is refused on every system alike.
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

/// A trigger's definition, as the scheduler reads it: the program its
/// fire runs, the environment it runs with besides the scheduler's own
/// (the shell whose login environment starts a daemon with your keys, as
/// Sonny starts one), its calendar entries and the path it watches.
/// Without entries or a path, the scheduler runs it only when asked
/// (`fire`): an ask is a file in its queue, `NAME.asks`.
pub fn definition(
    app: &Path,
    trigger: &Trigger,
    when: &When,
    environment: &[(&str, String)],
) -> String {
    let program: Vec<String> = std::iter::once(app.to_string_lossy().into_owned())
        .chain(trigger.args())
        .collect();
    let environment: serde_json::Map<String, Value> = environment
        .iter()
        .map(|(key, value)| ((*key).to_owned(), json!(value)))
        .collect();
    let mut job = json!({
        "program": program,
        "environment": environment,
        "calendar": when.entries.iter().map(Entry::json).collect::<Vec<_>>(),
    });
    if let Some(path) = &when.watch {
        job["watch"] = json!(path.to_string_lossy());
    }
    job.to_string() + "\n"
}

/// A definition read back, as the scheduler runs it.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct Job {
    pub program: Vec<String>,
    pub environment: Vec<(String, String)>,
    pub calendar: Vec<Entry>,
    pub watch: Option<PathBuf>,
}

impl Job {
    pub(crate) fn read(text: &str) -> Option<Self> {
        let value: Value = serde_json::from_str(text).ok()?;
        let program = value["program"]
            .as_array()?
            .iter()
            .map(|arg| arg.as_str().map(str::to_owned))
            .collect::<Option<Vec<_>>>()?;
        let environment = match &value["environment"] {
            Value::Object(map) => map
                .iter()
                .map(|(key, value)| Some((key.clone(), value.as_str()?.to_owned())))
                .collect::<Option<Vec<_>>>()?,
            Value::Null => Vec::new(),
            _ => return None,
        };
        let calendar = match &value["calendar"] {
            Value::Array(entries) => entries.iter().map(Entry::of).collect::<Option<Vec<_>>>()?,
            Value::Null => Vec::new(),
            _ => return None,
        };
        let watch = match &value["watch"] {
            Value::String(path) => Some(PathBuf::from(path)),
            Value::Null => None,
            _ => return None,
        };
        (!program.is_empty()).then_some(Self {
            program,
            environment,
            calendar,
            watch,
        })
    }
}

/// The definition, its program changed by `change`; none when it is not one.
fn with_program(text: &str, change: impl FnOnce(&mut Vec<Value>)) -> Option<String> {
    let mut value: Value = serde_json::from_str(text).ok()?;
    change(value["program"].as_array_mut()?);
    Some(value.to_string() + "\n")
}

/// Whether a definition runs its fires with this environment and no other.
fn runs_with(text: &str, environment: &[(&str, String)]) -> bool {
    Job::read(text).is_some_and(|job| {
        job.environment.len() == environment.len()
            && environment
                .iter()
                .all(|(key, value)| job.environment.iter().any(|(k, v)| k == key && v == value))
    })
}

/// The folder beside a queue that holds the ask a fire took.
fn taking(asks: &Path) -> PathBuf {
    asks.with_extension("taking")
}

/// Make a trigger's queue folders, which the scheduler watches.
fn queues(asks: &Path) -> Result<(), String> {
    [asks.to_owned(), taking(asks)].iter().try_for_each(|dir| {
        std::fs::create_dir_all(dir).map_err(|e| format!("{}: {e}", dir.display()))
    })
}

/// A definition's program arguments.
fn program(text: &str) -> Option<Vec<String>> {
    Job::read(text).map(|job| job.program)
}

/// The path a definition has the scheduler watch.
fn watched(text: &str) -> Option<PathBuf> {
    Job::read(text)?.watch
}

/// The trigger a definition runs, and the Sonny it runs it with.
fn read_job(text: &str) -> Option<(Trigger, PathBuf)> {
    trigger_of(&program(text)?)
}

/// The trigger a program runs, and the Sonny it runs it with.
pub(crate) fn trigger_of(program: &[String]) -> Option<(Trigger, PathBuf)> {
    let (app, rest) = program.split_first()?;
    let trigger = Trigger::parse(rest.strip_prefix(&[FIRE_FLAG.to_owned()])?).ok()?;
    Some((trigger, PathBuf::from(app)))
}

/// The definition with the Sonny it runs moved from `was` to `app`.
fn with_app(text: &str, was: &Path, app: &Path) -> String {
    with_program(text, |program| {
        if program.first().and_then(Value::as_str) == Some(&*was.to_string_lossy()) {
            program[0] = json!(app.to_string_lossy());
        }
    })
    .unwrap_or_else(|| text.to_owned())
}

/// One word of a systemd unit's command or environment, as systemd reads
/// it back: C escapes inside double quotes, `%` doubled for its specifiers
/// and, in a command, `$` doubled for its variables.
pub(crate) fn quoted(text: &str, command: bool) -> String {
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

/// Walk one definition at a time when refreshing Sonny's path.
fn read_all(places: &Places) -> impl Iterator<Item = (String, Read)> {
    std::fs::read_dir(&places.jobs)
        .into_iter()
        .flatten()
        .flatten()
        .filter_map(move |e| {
            let file = e.file_name().into_string().ok()?;
            let name = file.strip_suffix(".json")?.to_owned();
            valid_name(&name).ok()?;
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
    for dir in [&places.jobs, &places.state] {
        for e in std::fs::read_dir(dir).into_iter().flatten().flatten() {
            let file = e.file_name();
            let Some(name) = file.to_str().and_then(|f| f.strip_suffix(".json")) else {
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

/// A trigger as the sky shows it: the agent it messages, how it was
/// asked for, and, when a clock sets it, its next two times (Unix
/// seconds), so how near the next is can be told against the wait.
#[derive(Debug, Clone, PartialEq)]
pub struct Upcoming {
    pub name: String,
    pub generation: String,
    pub bot: String,
    pub when: String,
    pub next: Option<i64>,
    pub then: Option<i64>,
}

/// The triggers of the daemon serving `store_id`, as of `now`: one that
/// watches a file or a repository has no next time, nor a one-off whose
/// time has gone.
pub fn upcoming(places: &Places, store_id: &str, now: i64) -> Vec<Upcoming> {
    let mut found: Vec<Upcoming> = triggers(places)
        .into_iter()
        .filter(|(t, _)| t.store_id == store_id)
        .map(|(t, _)| {
            let (next, then) = match t.at {
                Some(at) => ((at > now).then_some(at), None),
                None => {
                    let entries = when_again(&t, None).entries;
                    let from = t.not_before.map_or(now, |first| now.max(first - 1));
                    let next = crate::scheduler::next_after(&entries, from);
                    (
                        next,
                        next.and_then(|next| crate::scheduler::next_after(&entries, next)),
                    )
                }
            };
            Upcoming {
                bot: t.bot().to_owned(),
                name: t.name,
                generation: t.generation,
                when: t.when,
                next,
                then,
            }
        })
        .collect();
    found.sort_by(|a, b| {
        (a.next.is_none(), a.next, &a.name).cmp(&(b.next.is_none(), b.next, &b.name))
    });
    found
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
            // Whose it is, as an ended one's row says too: its failures
            // are told to them.
            if let Target::Start {
                by: Some((by, by_id)),
                ..
            } = &t.target
            {
                (row["by"], row["by_id"]) = (json!(by), json!(by_id));
                bots.push(by.clone());
            }
            if let Some((to, _)) = &t.reply_to {
                bots.push(to.clone());
            }
            (row, bots)
        })
        .collect();
    // One that ended keeps its row when it has something to say, as a
    // one-shot that did not deliver or one stopped by an error.
    for entry in std::fs::read_dir(&places.state)
        .into_iter()
        .flatten()
        .flatten()
    {
        let file = entry.file_name();
        let Some(name) = file.to_str().and_then(|f| f.strip_suffix(".json")) else {
            continue;
        };
        if valid_name(name).is_err()
            || places.job(name).exists()
            || found.iter().any(|(row, _)| row["name"] == name)
        {
            continue;
        }
        let Some(mut row) = read_json(&places.last(name))
            .filter(|row| row["name"] == name && row["store_id"] == store_id)
        else {
            continue;
        };
        row["ended"] = json!(true);
        let bots: Vec<String> = ["bot", "by", "reply_to"]
            .iter()
            .filter_map(|key| row[*key].as_str().map(str::to_owned))
            .collect();
        found.push((row, bots));
    }
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

/// Write the trigger's definition and ring the scheduler. A trigger of that
/// name already there is left as it is: the same one is returned, to say
/// so, and another is `trigger_exists`, naming the first field that differs.
pub fn install(
    places: &Places,
    app: &Path,
    trigger: &Trigger,
    when: &When,
    environment: &[(&str, String)],
    replace: bool,
    ring: Ring,
) -> Result<Option<Trigger>, String> {
    valid_name(&trigger.name)?;
    let _lock = Lock::take(places)?;
    // macOS folders ignore case: `Build` would overwrite `build`'s files,
    // its definition or an ended one's last result.
    let taken = |dir: &Path, file: &str| {
        std::fs::read_dir(dir)
            .into_iter()
            .flatten()
            .flatten()
            .filter_map(|e| e.file_name().into_string().ok())
            .find(|f| f != file && f.eq_ignore_ascii_case(file))
            .map(|f| f[..f.len() - ".json".len()].to_owned())
    };
    let file = format!("{}.json", trigger.name);
    if let Some(other) = taken(&places.jobs, &file).or_else(|| taken(&places.state, &file)) {
        return Err(format!(
            "name_taken: {}: trigger {other} differs only in case; pass --name",
            trigger.name
        ));
    }
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
                    // folder. The scheduler is made to watch the one it is
                    // now, and to run it with the environment it is added
                    // with now.
                    if watched(&text) != when.watch || !runs_with(&text, environment) {
                        let want = definition(&there_app, &there, when, environment);
                        swap(&path, Some(&text), &want, ring)?;
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
                    queues(&places.asks(&trigger.name))?;
                    let want = definition(app, trigger, when, environment);
                    swap(&path, Some(&text), &want, ring)?;
                    // Without what it keeps the new one would start over:
                    // the old one goes back in.
                    if let Err(error) = write_row(places, trigger, &durable["last"], &kept) {
                        let _ = swap(&path, Some(&want), &text, ring);
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
    queues(&places.asks(&trigger.name))?;
    std::fs::create_dir_all(&places.jobs).map_err(|e| format!("{}: {e}", places.jobs.display()))?;
    swap(
        &path,
        None,
        &definition(app, trigger, when, environment),
        ring,
    )?;
    Ok(None)
}

/// Put `text` in the definition at `path` in place of `old` and ring the
/// scheduler: when it cannot run here, the old definition is written back,
/// or the new one goes.
fn swap(path: &Path, old: Option<&str>, text: &str, ring: Ring) -> Result<(), String> {
    // Check the serialized bytes once, before touching the job or its files.
    if text.len() as u64 > MAX_RECORD {
        return Err(format!(
            "invalid_trigger: its definition exceeds {MAX_RECORD} bytes; narrow --cron or shorten the message"
        ));
    }
    replace(path, text)?;
    if let Err(error) = ring() {
        match old {
            Some(old) => {
                let _ = replace(path, old);
            }
            None => {
                let _ = forget(path);
            }
        }
        return Err(error);
    }
    Ok(())
}

/// Held while a trigger's files change, so `add`, `rm`, a fire ending its
/// trigger and Sonny's refresh never interleave. The lock goes with the
/// process.
struct Lock {
    _held: std::fs::File,
}

impl Lock {
    fn take(places: &Places) -> Result<Self, String> {
        Self::flock(places, libc::LOCK_EX)
    }

    /// The lock now, or an error when another holds it, for work that can
    /// wait for a later turn rather than hold up who asked.
    fn try_take(places: &Places) -> Result<Self, String> {
        Self::flock(places, libc::LOCK_EX | libc::LOCK_NB)
    }

    fn flock(places: &Places, how: libc::c_int) -> Result<Self, String> {
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
        if unsafe { libc::flock(file.as_raw_fd(), how) } != 0 {
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

/// Remove a trigger, whatever is left of it: its definition, its last
/// result and its queue.
pub fn remove(places: &Places, name: &str) -> Result<(), String> {
    valid_name(name)?;
    let _lock = Lock::take(places)?;
    match retire(places, name, false)? {
        true => Ok(()),
        false => Err(format!("trigger_not_found: {name}")),
    }
}

/// `fire NAME`: an ask, which the scheduler runs the trigger's fire for.
/// While a fire of it runs it starts no other; it runs it again once that
/// one is done, for the ask it left. The ask's name, which what comes of
/// it carries.
pub fn fire_now(places: &Places, name: &str, ring: Ring) -> Result<String, String> {
    valid_name(name)?;
    let file = {
        let _lock = Lock::take(places)?;
        if !stored(&places.job(name)) {
            return Err(format!("trigger_not_found: {name}"));
        }
        ask(places, name, Ask::Fire)?
    };
    // A scheduler not running yet is started, and finds the ask.
    ring()?;
    Ok(file)
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
    poke(places);
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
/// under the lock, and only while the definition is still this trigger's.
/// One replaced or removed while its message went out is left as it now
/// is; what is left of one whose definition went is removed. Whether that
/// is all done: false when what it did is not on disk, or the trigger it
/// ends is still there.
fn settle(places: &Places, trigger: &Trigger, outcome: &Value, kept: &Kept) -> bool {
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
            return retire(places, &trigger.name, true).map_or_else(log, |_| true);
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
    // One that did not deliver ends only once why is on disk; else its
    // definition stays, listed. Its row stays whatever came of it: one that sent keeps
    // it until its turn is known to have ended well (`forget_done`), so a
    // turn that fails after the send still shows.
    if over && (sent || recorded.is_ok()) {
        return retire(places, &trigger.name, true).map_or_else(log, |_| true);
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
/// its definition first, so the scheduler starts no fire of it, then its
/// queue and, unless `keep`, its last result, so an end nobody asked for
/// still shows, and why. A fire of it that runs as it goes runs to its
/// end. Whether any of it was there. A folder that ignores case finds
/// `build`'s files for `Build`: only the name as stored is that trigger.
fn retire(places: &Places, name: &str, keep: bool) -> Result<bool, String> {
    let [job, last, asks, taking] = its_files(places, name);
    let mut here = false;
    for (path, go) in [(&job, true), (&last, !keep), (&asks, true), (&taking, true)] {
        if !(go && stored(path)) {
            continue;
        }
        here = true;
        match std::fs::symlink_metadata(path) {
            Ok(meta) if meta.is_dir() => std::fs::remove_dir_all(path)
                .map_err(|e| format!("{}: {e}", path.display()))
                .and_then(|()| synced(path)),
            _ => forget(path),
        }?;
    }
    if here {
        // A fire of it still running keeps its lock on the file it opened.
        let _ = std::fs::remove_file(places.running(name));
        poke(places);
    }
    Ok(here)
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

/// The scheduler's own files in the triggers' folder, which [`finish`]
/// leaves: its doorbell, and the folder of its locks and what it keeps
/// across its restarts, where its writes are.
pub(crate) const SCHEDULER_FILES: [&str; 2] = [".ring", ".scheduler"];

/// At Sonny's start, under the lock, finish what was cut short: a write
/// that died before its rename, whose temporary nothing else would remove,
/// and what an end left set aside before triggers moved to Sonny's
/// scheduler. Every write here is under the lock, so no other is under way.
pub fn finish(places: &Places) {
    let Ok(_lock) = Lock::take(places) else {
        return;
    };
    for dir in [&places.state, &places.jobs] {
        let dots = std::fs::read_dir(dir)
            .into_iter()
            .flatten()
            .flatten()
            .filter(|e| {
                let file = e.file_name();
                let file = file.to_string_lossy();
                file.starts_with('.') && file != ".lock" && !SCHEDULER_FILES.contains(&&*file)
            });
        for entry in dots {
            let path = entry.path();
            let gone = match entry.file_type() {
                Ok(kind) if kind.is_dir() => {
                    std::fs::remove_dir_all(&path).map_err(|e| format!("{}: {e}", path.display()))
                }
                _ => forget(&path),
            };
            if let Err(error) = gone {
                eprintln!("{}", error_json(&error));
            }
        }
    }
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
pub fn refresh(places: &Places, app: &Path) {
    rewrite(places, |_, text| {
        let was = PathBuf::from(program(text)?.into_iter().next()?);
        (was != app).then(|| with_app(text, &was, app))
    });
}

/// A trigger made before triggers named their store by its lineage names
/// it by its file's identity, which a replaced file does not keep: each
/// that names `identity`, the daemon's now, names `lineage` from here on.
pub fn restamp(places: &Places, identity: &str, lineage: &str) {
    rewrite(places, |trigger, text| {
        (trigger.store_id == identity).then_some(())?;
        with_program(text, |program| {
            if let Some(at) = program.iter().position(|arg| arg == "--store-id")
                && program.get(at + 1).and_then(Value::as_str) == Some(identity)
            {
                program[at + 1] = json!(lineage);
            }
        })
        .filter(|stamped| stamped != text)
    });
}

/// Each definition `change` says another text for, written in its place,
/// one at a time under the lock, read again under it, as an `rm`, `add` or
/// fire may have come first.
fn rewrite(places: &Places, change: impl Fn(&Trigger, &str) -> Option<String>) {
    let mut changed = false;
    for (name, _) in read_all(places).filter(|(_, r)| r.is_ok()) {
        let Ok(_lock) = Lock::take(places) else {
            break;
        };
        let path = places.job(&name);
        let Ok(text) = std::fs::read_to_string(&path) else {
            continue;
        };
        let Some(want) = read_job(&text).and_then(|(trigger, _)| change(&trigger, &text)) else {
            continue;
        };
        match replace(&path, &want) {
            Ok(()) => changed = true,
            Err(error) => eprintln!("{}", error_json(&error)),
        }
    }
    if changed {
        poke(places);
    }
}

/// Once Sonny knows its daemon: [`restamp`] the triggers made on its store.
pub fn restamp_at_start(identity: Option<&str>, lineage: Option<&str>) {
    let (Ok(places), Some(identity), Some(lineage)) = (Places::home(), identity, lineage) else {
        return;
    };
    let (identity, lineage) = (identity.to_owned(), lineage.to_owned());
    std::thread::spawn(move || restamp(&places, &identity, &lineage));
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

/// What a trigger's fire, and Sonny's scheduler, start with besides the
/// system's own: the shell whose login environment starts a daemon with
/// your keys, as Sonny starts one.
pub(crate) fn environment() -> Vec<(&'static str, String)> {
    // PATH too: an `--if` command runs with the adder's, as typed there.
    let mut keys = vec!["HOME", "SHELL", "PATH"];
    // Where systemd's unit folder is, which a fire finds as `add` did.
    if Scheduler::here() == Scheduler::Systemd {
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

/// At Sonny's start, off its way: finish what was cut short, carry over
/// triggers made when each was a job of the system's own, point every
/// trigger at this Sonny when it moved, and see that the scheduler runs
/// this Sonny when there is anything for it.
pub fn at_start() {
    let (Ok(places), Some(sonny)) = (Places::home(), crate::skills::sonny_path()) else {
        return;
    };
    std::thread::spawn(move || {
        finish(&places);
        if let Some(home) = std::env::var_os("HOME").map(PathBuf::from) {
            let record = home.join(".agent/sonny/migrated");
            crate::upgrade::once(&record, "scheduler-job", || {
                let scheduler = Scheduler::here();
                let let_go =
                    |dir: &Path, label: &str| crate::scheduler::let_go(scheduler, dir, label);
                let moved = migrate(&places, &home, scheduler, &ring, &let_go);
                crate::scheduler::retire_old(crate::boot::OLD_LABEL);
                crate::scheduler::retire_old(crate::home::OLD_LABEL);
                moved
            });
        }
        refresh(&places, &sonny);
        crate::scheduler::at_start();
    });
}

/// Each trigger made when it was a job of the system's own (a LaunchAgent
/// or a user service of its own, under [`LABEL`]) becomes a definition for
/// the scheduler, and the system lets go of its job. Only one made from
/// `home` moves: its tag is `home`'s, or, from before labels had a tag, it
/// runs with `home` as HOME; systemd's unit folder can be one that several
/// HOMEs share, and another HOME's trigger is that HOME's to move, at its
/// own Sonny's start. One that cannot be read is left as it is. `let_go`
/// asks the system to let go of an old job, given its folder and label.
/// Whether every one this HOME's start could move moved: one that failed is
/// tried again at the next start.
pub fn migrate(
    places: &Places,
    home: &Path,
    scheduler: Scheduler,
    ring: Ring,
    let_go: &dyn Fn(&Path, &str),
) -> bool {
    let dir = scheduler.jobs(home);
    let tag = home_tag(home.as_os_str());
    let suffix = scheduler.suffix();
    let old: Vec<(String, String)> = std::fs::read_dir(&dir)
        .into_iter()
        .flatten()
        .flatten()
        .filter_map(|e| e.file_name().into_string().ok())
        .filter_map(|f| {
            let rest = f.strip_prefix(LABEL)?.strip_suffix(suffix)?;
            let name = match rest.strip_prefix(&format!("{tag}.")) {
                Some(name) => name,
                None if !tagged(rest) => rest,
                None => return None,
            };
            valid_name(name).ok()?;
            Some((format!("{LABEL}{rest}"), name.to_owned()))
        })
        .collect();
    let (mut moved, mut all) = (false, true);
    for (label, name) in old {
        match migrated(places, home, scheduler, &dir, &label, &name, let_go) {
            Ok(done) => moved |= done,
            Err(error) => {
                all = false;
                eprintln!("{}", error_json(&error));
            }
        }
    }
    if moved && let Err(error) = ring() {
        eprintln!("{}", error_json(&error));
    }
    all
}

/// Whether a label, past [`LABEL`], starts with a HOME's tag.
fn tagged(rest: &str) -> bool {
    let bytes = rest.as_bytes();
    bytes.get(8) == Some(&b'.')
        && bytes[..8]
            .iter()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(b))
}

/// Under the lock, move one trigger's old job to its definition: written
/// unless a definition of the name is there already, then the old job let
/// go of and its file removed. Whether it moved.
fn migrated(
    places: &Places,
    home: &Path,
    scheduler: Scheduler,
    dir: &Path,
    label: &str,
    name: &str,
    let_go: &dyn Fn(&Path, &str),
) -> Result<bool, String> {
    let _lock = Lock::take(places)?;
    let old = dir.join(format!("{label}{}", scheduler.suffix()));
    let text = match read_record(&old) {
        Ok(text) => text,
        Err(_) if !old.exists() => return Ok(false),
        Err(error) => return Err(format!("{}: {error}", old.display())),
    };
    let plist = text.trim_start().starts_with('<');
    let Some((trigger, app)) = old_program(&text, plist).and_then(|args| trigger_of(&args)) else {
        return Ok(false);
    };
    // A label from before tags: only a trigger made from this HOME.
    let home = home.to_string_lossy();
    let from_home = match old_environment(&text, plist)
        .into_iter()
        .find(|(k, _)| k == "HOME")
    {
        Some((_, value)) => value == home,
        None => true,
    };
    if !label.ends_with(&format!(
        ".{}.{name}",
        home_tag(std::ffi::OsStr::new(&*home))
    )) && !from_home
    {
        return Ok(false);
    }
    if !stored(&places.job(name)) {
        let environment = old_environment(&text, plist);
        let environment: Vec<(&str, String)> = environment
            .iter()
            .map(|(k, v)| (k.as_str(), v.clone()))
            .collect();
        let when = when_again(&trigger, old_watched(&text, plist));
        queues(&places.asks(name))?;
        replace(
            &places.job(name),
            &definition(&app, &trigger, &when, &environment),
        )?;
    }
    let_go(dir, label);
    forget(&old)?;
    Ok(true)
}

/// An old job's program arguments: a plist's, or a unit's command.
fn old_program(text: &str, plist: bool) -> Option<Vec<String>> {
    if !plist {
        let line = text.lines().find_map(|l| l.strip_prefix("ExecStart="))?;
        return words(line);
    }
    let rest = &text[text.find("<key>ProgramArguments</key>")?..];
    let array = &rest[rest.find("<array>")? + "<array>".len()..rest.find("</array>")?];
    Some(plist_strings(array))
}

/// Every `<string>` in a plist's piece, unescaped.
fn plist_strings(mut at: &str) -> Vec<String> {
    let mut found = Vec::new();
    while let Some(start) = at.find("<string>") {
        let body = &at[start + "<string>".len()..];
        let Some(end) = body.find("</string>") else {
            break;
        };
        found.push(unescape(&body[..end]));
        at = &body[end + "</string>".len()..];
    }
    found
}

/// An old job's environment.
fn old_environment(text: &str, plist: bool) -> Vec<(String, String)> {
    if !plist {
        return text
            .lines()
            .filter_map(|l| words(l.strip_prefix("Environment=")?)?.pop())
            .filter_map(|pair| {
                let (key, value) = pair.split_once('=')?;
                Some((key.to_owned(), value.to_owned()))
            })
            .collect();
    }
    // Each `<key>K</key><string>V</string>` on a line of its own, as Sonny
    // wrote them.
    let Some(at) = text.find("<key>EnvironmentVariables</key>") else {
        return Vec::new();
    };
    let rest = &text[at..];
    rest[..rest.find("</dict>").unwrap_or(rest.len())]
        .lines()
        .filter_map(|line| {
            let key = line.trim().strip_prefix("<key>")?;
            let (key, value) = key.split_once("</key>")?;
            Some((key.to_owned(), plist_strings(value).into_iter().next()?))
        })
        .collect()
}

/// The path an old job had its scheduler watch.
fn old_watched(text: &str, plist: bool) -> Option<PathBuf> {
    if !plist {
        let at = text.find("\n[X-Path]\n")?;
        let line = text[at..]
            .lines()
            .find_map(|l| l.strip_prefix("PathChanged="))?;
        return Some(PathBuf::from(line.replace("%%", "%")));
    }
    let rest = &text[text.find("<key>WatchPaths</key>")?..];
    plist_strings(&rest[..rest.find("</array>")?])
        .into_iter()
        .next()
        .map(PathBuf::from)
}

/// A trigger's calendar again, from what it carries: a one-off's time, an
/// interval counted from the minute it was made (its first time less one
/// interval), a cron expression; and the path it watched.
fn when_again(trigger: &Trigger, watch: Option<PathBuf>) -> When {
    let entries = if let Some(at) = trigger.at {
        let at = local(at);
        vec![Entry {
            month: Some(at.month),
            day: Some(at.day),
            hour: Some(at.hour),
            minute: Some(at.minute),
            weekday: None,
        }]
    } else if let Some(value) = trigger.when.strip_prefix("every ") {
        let interval = amount(value).map_or(0, |(n, unit)| {
            i64::from(n)
                * match unit {
                    'm' => 60,
                    'h' => 3600,
                    _ => 86400,
                }
        });
        let made = trigger
            .not_before
            .map_or_else(now, |first| first - interval - 1);
        every(value, made).map(|w| w.entries).unwrap_or_default()
    } else if let Some(expression) = trigger.when.strip_prefix("cron ") {
        cron(expression).map(|w| w.entries).unwrap_or_default()
    } else {
        Vec::new()
    };
    When {
        text: trigger.when.clone(),
        entries,
        not_before: trigger.not_before,
        at: trigger.at,
        watch,
    }
}

/// Whether HOME is gone: the trigger's job was added from a HOME deleted
/// since, such as a test's or an eval's, and has nothing left to fire into.
fn orphaned() -> bool {
    std::env::var_os("HOME").is_some_and(|home| {
        matches!(std::fs::symlink_metadata(home), Err(e) if e.kind() == std::io::ErrorKind::NotFound)
    })
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
            remove(&places, name).map(|()| json!({"removed": name}))
        }
        Some((verb, [name])) if verb == "show" => Ok(one(&places, name).unwrap_or(Value::Null)),
        Some((verb, [name])) if verb == "fire" => {
            fire_now(&places, name, &ring).map(|file| fired(&places, name, &file, FIRE_WAIT))
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
        return remove(&places, name).map(|()| json!({"removed": name}));
    }
    if let Some(name) = named("fire") {
        return fire_now(&places, name, &ring).map(|file| fired(&places, name, &file, FIRE_WAIT));
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
    let environment = environment();
    match install(
        places,
        &app,
        &trigger,
        &asked.when,
        &environment,
        asked.replace,
        &ring,
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
                let _ = remove(places, &trigger.name);
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
    // Whose it is once its job is gone: a failure it retired with is
    // shown on that daemon's windows only, and on the windows of the
    // agent that added it to start its own.
    row["store_id"] = json!(trigger.store_id);
    if let Target::Start {
        by: Some((by, by_id)),
        ..
    } = &trigger.target
    {
        (row["by"], row["by_id"]) = (json!(by), json!(by_id));
    }
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
    let mut asked = json!({"bot": name, "workspace": dir, "model": model, "effort": effort,
        "instructions": instructions,
        "created_by": by.as_ref().map(|(bot, _)| bot),
        "created_by_id": by.as_ref().map(|(_, id)| id),
        "request_id": format!("trigger-{}", trigger.generation)});
    // As the person's `--new` makes one: Sonny's tools (the memory tool,
    // and `exec` while MCP servers are on, so an agent no one opens reaches
    // the person's chat, mail and the like as one they open does), gated
    // and sandboxed as `auto` confines them. A call the judge doubts waits
    // in `/approvals` for the person, as any agent's does.
    crate::made(client, &mut asked, model, dir)?;
    let made = client.request("create", asked).await.map_err(coded)?;
    made["bot_id"]
        .as_i64()
        .ok_or_else(|| "create: no bot id".into())
}

/// Give the agent a `--start` trigger made before its agents had Sonny's
/// tools those tools, as [`start`] gives them now; the tool host keeps them
/// up to date from then on. One that has any declared tools is left as it
/// is, and a failure leaves it to the next fire. One made before `--start`
/// confined its agents as `--new` does is confined too: it was never given
/// a sandbox, which every mode Sonny sets names, so one the person put in
/// `full` stays there.
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
        record["declared"] = fields["declare"].clone();
        let _ = client.request("configure", Value::Object(fields)).await;
    }
    if unconfined(&record)
        && let Some(fields) = crate::approvals::regate(&record, crate::approvals::default())
    {
        let mut asked = Value::Object(fields);
        asked["bot"] = json!(name);
        asked["bot_id"] = json!(id);
        let _ = client.request("configure", asked).await;
    }
}

/// Whether an agent was made with no sandbox and no gate, as `--start`
/// made them before #404: no mode Sonny sets leaves the sandbox unnamed.
fn unconfined(record: &Value) -> bool {
    record["settings"]["sandbox"].is_null() && crate::approvals::mode(record) == Some("full")
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
/// agent, or one with work waiting, gets it after its work, a repeating
/// trigger's only when no fire of it waits there already. A deleted agent's trigger goes. A
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
    let mut submitted = client.request("submit", asking.clone()).await;
    // A repeating trigger's fire that finds its agent busy waits behind its
    // work, as anacron runs a missed job once: one fire of the trigger
    // waits at a time, so a frequent one does not pile up.
    let mut late = false;
    if let Err(error) = &submitted
        && matches!(error.code.as_str(), "bot_busy" | "active_agent_limit")
        && asking["delivery"] == "reject"
        && waiting_fire(client, trigger, bot).await == Some(false)
    {
        let mut queued = asking.clone();
        queued["delivery"] = json!("queue");
        submitted = client.request("submit", queued).await;
        late = submitted.is_ok();
    }
    let outcome = match submitted {
        Ok(turn) => {
            let mut sent = json!({"outcome": "sent", "turn": turn["turn"],
                "handle": format!("turn:{bot}/{}", turn["turn"])});
            if started {
                sent["started"] = json!(true);
            }
            if late {
                sent["late"] = json!(true);
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
                "delivery": "attach", "origin": "trigger", "wait_ms": attach_wait()}),
        )
        .await;
}

/// How long a trigger's note waits for a call before it goes as a steer:
/// the person's `attach_wait`, a day unless config.toml says otherwise.
pub fn attach_wait() -> Option<u64> {
    agent_client::config::load()
        .unwrap_or_default()
        .attach_wait()
}

/// The generation of the trigger whose fire sent a turn with `request_id`:
/// a fire's own, not the answer it passes on (`reply.`), a missed fire's
/// note (`missed.`), nor one whose answer goes to `--reply-to` (`.to.`),
/// which that answer tells of.
pub fn fire_generation(request_id: &str) -> Option<&str> {
    let (generation, rest) = request_id.strip_prefix("trigger_")?.split_once('_')?;
    let own = !rest.contains(".to.")
        && !["reply.", "missed.", "failed."]
            .iter()
            .any(|note| rest.starts_with(note));
    own.then_some(generation)
}

/// A fire's turn on `bot` that failed (`error`), told to the agent that
/// added the trigger to start `bot`, else to `bot` itself, as a fire that
/// sent nothing is told: the note it reads in its next model call, without
/// starting a turn. The trigger's row, which the daemon at `store_id`
/// keeps, names it. Its request id is the turn's own, so it is told once.
pub async fn tell_failed(
    client: &Client,
    store_id: &str,
    generation: &str,
    (bot, bot_id, turn): (&str, Option<i64>, i64),
    error: &str,
) -> agent_client::Result<()> {
    let places = Places::home().ok();
    let (to, to_id, prompt) = failed_note(
        places.as_ref(),
        store_id,
        generation,
        (bot, bot_id, turn),
        error,
    );
    let mut submit = json!({"bot": to,
        "request_id": format!("trigger_{generation}_failed.{bot}.{turn}"),
        "prompt": prompt, "delivery": "attach", "origin": "trigger", "wait_ms": attach_wait()});
    // By its id too, as every delivery of a trigger's: a name used again
    // is not who it was for.
    if let Some(id) = to_id {
        submit["bot_id"] = json!(id);
    }
    client.request("submit", submit).await.map(drop)
}

/// Who is told of a fire's failed turn, by name and id when known, and the
/// note: the agent that made the trigger, else the one the fire sent.
fn failed_note(
    places: Option<&Places>,
    store_id: &str,
    generation: &str,
    (bot, bot_id, turn): (&str, Option<i64>, i64),
    error: &str,
) -> (String, Option<i64>, String) {
    let row = places.and_then(|places| {
        on_store(places, store_id)
            .into_iter()
            .map(|(row, _)| row)
            .find(|row| row["generation"] == generation)
    });
    let name = (row.as_ref()).and_then(|row| row["name"].as_str());
    let (to, to_id) = match (row.as_ref()).and_then(|row| Some((row["by"].as_str()?, row))) {
        Some((by, row)) => (by, row["by_id"].as_i64()),
        None => (bot, bot_id),
    };
    let prompt = format!(
        "[trigger {} · {} · turn {turn} it sent {bot} failed: {error}]",
        name.unwrap_or("(ended)"),
        stamp(now())
    );
    (to.to_owned(), to_id, prompt)
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

/// Whether a fire of `trigger` already waits behind `bot`'s work; `None`
/// when the agent's turns could not be read. Queued work starts in order,
/// so nothing older than a turn that started still waits: the turns are
/// read newest first, a page at a time, until one that started. Rows that
/// never ran (a steer the running turn took in, a message cancelled while
/// it waited, a note attached to the next turn) are passed over.
async fn waiting_fire(client: &Client, trigger: &Trigger, bot: &str) -> Option<bool> {
    let ours = sent_by(trigger);
    let mut before: Option<i64> = None;
    loop {
        let mut page = json!({"bot": bot, "newest": true, "limit": 256});
        if let Some(before) = before {
            page["before"] = json!(before);
        }
        let read = client.request("turns", page).await.ok()?;
        let turns = read["turns"].as_array()?;
        for turn in turns {
            match turn["status"].as_str()? {
                "running" | "waiting" | "paced" | "completed" | "failed" => return Some(false),
                "queued" | "ready"
                    if (turn["request_id"].as_str()).is_some_and(|id| id.starts_with(&ours)) =>
                {
                    return Some(true);
                }
                _ => {}
            }
        }
        match turns.last().and_then(|turn| turn["turn"].as_i64()) {
            Some(oldest) if turns.len() == 256 => before = Some(oldest),
            _ => return Some(false),
        }
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

/// The rows of triggers that ended, made on the daemon whose store is
/// `store_id`, whose last fire sent a turn that `ended` says completed,
/// with any answer passed on: nothing is left to say, so they go. One whose
/// turn failed stays, shown until it is removed, as systemd keeps a failed
/// unit until `reset-failed` and Kubernetes a finished Job until its TTL.
/// Run by whoever asked the daemon how those turns ended. How many went.
pub fn forget_done(places: &Places, store_id: &str, ended: &HashMap<String, Value>) -> usize {
    let done = |name: &str| {
        let row = read_json(&places.last(name))?;
        let handle = sent(&row)?;
        (row["name"] == name
            && row["store_id"] == store_id
            && row["last"]["reply"]["outcome"] != "failed"
            && ended.get(&handle)?["status"] == "completed"
            && !places.job(name).exists())
        .then_some(handle)
    };
    let names: Vec<String> = (std::fs::read_dir(&places.state)
        .into_iter()
        .flatten()
        .flatten())
    .filter_map(|e| Some(e.file_name().to_str()?.strip_suffix(".json")?.to_owned()))
    .filter(|name| valid_name(name).is_ok() && done(name).is_some())
    .collect();
    if names.is_empty() {
        return 0;
    }
    // Sonny's window asks every ten seconds, from its event loop: it never
    // waits for the lock another holds, and tries again next time.
    let Ok(_lock) = Lock::try_take(places) else {
        return 0;
    };
    // Under the lock, only while it is still that row: a trigger made since
    // under the name has a job, or a row of its own.
    let mut gone = 0;
    for name in names {
        if done(&name).is_none() {
            continue;
        }
        match std::fs::remove_file(places.last(&name)) {
            Ok(()) => gone += 1,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => eprintln!(
                "{}",
                error_json(&format!("{}: {e}", places.last(&name).display()))
            ),
        }
    }
    gone
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

fn answer(result: &Value) -> String {
    match (result["text"].as_str(), &result["error"]) {
        (Some(text), _) if !text.is_empty() => text.to_owned(),
        (_, Value::Null) => String::new(),
        (_, Value::String(error)) => error.clone(),
        (_, error) => error.to_string(),
    }
}

/// `APP --trigger-fire ...`, run by Sonny's scheduler.
pub fn fire_cli(args: &[String]) -> i32 {
    crate::scheduler::keep_lock();
    let (trigger, places) = match Trigger::parse(args).and_then(|t| Ok((t, Places::home()?))) {
        Ok(found) => found,
        Err(error) => {
            eprintln!("{}", error_json(&error));
            return 1;
        }
    };
    // A HOME deleted since, such as a test's or an eval's, has nothing
    // left to fire into: its scheduler lets its own job go.
    if orphaned() {
        eprintln!(
            "{}",
            error_json(&format!("home_gone: {}: its HOME is gone", trigger.name))
        );
        return 0;
    }
    let asked = match take_ask(&places, &trigger) {
        Ok(asked) => asked,
        Err(error) => {
            stops(&places, &trigger, error);
            return 1;
        }
    };
    fire(&places, &trigger, asked.as_ref());
    match asked.as_ref().map(done_with) {
        Some(Err(error)) => {
            stops(&places, &trigger, error);
            1
        }
        _ => 0,
    }
}

/// A trigger that cannot go on ends, saying why: its row stays.
fn stops(places: &Places, trigger: &Trigger, detail: String) {
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
        .and_then(|()| retire(places, &trigger.name, true).map(drop))
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
        return stops(places, trigger, error);
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
        settle(places, trigger, &named(outcome), &kept);
        return;
    }
    // Its last run went out but its end did not (launchd would not unload
    // it): it ends now, sending nothing more.
    if trigger.runs.is_some_and(|runs| kept.sent >= runs) {
        return ends(places, trigger, true);
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
            settle(places, trigger, &missed, &kept);
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
                settle(places, trigger, &declined, &kept);
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
            settle(places, trigger, &failed, &kept);
            return;
        }
    }
    let outcome = with_daemon(trigger, async |client| {
        deliver(client, places, trigger, &mut kept, &why, ask).await
    });
    let (Ok(outcome) | Err(outcome)) = outcome;
    settle(places, trigger, &named(outcome), &kept);
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
        && let Err(error) = retire(places, &trigger.name, keep)
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

    #[tokio::test]
    async fn a_failed_fires_turn_is_a_note_for_its_agent() {
        let asked = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        let seen = asked.clone();
        let client = crate::tests::daemon_answering(move |request| {
            seen.lock().unwrap().push(request.clone());
            json!({})
        })
        .await;
        tell_failed(
            &client,
            "no-such-store",
            "1-2",
            ("calc.lead", Some(5), 4),
            "provider_error",
        )
        .await
        .unwrap();
        let submit = asked.lock().unwrap()[0].clone();
        assert_eq!(submit["bot"], "calc.lead");
        assert_eq!(submit["bot_id"], 5);
        assert_eq!(submit["delivery"], "attach");
        assert_eq!(submit["origin"], "trigger");
        assert_eq!(submit["request_id"], "trigger_1-2_failed.calc.lead.4");
        let prompt = submit["prompt"].as_str().unwrap();
        assert!(prompt.starts_with("[trigger (ended) · "), "{prompt}");
        assert!(prompt.ends_with(" · turn 4 it sent calc.lead failed: provider_error]"));
    }

    #[test]
    fn a_failed_fires_turn_is_told_to_whoever_made_its_installed_trigger() {
        let w = World::new("failed-note");
        let started = Trigger {
            name: "p.digest".into(),
            target: Target::Start {
                name: "p.digest".into(),
                model: "openai/gpt".into(),
                effort: None,
                by: Some(("p.lead".into(), 7)),
            },
            ..trigger()
        };
        w.install(&started).unwrap();
        let (to, to_id, prompt) = failed_note(
            Some(&w.places),
            &started.store_id,
            &started.generation,
            ("p.digest", Some(12), 3),
            "provider_error",
        );
        assert_eq!((to.as_str(), to_id), ("p.lead", Some(7)));
        assert!(prompt.starts_with("[trigger p.digest · "), "{prompt}");
        // One no agent made, or no longer found: the agent the fire sent.
        let (to, to_id, prompt) = failed_note(
            Some(&w.places),
            &started.store_id,
            "9-9",
            ("p.fix-login", Some(12), 3),
            "provider_error",
        );
        assert_eq!((to.as_str(), to_id), ("p.fix-login", Some(12)));
        assert!(prompt.starts_with("[trigger (ended) · "), "{prompt}");
    }

    #[test]
    fn a_fires_own_turn_names_its_trigger_by_generation() {
        assert_eq!(
            fire_generation("trigger_123-456_7.1700.42.0"),
            Some("123-456")
        );
        assert_eq!(fire_generation("trigger_123-456_7.every"), Some("123-456"));
        // Not the fire's own: its answer passed on, notes, `--reply-to`'s.
        for id in [
            "trigger_123-456_reply.9.4",
            "trigger_123-456_missed.7.1.2.3",
            "trigger_123-456_failed.calc.lead.4",
            "trigger_123-456_7.1700.42.0.to.9",
            "tui-123",
        ] {
            assert_eq!(fire_generation(id), None, "{id}");
        }
    }

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
    fn a_definition_carries_everything_a_fire_needs() {
        let s = Trigger {
            // A message about a plist or a unit leaves the definition one.
            message: "100% sure: $HOME is \"home\" \\ not\tthere;\nThen say so. <key>ProgramArguments</key>".into(),
            ..trigger()
        };
        let when = every("30m", clock(2026, 9, 28, 23, 52)).unwrap();
        let app = Path::new("/Applications/Agent.app/Contents/MacOS/agent-app");
        let environment = [("SHELL", "/bin/zsh".to_owned())];
        let text = definition(app, &s, &when, &environment);
        let job = Job::read(&text).unwrap();
        assert_eq!(
            job.calendar,
            [
                Entry {
                    minute: Some(22),
                    ..Entry::default()
                },
                Entry {
                    minute: Some(52),
                    ..Entry::default()
                }
            ]
        );
        assert_eq!(
            job.environment,
            [("SHELL".to_owned(), "/bin/zsh".to_owned())]
        );
        assert_eq!(job.watch, None);
        assert_eq!(watched(&text), None);
        let args = program(&text).unwrap();
        assert_eq!(args[0], app.to_str().unwrap());
        assert_eq!(args[1], FIRE_FLAG);
        assert_eq!(Trigger::parse(&args[2..]).unwrap(), s);
        assert_eq!(read_job(&text).unwrap(), (s.clone(), app.to_owned()));
        assert!(runs_with(&text, &environment) && !runs_with(&text, &[]));
        let bare = definition(app, &s, &fired(), &[]);
        assert!(runs_with(&bare, &[]) && !runs_with(&bare, &environment));
        // Only an ask runs one with no times and no path.
        assert!(Job::read(&bare).unwrap().calendar.is_empty());
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
        let watching = When {
            watch: Some("/r/50% done/.git/logs/HEAD".into()),
            ..fired()
        };
        let text = definition(Path::new("/A/app"), &full, &watching, &[]);
        assert_eq!(watched(&text), watching.watch);
        assert!(Job::read(&text).unwrap().calendar.is_empty());
        assert_eq!(read_job(&text).unwrap().0, full);
        // The app as it moved, and nothing else.
        let moved = with_app(&text, Path::new("/A/app"), Path::new("/new/100%/sonny"));
        assert_eq!(
            read_job(&moved).unwrap(),
            (full.clone(), PathBuf::from("/new/100%/sonny"))
        );
        assert_eq!(
            with_app(&moved, Path::new("/A/app"), Path::new("/x")),
            moved
        );
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
        // Not a definition: none.
        for odd in [
            "",
            "{}",
            "<plist/>",
            r#"{"program": []}"#,
            r#"{"program": ["a"], "calendar": [{"minute": 300}]}"#,
        ] {
            assert_eq!(Job::read(odd), None, "{odd}");
        }
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
    fn only_an_agent_no_mode_was_ever_set_on_is_confined_when_its_trigger_fires() {
        // As `--start` made them before: no sandbox, no gate.
        assert!(unconfined(&json!({"settings": {}, "gates": []})));
        // The person put it in `full`, which names its sandbox.
        let full = json!({"settings": {"sandbox": "danger-full-access"}, "gates": []});
        assert!(!unconfined(&full));
        let auto = json!({"settings": {"sandbox": "workspace-write"},
            "gates": [{"tag": "auto", "tools": ["shell"]}]});
        assert!(!unconfined(&auto));
    }

    /// Sonny's scheduler as triggers see it: how often it was rung, and
    /// whether it cannot run here.
    #[derive(Default)]
    struct Fake {
        rings: std::cell::Cell<usize>,
        refuse: std::cell::Cell<bool>,
    }

    impl Fake {
        fn ring(&self) -> Result<(), String> {
            self.rings.set(self.rings.get() + 1);
            match self.refuse.get() {
                true => Err("triggers_unsupported: no scheduler here".into()),
                false => Ok(()),
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
            Self {
                places: Places::under(&root),
                root,
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
                &|| self.fake.ring(),
            )
        }
        fn settle(&self, s: &Trigger, outcome: Value) {
            let kept = Kept::of(&state(&self.places, s));
            settle(&self.places, s, &outcome, &kept);
        }
        fn remove(&self, name: &str) -> Result<(), String> {
            remove(&self.places, name)
        }
        /// Its definition, and its last result.
        fn state(&self, name: &str) -> (bool, bool) {
            (
                self.places.job(name).exists(),
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
        assert_eq!(found[0].0["by"], "p.lead");
    }

    #[test]
    fn a_trigger_made_by_its_store_file_s_identity_names_its_lineage_instead() {
        let w = World::new("restamp");
        let elsewhere = Trigger {
            name: "q.other".into(),
            store_id: "another-store".into(),
            ..trigger()
        };
        for t in [&trigger(), &elsewhere] {
            w.install(t).unwrap();
        }
        restamp(&w.places, "00ab", "0123456789abcdef");
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
        // Again it changes nothing.
        let text = w.plist("p.fix-login");
        restamp(&w.places, "00ab", "0123456789abcdef");
        assert_eq!(w.plist("p.fix-login"), text);
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
    fn add_again_is_the_same_trigger_and_rm_removes_it() {
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
        let rings = w.fake.rings.get();
        let again = Trigger {
            generation: "test-2".into(),
            not_before: Some(1),
            ..s.clone()
        };
        assert_eq!(w.install(&again).unwrap(), Some(s.clone()));
        assert_eq!(w.fake.rings.get(), rings);
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
                &|| w.fake.ring(),
            )
        };
        assert_eq!(add(&path).unwrap(), Some(s.clone()));
        assert_eq!(
            Job::read(&w.plist(&s.name)).unwrap().environment,
            [("PATH".to_owned(), "/opt/new/bin".to_owned())]
        );
        assert_eq!(w.fake.rings.get(), rings + 1);
        assert_eq!(add(&path).unwrap(), Some(s.clone()));
        assert_eq!(w.fake.rings.get(), rings + 1);
        assert_eq!(add(&[]).unwrap(), Some(s.clone()));
        assert!(Job::read(&w.plist(&s.name)).unwrap().environment.is_empty());
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
            definition(
                Path::new("/A/agent-app"),
                &s,
                &every("1d", clock(2026, 9, 28, 9, 7)).unwrap(),
                &[]
            )
        );
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
        // A move of Sonny is written into every trigger.
        refresh(&w.places, Path::new("/B/agent-app"));
        assert_eq!(triggers(&w.places)[0].1, Path::new("/B/agent-app"));
        assert_eq!(triggers(&w.places)[0].0, s);
        let text = w.plist(&s.name);
        refresh(&w.places, Path::new("/B/agent-app"));
        assert_eq!(w.plist(&s.name), text, "an unmoved app changes nothing");
        w.remove(&s.name).unwrap();
        assert_eq!(w.rows(), Vec::<Value>::new());
        assert_eq!(w.state(&s.name), (false, false));
        let bad = Trigger {
            name: "../x".into(),
            ..trigger()
        };
        assert!(w.install(&bad).is_err());
    }

    #[test]
    fn oversized_definitions_are_refused_before_installing_a_trigger() {
        let w = World::new("record-limit");
        let broad = cron("0-7 0-7 1-4 1-4 *").unwrap();
        // Escaped, a control character is six bytes.
        let s = Trigger {
            when: broad.text.clone(),
            message: "\u{1}".repeat(MAX_MESSAGE),
            ..trigger()
        };
        let app = Path::new("/A/agent-app");
        assert!(definition(app, &s, &broad, &[]).len() as u64 > MAX_RECORD);
        let refused = install(&w.places, app, &s, &broad, &[], false, &|| w.fake.ring());
        assert!(refused.unwrap_err().starts_with("invalid_trigger:"));
        assert_eq!(w.state(&s.name), (false, false));
        assert_eq!(w.fake.rings.get(), 0);
        // A large readable one still lists and follows an app move. The
        // limit is serialized bytes, not message bytes.
        let when = cron("0-7 0-7 1-2 1-4 *").unwrap();
        let kept = Trigger {
            when: when.text.clone(),
            ..s.clone()
        };
        install(&w.places, app, &kept, &when, &[], false, &|| w.fake.ring()).unwrap();
        let text = w.plist(&s.name);
        assert!(text.len() as u64 > MAX_RECORD * 3 / 4);
        assert!(text.len() as u64 <= MAX_RECORD);
        assert_eq!(w.rows()[0]["message"], kept.message);
        refresh(&w.places, Path::new("/B/agent-app"));
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
            definition(Path::new("/A/app"), &changed, &fired(), &[]),
        )
        .unwrap();
        assert!(w.rows()[0]["last"].is_null());
        assert_eq!(w.rows()[0]["sent"], 0);
        std::fs::write(
            w.places.job(&s.name),
            definition(Path::new("/A/app"), &s, &fired(), &[]),
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
                definition(Path::new("/A/app"), &item, &fired(), &[]),
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
    fn the_sky_reads_each_trigger_s_next_two_times_on_its_own_store() {
        let w = World::new("upcoming");
        let now = clock(2026, 9, 28, 9, 7) + 30;
        // Every 30m, made a minute ago: first at 09:36, then 10:06.
        let every = Trigger {
            not_before: Some(clock(2026, 9, 28, 9, 36)),
            ..trigger()
        };
        let soon = Trigger {
            name: "p.soon".into(),
            when: "in 2h".into(),
            at: Some(now + 7200),
            ..trigger()
        };
        let gone = Trigger {
            name: "p.gone".into(),
            at: Some(now - 60),
            ..soon.clone()
        };
        let watch = Trigger {
            name: "p.watch".into(),
            when: "file /a/b".into(),
            ..trigger()
        };
        let other = Trigger {
            name: "p.other".into(),
            store_id: "ffee".into(),
            ..trigger()
        };
        for t in [&every, &soon, &gone, &watch, &other] {
            w.install(t).unwrap();
        }
        let found = upcoming(&w.places, "00ab", now);
        let times: Vec<(&str, Option<i64>, Option<i64>)> = (found.iter())
            .map(|u| (u.name.as_str(), u.next, u.then))
            .collect();
        assert_eq!(
            times,
            [
                (
                    "p.fix-login",
                    Some(clock(2026, 9, 28, 9, 36)),
                    Some(clock(2026, 9, 28, 10, 6))
                ),
                ("p.soon", Some(now + 7200), None),
                ("p.gone", None, None),
                ("p.watch", None, None),
            ]
        );
        assert_eq!(
            (found[0].bot.as_str(), found[0].when.as_str()),
            ("p.fix-login", "every 30m")
        );
    }

    #[test]
    fn creating_leaves_a_trigger_whole_or_nothing() {
        let w = World::new("create");
        let s = trigger();
        // A scheduler that cannot run here leaves nothing.
        w.fake.refuse.set(true);
        assert!(
            w.install(&s)
                .unwrap_err()
                .starts_with("triggers_unsupported")
        );
        assert_eq!(w.state(&s.name), (false, false));
        w.fake.refuse.set(false);
        w.install(&s).unwrap();
        assert_eq!(w.state(&s.name), (true, false));
        // A definition there that cannot be read is not replaced.
        let odd = Trigger {
            name: "p.odd".into(),
            ..trigger()
        };
        std::fs::create_dir_all(w.places.job(&odd.name).join("x")).unwrap();
        let rings = w.fake.rings.get();
        assert!(w.install(&odd).is_err());
        assert_eq!(w.fake.rings.get(), rings);
        // It is listed, saying why.
        let row = w.rows().into_iter().find(|r| r["name"] == "p.odd").unwrap();
        assert!(row["problem"].as_str().unwrap().starts_with("unreadable"));
        std::fs::remove_dir_all(w.places.job(&odd.name)).unwrap();
        std::fs::write(w.places.job(&odd.name), "not a definition").unwrap();
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
        w.install(&odd).unwrap();
        assert_eq!(w.state(&odd.name), (true, false));
    }

    #[test]
    fn a_fire_ends_only_the_trigger_it_ran_for() {
        let w = World::new("fire");
        let one = once();
        // Delivered: its row stays until its turn is known to have ended,
        // and stays when that turn failed.
        w.install(&one).unwrap();
        let handle = "turn:p.fix-login/2";
        w.settle(
            &one,
            json!({"outcome": "sent", "turn": 2, "handle": handle}),
        );
        assert_eq!(w.state(&one.name), (false, true));
        let ended = |status: &str| HashMap::from([(handle.to_owned(), json!({"status": status}))]);
        assert_eq!(forget_done(&w.places, "00ab", &ended("failed")), 0);
        // Another daemon's turn of that handle is not its.
        forget_done(&w.places, "ffff", &ended("completed"));
        assert_eq!(w.state(&one.name), (false, true));
        // Not while another holds the lock: it waits for a later turn.
        let held = Lock::take(&w.places).unwrap();
        assert_eq!(forget_done(&w.places, "00ab", &ended("completed")), 0);
        drop(held);
        // One that completed leaves nothing. A process another test forks
        // shares the lock's descriptor until it execs, so the lock can stay
        // taken a moment after the drop; like the window, try again.
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        let mut gone = forget_done(&w.places, "00ab", &ended("completed"));
        while gone == 0 && std::time::Instant::now() < deadline {
            std::thread::sleep(std::time::Duration::from_millis(10));
            gone = forget_done(&w.places, "00ab", &ended("completed"));
        }
        assert_eq!(gone, 1);
        assert_eq!(w.state(&one.name), (false, false));
        // Not delivered: it ends, and its row says why until it is removed.
        w.install(&one).unwrap();
        w.settle(
            &one,
            json!({"outcome": "failed", "detail": "daemon_unavailable"}),
        );
        assert_eq!(w.state(&one.name), (false, true));
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
        assert_eq!(w.state(&one.name), (true, false));
        assert_eq!(triggers(&w.places)[0].0, replacement);
        // Its own end keeps what it did on its row.
        w.settle(&replacement, json!({"outcome": "sent", "turn": 4}));
        assert_eq!(w.state(&one.name), (false, true));
        assert_eq!(w.rows()[0]["last"]["turn"], 4);
        // A definition removed while its fire ran: what is left of it goes,
        // and its fire records nothing over the result there was.
        w.install(&replacement).unwrap();
        std::fs::remove_file(w.places.job(&one.name)).unwrap();
        w.settle(&replacement, json!({"outcome": "missed"}));
        assert_eq!(w.state(&one.name), (false, true));
        assert!(!w.places.asks(&one.name).exists());
        assert_eq!(w.rows()[0]["last"]["turn"], 4);
        w.remove(&one.name).unwrap();
        // A repeating one ends only when its agent is gone, keeping why.
        let s = trigger();
        w.install(&s).unwrap();
        w.settle(&s, json!({"outcome": "skipped"}));
        assert_eq!(w.state(&s.name), (true, true));
        w.settle(&s, json!({"outcome": "gone", "detail": "bot_not_found"}));
        assert_eq!(w.state(&s.name), (false, true));
        // So does one whose answers go to an agent that is gone.
        w.install(&s).unwrap();
        let lost = json!({"outcome": "failed", "bot": "p.lead", "gone": true});
        w.settle(&s, json!({"outcome": "sent", "turn": 2, "reply": lost}));
        assert_eq!(w.state(&s.name), (false, true));
        assert_eq!(w.rows()[0]["last"]["reply"]["gone"], true);
        // And one that starts its agent once the agent that added it is gone;
        // any other agent it could not make is tried again next time.
        w.install(&s).unwrap();
        w.settle(&s, unmade("bot_exists: p.review".into()));
        assert_eq!(w.state(&s.name), (true, true));
        w.settle(&s, unmade("creator_not_found: ".into()));
        assert_eq!(w.state(&s.name), (false, true));
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
        assert_eq!(w.state(&s.name), (false, true));
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
        assert_eq!(w.state(&s.name), (true, true));
        w.settle(&s, json!({"outcome": "sent", "turn": 5}));
        assert_eq!(w.state(&s.name), (false, true));
    }

    #[test]
    fn fire_asks_the_scheduler_to_run_the_fire_and_its_fire_knows() {
        let w = World::new("fire-now");
        let s = trigger();
        assert!(
            fire_now(&w.places, &s.name, &|| w.fake.ring())
                .unwrap_err()
                .starts_with("trigger_not_found")
        );
        w.install(&s).unwrap();
        let asks = w.places.asks(&s.name);
        assert!(asks.is_dir() && w.places.taking(&s.name).is_dir());
        let take = |places: &Places| {
            let asked = take_ask(places, &s).unwrap()?;
            done_with(&asked).unwrap();
            Some(asked.kind)
        };
        assert_eq!(take(&w.places), None, "a fire for its time");
        // Each ask is one fire, oldest first; the scheduler runs it again
        // while any is left, rung for each.
        let rings = w.fake.rings.get();
        for _ in 0..2 {
            let file = fire_now(&w.places, &s.name, &|| w.fake.ring()).unwrap();
            assert!(asks.join(file).exists());
        }
        assert_eq!(w.fake.rings.get(), rings + 2);
        ask(&w.places, &s.name, Ask::Wake).unwrap();
        assert_eq!(std::fs::read_dir(&asks).unwrap().count(), 3);
        assert_eq!(take(&w.places), Some(Ask::Fire));
        assert_eq!(take(&w.places), Some(Ask::Fire));
        assert_eq!(take(&w.places), Some(Ask::Wake));
        assert_eq!(take(&w.places), None);
        // One a fire took and did not finish with is the next fire's.
        fire_now(&w.places, &s.name, &|| w.fake.ring()).unwrap();
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
        // A trigger that goes takes its asks with it.
        fire_now(&w.places, &s.name, &|| w.fake.ring()).unwrap();
        w.remove(&s.name).unwrap();
        assert!(!asks.exists());
    }

    #[test]
    fn fire_says_what_came_of_its_own_ask() {
        let w = World::new("fire-came");
        let s = trigger();
        w.install(&s).unwrap();
        let wait = |file: &str| super::fired(&w.places, &s.name, file, Duration::ZERO);
        // launchd has not run the job yet.
        let first = fire_now(&w.places, &s.name, &|| w.fake.ring()).unwrap();
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
        let ending = fire_now(&w.places, &s.name, &|| w.fake.ring()).unwrap();
        std::fs::write(w.places.receipt(&s.name, &ending), sent.to_string()).unwrap();
        w.remove(&s.name).unwrap();
        assert_eq!(wait(&ending)["handle"], "turn:p.fix-login/7");
        w.install(&s).unwrap();
        // Settled: the row says, for this ask and no other.
        let second = fire_now(&w.places, &s.name, &|| w.fake.ring()).unwrap();
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
            fire_now(&w.places, &s.name, &|| w.fake.ring()).unwrap(),
            fire_now(&w.places, &s.name, &|| w.fake.ring()).unwrap(),
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
        let third = fire_now(&w.places, &s.name, &|| w.fake.ring()).unwrap();
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
        settle(&w.places, &s, &json!({"outcome": "sent", "turn": 5}), &kept);
        let other = Trigger {
            generation: "test-2".into(),
            message: "something else".into(),
            ..s.clone()
        };
        let when = every("1d", clock(2026, 9, 28, 9, 7)).unwrap();
        let app = Path::new("/A/agent-app");
        let add = |t: &Trigger, replace| {
            install(&w.places, app, t, &when, &[], replace, &|| w.fake.ring())
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
            &|| w.fake.ring(),
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
        fire_now(&w.places, &s.name, &|| w.fake.ring()).unwrap();
        let stuck = take_ask(&w.places, &s).unwrap_err();
        assert!(stuck.starts_with("asks_stuck:"), "{stuck}");
        stops(&w.places, &s, stuck);
        assert_eq!(w.state(&s.name), (false, true));
        let row = &w.rows()[0];
        assert_eq!(row["last"]["outcome"], "failed");
        assert!(
            row["last"]["detail"]
                .as_str()
                .unwrap()
                .starts_with("asks_stuck:")
        );
        // Its job gone, its failure is still its store's to show, and no
        // other's.
        let kept = on_store(&w.places, &s.store_id);
        assert_eq!(kept.len(), 1);
        assert_eq!(kept[0].0["ended"], true);
        assert_eq!(kept[0].0["last"]["outcome"], "failed");
        assert_eq!(kept[0].1, [s.bot().to_owned()]);
        assert!(on_store(&w.places, "another-store").is_empty());
        // One that starts its agent stays the adder's too.
        let started = Trigger {
            name: "digest".into(),
            target: Target::Start {
                name: "digest".into(),
                model: "openai/gpt".into(),
                effort: None,
                by: Some(("p.lead".into(), 7)),
            },
            ..trigger()
        };
        w.install(&started).unwrap();
        stops(&w.places, &started, "start_failed: no".into());
        let kept = on_store(&w.places, &s.store_id);
        let digest = kept
            .iter()
            .find(|(row, _)| row["name"] == "digest")
            .unwrap();
        assert_eq!(digest.1, ["digest".to_owned(), "p.lead".to_owned()]);
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
        install(&w.places, app, &s, &when, &[], false, &|| w.fake.ring()).unwrap();
        let moved = When {
            watch: Some(root.join("elsewhere/logs/HEAD")),
            ..commit(root.to_str().unwrap()).unwrap().0
        };
        let again = install(&w.places, app, &s, &moved, &[], false, &|| w.fake.ring());
        assert_eq!(again.unwrap(), Some(s.clone()));
        assert_eq!(watched(&w.plist(&s.name)), moved.watch);
        assert_eq!(w.state(&s.name), (true, false));
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn removal_reaches_whatever_is_left_of_a_trigger() {
        let w = World::new("remove");
        let s = trigger();
        w.install(&s).unwrap();
        record_last(&w.places, &s, &json!({"outcome": "sent"}), &Kept::default()).unwrap();
        w.remove(&s.name).unwrap();
        assert_eq!(w.state(&s.name), (false, false));
        assert!(
            w.remove(&s.name)
                .unwrap_err()
                .starts_with("trigger_not_found")
        );
        // Its queue alone, or its result alone, is what is left of it.
        queues(&w.places.asks(&s.name)).unwrap();
        w.remove(&s.name).unwrap();
        assert!(!w.places.asks(&s.name).exists());
        record_last(
            &w.places,
            &s,
            &json!({"outcome": "failed"}),
            &Kept::default(),
        )
        .unwrap();
        w.remove(&s.name).unwrap();
        assert_eq!(w.state(&s.name), (false, false));
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
        assert_eq!(w.state(&stale.name), (false, true));
        assert_eq!(w.rows()[0]["last"]["outcome"], "missed");
    }

    #[test]
    fn sonny_s_start_removes_what_writes_that_died_left() {
        let w = World::new("cut-short");
        let s = trigger();
        w.install(&s).unwrap();
        // What a write that died before its rename left, and what an end
        // set aside before triggers moved to the scheduler.
        let left = [
            w.places.state.join(format!(".{}.json.4242", s.name)),
            w.places.jobs.join(format!(".{}.json.4242", s.name)),
            w.places.state.join(format!(".{}.json.retiring", s.name)),
        ];
        for path in &left {
            std::fs::write(path, "half").unwrap();
        }
        std::fs::create_dir_all(w.places.state.join(".p.x.asks.retiring/a")).unwrap();
        // The scheduler's own files stay.
        for file in SCHEDULER_FILES {
            std::fs::write(w.places.state.join(file), "").unwrap();
        }
        finish(&w.places);
        assert_eq!(w.state(&s.name), (true, false));
        assert!(left.iter().all(|path| !path.exists()));
        assert!(!w.places.state.join(".p.x.asks.retiring").exists());
        assert!(w.places.state.join(".lock").exists());
        assert!(
            SCHEDULER_FILES
                .iter()
                .all(|f| w.places.state.join(f).exists())
        );
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

    #[test]
    fn each_home_has_its_own_tag() {
        use std::ffi::OsStr;
        // tests/test_trigger.py computes the same tag.
        assert_eq!(home_tag(OsStr::new("/Users/lydakis")), "11c173e6");
        assert_eq!(home_tag(OsStr::new("/Users/lydakis/")), "11c173e6");
        assert_eq!(home_tag(OsStr::new("/tmp/eval-1")), "535e3091");
        assert_eq!(home_tag(OsStr::new("/")), "29622bb2");
        assert!(tagged("11c173e6.p.fix-login"));
        assert!(!tagged("p.fix-login") && !tagged("11C173E6.x") && !tagged("11c173e6"));
    }

    /// A trigger's LaunchAgent as an earlier Sonny wrote it, with what
    /// moving it reads.
    fn old_plist(
        label: &str,
        s: &Trigger,
        environment: &[(&str, &str)],
        watch: Option<&str>,
    ) -> String {
        let string = |v: &str| format!("<string>{}</string>", escape(v));
        let mut out = format!(
            "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n<plist version=\"1.0\">\n<dict>\n  <key>Label</key>\n  {}\n  <key>ProgramArguments</key>\n  <array>\n",
            string(label)
        );
        for arg in std::iter::once("/A/agent-app".to_owned()).chain(s.args()) {
            out += &format!("    {}\n", string(&arg));
        }
        out += "  </array>\n  <key>StartCalendarInterval</key>\n  <array>\n    <dict><key>Minute</key><integer>7</integer></dict>\n  </array>\n";
        if let Some(path) = watch {
            out += &format!(
                "  <key>WatchPaths</key>\n  <array>\n    {}\n  </array>\n",
                string(path)
            );
        }
        if !environment.is_empty() {
            out += "  <key>EnvironmentVariables</key>\n  <dict>\n";
            for (key, value) in environment {
                out += &format!("    <key>{key}</key>{}\n", string(value));
            }
            out += "  </dict>\n";
        }
        out + "  <key>ProcessType</key>\n  <string>Background</string>\n</dict>\n</plist>\n"
    }

    /// A trigger's user service as an earlier Sonny wrote it.
    fn old_unit(s: &Trigger, environment: &[(&str, &str)], watch: Option<&str>) -> String {
        let command: Vec<String> = std::iter::once("/A/agent-app".to_owned())
            .chain(s.args())
            .map(|arg| quoted(&arg, true))
            .collect();
        let mut out = format!(
            "# Sonny's trigger {0}\n[Unit]\nDescription=Sonny trigger {0}\n\n[Service]\nType=oneshot\nExecStart={1}\n",
            s.name,
            command.join(" ")
        );
        for (key, value) in environment {
            out += &format!("Environment={}\n", quoted(&format!("{key}={value}"), false));
        }
        out += "\n[X-Timer]\nOnCalendar=*-*-* *:07:00\nPersistent=true\n\n[X-Path]\n";
        if let Some(path) = watch {
            out += &format!("PathChanged={}\n", path.replace('%', "%%"));
        }
        out + "DirectoryNotEmpty=/q\n"
    }

    #[test]
    fn a_launchagent_from_before_becomes_a_definition_once() {
        let w = World::new("migrate");
        let home = w.root.join("home");
        let dir = Scheduler::Launchd.jobs(&home);
        std::fs::create_dir_all(&dir).unwrap();
        let tag = home_tag(home.as_os_str());
        let first = every("1d", clock(2026, 9, 28, 9, 7)).unwrap();
        let s = Trigger {
            not_before: first.not_before,
            when: "every 1d".into(),
            message: "a <look> & \"more\"".into(),
            ..trigger()
        };
        // Tagged with this HOME's tag, and watching a path.
        let label = format!("{LABEL}{tag}.{}", s.name);
        let old = dir.join(format!("{label}.plist"));
        let environment = [("HOME", home.to_str().unwrap()), ("SHELL", "/bin/zsh")];
        std::fs::write(
            &old,
            old_plist(&label, &s, &environment, Some("/repo/.git/HEAD")),
        )
        .unwrap();
        // From before tags: this HOME's moves, another's stays for its own Sonny.
        let mine = Trigger {
            name: "mine".into(),
            ..s.clone()
        };
        let untagged = dir.join(format!("{LABEL}mine.plist"));
        std::fs::write(
            &untagged,
            old_plist(&format!("{LABEL}mine"), &mine, &environment, None),
        )
        .unwrap();
        let theirs = Trigger {
            name: "theirs".into(),
            ..s.clone()
        };
        let elsewhere = dir.join(format!("{LABEL}theirs.plist"));
        let other = old_plist(
            &format!("{LABEL}theirs"),
            &theirs,
            &[("HOME", "/elsewhere")],
            None,
        );
        std::fs::write(&elsewhere, &other).unwrap();
        // Another HOME's tag is never this one's.
        let tagged_other = dir.join(format!("{LABEL}0123abcd.x.plist"));
        std::fs::write(&tagged_other, &other).unwrap();
        let let_go = std::cell::RefCell::new(Vec::new());
        let record =
            |at: &Path, label: &str| let_go.borrow_mut().push((at.to_owned(), label.to_owned()));
        assert!(migrate(
            &w.places,
            &home,
            Scheduler::Launchd,
            &|| w.fake.ring(),
            &record,
        ));
        assert!(!old.exists() && !untagged.exists());
        assert_eq!(std::fs::read_to_string(&elsewhere).unwrap(), other);
        assert!(tagged_other.exists() && !w.places.job("theirs").exists());
        let mut gone = let_go.borrow().clone();
        gone.sort();
        let mut want = vec![
            (dir.clone(), label.clone()),
            (dir.clone(), format!("{LABEL}mine")),
        ];
        want.sort();
        assert_eq!(gone, want);
        assert_eq!(w.fake.rings.get(), 1);
        // The definition fires as the job did, at the times it had.
        let job = Job::read(&w.plist(&s.name)).unwrap();
        assert_eq!(read_job(&w.plist(&s.name)).unwrap().0, s);
        assert_eq!(job.calendar, first.entries);
        assert_eq!(job.watch, Some(PathBuf::from("/repo/.git/HEAD")));
        assert!(
            job.environment
                .contains(&("SHELL".into(), "/bin/zsh".into()))
        );
        assert!(w.places.asks(&s.name).is_dir());
        assert_eq!(w.rows().len(), 2);
        // Once: nothing is left to move.
        assert!(migrate(
            &w.places,
            &home,
            Scheduler::Launchd,
            &|| w.fake.ring(),
            &record,
        ));
        assert_eq!(w.fake.rings.get(), 1);
        assert_eq!(let_go.borrow().len(), 2);
    }

    #[test]
    fn a_user_service_from_before_becomes_a_definition() {
        let w = World::new("migrate-unit");
        let home = w.root.join("home");
        let dir = Scheduler::Systemd.jobs(&home);
        std::fs::create_dir_all(&dir).unwrap();
        let s = Trigger {
            message: "100% $HOME \"quoted\"".into(),
            ..trigger()
        };
        let label = format!("{LABEL}{}", s.name);
        let old = dir.join(format!("{label}.service"));
        let environment = [("HOME", home.to_str().unwrap())];
        std::fs::write(&old, old_unit(&s, &environment, Some("/repo/50%"))).unwrap();
        // A definition of the name already there stays as it is.
        let kept = Trigger {
            name: "kept".into(),
            ..s.clone()
        };
        assert_eq!(w.install(&kept).unwrap(), None);
        let before = w.plist("kept");
        let older = dir.join(format!("{LABEL}kept.service"));
        std::fs::write(
            &older,
            old_unit(
                &Trigger {
                    message: "older".into(),
                    ..kept.clone()
                },
                &environment,
                None,
            ),
        )
        .unwrap();
        assert!(migrate(
            &w.places,
            &home,
            Scheduler::Systemd,
            &|| w.fake.ring(),
            &|_, _| {},
        ));
        assert!(!old.exists() && !older.exists());
        assert_eq!(w.plist("kept"), before);
        assert_eq!(read_job(&w.plist(&s.name)).unwrap().0, s);
        assert_eq!(
            Job::read(&w.plist(&s.name)).unwrap().watch,
            Some(PathBuf::from("/repo/50%"))
        );
    }
}
