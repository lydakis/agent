//! Quiet: outside the person's working hours, once they have stepped
//! away, work goes on and nobody writes to them or rings. Off unless the
//! person sets `[sonny] hours`, or writes `/quiet`.
//!
//! Two files under `~/.agent/sonny/` hold it, so every window, the lookout
//! and the scheduler read one answer without asking each other:
//!
//! - `present`: its modified time is when the person last used any Sonny
//!   window, a key, click, paste or focus, marked with the per-agent mark
//!   in [`crate::used`] and as often (at most once every 10 seconds a
//!   window).
//! - `away`: there while quiet is on. It says since when and why. The
//!   lookout, which always runs, starts it on its own; a window starts it
//!   for `/quiet`; the window the person writes words from ends it, so
//!   quiet ends for every agent at once.
//!
//! A glance, a scroll or a focus keeps quiet from starting but does not end
//! it: opening the laptop at night should not wake every lead. The clock
//! never ends it either, so the morning brings no flood before the person
//! writes. Leads learn it from the line Sonny ends each message to them
//! with ([`crate::stamp`]): a fact, as the time is. Tasks never hear it:
//! they work and report to their lead as ever.
use agent_client::config::{DAYS, Hours};
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

const WEEK: i64 = 7 * DAY;
const DAY: i64 = 24 * 60;

/// When quiet starts on its own: the person's hours and how long no
/// window is used outside them first. No hours, no clock.
#[derive(Clone, Debug, PartialEq)]
pub struct Schedule {
    pub hours: Vec<Hours>,
    pub quiet_after: u64,
}

impl Default for Schedule {
    fn default() -> Self {
        Schedule::from(&agent_client::config::Sonny::default())
    }
}

impl Schedule {
    pub fn from(sonny: &agent_client::config::Sonny) -> Self {
        Schedule {
            hours: sonny.hours.clone(),
            quiet_after: sonny.quiet_after,
        }
    }

    /// The hours as the config writes them, joined: `Mon-Fri 09:00-19:00`.
    pub fn said(&self) -> String {
        let ranges: Vec<String> = self.hours.iter().map(Hours::to_string).collect();
        ranges.join(", ")
    }

    /// The minutes of the week, Sunday 00:00 first, each range covers:
    /// `[start, end)`, an end past the week going round.
    fn spans(&self) -> impl Iterator<Item = (i64, i64)> + '_ {
        self.hours.iter().flat_map(|hours| {
            (0..7).filter(|&day| hours.days[day]).map(move |day| {
                let start = day as i64 * DAY + i64::from(hours.start);
                let mut end = day as i64 * DAY + i64::from(hours.end);
                if end <= start {
                    end += DAY;
                }
                (start, end)
            })
        })
    }

    /// Whether the minute of the week `at` is inside the hours.
    fn inside(&self, at: i64) -> bool {
        self.spans()
            .any(|(start, end)| (start..end).contains(&at) || (start..end).contains(&(at + WEEK)))
    }

    /// When the stretch outside the hours that `epoch` is in began, in
    /// seconds; none inside them, or with no hours. Read in local time as
    /// it is at `epoch`, so a change of clocks in between is an hour off.
    pub fn off_since(&self, epoch: i64) -> Option<i64> {
        if self.hours.is_empty() {
            return None;
        }
        let tm = crate::stamp::local(epoch);
        let at = i64::from(tm.tm_wday) * DAY + i64::from(tm.tm_hour) * 60 + i64::from(tm.tm_min);
        if self.inside(at) {
            return None;
        }
        // The nearest end behind: any range that started after it and is
        // over now ended nearer.
        let back = (self.spans())
            .map(|(_, end)| (at - end).rem_euclid(WEEK))
            .min()?;
        Some(epoch - i64::from(tm.tm_sec) - back * 60)
    }

    /// When quiet would start on its own, given the person last used a
    /// window at `present` (seconds; none if never): once they are outside
    /// their hours and have been gone `quiet_after`. None while inside.
    pub fn starts(&self, present: Option<i64>, epoch: i64) -> Option<i64> {
        let off = self.off_since(epoch)?;
        let idle = present.map_or(i64::MIN, |at| at.saturating_add(self.quiet_after as i64));
        Some(off.max(idle))
    }
}

/// The schedule as this process last read the config, for the stamp to
/// name the hours without reading the file for each message.
static SCHEDULE: std::sync::Mutex<Option<Schedule>> = std::sync::Mutex::new(None);

/// Keep `schedule` as the config now says it: whoever reads the config
/// (a window, the lookout) passes it on.
pub fn set_schedule(schedule: Schedule) {
    if let Ok(mut kept) = SCHEDULE.lock() {
        *kept = Some(schedule);
    }
}

/// The schedule last kept, or the config's, read once.
pub fn schedule() -> Schedule {
    if let Ok(kept) = SCHEDULE.lock()
        && let Some(schedule) = kept.as_ref()
    {
        return schedule.clone();
    }
    let read = agent_client::config::load()
        .map(|config| Schedule::from(&config.sonny))
        .unwrap_or_default();
    set_schedule(read.clone());
    read
}

/// Why quiet is on.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Why {
    /// Outside the hours, and nobody used a window for `quiet_after`.
    Hours,
    /// The person wrote `/quiet`.
    Asked,
}

/// Quiet, as its file says.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Away {
    /// When it started, in seconds.
    pub since: i64,
    pub why: Why,
}

fn folder() -> Option<PathBuf> {
    let home = std::env::var_os("HOME").filter(|home| !home.is_empty())?;
    Some(PathBuf::from(home).join(".agent/sonny"))
}

fn present_file() -> Option<PathBuf> {
    Some(folder()?.join("present"))
}

fn away_file() -> Option<PathBuf> {
    Some(folder()?.join("away"))
}

fn now() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_secs() as i64)
}

/// The person used a window just now. Called where the window marks its
/// own agent used, as often.
pub fn mark_present() {
    if let Some(file) = present_file() {
        touch(&file);
    }
}

fn touch(file: &Path) {
    if let Some(dir) = file.parent() {
        let _ = std::fs::create_dir_all(dir);
    }
    if let Ok(opened) = std::fs::File::options()
        .create(true)
        .truncate(false)
        .write(true)
        .open(file)
    {
        let _ = opened.set_modified(SystemTime::now());
    }
}

/// When `file` was last changed, in seconds: for `present`, when the
/// person last used any window.
fn modified(file: &Path) -> Option<i64> {
    let modified = std::fs::metadata(file).ok()?.modified().ok()?;
    Some(modified.duration_since(UNIX_EPOCH).ok()?.as_secs() as i64)
}

/// Quiet as its file says now; none while it is off.
pub fn away() -> Option<Away> {
    // Kept until the file changes: a window asks once a minute and the
    // sound asks on each alert, and an idle window opens no files (A17).
    static KEPT: crate::kept::Slot<Option<Away>> = std::sync::Mutex::new(None);
    let file = away_file()?;
    crate::kept::read(&KEPT, "away", vec![file.clone()], || (away_at(&file), None))
}

fn away_at(file: &Path) -> Option<Away> {
    read(&std::fs::read_to_string(file).ok()?)
}

fn read(text: &str) -> Option<Away> {
    let mut lines = text.lines();
    let since = lines.next()?.trim().parse().ok()?;
    let why = match lines.next().map(str::trim) {
        Some("asked") => Why::Asked,
        _ => Why::Hours,
    };
    Some(Away { since, why })
}

fn write(away: Away) -> String {
    let why = match away.why {
        Why::Hours => "hours",
        Why::Asked => "asked",
    };
    format!("{}\n{why}\n", away.since)
}

/// Start quiet, unless it is on: the file appears whole, and only where
/// none is, so the lookout and a window starting it at once agree on one
/// start and no reader sees it half written.
pub fn start(away: Away) -> Result<(), String> {
    start_at(&away_file().ok_or("HOME is not set")?, away)
}

fn start_at(file: &Path, away: Away) -> Result<(), String> {
    let failed = |e: std::io::Error| format!("{}: {e}", file.display());
    if let Some(dir) = file.parent() {
        std::fs::create_dir_all(dir).map_err(failed)?;
    }
    let draft = file.with_extension(format!("{}.part", std::process::id()));
    std::fs::write(&draft, write(away)).map_err(failed)?;
    let linked = std::fs::hard_link(&draft, file);
    let _ = std::fs::remove_file(&draft);
    match linked {
        Err(e) if e.kind() != std::io::ErrorKind::AlreadyExists => Err(failed(e)),
        _ => Ok(()),
    }
}

/// End quiet, for every agent: what it was, none if it was off.
pub fn end() -> Option<Away> {
    end_at(&away_file()?)
}

fn end_at(file: &Path) -> Option<Away> {
    let away = away_at(file);
    std::fs::remove_file(file).ok()?;
    away
}

/// Start quiet if the schedule says it is time: the lookout asks this once
/// a minute. What it started, if it did.
pub fn arm(schedule: &Schedule) -> Option<Away> {
    arm_at(schedule, &present_file()?, &away_file()?, now())
}

fn arm_at(schedule: &Schedule, present: &Path, file: &Path, now: i64) -> Option<Away> {
    let since = schedule.starts(modified(present), now)?;
    if since > now || file.exists() {
        return None;
    }
    let away = Away {
        since,
        why: Why::Hours,
    };
    start_at(file, away).ok()?;
    Some(away)
}

/// `Fri 6 Nov 19:40`: when quiet started, as the stamp says the time.
pub fn when(epoch: i64) -> String {
    let tm = crate::stamp::local(epoch);
    format!(
        "{} {} {} {:02}:{:02}",
        DAYS[tm.tm_wday.rem_euclid(7) as usize],
        tm.tm_mday,
        crate::stamp::MONTHS[tm.tm_mon.rem_euclid(12) as usize],
        tm.tm_hour,
        tm.tm_min
    )
}

/// What the stamp says while quiet is on: a fact, never advice.
pub fn away_clause(away: Away, schedule: &Schedule) -> String {
    let since = when(away.since);
    match schedule.hours.is_empty() {
        true => format!("The person is away since {since}."),
        false => format!(
            "The person is away since {since}; their hours are {}.",
            schedule.said()
        ),
    }
}

/// What the stamp on the person's first message back says.
pub fn back_clause(away: Away) -> String {
    format!(
        "The person is back; they were away since {}.",
        when(away.since)
    )
}

/// Whether `text` is a clause [`away_clause`] or [`back_clause`] wrote, so
/// the stamp's reader takes it off with the time.
pub fn is_clause(text: &str) -> bool {
    (text.starts_with("The person is away since ")
        || text.starts_with("The person is back; they were away since "))
        && text.ends_with('.')
        && !text.contains('\n')
}

/// The footer's words for quiet: on since when, or waiting for the person
/// to step away outside their hours; none otherwise.
pub fn footer(away: Option<Away>, schedule: &Schedule, epoch: i64) -> Option<(String, bool)> {
    match away {
        Some(away) => {
            let tm = crate::stamp::local(away.since);
            Some((
                format!("quiet since {:02}:{:02}", tm.tm_hour, tm.tm_min),
                true,
            ))
        }
        None => schedule
            .off_since(epoch)
            .map(|_| ("quiet when you step away".to_owned(), false)),
    }
}

/// How often a window reads the file again: quiet starts on a minute's
/// check, and the window it ends in knows at once.
pub const READ_EVERY: Duration = Duration::from_secs(60);

#[cfg(test)]
mod tests {
    use super::*;

    /// Seconds for a local time, so tests read the hours as the machine
    /// keeps them.
    fn local(year: i32, month: i32, day: i32, hour: i32, minute: i32) -> i64 {
        // SAFETY: mktime reads the struct it is given and fills its own fields.
        unsafe {
            let mut tm: libc::tm = std::mem::zeroed();
            tm.tm_year = year - 1900;
            tm.tm_mon = month - 1;
            tm.tm_mday = day;
            tm.tm_hour = hour;
            tm.tm_min = minute;
            tm.tm_isdst = -1;
            libc::mktime(&mut tm) as i64
        }
    }

    fn week() -> Schedule {
        Schedule {
            hours: vec![agent_client::config::hours("Mon-Fri 09:00-19:00").unwrap()],
            quiet_after: 30 * 60,
        }
    }

    #[test]
    fn no_hours_is_no_clock() {
        let none = Schedule::default();
        assert!(none.hours.is_empty());
        let saturday = local(2026, 11, 7, 23, 0);
        assert_eq!(none.off_since(saturday), None);
        assert_eq!(none.starts(None, saturday), None);
    }

    #[test]
    fn outside_the_hours_starts_where_they_ended() {
        let week = week();
        // Friday 6 November 2026, 20:15: off since 19:00 that day.
        assert_eq!(
            week.off_since(local(2026, 11, 6, 20, 15)),
            Some(local(2026, 11, 6, 19, 0))
        );
        // Sunday morning: still since Friday 19:00.
        assert_eq!(
            week.off_since(local(2026, 11, 8, 10, 0)),
            Some(local(2026, 11, 6, 19, 0))
        );
        // Tuesday 08:59: since Monday 19:00. Tuesday 09:00: inside.
        assert_eq!(
            week.off_since(local(2026, 11, 10, 8, 59)),
            Some(local(2026, 11, 9, 19, 0))
        );
        assert_eq!(week.off_since(local(2026, 11, 10, 9, 0)), None);
        assert_eq!(week.off_since(local(2026, 11, 10, 18, 59)), None);
    }

    #[test]
    fn hours_past_midnight_and_round_the_week() {
        let late = Schedule {
            hours: vec![agent_client::config::hours("Sat-Sun 22:00-02:00").unwrap()],
            quiet_after: 60,
        };
        // Sunday 01:00 is Saturday's range; Sunday 03:00 is off since 02:00.
        assert_eq!(late.off_since(local(2026, 11, 8, 1, 0)), None);
        assert_eq!(
            late.off_since(local(2026, 11, 8, 3, 0)),
            Some(local(2026, 11, 8, 2, 0))
        );
        // Monday 00:30 is Sunday's range, past the week's end.
        assert_eq!(late.off_since(local(2026, 11, 9, 0, 30)), None);
    }

    #[test]
    fn quiet_waits_while_the_person_is_there_and_starts_once_they_step_away() {
        let week = week();
        let friday = |hour, minute| local(2026, 11, 6, hour, minute);
        // Left at 18:00: quiet starts at 19:00.
        assert_eq!(
            week.starts(Some(friday(18, 0)), friday(19, 5)),
            Some(friday(19, 0))
        );
        // Still at it at 23:10: it starts at 23:40, not before.
        assert_eq!(
            week.starts(Some(friday(23, 10)), friday(23, 20)),
            Some(friday(23, 40))
        );
        // Never used a window: since the hours ended.
        assert_eq!(week.starts(None, friday(21, 0)), Some(friday(19, 0)));
        // Inside the hours, never on its own.
        assert_eq!(week.starts(None, friday(12, 0)), None);
    }

    #[test]
    fn the_lookout_starts_quiet_once_and_the_person_s_words_end_it() {
        let dir = std::env::temp_dir().join(format!("sonny-quiet-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let (present, file) = (dir.join("present"), dir.join("away"));
        let week = week();
        let friday = |hour, minute| local(2026, 11, 6, hour, minute);
        // No hours: nothing starts, nothing is written.
        assert_eq!(
            arm_at(&Schedule::default(), &present, &file, friday(23, 0)),
            None
        );
        assert!(!file.exists());
        // Inside the hours: nothing.
        assert_eq!(arm_at(&week, &present, &file, friday(12, 0)), None);
        // Used a window at 19:20: not yet at 19:45, at 19:50 it starts.
        touch(&present);
        let used = std::fs::File::options().write(true).open(&present).unwrap();
        let at_19_20 = UNIX_EPOCH + Duration::from_secs(friday(19, 20) as u64);
        used.set_modified(at_19_20).unwrap();
        assert_eq!(arm_at(&week, &present, &file, friday(19, 45)), None);
        let started = arm_at(&week, &present, &file, friday(19, 50)).unwrap();
        assert_eq!(
            started,
            Away {
                since: friday(19, 50),
                why: Why::Hours
            }
        );
        assert_eq!(away_at(&file), Some(started));
        // Once on, a second check starts nothing new.
        assert_eq!(arm_at(&week, &present, &file, friday(23, 0)), None);
        // `/quiet` while on keeps the first start.
        start_at(
            &file,
            Away {
                since: friday(23, 0),
                why: Why::Asked,
            },
        )
        .unwrap();
        assert_eq!(away_at(&file), Some(started));
        // The person's words end it, once.
        assert_eq!(end_at(&file), Some(started));
        assert_eq!(end_at(&file), None);
        assert_eq!(away_at(&file), None);
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn the_file_says_since_when_and_why() {
        let away = Away {
            since: 1_793_473_920,
            why: Why::Asked,
        };
        assert_eq!(read(&write(away)), Some(away));
        assert_eq!(
            read("1793473920\n"),
            Some(Away {
                since: 1_793_473_920,
                why: Why::Hours
            })
        );
        assert_eq!(read("never"), None);
    }

    #[test]
    fn the_clauses_state_facts_the_stamp_reader_knows() {
        let away = Away {
            since: local(2026, 11, 6, 19, 40),
            why: Why::Hours,
        };
        let said = away_clause(away, &week());
        assert_eq!(
            said,
            "The person is away since Fri 6 Nov 19:40; their hours are Mon-Fri 09:00-19:00."
        );
        assert!(is_clause(&said));
        let asked = away_clause(away, &Schedule::default());
        assert_eq!(asked, "The person is away since Fri 6 Nov 19:40.");
        assert!(is_clause(&asked));
        let back = back_clause(away);
        assert_eq!(
            back,
            "The person is back; they were away since Fri 6 Nov 19:40."
        );
        assert!(is_clause(&back));
        assert!(!is_clause("The person is here."));
    }

    #[test]
    fn the_footer_says_quiet_or_that_it_waits() {
        let week = week();
        let friday = |hour, minute| local(2026, 11, 6, hour, minute);
        assert_eq!(week.clone().hours.len(), 1);
        assert_eq!(
            footer(None, &week, friday(19, 30)),
            Some(("quiet when you step away".to_owned(), false))
        );
        assert_eq!(footer(None, &week, friday(12, 0)), None);
        assert_eq!(footer(None, &Schedule::default(), friday(23, 0)), None);
        let away = Away {
            since: friday(19, 40),
            why: Why::Hours,
        };
        assert_eq!(
            footer(Some(away), &week, friday(23, 0)),
            Some(("quiet since 19:40".to_owned(), true))
        );
    }
}
