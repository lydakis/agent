//! When a message was sent. A model has no clock: the time it started at
//! falls out with compaction, and over a long conversation it guesses. So,
//! as chat harnesses stamp each message they pass a model, every agent
//! Sonny makes has the daemon end each message's line with the local time
//! it was sent (`trailer`), after the words, so what the model read before
//! stays cached; Sonny draws the words without it. The window's own times
//! are read here too.

pub use agent_client::clock::when;
use agent_client::clock::{DAYS, MONTHS, local};

/// What the line after each message names for the model of an agent
/// Sonny makes: the config's `trailer`, else who sent it and when.
pub fn trailer(config: &agent_client::config::Config) -> Vec<String> {
    match &config.trailer {
        Some(trailer) => trailer.clone(),
        None => vec!["sender".into(), "time".into()],
    }
}

fn epoch_now() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_secs() as i64)
}

/// When a message the window shows was written, `ms` its Unix time in
/// milliseconds, as the person's clock reads it: `Sat 7 Nov 09:13`, or
/// `Sat 7 Nov 9:13 AM` where their locale writes times on a 12-hour clock;
/// the year only when it is not this one.
pub fn written(ms: i64) -> String {
    let (at, now) = (local(ms.div_euclid(1000)), local(epoch_now()));
    written_as(&at, at.tm_year != now.tm_year, twelve_hour())
}

/// [`written`] for a time read already, with its year or not, on a
/// 12-hour clock or not.
fn written_as(at: &libc::tm, year: bool, twelve: bool) -> String {
    let mut out = format!(
        "{} {} {}",
        DAYS[at.tm_wday.rem_euclid(7) as usize],
        at.tm_mday,
        MONTHS[at.tm_mon.rem_euclid(12) as usize],
    );
    if year {
        out.push_str(&format!(" {}", at.tm_year + 1900));
    }
    match twelve {
        true => {
            let hour = (at.tm_hour + 11).rem_euclid(12) + 1;
            let half = if at.tm_hour < 12 { "AM" } else { "PM" };
            out.push_str(&format!(" {hour}:{:02} {half}", at.tm_min));
        }
        false => out.push_str(&format!(" {:02}:{:02}", at.tm_hour, at.tm_min)),
    }
    out
}

/// Whether the person's locale writes times on a 12-hour clock, as the
/// time format its `LC_TIME` names says: read once, from the
/// environment the window started in.
fn twelve_hour() -> bool {
    static TWELVE: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *TWELVE.get_or_init(|| locale_time_format().is_some_and(|format| twelve_in(&format)))
}

/// Whether a `strftime` time format reads on a 12-hour clock.
fn twelve_in(format: &str) -> bool {
    ["%I", "%l", "%r", "%p"]
        .iter()
        .any(|mark| format.contains(mark))
}

/// The locale's time format (`t_fmt`), read on this thread alone: the
/// process's own locale, which Rust leaves as C, is not changed.
fn locale_time_format() -> Option<String> {
    // SAFETY: the locale is made from the environment, used on this
    // thread only while its format is read and copied, then the thread's
    // own is put back and the made one freed; a null locale is never used.
    unsafe {
        let made = libc::newlocale(libc::LC_TIME_MASK, c"".as_ptr(), std::ptr::null_mut());
        if made.is_null() {
            return None;
        }
        let before = libc::uselocale(made);
        let format = libc::nl_langinfo(libc::T_FMT);
        let read = (!format.is_null()).then(|| {
            std::ffi::CStr::from_ptr(format)
                .to_string_lossy()
                .into_owned()
        });
        libc::uselocale(before);
        libc::freelocale(made);
        read
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn at(year: i32, month: i32, day: i32, weekday: i32, hour: i32, minute: i32) -> libc::tm {
        // SAFETY: a zeroed tm is a valid tm; the fields read are set.
        let mut tm: libc::tm = unsafe { std::mem::zeroed() };
        (tm.tm_year, tm.tm_mon, tm.tm_mday) = (year - 1900, month - 1, day);
        (tm.tm_wday, tm.tm_hour, tm.tm_min) = (weekday, hour, minute);
        tm
    }

    #[test]
    fn a_message_time_reads_as_the_day_and_the_clock() {
        let morning = at(2026, 11, 7, 6, 9, 13);
        assert_eq!(written_as(&morning, false, false), "Sat 7 Nov 09:13");
        assert_eq!(written_as(&morning, false, true), "Sat 7 Nov 9:13 AM");
        assert_eq!(written_as(&morning, true, false), "Sat 7 Nov 2026 09:13");
        let (noon, midnight) = (at(2026, 11, 7, 6, 12, 5), at(2026, 11, 8, 0, 0, 30));
        assert_eq!(written_as(&noon, false, true), "Sat 7 Nov 12:05 PM");
        assert_eq!(written_as(&midnight, false, true), "Sun 8 Nov 12:30 AM");
        assert_eq!(written_as(&midnight, false, false), "Sun 8 Nov 00:30");
    }

    #[test]
    fn a_locale_writing_hours_of_twelve_reads_on_a_twelve_hour_clock() {
        // glibc's en_US, a BSD's, the C locale's, en_GB's.
        assert!(twelve_in("%r"));
        assert!(twelve_in("%I:%M:%S %p"));
        assert!(!twelve_in("%H:%M:%S"));
        assert!(!twelve_in("%T"));
    }
}
