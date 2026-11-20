//! Sonny's scheduler: the one job Sonny gives the system, per HOME, which
//! fires every trigger ([`crate::trigger`]).
//!
//! A Mac lists each LaunchAgent as a background item, and tells the person
//! with a notification each time one is added or changed. A job of the
//! system's own for each trigger, written again whenever a trigger changed,
//! did that every time a lead set its next look. So the system keeps one
//! job, written once and never changed while Sonny stays where it is: a
//! LaunchAgent run at login and kept alive on a Mac, a user service on
//! Linux. It runs `sonny scheduler`, which is cron for triggers: it reads
//! their definitions from `~/.agent/automations/jobs`, and runs each one's
//! fire as launchd ran it, when one of its calendar times comes (once for
//! any number missed while the machine slept or the scheduler was not
//! running), when the path it watches changes, and while its queue of
//! asks holds one. One fire of a trigger at a time, also across a restart
//! of the scheduler: a fire holds a lock on its own file in
//! `~/.agent/automations/.scheduler` while it runs.
//!
//! A write to its doorbell (`~/.agent/automations/.ring`, a FIFO) has it look
//! again at once: Sonny rings it when a definition or a queue changes, and
//! a fire's end wakes it for that trigger alone. Else it looks at least
//! every minute, and at a watched path every two seconds. It sleeps until
//! the next calendar time, and takes no process between fires. A wake
//! looks only at the triggers it may start: calendar times are kept
//! soonest first, a trigger waiting for room has its place in line, and
//! one waiting a moment is kept by when its wait is over. When Sonny is upgraded it runs the
//! new Sonny in place of itself; when its HOME is gone, as a test's is, it
//! lets its own job go.
use crate::trigger::{self, Entry, Job, Places, Scheduler};
use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::path::{Path, PathBuf};
use std::time::Duration;

/// `sonny scheduler`, what the job runs.
pub const VERB: &str = "scheduler";
/// The job's label, and its systemd unit's name, before its HOME's tag:
/// unique among the person's jobs, as launchd and systemd need, and no
/// one's name, as Homebrew's `homebrew.mxcl.NAME` is no one's domain.
const LABEL: &str = "sonny.scheduler.";
/// What the label was before 0.9.0, which [`relabel`] moves off once.
const OLD_LABEL: &str = "me.lydakis.agent.scheduler.";
/// The record of [`relabel`] in `~/.agent/sonny/migrated`.
pub const RELABELED: &str = "scheduler-label";
/// The longest it sleeps: a clock set forward, a sleep that paused its
/// timer, or a ring it missed is seen within this.
const LOOK_EVERY: Duration = Duration::from_secs(60);
/// How often it looks at the paths triggers watch.
const WATCH_EVERY: Duration = Duration::from_secs(2);
/// The least time between two fires of one trigger the scheduler starts,
/// so one that keeps ending with its queue full does not spin.
const AGAIN_AFTER: Duration = Duration::from_secs(1);
/// What tells a fire which inherited descriptor is its trigger's lock, so
/// it keeps it from what it starts.
pub const LOCK_FD: &str = "AGENT_TRIGGER_LOCK_FD";

/// The job's label for HOME `home`.
fn label(home: &Path) -> String {
    format!("{LABEL}{}", trigger::home_tag(home.as_os_str()))
}

/// The job's file, where the system reads it.
fn job_file(scheduler: Scheduler, home: &Path) -> PathBuf {
    scheduler
        .jobs(home)
        .join(format!("{}{}", label(home), scheduler.suffix()))
}

/// The job: `SONNY scheduler`, started at login and again when it fails,
/// with `environment` besides the system's own. A fire it runs outlives a
/// restart of it.
pub fn definition(
    scheduler: Scheduler,
    label: &str,
    sonny: &Path,
    environment: &[(&str, String)],
) -> String {
    let sonny = sonny.to_string_lossy();
    match scheduler {
        Scheduler::Launchd => {
            let environment = match environment.is_empty() {
                true => String::new(),
                false => {
                    let mut out = String::from("  <key>EnvironmentVariables</key>\n  <dict>\n");
                    for (key, value) in environment {
                        out += &format!(
                            "    <key>{key}</key><string>{}</string>\n",
                            trigger::escape(value)
                        );
                    }
                    out + "  </dict>\n"
                }
            };
            format!(
                "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n<!DOCTYPE plist PUBLIC \"-//Apple//DTD PLIST 1.0//EN\" \"http://www.apple.com/DTDs/PropertyList-1.0.dtd\">\n<plist version=\"1.0\">\n<dict>\n  <key>Label</key>\n  <string>{}</string>\n  <key>ProgramArguments</key>\n  <array>\n    {}\n    <string>{VERB}</string>\n  </array>\n  <key>RunAtLoad</key>\n  <true/>\n  <key>KeepAlive</key>\n  <dict>\n    <key>SuccessfulExit</key>\n    <false/>\n  </dict>\n  <key>AbandonProcessGroup</key>\n  <true/>\n{environment}</dict>\n</plist>\n",
                trigger::escape(label),
                program(scheduler, Path::new(&*sonny)),
            )
        }
        Scheduler::Systemd => {
            let environment: String = environment
                .iter()
                .map(|(key, value)| {
                    format!(
                        "Environment={}\n",
                        trigger::quoted(&format!("{key}={value}"), false)
                    )
                })
                .collect();
            format!(
                "# Sonny's scheduler, which runs its automations.\n[Unit]\nDescription=Sonny's scheduler\n\n[Service]\n{} {VERB}\n{environment}Restart=on-failure\nRestartSec=10\n# A fire it started runs to its end.\nKillMode=process\n\n[Install]\nWantedBy=default.target\n",
                program(scheduler, Path::new(&*sonny)),
            )
        }
    }
}

/// How the job names Sonny: the one part of it that makes it out of date.
fn program(scheduler: Scheduler, sonny: &Path) -> String {
    let sonny = sonny.to_string_lossy();
    match scheduler {
        Scheduler::Launchd => format!("<string>{}</string>", trigger::escape(&sonny)),
        Scheduler::Systemd => format!("ExecStart={}", trigger::quoted(&sonny, true)),
    }
}

/// The scheduler of this HOME, set up when it is not and woken, for a
/// definition or a queue that changed.
pub fn ring() -> Result<(), String> {
    let places = Places::home()?;
    if trigger::poke(&places) {
        return Ok(());
    }
    ensure()?;
    trigger::poke(&places);
    Ok(())
}

/// The job there and running: written only when there is none or it runs
/// another Sonny, so the system has nothing new to tell the person about;
/// else started when the system has let it go.
pub fn ensure() -> Result<(), String> {
    // Sonny's start reaches here from several threads at once (triggers,
    // the backup, start-at-login): one writes the job, the others find it.
    static ENSURING: std::sync::Mutex<()> = std::sync::Mutex::new(());
    let _one = ENSURING.lock().unwrap_or_else(|e| e.into_inner());
    let home = PathBuf::from(std::env::var_os("HOME").ok_or("no HOME for automations")?);
    let scheduler = Scheduler::here();
    if scheduler == Scheduler::Systemd {
        UserManager::find()?;
    }
    let sonny = crate::skills::sonny_path().ok_or("no path to this Sonny")?;
    // Run by its own name, made again when Sonny changes, which a job
    // running the old Sonny then starts again for.
    let was = crate::named::current(&sonny, crate::named::SCHEDULER);
    let job = crate::named::named(&sonny, crate::named::SCHEDULER);
    let renamed = job != sonny && !was;
    let (file, label) = (job_file(scheduler, &home), label(&home));
    let there = std::fs::read_to_string(&file).ok();
    if !renamed
        && there
            .as_deref()
            .is_some_and(|text| text.contains(&program(scheduler, &job)))
    {
        return start(scheduler, &file, &label);
    }
    let mut environment = trigger::environment();
    if job != sonny {
        environment.push((crate::named::PROGRAM, sonny.to_string_lossy().into_owned()));
    }
    let text = definition(scheduler, &label, &job, &environment);
    if let Some(dir) = file.parent() {
        std::fs::create_dir_all(dir).map_err(|e| format!("{}: {e}", dir.display()))?;
    }
    if there.is_some() {
        let_go(scheduler, file.parent().unwrap_or(Path::new("")), &label);
    }
    trigger::replace_mode(&file, text.as_bytes(), 0o644)?;
    start(scheduler, &file, &label)
}

/// Whether the job is there and runs another Sonny than this one: by
/// another path, or by its name while the name is not this Sonny.
fn stale() -> bool {
    let (Some(home), Some(sonny)) = (std::env::var_os("HOME"), crate::skills::sonny_path()) else {
        return false;
    };
    let scheduler = Scheduler::here();
    let job = crate::named::path(crate::named::SCHEDULER);
    std::fs::read_to_string(job_file(scheduler, Path::new(&home))).is_ok_and(|text| {
        !text.contains(&program(scheduler, &job))
            || !crate::named::current(&sonny, crate::named::SCHEDULER)
    })
}

/// Start a job the system has, else load it from its file.
fn start(scheduler: Scheduler, file: &Path, label: &str) -> Result<(), String> {
    match scheduler {
        Scheduler::Launchd => {
            let domain = launchd_domain();
            let kickstart = || launchctl(&["kickstart", &format!("{domain}/{label}")]);
            // Running already, or loaded and stopped: nothing to load.
            if kickstart().is_ok() {
                return Ok(());
            }
            // Another Sonny may have loaded it just now.
            launchctl(&["bootstrap", &domain, &file.to_string_lossy()])
                .or_else(|error| kickstart().map_err(|_| error))
        }
        Scheduler::Systemd => {
            let manager = UserManager::find()?;
            manager.run(&["daemon-reload"])?;
            manager.run(&["enable", "--now", &format!("{label}.service")])
        }
    }
}

/// What a listing of the automations adds when the scheduler does not
/// run: its key, and the system's own words for the job's state.
pub const NOT_RUNNING: &str = "scheduler_not_running";

/// Whether this HOME's scheduler runs, as one read of the system says:
/// none while it runs, else the service manager's own words for its
/// state, as `failed (Result: resources)`. A fact only: nothing is
/// started or changed.
pub fn not_running() -> Option<String> {
    let home = PathBuf::from(std::env::var_os("HOME")?);
    let label = label(&home);
    match Scheduler::here() {
        Scheduler::Systemd => {
            let manager = match UserManager::find() {
                Ok(manager) => manager,
                Err(error) => return Some(error),
            };
            let out = std::process::Command::new(systemctl())
                .arg("--user")
                .args([
                    "show",
                    &format!("{label}.service"),
                    "--property=LoadState,ActiveState,SubState,Result,NRestarts",
                ])
                .env("XDG_RUNTIME_DIR", &manager.runtime)
                .stdin(std::process::Stdio::null())
                .output();
            match out {
                Ok(out) if out.status.success() => {
                    systemd_state(&String::from_utf8_lossy(&out.stdout))
                }
                Ok(out) => Some(last_line(&out.stderr, &out.stdout)),
                Err(error) => Some(format!("systemctl: {error}")),
            }
        }
        Scheduler::Launchd => {
            let out = std::process::Command::new("/bin/launchctl")
                .args(["print", &format!("{}/{label}", launchd_domain())])
                .stdin(std::process::Stdio::null())
                .output();
            match out {
                Ok(out) if out.status.success() => {
                    launchd_state(&String::from_utf8_lossy(&out.stdout))
                }
                Ok(out) => Some(last_line(&out.stderr, &out.stdout)),
                Err(error) => Some(format!("launchctl: {error}")),
            }
        }
    }
}

/// What `systemctl show` says of the job, in `systemctl status`'s words
/// for its state (`failed (Result: resources)`), with its restart
/// counter as systemd logs it: none while it is active.
fn systemd_state(show: &str) -> Option<String> {
    let get = |key: &str| {
        (show.lines())
            .find_map(|line| line.strip_prefix(key)?.strip_prefix('='))
            .unwrap_or_default()
            .trim()
    };
    let (load, active, sub, result) = (
        get("LoadState"),
        get("ActiveState"),
        get("SubState"),
        get("Result"),
    );
    if matches!(active, "active" | "reloading") {
        return None;
    }
    let mut said = Vec::new();
    if !load.is_empty() && load != "loaded" {
        said.push(load.to_owned());
    }
    let mut state = active.to_owned();
    if !sub.is_empty() && sub != active {
        state += &format!(" ({sub})");
    }
    if !result.is_empty() && result != "success" {
        state += &format!(" (Result: {result})");
    }
    said.push(state);
    if let Some(n) = get("NRestarts").parse::<u64>().ok().filter(|n| *n > 0) {
        said.push(format!("restart counter is at {n}"));
    }
    Some(said.join(" · "))
}

/// What `launchctl print` says of the job, in its own words (`state =
/// not running · last exit code = 78: EX_CONFIG`): none while it runs.
fn launchd_state(print: &str) -> Option<String> {
    let get = |key: &str| {
        (print.lines()).find_map(|line| {
            let value = line
                .trim()
                .strip_prefix(key)?
                .trim_start()
                .strip_prefix('=')?;
            Some(value.trim().to_owned())
        })
    };
    let state = get("state").unwrap_or_default();
    if state == "running" {
        return None;
    }
    let mut said = vec![format!("state = {state}")];
    if let Some(exit) = get("last exit code") {
        said.push(format!("last exit code = {exit}"));
    }
    Some(said.join(" · "))
}

/// The last line a command that failed printed, its error first.
fn last_line(stderr: &[u8], stdout: &[u8]) -> String {
    [stderr, stdout]
        .iter()
        .find_map(|out| {
            let out = String::from_utf8_lossy(out);
            let line = out.lines().rev().find(|l| !l.trim().is_empty())?;
            Some(line.trim().to_owned())
        })
        .unwrap_or_else(|| "no state".into())
}

/// Where a system's `systemctl` is installed. Never looked up on PATH:
/// these calls run as the person outside any sandbox, some for an agent's
/// own `automations` call, and a PATH entry in a folder an agent may write,
/// such as an activated virtualenv, would let it plant the `systemctl` they
/// run, as the daemon's `bwrap` is never looked up there either.
const SYSTEMCTL: [&str; 4] = [
    "/usr/bin/systemctl",
    "/bin/systemctl",
    "/usr/local/bin/systemctl",
    "/run/current-system/sw/bin/systemctl",
];

/// This system's `systemctl`, the first of [`SYSTEMCTL`] there; the
/// first one when none is, which fails to run with its own path.
fn systemctl() -> &'static str {
    (SYSTEMCTL.iter())
        .find(|path| Path::new(path).is_file())
        .unwrap_or(&SYSTEMCTL[0])
}

/// The logged-in user's systemd manager, found by its runtime folder as
/// `systemctl --user` finds it.
pub(crate) struct UserManager {
    runtime: PathBuf,
}

impl UserManager {
    pub(crate) fn find() -> Result<Self, String> {
        let runtime = std::env::var_os("XDG_RUNTIME_DIR")
            .map(PathBuf::from)
            .unwrap_or_else(|| {
                // SAFETY: getuid takes no arguments and cannot fail.
                PathBuf::from(format!("/run/user/{}", unsafe { libc::getuid() }))
            });
        match runtime.join("systemd/private").exists() {
            true => Ok(Self { runtime }),
            false => Err(format!(
                "automations_unsupported: automations on Linux use systemd's user manager, which is not running here ({} has no systemd/private)",
                runtime.display()
            )),
        }
    }
    pub(crate) fn run(&self, args: &[&str]) -> Result<(), String> {
        self.run_allowing(args, &[])
    }
    /// `systemctl --user ARGS`, also fine when it exits with one of `fine`.
    pub(crate) fn run_allowing(&self, args: &[&str], fine: &[i32]) -> Result<(), String> {
        let out = std::process::Command::new(systemctl())
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

/// The logged-in user's launchd domain.
fn launchd_domain() -> String {
    // SAFETY: getuid takes no arguments and cannot fail.
    format!("gui/{}", unsafe { libc::getuid() })
}

/// `launchctl ARGS`.
fn launchctl(args: &[&str]) -> Result<(), String> {
    if !cfg!(target_os = "macos") {
        return Err("automations_unsupported: launchd is macOS's".into());
    }
    let out = std::process::Command::new("/bin/launchctl")
        .args(args)
        .stdin(std::process::Stdio::null())
        .output()
        .map_err(|e| format!("launchctl: {e}"))?;
    match out.status.success() {
        true => Ok(()),
        false => Err(format!(
            "launchctl {}: {}",
            args[0],
            String::from_utf8_lossy(&out.stderr).trim()
        )),
    }
}

/// The system lets go of the job `label` from `dir`, whatever is left of
/// it: launchd unloads it; systemd stops it, and the timer and path units
/// an old trigger had beside its service go, with their links and the
/// timer's last time. Its own file stays its owner's to remove.
pub(crate) fn let_go(scheduler: Scheduler, dir: &Path, label: &str) {
    match scheduler {
        Scheduler::Launchd => {
            let _ = launchctl(&["bootout", &format!("{}/{label}", launchd_domain())]);
        }
        Scheduler::Systemd => {
            let Ok(manager) = UserManager::find() else {
                return;
            };
            let units = ["timer", "path", "service"].map(|kind| format!("{label}.{kind}"));
            // One unit at a time: a systemctl that refuses to disable a unit
            // with no file (older systemd) stops none of those it was given,
            // and the scheduler has no timer or path unit, so an upgrade went
            // on running the old Sonny.
            for unit in &units {
                let _ = manager.run_allowing(&["disable", "--now", unit], &[1, 5]);
            }
            for file in [
                dir.join(&units[0]),
                dir.join(&units[1]),
                dir.join("timers.target.wants").join(&units[0]),
                dir.join("paths.target.wants").join(&units[1]),
                dir.join("default.target.wants").join(&units[2]),
            ] {
                let _ = std::fs::remove_file(file);
            }
            if let Some(home) = std::env::var_os("HOME") {
                let stamps = trigger::xdg("XDG_DATA_HOME", Path::new(&home), ".local/share");
                let _ =
                    std::fs::remove_file(stamps.join(format!("systemd/timers/stamp-{}", units[0])));
            }
            let _ = manager.run(&["daemon-reload"]);
        }
    }
}

/// The job under its label from before 0.9.0 in `home`, when there is
/// one: let go of, its file removed, and the job made again by `make`,
/// under [`LABEL`]. Whether it is done, so a job that could not be made
/// is made at the next start.
fn relabel(home: &Path, scheduler: Scheduler, make: impl FnOnce() -> Result<(), String>) -> bool {
    let old = format!("{OLD_LABEL}{}", trigger::home_tag(home.as_os_str()));
    let dir = scheduler.jobs(home);
    let file = dir.join(format!("{old}{}", scheduler.suffix()));
    if !file.exists() {
        return true;
    }
    let_go(scheduler, &dir, &old);
    let _ = std::fs::remove_file(&file);
    match make() {
        Ok(()) => true,
        Err(error) => {
            eprintln!("{}", trigger::error_json(&error));
            false
        }
    }
}

/// At Sonny's start: the job under its own label, runs this Sonny, when
/// it ran one from elsewhere (an install that keeps each version in its
/// own folder), even while the old one still answers its doorbell; and
/// the scheduler running, when any trigger is there.
pub fn at_start() {
    let Ok(places) = Places::home() else {
        return;
    };
    if let Some(home) = std::env::var_os("HOME").map(PathBuf::from) {
        let record = home.join(".agent/sonny/migrated");
        crate::upgrade::once(&record, RELABELED, || {
            relabel(&home, Scheduler::here(), ensure)
        });
    }
    if stale()
        && let Err(error) = ensure()
    {
        eprintln!("{}", trigger::error_json(&error));
    }
    let any = std::fs::read_dir(&places.jobs)
        .into_iter()
        .flatten()
        .flatten()
        .any(|e| e.file_name().to_string_lossy().ends_with(".json"));
    if any && let Err(error) = ring() {
        eprintln!("{}", trigger::error_json(&error));
    }
}

/// What a trigger's watched path looked like: changed when any of it is.
type Look = Option<(u64, u64, i64, i64, i64, i64, u64)>;

fn look(path: &Path) -> Look {
    use std::os::unix::fs::MetadataExt;
    // The path itself, a link included: launchd watched what it names, and
    // a link pointed elsewhere changes too.
    let meta = std::fs::metadata(path)
        .or_else(|_| std::fs::symlink_metadata(path))
        .ok()?;
    Some((
        meta.dev(),
        meta.ino(),
        meta.mtime(),
        meta.mtime_nsec(),
        meta.ctime(),
        meta.ctime_nsec(),
        meta.size(),
    ))
}

/// One trigger, as the scheduler follows it.
struct Followed {
    /// Its definition's file, as last read: read again when it changes.
    file: (i64, i64, u64),
    job: Job,
    generation: String,
    /// The time up to which its calendar was looked at: a time after it
    /// that has come is due.
    looked: i64,
    /// Its next calendar time after `looked`.
    next: Option<i64>,
    /// What its watched path looked like last.
    seen: Look,
    /// A change or an ask that came while a fire of it ran: it fires again
    /// once that one is done.
    again: bool,
    /// When the scheduler last started a fire of it.
    started: Option<std::time::Instant>,
    /// Starts that failed in a row, and when to try again: what was due
    /// stays due.
    failures: u32,
    retry: Option<std::time::Instant>,
    /// A calendar time that came, whose fire has not started yet.
    unkept: Option<i64>,
    /// Its place in line while it waits for room in the fleet.
    queued: Option<u64>,
}

/// The fires the scheduler started and runs still, by process and by
/// trigger, and how many may run at once.
struct Fleet {
    running: HashMap<libc::pid_t, String>,
    busy: std::collections::HashSet<String>,
    cap: usize,
    /// The next place in line for a trigger that waits for room.
    line: u64,
}

impl Fleet {
    fn new() -> Self {
        // As make's and cargo's jobs scale with the machine: a burst of
        // due triggers waits its turn rather than starting all at once.
        let cores = std::thread::available_parallelism().map_or(1, |n| n.get());
        Self {
            running: HashMap::new(),
            busy: std::collections::HashSet::new(),
            cap: (cores * 4).max(8),
            line: 0,
        }
    }
    fn full(&self) -> bool {
        self.running.len() >= self.cap
    }
    /// A fire that ended: its trigger, when the scheduler started it.
    fn ended(&mut self, pid: libc::pid_t) -> Option<String> {
        let name = self.running.remove(&pid)?;
        self.busy.remove(&name);
        Some(name)
    }
}

/// Whether one of the trigger's calendar times came by `now`. Kept at
/// once when its fire runs, which skips it, as launchd and systemd skip
/// one; else once its fire starts ([`started`]), so a scheduler that
/// restarts before then (a full fleet, a start that failed) finds it still
/// due.
fn came(name: &str, f: &mut Followed, busy: bool, now: i64, kept: &mut Kept) -> bool {
    if !f.next.is_some_and(|next| next <= now) {
        return false;
    }
    f.looked = now;
    f.next = next_after(&f.job.calendar, now);
    match busy {
        true => {
            kept.insert(name.to_owned(), (f.generation.clone(), now));
        }
        false => f.unkept = Some(now),
    }
    true
}

/// After [`launch`] at `at`: the calendar time its fire was for kept, when
/// the fire started. Whether `kept` changed.
fn started(name: &str, f: &mut Followed, at: std::time::Instant, kept: &mut Kept) -> bool {
    if f.started != Some(at) {
        return false;
    }
    let Some(came) = f.unkept.take() else {
        return false;
    };
    kept.insert(name.to_owned(), (f.generation.clone(), came));
    true
}

/// How a trigger's fire is started: its process, or none while another
/// fire of it holds its lock.
type Spawn<'a> = dyn FnMut(&str, &Job) -> Result<Option<libc::pid_t>, String> + 'a;

/// What became of a [`launch`].
#[derive(Debug, PartialEq)]
enum Launched {
    Started,
    /// It waits: for its fire to end, a moment after its last start, a
    /// failed start's wait, or room in the fleet.
    Waits,
    /// A fire of it the scheduler did not start holds its lock.
    Held,
    Failed,
}

/// Start a due trigger's fire now if it may start: else it stays due
/// (`again`), for when its fire ends, its last start is a moment old, a
/// failed start's wait is over, or the fleet has room.
fn launch(
    name: &str,
    f: &mut Followed,
    fleet: &mut Fleet,
    now: std::time::Instant,
    spawn: &mut Spawn,
) -> Launched {
    let soon = f
        .started
        .is_some_and(|at| now.duration_since(at) < AGAIN_AFTER);
    let waiting = f.retry.is_some_and(|at| at > now);
    if fleet.busy.contains(name) || soon || waiting || fleet.full() {
        if fleet.full() && f.queued.is_none() {
            f.queued = Some(fleet.line);
            fleet.line += 1;
        }
        f.again = true;
        return Launched::Waits;
    }
    match spawn(name, &f.job) {
        Ok(Some(pid)) => {
            fleet.running.insert(pid, name.to_owned());
            fleet.busy.insert(name.to_owned());
            f.started = Some(now);
            (f.failures, f.retry, f.queued) = (0, None, None);
            Launched::Started
        }
        // A fire of it the scheduler did not start runs still, as one from
        // before a restart: its end wakes no one, so it is looked at
        // again after a wait that doubles, up to a minute.
        Ok(None) => {
            back_off(f, now, LOOK_EVERY);
            Launched::Held
        }
        // Tried again after a wait that doubles, up to five minutes.
        Err(error) => {
            eprintln!("{}", trigger::error_json(&error));
            back_off(f, now, LOOK_EVERY * 5);
            Launched::Failed
        }
    }
}

/// The triggers the scheduler follows, and where each one due waits, so
/// a wake looks at those it may start and no others: as a kernel keeps
/// its timers by deadline and a queue of what is ready. An entry in
/// `timed`, `waits` or `line` that no longer says what its trigger holds
/// is passed over and dropped when reached.
#[derive(Default)]
struct Schedule {
    followed: HashMap<String, Followed>,
    /// Each trigger's next calendar time, soonest first.
    timed: BTreeSet<(i64, String)>,
    /// Those due again until a moment after their last start, or a failed
    /// start's wait, is over: by when.
    waits: BTreeSet<(std::time::Instant, String)>,
    /// Those waiting for room in the fleet, by their place in line.
    line: BTreeMap<u64, String>,
    /// Those due again with no place in line, as their fire runs or they
    /// wait: by name, the order a full fleet gives them places in.
    held: BTreeSet<String>,
    /// Those that watch a path.
    watched: BTreeSet<String>,
}

/// When a trigger due again may start, its last start a moment old and a
/// failed start's wait over.
fn until(f: &Followed) -> Option<std::time::Instant> {
    [f.started.map(|at| at + AGAIN_AFTER), f.retry]
        .into_iter()
        .flatten()
        .max()
}

/// The triggers a wake looks at: due (a calendar time came, a watched
/// path changed), with an ask in their queue, or due again where it may
/// start now.
#[derive(Default)]
struct Ready(HashMap<String, (bool, bool)>);

impl Ready {
    fn due(&mut self, name: &str) {
        self.0.entry(name.to_owned()).or_default().0 = true;
    }
    fn asked(&mut self, name: &str) {
        self.0.entry(name.to_owned()).or_default().1 = true;
    }
    /// Due again only: whether it is, its `again` says.
    fn again(&mut self, name: &str) {
        self.0.entry(name.to_owned()).or_default();
    }
}

impl Schedule {
    /// `name` filed where it waits now, after its trigger changed.
    fn file(&mut self, name: &str, now: std::time::Instant) {
        let Some(f) = self.followed.get(name) else {
            self.held.remove(name);
            self.watched.remove(name);
            return;
        };
        if let Some(next) = f.next {
            self.timed.insert((next, name.to_owned()));
        }
        if let Some(queued) = f.queued {
            self.line.insert(queued, name.to_owned());
        }
        match f.again && f.queued.is_none() {
            true => self.held.insert(name.to_owned()),
            false => self.held.remove(name),
        };
        if f.again
            && let Some(at) = until(f).filter(|at| *at > now)
        {
            self.waits.insert((at, name.to_owned()));
        }
        match f.job.watch.is_some() {
            true => self.watched.insert(name.to_owned()),
            false => self.watched.remove(name),
        };
    }

    /// One wake: the calendar times that came by `now`, the watched paths
    /// when `watch`, the queues of every trigger when `asks` and of those
    /// whose fire `ended`, and those due again whose wait is over or that
    /// now have room, then those [`Schedule::admit`] may start, started.
    /// `fresh` are those read again. Whether `kept` changed.
    #[allow(clippy::too_many_arguments)]
    fn pass(
        &mut self,
        fleet: &mut Fleet,
        now: i64,
        instant: std::time::Instant,
        watch: bool,
        asks: bool,
        ended: &std::collections::HashSet<String>,
        fresh: &[String],
        asked: &mut dyn FnMut(&str) -> bool,
        kept: &mut Kept,
        spawn: &mut Spawn,
    ) -> bool {
        let mut fired = false;
        let mut ready = Ready::default();
        while let Some((at, name)) = self.timed.first().cloned()
            && at <= now
        {
            self.timed.pop_first();
            visit();
            let Some(f) = self.followed.get_mut(&name).filter(|f| f.next == Some(at)) else {
                continue;
            };
            let busy = fleet.busy.contains(&name);
            if came(&name, f, busy, now, kept) {
                fired |= busy;
                if !busy {
                    ready.due(&name);
                }
            }
            if let Some(next) = f.next {
                self.timed.insert((next, name));
            }
        }
        while let Some((at, name)) = self.waits.first().cloned()
            && at <= instant
        {
            self.waits.pop_first();
            visit();
            if (self.followed.get(&name)).is_some_and(|f| f.again && until(f) == Some(at)) {
                ready.again(&name);
            }
        }
        for name in fresh.iter().chain(ended) {
            visit();
            if self.followed.get(name).is_some_and(|f| f.again) {
                ready.again(name);
            }
        }
        if watch {
            for name in &self.watched {
                visit();
                let Some(f) = self.followed.get_mut(name) else {
                    continue;
                };
                let Some(path) = f.job.watch.as_ref() else {
                    continue;
                };
                let seen = look(path);
                if seen != f.seen {
                    f.seen = seen;
                    ready.due(name);
                }
            }
        }
        // An ask while its fire runs is looked at when that fire ends:
        // the one it is taking is no ask for another.
        let looked: Box<dyn Iterator<Item = &String>> = match asks {
            true => Box::new(self.followed.keys()),
            false => Box::new(ended.iter()),
        };
        for name in looked {
            visit();
            if self.followed.contains_key(name) && !fleet.busy.contains(name) && asked(name) {
                ready.asked(name);
            }
        }
        fired | self.admit(ready, fleet, instant, kept, spawn)
    }

    /// Start the fires `ready`: those that waited for room longest first,
    /// so a place a fire leaves goes to one in line before one just
    /// served, then the rest by name. One due only for an ask its queue
    /// holds, whose lock a fire the scheduler did not start holds, is not
    /// due again: the ask waits in its queue, looked at each minute and
    /// ring, and that fire may be the one taking it. Those due again but
    /// held take places in line, by name, once the fleet is full, as they
    /// would were each looked at. Whether `kept` changed.
    fn admit(
        &mut self,
        mut ready: Ready,
        fleet: &mut Fleet,
        now: std::time::Instant,
        kept: &mut Kept,
        spawn: &mut Spawn,
    ) -> bool {
        let mut changed = false;
        // The line, from its head, while the fleet has room.
        let mut from = 0;
        while !fleet.full()
            && let Some((&place, name)) = self.line.range(from..).next()
        {
            let name = name.clone();
            from = place + 1;
            visit();
            if self.followed.get(&name).and_then(|f| f.queued) != Some(place) {
                self.line.remove(&place);
                continue;
            }
            let (due, ask) = ready.0.remove(&name).unwrap_or_default();
            changed |= self.start(&name, due, ask, fleet, now, kept, spawn);
        }
        // The rest: what is left in line, then the others by name.
        let mut rest: Vec<_> = (ready.0.into_iter())
            .filter_map(|(name, (due, ask))| {
                let queued = self.followed.get(&name)?.queued;
                Some((queued.is_none(), queued.unwrap_or(0), name, due, ask))
            })
            .collect();
        rest.sort_unstable();
        let mut after = std::ops::Bound::Unbounded;
        for (unqueued, _, name, due, ask) in rest {
            if unqueued {
                self.line_up(fleet, (after, std::ops::Bound::Excluded(name.clone())));
                after = std::ops::Bound::Excluded(name.clone());
            }
            changed |= self.start(&name, due, ask, fleet, now, kept, spawn);
        }
        self.line_up(fleet, (after, std::ops::Bound::Unbounded));
        changed
    }

    /// Start `name` if it is due, or `asked` alone, and may start; filed
    /// again where it waits. Whether `kept` changed.
    #[allow(clippy::too_many_arguments)]
    fn start(
        &mut self,
        name: &str,
        due: bool,
        asked: bool,
        fleet: &mut Fleet,
        now: std::time::Instant,
        kept: &mut Kept,
        spawn: &mut Spawn,
    ) -> bool {
        let Some(f) = self.followed.get_mut(name) else {
            return false;
        };
        let due = std::mem::take(&mut f.again) || due;
        if !due && !asked {
            return false;
        }
        let queued = f.queued;
        if launch(name, f, fleet, now, spawn) == Launched::Held && !due {
            (f.again, f.failures, f.retry) = (false, 0, None);
        }
        let changed = started(name, f, now, kept);
        if let Some(place) = queued.filter(|_| f.queued != queued) {
            self.line.remove(&place);
        }
        self.file(name, now);
        changed
    }

    /// With the fleet full, those held in `names` take places in line, in
    /// order, as each would were it looked at.
    fn line_up(
        &mut self,
        fleet: &mut Fleet,
        names: (std::ops::Bound<String>, std::ops::Bound<String>),
    ) {
        if !fleet.full() {
            return;
        }
        let held: Vec<String> = self.held.range(names).cloned().collect();
        for name in held {
            visit();
            self.held.remove(&name);
            let Some(f) = self.followed.get_mut(&name) else {
                continue;
            };
            f.queued = Some(fleet.line);
            self.line.insert(fleet.line, name);
            fleet.line += 1;
        }
    }

    /// How long until the next calendar time, a watched path's next look,
    /// or a wait over for one that may start then: one whose fire runs,
    /// or that waits for room, is woken by a fire's end.
    fn wake(&mut self, fleet: &Fleet, mut wake: Duration, watch_in: Duration) -> Duration {
        let now_exact = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default();
        while let Some((next, name)) = self.timed.first() {
            visit();
            if self
                .followed
                .get(name)
                .is_some_and(|f| f.next == Some(*next))
            {
                let at = Duration::from_secs((*next).max(0) as u64);
                wake = wake.min(at.saturating_sub(now_exact));
                break;
            }
            self.timed.pop_first();
        }
        if !self.watched.is_empty() {
            wake = wake.min(watch_in);
        }
        if fleet.full() {
            return wake;
        }
        let mut stale = Vec::new();
        for (at, name) in &self.waits {
            visit();
            match self.followed.get(name) {
                Some(f) if f.again && until(f) == Some(*at) => {
                    if !fleet.busy.contains(name) {
                        let now = std::time::Instant::now();
                        wake = wake.min(at.saturating_duration_since(now));
                        break;
                    }
                }
                _ => stale.push((*at, name.clone())),
            }
        }
        for entry in stale {
            self.waits.remove(&entry);
        }
        wake
    }
}

// Counts the triggers a wake looks at, in tests: a fire's end looks at
// those it may start, not at every trigger.
#[cfg(test)]
thread_local!(static VISITED: std::cell::Cell<usize> = const { std::cell::Cell::new(0) });

/// One trigger looked at.
fn visit() {
    #[cfg(test)]
    VISITED.with(|visited| visited.set(visited.get() + 1));
}

/// A trigger whose fire did not start, due again after a wait that
/// doubles with each try, up to `most`.
fn back_off(f: &mut Followed, now: std::time::Instant, most: Duration) {
    f.failures = f.failures.saturating_add(1);
    let wait = AGAIN_AFTER * 2u32.pow(f.failures.min(9) - 1);
    f.retry = Some(now + wait.min(most));
    f.again = true;
}

/// `sonny scheduler`: fire every trigger of this HOME, until its HOME is
/// gone or another scheduler runs for it.
pub fn run() -> i32 {
    let Ok(places) = Places::home() else {
        eprintln!("{}", trigger::error_json("no HOME for automations"));
        return 1;
    };
    // Before anything is made under HOME, which would make HOME again:
    // one gone while no scheduler ran is let go of as one gone while it
    // runs.
    if orphaned() {
        return let_job_go();
    }
    let bell = match Doorbell::open(&places) {
        Ok(Some(bell)) => bell,
        // Another scheduler of this HOME has it.
        Ok(None) => return 0,
        Err(error) => {
            eprintln!("{}", trigger::error_json(&error));
            return 1;
        }
    };
    let me = crate::skills::sonny_path();
    // `sonny.start_at_boot`: the daemon, as the scheduler starts at login.
    if crate::boot::on()
        && let Some(me) = &me
    {
        run_own(me, &["boot"]);
    }
    let home = crate::home::Places::home().ok();
    let mut syncing: Option<libc::pid_t> = None;
    let identity = |path: &Path| {
        use std::os::unix::fs::MetadataExt;
        std::fs::metadata(path).ok().map(|m| (m.dev(), m.ino()))
    };
    let born = me.as_deref().and_then(identity);
    let zoned = zone();
    let mut schedule = Schedule::default();
    let mut kept = Kept::read(&places);
    let mut fleet = Fleet::new();
    let mut folder = None;
    let mut last_look = std::time::Instant::now() - LOOK_EVERY;
    let mut last_watch = std::time::Instant::now() - WATCH_EVERY;
    let mut rang = true;
    let mut first = true;
    loop {
        if orphaned() {
            return let_job_go();
        }
        let every_minute = last_look.elapsed() >= LOOK_EVERY;
        // Sonny upgraded where it stays: the new one schedules from here.
        // So does a new one when the time zone changed, as the C library
        // keeps the zone it read at the start: its calendar times are
        // worked out again from the times kept, as at any start.
        if every_minute
            && let Some(me) = &me
            && identity(me).is_some_and(|now| Some(now) != born || zone() != zoned)
        {
            use std::os::unix::process::CommandExt;
            let error = std::process::Command::new(me).arg(VERB).exec();
            eprintln!(
                "{}",
                trigger::error_json(&format!("{}: {error}", me.display()))
            );
        }
        // Fires that ended: their queues are looked at again.
        let mut ended = std::collections::HashSet::new();
        loop {
            let mut status = 0;
            // SAFETY: waitpid only writes the status it is given.
            let pid = unsafe { libc::waitpid(-1, &mut status, libc::WNOHANG) };
            if pid <= 0 {
                break;
            }
            if let Some(name) = fleet.ended(pid) {
                ended.insert(name);
            }
            if syncing == Some(pid) {
                syncing = None;
            }
        }
        // The home's backup, every ten minutes while it is set up.
        if every_minute
            && syncing.is_none()
            && let (Some(home), Some(me)) = (&home, &me)
            && crate::home::due(home, crate::home::now_ms())
        {
            syncing = run_own(me, &["backup", "sync", "--job"]);
        }
        let now_folder = modified(&places.jobs);
        let mut fired = false;
        let mut read = Vec::new();
        if rang || every_minute || now_folder != folder {
            folder = now_folder;
            fired = read_jobs(&places, &mut schedule.followed, &mut kept, &mut read);
        }
        let now = trigger::now();
        let instant = std::time::Instant::now();
        for name in &read {
            schedule.file(name, instant);
        }
        // Watched paths every two seconds, however often it wakes.
        let watch = instant.duration_since(last_watch) >= WATCH_EVERY;
        if watch {
            last_watch = instant;
        }
        fired |= schedule.pass(
            &mut fleet,
            now,
            instant,
            watch,
            rang || every_minute,
            &ended,
            &read,
            &mut |name| asked(&places, name),
            &mut kept,
            &mut |name, job| spawn(&places, name, job),
        );
        if first {
            // Times kept for triggers gone while no scheduler ran.
            first = false;
            kept.retain(|name| schedule.followed.contains_key(name));
        }
        for name in read
            .iter()
            .filter(|name| !schedule.followed.contains_key(*name))
        {
            kept.remove(name);
        }
        if fired {
            kept.write(&places);
        }
        if every_minute {
            last_look = std::time::Instant::now();
        }
        // Asleep until the next time, a watched path's next look, a fire
        // run again, or a ring.
        let minute = Duration::from_secs((60 - now.rem_euclid(60)) as u64).min(LOOK_EVERY);
        let watch_in = WATCH_EVERY.saturating_sub(last_watch.elapsed());
        let wake = schedule.wake(&fleet, minute, watch_in);
        rang = bell.wait(wake.max(Duration::from_millis(50)));
    }
}

/// Read the definitions again: a new one is followed from the time kept
/// for it, else from when it was written; a changed one is read again,
/// keeping where it looked when it is the same trigger; a removed one is
/// let go. A trigger new since the scheduler last kept its times fires
/// once for a write to its watched path after its definition was written,
/// as a job the system watched from the moment it was added would have.
/// Whether `kept` has a trigger new to it, to be written.
fn read_jobs(
    places: &Places,
    followed: &mut HashMap<String, Followed>,
    kept: &mut Kept,
    read: &mut Vec<String>,
) -> bool {
    let mut new = false;
    use std::os::unix::fs::MetadataExt;
    let mut there = std::collections::HashSet::new();
    for entry in std::fs::read_dir(&places.jobs)
        .into_iter()
        .flatten()
        .flatten()
    {
        let file = entry.file_name().to_string_lossy().into_owned();
        let Some(name) = file.strip_suffix(".json").filter(|n| !n.starts_with('.')) else {
            continue;
        };
        let Ok(meta) = entry.metadata() else {
            continue;
        };
        there.insert(name.to_owned());
        let stamp = (meta.mtime(), meta.mtime_nsec(), meta.size());
        if followed.get(name).is_some_and(|f| f.file == stamp) {
            continue;
        }
        let read_job = std::fs::read_to_string(entry.path())
            .ok()
            .and_then(|text| Job::read(&text))
            .and_then(|job| Some((trigger::trigger_of(&job.program)?.0.generation, job)));
        let Some((generation, job)) = read_job else {
            if followed.remove(name).is_some() {
                read.push(name.to_owned());
            }
            continue;
        };
        let known = followed
            .get(name)
            .is_some_and(|f| f.generation == generation)
            || kept.get(name).is_some_and(|(kept, _)| *kept == generation);
        let looked = match followed.get(name) {
            Some(f) if f.generation == generation => f.looked,
            _ => match kept.get(name) {
                Some((kept, at)) if *kept == generation => *at,
                _ => meta.mtime(),
            },
        };
        if !known {
            kept.insert(name.to_owned(), (generation.clone(), looked));
            new = true;
        }
        let was = followed.remove(name);
        let unkept = was
            .as_ref()
            .filter(|f| f.generation == generation)
            .and_then(|f| f.unkept);
        let queued = was.as_ref().and_then(|f| f.queued);
        let (seen, again, started) = match was {
            Some(f) if f.job.watch == job.watch => (f.seen, f.again, f.started),
            _ => {
                let seen = job.watch.as_deref().and_then(look);
                // Changed (written, or its metadata) after the definition.
                let after = |(_, _, m, mn, c, cn, _): (u64, u64, i64, i64, i64, i64, u64)| {
                    (m, mn).max((c, cn)) > (meta.mtime(), meta.mtime_nsec())
                };
                (seen, !known && seen.is_some_and(after), None)
            }
        };
        followed.insert(
            name.to_owned(),
            Followed {
                file: stamp,
                next: next_after(&job.calendar, looked),
                job,
                generation,
                looked,
                seen,
                again,
                started,
                failures: 0,
                retry: None,
                unkept,
                queued,
            },
        );
        read.push(name.to_owned());
    }
    followed.retain(|name, _| {
        let stays = there.contains(name);
        if !stays {
            read.push(name.clone());
        }
        stays
    });
    new
}

/// The machine's time zone: where `/etc/localtime` points, and the file
/// there, which a Mac and systemd's `timedatectl` both change.
fn zone() -> Option<(PathBuf, u64, u64, i64, i64)> {
    use std::os::unix::fs::MetadataExt;
    let path = Path::new("/etc/localtime");
    let meta = std::fs::metadata(path).ok()?;
    Some((
        std::fs::read_link(path).unwrap_or_default(),
        meta.dev(),
        meta.ino(),
        meta.mtime(),
        meta.mtime_nsec(),
    ))
}

/// Whether a trigger's queue holds an ask, or one a fire took and was cut
/// short before it was done with.
fn asked(places: &Places, name: &str) -> bool {
    [places.asks(name), places.taking(name)].iter().any(|dir| {
        std::fs::read_dir(dir)
            .map(|mut entries| entries.next().is_some())
            .unwrap_or(false)
    })
}

/// Sonny itself with `args`, reaped with the fires.
fn run_own(me: &Path, args: &[&str]) -> Option<libc::pid_t> {
    std::process::Command::new(me)
        .args(args)
        .current_dir("/")
        .stdin(std::process::Stdio::null())
        .spawn()
        .map_err(|e| eprintln!("{}", trigger::error_json(&format!("{}: {e}", me.display()))))
        .ok()
        .map(|child| child.id() as libc::pid_t)
}

/// `wait` run on a thread of its own, for a wait on another process (a
/// lock it holds): a window that ends meanwhile goes without it, where a
/// blocking task would hold the runtime's end until the other let go.
/// What `wait` gives once nothing awaits it is dropped, a lock with it.
pub(crate) async fn apart<T: Send + 'static>(
    wait: impl FnOnce() -> Option<T> + Send + 'static,
) -> Option<T> {
    let (sent, got) = tokio::sync::oneshot::channel();
    std::thread::Builder::new()
        .name("sonny-wait".into())
        .spawn(move || {
            let _ = sent.send(wait());
        })
        .ok()?;
    got.await.ok().flatten()
}

/// A lock on `path`, a plain file made when it is not there: None while
/// another process holds it.
pub(crate) fn locked(path: &Path) -> Result<Option<std::fs::File>, String> {
    use std::os::fd::AsRawFd;
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir).map_err(|e| format!("{}: {e}", dir.display()))?;
    }
    let file = std::fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .write(true)
        .open(path)
        .map_err(|e| format!("{}: {e}", path.display()))?;
    // SAFETY: flock on a descriptor this function owns.
    if unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } == 0 {
        return Ok(Some(file));
    }
    match std::io::Error::last_os_error() {
        e if e.kind() == std::io::ErrorKind::WouldBlock => Ok(None),
        e => Err(format!("{}: {e}", path.display())),
    }
}

/// Start the trigger's fire, holding a lock for as long as the fire runs,
/// which it inherits. None when a fire of it holds it.
fn spawn(places: &Places, name: &str, job: &Job) -> Result<Option<libc::pid_t>, String> {
    use std::os::fd::AsRawFd;
    use std::os::unix::process::CommandExt;
    let Some(lock) = locked(&places.running(name))? else {
        return Ok(None);
    };
    let fd = lock.as_raw_fd();
    let (program, args) = job.program.split_first().ok_or("an empty program")?;
    let mut command = std::process::Command::new(program);
    command
        .args(args)
        .envs(job.environment.iter().map(|(k, v)| (k, v)))
        .env(LOCK_FD, fd.to_string())
        .current_dir("/")
        .stdin(std::process::Stdio::null());
    // SAFETY: fcntl on a descriptor open in the child, between fork and exec.
    unsafe {
        command.pre_exec(move || {
            let flags = libc::fcntl(fd, libc::F_GETFD);
            if flags < 0 || libc::fcntl(fd, libc::F_SETFD, flags & !libc::FD_CLOEXEC) < 0 {
                return Err(std::io::Error::last_os_error());
            }
            Ok(())
        });
    }
    let child = command.spawn().map_err(|e| format!("{program}: {e}"))?;
    // The fire's copy keeps the lock; it is reaped with waitpid.
    Ok(Some(child.id() as libc::pid_t))
}

/// In a fire: its trigger's lock stays its own, not inherited by what it
/// starts, such as a daemon.
pub fn keep_lock() {
    let Some(fd) = std::env::var(LOCK_FD)
        .ok()
        .and_then(|fd| fd.parse::<libc::c_int>().ok())
    else {
        return;
    };
    // SAFETY: fcntl on a descriptor this process inherited.
    unsafe {
        let flags = libc::fcntl(fd, libc::F_GETFD);
        if flags >= 0 {
            libc::fcntl(fd, libc::F_SETFD, flags | libc::FD_CLOEXEC);
        }
    }
    // SAFETY: nothing else runs yet: the fire starts no thread before this.
    unsafe { std::env::remove_var(LOCK_FD) };
}

/// When a folder last changed, to read it again only then.
fn modified(dir: &Path) -> Option<(i64, i64)> {
    use std::os::unix::fs::MetadataExt;
    std::fs::metadata(dir)
        .ok()
        .map(|m| (m.mtime(), m.mtime_nsec()))
}

/// HOME gone: the scheduler removes its own job from the system, so it
/// runs no more.
fn let_job_go() -> i32 {
    let home = PathBuf::from(std::env::var_os("HOME").unwrap_or_default());
    eprintln!(
        "{}",
        trigger::error_json("home_gone: its HOME is gone; the scheduler lets its job go")
    );
    let scheduler = Scheduler::here();
    let file = job_file(scheduler, &home);
    let _ = std::fs::remove_file(&file);
    let_go(
        scheduler,
        file.parent().unwrap_or(Path::new("")),
        &label(&home),
    );
    0
}

/// Whether HOME is gone, as a test's or an eval's is.
fn orphaned() -> bool {
    std::env::var_os("HOME").is_some_and(|home| {
        matches!(std::fs::symlink_metadata(home), Err(e) if e.kind() == std::io::ErrorKind::NotFound)
    })
}

/// Up to when each trigger's calendar was looked at when it last came, by
/// its generation, kept across restarts of the scheduler: a time missed
/// while none ran comes once when one runs. Kept whole in `looked`, and
/// each change since, a line, in `looked.log`: a fire's start writes its
/// own trigger's line, not every trigger's. Once the log would hold as
/// many lines as there are triggers kept, the whole is written again and
/// the log starts over, so the log stays shorter than the whole. Each line
/// is numbered, and the whole says the last number it holds, so lines a
/// crash left in the log after the whole was written are passed over.
#[derive(Debug, Default, PartialEq)]
struct Kept {
    times: HashMap<String, (String, i64)>,
    /// Those changed since they were last written.
    changed: BTreeSet<String>,
    /// Lines in the log.
    logged: usize,
    /// The number of the last change written.
    last: u64,
}

/// The key of the whole that holds the number of the last change in it:
/// no trigger's name starts with a dot.
const LAST: &str = ".last";

impl Kept {
    fn get(&self, name: &str) -> Option<&(String, i64)> {
        self.times.get(name)
    }

    fn insert(&mut self, name: String, kept: (String, i64)) {
        self.changed.insert(name.clone());
        self.times.insert(name, kept);
    }

    fn remove(&mut self, name: &str) {
        if self.times.remove(name).is_some() {
            self.changed.insert(name.to_owned());
        }
    }

    fn retain(&mut self, keep: impl Fn(&str) -> bool) {
        let gone: Vec<String> = (self.times.keys())
            .filter(|name| !keep(name))
            .cloned()
            .collect();
        for name in gone {
            self.remove(&name);
        }
    }

    #[cfg(test)]
    fn is_empty(&self) -> bool {
        self.times.is_empty()
    }

    #[cfg(test)]
    fn clear(&mut self) {
        *self = Self::default();
    }

    /// What the last scheduler kept: the whole, then each change in the
    /// log after it. A line cut short by a crash is left out, and the log
    /// is written into the whole at once, so the next line starts clean.
    fn read(places: &Places) -> Self {
        let whole = std::fs::read(places.looked())
            .ok()
            .and_then(|bytes| serde_json::from_slice::<serde_json::Value>(&bytes).ok())
            .unwrap_or_default();
        let mut times: HashMap<String, (String, i64)> = (whole.as_object().into_iter())
            .flatten()
            .filter_map(|(name, v)| {
                Some((
                    name.clone(),
                    (v["generation"].as_str()?.to_owned(), v["looked"].as_i64()?),
                ))
            })
            .collect();
        let mut last = whole[LAST].as_u64().unwrap_or(0);
        let log = std::fs::read(places.looked_log()).unwrap_or_default();
        for line in log.split(|byte| *byte == b'\n') {
            let Ok(change) = serde_json::from_slice::<serde_json::Value>(line) else {
                continue;
            };
            let (Some(name), Some(number)) = (change["name"].as_str(), change["n"].as_u64()) else {
                continue;
            };
            // Already in the whole, written after it.
            if number <= last {
                continue;
            }
            last = number;
            match (change["generation"].as_str(), change["looked"].as_i64()) {
                (Some(generation), Some(looked)) => {
                    times.insert(name.to_owned(), (generation.to_owned(), looked))
                }
                _ => times.remove(name),
            };
        }
        let mut kept = Self {
            times,
            last,
            ..Self::default()
        };
        if !log.is_empty() {
            // Until the log starts over, its last line may be cut short:
            // the next write is the whole again, never a line after it.
            kept.logged = kept.times.len();
            kept.write_whole(places);
        }
        kept
    }

    /// What changed since it was last written, a line each in the log;
    /// the whole, once the log would be as long as what is kept.
    fn write(&mut self, places: &Places) {
        if self.changed.is_empty() {
            return;
        }
        if self.logged + self.changed.len() >= self.times.len() {
            self.write_whole(places);
            return;
        }
        let mut lines = Vec::new();
        let mut last = self.last;
        for name in &self.changed {
            last += 1;
            let change = match self.times.get(name) {
                Some((generation, looked)) => serde_json::json!({
                    "n": last, "name": name, "generation": generation, "looked": looked
                }),
                None => serde_json::json!({ "n": last, "name": name }),
            };
            lines.extend(change.to_string().into_bytes());
            lines.push(b'\n');
        }
        let path = places.looked_log();
        // The log is made only with the whole, so that its name lasts.
        let opened = std::fs::OpenOptions::new().append(true).open(&path);
        if matches!(&opened, Err(e) if e.kind() == std::io::ErrorKind::NotFound) {
            self.write_whole(places);
            return;
        }
        let appended = opened.and_then(|mut log| {
            std::io::Write::write_all(&mut log, &lines)?;
            log.sync_data()
        });
        match appended {
            Ok(()) => {
                written(lines.len());
                self.logged += self.changed.len();
                self.changed.clear();
                self.last = last;
            }
            Err(error) => {
                // Some of the lines may be in the log, the last cut short:
                // their numbers are not used again, and the next write is
                // the whole, which passes over them and starts the log over.
                self.last = last;
                self.logged = self.times.len();
                let error = format!("{}: {error}", path.display());
                eprintln!("{}", trigger::error_json(&error));
            }
        }
    }

    /// Everything kept, written whole, and the log started over.
    fn write_whole(&mut self, places: &Places) {
        #[cfg(test)]
        if WHOLE_FAILS.with(|fails| fails.get()) {
            return;
        }
        // Every change so far is in it, those not yet logged too.
        let last = self.last + self.changed.len() as u64;
        let mut value: serde_json::Map<String, serde_json::Value> = (self.times.iter())
            .map(|(name, (generation, looked))| {
                (
                    name.clone(),
                    serde_json::json!({"generation": generation, "looked": looked}),
                )
            })
            .collect();
        value.insert(LAST.to_owned(), last.into());
        let whole = serde_json::Value::Object(value).to_string();
        // The log is made, when it is not yet, before the whole is written,
        // whose write syncs the folder: its name lasts as the whole's does.
        let path = places.looked_log();
        let log = (|| {
            use std::os::unix::fs::OpenOptionsExt;
            if let Some(folder) = path.parent() {
                std::fs::create_dir_all(folder)?;
            }
            std::fs::OpenOptions::new()
                .write(true)
                .create(true)
                .truncate(false)
                .mode(0o600)
                .open(&path)
        })();
        if let Err(error) = trigger::replace_mode(&places.looked(), whole.as_bytes(), 0o600) {
            eprintln!("{}", trigger::error_json(&error));
            return;
        }
        written(whole.len());
        self.changed.clear();
        // The lines left in the log, should it not start over, are passed
        // over when read: each is numbered at most the whole's last.
        self.last = last;
        let started_over = log.and_then(|log| log.set_len(0).and_then(|()| log.sync_data()));
        match started_over {
            Ok(()) => self.logged = 0,
            Err(e) => {
                let error = format!("{}: {e}", path.display());
                eprintln!("{}", trigger::error_json(&error));
            }
        }
    }
}

impl std::ops::Index<&str> for Kept {
    type Output = (String, i64);
    fn index(&self, name: &str) -> &(String, i64) {
        &self.times[name]
    }
}

// Counts the bytes the kept times are written in, in tests: a fire's
// start writes its own trigger's, not every trigger's.
#[cfg(test)]
thread_local!(static WRITTEN: std::cell::Cell<usize> = const { std::cell::Cell::new(0) });

// Makes the whole fail to be written, in tests, as a full disk does.
#[cfg(test)]
thread_local!(static WHOLE_FAILS: std::cell::Cell<bool> = const { std::cell::Cell::new(false) });

/// `bytes` of the kept times written.
fn written(bytes: usize) {
    #[cfg(test)]
    WRITTEN.with(|written| written.set(written.get() + bytes));
    #[cfg(not(test))]
    let _ = bytes;
}

/// The doorbell's own writer, for the SIGCHLD handler.
static BELL: std::sync::atomic::AtomicI32 = std::sync::atomic::AtomicI32::new(-1);

/// The byte a fire's end writes on the doorbell: it wakes the scheduler
/// for the fires that ended, and is no ring, which every other writer's
/// byte is, to read the definitions and every queue again.
const ENDED: u8 = 2;

extern "C" fn rung(_: libc::c_int) {
    let fd = BELL.load(std::sync::atomic::Ordering::Relaxed);
    if fd >= 0 {
        // SAFETY: write is async-signal-safe; a full pipe drops the byte.
        unsafe { libc::write(fd, [ENDED].as_ptr().cast(), 1) };
    }
}

/// The doorbell: a FIFO the scheduler reads, holding a lock on it so one
/// scheduler runs per HOME. It keeps a writer of its own open, so the FIFO
/// never reads as closed between rings.
struct Doorbell {
    read: std::fs::File,
    _write: std::fs::File,
    /// One scheduler per HOME.
    _lock: std::fs::File,
}

impl Doorbell {
    fn open(places: &Places) -> Result<Option<Self>, String> {
        use std::os::fd::AsRawFd;
        use std::os::unix::fs::{FileTypeExt, OpenOptionsExt};
        // Another scheduler of this HOME has it.
        let Some(lock) = locked(&places.scheduling())? else {
            return Ok(None);
        };
        let path = places.doorbell();
        std::fs::create_dir_all(&places.state)
            .map_err(|e| format!("{}: {e}", places.state.display()))?;
        match std::fs::symlink_metadata(&path) {
            Ok(meta) if meta.file_type().is_fifo() => {}
            Ok(_) => std::fs::remove_file(&path).map_err(|e| format!("{}: {e}", path.display()))?,
            Err(_) => {}
        }
        if !path.exists() {
            let c = std::ffi::CString::new(path.as_os_str().as_encoded_bytes())
                .map_err(|e| e.to_string())?;
            // SAFETY: mkfifo reads the path it is given.
            if unsafe { libc::mkfifo(c.as_ptr(), 0o600) } != 0 {
                let error = std::io::Error::last_os_error();
                if error.kind() != std::io::ErrorKind::AlreadyExists {
                    return Err(format!("{}: {error}", path.display()));
                }
            }
        }
        let read = std::fs::OpenOptions::new()
            .read(true)
            .custom_flags(libc::O_NONBLOCK)
            .open(&path)
            .map_err(|e| format!("{}: {e}", path.display()))?;
        let write = std::fs::OpenOptions::new()
            .write(true)
            .custom_flags(libc::O_NONBLOCK)
            .open(&path)
            .map_err(|e| format!("{}: {e}", path.display()))?;
        // A fire that ends rings it too: its queue is looked at again.
        BELL.store(write.as_raw_fd(), std::sync::atomic::Ordering::Relaxed);
        // SAFETY: the handler only writes a byte to a descriptor kept open
        // as long as the scheduler runs, which is async-signal-safe.
        unsafe {
            let mut action: libc::sigaction = std::mem::zeroed();
            action.sa_sigaction = rung as *const () as libc::sighandler_t;
            action.sa_flags = libc::SA_RESTART | libc::SA_NOCLDSTOP;
            libc::sigemptyset(&mut action.sa_mask);
            libc::sigaction(libc::SIGCHLD, &action, std::ptr::null_mut());
        }
        Ok(Some(Self {
            read,
            _write: write,
            _lock: lock,
        }))
    }

    /// Wait up to `wait` for a ring or a fire's end; whether a ring came.
    fn wait(&self, wait: Duration) -> bool {
        use std::io::Read;
        use std::os::fd::AsRawFd;
        let mut poll = libc::pollfd {
            fd: self.read.as_raw_fd(),
            events: libc::POLLIN,
            revents: 0,
        };
        let ms = wait.as_millis().min(i32::MAX as u128) as libc::c_int;
        // SAFETY: poll reads and writes the one pollfd it is given.
        let ready = unsafe { libc::poll(&mut poll, 1, ms) };
        if ready <= 0 {
            return false;
        }
        let mut buffer = [0u8; 256];
        let mut rang = false;
        while let Ok(n @ 1..) = (&self.read).read(&mut buffer) {
            rang |= buffer[..n].iter().any(|byte| *byte != ENDED);
            if n < buffer.len() {
                break;
            }
        }
        rang
    }
}

/// The first calendar time after `after`, within eight years and a day,
/// which holds a 29 February past a century that has none; none for no
/// entries. A field an entry leaves out is any value, as launchd reads
/// it; weekday 0 and 7 are Sunday.
pub fn next_after(entries: &[Entry], after: i64) -> Option<i64> {
    if entries.is_empty() {
        return None;
    }
    let first = after - after.rem_euclid(60) + 60;
    let start = trigger::local(first);
    let day0 = days_from_civil(start.year, start.month, start.day);
    // How far the clock reads ahead of UTC at `at`.
    let ahead = |at: i64| {
        let clock = trigger::local(at);
        let day = days_from_civil(clock.year, clock.month, clock.day);
        day * 86_400 + clock.hour as i64 * 3600 + clock.minute as i64 * 60 - at
    };
    // The times of the first day before `start` all came before it, so
    // none is tried, unless the clock goes back later that day and comes
    // through some of them again.
    let came = ahead(first + 86_400) >= ahead(first);
    for offset in 0..=(8 * 366 + 1) {
        let before = |hour: u8, minute: u8| {
            offset == 0 && came && (hour, minute) < (start.hour, start.minute)
        };
        let (year, month, day) = civil_from_days(day0 + offset);
        // 1970-01-01 was a Thursday.
        let weekday = (day0 + offset + 4).rem_euclid(7) as u8;
        let mut best: Option<i64> = None;
        for entry in entries {
            let matches = entry.month.is_none_or(|m| m == month)
                && entry.day.is_none_or(|d| d == day)
                && entry.weekday.is_none_or(|w| w % 7 == weekday);
            if !matches {
                continue;
            }
            let hours: Vec<u8> = entry.hour.map_or_else(|| (0..24).collect(), |h| vec![h]);
            let minutes: Vec<u8> = entry.minute.map_or_else(|| (0..60).collect(), |m| vec![m]);
            'hours: for &hour in &hours {
                if before(hour, 59) {
                    continue;
                }
                for &minute in &minutes {
                    if before(hour, minute) {
                        continue;
                    }
                    visit();
                    let local = trigger::Local {
                        year,
                        month,
                        day,
                        hour,
                        minute,
                    };
                    // A time the clock goes through twice as daylight
                    // saving ends: an entry with a set hour comes at the
                    // first, as cron runs a job at a set time once; one
                    // that comes every hour comes at both, as time goes.
                    let mut ats = trigger::epochs_of(local);
                    if entry.hour.is_some() {
                        ats.truncate(1);
                    }
                    if let Some((i, at)) = ats.into_iter().enumerate().find(|(_, at)| *at >= first)
                    {
                        best = Some(best.map_or(at, |b| b.min(at)));
                        // Later minutes this hour, and later hours, come
                        // later than a first time. A second time does not:
                        // after 01:30 the first time round, 01:31 comes
                        // before the second 01:00, so the search goes on.
                        if i == 0 {
                            break 'hours;
                        }
                    }
                }
            }
        }
        if best.is_some() {
            return best;
        }
    }
    None
}

/// Days since 1970-01-01 of a date in the proleptic Gregorian calendar.
fn days_from_civil(year: i32, month: u8, day: u8) -> i64 {
    let y = i64::from(year) - i64::from(month <= 2);
    let era = y.div_euclid(400);
    let yoe = y - era * 400;
    let m = i64::from(month);
    let doy = (153 * (m + if m > 2 { -3 } else { 9 }) + 2) / 5 + i64::from(day) - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + doe - 719_468
}

/// The date `days` since 1970-01-01.
fn civil_from_days(days: i64) -> (i32, u8, u8) {
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = (doy - (153 * mp + 2) / 5 + 1) as u8;
    let month = (if mp < 10 { mp + 3 } else { mp - 9 }) as u8;
    let year = (yoe + era * 400 + i64::from(month <= 2)) as i32;
    (year, month, day)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_window_ending_while_it_waits_apart_ends_at_once() {
        // Another process holding the lock: the wait never ends by itself.
        let (_hold, held) = std::sync::mpsc::channel::<()>();
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(1)
            .enable_all()
            .build()
            .unwrap();
        runtime.spawn(apart(move || held.recv().ok()));
        runtime.block_on(async { tokio::time::sleep(std::time::Duration::from_millis(50)).await });
        let ending = std::time::Instant::now();
        drop(runtime);
        assert!(ending.elapsed() < std::time::Duration::from_secs(2));
        // What it waited for, once given, goes to nothing awaiting it.
        let (sent, got) = std::sync::mpsc::channel();
        let runtime = tokio::runtime::Runtime::new().unwrap();
        let waiting = runtime.spawn(apart(move || got.recv().ok()));
        runtime.block_on(async { tokio::time::sleep(std::time::Duration::from_millis(50)).await });
        waiting.abort();
        sent.send(7).unwrap();
        assert!(runtime.block_on(waiting).is_err());
    }

    fn clock(y: i32, mo: u8, d: u8, h: u8, mi: u8) -> i64 {
        trigger::epoch_of(trigger::Local {
            year: y,
            month: mo,
            day: d,
            hour: h,
            minute: mi,
        })
        .unwrap()
    }

    #[test]
    fn dates_and_days_agree() {
        for days in [-1, 0, 59, 365, 11_016, 20_000, 20_757] {
            let (y, m, d) = civil_from_days(days);
            assert_eq!(days_from_civil(y, m, d), days);
        }
        assert_eq!(civil_from_days(0), (1970, 1, 1));
        assert_eq!(days_from_civil(2026, 10, 31), 20_757);
    }

    /// [`next_after`] as it was, trying every time of the first day.
    fn next_after_trying_every_time(entries: &[Entry], after: i64) -> Option<i64> {
        if entries.is_empty() {
            return None;
        }
        let first = after - after.rem_euclid(60) + 60;
        let start = trigger::local(first);
        let day0 = days_from_civil(start.year, start.month, start.day);
        for offset in 0..=(8 * 366 + 1) {
            let (year, month, day) = civil_from_days(day0 + offset);
            // 1970-01-01 was a Thursday.
            let weekday = (day0 + offset + 4).rem_euclid(7) as u8;
            let mut best: Option<i64> = None;
            for entry in entries {
                let matches = entry.month.is_none_or(|m| m == month)
                    && entry.day.is_none_or(|d| d == day)
                    && entry.weekday.is_none_or(|w| w % 7 == weekday);
                if !matches {
                    continue;
                }
                let hours: Vec<u8> = entry.hour.map_or_else(|| (0..24).collect(), |h| vec![h]);
                let minutes: Vec<u8> = entry.minute.map_or_else(|| (0..60).collect(), |m| vec![m]);
                'hours: for &hour in &hours {
                    for &minute in &minutes {
                        let local = trigger::Local {
                            year,
                            month,
                            day,
                            hour,
                            minute,
                        };
                        // A time the clock goes through twice as daylight
                        // saving ends: an entry with a set hour comes at the
                        // first, as cron runs a job at a set time once; one
                        // that comes every hour comes at both, as time goes.
                        let mut ats = trigger::epochs_of(local);
                        if entry.hour.is_some() {
                            ats.truncate(1);
                        }
                        if let Some((i, at)) =
                            ats.into_iter().enumerate().find(|(_, at)| *at >= first)
                        {
                            best = Some(best.map_or(at, |b| b.min(at)));
                            // Later minutes this hour, and later hours, come
                            // later than a first time. A second time does not:
                            // after 01:30 the first time round, 01:31 comes
                            // before the second 01:00, so the search goes on.
                            if i == 0 {
                                break 'hours;
                            }
                        }
                    }
                }
            }
            if best.is_some() {
                return best;
            }
        }
        None
    }

    /// Every 17 minutes for three days from `from`: [`next_after`] finds
    /// what trying every time found, for each of a few calendars; how
    /// many times it tried, at most, for every minute and for one minute
    /// an hour, on days the clock does not go back.
    fn next_times_agree(from: i64) -> (usize, usize) {
        let calendars = [
            "* * * * *",
            "7 * * * *",
            "30 1 * * *",
            "0 9 * * 1",
            "45 23 * * *",
        ];
        let (mut every, mut hourly) = (0, 0);
        for (i, calendar) in calendars.iter().enumerate() {
            let entries = trigger::cron(calendar).unwrap().entries;
            let mut after = from;
            while after < from + 3 * 86_400 {
                VISITED.with(|visited| visited.set(0));
                let next = next_after(&entries, after);
                let tried = VISITED.with(|visited| visited.get());
                assert_eq!(
                    next,
                    next_after_trying_every_time(&entries, after),
                    "{calendar} after {after}"
                );
                match i {
                    0 => every = every.max(tried),
                    1 => hourly = hourly.max(tried),
                    _ => {}
                }
                after += 17 * 60 + 13;
            }
        }
        (every, hourly)
    }

    #[test]
    fn the_next_time_tries_no_time_of_the_day_before_the_first_it_may_be() {
        // Late in the day, every minute tried each time of the day before
        // it: 1,410 at 23:30. Now one, and one or two for a time an hour.
        let (every, hourly) = next_times_agree(clock(2026, 10, 30, 0, 0) + 11);
        assert_eq!(every, 1);
        assert!(hourly <= 2, "{hourly}");
    }

    #[test]
    fn the_next_time_agrees_across_the_nights_the_clock_changes() {
        if std::env::var_os("AGENT_TEST_DST").is_none() {
            let me = std::env::current_exe().unwrap();
            let out = std::process::Command::new(me)
                .args([
                    "--exact",
                    "scheduler::tests::the_next_time_agrees_across_the_nights_the_clock_changes",
                    "--nocapture",
                ])
                .env("AGENT_TEST_DST", "1")
                .env("TZ", "America/New_York")
                .output()
                .unwrap();
            assert!(
                out.status.success() && String::from_utf8_lossy(&out.stdout).contains("1 passed"),
                "{}{}",
                String::from_utf8_lossy(&out.stdout),
                String::from_utf8_lossy(&out.stderr)
            );
            return;
        }
        // The days around 2026-11-01, when 01:00 to 02:00 comes twice, and
        // 2027-03-14, when 02:00 to 03:00 never comes.
        next_times_agree(1_793_512_800 - 2 * 86_400);
        next_times_agree(1_805_007_600 - 2 * 86_400);
    }

    #[test]
    fn the_next_time_is_the_first_entry_after() {
        let every_30 = trigger::every("30m", clock(2026, 10, 31, 9, 7)).unwrap();
        // Entries at :07 and :37; the time itself is not after.
        assert_eq!(
            next_after(&every_30.entries, clock(2026, 10, 31, 9, 7)),
            Some(clock(2026, 10, 31, 9, 37))
        );
        assert_eq!(
            next_after(&every_30.entries, clock(2026, 10, 31, 9, 6) + 59),
            Some(clock(2026, 10, 31, 9, 7))
        );
        // Mondays at 9:00; 2026-10-31 is a Saturday.
        let monday = trigger::cron("0 9 * * 1").unwrap();
        assert_eq!(
            next_after(&monday.entries, clock(2026, 10, 31, 12, 0)),
            Some(clock(2026, 11, 2, 9, 0))
        );
        // Day or weekday, as cron reads both.
        let either = trigger::cron("0 0 1 * 0").unwrap();
        assert_eq!(
            next_after(&either.entries, clock(2026, 10, 31, 12, 0)),
            Some(clock(2026, 11, 1, 0, 0))
        );
        assert_eq!(
            next_after(&either.entries, clock(2026, 11, 1, 0, 0)),
            Some(clock(2026, 11, 8, 0, 0))
        );
        // A one-off comes once a year; nothing for no entries.
        let once = trigger::at("2026-12-25 07:30", clock(2026, 10, 31, 12, 0)).unwrap();
        assert_eq!(
            next_after(&once.entries, clock(2026, 10, 31, 12, 0)),
            Some(clock(2026, 12, 25, 7, 30))
        );
        assert_eq!(next_after(&[], 0), None);
        // Sparse, beyond a year: the next 29 February.
        let leap = trigger::cron("0 0 29 2 *").unwrap();
        assert_eq!(
            next_after(&leap.entries, clock(2026, 10, 31, 12, 0)),
            Some(clock(2028, 2, 29, 0, 0))
        );
    }

    #[test]
    fn ten_thousand_due_triggers_start_a_fleet_at_a_time_and_none_is_lost() {
        let job = || Job {
            program: vec!["/A/sonny".into()],
            environment: Vec::new(),
            calendar: Vec::new(),
            watch: None,
        };
        let mut followed: HashMap<String, Followed> = (0..10_000)
            .map(|i| {
                let f = Followed {
                    file: (0, 0, 0),
                    job: job(),
                    generation: "g".into(),
                    looked: 0,
                    next: None,
                    seen: None,
                    again: true,
                    started: None,
                    failures: 0,
                    retry: None,
                    unkept: None,
                    queued: None,
                };
                (format!("t{i:05}"), f)
            })
            .collect();
        let mut fleet = Fleet {
            running: HashMap::new(),
            busy: std::collections::HashSet::new(),
            cap: 64,
            line: 0,
        };
        let (mut pid, mut fired) = (0, std::collections::HashSet::new());
        let mut now = std::time::Instant::now();
        // Every third start fails the first time it is tried.
        let mut failed = std::collections::HashSet::new();
        let mut spawn = |name: &str, _: &Job| {
            let n: usize = name[1..].parse().unwrap();
            if n.is_multiple_of(3) && failed.insert(name.to_owned()) {
                return Err(format!("{name}: no such file"));
            }
            pid += 1;
            Ok(Some(pid))
        };
        for _ in 0..1_000 {
            for (name, f) in followed.iter_mut() {
                if std::mem::take(&mut f.again) {
                    launch(name, f, &mut fleet, now, &mut spawn);
                }
            }
            assert!(fleet.running.len() <= fleet.cap);
            // Every fire ends, and time passes past any wait.
            for pid in fleet.running.keys().copied().collect::<Vec<_>>() {
                fired.insert(fleet.ended(pid).unwrap());
            }
            now += LOOK_EVERY * 5;
            if followed.values().all(|f| !f.again) {
                break;
            }
        }
        assert_eq!(fired.len(), 10_000, "every due trigger fired once");
        assert!(
            followed
                .values()
                .all(|f| f.failures == 0 && f.retry.is_none())
        );
        // A failed start waits, doubling, rather than spinning.
        let f = followed.get_mut("t00000").unwrap();
        f.again = true;
        let mut failing = |_: &str, _: &Job| Err::<Option<libc::pid_t>, String>("gone".into());
        launch("t00000", f, &mut fleet, now, &mut failing);
        launch("t00000", f, &mut fleet, now, &mut failing);
        assert_eq!((f.failures, f.again), (1, true));
        assert_eq!(f.retry, Some(now + AGAIN_AFTER));
        launch("t00000", f, &mut fleet, now + AGAIN_AFTER, &mut failing);
        assert_eq!(f.retry, Some(now + AGAIN_AFTER * 3));
        // A fire from before a restart holds the lock: it waits too, up
        // to a minute, rather than being tried at every pass.
        let f = followed.get_mut("t00001").unwrap();
        let mut held = |_: &str, _: &Job| Ok::<Option<libc::pid_t>, String>(None);
        let mut at = now;
        for _ in 0..20 {
            f.again = true;
            launch("t00001", f, &mut fleet, at, &mut held);
            assert!(f.again && f.retry.is_some_and(|r| r > at));
            assert!(f.retry.unwrap() - at <= LOOK_EVERY);
            at = f.retry.unwrap();
        }
        assert_eq!(f.retry, Some(at));
        let mut ok = |_: &str, _: &Job| Ok::<Option<libc::pid_t>, String>(Some(7));
        launch("t00001", f, &mut fleet, at, &mut ok);
        assert_eq!((f.failures, f.retry), (0, None));
    }

    #[test]
    fn a_place_in_the_fleet_goes_to_the_one_waiting_longest() {
        let follow = || Followed {
            file: (0, 0, 0),
            job: Job {
                program: vec!["/A/sonny".into()],
                environment: Vec::new(),
                calendar: Vec::new(),
                watch: None,
            },
            generation: "g".into(),
            looked: 0,
            next: None,
            seen: None,
            again: false,
            started: None,
            failures: 0,
            retry: None,
            unkept: None,
            queued: None,
        };
        let mut schedule = Schedule {
            followed: ["a", "b", "z"].map(|n| (n.to_owned(), follow())).into(),
            ..Schedule::default()
        };
        let mut fleet = Fleet {
            running: HashMap::new(),
            busy: std::collections::HashSet::new(),
            cap: 2,
            line: 0,
        };
        let mut kept = Kept::default();
        let mut pid = 0;
        let mut spawn = |_: &str, _: &Job| {
            pid += 1;
            Ok::<_, String>(Some(pid))
        };
        let ready = |names: &[&str]| {
            let mut ready = Ready::default();
            names.iter().for_each(|name| ready.due(name));
            ready
        };
        let now = std::time::Instant::now();
        schedule.admit(
            ready(&["a", "b", "z"]),
            &mut fleet,
            now,
            &mut kept,
            &mut spawn,
        );
        assert!(fleet.busy.contains("a") && fleet.busy.contains("b"));
        assert_eq!(schedule.followed["z"].queued, Some(0));
        // a's fire ends and a is due again at once: z, in line, goes first.
        let a = *fleet.running.iter().find(|(_, n)| *n == "a").unwrap().0;
        fleet.ended(a);
        let later = now + AGAIN_AFTER * 2;
        schedule.admit(ready(&["a", "z"]), &mut fleet, later, &mut kept, &mut spawn);
        assert!(fleet.busy.contains("z") && !fleet.busy.contains("a"));
        assert_eq!(schedule.followed["z"].queued, None);
        assert_eq!(schedule.followed["a"].queued, Some(1));
        assert!(schedule.followed["a"].again);
    }

    #[test]
    fn an_ask_whose_lock_another_fire_holds_waits_in_its_queue() {
        let f = Followed {
            file: (0, 0, 0),
            job: Job {
                program: vec!["/A/sonny".into()],
                environment: Vec::new(),
                calendar: Vec::new(),
                watch: None,
            },
            generation: "g".into(),
            looked: 0,
            next: None,
            seen: None,
            again: false,
            started: None,
            failures: 0,
            retry: None,
            unkept: None,
            queued: None,
        };
        let mut schedule = Schedule {
            followed: [("t".to_owned(), f)].into(),
            ..Schedule::default()
        };
        let mut fleet = Fleet::new();
        let mut kept = Kept::default();
        let mut held = |_: &str, _: &Job| Ok::<Option<libc::pid_t>, String>(None);
        let now = std::time::Instant::now();
        // An ask alone: not due again, so no fire follows the one that
        // holds the lock once it ends; the queue is looked at again.
        let mut asked = Ready::default();
        asked.asked("t");
        schedule.admit(asked, &mut fleet, now, &mut kept, &mut held);
        let f = &schedule.followed["t"];
        assert_eq!((f.again, f.retry), (false, None));
        // A time or a change: due again, after a wait.
        let mut due = Ready::default();
        due.due("t");
        schedule.admit(due, &mut fleet, now, &mut kept, &mut held);
        let f = &schedule.followed["t"];
        assert!(f.again && f.retry.is_some_and(|at| at > now));
    }

    /// A trigger as the fleet tests follow it: due again when `again`, a
    /// calendar time each hour at the minute `timed` names.
    fn followed(again: bool, timed: Option<usize>, now: i64) -> Followed {
        let calendar = match timed {
            Some(minute) => trigger::cron(&format!("{minute} * * * *")).unwrap().entries,
            None => Vec::new(),
        };
        Followed {
            file: (0, 0, 0),
            next: next_after(&calendar, now),
            job: Job {
                program: vec!["/A/sonny".into()],
                environment: Vec::new(),
                calendar,
                watch: None,
            },
            generation: "g".into(),
            looked: now,
            seen: None,
            again,
            started: None,
            failures: 0,
            retry: None,
            unkept: None,
            queued: None,
        }
    }

    /// A wake as the scheduler took it when it looked at every trigger:
    /// each one's calendar and queue, then those ready, sorted, launched.
    #[allow(clippy::too_many_arguments)]
    fn every_trigger(
        followed: &mut HashMap<String, Followed>,
        fleet: &mut Fleet,
        now: i64,
        instant: std::time::Instant,
        asks: bool,
        ended: &std::collections::HashSet<String>,
        asked: &mut dyn FnMut(&str) -> bool,
        kept: &mut Kept,
        spawn: &mut Spawn,
    ) -> bool {
        let mut fired = false;
        let mut ready = Vec::new();
        for (name, f) in followed.iter_mut() {
            let busy = fleet.busy.contains(name);
            let mut due = std::mem::take(&mut f.again);
            if came(name, f, busy, now, kept) {
                fired |= busy;
                due |= !busy;
            }
            let ask = !busy && (asks || ended.contains(name)) && asked(name);
            if due || ask {
                ready.push((name.clone(), !due));
            }
        }
        let mut ready: Vec<_> = (ready.into_iter())
            .map(|(name, ask)| {
                let queued = followed[&name].queued;
                (queued.is_none(), queued.unwrap_or(0), name, ask)
            })
            .collect();
        ready.sort_unstable();
        for (_, _, name, ask) in ready {
            let f = followed.get_mut(&name).unwrap();
            if launch(&name, f, fleet, instant, spawn) == Launched::Held && ask {
                (f.again, f.failures, f.retry) = (false, 0, None);
            }
            fired |= started(&name, f, instant, kept);
        }
        fired
    }

    /// What a test compares of a trigger.
    type State = (
        bool,
        Option<u64>,
        Option<i64>,
        i64,
        u32,
        Option<std::time::Instant>,
    );

    #[test]
    fn a_wake_starts_the_fires_looking_at_every_trigger_started() {
        // Two schedulers, one looking at every trigger and this one, over
        // the same 3,000 wakes: fires end, time passes, asks come, starts
        // fail or find a fire from before holding the lock, triggers are
        // read again due. Every wake starts the same fires in the same
        // order and leaves every trigger the same.
        let start = 1_799_999_940;
        let base = std::time::Instant::now();
        let names: Vec<String> = (0..300).map(|i| format!("t{i:03}")).collect();
        let make = |i: usize| {
            followed(
                i.is_multiple_of(5),
                i.is_multiple_of(4).then_some(i % 3 * 20),
                start,
            )
        };
        let mut old: HashMap<String, Followed> = (names.iter().enumerate())
            .map(|(i, n)| (n.clone(), make(i)))
            .collect();
        let mut new = Schedule::default();
        for (i, name) in names.iter().enumerate() {
            new.followed.insert(name.clone(), make(i));
            new.file(name, base);
        }
        let fleet = || Fleet {
            running: HashMap::new(),
            busy: std::collections::HashSet::new(),
            cap: 4,
            line: 0,
        };
        let (mut old_fleet, mut new_fleet) = (fleet(), fleet());
        let (mut old_kept, mut new_kept) = (Kept::default(), Kept::default());
        let mut seed: u64 = 7;
        let mut roll = move |n: u64| {
            seed = seed.wrapping_mul(6_364_136_223_846_793_005).wrapping_add(1);
            (seed >> 33) % n
        };
        // How each try to start a trigger goes, by how many it had: the
        // same in both, as each tries the same.
        let outcome = |name: &str, tries: u32| {
            let hash = name
                .bytes()
                .fold(tries as u64, |h, b| h.wrapping_mul(31) + b as u64);
            match hash % 10 {
                0 => Err(format!("{name}: gone")),
                1 => Ok(None),
                _ => Ok(Some(())),
            }
        };
        let (mut old_tries, mut new_tries) = (HashMap::new(), HashMap::new());
        let (mut now, mut instant) = (start, base);
        for wake in 0..3_000 {
            now += roll(40) as i64;
            instant += Duration::from_millis(roll(4_000));
            // Some fires end, the same in both.
            let mut running: Vec<String> = old_fleet.busy.iter().cloned().collect();
            running.sort();
            let ended: std::collections::HashSet<String> =
                (running.into_iter()).filter(|_| roll(4) == 0).collect();
            for (fleet, running) in [(&mut old_fleet, true), (&mut new_fleet, false)] {
                let _ = running;
                let pids: Vec<_> = (fleet.running.iter())
                    .filter(|(_, name)| ended.contains(*name))
                    .map(|(pid, _)| *pid)
                    .collect();
                for pid in pids {
                    fleet.ended(pid);
                }
            }
            // A ring, asks in some queues, a trigger read again due.
            let asks = roll(4) == 0;
            let asking: std::collections::HashSet<String> = (0..roll(3))
                .map(|_| names[roll(300) as usize].clone())
                .collect();
            // At the start every trigger was read.
            let mut fresh = match wake {
                0 => names.clone(),
                _ => Vec::new(),
            };
            for _ in 0..roll(4) {
                let name = names[roll(300) as usize].clone();
                old.get_mut(&name).unwrap().again = true;
                new.followed.get_mut(&name).unwrap().again = true;
                new.file(&name, instant);
                fresh.push(name);
            }
            let (mut old_started, mut new_started) = (Vec::new(), Vec::new());
            let mut pid = 0;
            let old_fired = every_trigger(
                &mut old,
                &mut old_fleet,
                now,
                instant,
                asks,
                &ended,
                &mut |name| asking.contains(name),
                &mut old_kept,
                &mut |name, _| {
                    let tries = old_tries.entry(name.to_owned()).or_insert(0);
                    *tries += 1;
                    outcome(name, *tries).map(|ok| {
                        ok.map(|()| {
                            old_started.push(name.to_owned());
                            pid += 1;
                            pid
                        })
                    })
                },
            );
            let mut pid = 0;
            let new_fired = new.pass(
                &mut new_fleet,
                now,
                instant,
                false,
                asks,
                &ended,
                &fresh,
                &mut |name| asking.contains(name),
                &mut new_kept,
                &mut |name, _| {
                    let tries = new_tries.entry(name.to_owned()).or_insert(0);
                    *tries += 1;
                    outcome(name, *tries).map(|ok| {
                        ok.map(|()| {
                            new_started.push(name.to_owned());
                            pid += 1;
                            pid
                        })
                    })
                },
            );
            assert_eq!(
                old_started, new_started,
                "the same fires, in the same order"
            );
            assert_eq!(old_fired, new_fired);
            assert_eq!(old_kept, new_kept);
            assert_eq!(old_fleet.busy, new_fleet.busy);
            assert_eq!(old_fleet.line, new_fleet.line);
            let state = |f: &Followed| -> State {
                (f.again, f.queued, f.next, f.looked, f.failures, f.retry)
            };
            for name in &names {
                let (a, b) = (&old[name], &new.followed[name]);
                assert_eq!(state(a), state(b), "{name}");
                assert_eq!((a.started, a.unkept), (b.started, b.unkept), "{name}");
            }
        }
        assert!(old_fleet.line > 300, "{} places in line", old_fleet.line);
    }

    #[test]
    fn a_fire_ending_looks_at_the_triggers_it_may_start_not_at_every_one() {
        // `triggers` due at once, a fleet of 64 at a time: the triggers
        // looked at by each wake a fire's end brings, and by them all.
        let looked = |triggers: usize| {
            let mut schedule = Schedule::default();
            let names: Vec<String> = (0..triggers).map(|i| format!("t{i:05}")).collect();
            let now = std::time::Instant::now();
            for name in &names {
                schedule
                    .followed
                    .insert(name.clone(), followed(true, None, 0));
                schedule.file(name, now);
            }
            let mut fleet = Fleet {
                running: HashMap::new(),
                busy: std::collections::HashSet::new(),
                cap: 64,
                line: 0,
            };
            let mut kept = Kept::default();
            let mut pid = 0;
            let mut spawn = |_: &str, _: &Job| {
                pid += 1;
                Ok::<_, String>(Some(pid))
            };
            let none = std::collections::HashSet::new();
            let mut asked = |_: &str| false;
            schedule.pass(
                &mut fleet, 0, now, false, false, &none, &names, &mut asked, &mut kept, &mut spawn,
            );
            let (mut most, mut all) = (0, 0);
            let mut at = now;
            while let Some(&pid) = fleet.running.keys().min() {
                let ended = std::collections::HashSet::from([fleet.ended(pid).unwrap()]);
                at += AGAIN_AFTER;
                VISITED.with(|visited| visited.set(0));
                schedule.pass(
                    &mut fleet,
                    0,
                    at,
                    false,
                    false,
                    &ended,
                    &[],
                    &mut asked,
                    &mut kept,
                    &mut spawn,
                );
                schedule.wake(&fleet, LOOK_EVERY, LOOK_EVERY);
                let wake = VISITED.with(|visited| visited.get());
                (most, all) = (most.max(wake), all + wake);
            }
            assert!(
                schedule
                    .followed
                    .values()
                    .all(|f| !f.again && f.queued.is_none())
            );
            assert_eq!(pid as usize, triggers, "every trigger fired once");
            (most, all)
        };
        let (most, all) = looked(10_000);
        assert!(most <= 4, "a wake looked at {most}");
        assert!(all <= 3 * 10_000, "{all} in all");
        assert_eq!(most, looked(200).0);
    }

    /// In a zone with daylight saving, as a child of this test binary, so
    /// its TZ is no other test's.
    #[test]
    fn the_hour_the_clock_repeats_comes_once_for_a_set_time_and_twice_for_every_minute() {
        if std::env::var_os("AGENT_TEST_DST").is_none() {
            let me = std::env::current_exe().unwrap();
            let out = std::process::Command::new(me)
                .args(["--exact", "scheduler::tests::the_hour_the_clock_repeats_comes_once_for_a_set_time_and_twice_for_every_minute", "--nocapture"])
                .env("AGENT_TEST_DST", "1")
                .env("TZ", "America/New_York")
                .output()
                .unwrap();
            assert!(
                out.status.success() && String::from_utf8_lossy(&out.stdout).contains("1 passed"),
                "{}{}",
                String::from_utf8_lossy(&out.stdout),
                String::from_utf8_lossy(&out.stderr)
            );
            return;
        }
        // 2026-11-01 01:00 EST, the second 01:00 of that night.
        let repeated = 1_793_512_800;
        assert_eq!(trigger::local(repeated).hour, 1);
        let every_minute = trigger::every("1m", repeated - 120).unwrap();
        assert_eq!(
            next_after(&every_minute.entries, repeated),
            Some(repeated + 60)
        );
        // Every minute as cron writes it, halfway through the first 01:00
        // hour: 01:31 that first time, not the second 01:00.
        let cron_every = trigger::cron("* * * * *").unwrap();
        let half = repeated - 3600 + 30 * 60;
        assert_eq!(next_after(&cron_every.entries, half), Some(half + 60));
        // And halfway through the second, 01:31 the second time.
        assert_eq!(
            next_after(&cron_every.entries, repeated + 30 * 60),
            Some(repeated + 31 * 60)
        );
        // A set time, 01:30, came once already, at its first.
        let set = trigger::cron("30 1 * * *").unwrap();
        assert_eq!(
            next_after(&set.entries, repeated - 3600 + 30 * 60),
            Some(repeated - 3600 + 30 * 60 + 86_400 + 3600)
        );
    }

    #[test]
    fn a_fire_starting_writes_its_own_kept_time_not_every_trigger_s() {
        let root =
            std::env::temp_dir().join(format!("agent-scheduler-kept-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        let places = Places::under(&root);
        // 10,000 triggers kept, written whole.
        let mut kept = Kept::default();
        for i in 0..10_000 {
            kept.insert(format!("t{i:05}"), ("g".into(), 60));
        }
        kept.write(&places);
        let whole = std::fs::metadata(places.looked()).unwrap().len() as usize;
        assert!(whole > 10_000 * 30);
        // The log is made with the whole, so the folder's sync that makes
        // the whole's name last makes the log's last too.
        assert_eq!(std::fs::metadata(places.looked_log()).unwrap().len(), 0);
        // Each start writes its own line: the bytes of 20,000 starts,
        // one a write, are those of the lines and of the whole written
        // again each time the log grew as long as what is kept.
        WRITTEN.with(|written| written.set(0));
        let mut most = 0;
        for start in 0..20_000 {
            let before = WRITTEN.with(|written| written.get());
            kept.insert(
                format!("t{:05}", start % 10_000),
                ("g".into(), 120 + start as i64),
            );
            kept.write(&places);
            let wrote = WRITTEN.with(|written| written.get()) - before;
            if wrote < whole {
                most = most.max(wrote);
            }
        }
        let all = WRITTEN.with(|written| written.get());
        assert!(most < 80, "a start wrote {most} bytes");
        assert!(all < 2 * 20_000 * 80 + 2 * whole + 4_096, "{all} in all");
        // Read again as the next scheduler reads it, a trigger gone and a
        // line cut short by a crash at the end of the log.
        kept.remove("t00007");
        kept.insert("t00008".into(), ("h".into(), 999));
        kept.write(&places);
        let mut log = std::fs::OpenOptions::new()
            .append(true)
            .open(places.looked_log())
            .unwrap();
        std::io::Write::write_all(&mut log, br#"{"name":"t00009","genera"#).unwrap();
        let read = Kept::read(&places);
        assert_eq!(read.times, kept.times);
        assert_eq!(read.get("t00008"), Some(&("h".to_owned(), 999)));
        assert_eq!(read.get("t09999"), Some(&("g".to_owned(), 120 + 19_999)));
        // The log was written into the whole: the next line starts clean.
        assert_eq!(std::fs::metadata(places.looked_log()).unwrap().len(), 0);
        assert_eq!(Kept::read(&places).times, kept.times);
        // A log gone is made again only with the whole.
        std::fs::remove_file(places.looked_log()).unwrap();
        kept.insert("t00010".into(), ("h".into(), 5));
        kept.write(&places);
        assert_eq!(std::fs::metadata(places.looked_log()).unwrap().len(), 0);
        assert_eq!(Kept::read(&places).times, kept.times);
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn a_crash_after_the_whole_is_written_and_before_the_log_starts_over_loses_nothing() {
        let root =
            std::env::temp_dir().join(format!("agent-scheduler-torn-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        let places = Places::under(&root);
        let mut kept = Kept::default();
        for name in ["a", "b", "c", "d", "e", "f", "g", "h", "i", "j"] {
            kept.insert(name.into(), ("g".into(), 60));
        }
        kept.write(&places);
        // Logged: a at 100, then a at 200 and b gone.
        kept.insert("a".into(), ("g".into(), 100));
        kept.write(&places);
        kept.insert("a".into(), ("g".into(), 200));
        kept.remove("b");
        kept.write(&places);
        assert!(std::fs::metadata(places.looked_log()).unwrap().len() > 0);
        // a at 300 and b back, not yet logged when the whole is written;
        // the crash leaves the log as it was.
        kept.insert("a".into(), ("g".into(), 300));
        kept.insert("b".into(), ("g".into(), 50));
        let log = std::fs::read(places.looked_log()).unwrap();
        kept.write_whole(&places);
        std::fs::write(places.looked_log(), &log).unwrap();
        let read = Kept::read(&places);
        assert_eq!(read.get("a"), Some(&("g".to_owned(), 300)));
        assert_eq!(read.get("b"), Some(&("g".to_owned(), 50)));
        assert_eq!(read.times, kept.times);
        // And what is logged after it is read over it.
        let mut again = read;
        again.insert("d".into(), ("g".into(), 400));
        again.write(&places);
        assert_eq!(Kept::read(&places).get("d"), Some(&("g".to_owned(), 400)));
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn a_log_write_that_fails_part_way_is_not_read_over_what_comes_after() {
        let root =
            std::env::temp_dir().join(format!("agent-scheduler-failed-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        let places = Places::under(&root);
        let mut kept = Kept::default();
        for name in ["a", "b", "c", "d", "e", "f", "g", "h", "i", "j"] {
            kept.insert(name.into(), ("g".into(), 60));
        }
        kept.write(&places);
        // The append fails: here the log cannot be opened, as when a
        // write or its sync fails after some of its lines went in.
        let next = kept.last + 1;
        std::fs::remove_file(places.looked_log()).unwrap();
        std::fs::create_dir_all(places.looked_log()).unwrap();
        kept.insert("a".into(), ("g".into(), 100));
        kept.insert("b".into(), ("g".into(), 100));
        kept.write(&places);
        assert_eq!(kept.changed.len(), 2);
        // What such a write may leave: a's line, b's cut short.
        std::fs::remove_dir(places.looked_log()).unwrap();
        let left = format!(
            "{{\"n\":{next},\"name\":\"a\",\"generation\":\"g\",\"looked\":100}}\n{{\"n\":{},\"na",
            next + 1
        );
        std::fs::write(places.looked_log(), left).unwrap();
        // The next write gives a newer time; read again, it holds.
        kept.insert("a".into(), ("g".into(), 200));
        kept.write(&places);
        let read = Kept::read(&places);
        assert_eq!(read.get("a"), Some(&("g".to_owned(), 200)));
        assert_eq!(read.get("b"), Some(&("g".to_owned(), 100)));
        assert_eq!(read.times, kept.times);
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn a_log_read_that_cannot_start_over_is_never_written_after() {
        let root =
            std::env::temp_dir().join(format!("agent-scheduler-unstarted-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        let places = Places::under(&root);
        let mut kept = Kept::default();
        for name in ["a", "b", "c", "d", "e", "f", "g", "h", "i", "j"] {
            kept.insert(name.into(), ("g".into(), 60));
        }
        kept.write(&places);
        // A crash cut the log's last line short.
        let next = kept.last + 1;
        std::fs::write(places.looked_log(), format!("{{\"n\":{next},\"na")).unwrap();
        // The whole cannot be written as it is read, as when the disk is
        // full.
        WHOLE_FAILS.with(|fails| fails.set(true));
        let mut read = Kept::read(&places);
        WHOLE_FAILS.with(|fails| fails.set(false));
        // Room again: the next change is written, then read back.
        read.insert("a".into(), ("g".into(), 200));
        read.write(&places);
        assert_eq!(Kept::read(&places).get("a"), Some(&("g".to_owned(), 200)));
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn a_calendar_time_is_kept_only_once_its_fire_starts() {
        let mut f = Followed {
            file: (0, 0, 0),
            job: Job {
                program: vec!["/A/sonny".into()],
                environment: Vec::new(),
                calendar: trigger::every("1m", 0).unwrap().entries,
                watch: None,
            },
            generation: "g".into(),
            looked: 0,
            next: Some(60),
            seen: None,
            again: false,
            started: None,
            failures: 0,
            retry: None,
            unkept: None,
            queued: None,
        };
        let mut kept = Kept::default();
        let mut fleet = Fleet {
            running: HashMap::new(),
            busy: std::collections::HashSet::new(),
            cap: 0,
            line: 0,
        };
        let at = std::time::Instant::now();
        // Due with the fleet full: not kept, so a restart finds it due.
        assert!(came("t", &mut f, false, 90, &mut kept));
        launch("t", &mut f, &mut fleet, at, &mut |_, _| Ok(Some(1)));
        assert!(!started("t", &mut f, at, &mut kept));
        assert!(kept.is_empty() && f.again);
        // A start that fails: still not kept.
        fleet.cap = 8;
        launch("t", &mut f, &mut fleet, at, &mut |_, _| Err("gone".into()));
        assert!(!started("t", &mut f, at, &mut kept));
        assert!(kept.is_empty());
        // It starts: kept at the time that came.
        let later = at + LOOK_EVERY * 5;
        launch("t", &mut f, &mut fleet, later, &mut |_, _| Ok(Some(2)));
        assert!(started("t", &mut f, later, &mut kept));
        assert_eq!(kept["t"], ("g".to_owned(), 90));
        // One that comes while its fire runs is skipped, and kept at once.
        f.next = Some(120);
        assert!(came("t", &mut f, true, 130, &mut kept));
        assert_eq!(kept["t"].1, 130);
    }

    #[test]
    fn a_write_between_adding_and_the_first_read_fires_once() {
        let root =
            std::env::temp_dir().join(format!("agent-scheduler-watch-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        let places = Places::under(&root);
        std::fs::create_dir_all(&places.jobs).unwrap();
        let watched = root.join("notes.md");
        std::fs::write(&watched, "before\n").unwrap();
        let program: Vec<String> = [
            "/A/sonny",
            "--trigger-fire",
            "--name",
            "w",
            "--bot",
            "w",
            "--bot-id",
            "1",
            "--generation",
            "g1",
            "--when",
            "file",
            "--store",
            "/s",
            "--store-id",
            "x",
            "--",
            "hi",
        ]
        .map(String::from)
        .to_vec();
        let text = serde_json::json!({"program": program, "watch": watched}).to_string();
        let define = || std::fs::write(places.job("w"), &text).unwrap();
        let (mut followed, mut kept) = (HashMap::new(), Kept::default());
        // Written before the trigger was added: nothing to fire for.
        std::thread::sleep(Duration::from_millis(20));
        define();
        assert!(read_jobs(
            &places,
            &mut followed,
            &mut kept,
            &mut Vec::new()
        ));
        assert!(!followed["w"].again);
        assert_eq!(kept["w"].0, "g1");
        // Written after it was added, before the scheduler first read it.
        followed.clear();
        kept.clear();
        std::thread::sleep(Duration::from_millis(20));
        std::fs::write(&watched, "after\n").unwrap();
        assert!(read_jobs(
            &places,
            &mut followed,
            &mut kept,
            &mut Vec::new()
        ));
        assert!(followed["w"].again);
        // One it kept already, as after the scheduler restarts: no fire.
        followed.clear();
        assert!(!read_jobs(
            &places,
            &mut followed,
            &mut kept,
            &mut Vec::new()
        ));
        assert!(!followed["w"].again);
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn the_job_under_its_old_label_is_made_again_under_one_that_names_no_one() {
        let root = std::env::temp_dir().join(format!("relabel-{}", std::process::id()));
        let home = root.join("home");
        let scheduler = Scheduler::Launchd;
        let dir = scheduler.jobs(&home);
        std::fs::create_dir_all(&dir).unwrap();
        let tag = trigger::home_tag(home.as_os_str());
        let old = dir.join(format!("me.lydakis.agent.scheduler.{tag}.plist"));
        std::fs::write(&old, "old").unwrap();
        assert_eq!(label(&home), format!("sonny.scheduler.{tag}"));
        assert!(!label(&home).contains("lydakis"));
        // The new job could not be made: the old one is gone and it is
        // tried again at the next start.
        let mut made = 0;
        assert!(!relabel(&home, scheduler, || {
            made += 1;
            Err("no".into())
        }));
        assert!(!old.exists());
        std::fs::write(&old, "old").unwrap();
        assert!(relabel(&home, scheduler, || {
            made += 1;
            Ok(())
        }));
        assert!(!old.exists());
        // With none under the old label, nothing to make.
        assert!(relabel(&home, scheduler, || {
            made += 1;
            Ok(())
        }));
        assert_eq!(made, 2);
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn the_job_is_one_and_runs_the_scheduler() {
        let sonny = Path::new("/opt/Sonny Dir/sonny");
        let environment = [("SHELL", "/bin/zsh".to_owned())];
        let plist = definition(
            Scheduler::Launchd,
            "sonny.scheduler.0a",
            sonny,
            &environment,
        );
        assert!(
            plist.contains("<string>/opt/Sonny Dir/sonny</string>\n    <string>scheduler</string>")
        );
        assert!(plist.contains("<key>RunAtLoad</key>\n  <true/>"));
        assert!(plist.contains("<key>SuccessfulExit</key>\n    <false/>"));
        assert!(plist.contains("<key>AbandonProcessGroup</key>\n  <true/>"));
        assert!(plist.contains("<key>SHELL</key><string>/bin/zsh</string>"));
        assert!(plist.contains(&program(Scheduler::Launchd, sonny)));
        let unit = definition(Scheduler::Systemd, "x", sonny, &environment);
        assert!(unit.contains("ExecStart=\"/opt/Sonny Dir/sonny\" scheduler\n"));
        assert!(unit.contains("KillMode=process"));
        assert!(unit.contains("Environment=\"SHELL=/bin/zsh\""));
        assert!(unit.contains(&program(Scheduler::Systemd, sonny)));
    }

    #[test]
    fn systemctl_is_a_system_one_never_one_on_path() {
        assert!(SYSTEMCTL.iter().all(|path| path.starts_with('/')));
        assert!(SYSTEMCTL.contains(&systemctl()));
    }

    #[test]
    fn a_scheduler_that_does_not_run_is_said_in_the_system_s_own_words() {
        // Running: nothing to say.
        let running =
            "LoadState=loaded\nActiveState=active\nSubState=running\nResult=success\nNRestarts=0\n";
        assert_eq!(systemd_state(running), None);
        // 11-17: a drop-in named an environment file that had moved.
        let failed = "LoadState=loaded\nActiveState=failed\nSubState=failed\nResult=resources\nNRestarts=5\n";
        assert_eq!(
            systemd_state(failed).as_deref(),
            Some("failed (Result: resources) · restart counter is at 5")
        );
        let waiting = "LoadState=loaded\nActiveState=activating\nSubState=auto-restart\nResult=exit-code\nNRestarts=2\n";
        assert_eq!(
            systemd_state(waiting).as_deref(),
            Some("activating (auto-restart) (Result: exit-code) · restart counter is at 2")
        );
        let gone = "LoadState=not-found\nActiveState=inactive\nSubState=dead\nResult=success\nNRestarts=0\n";
        assert_eq!(
            systemd_state(gone).as_deref(),
            Some("not-found · inactive (dead)")
        );
        let print = "gui/501/sonny.scheduler.0a = {\n\tactive count = 0\n\tstate = running\n\tlast exit code = (never exited)\n}\n";
        assert_eq!(launchd_state(print), None);
        let print = "gui/501/sonny.scheduler.0a = {\n\tactive count = 0\n\tstate = not running\n\truns = 9\n\tlast exit code = 78: EX_CONFIG\n\tevent triggers = {\n\t\tstate = 1\n\t}\n}\n";
        assert_eq!(
            launchd_state(print).as_deref(),
            Some("state = not running · last exit code = 78: EX_CONFIG")
        );
        assert_eq!(
            last_line(
                b"Bad request.\nCould not find service \"sonny.scheduler.0a\" in domain for user gui: 501\n",
                b""
            ),
            "Could not find service \"sonny.scheduler.0a\" in domain for user gui: 501"
        );
    }
}
