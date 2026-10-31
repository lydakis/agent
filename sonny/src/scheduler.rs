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
//! their definitions from `~/.agent/triggers/jobs`, and runs each one's
//! fire as launchd ran it, when one of its calendar times comes (once for
//! any number missed while the machine slept or the scheduler was not
//! running), when the path it watches changes, and while its queue of
//! asks holds one. One fire of a trigger at a time, also across a restart
//! of the scheduler: a fire holds a lock on its own file in
//! `~/.agent/triggers/.scheduler` while it runs.
//!
//! A write to its doorbell (`~/.agent/triggers/.ring`, a FIFO) has it look
//! again at once: Sonny rings it when a definition or a queue changes, and
//! a fire when it ends. Else it looks at least every minute, and at a
//! watched path every two seconds. It sleeps until the next calendar time,
//! and takes no process between fires. When Sonny is upgraded it runs the
//! new Sonny in place of itself; when its HOME is gone, as a test's is, it
//! lets its own job go.
use crate::trigger::{self, Entry, Job, Places, Scheduler};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::time::Duration;

/// `sonny scheduler`, what the job runs.
pub const VERB: &str = "scheduler";
/// The job's label, and its systemd unit's name, before its HOME's tag.
const LABEL: &str = "me.lydakis.agent.scheduler.";
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
                "# Sonny's scheduler, which fires its triggers.\n[Unit]\nDescription=Sonny's scheduler\n\n[Service]\n{} {VERB}\n{environment}Restart=on-failure\nRestartSec=10\n# A fire it started runs to its end.\nKillMode=process\n\n[Install]\nWantedBy=default.target\n",
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
    let home = PathBuf::from(std::env::var_os("HOME").ok_or("no HOME for triggers")?);
    let scheduler = Scheduler::here();
    if scheduler == Scheduler::Systemd {
        UserManager::find()?;
    }
    let sonny = crate::skills::sonny_path().ok_or("no path to this Sonny")?;
    let (file, label) = (job_file(scheduler, &home), label(&home));
    let there = std::fs::read_to_string(&file).ok();
    if there
        .as_deref()
        .is_some_and(|text| text.contains(&program(scheduler, &sonny)))
    {
        return start(scheduler, &file, &label);
    }
    let text = definition(scheduler, &label, &sonny, &trigger::environment());
    if let Some(dir) = file.parent() {
        std::fs::create_dir_all(dir).map_err(|e| format!("{}: {e}", dir.display()))?;
    }
    if there.is_some() {
        let_go(scheduler, file.parent().unwrap_or(Path::new("")), &label);
    }
    trigger::replace_mode(&file, text.as_bytes(), 0o644)?;
    start(scheduler, &file, &label)
}

/// Whether the job is there and runs another Sonny than this one.
fn stale() -> bool {
    let (Some(home), Some(sonny)) = (std::env::var_os("HOME"), crate::skills::sonny_path()) else {
        return false;
    };
    let scheduler = Scheduler::here();
    std::fs::read_to_string(job_file(scheduler, Path::new(&home)))
        .is_ok_and(|text| !text.contains(&program(scheduler, &sonny)))
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
                "triggers_unsupported: triggers on Linux use systemd's user manager, which is not running here ({} has no systemd/private)",
                runtime.display()
            )),
        }
    }
    pub(crate) fn run(&self, args: &[&str]) -> Result<(), String> {
        self.run_allowing(args, &[])
    }
    /// `systemctl --user ARGS`, also fine when it exits with one of `fine`.
    pub(crate) fn run_allowing(&self, args: &[&str], fine: &[i32]) -> Result<(), String> {
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

/// The logged-in user's launchd domain.
fn launchd_domain() -> String {
    // SAFETY: getuid takes no arguments and cannot fail.
    format!("gui/{}", unsafe { libc::getuid() })
}

/// `launchctl ARGS`.
fn launchctl(args: &[&str]) -> Result<(), String> {
    if !cfg!(target_os = "macos") {
        return Err("triggers_unsupported: launchd is macOS's".into());
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
            let _ = manager.run_allowing(
                &["disable", "--now", &units[0], &units[1], &units[2]],
                &[1, 5],
            );
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

/// A job of its own an earlier Sonny kept, which the scheduler runs now:
/// let go of, and its file removed.
pub(crate) fn retire_old(label: &str) {
    let Some(home) = std::env::var_os("HOME").map(PathBuf::from) else {
        return;
    };
    let scheduler = Scheduler::here();
    let dir = scheduler.jobs(&home);
    let file = dir.join(format!("{label}{}", scheduler.suffix()));
    if file.exists() {
        let_go(scheduler, &dir, label);
        let _ = std::fs::remove_file(&file);
    }
}

/// At Sonny's start: the job runs this Sonny, when it ran one from
/// elsewhere (an install that keeps each version in its own folder), even
/// while the old one still answers its doorbell; and the scheduler
/// running, when any trigger is there.
pub fn at_start() {
    let Ok(places) = Places::home() else {
        return;
    };
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
}

/// The fires the scheduler started and runs still, by process and by
/// trigger, and how many may run at once.
struct Fleet {
    running: HashMap<libc::pid_t, String>,
    busy: std::collections::HashSet<String>,
    cap: usize,
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
fn came(
    name: &str,
    f: &mut Followed,
    busy: bool,
    now: i64,
    kept: &mut HashMap<String, (String, i64)>,
) -> bool {
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
fn started(
    name: &str,
    f: &mut Followed,
    at: std::time::Instant,
    kept: &mut HashMap<String, (String, i64)>,
) -> bool {
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

/// Start a due trigger's fire now if it may start: else it stays due
/// (`again`), for when its fire ends, its last start is a moment old, a
/// failed start's wait is over, or the fleet has room.
fn launch(
    name: &str,
    f: &mut Followed,
    fleet: &mut Fleet,
    now: std::time::Instant,
    spawn: &mut Spawn,
) {
    let soon = f
        .started
        .is_some_and(|at| now.duration_since(at) < AGAIN_AFTER);
    let waiting = f.retry.is_some_and(|at| at > now);
    if fleet.busy.contains(name) || soon || waiting || fleet.full() {
        f.again = true;
        return;
    }
    match spawn(name, &f.job) {
        Ok(Some(pid)) => {
            fleet.running.insert(pid, name.to_owned());
            fleet.busy.insert(name.to_owned());
            f.started = Some(now);
            (f.failures, f.retry) = (0, None);
        }
        // A fire of it the scheduler did not start runs still, as one from
        // before a restart: its end wakes no one, so it is looked at
        // again after a wait that doubles, up to a minute.
        Ok(None) => back_off(f, now, LOOK_EVERY),
        // Tried again after a wait that doubles, up to five minutes.
        Err(error) => {
            eprintln!("{}", trigger::error_json(&error));
            back_off(f, now, LOOK_EVERY * 5);
        }
    }
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
        eprintln!("{}", trigger::error_json("no HOME for triggers"));
        return 1;
    };
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
    let mut followed: HashMap<String, Followed> = HashMap::new();
    let mut kept = read_looked(&places);
    let mut fleet = Fleet::new();
    let mut folder = None;
    let mut last_look = std::time::Instant::now() - LOOK_EVERY;
    let mut last_watch = std::time::Instant::now() - WATCH_EVERY;
    let mut rang = true;
    loop {
        if orphaned() {
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
            return 0;
        }
        let every_minute = last_look.elapsed() >= LOOK_EVERY;
        // Sonny upgraded where it stays: the new one schedules from here.
        if every_minute
            && let Some(me) = &me
            && identity(me).is_some_and(|now| Some(now) != born)
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
            syncing = run_own(me, &["home", "sync", "--job"]);
        }
        let now_folder = modified(&places.jobs);
        let mut fired = false;
        if rang || every_minute || now_folder != folder {
            folder = now_folder;
            fired = read_jobs(&places, &mut followed, &mut kept);
        }
        let now = trigger::now();
        let instant = std::time::Instant::now();
        // Watched paths every two seconds, however often it wakes.
        let watch = instant.duration_since(last_watch) >= WATCH_EVERY;
        if watch {
            last_watch = instant;
        }
        for (name, f) in followed.iter_mut() {
            let busy = fleet.busy.contains(name);
            let mut due = std::mem::take(&mut f.again);
            if came(name, f, busy, now, &mut kept) {
                fired |= busy;
                due |= !busy;
            }
            if let Some(path) = f.job.watch.as_ref().filter(|_| watch) {
                let seen = look(path);
                if seen != f.seen {
                    f.seen = seen;
                    due = true;
                }
            }
            if (rang || every_minute || ended.contains(name)) && asked(&places, name) {
                due = true;
            }
            if due {
                launch(name, f, &mut fleet, instant, &mut |name, job| {
                    spawn(&places, name, job)
                });
                fired |= started(name, f, instant, &mut kept);
            }
        }
        if fired {
            kept.retain(|name, _| followed.contains_key(name));
            write_looked(&places, &kept);
        }
        if every_minute {
            last_look = std::time::Instant::now();
        }
        // Asleep until the next time, a watched path's next look, a fire
        // run again, or a ring.
        let mut wake = Duration::from_secs((60 - now.rem_euclid(60)) as u64).min(LOOK_EVERY);
        let now_exact = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default();
        for (name, f) in &followed {
            if let Some(next) = f.next {
                let at = Duration::from_secs(next.max(0) as u64);
                wake = wake.min(at.saturating_sub(now_exact));
            }
            if f.job.watch.is_some() {
                wake = wake.min(WATCH_EVERY.saturating_sub(last_watch.elapsed()));
            }
            // One whose fire runs, or that waits for room, is woken by a
            // fire's end.
            if f.again && !fleet.full() && !fleet.busy.contains(name) {
                let at = [f.started.map(|at| at + AGAIN_AFTER), f.retry]
                    .into_iter()
                    .flatten()
                    .max();
                wake = wake.min(at.map_or(Duration::ZERO, |at| {
                    at.saturating_duration_since(std::time::Instant::now())
                }));
            }
        }
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
    kept: &mut HashMap<String, (String, i64)>,
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
        let read = std::fs::read_to_string(entry.path())
            .ok()
            .and_then(|text| Job::read(&text))
            .and_then(|job| Some((trigger::trigger_of(&job.program)?.0.generation, job)));
        let Some((generation, job)) = read else {
            followed.remove(name);
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
            },
        );
    }
    followed.retain(|name, _| there.contains(name));
    new
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

/// A lock on `path`, a plain file made when it is not there: None while
/// another process holds it.
fn locked(path: &Path) -> Result<Option<std::fs::File>, String> {
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

/// Whether HOME is gone, as a test's or an eval's is.
fn orphaned() -> bool {
    std::env::var_os("HOME").is_some_and(|home| {
        matches!(std::fs::symlink_metadata(home), Err(e) if e.kind() == std::io::ErrorKind::NotFound)
    })
}

/// Up to when each trigger's calendar was looked at when it last came, by
/// its generation, kept across restarts of the scheduler: a time missed
/// while none ran comes once when one runs.
fn read_looked(places: &Places) -> HashMap<String, (String, i64)> {
    std::fs::read(places.looked())
        .ok()
        .and_then(|bytes| serde_json::from_slice::<serde_json::Value>(&bytes).ok())
        .and_then(|value| {
            value.as_object().map(|map| {
                map.iter()
                    .filter_map(|(name, v)| {
                        Some((
                            name.clone(),
                            (v["generation"].as_str()?.to_owned(), v["looked"].as_i64()?),
                        ))
                    })
                    .collect()
            })
        })
        .unwrap_or_default()
}

fn write_looked(places: &Places, kept: &HashMap<String, (String, i64)>) {
    let value: serde_json::Map<String, serde_json::Value> = kept
        .iter()
        .map(|(name, (generation, looked))| {
            (
                name.clone(),
                serde_json::json!({"generation": generation, "looked": looked}),
            )
        })
        .collect();
    let written = trigger::replace_mode(
        &places.looked(),
        serde_json::Value::Object(value).to_string().as_bytes(),
        0o600,
    );
    if let Err(error) = written {
        eprintln!("{}", trigger::error_json(&error));
    }
}

/// The doorbell's own writer, for the SIGCHLD handler.
static BELL: std::sync::atomic::AtomicI32 = std::sync::atomic::AtomicI32::new(-1);

extern "C" fn rung(_: libc::c_int) {
    let fd = BELL.load(std::sync::atomic::Ordering::Relaxed);
    if fd >= 0 {
        // SAFETY: write is async-signal-safe; a full pipe drops the byte.
        unsafe { libc::write(fd, [1u8].as_ptr().cast(), 1) };
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

    /// Wait up to `wait` for a ring; whether one came.
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
        let mut any = false;
        while let Ok(n @ 1..) = (&self.read).read(&mut buffer) {
            any = true;
            if n < buffer.len() {
                break;
            }
        }
        any
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
                };
                (format!("t{i:05}"), f)
            })
            .collect();
        let mut fleet = Fleet {
            running: HashMap::new(),
            busy: std::collections::HashSet::new(),
            cap: 64,
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
        };
        let mut kept = HashMap::new();
        let mut fleet = Fleet {
            running: HashMap::new(),
            busy: std::collections::HashSet::new(),
            cap: 0,
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
        let (mut followed, mut kept) = (HashMap::new(), HashMap::new());
        // Written before the trigger was added: nothing to fire for.
        std::thread::sleep(Duration::from_millis(20));
        define();
        assert!(read_jobs(&places, &mut followed, &mut kept));
        assert!(!followed["w"].again);
        assert_eq!(kept["w"].0, "g1");
        // Written after it was added, before the scheduler first read it.
        followed.clear();
        kept.clear();
        std::thread::sleep(Duration::from_millis(20));
        std::fs::write(&watched, "after\n").unwrap();
        assert!(read_jobs(&places, &mut followed, &mut kept));
        assert!(followed["w"].again);
        // One it kept already, as after the scheduler restarts: no fire.
        followed.clear();
        assert!(!read_jobs(&places, &mut followed, &mut kept));
        assert!(!followed["w"].again);
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn the_job_is_one_and_runs_the_scheduler() {
        let sonny = Path::new("/opt/Sonny Dir/sonny");
        let environment = [("SHELL", "/bin/zsh".to_owned())];
        let plist = definition(
            Scheduler::Launchd,
            "me.lydakis.agent.scheduler.0a",
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
}
