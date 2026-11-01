//! One list over the composer for every pick, as Codex draws all its
//! popups with one component: at most `ROWS` rows scrolled to keep the pick
//! in view, a blank row between it and the conversation, the pick in
//! reverse video, `•` before what is in effect now, and the letters typed
//! to narrow it in bold, as Codex marks them.
use ratatui::{
    style::Stylize,
    text::{Line, Span},
};
use unicode_width::UnicodeWidthStr;

/// Rows shown at most, as Codex's `MAX_POPUP_ROWS`.
pub const ROWS: usize = 8;

/// One row: its name, what it is (muted), whether it is in effect, and
/// where in its name the letters typed are.
pub struct Item {
    pub name: String,
    pub about: String,
    pub current: bool,
    pub found: Vec<usize>,
}

impl Item {
    pub fn new(name: impl Into<String>, about: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            about: about.into(),
            current: false,
            found: Vec::new(),
        }
    }

    /// The chars of its name that matched what was typed.
    pub fn found(mut self, found: &[usize]) -> Self {
        self.found = found.to_vec();
        self
    }

    pub fn current(mut self, current: bool) -> Self {
        self.current = current;
        self
    }
}

/// `items` with `pick` picked, `width` wide: the blank row, then up to
/// `ROWS` rows around the pick, fewer when only `room` rows are free above
/// the composer, so the pick is always among those drawn.
pub fn lines(items: &[Item], pick: usize, width: usize, room: usize) -> Vec<Line<'static>> {
    let shown = items.len().min(ROWS).min(room.saturating_sub(1));
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
        let picked = at == pick;
        let mut row = vec![Span::raw(mark)];
        row.extend(marked(&name, &item.found, picked));
        row.push(match picked {
            true => Span::raw(format!("{about} ")).reversed(),
            false => Span::raw(about).dim(),
        });
        out.push(Line::from(row));
    }
    out
}

/// `line`, a row of a list in the pane, drawn as the picker draws its
/// pick: reversed from its first mark to a column past its last word, its
/// muted words at the text's own weight, its lead left as it was.
pub fn pick(line: &mut Line<'static>) {
    let mut leading = true;
    for span in &mut line.spans {
        if leading && span.content.trim().is_empty() {
            continue;
        }
        leading = false;
        span.style = span.style.remove_modifier(ratatui::style::Modifier::DIM);
        span.style = span.style.add_modifier(ratatui::style::Modifier::REVERSED);
    }
    if !leading {
        line.spans.push(Span::raw(" ").reversed());
    }
}

/// `name` in runs, the chars at `found` bold, all reversed when picked.
fn marked(name: &str, found: &[usize], picked: bool) -> Vec<Span<'static>> {
    let mut runs: Vec<(String, bool)> = Vec::new();
    for (at, c) in name.chars().enumerate() {
        let bold = found.contains(&at);
        match runs.last_mut() {
            Some((run, was)) if *was == bold => run.push(c),
            _ => runs.push((c.to_string(), bold)),
        }
    }
    (runs.into_iter())
        .map(|(run, bold)| {
            let span = Span::raw(run);
            let span = if bold { span.bold() } else { span };
            if picked { span.reversed() } else { span }
        })
        .collect()
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
        let top = text(lines(&items, 3, 40, 20));
        assert_eq!(top.len(), 9);
        assert_eq!(top[0], "");
        assert_eq!(top[1], "    /c0   does");
        assert_eq!(top[4], "  • /c3   does ");
        let end = text(lines(&items, 20, 40, 20));
        assert_eq!(end[8], "    /c20  does ");
        assert_eq!(end[1], "    /c13  does");
        // With four rows free above the composer, three items show, the
        // pick among them.
        let short = text(lines(&items, 20, 40, 4));
        assert_eq!(short.len(), 4);
        assert_eq!(short[3], "    /c20  does ");
    }

    #[test]
    fn the_letters_typed_are_bold_and_the_rest_plain() {
        use ratatui::style::Modifier;
        let items = [
            Item::new("/commons", "read").found(&[1, 2, 3]),
            Item::new("/compact", "go"),
        ];
        let rows = lines(&items, 1, 40, 20);
        let bold: Vec<(String, bool)> = (rows[1].spans.iter())
            .map(|s| {
                (
                    s.content.to_string(),
                    s.style.add_modifier.contains(Modifier::BOLD),
                )
            })
            .collect();
        assert_eq!(bold[1], ("/".into(), false));
        assert_eq!(bold[2], ("com".into(), true));
        assert_eq!(bold[3], ("mons".into(), false));
        assert_eq!(rows[1].to_string(), "    /commons  read");
        assert_eq!(rows[2].to_string(), "    /compact  go ");
    }
}
