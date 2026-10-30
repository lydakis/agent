//! The tasks the lead started, drawn where it started them so the person
//! sees them come and go: each with its name, its brief's first words,
//! and what it is doing now. A click on a task opens it in the pane.
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
    pub brief: String,
    /// What it is doing, as [`state`] reads it.
    pub state: String,
}

/// What a task is doing, from its newest turn as `turns` lists it.
pub fn state(turn: &Value) -> String {
    match turn["status"].as_str() {
        None => "starting",
        Some("waiting") if turn["waiting_on"]["approval"] == true => "needs you",
        Some("completed" | "steered") => "done",
        Some("failed") if turn["error"] == "empty_completion" => QUIET,
        Some("failed") => "failed",
        Some("interrupted") => "stopped",
        Some(_) => "working",
    }
    .to_owned()
}

/// The state of a task whose turn ended with the model saying nothing: no
/// failure, and not a reply either.
pub const QUIET: &str = "no reply";

/// The state of a task whose turn ended while it has a goal that turn did
/// not meet yet: its check runs, or it works on.
pub const CHECKING: &str = "checking";

/// The state of a task whose turn ended and met its goal: its check passed.
pub const CHECKED: &str = "checked";

/// Whether a task in `state` waits on nothing more of its own.
pub fn settled(state: &str) -> bool {
    matches!(
        state,
        "done" | QUIET | CHECKED | "failed" | "stopped" | "removed"
    )
}

/// A state's mark and its color.
fn sign(state: &str) -> Span<'static> {
    match state {
        "done" => Span::raw("✓").fg(Color::Green),
        CHECKED => Span::raw("✓").fg(Color::Green),
        QUIET => Span::raw("✓").dim(),
        "failed" => Span::raw("✗").fg(Color::Red),
        "needs you" => Span::raw("?").fg(Color::Yellow).bold(),
        "stopped" | "removed" => Span::raw("■").dim(),
        "starting" => Span::raw("○").dim(),
        _ => Span::raw("●").fg(Color::Cyan),
    }
}

/// A brief's first line, cut short enough to keep.
pub fn first(brief: &str) -> String {
    let line = brief
        .lines()
        .find(|line| !line.trim().is_empty())
        .unwrap_or("");
    cut(line.trim(), 200)
}

/// The rows of `tasks` started together, `width` wide, each with the
/// task a click on it opens, if any: `• Started 3 tasks`, then a row for
/// each with its state's mark, its name, what it is doing, and its brief
/// cut to fit.
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
    for (at, task) in tasks.iter().enumerate() {
        // "  ● name  state  brief"
        let room = width.saturating_sub(4 + names + 2 + states + 2);
        out.push((
            Line::from(vec![
                Span::raw("  "),
                sign(&task.state),
                Span::raw(" "),
                Span::raw(format!("{:<names$}", task.name))
                    .bold()
                    .fg(Color::Blue),
                Span::raw("  "),
                Span::raw(format!("{:<states$}", task.state)).dim(),
                Span::raw("  "),
                Span::raw(cut(&first(&task.brief), room)),
            ]),
            Some(at),
        ));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn task(name: &str, brief: &str, state: &str) -> Task {
        Task {
            bot: format!("calc.{name}"),
            name: name.into(),
            brief: brief.into(),
            state: state.into(),
        }
    }

    fn texts(rows: Vec<(Line<'static>, Option<usize>)>) -> Vec<String> {
        rows.into_iter().map(|(line, _)| line.to_string()).collect()
    }

    #[test]
    fn a_task_reads_its_state_from_its_newest_turn() {
        assert_eq!(state(&Value::Null), "starting");
        assert_eq!(state(&json!({"status": "running"})), "working");
        let asked = json!({"status": "waiting", "waiting_on": {"approval": true}});
        assert_eq!(state(&asked), "needs you");
        assert_eq!(state(&json!({"status": "waiting"})), "working");
        assert_eq!(state(&json!({"status": "completed"})), "done");
        let empty = json!({"status": "failed", "error": "empty_completion"});
        assert_eq!(state(&empty), "no reply");
        assert_eq!(state(&json!({"status": "failed"})), "failed");
    }

    #[test]
    fn a_roster_counts_the_tasks_and_lines_their_states_up() {
        let tasks = [
            task("add", "Fix the add bug: 2+2 returns 5\nmore", "working"),
            task("divide", "Fix division by zero", "done"),
        ];
        assert_eq!(
            texts(draw(&tasks, 60)),
            [
                "• Started 2 tasks · click one to open it",
                "  ● add     working  Fix the add bug: 2+2 returns 5",
                "  ✓ divide  done     Fix division by zero",
            ]
        );
        let rows = draw(&tasks, 60);
        assert_eq!(
            rows.iter().map(|(_, at)| *at).collect::<Vec<_>>(),
            [None, Some(0), Some(1)]
        );
    }
}
