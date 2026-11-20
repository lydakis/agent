//! `ctrl+f` in a view of rows (Rulebook, Keys and mouse, Find), as
//! **Projects** first had it, made once and given to each view: what is
//! typed keeps the rows that say it, in their title, their name or their
//! words; a section with no match is not drawn; a fold is searched and
//! stands open; a row that holds a match stays, to keep what hangs under
//! it in its place. The pick goes to the first match, and `esc` drops the
//! find and puts the rows and the pick back as they were. The find row is
//! the view's last: a muted `find`, then what was typed.
//!
//! Find reads what the view has already read: it asks the daemon nothing
//! as you type.
use ratatui::crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use ratatui::{
    style::Stylize,
    text::{Line, Span},
};

/// The key, the same in every view.
pub const KEY: &str = "ctrl+f";

/// Whether `key` starts a find: `ctrl+f`, and nothing else held.
pub fn opens(key: &KeyEvent) -> bool {
    key.code == KeyCode::Char('f') && key.modifiers == KeyModifiers::CONTROL
}

/// A find under way: what is typed, and the pick as it was when the find
/// began, which `esc` puts back.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Find<P> {
    pub query: String,
    pub before: P,
}

/// What a key did to a find.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Typed {
    /// What is typed changed: the rows narrow again, the pick to the
    /// first match.
    Changed,
    /// `esc`: the find is dropped, the rows and the pick put back.
    Dropped,
    /// Not the find's: the arrows, `enter` and the rest are the view's.
    Other,
}

impl<P> Find<P> {
    pub fn new(before: P) -> Self {
        Self {
            query: String::new(),
            before,
        }
    }

    /// A key while finding: a letter or a space is typed, backspace takes
    /// one away, `esc` drops the find.
    pub fn key(&mut self, key: &KeyEvent) -> Typed {
        let held = key
            .modifiers
            .intersects(KeyModifiers::CONTROL | KeyModifiers::ALT | KeyModifiers::SUPER);
        match key.code {
            KeyCode::Esc => Typed::Dropped,
            KeyCode::Backspace => {
                self.query.pop();
                Typed::Changed
            }
            KeyCode::Char(c) if !held => {
                self.query.push(c);
                Typed::Changed
            }
            _ => Typed::Other,
        }
    }

    /// Words pasted while finding, as if typed.
    pub fn paste(&mut self, text: &str) {
        (self.query).extend(text.chars().filter(|c| !c.is_control()));
    }

    /// Whether `words` say what is typed, in any case. Nothing typed is
    /// said by every row.
    pub fn says(&self, words: &str) -> bool {
        says(&self.query, words)
    }

    /// Whether anything is typed yet: before it, every row stays.
    pub fn typed(&self) -> bool {
        !self.query.is_empty()
    }
}

/// Whether `words` say `query`, in any case.
pub fn says(query: &str, words: &str) -> bool {
    if query.is_empty() {
        return true;
    }
    match query.chars().any(char::is_uppercase) {
        // A capital typed asks for that case, as a smart-case search does.
        true => words.contains(query),
        false => words.to_lowercase().contains(&query.to_lowercase()),
    }
}

/// One row of a view as find reads it: how deep it hangs under the rows
/// before it, whether it is a section's heading, and the words it says.
#[derive(Clone, Debug, PartialEq)]
pub struct Row {
    pub depth: usize,
    /// A section's heading is never a match itself: it stays while a row
    /// in it does.
    pub section: bool,
    pub words: String,
}

impl Row {
    pub fn new(depth: usize, words: impl Into<String>) -> Self {
        Self {
            depth,
            section: false,
            words: words.into(),
        }
    }

    pub fn section(depth: usize) -> Self {
        Self {
            depth,
            section: true,
            words: String::new(),
        }
    }
}

/// What a find keeps of `rows`: each row that says `query`, and each row
/// or section a kept row hangs under, so the nesting stays. With nothing
/// typed, every row.
pub fn keep(rows: &[Row], query: &str) -> Vec<bool> {
    if query.is_empty() {
        return vec![true; rows.len()];
    }
    let matched: Vec<bool> = (rows.iter())
        .map(|row| !row.section && says(query, &row.words))
        .collect();
    let mut kept = matched.clone();
    for at in (0..rows.len()).filter(|at| matched[*at]) {
        // Up through what it hangs under: the nearest row before it that
        // stands less deep, and so on to the top.
        let mut depth = rows[at].depth;
        for above in (0..at).rev() {
            if depth == 0 {
                break;
            }
            if rows[above].depth < depth {
                depth = rows[above].depth;
                kept[above] = true;
            }
        }
    }
    kept
}

/// The first row that says `query` itself, not one kept only for what it
/// holds: where the pick goes.
pub fn first(rows: &[Row], query: &str) -> Option<usize> {
    (rows.iter()).position(|row| !row.section && says(query, &row.words))
}

/// The find row, the view's last: a muted `find`, then what was typed.
pub fn line(query: &str) -> Line<'static> {
    Line::from(vec![
        Span::raw(" "),
        Span::raw("find ").dim(),
        Span::raw(query.to_owned()),
    ])
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rows() -> Vec<Row> {
        vec![
            Row::section(0),
            Row::new(1, "nightly Run the full test suite"),
            Row::new(1, "pushcheck"),
            Row::section(0),
            Row::new(1, "inbox lead"),
            Row::new(2, "mail down since 11:28"),
            Row::new(2, "ci main: build 412 passed"),
            Row::section(0),
            Row::new(1, "python3 -m http.server 8000"),
        ]
    }

    #[test]
    fn a_row_that_says_it_stays_with_what_it_hangs_under() {
        let rows = rows();
        let kept = |query| -> Vec<usize> {
            (keep(&rows, query).into_iter().enumerate())
                .filter(|(_, kept)| *kept)
                .map(|(at, _)| at)
                .collect()
        };
        // Nothing typed: every row.
        assert_eq!(kept("").len(), rows.len());
        // A match keeps its section; a section with none is not drawn.
        assert_eq!(kept("nightly"), [0, 1]);
        // A match under a row keeps that row too, and its section.
        assert_eq!(kept("build"), [3, 4, 6]);
        // In any case, unless a capital asks for it.
        assert_eq!(kept("MAIL"), Vec::<usize>::new());
        assert_eq!(kept("mail"), [3, 4, 5]);
        assert_eq!(kept("Run"), [0, 1]);
        // A section's own heading is never a match.
        assert_eq!(kept("zzz"), Vec::<usize>::new());
        // The pick goes to the first match, not a row kept for what it
        // holds.
        assert_eq!(first(&rows, "build"), Some(6));
        assert_eq!(first(&rows, "zzz"), None);
    }

    #[test]
    fn typing_narrows_backspace_widens_and_esc_drops() {
        let key = |code| KeyEvent::new(code, KeyModifiers::NONE);
        let mut find = Find::new(Some(3));
        assert!(!find.typed());
        assert_eq!(find.key(&key(KeyCode::Char('m'))), Typed::Changed);
        assert_eq!(find.key(&key(KeyCode::Char('a'))), Typed::Changed);
        assert_eq!(find.query, "ma");
        assert!(find.says("Mail down") && !find.says("ci"));
        assert_eq!(find.key(&key(KeyCode::Backspace)), Typed::Changed);
        assert_eq!(find.query, "m");
        // The arrows and enter are the view's.
        for code in [KeyCode::Up, KeyCode::Down, KeyCode::Enter] {
            assert_eq!(find.key(&key(code)), Typed::Other);
        }
        // A letter held with ctrl is no typing.
        let ctrl_a = KeyEvent::new(KeyCode::Char('a'), KeyModifiers::CONTROL);
        assert_eq!(find.key(&ctrl_a), Typed::Other);
        find.paste("ain\n");
        assert_eq!(find.query, "main");
        assert_eq!(find.key(&key(KeyCode::Esc)), Typed::Dropped);
        // What the pick was is kept for the esc to put back.
        assert_eq!(find.before, Some(3));
        assert!(opens(&KeyEvent::new(
            KeyCode::Char('f'),
            KeyModifiers::CONTROL
        )));
        assert!(!opens(&key(KeyCode::Char('f'))));
    }

    #[test]
    fn the_find_row_is_a_muted_find_then_what_was_typed() {
        let line = line("main");
        let text: String = line.spans.iter().map(|s| s.content.as_ref()).collect();
        assert_eq!(text, " find main");
        assert!(
            line.spans[1]
                .style
                .add_modifier
                .contains(ratatui::style::Modifier::DIM)
        );
    }
}
