//! What a read the person waits on is before it has rows, drawn the one
//! way everywhere (the Sonny Rulebook's **Reading**, 28 Nov): `reading…`
//! while it is out, its time after two seconds (`reading… 3s`), the empty
//! sentence once it read nothing, or the failure. Every view draws these
//! three through here, and none writes the word itself.

// The views' empty sentences and failures come here with the views' own
// change; the window's signs use only the word so far.
#![allow(dead_code)]

use std::time::{Duration, Instant};

use ratatui::{
    style::{Color, Stylize},
    text::{Line, Span},
};

/// The word, muted wherever it shows.
pub const WORD: &str = "reading…";

/// How long a read is out before its sign says for how long, as the
/// running row counts a turn: a word that never changes cannot tell slow
/// from stuck (the Rulebook's Reading, 28 Nov: two seconds).
pub const CLOCK_AFTER: Duration = Duration::from_secs(2);

#[cfg(test)]
thread_local! {
    /// How many empty sentences were drawn on this thread: the walk over
    /// every command checks none is drawn while every read is held.
    static EMPTIES: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
}

/// What a read is before it has rows.
pub enum Before<'a> {
    /// Still out, asked at that instant.
    Reading(Instant),
    /// Read, and nothing in it: the sentence that says so, and the muted
    /// line that says how the place fills, if any.
    Empty(&'a str, Option<&'a str>),
    /// It failed: what was read (`usage`), and why.
    Failed(&'a str, &'a str),
}

/// The sign's words for a read out since `since`: the word, and past
/// [`CLOCK_AFTER`] its time by the second.
pub fn word(since: Instant) -> String {
    let out = since.elapsed();
    match out < CLOCK_AFTER {
        true => WORD.to_owned(),
        false => format!("{WORD} {}", crate::transcript::elapsed(out.as_secs())),
    }
}

/// The sign as a muted span, to stand on a row of its own or where a
/// value goes.
pub fn sign(since: Instant) -> Span<'static> {
    Span::raw(word(since)).dim()
}

/// The sign as a row of its own.
pub fn line(since: Instant) -> Line<'static> {
    Line::from(sign(since))
}

/// The sign at the end of a head that keeps its answer while a key
/// changes it: ` · reading…`.
pub fn head(since: Instant) -> Span<'static> {
    Span::raw(format!(" · {}", word(since))).dim()
}

/// A read that failed, in its place: a red `✗`, then what could not be
/// read and why, as the Rulebook's Voice says.
pub fn failed(what: &str, why: &str) -> Line<'static> {
    Line::from(vec![
        Span::raw("✗ ").fg(Color::Red),
        Span::raw(format!("could not read {what}: {why}")),
    ])
}

/// A read that found nothing: one plain sentence, then one muted line
/// that says how the place fills.
pub fn empty(said: &str, fills: Option<&str>) -> Vec<Line<'static>> {
    #[cfg(test)]
    EMPTIES.with(|drawn| drawn.set(drawn.get() + 1));
    let mut out = vec![Line::from(said.to_owned())];
    out.extend(fills.map(|fills| Line::from(fills.to_owned()).dim()));
    out
}

/// What a read is before it has rows, as the rows that say so.
pub fn lines(before: Before) -> Vec<Line<'static>> {
    match before {
        Before::Reading(since) => vec![line(since)],
        Before::Empty(said, fills) => empty(said, fills),
        Before::Failed(what, why) => vec![failed(what, why)],
    }
}

/// How many empty sentences this thread has drawn.
#[cfg(test)]
pub fn empties() -> usize {
    EMPTIES.with(std::cell::Cell::get)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn text(line: &Line) -> String {
        line.spans
            .iter()
            .map(|span| span.content.as_ref())
            .collect()
    }

    #[test]
    fn the_sign_is_the_word_then_carries_its_time_after_two_seconds() {
        let now = Instant::now();
        assert_eq!(word(now), "reading…");
        assert_eq!(word(now - Duration::from_millis(1_999)), "reading…");
        assert_eq!(word(now - Duration::from_secs(2)), "reading… 2s");
        assert_eq!(word(now - Duration::from_secs(3)), "reading… 3s");
        assert_eq!(word(now - Duration::from_secs(75)), "reading… 1m 15s");
        assert_eq!(text(&Line::from(head(now))), " · reading…");
    }

    #[test]
    fn a_read_before_its_rows_is_reading_empty_or_failed() {
        let reading = lines(Before::Reading(Instant::now()));
        assert_eq!(reading.iter().map(text).collect::<Vec<_>>(), ["reading…"]);
        let empty = lines(Before::Empty(
            "No tool calls yet.",
            Some("Each tool call shows here as it ends."),
        ));
        assert_eq!(
            empty.iter().map(text).collect::<Vec<_>>(),
            [
                "No tool calls yet.",
                "Each tool call shows here as it ends."
            ]
        );
        let failed = lines(Before::Failed("usage", "the store is locked"));
        assert_eq!(
            failed.iter().map(text).collect::<Vec<_>>(),
            ["✗ could not read usage: the store is locked"]
        );
    }
}
