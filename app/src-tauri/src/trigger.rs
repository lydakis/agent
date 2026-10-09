//! Triggers: an agent woken with a message it wrote for itself, or one
//! written for it, when something happens: a time comes, a file is written,
//! a repository's HEAD moves, or someone fires it by name. launchd watches,
//! so a trigger fires with the app closed, and once on waking for times the
//! Mac slept through. The daemon has no clock or watcher of its own, and no
//! process of a trigger runs between its fires.
//!
//! A trigger is one LaunchAgent, `~/Library/LaunchAgents/LABEL.plist`,
//! which runs this executable with `--trigger-fire` and everything the fire
//! needs as its arguments: the plist is its definition. What its fires did
//! and leave for the next (the commit it saw) is
//! `~/.agent/triggers/NAME.json`. A fire messages the agent the trigger was
//! made for, pinned by its id, and never a working agent, for a repeating
//! trigger: that time is skipped. It never creates an agent. A deleted agent
//! takes its triggers with it.
//!
//! `~/.agent/trigger`, a script the app writes, is how agents and people
//! add, list, fire and remove them.
use agent_client::Client;
use serde_json::{Value, json};
use std::{
    io::Write,
    path::{Path, PathBuf},
};

pub const FLAG: &str = "--trigger";
pub const FIRE_FLAG: &str = "--trigger-fire";
/// What an earlier app's schedules run; such a run converts them (`migrate`).
pub const SCHEDULE_FIRE_FLAG: &str = "--schedule-fire";
/// Every trigger's launchd label starts with this; the rest is its name.
const LABEL: &str = "me.lydakis.agent.trigger.";
const SCHEDULE_LABEL: &str = "me.lydakis.agent.schedule.";
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
/// How old a `fire NAME` may be when launchd starts the fire it asked for.
const ASKED_WITHIN: i64 = 120;
const USAGE: &str = "usage: trigger add [--name NAME] [WHEN] [--bot NAME] -- MESSAGE\n         WHEN: --every N{m,h,d} | --in N{m,h} | --at 'YYYY-MM-DD HH:MM' | --cron 'MIN HOUR DAY MONTH WEEKDAY' | --file PATH | --commit REPO; none: only fire runs it\n       trigger ls [--after NAME]\n       trigger fire NAME\n       trigger rm NAME";

/// Where triggers live: the LaunchAgents folder holds their plists, and
/// `~/.agent/triggers` what each one's fires did.
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
    /// Left by `fire NAME` for the fire launchd starts.
    fn asked(&self, name: &str) -> PathBuf {
        self.state.join(format!("{name}.fire"))
    }
}

/// What launchd is asked to do. Tests stand in for it.
pub enum Launchd<'a> {
    Load(&'a Path),
    Unload(&'a str),
    /// Run a loaded job now.
    Start(&'a str),
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
        Launchd::Start(label) => vec!["kickstart".into(), format!("{domain}/{label}")],
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

/// One trigger, as its plist's arguments carry it.
#[derive(Debug, Clone, PartialEq)]
pub struct Trigger {
    pub name: String,
    pub generation: String,
    pub not_before: Option<i64>,
    pub bot: String,
    pub bot_id: i64,
    /// How it was asked for, to show: `every 30m`, `file /a/b`, `fire`.
    pub when: String,
    /// A one-off's time; it fires once, then removes itself.
    pub at: Option<i64>,
    /// The repository whose HEAD it follows; a fire for any other write to
    /// its HEAD log sends nothing.
    pub commit: Option<PathBuf>,
    pub daemon: Daemon,
    /// The store identity its daemon announced when it was made; a daemon
    /// on that socket serving another store is not its daemon.
    pub store_id: String,
    pub message: String,
}

impl Trigger {
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
        pair("--bot", self.bot.clone());
        pair("--bot-id", self.bot_id.to_string());
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
            const FLAGS: [&str; 11] = [
                "--name",
                "--generation",
                "--bot",
                "--bot-id",
                "--when",
                "--not-before",
                "--at",
                "--commit",
                "--store",
                "--socket",
                "--store-id",
            ];
            let known = FLAGS
                .iter()
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
            bot: need("--bot")?,
            bot_id: number("--bot-id")?.ok_or_else(|| bad("no --bot-id"))?,
            when: need("--when")?,
            at: number("--at")?,
            commit: text("--commit").map(PathBuf::from),
            daemon,
            store_id: need("--store-id")?,
            message,
        })
    }

    /// Its row, with the last outcome its fires left in `state`.
    fn json(&self, state: &Value) -> Value {
        json!({"name": self.name, "generation": self.generation, "when": self.when,
            "bot": self.bot, "bot_id": self.bot_id, "message": self.message,
            "once": self.at.is_some(), "last": state["last"]})
    }

    /// The first field another trigger of its name differs in, to name in
    /// `trigger_exists`. Its generation and first time are when it was
    /// made, not what it is.
    fn differs(&self, other: &Self) -> Option<&'static str> {
        [
            ("when", self.when == other.when),
            ("bot", self.bot == other.bot && self.bot_id == other.bot_id),
            ("message", self.message == other.message),
            ("commit", self.commit == other.commit),
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

fn local(epoch: i64) -> Local {
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

fn git(repo: &Path, args: &[&str]) -> Option<String> {
    let out = std::process::Command::new("git")
        .arg("-C")
        .arg(repo)
        .args(args)
        .stdin(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .output()
        .ok()?;
    let text = String::from_utf8(out.stdout).ok()?;
    out.status.success().then(|| text.trim().to_owned())
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
/// or a watched path, launchd runs it only when asked (`fire`).
pub fn plist(app: &Path, trigger: &Trigger, when: &When, environment: &[(&str, String)]) -> String {
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
    if !environment.is_empty() {
        out += "  <key>EnvironmentVariables</key>\n  <dict>\n";
        for (key, value) in environment {
            out += &format!("    <key>{key}</key>{}\n", string(value));
        }
        out += "  </dict>\n";
    }
    out += "  <key>ProcessType</key>\n  <string>Background</string>\n</dict>\n</plist>\n";
    out
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
    let rows: Vec<_> = names
        .into_iter()
        .map(|name| {
            let path = places.plist(&name);
            if !path.exists() {
                let mut row = read_json(&places.last(&name))
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
        })
        .collect();
    json!({"triggers": rows, "next_after": next})
}

/// Write a file whole beside its place, then rename it there.
fn replace(path: &Path, text: &str) -> Result<(), String> {
    replace_mode(path, text, 0o644)
}

/// `replace` with the file's mode set from creation, so the new name never
/// has any other.
fn replace_mode(path: &Path, text: &str, mode: u32) -> Result<(), String> {
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
        file.write_all(text.as_bytes())?;
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
                    // folder. launchd is made to watch the one it is now.
                    if watched(&text) != when.watch {
                        let want = plist(&there_app, &there, when, environment);
                        swap(&path, &there.name, Some(&text), &want, launchd)?;
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
    swap(
        &path,
        &trigger.name,
        None,
        &plist(app, trigger, when, environment),
        launchd,
    )?;
    Ok(None)
}

/// Put `text` in the plist at `path` and load it in place of `old`: when
/// launchd will not unload the old job nothing changes, and when the new
/// one cannot be written or loaded the old one is written back and loaded.
fn swap(
    path: &Path,
    name: &str,
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
    unload(&format!("{LABEL}{name}"), launchd)?;
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

/// Remove a trigger: launchd's copy, then its plist and last result. An
/// unload launchd refuses keeps the plist, so the removal can be retried.
/// Whatever is left of it goes, a job loaded without its plist included.
pub fn remove(places: &Places, name: &str, launchd: Loader) -> Result<(), String> {
    valid_name(name)?;
    let _lock = Lock::take(places)?;
    let path = places.plist(name);
    let last = places.last(name);
    // A folder that ignores case finds `build`'s files for `Build`, whose
    // label launchd does not have: only the name as stored is that trigger.
    let stored = |dir: &Path, file: &Path| {
        let want = file.file_name();
        std::fs::read_dir(dir)
            .into_iter()
            .flatten()
            .flatten()
            .any(|e| Some(e.file_name().as_os_str()) == want)
    };
    let (path_here, last_here) = (stored(&places.agents, &path), stored(&places.state, &last));
    // launchd's labels keep their case, so this reaches only this name's job.
    let loaded = match launchd(Launchd::Unload(&format!("{LABEL}{name}"))) {
        Ok(()) => true,
        Err(error) if error.starts_with(NOT_LOADED) => false,
        Err(error) => return Err(error),
    };
    if !path_here && !last_here && !loaded {
        return Err(format!("trigger_not_found: {name}"));
    }
    if path_here {
        forget(&path)?;
    }
    if last_here {
        forget(&last)?;
    }
    let _ = forget(&places.asked(name));
    Ok(())
}

/// `fire NAME`: launchd runs the trigger's job now. The note it leaves tells
/// that fire it was asked for, so it sends whatever the time or its watched
/// path. While one of its fires still runs launchd starts no other: that
/// fire finds the note when it is done and sends once more.
pub fn fire_now(places: &Places, name: &str, launchd: Loader) -> Result<Value, String> {
    valid_name(name)?;
    let _lock = Lock::take(places)?;
    if !places.plist(name).exists() {
        return Err(format!("trigger_not_found: {name}"));
    }
    let asked = places.asked(name);
    replace(&asked, &now().to_string())?;
    if let Err(error) = launchd(Launchd::Start(&format!("{LABEL}{name}"))) {
        let _ = forget(&asked);
        return Err(error);
    }
    Ok(json!({"name": name, "fired": true}))
}

/// Whether this fire is one `fire NAME` asked for; the note goes either way.
fn take_asked(places: &Places, name: &str) -> bool {
    take_note(places, name, |at| (now() - at).abs() <= ASKED_WITHIN)
}

/// The `fire NAME` note, gone once read, when `fresh` takes its time.
fn take_note(places: &Places, name: &str, fresh: impl Fn(i64) -> bool) -> bool {
    let path = places.asked(name);
    let at = std::fs::read_to_string(&path)
        .ok()
        .and_then(|t| t.trim().parse::<i64>().ok());
    let _ = forget(&path);
    at.is_some_and(fresh)
}

/// What a fire leaves for the next: the commit it saw.
#[derive(Debug, Default, Clone, PartialEq)]
struct Kept {
    head: Option<String>,
}

impl Kept {
    fn of(state: &Value) -> Self {
        Self {
            head: state["head"].as_str().map(str::to_owned),
        }
    }
}

/// What a fire did goes on disk, and a trigger that is over ends: both
/// under the lock, and only while the plist is still this trigger's. One
/// replaced or removed while its message went out is left as it now is; a
/// job left loaded after its plist went (an end cut short) is unloaded.
fn settle(places: &Places, trigger: &Trigger, outcome: &Value, kept: &Kept, launchd: Loader) {
    let log = |error: String| eprintln!("{}", error_json(&error));
    let _lock = match Lock::take(places) {
        Ok(lock) => lock,
        Err(error) => return log(error),
    };
    let path = places.plist(&trigger.name);
    let text = match std::fs::read_to_string(&path) {
        Ok(text) => text,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            // Under the lock no `add` is between writing and loading one.
            if let Err(error) = unload(&format!("{LABEL}{}", trigger.name), launchd) {
                log(error);
            }
            return;
        }
        Err(e) => return log(format!("{}: {e}", path.display())),
    };
    if read_plist(&text).is_none_or(|(now, _)| now != *trigger) {
        return;
    }
    let recorded = record_last(places, trigger, outcome, kept);
    if let Err(error) = &recorded {
        log(error.clone());
    }
    let sent = outcome["outcome"] == "sent";
    // One that did not deliver ends only once why is on disk; else its plist
    // stays, listed.
    if (trigger.at.is_some() || outcome["outcome"] == "gone") && (sent || recorded.is_ok()) {
        end(places, &trigger.name, &path, &text, !sent, launchd);
    }
}

/// A trigger ends itself: its plist first, then launchd's copy, whose
/// unload ends this process. `keep` leaves its last result, so an end nobody
/// asked for still shows, and why. A plist that will not go stays loaded,
/// listed, for `rm`: unloaded, it would load again at the next login. An
/// unload launchd refuses writes the plist back, so the job still loaded
/// stays listed for `rm`. Called under the lock.
fn end(places: &Places, name: &str, path: &Path, text: &str, keep: bool, launchd: Loader) {
    let log = |error: String| eprintln!("{}", error_json(&error));
    if let Err(error) = forget(path) {
        return log(error);
    }
    let last = places.last(name);
    let result = if keep {
        None
    } else {
        let result = std::fs::read_to_string(&last).ok();
        if let Err(error) = forget(&last) {
            log(error);
        }
        result
    };
    if let Err(error) = unload(&format!("{LABEL}{name}"), launchd) {
        log(error);
        // Still loaded, it is still this trigger, with what its fire did.
        for (path, text) in [(path, Some(text)), (&last, result.as_deref())] {
            if let Some(text) = text
                && let Err(error) = replace(path, text)
            {
                log(error);
            }
        }
    }
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
        let _ = swap(&path, &trigger.name, Some(&text), &moved, launchd);
    }
}

/// Earlier apps called triggers schedules: jobs labelled
/// `me.lydakis.agent.schedule.NAME` that ran `--schedule-fire`, with their
/// results in `~/.agent/schedules`, added through `~/.agent/schedule`. Each
/// becomes the trigger of its name once, its result kept; the old folder
/// and script go. A schedule that cannot be read or whose name a trigger
/// has is left where it is and said so. `running` is the schedule whose
/// fire runs this, unloaded last since that ends this process.
pub fn migrate(places: &Places, running: Option<&str>, launchd: Loader) {
    let log = |error: String| eprintln!("{}", error_json(&error));
    let Some(home) = places.state.parent() else {
        return;
    };
    let old_state = home.join("schedules");
    let mut names: Vec<String> = std::fs::read_dir(&places.agents)
        .into_iter()
        .flatten()
        .flatten()
        .filter_map(|e| {
            let file = e.file_name().into_string().ok()?;
            Some(
                file.strip_prefix(SCHEDULE_LABEL)?
                    .strip_suffix(".plist")?
                    .to_owned(),
            )
        })
        .filter(|name| valid_name(name).is_ok())
        .collect();
    // The running one last: its unload ends this process.
    names.sort_by_key(|name| Some(name.as_str()) == running);
    let Ok(_lock) = Lock::take(places) else {
        return log("migration_failed: no trigger lock".into());
    };
    let mut done = Vec::new();
    for name in names {
        let old = places.agents.join(format!("{SCHEDULE_LABEL}{name}.plist"));
        let Ok(text) = read_record(&old) else {
            continue;
        };
        let converted = text
            .replacen(
                &format!("<string>{SCHEDULE_LABEL}{name}</string>"),
                &format!("<string>{LABEL}{name}</string>"),
                1,
            )
            .replacen(
                &format!("<string>{SCHEDULE_FIRE_FLAG}</string>"),
                &format!("<string>{FIRE_FLAG}</string>"),
                1,
            );
        if read_plist(&converted).is_none() {
            log(format!(
                "unconvertible: schedule {name} is left at {}",
                old.display()
            ));
            continue;
        }
        let path = places.plist(&name);
        if path.exists() {
            log(format!(
                "name_taken: schedule {name} is left at {}: a trigger has its name",
                old.display()
            ));
            continue;
        }
        let result = old_state.join(format!("{name}.json"));
        if result.exists()
            && let Err(e) = std::fs::rename(&result, places.last(&name))
        {
            log(format!("{}: {e}", result.display()));
        }
        if let Err(error) = swap(&path, &name, None, &converted, launchd) {
            log(error);
            continue;
        }
        if let Err(error) = forget(&old) {
            log(error);
        }
        done.push(name);
    }
    // Results of schedules that had ended, then the old folder and script.
    for e in std::fs::read_dir(&old_state)
        .into_iter()
        .flatten()
        .flatten()
    {
        let to = places.state.join(e.file_name());
        if e.file_name().to_string_lossy().ends_with(".json") && !to.exists() {
            let _ = std::fs::rename(e.path(), to);
        }
    }
    let _ = std::fs::remove_file(old_state.join(".lock"));
    let _ = std::fs::remove_dir(&old_state);
    let script = home.join("schedule");
    if std::fs::read_to_string(&script).is_ok_and(|t| t.contains(" --schedule \"$@\"")) {
        let _ = forget(&script);
    }
    for name in done {
        if let Err(error) = unload(&format!("{SCHEDULE_LABEL}{name}"), launchd) {
            log(error);
        }
    }
}

/// `APP --schedule-fire ...`: a schedule an earlier app made fired before
/// this app converted them. Its time is not sent; its trigger fires at the
/// next one.
pub fn migrate_cli(args: &[String]) -> i32 {
    let running = args
        .iter()
        .position(|a| a == "--name")
        .and_then(|i| args.get(i + 1));
    match Places::home() {
        Ok(places) => migrate(&places, running.map(String::as_str), &launchctl),
        Err(error) => eprintln!("{}", error_json(&error)),
    }
    0
}

/// What `trigger add` was asked.
#[derive(Debug, PartialEq)]
struct Add {
    name: Option<String>,
    when: When,
    commit: Option<PathBuf>,
    bot: Option<String>,
    message: String,
}

fn parse_add(args: &[String], now: i64) -> Result<Add, String> {
    let bad = |what: String| format!("invalid_trigger: {what}\n{USAGE}");
    let mut v = std::collections::HashMap::<String, String>::new();
    let (mut when, mut commit) = (None, None);
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
            "--name" | "--bot" => {
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
    if message.trim().is_empty() {
        return Err(bad("a message goes after --".into()));
    }
    if message.len() > MAX_MESSAGE {
        return Err(format!(
            "invalid_trigger: a message is at most {MAX_MESSAGE} bytes"
        ));
    }
    // XML 1.0 has no place for other control characters.
    if message
        .chars()
        .any(|c| c.is_control() && !matches!(c, '\n' | '\t' | '\r'))
    {
        return Err("invalid_trigger: the message has control characters".into());
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
        bot: take("--bot"),
        message,
    })
}

fn runtime() -> Result<tokio::runtime::Runtime, String> {
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .map_err(|e| e.to_string())
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
        Some((verb, [name])) if verb == "fire" => fire_now(&places, name, &launchctl),
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
/// fire writes its result there. Folders on a Mac ignore case, and a link
/// is followed as far as the path exists.
fn watches_state(places: &Places, watch: &Path) -> Result<(), String> {
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
    Ok(())
}

/// An agent's id by its name, now.
async fn bot_id(client: &Client, name: &str) -> Result<i64, String> {
    let record = client
        .request("resume", json!({"bot": name}))
        .await
        .map_err(|e| e.to_string())?;
    record["id"]
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
    if let Some(watch) = &asked.when.watch {
        watches_state(places, watch)?;
    }
    let daemon = Daemon::current()?;
    let socket = daemon.socket()?;
    let name = asked
        .name
        .clone()
        .or_else(|| asked.bot.clone())
        .or_else(|| shell.as_ref().map(|s| s.0.clone()))
        .ok_or("invalid_trigger: --bot NAME, or run it from an agent's shell")?;
    let ((bot, bot_id), store_id) = runtime()?.block_on(async {
        let client = connect(&socket, &daemon).await?;
        let resolved = async {
            let bot = match (&asked.bot, &shell) {
                (Some(bot), _) => (bot.clone(), bot_id(&client, bot).await?),
                (None, Some((bot, id))) => {
                    // The trigger is pinned to this identity, which must still exist.
                    if bot_id(&client, bot).await? != *id {
                        return Err(
                            "bot_not_found: the shell's bot identity no longer exists".into()
                        );
                    }
                    (bot.clone(), *id)
                }
                (None, None) => {
                    return Err(
                        "invalid_trigger: --bot NAME, or run it from an agent's shell".into(),
                    );
                }
            };
            let store_id = client
                .store()
                .map(str::to_owned)
                .ok_or_else(|| "the daemon announced no store identity".to_owned())?;
            Ok::<_, String>((bot, store_id))
        }
        .await;
        client.close().await;
        resolved
    })?;
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
        bot,
        bot_id,
        when: asked.when.text.clone(),
        at: asked.when.at,
        commit: asked.commit,
        daemon,
        store_id,
        message: asked.message,
    };
    let app = std::env::current_exe().map_err(|e| e.to_string())?;
    let environment: Vec<(&str, String)> = ["HOME", "SHELL"]
        .into_iter()
        .filter_map(|key| std::env::var(key).ok().map(|v| (key, v)))
        .collect();
    match install(
        places,
        &app,
        &trigger,
        &asked.when,
        &environment,
        &launchctl,
    )? {
        Some(there) => {
            let mut row = there.json(&state(places, &there));
            row["duplicate"] = json!(true);
            Ok(row)
        }
        None => {
            // The commit there now is seen: only the next one fires. A
            // trigger without it would take that commit for news, so it goes.
            if let Some(head) = trigger.commit.as_deref().and_then(head) {
                let kept = Kept { head: Some(head) };
                if let Err(error) = record_last(places, &trigger, &Value::Null, &kept) {
                    let _ = remove(places, &trigger.name, &launchctl);
                    return Err(error);
                }
            }
            Ok(trigger.json(&state(places, &trigger)))
        }
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
    let mut row = trigger.json(&json!({"last": outcome}));
    row["head"] = json!(kept.head);
    replace(&places.last(&trigger.name), &row.to_string())
}

/// `YYYY-MM-DD HH:MM`, local.
fn stamp(epoch: i64) -> String {
    let t = local(epoch);
    format!(
        "{}-{:02}-{:02} {:02}:{:02}",
        t.year, t.month, t.day, t.hour, t.minute
    )
}

/// Send the fire's message: a new turn when the agent is resting; a working
/// agent, or one with work waiting, skips this time of a repeating trigger
/// and gets any other's after its work. A deleted agent's trigger goes.
async fn deliver(client: &Client, trigger: &Trigger, why: &str, asked: bool) -> Value {
    let prompt = format!(
        "[trigger {} · {} · {why}]\n{}",
        trigger.name,
        stamp(now()),
        trigger.message
    );
    let delivery = if trigger.at.is_some() || asked {
        "queue"
    } else {
        "reject"
    };
    let submitted = client
        .request(
            "submit",
            json!({"bot": trigger.bot, "bot_id": trigger.bot_id,
                "request_id": format!("trigger-{}-{}-{}", trigger.bot_id, now(), std::process::id()),
                "prompt": prompt, "delivery": delivery, "origin": "trigger"}),
        )
        .await;
    match submitted {
        Ok(turn) => json!({"outcome": "sent", "turn": turn["turn"]}),
        Err(error) if error.code == "bot_busy" || error.code == "active_agent_limit" => {
            json!({"outcome": "skipped", "detail": error.to_string()})
        }
        Err(error) if error.code == "bot_not_found" => {
            json!({"outcome": "gone", "detail": error.to_string()})
        }
        Err(error) => json!({"outcome": "failed", "detail": error.to_string()}),
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
    fires(&places, &trigger.name, |asked| {
        fire(&places, &trigger, asked)
    });
    0
}

/// Fire, and again for each `fire NAME` asked while it ran.
/// One asked while it ran is this fire's however long it ran (a fire can
/// wait a day); one asked before it started was read at its start.
fn fires(places: &Places, name: &str, mut fire: impl FnMut(bool)) {
    let mut asked = take_asked(places, name);
    loop {
        let started = now();
        fire(asked);
        if !take_note(places, name, |at| at >= started) {
            return;
        }
        asked = true;
    }
}

/// One fire: send the message when it is time, or when it was asked for.
fn fire(places: &Places, trigger: &Trigger, asked: bool) {
    let now = now();
    let mut kept = Kept::of(&state(places, trigger));
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
    let mut why = if asked {
        "fired".to_owned()
    } else {
        trigger.when.clone()
    };
    // Any write to the HEAD log wakes it; only another commit is news.
    if let Some(repo) = &trigger.commit {
        let seen = head(repo);
        if !asked && (seen.is_none() || seen == kept.head) {
            return;
        }
        // A HEAD that cannot be read now keeps the last one seen.
        if let Some(sha) = seen {
            why = format!("{why} at {}", &sha[..sha.len().min(12)]);
            kept.head = Some(sha);
        }
    }
    let outcome = match runtime() {
        Ok(runtime) => runtime.block_on(async {
            let socket = match trigger.daemon.socket() {
                Ok(socket) => socket,
                Err(error) => return json!({"outcome": "failed", "detail": error}),
            };
            let client = match connect(&socket, &trigger.daemon).await {
                Ok(client) => client,
                Err(error) => return json!({"outcome": "failed", "detail": error}),
            };
            // The socket may now be another store's daemon's; its bot ids are its own.
            if client.store() != Some(trigger.store_id.as_str()) {
                client.close().await;
                return json!({"outcome": "failed", "detail": format!(
                    "store_mismatch: the daemon at {} serves another store", socket.display())});
            }
            let outcome = deliver(&client, trigger, &why, asked).await;
            client.close().await;
            outcome
        }),
        Err(error) => json!({"outcome": "failed", "detail": error}),
    };
    settle(places, trigger, &outcome, &kept, &launchctl);
}

/// The daemon, started the way the app starts one when none answers and
/// the trigger names its store, on the socket the trigger was made with.
async fn connect(socket: &Path, daemon: &Daemon) -> Result<std::sync::Arc<Client>, String> {
    match Client::connect(socket).await {
        Ok((client, _events)) => Ok(client),
        Err(error) if error.code == "daemon_unavailable" => {
            let (Some(store), Some(agent)) = (&daemon.store, crate::daemon::bundled()) else {
                return Err(error.to_string());
            };
            crate::daemon::Starts::default()
                .start(&agent, store, daemon.socket.as_deref())
                .await?;
            let (client, _events) = Client::connect(socket).await.map_err(|e| e.to_string())?;
            Ok(client)
        }
        Err(error) => Err(error.to_string()),
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
    replace_mode(&path, &text, 0o755)
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
            bot: "p.fix-login".into(),
            bot_id: 42,
            when: "every 30m".into(),
            at: None,
            commit: None,
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
            &[("SHELL", "/bin/zsh".into())],
        );
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
            when: "commit /r".into(),
            commit: Some("/r".into()),
            ..s.clone()
        };
        let watched = When {
            watch: Some("/r/.git/logs/HEAD".into()),
            ..fired()
        };
        let text = plist(Path::new("/A/app"), &full, &watched, &[]);
        assert!(
            text.contains(
                "<key>WatchPaths</key>\n  <array>\n    <string>/r/.git/logs/HEAD</string>"
            )
        );
        assert!(!text.contains("StartCalendarInterval"));
        assert_eq!(read_plist(&text).unwrap().0, full);
        // Only `fire` runs one with neither.
        let text = plist(Path::new("/A/app"), &s, &fired(), &[]);
        assert!(!text.contains("StartCalendarInterval") && !text.contains("WatchPaths"));
        let mut args: Vec<String> = s.args()[1..].to_vec();
        let at = args.iter().position(|a| a == "--bot-id").unwrap();
        args.drain(at..at + 2);
        assert!(Trigger::parse(&args).is_err(), "a bot needs its id");
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
                Launchd::Start(label) => {
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
        let other_bot = Trigger {
            bot_id: 43,
            ..s.clone()
        };
        assert!(
            w.install(&other_bot)
                .unwrap_err()
                .starts_with("trigger_exists: bot:")
        );
        assert_eq!(
            w.plist(&s.name),
            plist(
                Path::new("/A/agent-app"),
                &s,
                &every("1d", clock(2026, 9, 28, 9, 7)).unwrap(),
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
        assert!(plist(app, &s, &broad, &[]).len() as u64 > MAX_RECORD);
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
        record_last(
            &w.places,
            &s,
            &json!({"outcome": "sent", "turn": 7}),
            &Kept::default(),
        )
        .unwrap();
        assert_eq!(w.rows()[0]["last"]["turn"], 7);
        // Another of its name made since (an `rm`, then an `add`) does not
        // take its result; the first one put back sees it again.
        let changed = Trigger {
            generation: "test-next".into(),
            ..s.clone()
        };
        std::fs::write(
            w.places.plist(&s.name),
            plist(Path::new("/A/app"), &changed, &fired(), &[]),
        )
        .unwrap();
        assert!(w.rows()[0]["last"].is_null());
        std::fs::write(
            w.places.plist(&s.name),
            plist(Path::new("/A/app"), &s, &fired(), &[]),
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
                plist(Path::new("/A/app"), &item, &fired(), &[]),
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
        // A plist that will not go keeps its job loaded and its result.
        let stuck = w.places.plist("p.stuck");
        std::fs::create_dir_all(stuck.join("x")).unwrap();
        w.fake.loaded.borrow_mut().insert(format!("{LABEL}p.stuck"));
        end(&w.places, "p.stuck", &stuck, "", false, &|x| w.fake.call(x));
        assert!(w.fake.loaded.borrow().contains(&format!("{LABEL}p.stuck")));
        std::fs::remove_dir_all(&stuck).unwrap();
        // A repeating one ends only when its agent is gone, keeping why.
        let s = trigger();
        w.install(&s).unwrap();
        w.settle(&s, json!({"outcome": "skipped"}));
        assert_eq!(w.state(&s.name), (true, true, true));
        w.settle(&s, json!({"outcome": "gone", "detail": "bot_not_found"}));
        assert_eq!(w.state(&s.name), (false, false, true));
    }

    #[test]
    fn fire_asks_launchd_to_run_the_job_now_and_its_fire_knows() {
        let w = World::new("fire-now");
        let s = trigger();
        assert!(
            fire_now(&w.places, &s.name, &|x| w.fake.call(x))
                .unwrap_err()
                .starts_with("trigger_not_found")
        );
        w.install(&s).unwrap();
        assert_eq!(
            fire_now(&w.places, &s.name, &|x| w.fake.call(x)).unwrap(),
            json!({"name": s.name, "fired": true})
        );
        assert_eq!(*w.fake.started.borrow(), [format!("{LABEL}{}", s.name)]);
        assert!(take_asked(&w.places, &s.name));
        // Read once: the next fire is launchd's own.
        assert!(!take_asked(&w.places, &s.name));
        // One asked for long ago is not this fire's.
        replace(&w.places.asked(&s.name), &(now() - 3600).to_string()).unwrap();
        assert!(!take_asked(&w.places, &s.name));
        // One asked while a fire runs is sent once that fire is done.
        let mut seen = Vec::new();
        fires(&w.places, &s.name, |asked| {
            if seen.is_empty() {
                fire_now(&w.places, &s.name, &|x| w.fake.call(x)).unwrap();
            }
            seen.push(asked);
        });
        assert_eq!(seen, [false, true]);
        // However long that fire ran; but not one older than its start.
        let mut seen = Vec::new();
        fires(&w.places, &s.name, |asked| {
            if seen.is_empty() {
                replace(&w.places.asked(&s.name), &(now() + 3600).to_string()).unwrap();
            } else if seen.len() == 1 {
                replace(&w.places.asked(&s.name), &(now() - 3600).to_string()).unwrap();
            }
            seen.push(asked);
        });
        assert_eq!(seen, [false, true]);
        // One launchd will not run leaves no note behind.
        w.fake.loaded.borrow_mut().clear();
        assert!(fire_now(&w.places, &s.name, &|x| w.fake.call(x)).is_err());
        assert!(!w.places.asked(&s.name).exists());
    }

    #[test]
    fn earlier_schedules_become_triggers_once() {
        let w = World::new("migrate");
        let home = w.places.state.parent().unwrap().to_owned();
        let old_state = home.join("schedules");
        std::fs::create_dir_all(&old_state).unwrap();
        std::fs::create_dir_all(&w.places.agents).unwrap();
        // What an earlier app wrote for two schedules, an ended one's result,
        // and one that is not a schedule's plist.
        let old = |s: &Trigger| {
            plist(
                Path::new("/A/agent-app"),
                s,
                &every("1d", clock(2026, 9, 28, 9, 7)).unwrap(),
                &[],
            )
            .replace(LABEL, SCHEDULE_LABEL)
            .replace(FIRE_FLAG, SCHEDULE_FIRE_FLAG)
        };
        let (a, b) = (
            trigger(),
            Trigger {
                name: "p.b".into(),
                ..trigger()
            },
        );
        for s in [&a, &b] {
            let path = w
                .places
                .agents
                .join(format!("{SCHEDULE_LABEL}{}.plist", s.name));
            std::fs::write(&path, old(s)).unwrap();
            w.fake
                .loaded
                .borrow_mut()
                .insert(format!("{SCHEDULE_LABEL}{}", s.name));
        }
        let mut result = a.json(&json!({"last": {"outcome": "sent", "turn": 3}}));
        result["generation"] = json!(a.generation);
        std::fs::write(old_state.join("p.fix-login.json"), result.to_string()).unwrap();
        std::fs::write(
            old_state.join("p.ended.json"),
            json!({"name": "p.ended", "message": "m"}).to_string(),
        )
        .unwrap();
        std::fs::write(old_state.join(".lock"), "").unwrap();
        let odd = w.places.agents.join(format!("{SCHEDULE_LABEL}p.odd.plist"));
        std::fs::write(&odd, "<plist/>").unwrap();
        std::fs::write(
            home.join("schedule"),
            "#!/bin/sh\nexec '/A/agent-app' --schedule \"$@\"\n",
        )
        .unwrap();
        // Run by p.b's own fire, whose unload is last.
        let order = RefCell::new(Vec::new());
        migrate(&w.places, Some("p.b"), &|x| {
            if let Launchd::Unload(label) = &x {
                order.borrow_mut().push(label.to_string());
            }
            w.fake.call(x)
        });
        assert_eq!(
            triggers(&w.places)
                .into_iter()
                .map(|t| t.0)
                .collect::<Vec<_>>(),
            [b.clone(), a.clone()]
        );
        assert_eq!(
            w.fake.loaded.borrow().iter().cloned().collect::<Vec<_>>(),
            [format!("{LABEL}p.b"), format!("{LABEL}p.fix-login")]
        );
        assert_eq!(
            order.borrow().last().unwrap(),
            &format!("{SCHEDULE_LABEL}p.b")
        );
        let rows = w.rows();
        assert_eq!(rows.len(), 3);
        assert_eq!(rows[0]["name"], "p.b");
        assert_eq!(
            (&rows[1]["name"], &rows[1]["ended"]),
            (&json!("p.ended"), &json!(true))
        );
        assert_eq!(rows[2]["last"]["turn"], 3);
        // What could not be read stays where it was; the rest is gone.
        assert!(odd.exists());
        assert!(!old_state.exists());
        assert!(!home.join("schedule").exists());
        // Run again, there is nothing more to do.
        let unloads = w.fake.unloads.get();
        migrate(&w.places, None, &|x| w.fake.call(x));
        assert_eq!(w.fake.unloads.get(), unloads);
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
            let refused = watches_state(&w.places, &own).unwrap_err();
            assert!(refused.starts_with("invalid_file:"), "{own:?}: {refused}");
        }
        std::os::unix::fs::symlink(&w.places.state, w.root.join("link")).unwrap();
        assert!(watches_state(&w.places, &w.root.join("link/p.x.json")).is_err());
        for other in [w.root.join(".agent"), w.root.join(".agent/triggers-not")] {
            assert_eq!(watches_state(&w.places, &other), Ok(()), "{other:?}");
        }
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
        git(&["commit", "-q", "--allow-empty", "-m", "two"]);
        assert_ne!(head(&root).unwrap(), first);
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
    fn add_takes_one_when_and_a_message() {
        let now = clock(2026, 9, 28, 23, 52);
        let words = |s: &str| s.split(' ').map(str::to_owned).collect::<Vec<_>>();
        let asked = parse_add(&words("--every 30m --bot p.x -- check the PR"), now).unwrap();
        assert_eq!(
            (asked.bot.as_deref(), asked.message.as_str()),
            (Some("p.x"), "check the PR")
        );
        let watched = parse_add(&words("--file notes.md -- x"), now).unwrap();
        assert_eq!(
            watched.when.watch,
            Some(std::path::absolute("notes.md").unwrap())
        );
        for bad in [
            "--every 30m --in 5m -- x",
            "--every 30m --file x -- x",
            "--every 30m",
            "--every 30m --",
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
