//! `ctrl+f` in a view of rows (Rulebook, Keys and mouse, Find), as
//! **Projects** first had it, made once and given to each view: the key,
//! what is typed and what `esc` puts back, whether a row's words say what
//! is typed, and the find row. Each view keeps the rows that say it and
//! what they hang under in its own nesting (Projects' folders, `/status`'s
//! makers, `/background`'s sections); a section with no match is not
//! drawn; a fold is searched and stands open. The pick goes to the first
//! match, and `esc` drops the find and puts the rows and the pick back as
//! they were. The find row is the view's last: a muted `find`, then what
//! was typed.
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

/// The find row, the view's last: a muted `find`, then what was typed.
pub fn line(query: &str) -> Line<'static> {
    Line::from(vec![Span::raw("find ").dim(), Span::raw(query.to_owned())])
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn words_say_it_in_any_case_unless_a_capital_asks_for_one() {
        assert!(says("", "anything"));
        assert!(says("mail", "Mail down since 11:28"));
        assert!(!says("MAIL", "Mail down since 11:28"));
        assert!(says("Run", "nightly Run the full test suite"));
        assert!(!says("Run", "nightly run the full test suite"));
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
        assert_eq!(text, "find main");
        assert!(
            line.spans[0]
                .style
                .add_modifier
                .contains(ratatui::style::Modifier::DIM)
        );
    }
}
