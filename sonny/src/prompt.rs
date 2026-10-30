//! The composer's text and where the cursor is in it, laid out the way
//! the composer draws it, so moving up and down follows the wrapped rows.
//!
//! The cursor moves, and rows break, between graphemes: what the terminal
//! draws as one character, such as an emoji made of several.
//!
//! The full screen selects text with the mouse, as Codex's composer does:
//! typing replaces the selection, and the deletions take it out.
use ratatui::{
    style::{Color, Modifier, Style},
    text::Span,
};
use std::ops::{Deref, Range};
use unicode_segmentation::UnicodeSegmentation;
use unicode_width::UnicodeWidthStr;

/// What the composer's rows start with: the prompt mark on the first,
/// two spaces on the rest.
pub const LEAD: usize = 2;

#[derive(Default)]
pub struct Prompt {
    text: String,
    /// The cursor, a byte offset on a grapheme boundary.
    at: usize,
    /// The column up and down aim for, kept while they alone move the
    /// cursor, so a short row on the way does not pull it in.
    goal: Option<usize>,
    /// What the last word or line deletion took, for Ctrl-Y to put back.
    killed: String,
    /// Where a selection began; it runs to the cursor.
    anchor: Option<usize>,
}

/// One drawn row: the bytes of the text it shows.
#[derive(Debug, PartialEq)]
pub struct Row {
    pub start: usize,
    pub end: usize,
}

impl Deref for Prompt {
    type Target = str;
    fn deref(&self) -> &str {
        &self.text
    }
}

impl Prompt {
    pub fn clear(&mut self) {
        self.goal = None;
        self.anchor = None;
        self.text.clear();
        self.at = 0;
    }

    /// Replace the text, with the cursor at its end.
    pub fn set(&mut self, text: &str) {
        self.goal = None;
        self.anchor = None;
        self.text = text.to_owned();
        self.at = self.text.len();
    }

    /// The selected bytes, when any are.
    pub fn selection(&self) -> Option<Range<usize>> {
        let anchor = self.anchor?;
        let range = anchor.min(self.at)..anchor.max(self.at);
        (!range.is_empty()).then_some(range)
    }

    /// Select from `anchor` to `at`, the cursor's end; both are put on
    /// grapheme boundaries.
    pub fn select(&mut self, anchor: usize, at: usize) {
        self.goal = None;
        self.at = anchor.min(self.text.len());
        self.settle();
        let anchor = self.at;
        self.at = at.min(self.text.len());
        self.settle();
        self.anchor = Some(anchor);
    }

    /// Let go of the selection; whether there was one.
    pub fn unselect(&mut self) -> bool {
        let had = self.selection().is_some();
        self.anchor = None;
        had
    }

    /// Take the selection out, the cursor where it began; whether there
    /// was one.
    fn cut(&mut self) -> bool {
        let Some(range) = self.selection() else {
            self.anchor = None;
            return false;
        };
        self.anchor = None;
        self.at = range.start;
        self.text.drain(range);
        self.settle();
        true
    }

    /// Kill the selection, kept for Ctrl-Y; whether there was one.
    fn kill_selection(&mut self) -> bool {
        match self.selection() {
            Some(range) => {
                self.anchor = None;
                self.kill(range);
                true
            }
            None => {
                self.anchor = None;
                false
            }
        }
    }

    /// Typing over a selection replaces it.
    pub fn insert(&mut self, text: &str) {
        self.goal = None;
        self.cut();
        self.text.insert_str(self.at, text);
        self.at += text.len();
        self.settle();
    }

    pub fn backspace(&mut self) {
        self.goal = None;
        if self.cut() {
            return;
        }
        let start = self.before(self.at);
        self.text.drain(start..self.at);
        self.at = start;
        self.settle();
    }

    pub fn delete(&mut self) {
        self.goal = None;
        if self.cut() {
            return;
        }
        let end = self.after(self.at);
        self.text.drain(self.at..end);
        self.settle();
    }

    /// Left from a selection goes to its start.
    pub fn left(&mut self) {
        self.goal = None;
        self.at = match self.selection() {
            Some(range) => range.start,
            None => self.before(self.at),
        };
        self.anchor = None;
    }

    /// Right from a selection goes to its end.
    pub fn right(&mut self) {
        self.goal = None;
        self.at = match self.selection() {
            Some(range) => range.end,
            None => self.after(self.at),
        };
        self.anchor = None;
    }

    /// The start of the cursor's line.
    pub fn home(&mut self) {
        self.goal = None;
        self.anchor = None;
        self.at = self.line_start();
        self.settle();
    }

    /// The end of the cursor's line.
    pub fn end(&mut self) {
        self.goal = None;
        self.anchor = None;
        self.at = self.line_end();
    }

    pub fn word_left(&mut self) {
        self.goal = None;
        self.anchor = None;
        self.at = self.word_start();
        self.settle();
    }

    pub fn word_right(&mut self) {
        self.goal = None;
        self.anchor = None;
        let rest = &self.text[self.at..];
        let spaces = rest.len() - rest.trim_start().len();
        let word = rest[spaces..]
            .find(char::is_whitespace)
            .unwrap_or(rest.len() - spaces);
        self.at += spaces + word;
        self.settle();
    }

    /// Delete back to the start of the word before the cursor.
    pub fn delete_word(&mut self) {
        self.goal = None;
        if self.kill_selection() {
            return;
        }
        let start = self.word_start();
        self.kill(start..self.at);
    }

    /// Delete to the end of the word after the cursor.
    pub fn delete_word_forward(&mut self) {
        self.goal = None;
        if self.kill_selection() {
            return;
        }
        let start = self.at;
        self.word_right();
        self.kill(start..self.at);
    }

    /// What Ctrl-Y would put back.
    pub fn killed(&self) -> &str {
        &self.killed
    }

    /// Put back what the last word or line deletion took, at the cursor.
    pub fn yank(&mut self) {
        let killed = self.killed.clone();
        self.insert(&killed);
    }

    /// Take out `range`, keeping it for [`Self::yank`] when there is any,
    /// and leave the cursor where it began.
    fn kill(&mut self, range: std::ops::Range<usize>) {
        if !range.is_empty() {
            self.killed = self.text[range.clone()].to_owned();
        }
        self.at = range.start;
        self.text.drain(range);
        self.settle();
    }

    /// Delete back to the start of the line, or the line break before the
    /// cursor when it is already there.
    pub fn delete_to_start(&mut self) {
        self.goal = None;
        if self.kill_selection() {
            return;
        }
        let start = match self.line_start() {
            start if start == self.at => self.before(self.at),
            start => start,
        };
        self.kill(start..self.at);
    }

    /// Delete to the end of the line, or the line break after the cursor
    /// when it is already there.
    pub fn delete_to_end(&mut self) {
        self.goal = None;
        if self.kill_selection() {
            return;
        }
        let end = match self.line_end() {
            end if end == self.at => self.after(self.at),
            end => end,
        };
        self.kill(self.at..end);
    }

    /// Put the cursor back on a grapheme boundary, the next one, after an
    /// edit joined the graphemes around it (a flag's two halves) or a
    /// move stopped inside one.
    fn settle(&mut self) {
        self.at = self
            .text
            .grapheme_indices(true)
            .map(|(i, _)| i)
            .find(|&i| i >= self.at)
            .unwrap_or(self.text.len());
    }

    /// Move a drawn row up; false when the cursor is on the first.
    pub fn up(&mut self, width: usize) -> bool {
        self.vertical(width, false)
    }

    /// Move a drawn row down; false when the cursor is on the last.
    pub fn down(&mut self, width: usize) -> bool {
        self.vertical(width, true)
    }

    fn vertical(&mut self, width: usize, down: bool) -> bool {
        self.anchor = None;
        let rows = self.layout(width);
        let (row, col) = self.cursor(&rows);
        let col = *self.goal.get_or_insert(col);
        let target = match down {
            true if row + 1 < rows.len() => row + 1,
            false if row > 0 => row - 1,
            _ => return false,
        };
        self.at = self.spot(&rows[target], col);
        true
    }

    /// Where a drawn `row` has column `col`, the lead counted.
    pub fn spot(&self, row: &Row, col: usize) -> usize {
        let Row { start, end } = *row;
        // The last column at or before `col`, but never past the row's
        // end, where the next row starts.
        let mut at = start;
        let mut cols = LEAD;
        for (i, g) in self.text[start..end].grapheme_indices(true) {
            let w = g.width();
            if cols + w > col {
                break;
            }
            cols += w;
            at = start + i + g.len();
        }
        if at == end && end < self.text.len() && !self.text[end..].starts_with('\n') {
            at = self.before(end);
        }
        at.max(start)
    }

    /// The word at `at`, or the spaces there, for a double click.
    pub fn word_at(&self, at: usize) -> Range<usize> {
        let space = |c: char| c.is_whitespace();
        let on_space = self.text[at..].chars().next().is_some_and(space);
        let start = self.text[..at]
            .char_indices()
            .rev()
            .find(|&(_, c)| space(c) != on_space || c == '\n')
            .map_or(0, |(i, c)| i + c.len_utf8());
        let end = self.text[at..]
            .char_indices()
            .find(|&(_, c)| space(c) != on_space || c == '\n')
            .map_or(self.text.len(), |(i, _)| at + i);
        start..end
    }

    /// The line at `at`, for a triple click.
    pub fn line_at(&self, at: usize) -> Range<usize> {
        let start = self.text[..at].rfind('\n').map_or(0, |i| i + 1);
        let end = self.text[at..]
            .find('\n')
            .map_or(self.text.len(), |i| at + i);
        start..end
    }

    /// The text as the composer draws it at `width`: each line wrapped at
    /// word breaks into rows of `width - 1` columns, lead included.
    pub fn layout(&self, width: usize) -> Vec<Row> {
        let room = width.saturating_sub(1 + LEAD).max(8);
        let mut rows = Vec::new();
        let mut line_start = 0;
        for line in self.text.split('\n') {
            let mut start = line_start;
            let mut used = 0;
            let mut word_start = line_start;
            for word in words(line) {
                let wide = word.trim_end().width();
                if used + wide > room && used > 0 {
                    rows.push(Row {
                        start,
                        end: word_start,
                    });
                    start = word_start;
                    used = 0;
                }
                // A word wider than a row is cut where the row ends.
                for (i, g) in word.grapheme_indices(true) {
                    let w = g.width();
                    if used + w > room && used > 0 {
                        let at = word_start + i;
                        rows.push(Row { start, end: at });
                        start = at;
                        used = 0;
                    }
                    used += w;
                }
                word_start += word.len();
            }
            rows.push(Row {
                start,
                end: line_start + line.len(),
            });
            line_start += line.len() + 1;
        }
        rows
    }

    /// The cursor's drawn row, and its column counting the lead.
    pub fn cursor(&self, rows: &[Row]) -> (usize, usize) {
        let row = rows
            .iter()
            .rposition(|row| row.start <= self.at && self.at <= row.end)
            .unwrap_or(0);
        let col = LEAD + self.text[rows[row].start..self.at].width();
        (row, col)
    }

    fn before(&self, at: usize) -> usize {
        self.text[..at]
            .grapheme_indices(true)
            .next_back()
            .map_or(0, |(i, _)| i)
    }

    fn after(&self, at: usize) -> usize {
        self.text[at..]
            .graphemes(true)
            .next()
            .map_or(at, |g| at + g.len())
    }

    fn line_start(&self) -> usize {
        self.text[..self.at].rfind('\n').map_or(0, |i| i + 1)
    }

    fn line_end(&self) -> usize {
        self.text[self.at..]
            .find('\n')
            .map_or(self.text.len(), |i| self.at + i)
    }

    fn word_start(&self) -> usize {
        let before = self.text[..self.at].trim_end();
        before.rfind(char::is_whitespace).map_or(0, |i| {
            i + before[i..].chars().next().map_or(1, char::len_utf8)
        })
    }
}

/// A line's words, each with the spaces after it.
fn words(line: &str) -> impl Iterator<Item = &str> {
    let mut rest = line;
    std::iter::from_fn(move || {
        if rest.is_empty() {
            return None;
        }
        let word = rest.find(char::is_whitespace).unwrap_or(rest.len());
        let spaces = rest[word..]
            .find(|c: char| !c.is_whitespace())
            .unwrap_or(rest.len() - word);
        let (head, tail) = rest.split_at(word + spaces);
        rest = tail;
        Some(head)
    })
}

/// How the composer draws what is typed: Markdown's marks kept and dim,
/// what they mark styled as the conversation draws it (bold, italic,
/// struck, `code` in code's colour, a `#` line as a heading), so the
/// person sees what they will send and sends the plain text they typed.
/// Each styled piece as a byte range; the rest is plain. A slash command
/// and a `!` command are not Markdown.
pub fn marks(text: &str) -> Vec<(Range<usize>, Style)> {
    let mut marked = Vec::new();
    if text.starts_with('/') || text.starts_with('!') {
        return marked;
    }
    let mut start = 0;
    for line in text.split('\n') {
        let hashes = line.len() - line.trim_start_matches('#').len();
        match (1..=6).contains(&hashes) && line[hashes..].starts_with(' ') {
            true => {
                let heading = Style::new().add_modifier(Modifier::BOLD);
                marked.push((start..start + hashes + 1, dim()));
                inline(line, hashes + 1, start, heading, &mut marked);
            }
            false => inline(line, 0, start, Style::new(), &mut marked),
        }
        start += line.len() + 1;
    }
    marked
}

fn dim() -> Style {
    Style::new().add_modifier(Modifier::DIM)
}

/// The inline marks of `line` from `from`, at `offset` in the text, over
/// `base`: as `transcript::inline` reads them, a mark closed on its line.
fn inline(
    line: &str,
    from: usize,
    offset: usize,
    base: Style,
    marked: &mut Vec<(Range<usize>, Style)>,
) {
    let pairs: [(&str, Style, bool); 6] = [
        ("`", base.fg(Color::Cyan), false),
        ("**", base.add_modifier(Modifier::BOLD), true),
        ("__", base.add_modifier(Modifier::BOLD), true),
        ("~~", base.add_modifier(Modifier::CROSSED_OUT), true),
        ("*", base.add_modifier(Modifier::ITALIC), true),
        ("_", base.add_modifier(Modifier::ITALIC), true),
    ];
    let mut at = from;
    let mut plain = from;
    while at < line.len() {
        let rest = &line[at..];
        let found = pairs.iter().find_map(|(mark, style, nested)| {
            let after = rest.strip_prefix(mark)?;
            // `_` and `*` mark emphasis only around a word, not in
            // snake_case or 2*3*4.
            if matches!(*mark, "*" | "_")
                && (after.starts_with(' ')
                    || line[..at].chars().last().is_some_and(char::is_alphanumeric))
            {
                return None;
            }
            let end = after.find(mark)?;
            (end > 0).then_some((mark.len(), end, *style, *nested))
        });
        let Some((mark, end, style, nested)) = found else {
            at += rest.chars().next().map_or(1, char::len_utf8);
            continue;
        };
        if plain < at && base != Style::new() {
            marked.push((offset + plain..offset + at, base));
        }
        let (inner, close) = (at + mark, at + mark + end);
        marked.push((offset + at..offset + inner, dim()));
        match nested {
            true => inline(&line[..close], inner, offset, style, marked),
            false => marked.push((offset + inner..offset + close, style)),
        }
        marked.push((offset + close..offset + close + mark, dim()));
        at = close + mark;
        plain = at;
    }
    if plain < line.len() && base != Style::new() {
        marked.push((offset + plain..offset + line.len(), base));
    }
}

/// The text `range` of `text` as spans, styled where `marked` says, and
/// reversed where it is `selected`.
pub fn spans(
    text: &str,
    range: Range<usize>,
    marked: &[(Range<usize>, Style)],
    selected: Option<&Range<usize>>,
) -> Vec<Span<'static>> {
    // The marks on this row alone, in their order: a later one is laid
    // over an earlier.
    let marked: Vec<_> = (marked.iter())
        .filter(|(piece, _)| piece.start < range.end && range.start < piece.end)
        .collect();
    let mut cuts = vec![range.start, range.end];
    for (piece, _) in &marked {
        cuts.extend([piece.start, piece.end]);
    }
    if let Some(selected) = selected {
        cuts.extend([selected.start, selected.end]);
    }
    cuts.retain(|cut| range.contains(cut) || *cut == range.end);
    cuts.sort_unstable();
    cuts.dedup();
    (cuts.windows(2))
        .map(|cut| {
            let (from, to) = (cut[0], cut[1]);
            let mut style = (marked.iter())
                .filter(|(piece, _)| piece.start <= from && to <= piece.end)
                .map(|(_, style)| *style)
                .next_back()
                .unwrap_or_default();
            if selected.is_some_and(|s| s.start <= from && to <= s.end) {
                style = style.add_modifier(Modifier::REVERSED);
            }
            Span::styled(text[from..to].to_owned(), style)
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Each styled piece of `text` as (its text, bold, italic, dim, cyan).
    fn styled(text: &str) -> Vec<(&str, bool, bool, bool, bool)> {
        marks(text)
            .into_iter()
            .map(|(range, style)| {
                let has = |m| style.add_modifier.contains(m);
                (
                    &text[range],
                    has(Modifier::BOLD),
                    has(Modifier::ITALIC),
                    has(Modifier::DIM),
                    style.fg == Some(Color::Cyan),
                )
            })
            .collect()
    }

    #[test]
    fn markdown_marks_stay_dim_and_what_they_mark_is_styled() {
        assert_eq!(
            styled("a **b** `c`"),
            [
                ("**", false, false, true, false),
                ("b", true, false, false, false),
                ("**", false, false, true, false),
                ("`", false, false, true, false),
                ("c", false, false, false, true),
                ("`", false, false, true, false),
            ]
        );
        // A heading: its marks dim, the rest bold, emphasis inside it too.
        assert_eq!(
            styled("## Hi *there*"),
            [
                ("## ", false, false, true, false),
                ("Hi ", true, false, false, false),
                ("*", false, false, true, false),
                ("there", true, true, false, false),
                ("*", false, false, true, false),
            ]
        );
        // Not yet closed, snake_case, 2*3*4, code's insides, a command.
        assert!(styled("**bold so far").is_empty());
        assert!(styled("snake_case_name and 2*3*4").is_empty());
        assert_eq!(styled("`a*b*c`")[1], ("a*b*c", false, false, false, true));
        assert!(styled("/model **x**").is_empty());
        assert!(styled("!ls `x`").is_empty());
        // Each line on its own: a mark never closes on the next.
        assert!(styled("a *b\nc* d").is_empty());
    }

    #[test]
    fn a_row_is_cut_where_its_styles_and_the_selection_change() {
        let text = "a **b** c";
        let marked = marks(text);
        let spans = spans(text, 0..text.len(), &marked, Some(&(5..9)));
        let shown: Vec<(&str, bool)> = (spans.iter())
            .map(|s| {
                (
                    s.content.as_ref(),
                    s.style.add_modifier.contains(Modifier::REVERSED),
                )
            })
            .collect();
        assert_eq!(
            shown,
            [
                ("a ", false),
                ("**", false),
                ("b", false),
                ("**", true),
                (" c", true)
            ]
        );
        // A row that starts inside a marked word keeps its style.
        let spans = super::spans(text, 5..9, &marked, None);
        assert_eq!(spans[0].content, "**");
        assert!(spans[0].style.add_modifier.contains(Modifier::DIM));
    }

    fn typed(text: &str) -> Prompt {
        let mut prompt = Prompt::default();
        prompt.insert(text);
        prompt
    }

    #[test]
    fn typing_goes_where_the_cursor_is() {
        let mut prompt = typed("helo");
        prompt.left();
        prompt.insert("l");
        assert_eq!(&*prompt, "hello");
        prompt.home();
        prompt.insert("¡");
        prompt.end();
        prompt.insert("!");
        assert_eq!(&*prompt, "¡hello!");
        prompt.left();
        prompt.backspace();
        assert_eq!(&*prompt, "¡hell!");
        prompt.home();
        prompt.delete();
        assert_eq!(&*prompt, "hell!");
    }

    #[test]
    fn words_and_lines_move_and_delete() {
        let mut prompt = typed("one two  three");
        prompt.word_left();
        assert_eq!(prompt.at, "one two  ".len());
        prompt.word_left();
        prompt.word_right();
        assert_eq!(prompt.at, "one two".len());
        prompt.end();
        prompt.delete_word();
        assert_eq!(&*prompt, "one two  ");
        let mut prompt = typed("first\nsecond");
        prompt.delete_to_start();
        assert_eq!(&*prompt, "first\n");
        prompt.delete_to_start();
        assert_eq!(&*prompt, "first");
        prompt.home();
        prompt.right();
        prompt.delete_to_end();
        assert_eq!(&*prompt, "f");
    }

    #[test]
    fn what_a_deletion_takes_comes_back_with_yank() {
        let mut prompt = typed("one two  three");
        prompt.home();
        prompt.delete_word_forward();
        assert_eq!(&*prompt, " two  three");
        prompt.delete_word_forward();
        assert_eq!(&*prompt, "  three");
        prompt.end();
        prompt.yank();
        assert_eq!(&*prompt, "  three two");
        prompt.delete_to_start();
        // Deleting nothing keeps what was taken before.
        prompt.delete_word();
        prompt.yank();
        prompt.yank();
        assert_eq!(&*prompt, "  three two  three two");
    }

    #[test]
    fn rows_wrap_at_words_and_the_cursor_follows_them() {
        // Eleven columns of text a row at width 14.
        let mut prompt = typed("alpha beta gamma\nd");
        let rows = prompt.layout(14);
        let shown: Vec<_> = rows.iter().map(|r| &prompt[r.start..r.end]).collect();
        assert_eq!(shown, ["alpha beta ", "gamma", "d"]);
        assert_eq!(prompt.cursor(&rows), (2, LEAD + 1));
        assert!(prompt.up(14));
        assert_eq!(prompt.cursor(&prompt.layout(14)), (1, LEAD + 1));
        assert!(prompt.up(14));
        assert_eq!(prompt.at, 1);
        assert!(!prompt.up(14));
        prompt.word_right();
        prompt.word_right();
        // Past the end of the shorter row below: its end, not the next's.
        assert!(prompt.down(14));
        assert_eq!(prompt.cursor(&prompt.layout(14)), (1, LEAD + 5));
        assert!(prompt.down(14));
        assert_eq!(prompt.at, prompt.len());
        assert!(!prompt.down(14));
    }

    #[test]
    fn up_and_down_keep_the_column_past_a_short_row() {
        let mut prompt = typed("a long first line\nab\nanother long line");
        assert!(prompt.up(40));
        assert!(prompt.up(40));
        assert_eq!(prompt.cursor(&prompt.layout(40)), (0, LEAD + 17));
        prompt.end();
        prompt.left();
        assert!(prompt.down(40));
        assert_eq!(prompt.cursor(&prompt.layout(40)), (1, LEAD + 2));
        // Typing forgets the column: down goes on from where it is now.
        prompt.insert("c");
        assert!(prompt.down(40));
        assert_eq!(prompt.cursor(&prompt.layout(40)), (2, LEAD + 3));
    }

    #[test]
    fn an_emoji_of_several_chars_moves_and_wraps_whole() {
        let family = "\u{1F468}\u{200D}\u{1F469}\u{200D}\u{1F467}";
        let mut prompt = typed(&format!("a{family}b"));
        prompt.left();
        prompt.left();
        assert_eq!(prompt.at, 1);
        prompt.right();
        prompt.backspace();
        assert_eq!(&*prompt, "ab");
        // Ten columns a row at width 13: the emoji goes whole to the next.
        let prompt = typed(&format!("abcdefghi{family}"));
        let rows = prompt.layout(13);
        let shown: Vec<_> = rows.iter().map(|r| &prompt[r.start..r.end]).collect();
        assert_eq!(shown, ["abcdefghi", family]);
        assert_eq!(prompt.cursor(&rows), (1, LEAD + 2));
    }

    #[test]
    fn an_edit_that_joins_graphemes_leaves_the_cursor_after_them() {
        let (us, s) = ("\u{1F1FA}", "\u{1F1F8}");
        let mut prompt = typed(s);
        prompt.home();
        prompt.insert(us);
        assert_eq!(prompt.at, prompt.len());
        let mut prompt = typed(&format!("{us}x{s}"));
        prompt.left();
        prompt.backspace();
        assert_eq!(prompt.at, prompt.len());
        prompt.backspace();
        assert!(prompt.is_empty());
    }

    #[test]
    fn spaces_past_a_row_wrap_like_any_text() {
        let prompt = typed(&" ".repeat(16));
        let rows = prompt.layout(13);
        assert_eq!(rows.len(), 2);
        assert_eq!(prompt.cursor(&rows), (1, LEAD + 6));
    }

    #[test]
    fn typing_and_deleting_take_the_selection() {
        let mut prompt = typed("one two three");
        prompt.select(4, 7);
        assert_eq!(prompt.selection(), Some(4..7));
        prompt.backspace();
        assert_eq!(&*prompt, "one  three");
        assert_eq!((prompt.at, prompt.selection()), (4, None));
        // Selected backwards, the cursor at its start.
        prompt.select(10, 5);
        prompt.delete();
        assert_eq!(&*prompt, "one  ");
        prompt.select(0, 3);
        prompt.insert("1");
        assert_eq!(&*prompt, "1  ");
        // A word or line deletion takes the selection, for Ctrl-Y.
        let mut prompt = typed("keep cut keep");
        prompt.select(5, 9);
        prompt.delete_word();
        assert_eq!(&*prompt, "keep keep");
        prompt.end();
        prompt.yank();
        assert_eq!(&*prompt, "keep keepcut ");
    }

    #[test]
    fn moving_lets_go_of_the_selection() {
        let mut prompt = typed("one two three");
        prompt.select(4, 7);
        prompt.left();
        assert_eq!((prompt.at, prompt.selection()), (4, None));
        prompt.select(7, 4);
        prompt.right();
        assert_eq!((prompt.at, prompt.selection()), (7, None));
        prompt.select(4, 7);
        prompt.home();
        prompt.backspace();
        assert_eq!(&*prompt, "one two three");
        prompt.select(4, 7);
        assert!(prompt.unselect());
        assert!(!prompt.unselect());
    }

    #[test]
    fn clicks_find_their_spot_word_and_line() {
        let prompt = typed("alpha beta gamma\nd");
        let rows = prompt.layout(14);
        // Past a wrapped row's end: its last character, not the next row.
        assert_eq!(prompt.spot(&rows[0], 40), "alpha beta".len());
        assert_eq!(prompt.spot(&rows[1], LEAD + 2), "alpha beta ga".len());
        assert_eq!(prompt.spot(&rows[1], 0), "alpha beta ".len());
        assert_eq!(prompt.spot(&rows[2], 40), prompt.len());
        assert_eq!(prompt.word_at(7), 6..10);
        assert_eq!(prompt.word_at(5), 5..6);
        assert_eq!(prompt.word_at(12), 11..16);
        assert_eq!(prompt.line_at(7), 0..16);
        assert_eq!(prompt.line_at(17), 17..18);
    }

    #[test]
    fn a_word_wider_than_a_row_is_cut() {
        let prompt = typed("abcdefghijklmnop");
        let rows = prompt.layout(13);
        let shown: Vec<_> = rows.iter().map(|r| &prompt[r.start..r.end]).collect();
        assert_eq!(shown, ["abcdefghij", "klmnop"]);
        assert_eq!(prompt.cursor(&rows), (1, LEAD + 6));
    }
}
