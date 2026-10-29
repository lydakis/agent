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

/// How tasks started are drawn. A prototype picks one by
/// `SONNY_TASKS=cards`; the roster is the default.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Style {
    /// One row each under a header that counts them.
    Roster,
    /// A box each, its state in the corner.
    Cards,
}

impl Style {
    pub fn chosen() -> Self {
        match std::env::var("SONNY_TASKS").as_deref() {
            Ok("cards") => Style::Cards,
            _ => Style::Roster,
        }
    }
}

/// What a task is doing, from its newest turn as `turns` lists it.
pub fn state(turn: &Value) -> String {
    match turn["status"].as_str() {
        None => "starting",
        Some("waiting") if turn["waiting_on"]["approval"] == true => "needs you",
        Some("completed" | "steered") => "done",
        Some("failed") if turn["error"] == "empty_completion" => "done",
        Some("failed") => "failed",
        Some("interrupted") => "stopped",
        Some(_) => "working",
    }
    .to_owned()
}

/// Whether a task in `state` waits on nothing more of its own.
pub fn settled(state: &str) -> bool {
    matches!(state, "done" | "failed" | "stopped" | "removed")
}

/// A state's mark and its color.
fn sign(state: &str) -> Span<'static> {
    match state {
        "done" => Span::raw("✓").fg(Color::Green),
        "failed" => Span::raw("✗").fg(Color::Red),
        "needs you" => Span::raw("●").fg(Color::Yellow),
        "stopped" | "removed" => Span::raw("■").dim(),
        "starting" => Span::raw("○").dim(),
        _ => Span::raw("●").fg(Color::Cyan),
    }
}

fn first(brief: &str) -> &str {
    brief
        .lines()
        .find(|line| !line.trim().is_empty())
        .unwrap_or("")
        .trim()
}

/// The rows of `tasks` started together, `width` wide, each with the
/// task a click on it opens, if any.
pub fn draw(style: Style, tasks: &[Task], width: usize) -> Vec<(Line<'static>, Option<usize>)> {
    match style {
        Style::Roster => roster(tasks, width),
        Style::Cards => cards(tasks, width),
    }
}

/// `◆ Started 3 tasks`, then a row for each: its state's mark, its name,
/// its brief cut to fit, and its state at the end.
fn roster(tasks: &[Task], width: usize) -> Vec<(Line<'static>, Option<usize>)> {
    let count = match tasks.len() {
        1 => "1 task".to_owned(),
        n => format!("{n} tasks"),
    };
    let mut out = vec![(
        Line::from(vec![
            Span::raw("◆ ").fg(Color::Magenta),
            Span::raw(format!("Started {count}")).bold(),
        ]),
        None,
    )];
    let names = tasks.iter().map(|t| t.name.width()).max().unwrap_or(0);
    for (at, task) in tasks.iter().enumerate() {
        let end = format!("{} ›", task.state);
        // "  ● name  brief   state ›"
        let room = width.saturating_sub(4 + names + 2 + 2 + end.width());
        let brief = cut(first(&task.brief), room);
        let pad = room.saturating_sub(brief.width());
        out.push((
            Line::from(vec![
                Span::raw("  "),
                sign(&task.state),
                Span::raw(" "),
                Span::raw(format!("{:<names$}", task.name)).bold(),
                Span::raw("  "),
                Span::raw(brief),
                Span::raw(" ".repeat(pad + 2)),
                Span::raw(end).dim(),
            ]),
            Some(at),
        ));
    }
    out
}

/// A box for each task: its name and state on the top edge, its brief
/// inside, `open ›` on the bottom edge.
fn cards(tasks: &[Task], width: usize) -> Vec<(Line<'static>, Option<usize>)> {
    let width = width.clamp(24, 72);
    let inner = width - 4;
    let mut out = Vec::new();
    for (at, task) in tasks.iter().enumerate() {
        let state = format!(" {} ", task.state);
        let name = format!(" {} ", task.name);
        let rule = width.saturating_sub(6 + name.width() + state.width());
        out.push((
            Line::from(vec![
                Span::raw("╭─").fg(Color::Magenta),
                Span::raw(name).bold(),
                Span::raw("─".repeat(rule)).fg(Color::Magenta),
                Span::raw(" "),
                sign(&task.state),
                Span::raw(state).dim(),
                Span::raw("─╮").fg(Color::Magenta),
            ]),
            Some(at),
        ));
        let brief = cut(first(&task.brief), inner);
        let pad = inner.saturating_sub(brief.width());
        out.push((
            Line::from(vec![
                Span::raw("│ ").fg(Color::Magenta),
                Span::raw(brief),
                Span::raw(" ".repeat(pad)),
                Span::raw(" │").fg(Color::Magenta),
            ]),
            Some(at),
        ));
        let open = " open › ";
        out.push((
            Line::from(vec![
                Span::raw("╰".to_owned() + &"─".repeat(width - 3 - open.width()))
                    .fg(Color::Magenta),
                Span::raw(open).dim(),
                Span::raw("─╯").fg(Color::Magenta),
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
        assert_eq!(state(&empty), "done");
        assert_eq!(state(&json!({"status": "failed"})), "failed");
    }

    #[test]
    fn a_roster_counts_the_tasks_and_lines_their_states_up() {
        let tasks = [
            task("add", "Fix the add bug: 2+2 returns 5\nmore", "working"),
            task("divide", "Fix division by zero", "done"),
        ];
        assert_eq!(
            texts(roster(&tasks, 60)),
            [
                "◆ Started 2 tasks",
                "  ● add     Fix the add bug: 2+2 returns 5         working ›",
                "  ✓ divide  Fix division by zero                      done ›",
            ]
        );
        let rows = roster(&tasks, 60);
        assert_eq!(
            rows.iter().map(|(_, at)| *at).collect::<Vec<_>>(),
            [None, Some(0), Some(1)]
        );
    }

    #[test]
    fn a_card_boxes_the_task_with_its_state_on_top() {
        let rows = texts(cards(&[task("add", "Fix the add bug", "needs you")], 40));
        assert_eq!(
            rows,
            [
                "╭─ add ────────────────── ● needs you ─╮",
                "│ Fix the add bug                      │",
                "╰───────────────────────────── open › ─╯",
            ]
        );
    }
}
