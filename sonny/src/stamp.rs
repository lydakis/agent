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
pub const MONTHS: [&str; 12] = [
    "Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec",
];

/// `prompt` with the line naming `epoch` (seconds) in local time after it.
pub fn stamped(prompt: &str, epoch: i64) -> String {
    format!("{prompt}{OPEN}{}{CLOSE}", when(epoch))
}

fn epoch_now() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_secs() as i64)
}

/// `prompt` stamped now.
pub fn now(prompt: &str) -> String {
    stamped(prompt, epoch_now())
}

/// A message's words without the line `stamped` put after them.
pub fn unstamped(text: &str) -> &str {
    let Some(at) = text.rfind(OPEN) else {
        return text;
    };
    let line = &text[at + OPEN.len()..];
    let read = |line: &str| match line.split_once(". ") {
        Some((time, clause)) => names_a_time(time) && was_away_or_back(&format!("{clause}.")),
        None => names_a_time(line),
    };
    match line.strip_suffix(CLOSE).is_some_and(read) {
        true => &text[..at],
        false => text,
    }
}

/// Whether `text` is a sentence Sonny once put after the time, while
/// quiet mode was (gone in 0.9.1): kept so the conversations it stamped
/// draw as their words still.
fn was_away_or_back(text: &str) -> bool {
    (text.starts_with("The person is away since ")
        || text.starts_with("The person is back; they were away since "))
        && text.ends_with('.')
        && !text.contains('\n')
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

/// `epoch` (seconds) in local time.
pub fn local(epoch: i64) -> libc::tm {
    // SAFETY: localtime_r fills the zeroed struct it is given and nothing else.
    unsafe {
        let mut tm: libc::tm = std::mem::zeroed();
        libc::localtime_r(&(epoch as libc::time_t), &mut tm);
        tm
    }
}

/// `epoch` in local time, its weekday and its offset from UTC.
pub fn when(epoch: i64) -> String {
    let tm = local(epoch);
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
    fn a_stamp_older_sonnys_wrote_saying_the_person_was_away_or_back_reads_as_the_words_too() {
        let away = "look at the build\n\n[Sent Sat 7 Nov 2026 16:12, UTC-04:00. The person is away since Fri 6 Nov 19:40; their hours are Mon-Fri 09:00-19:00.]";
        assert_eq!(unstamped(away), "look at the build");
        let back = "morning\n\n[Sent Sat 7 Nov 2026 16:12, UTC-04:00. The person is back; they were away since Fri 6 Nov 19:40.]";
        assert_eq!(unstamped(back), "morning");
        // A sentence of the person's own after a time is theirs.
        let typed = "see\n\n[Sent Sat 31 Oct 2026 16:12, UTC-04:00. Then I left.]";
        assert_eq!(unstamped(typed), typed);
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
