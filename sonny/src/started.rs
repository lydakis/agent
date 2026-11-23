//! The tasks the lead started, drawn where it started them so the person
//! sees them come and go: each with its title, its name, and what it is
//! doing now. A click on a task opens it in the pane.
use ratatui::{
    style::{Color, Stylize},
    text::{Line, Span},
};
use serde_json::Value;
use unicode_width::UnicodeWidthStr;

use crate::transcript::cut;

/// A task the lead started.
#[derive(Clone, Debug, PartialEq)]
pub struct Task {
    /// The bot's whole name, `shop.fix`.
    pub bot: String,
    /// Its name in the project, `fix`.
    pub name: String,
    /// What it is for, as `start_task` set it: empty for one started
    /// without.
    pub title: String,
    /// What it is doing, as [`state`] reads it.
    pub state: String,
}

/// What a task is doing, from its newest turn as `turns` lists it; what
/// it waits on the person for is [`crate::needs`]'s to say.
pub fn state(turn: &Value) -> String {
    match turn["status"].as_str() {
        None => "starting",
        Some("completed") => IDLE,
        Some("failed") if turn["error"] == "empty_completion" => QUIET,
        Some("failed") => "failed",
        Some("interrupted") => INTERRUPTED,
        Some(_) => "working",
    }
    .to_owned()
}

/// The state of a task whose turn ended with nothing open: it rests, until
/// it or the person picks the work up again, or it is closed, done.
pub const IDLE: &str = "idle";

/// The state of a task closed, done, by itself, its lead or the person,
/// until new work reopens it.
pub const DONE: &str = "done";

/// The state of a task whose turn ended with the model saying nothing: no
/// failure, and not a reply either.
pub const QUIET: &str = "no reply";

/// The state of a task whose turn the person ended.
pub const INTERRUPTED: &str = "interrupted";

/// Whether a task in `state` waits on nothing more of its own.
pub fn settled(state: &str) -> bool {
    matches!(
        state,
        IDLE | DONE | QUIET | INTERRUPTED | "failed" | "removed"
    )
}

/// A message's first line, cut short enough to keep.
pub fn first(brief: &str) -> String {
    let line = brief
        .lines()
        .find(|line| !line.trim().is_empty())
        .unwrap_or("");
    cut(line.trim(), 200)
}

/// The rows of `tasks` started together, `width` wide, each with the
/// task a click on it opens, if any: `• Started 3 tasks`, then a row for
/// each that leads with its title, as `/status` does: its state's mark,
/// its title, cut at a word to fit, its name and what it is doing.
pub fn draw(tasks: &[Task], width: usize) -> Vec<(Line<'static>, Option<usize>)> {
    let count = match tasks.len() {
        1 => "1 task".to_owned(),
        n => format!("{n} tasks"),
    };
    let mut out = vec![(
        Line::from(vec![
            Span::raw("• ").fg(Color::Green),
            Span::raw(format!("Started {count}")).bold(),
            Span::raw(" · click one to open it").dim(),
        ]),
        None,
    )];
    let names = tasks.iter().map(|t| t.name.width()).max().unwrap_or(0);
    let states = tasks.iter().map(|t| t.state.width()).max().unwrap_or(0);
    // "  ● title  name  state": the titles share a column as wide as the
    // widest, as far as the row has room.
    let room = width.saturating_sub(4 + 2 + names + 2 + states);
    let titles = (tasks.iter())
        .map(|t| crate::title::cut(&t.title, room).width())
        .max()
        .unwrap_or(0);
    for (at, task) in tasks.iter().enumerate() {
        let mut row = vec![
            Span::raw("  "),
            crate::status::sign(&task.state),
            Span::raw(" "),
        ];
        if titles > 0 {
            let title = crate::title::cut(&task.title, room);
            let pad = " ".repeat(titles.saturating_sub(title.width()));
            row.push(Span::raw(format!("{title}{pad}  ")));
        }
        row.extend([
            Span::raw(format!("{:<names$}", task.name))
                .bold()
                .fg(Color::Blue),
            Span::raw("  "),
            Span::raw(task.state.clone()).dim(),
        ]);
        out.push((Line::from(row), Some(at)));
    }
    out
}

/// One thing a turn set going, for the `└` row under its reply: a task
/// it `started`, or an agent there already it `sent to`.
#[derive(Clone, Debug, PartialEq)]
pub struct SetGoing {
    pub verb: &'static str,
    /// The bot's whole name, `shop.fix`, which its link opens.
    pub bot: String,
    /// Its name as the conversation says it, `fix`.
    pub name: String,
    /// Its title: a started task's, as `start_task` set it, or the first
    /// line of the message sent.
    pub title: String,
}

/// The `└` row under a reply: what its turn set going, `width` wide, each
/// name with its state's mark before it, as `state` reads it; and where
/// each name is on the row, with the bot it opens. With one task and
/// room left its title follows; names give way to `+N`.
pub fn hint(
    set: &[SetGoing],
    state: impl Fn(&str) -> String,
    width: usize,
) -> (Line<'static>, Vec<(u16, u16, String)>) {
    // How many names fit: each drawn group is `verb ● name · ● name`.
    let draw = |shown: usize| {
        let mut spans = vec![Span::raw("└ ").dim()];
        let mut links = Vec::new();
        let mut column = 2;
        let mut verb = None;
        for (at, one) in set.iter().take(shown).enumerate() {
            let words = match verb == Some(one.verb) {
                true => " · ".to_owned(),
                false if at == 0 => format!("{} ", one.verb),
                false => format!(" · {} ", one.verb),
            };
            verb = Some(one.verb);
            column += words.width();
            spans.push(Span::raw(words).dim());
            spans.push(crate::status::sign(&state(&one.bot)));
            spans.push(Span::raw(" "));
            column += 2;
            let wide = one.name.width();
            spans.push(Span::raw(one.name.clone()).fg(Color::Blue));
            links.push((column as u16, (column + wide) as u16, one.bot.clone()));
            column += wide;
        }
        if shown < set.len() {
            let more = format!(" +{}", set.len() - shown);
            column += more.width();
            spans.push(Span::raw(more).dim());
        }
        (spans, links, column)
    };
    let mut shown = set.len();
    let (mut spans, mut links, mut column) = draw(shown);
    while column > width && shown > 1 {
        shown -= 1;
        (spans, links, column) = draw(shown);
    }
    // One alone, and room for its title.
    if let [one] = set
        && !one.title.is_empty()
        && column + 3 + 8 <= width
    {
        spans.push(Span::raw(format!(" · {}", cut(&one.title, width - column - 3))).dim());
    }
    (Line::from(spans), links)
}

/// The verb of a task a turn started.
pub const STARTED: &str = "started";

/// The verb of a message a turn sent an agent there already.
pub const SENT: &str = "sent to";

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn task(name: &str, title: &str, state: &str) -> Task {
        Task {
            bot: format!("calc.{name}"),
            name: name.into(),
            title: title.into(),
            state: state.into(),
        }
    }

    fn texts(rows: Vec<(Line<'static>, Option<usize>)>) -> Vec<String> {
        rows.into_iter().map(|(line, _)| line.to_string()).collect()
    }

    #[test]
    fn the_hint_names_what_a_turn_set_going_with_marks_and_gives_way_to_a_count() {
        let one = |verb, name: &str, title: &str| SetGoing {
            verb,
            bot: format!("calc.{name}"),
            name: name.into(),
            title: title.into(),
        };
        let working = |_: &str| "working".to_owned();
        let set = [one(STARTED, "halves", "Add a test for half cents")];
        let (row, links) = hint(&set, working, 80);
        assert_eq!(
            row.to_string(),
            "└ started ● halves · Add a test for half cents"
        );
        assert_eq!(links, [(12, 18, "calc.halves".to_owned())]);
        // No room for the title: the name alone.
        assert_eq!(hint(&set, working, 24).0.to_string(), "└ started ● halves");
        let set = [
            one(STARTED, "halves", "Add a test"),
            one(STARTED, "readme", "Say how"),
            one(SENT, "fix", ""),
        ];
        let (row, links) = hint(&set, working, 80);
        assert_eq!(
            row.to_string(),
            "└ started ● halves · ● readme · sent to ● fix"
        );
        assert_eq!(links[2], (42, 45, "calc.fix".to_owned()));
        // Too narrow: names give way to a count.
        assert_eq!(
            hint(&set, working, 32).0.to_string(),
            "└ started ● halves · ● readme +1"
        );
        assert_eq!(
            hint(&set, |_| "idle".into(), 24).0.to_string(),
            "└ started ○ halves +2"
        );
    }

    #[test]
    fn a_task_reads_its_state_from_its_newest_turn() {
        assert_eq!(state(&Value::Null), "starting");
        assert_eq!(state(&json!({"status": "running"})), "working");
        // Whether a call waits on the person, not a reviewer, the
        // approvals listing says ([`crate::needs`]).
        let asked = json!({"status": "waiting", "waiting_on": {"approval": true}});
        assert_eq!(state(&asked), "working");
        assert_eq!(state(&json!({"status": "waiting"})), "working");
        assert_eq!(state(&json!({"status": "completed"})), "idle");
        let empty = json!({"status": "failed", "error": "empty_completion"});
        assert_eq!(state(&empty), "no reply");
        assert_eq!(state(&json!({"status": "failed"})), "failed");
    }

    #[test]
    fn a_roster_counts_the_tasks_and_leads_each_with_its_title() {
        let tasks = [
            task("add", "Fix two plus two", "working"),
            task("divide", "Stop dividing by zero in the totals", "done"),
        ];
        assert_eq!(
            texts(draw(&tasks, 64)),
            [
                "• Started 2 tasks · click one to open it",
                "  ● Fix two plus two                     add     working",
                "  ✓ Stop dividing by zero in the totals  divide  done",
            ]
        );
        let rows = draw(&tasks, 64);
        assert_eq!(
            rows.iter().map(|(_, at)| *at).collect::<Vec<_>>(),
            [None, Some(0), Some(1)]
        );
        // Short of room, the title is cut at a word; the name stays.
        assert_eq!(
            texts(draw(&tasks, 44))[2],
            "  ✓ Stop dividing by zero…  divide  done"
        );
        // A task started without one: its name alone.
        assert_eq!(
            texts(draw(&[task("add", "", "idle")], 44))[1],
            "  ○ add  idle"
        );
    }
}
