//! `/status`: the project's agents on one card, read from the daemon and
//! Rex with no model call. Each row says what the agent is doing, for how
//! long, on which model, its latest step, and where it is in Rex.
use ratatui::{
    style::{Color, Stylize},
    text::{Line, Span},
};

pub struct Row {
    /// The agent's name in its project: `lead`, or the task's.
    pub name: String,
    pub state: String,
    /// Seconds it has been at it, or since it finished.
    pub seconds: Option<u64>,
    pub model: String,
    pub latest: String,
    /// Where it is in Rex: `tab 2`, `tab 1, split`, or empty.
    pub place: String,
}

/// The mark and colour a state reads with.
pub fn mark(state: &str) -> Span<'static> {
    match state {
        "working" => Span::raw("● ").fg(Color::Cyan),
        "needs you" => Span::raw("◐ ").fg(Color::Magenta),
        "done" | "completed" => Span::raw("✓ ").fg(Color::Green),
        "failed" => Span::raw("✗ ").fg(Color::Red),
        s if s.starts_with("waiting") || s == "queued" || s == "rate limited" => {
            Span::raw("◌ ").fg(Color::Yellow)
        }
        _ => Span::raw("○ ").dim(),
    }
}

pub fn card(project: &str, rows: &[Row], width: usize) -> Vec<Line<'static>> {
    let count = |state: &str| rows.iter().filter(|r| r.state == state).count();
    let mut head = format!(
        "{project} · {} agent{}",
        rows.len(),
        if rows.len() == 1 { "" } else { "s" }
    );
    for state in ["working", "needs you"] {
        if count(state) > 0 {
            head.push_str(&format!(" · {} {state}", count(state)));
        }
    }
    let mut out = vec![Line::from(head).bold()];
    for row in rows {
        let time = match (row.seconds, row.state.as_str()) {
            (Some(s), "working" | "needs you") => crate::transcript::elapsed(s),
            (Some(s), _) => format!("{} ago", crate::transcript::elapsed(s)),
            (None, _) => String::new(),
        };
        let mut spans = vec![
            Span::raw(" "),
            mark(&row.state),
            Span::raw(row.name.clone()).bold(),
            Span::raw(format!(" {}", row.state)),
        ];
        if !time.is_empty() {
            spans.push(Span::raw(format!(" · {time}")).dim());
        }
        out.extend(crate::transcript::wrap(
            Line::from(spans),
            width,
            Span::raw(""),
            Span::raw("   "),
        ));
        let facts: Vec<&str> = [row.model.as_str(), row.place.as_str()]
            .into_iter()
            .filter(|f| !f.is_empty())
            .collect();
        if !facts.is_empty() {
            let line = Line::from(facts.join(" · ")).dim();
            out.extend(crate::transcript::wrap(
                line,
                width,
                Span::raw("   "),
                Span::raw("   "),
            ));
        }
        if !row.latest.is_empty() {
            let line = Line::from(row.latest.clone()).dim();
            out.extend(crate::transcript::capped(
                line,
                width,
                Span::raw("   └ ").dim(),
                1,
            ));
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_card_counts_what_is_working_and_what_needs_the_person() {
        let row = |name: &str, state: &str| Row {
            name: name.into(),
            state: state.into(),
            seconds: Some(42),
            model: "chatgpt/gpt-6.1-sol".into(),
            latest: String::new(),
            place: "tab 1".into(),
        };
        let lines = card(
            "calc",
            &[
                row("lead", "working"),
                row("zero", "needs you"),
                row("equal", "done"),
            ],
            100,
        );
        let text: Vec<String> = lines
            .iter()
            .map(|l| l.spans.iter().map(|s| s.content.as_ref()).collect())
            .collect();
        assert_eq!(text[0], "calc · 3 agents · 1 working · 1 needs you");
        assert_eq!(text[1], " ● lead working · 42s");
        assert_eq!(text[2], "   chatgpt/gpt-6.1-sol · tab 1");
        assert_eq!(text[5], " ✓ equal done · 42s ago");
    }
}
