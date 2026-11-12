//! Project colors, experimental (`/colors`, `[sonny] project_colors`): in
//! a window that follows other projects' leads, a reply its agent writes
//! because one followed project's agents woke it wears that project's
//! color down its first column, so back-to-back replies read as the same
//! project's or not as updates from several projects interleave. The
//! person's own turns, and those several projects woke at once, stay
//! plain. Each followed project takes a color in the followed leads'
//! order, sorted by name, up to six apart; `[sonny.projects.NAME] color`
//! pins one.
use std::collections::BTreeMap;

use ratatui::{
    style::{Color, Style},
    text::{Line, Span},
};

use crate::screen::Mark;

/// The project the turn is about, and its color's slot.
#[derive(Clone, Debug, PartialEq)]
pub struct Bar {
    pub project: String,
    pub slot: u8,
}

/// Each followed project, once, with its color's slot: those `pinned`
/// take theirs; the rest take the colors no pin took, in the followed
/// leads' order, again from the first past the last.
pub fn palette(followed: &[String], pinned: &BTreeMap<String, String>) -> Vec<(String, u8)> {
    let colors = agent_client::config::PROJECT_COLORS;
    let slot = |color: &str| colors.iter().position(|c| *c == color).map(|n| n as u8);
    let mut projects: Vec<&str> = Vec::new();
    for lead in followed {
        if let Some(project) = crate::project::lead_project(lead)
            && !projects.contains(&project)
        {
            projects.push(project);
        }
    }
    let taken: Vec<u8> = (projects.iter())
        .filter_map(|project| pinned.get(*project).and_then(|color| slot(color)))
        .collect();
    let mut free: Vec<u8> = (0..colors.len() as u8)
        .filter(|slot| !taken.contains(slot))
        .collect();
    if free.is_empty() {
        free = (0..colors.len() as u8).collect();
    }
    let mut next = 0;
    (projects.into_iter())
        .map(|project| {
            let slot = (pinned.get(project).and_then(|color| slot(color))).unwrap_or_else(|| {
                next += 1;
                free[(next - 1) % free.len()]
            });
            (project.to_owned(), slot)
        })
        .collect()
}

/// The one followed project `names` (whom a message between agents is
/// from or about, [`crate::transcript::relayed_names`]) all belong to: a
/// lead or a task of it, by the longest project its name starts with.
/// None when they name several, or one not followed, or none.
pub fn about(names: &[String], palette: &[(String, u8)]) -> Option<(String, u8)> {
    let mut found: Option<&(String, u8)> = None;
    for name in names {
        let project = (palette.iter())
            .filter(|(project, _)| {
                name == project
                    || name
                        .strip_prefix(project.as_str())
                        .is_some_and(|rest| rest.starts_with('.'))
            })
            .max_by_key(|(project, _)| project.len())?;
        match found {
            Some(other) if other != project => return None,
            _ => found = Some(project),
        }
    }
    found.cloned()
}

/// `lines`, about to be written, worn with `bar`: every row from the
/// first not blank on, a blank row only after one that wears it, so the
/// gap above a reply stays bare and two replies read as two. `barred` is
/// whether the row before wore it.
pub fn wear(lines: &[Line<'static>], marks: &mut Vec<(usize, Mark)>, bar: &Bar, barred: &mut bool) {
    for (at, line) in lines.iter().enumerate() {
        *barred = line.width() > 0 || *barred;
        if *barred {
            marks.push((at, Mark::Bar(bar.slot)));
        }
    }
}

/// `line` wearing the bar of the project at `slot` in its first column,
/// as [`crate::look::bar_cell`] draws it on the screen's rows: for rows
/// drawn before they are written, as a reply's open lines are.
pub fn bar_line(line: &mut Line<'static>, slot: u8) {
    let barred = Style::new().bg(crate::look::bar(slot));
    let Some(at) = line.spans.iter().position(|span| !span.content.is_empty()) else {
        line.spans.push(Span::styled(" ", barred));
        return;
    };
    let span = &line.spans[at];
    let Some(first) = span.content.chars().next() else {
        return;
    };
    if !matches!(first, ' ' | '•') || span.style.fg.is_some_and(|fg| fg != Color::Reset) {
        return;
    }
    let style = span.style;
    let rest = span.content[first.len_utf8()..].to_owned();
    let mut spans = vec![Span::styled(" ", style.patch(barred))];
    if !rest.is_empty() {
        spans.push(Span::styled(rest, style));
    }
    line.spans.splice(at..=at, spans);
}

#[cfg(test)]
mod tests {
    use super::*;

    fn leads(names: &[&str]) -> Vec<String> {
        names.iter().map(|name| name.to_string()).collect()
    }

    #[test]
    fn followed_projects_take_colors_in_order_and_a_pin_keeps_its_own() {
        let followed = leads(&["atlas.lead", "blog.lead", "shop.lead", "shop.lead.ops"]);
        let none = BTreeMap::new();
        assert_eq!(
            palette(&followed, &none),
            [("atlas".into(), 0), ("blog".into(), 1), ("shop".into(), 2)]
        );
        // A pin takes its color from the rest, who share what is left.
        let pinned = BTreeMap::from([("shop".to_owned(), "jade".to_owned())]);
        assert_eq!(
            palette(&followed, &pinned),
            [("atlas".into(), 1), ("blog".into(), 2), ("shop".into(), 0)]
        );
        // Past six, colors come round again.
        let seven: Vec<String> = (0..7).map(|n| format!("p{n}.lead")).collect();
        let slots: Vec<u8> = palette(&seven, &none).into_iter().map(|(_, s)| s).collect();
        assert_eq!(slots, [0, 1, 2, 3, 4, 5, 0]);
    }

    #[test]
    fn a_turn_is_about_a_project_only_when_everything_that_woke_it_is_that_project_s() {
        let palette = [
            ("shop".to_owned(), 0),
            ("shop.api".to_owned(), 3),
            ("atlas".to_owned(), 1),
        ];
        let about = |names: &[&str]| about(&leads(names), &palette);
        assert_eq!(about(&["shop.lead"]), Some(("shop".into(), 0)));
        assert_eq!(
            about(&["shop.lead", "shop.refunds"]),
            Some(("shop".into(), 0))
        );
        // The longest project a name starts with is its own.
        assert_eq!(about(&["shop.api.lead"]), Some(("shop.api".into(), 3)));
        // Several projects, one not followed, or none: no bar.
        assert_eq!(about(&["shop.lead", "atlas.lead"]), None);
        assert_eq!(about(&["shop.lead", "home.task"]), None);
        assert_eq!(about(&["shopping.lead"]), None);
        assert_eq!(about(&[]), None);
    }

    fn texts(lines: &[Line]) -> Vec<String> {
        lines.iter().map(|line| line.to_string()).collect()
    }

    fn rows_barred(marks: &[(usize, Mark)], slot: u8) -> Vec<usize> {
        (marks.iter())
            .filter(|(_, mark)| *mark == Mark::Bar(slot))
            .map(|(at, _)| *at)
            .collect()
    }

    #[test]
    fn a_reply_wears_its_bar_from_its_first_row_to_its_last_and_no_name() {
        let lines = vec![
            Line::default(),
            Line::from("• The refunds task finished."),
            Line::default(),
            Line::from("  Two tests were flaky."),
        ];
        let mut marks = vec![(1, Mark::Opens)];
        let bar = Bar {
            project: "shop".into(),
            slot: 2,
        };
        let mut barred = false;
        wear(&lines, &mut marks, &bar, &mut barred);
        assert_eq!(
            texts(&lines),
            [
                "",
                "• The refunds task finished.",
                "",
                "  Two tests were flaky."
            ]
        );
        // The gap above is bare; the blank between its paragraphs is not.
        assert_eq!(rows_barred(&marks, 2), [1, 2, 3]);
        assert!(barred);
        // Its next message in the same turn wears it on from the gap.
        let more = vec![Line::default(), Line::from("• And CI is green.")];
        let mut marks = Vec::new();
        wear(&more, &mut marks, &bar, &mut barred);
        assert_eq!(rows_barred(&marks, 2), [0, 1]);
    }

    #[test]
    fn an_open_row_wears_the_bar_in_place_of_its_bullet_or_blank_only() {
        let mut line = Line::from("• Working on it");
        bar_line(&mut line, 1);
        assert_eq!(line.to_string(), "  Working on it");
        assert_eq!(line.spans[0].style.bg, Some(crate::look::bar(1)));
        let mut blank = Line::default();
        bar_line(&mut blank, 1);
        assert_eq!(blank.spans[0].style.bg, Some(crate::look::bar(1)));
        let mut card = Line::from("┃ Which one?");
        bar_line(&mut card, 1);
        assert_eq!(card.to_string(), "┃ Which one?");
        let mut dot = Line::from(Span::styled("• ran tests", Style::new().fg(Color::Green)));
        bar_line(&mut dot, 1);
        assert_eq!(dot.to_string(), "• ran tests");
    }
}
