//! One list over the composer for every pick, as Codex draws all its
//! popups with one component: at most `ROWS` rows scrolled to keep the pick
//! in view, a blank row between it and the conversation, the pick in
//! reverse video, and `•` before what is in effect now.
use ratatui::{
    style::Stylize,
    text::{Line, Span},
};
use unicode_width::UnicodeWidthStr;

/// Rows shown at most, as Codex's `MAX_POPUP_ROWS`.
pub const ROWS: usize = 8;

/// One row: its name, what it is (muted), and whether it is in effect.
pub struct Item {
    pub name: String,
    pub about: String,
    pub current: bool,
}

impl Item {
    pub fn new(name: impl Into<String>, about: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            about: about.into(),
            current: false,
        }
    }

    pub fn current(mut self, current: bool) -> Self {
        self.current = current;
        self
    }
}

/// `items` with `pick` picked, `width` wide: the blank row, then up to
/// `ROWS` rows around the pick.
pub fn lines(items: &[Item], pick: usize, width: usize) -> Vec<Line<'static>> {
    let shown = items.len().min(ROWS);
    let first = (pick + 1)
        .saturating_sub(shown)
        .min(items.len().saturating_sub(shown));
    let wide = items
        .iter()
        .map(|item| item.name.width())
        .max()
        .unwrap_or(0);
    let mut out = vec![Line::default()];
    for (at, item) in items.iter().enumerate().skip(first).take(shown) {
        let mark = match item.current {
            true => "  • ",
            false => "    ",
        };
        let name = format!("{:<wide$}", item.name);
        let room = width.saturating_sub(mark.width() + wide + 4);
        let about = match item.about.is_empty() {
            true => String::new(),
            false => format!("  {}", crate::transcript::cut(&item.about, room)),
        };
        out.push(match at == pick {
            true => Line::from(vec![
                Span::raw(mark),
                Span::raw(format!("{name}{about} ")).reversed(),
            ]),
            false => Line::from(vec![
                Span::raw(mark),
                Span::raw(name),
                Span::raw(about).dim(),
            ]),
        });
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_long_list_shows_eight_rows_around_the_pick_under_a_blank_one() {
        let items: Vec<Item> = (0..21)
            .map(|n| Item::new(format!("/c{n}"), "does").current(n == 3))
            .collect();
        let text = |lines: Vec<Line>| -> Vec<String> {
            lines.iter().map(|line| line.to_string()).collect()
        };
        let top = text(lines(&items, 3, 40));
        assert_eq!(top.len(), 9);
        assert_eq!(top[0], "");
        assert_eq!(top[1], "    /c0   does");
        assert_eq!(top[4], "  • /c3   does ");
        let end = text(lines(&items, 20, 40));
        assert_eq!(end[8], "    /c20  does ");
        assert_eq!(end[1], "    /c13  does");
    }
}
