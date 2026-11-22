//! `ctrl+f` in a view (Rulebook, Keys and mouse, Find), as **Projects**
//! first had it, made once and given to each view: the key, what is typed
//! and what `esc` puts back, whether a row's words say what is typed, and
//! the find field. Each view keeps the rows that say it and what they hang
//! under in its own nesting (Projects' folders, `/status`'s makers,
//! `/background`'s sections); a section with no match is not drawn; a fold
//! is searched and stands open. The pick goes to the first match, and
//! `esc` drops the find and puts the rows and the pick back as they were.
//!
//! The field has one place (21 Nov): the row under the view's title row,
//! which every view keeps empty at rest, so nothing moves when it opens or
//! as the rows narrow. It is drawn here, once, for every view: one row on
//! the band, a muted `find`, what was typed, the terminal's own cursor,
//! and at its right end how many rows are left of how many. The
//! conversation's own find, above the composer, is drawn by it too.
//!
//! Find reads what the view has already read: it asks the daemon nothing
//! as you type.
use ratatui::crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use ratatui::{
    buffer::Buffer,
    layout::Rect,
    style::{Style, Stylize},
    text::{Line, Span},
};
use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

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
    /// The find's key, which changed nothing: backspace with nothing
    /// typed.
    Held,
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
            KeyCode::Backspace => match self.query.pop() {
                Some(_) => Typed::Changed,
                None => Typed::Held,
            },
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

/// The find field as a view has it now: what was typed, what its right
/// end says, and whether it finds in rows or steps through text.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Field {
    pub query: String,
    /// Muted at the right end: how many rows are left of how many,
    /// `4 of 14`, or in text which place of how many, `4 of 7`.
    pub said: String,
    /// A view of text, which steps: `type to find`, not `type to narrow`.
    pub text: bool,
}

impl Field {
    /// In a view of rows: `left` rows say what is typed, of `of`.
    pub fn rows(query: &str, left: usize, of: usize) -> Self {
        Self {
            query: query.to_owned(),
            said: format!("{left} of {of}"),
            text: false,
        }
    }

    /// In a view of text: the place shown, `at` of `of`, once something
    /// is typed.
    pub fn text(query: &str, place: Option<(usize, usize)>) -> Self {
        let said = match place {
            Some((at, of)) if !query.is_empty() => format!("{at} of {of}"),
            _ => String::new(),
        };
        Self {
            query: query.to_owned(),
            said,
            text: true,
        }
    }

    /// What the field shows after the cursor with nothing typed.
    fn prompt(&self) -> &'static str {
        match self.text {
            true => "type to find",
            false => "type to narrow",
        }
    }
}

/// The find field drawn on `row`, from its first column to its edge, all
/// of it on the band: a muted `find` (bold with `NO_COLOR`), what was
/// typed, the cursor, a muted prompt after it with nothing typed, and the
/// count at the right end. Where the terminal's cursor goes.
pub fn draw(field: &Field, row: Rect, buf: &mut Buffer) -> (u16, u16) {
    let row = Rect { height: 1, ..row };
    let row = row.intersection(buf.area);
    if row.width == 0 || row.height == 0 {
        return (row.x, row.y);
    }
    let band = Style::new().bg(crate::look::BAND);
    buf.set_style(row, band);
    for x in row.left()..row.right() {
        buf[(x, row.y)].set_symbol(" ");
    }
    let width = row.width as usize;
    let label = match crate::look::no_color() {
        true => Span::raw("find").bold(),
        false => Span::raw("find").dim(),
    };
    let lead = "find ".width();
    // The count keeps its place while the words leave the cursor room;
    // too narrow for both, it gives way.
    let said = field.said.width();
    let said = match lead + 1 + 2 + said <= width && said > 0 {
        true => said,
        false => 0,
    };
    let room = width.saturating_sub(lead + 1 + if said > 0 { said + 2 } else { 0 });
    // Typed past the room, its end shows: where the next letter goes.
    let typed = end_within(&field.query, room);
    let cursor = (row.x + (lead + typed.width()).min(width - 1) as u16, row.y);
    let mut spans = vec![label, Span::raw(" "), Span::raw(typed.to_owned())];
    if field.query.is_empty() {
        let prompt = crate::transcript::cut(field.prompt(), room);
        spans.push(Span::raw(prompt).dim());
    }
    buf.set_line(row.x, row.y, &Line::from(spans), row.width);
    if said > 0 {
        let x = row.x + (width - 1 - said) as u16;
        buf.set_span(x, row.y, &Span::raw(field.said.clone()).dim(), said as u16);
    }
    cursor
}

/// The longest end of `text` no wider than `room`: what shows of a query
/// typed past its field. Its width is measured a room's worth at a time, not
/// again from each letter on, so a long paste costs its length, not its
/// length squared.
fn end_within(text: &str, room: usize) -> &str {
    if text.width() <= room {
        return text;
    }
    // The letters from the end whose own widths fill the room: what shows,
    // or nearly, since letters together (an emoji and its selector, a
    // joined family) can draw wider or narrower than each alone.
    let mut start = text.len();
    let mut filled = 0;
    for (at, c) in text.char_indices().rev() {
        filled += c.width().unwrap_or(0);
        if filled > room {
            break;
        }
        start = at;
    }
    while text[start..].width() > room {
        start += text[start..].chars().next().map_or(0, char::len_utf8);
    }
    while let Some((at, _)) = text[..start].char_indices().next_back()
        && text[at..].width() <= room
    {
        start = at;
    }
    &text[start..]
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
    fn backspace_with_nothing_typed_does_nothing() {
        let mut find = Find::new(());
        let back = KeyEvent::new(KeyCode::Backspace, KeyModifiers::NONE);
        assert_eq!(find.key(&back), Typed::Held);
        find.paste("a");
        assert_eq!(find.key(&back), Typed::Changed);
        assert_eq!(find.key(&back), Typed::Held);
    }

    /// The row as text, and each cell's style.
    fn drawn(field: &Field, width: u16) -> (String, Vec<Style>, (u16, u16)) {
        let mut buf = Buffer::empty(Rect::new(0, 0, width, 1));
        let cursor = draw(field, Rect::new(0, 0, width, 1), &mut buf);
        let text = (0..width)
            .map(|x| buf[(x, 0)].symbol().to_owned())
            .collect();
        let styles = (0..width).map(|x| buf[(x, 0)].style()).collect();
        (text, styles, cursor)
    }

    #[test]
    fn the_field_is_a_muted_find_the_words_the_cursor_and_the_count_on_the_band() {
        use ratatui::style::Modifier;
        let (text, styles, cursor) = drawn(&Field::rows("main", 4, 14), 30);
        assert_eq!(text, format!("find main{}4 of 14 ", " ".repeat(13)));
        assert_eq!(text.width(), 30);
        // The cursor after what was typed.
        assert_eq!(cursor, (9, 0));
        // All of it on the band, edge to edge.
        assert!(styles.iter().all(|s| s.bg == Some(crate::look::BAND)));
        // `find` and the count muted; the words in ink.
        assert!(styles[0].add_modifier.contains(Modifier::DIM));
        assert!(!styles[5].add_modifier.contains(Modifier::DIM));
        assert!(styles[25].add_modifier.contains(Modifier::DIM));
    }

    #[test]
    fn with_nothing_typed_a_muted_prompt_follows_the_cursor() {
        use ratatui::style::Modifier;
        let (text, styles, cursor) = drawn(&Field::rows("", 14, 14), 32);
        assert_eq!(
            text,
            format!("find type to narrow{}14 of 14 ", " ".repeat(4))
        );
        assert_eq!(cursor, (5, 0));
        assert!(styles[5].add_modifier.contains(Modifier::DIM));
        let (text, _, cursor) = drawn(&Field::text("", None), 24);
        assert_eq!(text.trim_end(), "find type to find");
        assert_eq!(cursor, (5, 0));
        // In text, which place of how many, once typed.
        let (text, _, _) = drawn(&Field::text("cents", Some((4, 7))), 24);
        assert_eq!(text, "find cents       4 of 7 ");
        // Nothing found: none of all.
        let (text, _, _) = drawn(&Field::rows("zz", 0, 14), 24);
        assert_eq!(text, "find zz         0 of 14 ");
    }

    #[test]
    fn typed_past_the_room_its_end_shows_and_the_count_stays() {
        let (text, _, cursor) = drawn(&Field::rows("a long query typed", 1, 9), 20);
        assert_eq!(text, "find  typed  1 of 9 ");
        assert_eq!(cursor.0, 11);
        // Too narrow for the count: it gives way, the words stay.
        let (text, _, _) = drawn(&Field::rows("ab", 1, 9), 10);
        assert_eq!(text, "find ab   ");
    }

    /// What shows of a long query is what dropping a letter at a time from
    /// its start until it fits showed, for any letters, and a long paste
    /// costs its length, not its length squared.
    #[test]
    fn a_long_query_shows_its_end_as_dropping_a_letter_at_a_time_did() {
        fn dropping(text: &str, room: usize) -> &str {
            let mut typed = text;
            while typed.width() > room {
                let mut chars = typed.chars();
                chars.next();
                typed = chars.as_str();
            }
            typed
        }
        let pieces = [
            "plain words ",
            "漢字かな",
            "e\u{301}",
            "👨\u{200d}👩\u{200d}👧",
            "☺\u{fe0f}",
            "🇬🇷",
            "\u{200b}",
            "x",
        ];
        for length in 0..60 {
            for room in [0, 1, 2, 3, 7, 20] {
                for shift in 0..pieces.len() {
                    let text: String = (0..length)
                        .map(|at| pieces[(at * 3 + shift) % pieces.len()])
                        .collect();
                    assert_eq!(
                        end_within(&text, room),
                        dropping(&text, room),
                        "{text:?} in {room}"
                    );
                }
            }
        }
        // A 1 MiB paste in a 100-column field: the old way measured it
        // about half a million times, minutes of work.
        let pasted: String = "find me 漢字 ".chars().cycle().take(1 << 20).collect();
        let began = std::time::Instant::now();
        let shown = end_within(&pasted, 100);
        assert!(
            shown.width() <= 100 && shown.width() >= 99,
            "{}",
            shown.width()
        );
        assert!(pasted.ends_with(shown));
        assert!(
            began.elapsed() < std::time::Duration::from_secs(2),
            "{:?}",
            began.elapsed()
        );
    }
}
