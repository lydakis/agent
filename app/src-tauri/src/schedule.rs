//! Schedules: an agent woken at set times with a message it wrote for
//! itself, or one written for it. launchd keeps the time, so a schedule
//! fires with the app closed, and once on waking for times the Mac slept
//! through. The daemon has no clock of its own.
//!
//! A schedule is one LaunchAgent, `~/Library/LaunchAgents/LABEL.plist`,
//! which runs this executable with `--schedule-fire` and everything the fire
//! needs as its arguments: the plist is the only record. A fire sends the
//! message as a new turn of the bot the schedule was made for, pinned by its
//! id, and never to a bot that is working: that time is skipped. It never
//! creates a bot. A bot deleted since takes its schedules with it.
//!
//! `~/.agent/schedule`, a script the app writes, is how agents and people
//! add, list and remove them.
use agent_client::Client;
use serde_json::{Value, json};
use std::{
    io::Write,
    path::{Path, PathBuf},
};

pub const FLAG: &str = "--schedule";
pub const FIRE_FLAG: &str = "--schedule-fire";
/// Every schedule's launchd label starts with this; the rest is its name.
const LABEL: &str = "me.lydakis.agent.schedule.";
/// A message is a reminder of what to do, not a document.
const MAX_MESSAGE: usize = 16 * 1024;
/// Calendar entries one schedule may expand to.
const MAX_ENTRIES: usize = 1024;
/// launchd's calendar has no year, so a one-off time must fall within one.
const MAX_AHEAD: i64 = 364 * 24 * 3600;
/// How far from its time a one-off's fire may be and still be its own: a
/// time zone changed since it was made moves launchd's clock by up to a day.
const SLACK: i64 = 2 * 24 * 3600;
/// A one-off's calendar entry comes again a year later; a fire this late
/// is that, not a wake after a long sleep.
const STALE: i64 = 182 * 24 * 3600;
const USAGE: &str = "usage: schedule add [--bot NAME] [--name NAME] (--every N{m,h,d} | --in N{m,h} | --at 'YYYY-MM-DD HH:MM' | --cron 'MIN HOUR DAY MONTH WEEKDAY') -- MESSAGE\n       schedule ls [--after NAME]\n       schedule rm NAME";

/// Where schedules live: the LaunchAgents folder holds their plists, and
/// `~/.agent/schedules` what each one's last fire did.
pub struct Places {
    pub agents: PathBuf,
    pub state: PathBuf,
}

impl Places {
    pub fn home() -> Result<Self, String> {
        let home = PathBuf::from(std::env::var_os("HOME").ok_or("no HOME for schedules")?);
        Ok(Self {
            agents: home.join("Library/LaunchAgents"),
            state: home.join(".agent/schedules"),
        })
    }
    fn plist(&self, name: &str) -> PathBuf {
        self.agents.join(format!("{LABEL}{name}.plist"))
    }
    fn last(&self, name: &str) -> PathBuf {
        self.state.join(format!("{name}.json"))
    }
}

/// What launchd is asked to do. Tests stand in for it.
pub enum Launchd<'a> {
    Load(&'a Path),
    Unload(&'a str),
}

pub type Loader<'a> = &'a dyn Fn(Launchd) -> Result<(), String>;

/// `launchctl` in the logged-in user's domain. Other systems have no
/// launchd, and schedules are refused there.
pub fn launchctl(what: Launchd) -> Result<(), String> {
    if !cfg!(target_os = "macos") {
        return Err("schedules_unsupported: schedules use launchd, which only macOS has".into());
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

/// Which daemon a schedule reaches: its store, which a fire may start the
/// daemon of, and the socket that daemon listens on, when it was given one.
/// A socket without its store is reached but never started.
#[derive(Debug, Clone, PartialEq)]
pub struct Daemon {
    pub store: Option<PathBuf>,
    pub socket: Option<PathBuf>,
}

impl Daemon {
    /// The daemon of the shell the schedule is made from, found as the CLI
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

/// One schedule, as its plist's arguments carry it.
#[derive(Debug, Clone, PartialEq)]
pub struct Schedule {
    pub name: String,
    pub generation: String,
    pub not_before: Option<i64>,
    pub bot: String,
    pub bot_id: i64,
    /// How it was asked for, to show: `every 30m`, `at 2026-09-29 09:07`.
    pub when: String,
    /// A one-off's time; it fires once, then removes itself.
    pub at: Option<i64>,
    pub daemon: Daemon,
    /// The store identity its daemon announced when it was made; a daemon
    /// on that socket serving another store is not its daemon.
    pub store_id: String,
    pub message: String,
}

impl Schedule {
    /// The fire's arguments, after the executable.
    fn args(&self) -> Vec<String> {
        let mut args = vec![
            FIRE_FLAG.into(),
            "--name".into(),
            self.name.clone(),
            "--generation".into(),
            self.generation.clone(),
            "--bot".into(),
            self.bot.clone(),
            "--bot-id".into(),
            self.bot_id.to_string(),
            "--when".into(),
            self.when.clone(),
        ];
        if let Some(first) = self.not_before {
            args.extend(["--not-before".into(), first.to_string()]);
        }
        if let Some(at) = self.at {
            args.extend(["--at".into(), at.to_string()]);
        }
        if let Some(store) = &self.daemon.store {
            args.extend(["--store".into(), store.to_string_lossy().into()]);
        }
        if let Some(socket) = &self.daemon.socket {
            args.extend(["--socket".into(), socket.to_string_lossy().into()]);
        }
        args.extend(["--store-id".into(), self.store_id.clone()]);
        args.extend(["--".into(), self.message.clone()]);
        args
    }

    /// Read back from a fire's arguments, after the flag.
    fn parse(args: &[String]) -> Result<Self, String> {
        let bad = |what: &str| format!("invalid_schedule: {what}");
        let (mut name, mut bot, mut id, mut when, mut at, mut store_id) =
            (None, None, None, None, None, None);
        let (mut generation, mut not_before) = (None, None);
        let mut daemon = Daemon {
            store: None,
            socket: None,
        };
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
            match flag.as_str() {
                "--name" => name = Some(value.clone()),
                "--generation" => generation = Some(value.clone()),
                "--not-before" => {
                    not_before = Some(value.parse().map_err(|_| bad("--not-before"))?)
                }
                "--bot" => bot = Some(value.clone()),
                "--bot-id" => id = value.parse().ok(),
                "--when" => when = Some(value.clone()),
                "--at" => at = Some(value.parse().map_err(|_| bad("--at"))?),
                "--store" => daemon.store = Some(value.into()),
                "--socket" => daemon.socket = Some(value.into()),
                "--store-id" => store_id = Some(value.clone()),
                other => return Err(bad(other)),
            }
        };
        Ok(Self {
            name: name.ok_or_else(|| bad("no --name"))?,
            generation: generation.ok_or_else(|| bad("no --generation"))?,
            not_before,
            bot: bot.ok_or_else(|| bad("no --bot"))?,
            bot_id: id.ok_or_else(|| bad("no --bot-id"))?,
            when: when.ok_or_else(|| bad("no --when"))?,
            at,
            daemon: (daemon.store.is_some() || daemon.socket.is_some())
                .then_some(daemon)
                .ok_or_else(|| bad("no --store or --socket"))?,
            store_id: store_id.ok_or_else(|| bad("no --store-id"))?,
            message,
        })
    }

    fn json(&self, last: Option<Value>) -> Value {
        json!({"name": self.name, "generation": self.generation, "bot": self.bot, "bot_id": self.bot_id, "when": self.when,
            "message": self.message, "once": self.at.is_some(), "last": last})
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

/// When, as asked for: how to show it, launchd's entries, and a one-off's
/// time.
#[derive(Debug, PartialEq)]
pub struct When {
    pub text: String,
    pub entries: Vec<Entry>,
    pub not_before: Option<i64>,
    pub at: Option<i64>,
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

/// The LaunchAgent for a schedule. `environment` is what the fire starts
/// with besides launchd's own: the shell whose login environment starts a
/// daemon with your keys, as the app starts one.
pub fn plist(
    app: &Path,
    schedule: &Schedule,
    when: &[Entry],
    environment: &[(&str, String)],
) -> String {
    let mut out = String::from(
        "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n<!DOCTYPE plist PUBLIC \"-//Apple//DTD PLIST 1.0//EN\" \"http://www.apple.com/DTDs/PropertyList-1.0.dtd\">\n<plist version=\"1.0\">\n<dict>\n",
    );
    let string = |s: &str| format!("<string>{}</string>", escape(s));
    out += &format!(
        "  <key>Label</key>\n  {}\n",
        string(&format!("{LABEL}{}", schedule.name))
    );
    out += "  <key>ProgramArguments</key>\n  <array>\n";
    for arg in std::iter::once(app.to_string_lossy().into_owned()).chain(schedule.args()) {
        out += &format!("    {}\n", string(&arg));
    }
    out += "  </array>\n  <key>StartCalendarInterval</key>\n  <array>\n";
    for entry in when {
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

/// The schedule a plist runs, and the app it runs it with.
fn read_plist(text: &str) -> Option<(Schedule, PathBuf)> {
    let args = program(text)?;
    let (app, rest) = args.split_first()?;
    let schedule = Schedule::parse(rest.strip_prefix(&[FIRE_FLAG.to_owned()])?).ok()?;
    Some((schedule, PathBuf::from(app)))
}

/// A plist's schedule and app, or why it could not be read.
type Read = Result<(Schedule, PathBuf), String>;

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
        return Err("unreadable: schedule record too large".into());
    }
    Ok(text)
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
            let read = read_record(&e.path()).and_then(|text| {
                read_plist(&text).ok_or_else(|| "unreadable: not a schedule's plist".into())
            });
            Some((name, read))
        })
}

#[cfg(test)]
fn schedules(places: &Places) -> Vec<(Schedule, PathBuf)> {
    read_all(places).filter_map(|(_, r)| r.ok()).collect()
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
    let read = |path: &Path| {
        read_record(path)
            .ok()
            .and_then(|s| serde_json::from_str::<Value>(&s).ok())
    };
    let rows: Vec<_> = names
        .into_iter()
        .map(|name| {
            let path = places.plist(&name);
            if !path.exists() {
                let mut row = read(&places.last(&name))
                    .filter(|r| r["name"] == name)
                    .unwrap_or_else(|| json!({"name": name, "problem": "unreadable: result"}));
                row["ended"] = json!(true);
                return row;
            }
            match read_record(&path).and_then(|text| {
                read_plist(&text).ok_or_else(|| "unreadable: not a schedule's plist".into())
            }) {
                Ok((s, _)) => {
                    let last = read(&places.last(&name))
                        .filter(|r| r["generation"] == s.generation)
                        .map(|r| r["last"].clone());
                    let mut row = s.json(last);
                    row["ended"] = json!(false);
                    if s.at.is_some_and(|at| now() > at + SLACK) {
                        row["missed"] = json!(true);
                    }
                    row
                }
                Err(problem) => json!({"name": name, "ended": false, "problem": problem}),
            }
        })
        .collect();
    json!({"schedules": rows, "next_after": next})
}

/// Write a file whole beside its place, then rename it there.
pub(crate) fn replace(path: &Path, text: &str) -> Result<(), String> {
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
        // Created anew, never opened through whatever an old temporary of
        // this pid left at the name, a link included.
        match std::fs::remove_file(&temporary) {
            Err(e) if e.kind() != std::io::ErrorKind::NotFound => return Err(e),
            _ => {}
        }
        let mut file = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(mode)
            .open(&temporary)?;
        // The umask may have narrowed the mode.
        file.set_permissions(std::os::unix::fs::PermissionsExt::from_mode(mode))?;
        file.write_all(text.as_bytes())?;
        file.sync_all()?;
        std::fs::rename(&temporary, path)?;
        reap(dir, path);
        // The new name is durable only once its folder is.
        std::fs::File::open(dir)?.sync_all()
    })();
    if written.is_err() {
        let _ = std::fs::remove_file(&temporary);
    }
    written.map_err(|e| format!("{}: {e}", path.display()))
}

/// Remove the temporaries of `path` that processes now gone left behind,
/// such as one killed between its write and its rename.
fn reap(dir: &Path, path: &Path) {
    let prefix = format!(
        ".{}.",
        path.file_name().unwrap_or_default().to_string_lossy()
    );
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let name = entry.file_name();
        let Some(pid) = name
            .to_str()
            .and_then(|name| name.strip_prefix(&prefix))
            .filter(|pid| pid.bytes().all(|b| b.is_ascii_digit()))
            .and_then(|pid| pid.parse::<libc::pid_t>().ok())
        else {
            continue;
        };
        // SAFETY: signal 0 sends nothing; it only asks whether pid exists.
        let gone = pid > 0
            && unsafe { libc::kill(pid, 0) } == -1
            && std::io::Error::last_os_error().raw_os_error() == Some(libc::ESRCH);
        if gone {
            let _ = std::fs::remove_file(entry.path());
        }
    }
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

/// Write the schedule's plist and load it, replacing one of the same name.
/// A replacement that fails puts the old one back as it was.
pub fn install(
    places: &Places,
    app: &Path,
    schedule: &Schedule,
    when: &[Entry],
    environment: &[(&str, String)],
    launchd: Loader,
) -> Result<(), String> {
    valid_name(&schedule.name)?;
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
    let taken = taken(&places.agents, &format!("{LABEL}{}.plist", schedule.name))
        .map(|f| f[LABEL.len()..f.len() - ".plist".len()].to_owned())
        .or_else(|| {
            taken(&places.state, &format!("{}.json", schedule.name))
                .map(|f| f[..f.len() - ".json".len()].to_owned())
        });
    if let Some(other) = taken {
        return Err(format!(
            "name_taken: {}: schedule {other} differs only in case; pass --name",
            schedule.name
        ));
    }
    let path = places.plist(&schedule.name);
    // One that cannot be read cannot be put back, so it is not replaced.
    let old = match std::fs::read_to_string(&path) {
        Ok(old) => Some(old),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => None,
        Err(e) => return Err(format!("{}: {e}", path.display())),
    };
    swap(
        &path,
        &schedule.name,
        old.as_deref(),
        &plist(app, schedule, when, environment),
        launchd,
    )
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
            "invalid_schedule: serialized plist exceeds {MAX_RECORD} bytes; narrow --cron or shorten the message"
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

/// Held while a schedule's files or launchd's job change, so `add`, `rm`,
/// a fire ending its schedule and the app's refresh never interleave. The
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
pub(crate) fn forget(path: &Path) -> Result<(), String> {
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

/// Remove a schedule: launchd's copy, then its plist and last result. An
/// unload launchd refuses keeps the plist, so the removal can be retried.
/// Whatever is left of it goes, a job loaded without its plist included.
pub fn remove(places: &Places, name: &str, launchd: Loader) -> Result<(), String> {
    valid_name(name)?;
    let _lock = Lock::take(places)?;
    let path = places.plist(name);
    let last = places.last(name);
    // A folder that ignores case finds `build`'s files for `Build`, whose
    // label launchd does not have: only the name as stored is that schedule.
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
        return Err(format!("schedule_not_found: {name}"));
    }
    if path_here {
        forget(&path)?;
    }
    if last_here {
        forget(&last)?;
    }
    Ok(())
}

/// What a fire did goes on disk, and a schedule that is over ends: both
/// under the lock, and only while the plist is still this schedule's. One
/// replaced or removed while its message went out is left as it now is; a
/// job left loaded after its plist went (an end cut short) is unloaded.
fn settle(places: &Places, schedule: &Schedule, outcome: &Value, launchd: Loader) {
    let log = |error: String| eprintln!("{}", json!({"error": error}));
    let _lock = match Lock::take(places) {
        Ok(lock) => lock,
        Err(error) => return log(error),
    };
    let path = places.plist(&schedule.name);
    let text = match std::fs::read_to_string(&path) {
        Ok(text) => text,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            // Under the lock no `add` is between writing and loading one.
            if let Err(error) = unload(&format!("{LABEL}{}", schedule.name), launchd) {
                log(error);
            }
            return;
        }
        Err(e) => return log(format!("{}: {e}", path.display())),
    };
    if read_plist(&text).is_none_or(|(now, _)| now != *schedule) {
        return;
    }
    let recorded = record_last(places, schedule, outcome);
    if let Err(error) = &recorded {
        log(error.clone());
    }
    let sent = outcome["outcome"] == "sent";
    // One that did not deliver ends only once why is on disk; else its plist
    // stays, listed.
    if (schedule.at.is_some() || outcome["outcome"] == "gone") && (sent || recorded.is_ok()) {
        end(places, &schedule.name, &path, &text, !sent, launchd);
    }
}

/// A schedule ends itself: its plist first, then launchd's copy, whose
/// unload ends this process. `keep` leaves its last result, so an end nobody
/// asked for still shows, and why. A plist that will not go stays loaded,
/// listed, for `rm`: unloaded, it would load again at the next login. An
/// unload launchd refuses writes the plist back, so the job still loaded
/// stays listed for `rm`. Called under the lock.
fn end(places: &Places, name: &str, path: &Path, text: &str, keep: bool, launchd: Loader) {
    let log = |error: String| eprintln!("{}", json!({"error": error}));
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
        // Still loaded, it is still this schedule, with what its fire did.
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
/// schedule again when it starts from somewhere else. One that fails keeps
/// the old path, so the next start tries it again.
pub fn refresh(places: &Places, app: &Path, launchd: Loader) {
    for (schedule, _) in read_all(places).filter_map(|(_, r)| r.ok()) {
        let Ok(_lock) = Lock::take(places) else {
            return;
        };
        // Read again under the lock: an `rm`, `add` or fire may have come first.
        let path = places.plist(&schedule.name);
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
        let _ = swap(&path, &schedule.name, Some(&text), &moved, launchd);
    }
}

/// What `schedule add` was asked.
#[derive(Debug, PartialEq)]
struct Add {
    bot: Option<String>,
    name: Option<String>,
    when: When,
    message: String,
}

fn parse_add(args: &[String], now: i64) -> Result<Add, String> {
    let bad = |what: String| format!("{what}\n{USAGE}");
    let (mut bot, mut name, mut when) = (None, None, None);
    let mut iter = args.iter();
    let message = loop {
        let Some(flag) = iter.next() else {
            return Err(bad("invalid_schedule: a message goes after --".into()));
        };
        if flag == "--" {
            break iter.cloned().collect::<Vec<_>>().join(" ");
        }
        let value = iter
            .next()
            .ok_or_else(|| bad(format!("invalid_schedule: {flag} needs a value")))?;
        let asked = match flag.as_str() {
            "--bot" => {
                bot = Some(value.clone());
                continue;
            }
            "--name" => {
                name = Some(value.clone());
                continue;
            }
            "--every" => every(value, now)?,
            "--in" => after(value, now)?,
            "--at" => at(value, now)?,
            "--cron" => cron(value)?,
            other => return Err(bad(format!("invalid_schedule: {other}"))),
        };
        if when.replace(asked).is_some() {
            return Err(bad(
                "invalid_schedule: one of --every, --in, --at or --cron".into(),
            ));
        }
    };
    let when = when.ok_or_else(|| bad("invalid_schedule: --every, --in, --at or --cron".into()))?;
    if message.trim().is_empty() {
        return Err(bad("invalid_schedule: a message goes after --".into()));
    }
    if message.len() > MAX_MESSAGE {
        return Err(format!(
            "invalid_schedule: a message is at most {MAX_MESSAGE} bytes"
        ));
    }
    // XML 1.0 has no place for other control characters.
    if message
        .chars()
        .any(|c| c.is_control() && !matches!(c, '\n' | '\t' | '\r'))
    {
        return Err("invalid_schedule: the message has control characters".into());
    }
    Ok(Add {
        bot,
        name,
        when,
        message,
    })
}

fn runtime() -> Result<tokio::runtime::Runtime, String> {
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .map_err(|e| e.to_string())
}

/// `APP --schedule add|ls|rm`, from `~/.agent/schedule`.
pub fn cli(args: &[String]) -> i32 {
    let fail = |message: String| {
        eprintln!("{}", json!({"error": message}));
        1
    };
    let places = match Places::home() {
        Ok(places) => places,
        Err(error) => return fail(error),
    };
    match args.split_first() {
        Some((verb, rest)) if verb == "ls" => {
            let after = match rest {
                [] => None,
                [flag, name] if flag == "--after" => Some(name.as_str()),
                _ => return fail(USAGE.into()),
            };
            println!("{}", list(&places, after));
            0
        }
        Some((verb, [name])) if verb == "rm" => match remove(&places, name, &launchctl) {
            Ok(()) => {
                println!("{}", json!({"removed": name}));
                0
            }
            Err(error) => fail(error),
        },
        Some((verb, rest)) if verb == "add" => match add(&places, rest) {
            Ok(value) => {
                println!("{value}");
                0
            }
            Err(error) => fail(error),
        },
        _ => fail(USAGE.into()),
    }
}

fn add(places: &Places, args: &[String]) -> Result<Value, String> {
    let asked = parse_add(args, now())?;
    let implicit = asked.bot.is_none();
    let expected_id = if implicit {
        Some(
            std::env::var("AGENT_BOT_ID")
                .ok()
                .and_then(|v| v.parse::<i64>().ok())
                .ok_or("invalid_schedule: implicit --bot needs AGENT_BOT_ID")?,
        )
    } else {
        None
    };
    let bot = match asked.bot {
        Some(bot) => bot,
        None => std::env::var("AGENT_BOT")
            .ok()
            .filter(|b| !b.is_empty())
            .ok_or("invalid_schedule: --bot NAME, or run it from an agent's shell")?,
    };
    let daemon = Daemon::current()?;
    let socket = daemon.socket()?;
    // The bot must exist now; the schedule is pinned to this identity.
    let (record, store_id) = runtime()?.block_on(async {
        let client = connect(&socket, &daemon).await?;
        let record = client.request("resume", json!({"bot": bot})).await;
        let store_id = client
            .store()
            .map(str::to_owned)
            .ok_or_else(|| "the daemon announced no store identity".to_owned());
        client.close().await;
        Ok::<_, String>((record.map_err(|e| e.to_string())?, store_id?))
    })?;
    let bot_id = record["id"].as_i64().ok_or("the daemon named no bot id")?;
    if expected_id.is_some_and(|id| id != bot_id) {
        return Err("bot_not_found: the shell's bot identity no longer exists".into());
    }
    let schedule = Schedule {
        generation: format!(
            "{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map_err(|e| e.to_string())?
                .as_nanos()
        ),
        not_before: asked.when.not_before,
        name: asked.name.unwrap_or_else(|| bot.clone()),
        bot,
        bot_id,
        when: asked.when.text,
        at: asked.when.at,
        daemon,
        store_id,
        message: asked.message,
    };
    let app = std::env::current_exe().map_err(|e| e.to_string())?;
    let environment: Vec<(&str, String)> = ["HOME", "SHELL"]
        .into_iter()
        .filter_map(|key| std::env::var(key).ok().map(|v| (key, v)))
        .collect();
    install(
        places,
        &app,
        &schedule,
        &asked.when.entries,
        &environment,
        &launchctl,
    )?;
    Ok(schedule.json(None))
}

/// What a fire did, kept for the app to show.
/// It is the schedule's whole row, which Settings still shows once the
/// schedule has ended on its own.
fn record_last(places: &Places, schedule: &Schedule, outcome: &Value) -> Result<(), String> {
    let mut outcome = outcome.clone();
    outcome["fired_ms"] = json!(now() * 1000);
    let row = schedule.json(Some(outcome));
    replace(&places.last(&schedule.name), &row.to_string())
}

/// Send the message: a new turn when the bot is resting; a working bot, or
/// one with work waiting, skips this time of a repeating schedule, and gets
/// a one-off's message after its work. A deleted bot's schedule goes.
pub async fn send(client: &Client, schedule: &Schedule) -> Value {
    let delivery = if schedule.at.is_some() {
        "queue"
    } else {
        "reject"
    };
    let submitted = client
        .request(
            "submit",
            json!({"bot": schedule.bot, "bot_id": schedule.bot_id,
                "request_id": format!("schedule-{}-{}-{}", schedule.bot_id, now(), std::process::id()),
                "prompt": schedule.message, "delivery": delivery, "origin": "schedule"}),
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

/// `APP --schedule-fire ...`, run by launchd.
pub fn fire_cli(args: &[String]) -> i32 {
    let schedule = match Schedule::parse(args) {
        Ok(schedule) => schedule,
        Err(error) => {
            eprintln!("{}", json!({"error": error}));
            return 1;
        }
    };
    let places = match Places::home() {
        Ok(places) => places,
        Err(error) => {
            eprintln!("{}", json!({"error": error}));
            return 1;
        }
    };
    let now = now();
    // A one-off's calendar entry has no year: the same date a year early is
    // not its time. A day's slack keeps its time when the Mac's time zone
    // changed since it was made, which launchd follows and `at` does not.
    if schedule.at.is_some_and(|at| now < at - SLACK) {
        return 0;
    }
    // Months late is the entry's next year: the Mac was off at its time, or
    // its end was cut short. It is not sent; if it is still listed, it ends
    // saying so.
    if schedule.not_before.is_some_and(|first| now < first) {
        return 0;
    }
    if schedule.at.is_some_and(|at| now > at + STALE) {
        let missed = json!({"outcome": "missed", "detail": "its time passed long ago"});
        settle(&places, &schedule, &missed, &launchctl);
        return 0;
    }
    let outcome = match runtime() {
        Ok(runtime) => runtime.block_on(async {
            let socket = match schedule.daemon.socket() {
                Ok(socket) => socket,
                Err(error) => return json!({"outcome": "failed", "detail": error}),
            };
            let client = match connect(&socket, &schedule.daemon).await {
                Ok(client) => client,
                Err(error) => return json!({"outcome": "failed", "detail": error}),
            };
            // The socket may now be another store's daemon's; its bot ids are its own.
            if client.store() != Some(schedule.store_id.as_str()) {
                client.close().await;
                return json!({"outcome": "failed", "detail": format!(
                    "store_mismatch: the daemon at {} serves another store", socket.display())});
            }
            let outcome = send(&client, &schedule).await;
            client.close().await;
            outcome
        }),
        Err(error) => json!({"outcome": "failed", "detail": error}),
    };
    settle(&places, &schedule, &outcome, &launchctl);
    0
}

/// The daemon, started the way the app starts one when none answers and
/// the schedule names its store, on the socket the schedule was made with.
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

/// `~/.agent/schedule`, written again whenever the app starts from
/// somewhere else.
pub fn write_script(home: &Path, app: &Path) -> Result<(), String> {
    let quote = |p: &Path| format!("'{}'", p.to_string_lossy().replace('\'', r"'\''"));
    let usage = USAGE.replace('\n', "\n# ");
    let text = format!("#!/bin/sh\n# {usage}\nexec {} {FLAG} \"$@\"\n", quote(app));
    let path = home.join("schedule");
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

    #[test]
    fn a_temporary_a_gone_process_left_is_reaped() {
        let dir = std::env::temp_dir().join(format!("agent-app-reap-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let mut child = std::process::Command::new("true").spawn().unwrap();
        let gone = child.id();
        child.wait().unwrap();
        let (dead, live) = (dir.join(format!(".x.{gone}")), dir.join(".x.1"));
        std::fs::write(&dead, "").unwrap();
        std::fs::write(&live, "").unwrap();
        replace(&dir.join("x"), "text").unwrap();
        assert!(!dead.exists());
        assert!(live.exists());
        std::fs::remove_dir_all(dir).unwrap();
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

    fn schedule() -> Schedule {
        Schedule {
            generation: "test-1".into(),
            not_before: None,
            name: "p.fix-login".into(),
            bot: "p.fix-login".into(),
            bot_id: 42,
            when: "every 30m".into(),
            at: None,
            daemon: Daemon {
                store: Some("/Users/a/.agent/state.sqlite".into()),
                socket: Some("/tmp/s".into()),
            },
            store_id: "00ab".into(),
            message: "Check the PR's CI & reviews; <fix> what's \"red\".\nThen say so.".into(),
        }
    }

    #[test]
    fn a_plist_carries_everything_a_fire_needs() {
        let s = schedule();
        let when = every("30m", clock(2026, 9, 28, 23, 52)).unwrap();
        let text = plist(
            Path::new("/Applications/Agent.app/Contents/MacOS/agent-app"),
            &s,
            &when.entries,
            &[("SHELL", "/bin/zsh".into())],
        );
        assert!(text.contains(
            "<key>Label</key>\n  <string>me.lydakis.agent.schedule.p.fix-login</string>"
        ));
        assert!(text.contains("<dict><key>Minute</key><integer>22</integer></dict>"));
        assert!(text.contains("<key>SHELL</key><string>/bin/zsh</string>"));
        let args = program(&text).unwrap();
        assert_eq!(args[0], "/Applications/Agent.app/Contents/MacOS/agent-app");
        assert_eq!(args[1], FIRE_FLAG);
        assert_eq!(Schedule::parse(&args[2..]).unwrap(), s);
        let once = Schedule {
            at: Some(1_790_000_000),
            daemon: Daemon {
                store: None,
                socket: Some("/tmp/s".into()),
            },
            ..s
        };
        assert_eq!(Schedule::parse(&once.args()[1..]).unwrap(), once);
    }

    #[test]
    fn add_replaces_and_rm_removes_through_launchd() {
        let root = std::env::temp_dir().join(format!("agent-app-schedule-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        let places = Places {
            agents: root.join("LaunchAgents"),
            state: root.join("schedules"),
        };
        let asked = RefCell::new(Vec::new());
        let launchd = |what: Launchd| {
            let unloading = match what {
                Launchd::Load(path) => {
                    asked.borrow_mut().push(format!(
                        "load {}",
                        path.file_name().unwrap().to_string_lossy()
                    ));
                    return Ok(());
                }
                Launchd::Unload(label) => label.to_owned(),
            };
            asked.borrow_mut().push(format!("unload {unloading}"));
            // launchd's labels keep their case.
            match unloading.ends_with("p.fix-login") {
                true => Ok(()),
                false => Err(format!("{NOT_LOADED}no such process")),
            }
        };
        let app = Path::new("/A/agent-app");
        let s = schedule();
        let entries = every("1d", clock(2026, 9, 28, 9, 7)).unwrap().entries;
        install(&places, app, &s, &entries, &[], &launchd).unwrap();
        install(&places, app, &s, &entries, &[], &launchd).unwrap();
        assert_eq!(
            *asked.borrow(),
            [
                "unload me.lydakis.agent.schedule.p.fix-login",
                "load me.lydakis.agent.schedule.p.fix-login.plist",
                "unload me.lydakis.agent.schedule.p.fix-login",
                "load me.lydakis.agent.schedule.p.fix-login.plist",
            ]
        );
        record_last(&places, &s, &json!({"outcome": "skipped"})).unwrap();
        let listed = list(&places, None)["schedules"].clone();
        assert_eq!(listed.as_array().unwrap().len(), 1);
        assert_eq!(listed[0]["bot"], "p.fix-login");
        assert_eq!(listed[0]["message"], s.message);
        assert_eq!(listed[0]["last"]["outcome"], "skipped");
        // A replacement starts with no last time of its own, while retaining
        // the prior result for a rolled-back plist after interruption.
        let s = Schedule {
            generation: "test-2".into(),
            ..s
        };
        install(&places, app, &s, &entries, &[], &launchd).unwrap();
        assert!(list(&places, None)["schedules"].clone()[0]["last"].is_null());
        // A load launchd refuses leaves nothing behind.
        let other = Schedule {
            name: "p.other".into(),
            ..schedule()
        };
        let refused = install(&places, app, &other, &entries, &[], &|what| match what {
            Launchd::Load(_) => Err("launchctl bootstrap: refused".into()),
            Launchd::Unload(_) => Ok(()),
        });
        assert!(refused.unwrap_err().contains("refused"));
        assert_eq!(
            list(&places, None)["schedules"]
                .clone()
                .as_array()
                .unwrap()
                .len(),
            1
        );
        // A move launchd refuses to load keeps the old path, to be tried again.
        let refused = |what: Launchd| match what {
            Launchd::Load(path) if std::fs::read_to_string(path).unwrap().contains("/B/") => {
                Err("launchctl bootstrap: refused".to_owned())
            }
            _ => Ok(()),
        };
        refresh(&places, Path::new("/B/agent-app"), &refused);
        assert_eq!(schedules(&places)[0].1, Path::new("/A/agent-app"));
        // A name differing only in case would share its files on macOS.
        let cased = Schedule {
            name: "P.Fix-Login".into(),
            ..schedule()
        };
        let taken = install(&places, app, &cased, &entries, &[], &launchd).unwrap_err();
        assert!(
            taken.starts_with("name_taken: P.Fix-Login: schedule p.fix-login"),
            "{taken}"
        );
        // Nor does `rm` of that other case reach it, even where the folder would alias it.
        assert!(
            remove(&places, "P.Fix-Login", &launchd)
                .unwrap_err()
                .starts_with("schedule_not_found")
        );
        // A move of the app is written into every schedule and reloaded.
        asked.borrow_mut().clear();
        refresh(&places, Path::new("/B/agent-app"), &launchd);
        assert_eq!(schedules(&places)[0].1, Path::new("/B/agent-app"));
        assert_eq!(schedules(&places)[0].0, s);
        assert_eq!(asked.borrow().len(), 2);
        refresh(&places, Path::new("/B/agent-app"), &launchd);
        assert_eq!(asked.borrow().len(), 2, "an unmoved app changes nothing");
        remove(&places, &s.name, &launchd).unwrap();
        assert_eq!(list(&places, None)["schedules"].clone(), json!([]));
        assert!(!places.last(&s.name).exists());
        assert!(
            install(
                &places,
                app,
                &Schedule {
                    name: "../x".into(),
                    ..schedule()
                },
                &entries,
                &[],
                &launchd
            )
            .is_err()
        );
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn the_script_is_runnable_even_when_its_text_was_already_there() {
        use std::os::unix::fs::PermissionsExt;
        let root = std::env::temp_dir().join(format!("agent-app-script-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        let app = Path::new("/A/agent-app");
        write_script(&root, app).unwrap();
        let path = root.join("schedule");
        let mode = |p: &Path| std::fs::metadata(p).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode(&path), 0o755);
        // Its text survived a crash before its mode did.
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644)).unwrap();
        write_script(&root, app).unwrap();
        assert_eq!(mode(&path), 0o755);
        std::fs::remove_dir_all(root).unwrap();
    }

    /// launchd as schedules see it: which labels are loaded, and what it
    /// refuses.
    #[derive(Default)]
    struct Fake {
        loaded: RefCell<std::collections::BTreeSet<String>>,
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
                state: root.join("schedules"),
            };
            Self {
                root,
                places,
                fake: Fake::default(),
            }
        }
        fn install(&self, s: &Schedule) -> Result<(), String> {
            let entries = every("1d", clock(2026, 9, 28, 9, 7)).unwrap().entries;
            install(
                &self.places,
                Path::new("/A/agent-app"),
                s,
                &entries,
                &[],
                &|w| self.fake.call(w),
            )
        }
        fn settle(&self, s: &Schedule, outcome: Value) {
            settle(&self.places, s, &outcome, &|w| self.fake.call(w));
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
    }

    impl Drop for World {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.root);
        }
    }

    fn once() -> Schedule {
        Schedule {
            name: "p.once".into(),
            when: "in 2h".into(),
            at: Some(1_790_000_000),
            ..schedule()
        }
    }

    #[test]
    fn oversized_plists_are_refused_before_installing_or_replacing_a_schedule() {
        let w = World::new("record-limit");
        let broad = cron("0-7 0-7 1-4 1-4 *").unwrap();
        let s = Schedule {
            when: broad.text,
            ..schedule()
        };
        let app = Path::new("/A/agent-app");
        let oversized = plist(app, &s, &broad.entries, &[]);
        assert!(oversized.len() as u64 > MAX_RECORD);
        let install_broad = || {
            install(&w.places, app, &s, &broad.entries, &[], &|what| {
                w.fake.call(what)
            })
        };
        assert!(
            install_broad()
                .unwrap_err()
                .starts_with("invalid_schedule:")
        );
        assert_eq!(w.state(&s.name), (false, false, false));
        assert_eq!(w.fake.unloads.get(), 0);

        // A large readable plist, including XML expansion, still lists and
        // follows an app move. The limit is serialized bytes, not message bytes.
        let when = cron("0-7 0-7 1-2 1-4 *").unwrap();
        let kept = Schedule {
            when: when.text,
            message: "&".repeat(MAX_MESSAGE / 2),
            ..s.clone()
        };
        install(&w.places, app, &kept, &when.entries, &[], &|what| {
            w.fake.call(what)
        })
        .unwrap();
        let text = w.plist(&s.name);
        assert!(text.len() as u64 > MAX_RECORD * 9 / 10);
        assert!(text.len() as u64 <= MAX_RECORD);
        assert_eq!(
            list(&w.places, None)["schedules"][0]["message"],
            kept.message
        );
        refresh(&w.places, Path::new("/B/agent-app"), &|what| {
            w.fake.call(what)
        });
        let before = w.plist(&s.name);
        assert_eq!(read_plist(&before).unwrap().1, Path::new("/B/agent-app"));
        record_last(&w.places, &kept, &json!({"outcome": "sent", "turn": 7})).unwrap();
        let unloads = w.fake.unloads.get();

        assert!(
            install_broad()
                .unwrap_err()
                .starts_with("invalid_schedule:")
        );
        assert_eq!(w.fake.unloads.get(), unloads);
        assert_eq!(w.plist(&s.name), before);
        assert_eq!(w.state(&s.name), (true, true, true));
        assert_eq!(list(&w.places, None)["schedules"][0]["last"]["turn"], 7);
    }

    #[test]
    fn pages_bound_messages_and_keep_replacement_results_with_their_generation() {
        let w = World::new("pages");
        let s = schedule();
        w.install(&s).unwrap();
        record_last(&w.places, &s, &json!({"outcome": "sent", "turn": 7})).unwrap();
        let prior = std::fs::read_to_string(w.places.last(&s.name)).unwrap();
        let changed = Schedule {
            generation: "test-next".into(),
            ..s.clone()
        };
        install(&w.places, Path::new("/A/app"), &changed, &[], &[], &|_| {
            // A crash at any launchd transition still leaves the prior result.
            assert_eq!(
                std::fs::read_to_string(w.places.last(&s.name)).unwrap(),
                prior
            );
            Ok(())
        })
        .unwrap();
        assert!(list(&w.places, None)["schedules"][0]["last"].is_null());
        // A restored old plist sees its own result, without a result rollback.
        std::fs::write(
            w.places.plist(&s.name),
            plist(Path::new("/A/app"), &s, &[], &[]),
        )
        .unwrap();
        assert_eq!(list(&w.places, None)["schedules"][0]["last"]["turn"], 7);
        for i in 0..PAGE_SIZE + 2 {
            let item = Schedule {
                name: format!("task-{i:03}"),
                message: "x".repeat(MAX_MESSAGE),
                ..s.clone()
            };
            std::fs::write(
                w.places.plist(&item.name),
                plist(Path::new("/A/app"), &item, &[], &[]),
            )
            .unwrap();
        }
        let first = list(&w.places, None);
        assert_eq!(first["schedules"].as_array().unwrap().len(), PAGE_SIZE);
        let second = list(&w.places, first["next_after"].as_str());
        assert_eq!(second["schedules"].as_array().unwrap().len(), 3);
        assert!(second["next_after"].is_null());
        let before = first["next_after"].as_str().unwrap();
        assert!(
            second["schedules"]
                .as_array()
                .unwrap()
                .iter()
                .all(|r| r["name"].as_str().unwrap() > before)
        );
    }

    #[test]
    fn recurring_schedules_wait_a_full_interval_before_the_first_fire() {
        let now = clock(2026, 9, 28, 23, 52) + 30;
        for (value, seconds) in [("30m", 1800), ("2h", 7200), ("1d", 86400)] {
            let when = every(value, now).unwrap();
            assert_eq!(when.not_before, Some(now + 30 + seconds));
            let s = Schedule {
                not_before: when.not_before,
                ..schedule()
            };
            assert_eq!(
                Schedule::parse(&s.args()[1..]).unwrap().not_before,
                when.not_before
            );
        }
    }

    #[test]
    fn creating_and_replacing_leave_a_schedule_whole_or_as_it_was() {
        let w = World::new("create");
        let s = schedule();
        // A load launchd refuses leaves nothing.
        w.fake.refuse_load.set(true);
        assert!(w.install(&s).unwrap_err().contains("refused"));
        assert_eq!(w.state(&s.name), (false, false, false));
        w.fake.refuse_load.set(false);
        w.install(&s).unwrap();
        assert_eq!(w.state(&s.name), (true, true, false));
        // A replacement starts with no last result.
        record_last(&w.places, &s, &json!({"outcome": "skipped"})).unwrap();
        let changed = Schedule {
            generation: "test-2".into(),
            message: "something else".into(),
            ..schedule()
        };
        w.install(&changed).unwrap();
        assert_eq!(w.state(&s.name), (true, true, true));
        assert!(list(&w.places, None)["schedules"][0]["last"].is_null());
        assert_eq!(schedules(&w.places)[0].0, changed);
        // One launchd will not load puts the old one back, loaded, with its result.
        record_last(&w.places, &changed, &json!({"outcome": "skipped"})).unwrap();
        let before = w.plist(&s.name);
        w.fake.refuse_load.set(true);
        assert!(w.install(&s).is_err());
        w.fake.refuse_load.set(false);
        // The fake refused the old one's load too; launchd loads it at the next login.
        assert_eq!(w.plist(&s.name), before);
        assert_eq!(
            list(&w.places, None)["schedules"].clone()[0]["last"]["outcome"],
            "skipped"
        );
        w.fake
            .loaded
            .borrow_mut()
            .insert(format!("{LABEL}{}", s.name));
        // An old job launchd will not unload is not replaced at all.
        w.fake.refuse_unload.set(true);
        assert!(w.install(&s).unwrap_err().contains("busy"));
        w.fake.refuse_unload.set(false);
        assert_eq!(w.plist(&s.name), before);
        assert_eq!(w.state(&s.name), (true, true, true));
        // An old plist that cannot be read could not be put back: nothing is touched.
        let odd = Schedule {
            name: "p.odd".into(),
            ..schedule()
        };
        std::fs::create_dir_all(w.places.plist(&odd.name).join("x")).unwrap();
        let unloads = w.fake.unloads.get();
        assert!(w.install(&odd).is_err());
        assert_eq!(w.fake.unloads.get(), unloads);
        // It is listed, saying why, and `rm` cannot take a folder away.
        let row = list(&w.places, None)["schedules"]
            .clone()
            .as_array()
            .unwrap()
            .iter()
            .find(|r| r["name"] == "p.odd")
            .cloned()
            .unwrap();
        assert!(row["problem"].as_str().unwrap().starts_with("unreadable"));
        std::fs::remove_dir_all(w.places.plist(&odd.name)).unwrap();
        // Nor may a name differing only in case take an ended one's result.
        record_last(&w.places, &once(), &json!({"outcome": "failed"})).unwrap();
        let cased = Schedule {
            name: "P.Once".into(),
            ..schedule()
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
    fn a_fire_ends_only_the_schedule_it_ran_for() {
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
        let listed = list(&w.places, None)["schedules"].clone();
        assert_eq!(listed.as_array().unwrap().len(), 1);
        assert_eq!(
            (&listed[0]["ended"], &listed[0]["last"]["outcome"]),
            (&json!(true), &json!("failed"))
        );
        assert_eq!(listed[0]["message"], one.message);
        w.remove(&one.name).unwrap();
        assert_eq!(list(&w.places, None)["schedules"].clone(), json!([]));
        // Replaced while its message went out: the new one is left alone.
        w.install(&one).unwrap();
        let replacement = Schedule {
            message: "a new reminder".into(),
            ..once()
        };
        w.install(&replacement).unwrap();
        w.settle(&one, json!({"outcome": "sent", "turn": 3}));
        assert_eq!(w.state(&one.name), (true, true, false));
        assert_eq!(schedules(&w.places)[0].0, replacement);
        // An unload launchd refuses puts the plist back, listed for `rm`.
        w.fake.refuse_unload.set(true);
        w.settle(&replacement, json!({"outcome": "sent", "turn": 4}));
        w.fake.refuse_unload.set(false);
        assert_eq!(w.state(&one.name), (true, true, true));
        assert_eq!(schedules(&w.places)[0].0, replacement);
        assert_eq!(
            list(&w.places, None)["schedules"].clone()[0]["last"]["turn"],
            4
        );
        // A job left loaded without its plist (an end cut short) is unloaded
        // by its next fire, which records nothing over the result there was.
        std::fs::remove_file(w.places.plist(&one.name)).unwrap();
        w.settle(&replacement, json!({"outcome": "missed"}));
        assert_eq!(w.state(&one.name), (false, false, true));
        assert_eq!(
            list(&w.places, None)["schedules"].clone()[0]["last"]["turn"],
            4
        );
        w.remove(&one.name).unwrap();
        // A plist that will not go keeps its job loaded and its result.
        let stuck = w.places.plist("p.stuck");
        std::fs::create_dir_all(stuck.join("x")).unwrap();
        record_last(&w.places, &once(), &json!({"outcome": "sent"})).unwrap();
        w.fake.loaded.borrow_mut().insert(format!("{LABEL}p.stuck"));
        end(&w.places, "p.stuck", &stuck, "", false, &|x| w.fake.call(x));
        assert!(w.fake.loaded.borrow().contains(&format!("{LABEL}p.stuck")));
        std::fs::remove_dir_all(&stuck).unwrap();
        // A repeating one ends only when its agent is gone, keeping why.
        let s = schedule();
        w.install(&s).unwrap();
        w.settle(&s, json!({"outcome": "skipped"}));
        assert_eq!(w.state(&s.name), (true, true, true));
        w.settle(&s, json!({"outcome": "gone", "detail": "bot_not_found"}));
        assert_eq!(w.state(&s.name), (false, false, true));
    }

    #[test]
    fn removal_reaches_whatever_is_left_of_a_schedule() {
        let w = World::new("remove");
        let s = schedule();
        w.install(&s).unwrap();
        record_last(&w.places, &s, &json!({"outcome": "sent"})).unwrap();
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
                .starts_with("schedule_not_found")
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
        let late = Schedule {
            at: Some(now() - 3 * 24 * 3600),
            ..once()
        };
        w.install(&late).unwrap();
        assert_eq!(
            list(&w.places, None)["schedules"].clone()[0]["missed"],
            true
        );
        // Its fire the next year does not send it; it ends, saying so.
        let stale = Schedule {
            at: Some(now() - STALE - 3600),
            ..once()
        };
        w.install(&stale).unwrap();
        w.settle(
            &stale,
            json!({"outcome": "missed", "detail": "its time passed long ago"}),
        );
        assert_eq!(w.state(&stale.name), (false, false, true));
        assert_eq!(
            list(&w.places, None)["schedules"].clone()[0]["last"]["outcome"],
            "missed"
        );
    }

    #[test]
    fn add_takes_one_time_and_a_message() {
        let now = clock(2026, 9, 28, 23, 52);
        let words = |s: &str| s.split(' ').map(str::to_owned).collect::<Vec<_>>();
        let asked = parse_add(&words("--every 30m --bot p.x -- check the PR"), now).unwrap();
        assert_eq!(
            (asked.bot.as_deref(), asked.message.as_str()),
            (Some("p.x"), "check the PR")
        );
        assert!(
            parse_add(&words("--every 30m --in 5m -- x"), now)
                .unwrap_err()
                .contains("one of")
        );
        assert!(parse_add(&words("--every 30m"), now).is_err());
        assert!(parse_add(&words("-- x"), now).is_err());
        assert!(parse_add(&words("--every 30m --"), now).is_err());
        assert!(
            parse_add(
                &["--in".into(), "5m".into(), "--".into(), "a\u{7}b".into()],
                now
            )
            .is_err()
        );
    }
}
