//! When Sonny sent a message, on the message itself. A model has no clock:
//! the time it started at falls out with compaction, and over a long
//! conversation it guesses. So, as chat harnesses stamp each message they
//! pass a model, the person's words and the lookout's notes end with a
//! line naming the local time they were sent. The line comes after the
//! words, so what the model read before stays cached, and Sonny draws the
//! words without it.

/// The line's start and end: `[Sent Sat 31 Oct 2026 16:12, UTC-04:00.]`.
const OPEN: &str = "\n\n[Sent ";
const CLOSE: &str = ".]";

const DAYS: [&str; 7] = ["Sun", "Mon", "Tue", "Wed", "Thu", "Fri", "Sat"];
const MONTHS: [&str; 12] = [
    "Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec",
];

/// `prompt` with the line naming `epoch` (seconds) in local time after it.
pub fn stamped(prompt: &str, epoch: i64) -> String {
    format!("{prompt}{OPEN}{}{CLOSE}", when(epoch))
}

/// `prompt`, stamped now.
pub fn now(prompt: &str) -> String {
    let epoch = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_secs() as i64);
    stamped(prompt, epoch)
}

/// A message's words without the line `stamped` put after them.
pub fn unstamped(text: &str) -> &str {
    let Some(at) = text.rfind(OPEN) else {
        return text;
    };
    let line = &text[at + OPEN.len()..];
    match line.strip_suffix(CLOSE).is_some_and(names_a_time) {
        true => &text[..at],
        false => text,
    }
}

/// Whether `line` is what `when` writes: `Sat 31 Oct 2026 16:12, UTC-04:00`.
fn names_a_time(line: &str) -> bool {
    let Some((at, zone)) = line.split_once(", UTC") else {
        return false;
    };
    let words: Vec<&str> = at.split(' ').collect();
    let digits = |s: &str, n: usize| s.len() == n && s.bytes().all(|b| b.is_ascii_digit());
    let zone = zone.as_bytes();
    matches!(words[..], [day, date, month, year, time]
        if DAYS.contains(&day)
            && (digits(date, 1) || digits(date, 2))
            && MONTHS.contains(&month)
            && digits(year, 4)
            && time.split_once(':').is_some_and(|(h, m)| digits(h, 2) && digits(m, 2)))
        && zone.len() == 6
        && matches!(zone[0], b'+' | b'-')
        && zone[3] == b':'
        && [1, 2, 4, 5].iter().all(|&i| zone[i].is_ascii_digit())
}

/// `epoch` in local time, its weekday and its offset from UTC.
fn when(epoch: i64) -> String {
    // SAFETY: localtime_r fills the zeroed struct it is given and nothing else.
    let tm = unsafe {
        let mut tm: libc::tm = std::mem::zeroed();
        libc::localtime_r(&(epoch as libc::time_t), &mut tm);
        tm
    };
    let offset = tm.tm_gmtoff / 60;
    format!(
        "{} {} {} {} {:02}:{:02}, UTC{}{:02}:{:02}",
        DAYS[tm.tm_wday.rem_euclid(7) as usize],
        tm.tm_mday,
        MONTHS[tm.tm_mon.rem_euclid(12) as usize],
        tm.tm_year + 1900,
        tm.tm_hour,
        tm.tm_min,
        if offset < 0 { '-' } else { '+' },
        offset.abs() / 60,
        offset.abs() % 60,
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_stamped_message_reads_as_its_words_again() {
        let sent = stamped("fix the build", 1_793_473_920);
        assert!(sent.starts_with("fix the build\n\n[Sent "), "{sent}");
        assert!(sent.ends_with(".]"), "{sent}");
        assert!(names_a_time(
            &sent["fix the build\n\n[Sent ".len()..sent.len() - 2]
        ));
        assert_eq!(unstamped(&sent), "fix the build");
        // Twice is still once more each.
        assert_eq!(unstamped(&stamped(&sent, 0)), sent);
    }

    #[test]
    fn words_that_only_look_like_a_stamp_stay() {
        for text in [
            "plain",
            "see\n\n[Sent by the person.]",
            "see\n\n[Sent Sat 31 Oct 2026 16:12, UTC-04:00.] and more",
            "see\n\n[Sent Sat 31 Oct 2026 16:12.]",
            "see\n\n[Sent Someday 31 Oct 2026 16:12, UTC-04:00.]",
        ] {
            assert_eq!(unstamped(text), text);
        }
        assert_eq!(
            unstamped("see\n\n[Sent Sat 31 Oct 2026 16:12, UTC-04:00.]"),
            "see"
        );
    }
}
