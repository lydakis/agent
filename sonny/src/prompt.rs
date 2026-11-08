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
use std::cell::RefCell;
use std::ops::{Deref, Range};
use std::rc::Rc;
use unicode_segmentation::{GraphemeCursor, UnicodeSegmentation};
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
    /// The rows last laid out, and what the text was then: a key lays out
    /// again only the rows its edit reached, so typing into a long paste
    /// costs what typing into a short one does.
    laid: RefCell<Option<Laid>>,
    /// The marks last drawn and the bytes shown then, until the text
    /// changes: a frame that redraws a long prompt unchanged reads none.
    styled: RefCell<Option<Styled>>,
}

/// Marks styling the bytes `shown`, as [`marks_in`] has them.
type Styled = (Range<usize>, Rc<Vec<(Range<usize>, Style)>>);

/// Rows laid out at a width, and how the text changed since: the bytes
/// before `from` and the last `tail` bytes are as they were, and the text
/// was `len` bytes long.
struct Laid {
    width: usize,
    rows: Rc<Vec<Row>>,
    len: usize,
    from: usize,
    tail: usize,
}

/// One drawn row: the bytes of the text it shows.
#[derive(Clone, Copy, Debug, PartialEq)]
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
        self.laid.take();
        self.styled.take();
    }

    /// Replace the text, with the cursor at its end.
    pub fn set(&mut self, text: &str) {
        self.goal = None;
        self.anchor = None;
        self.text = text.to_owned();
        self.at = self.text.len();
        self.laid.take();
        self.styled.take();
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
        self.splice(range, "");
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
        self.splice(self.at..self.at, text);
        self.at += text.len();
        self.settle();
    }

    pub fn backspace(&mut self) {
        self.goal = None;
        if self.cut() {
            return;
        }
        let start = self.before(self.at);
        self.splice(start..self.at, "");
        self.at = start;
        self.settle();
    }

    pub fn delete(&mut self) {
        self.goal = None;
        if self.cut() {
            return;
        }
        let end = self.after(self.at);
        self.splice(self.at..end, "");
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

    /// The cursor to the very start of the text.
    pub fn top(&mut self) {
        self.goal = None;
        self.anchor = None;
        self.at = 0;
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
        self.splice(range, "");
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
        // Read around the cursor alone, not from the start of the text.
        let len = self.text.len();
        let mut at = self.at.min(len);
        while !self.text.is_char_boundary(at) {
            at += 1;
        }
        let mut cursor = GraphemeCursor::new(at, len, true);
        self.at = match cursor.is_boundary(&self.text, 0) {
            Ok(true) => at,
            _ => (cursor.next_boundary(&self.text, 0).ok().flatten()).unwrap_or(len),
        };
    }

    /// Put `with` in place of the bytes `range`, noting what changed for
    /// the next layout.
    fn splice(&mut self, range: Range<usize>, with: &str) {
        if let Some(laid) = self.laid.get_mut() {
            laid.from = laid.from.min(range.start);
            laid.tail = laid.tail.min(self.text.len() - range.end);
        }
        self.styled.take();
        self.text.replace_range(range, with);
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
    ///
    /// Kept between calls: after an edit, the rows are wrapped again from
    /// the row before the one it touched, until a row starts where one
    /// started before in the text after it, and the rest are moved over.
    /// A row's start is all its wrapping depends on, so what follows such
    /// a row is the same. A key in a long text costs a few rows, not the
    /// text.
    pub fn layout(&self, width: usize) -> Rc<Vec<Row>> {
        let mut laid = self.laid.borrow_mut();
        let len = self.text.len();
        match laid.as_mut() {
            Some(laid) if laid.width == width => {
                if (laid.from, laid.tail) != (len, len) || laid.len != len {
                    let rows = Rc::make_mut(&mut laid.rows);
                    relayout(&self.text, width, rows, laid.len, laid.from, laid.tail);
                }
                laid.len = len;
                (laid.from, laid.tail) = (len, len);
                laid.rows.clone()
            }
            _ => {
                let mut rows = Vec::new();
                wrap(&self.text, 0, width, &mut rows, |_| None);
                let rows = Rc::new(rows);
                *laid = Some(Laid {
                    width,
                    rows: rows.clone(),
                    len,
                    from: len,
                    tail: len,
                });
                rows
            }
        }
    }

    /// The marks on the bytes `shown`, as [`marks_in`] finds them in the
    /// whole text, read from the lines shown alone; kept until the text
    /// changes or other bytes are shown.
    pub fn marks(&self, shown: Range<usize>) -> Rc<Vec<(Range<usize>, Style)>> {
        let mut styled = self.styled.borrow_mut();
        match &*styled {
            Some((at, marked)) if *at == shown => marked.clone(),
            _ => {
                let mut marked = marks_in(&self.text, shown.clone());
                marked.retain(|(piece, _)| piece.start <= shown.end && shown.start <= piece.end);
                let marked = Rc::new(marked);
                *styled = Some((shown, marked.clone()));
                marked
            }
        }
    }

    /// The cursor's drawn row, and its column counting the lead.
    pub fn cursor(&self, rows: &[Row]) -> (usize, usize) {
        // The last row that starts at or before the cursor: where a word
        // is cut, the cursor there is on the row it begins.
        let row = rows
            .partition_point(|row| row.start <= self.at)
            .saturating_sub(1)
            .min(rows.len().saturating_sub(1));
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

/// Wrap `text` from `start`, the start of a row, into `rows` at `width`,
/// until `stop`, asked the start of each next row, says where the rows
/// laid before take over: then those are put after, and wrapping ends.
fn wrap(
    text: &str,
    start: usize,
    width: usize,
    rows: &mut Vec<Row>,
    mut stop: impl FnMut(usize) -> Option<()>,
) {
    let room = width.saturating_sub(1 + LEAD).max(8);
    #[cfg(test)]
    let mut rows = Counted(rows);
    let mut line_start = start;
    let mut start = start;
    loop {
        let line_end = (text[line_start..].find('\n')).map_or(text.len(), |i| line_start + i);
        let line = &text[line_start..line_end];
        let mut used = 0;
        let mut word_start = line_start;
        for word in words(line) {
            // A word wider than the rest of a row starts the next one; a
            // row's first word, however wide, needs no measuring here.
            if used > 0 && used + word.trim_end().width() > room {
                rows.push(Row {
                    start,
                    end: word_start,
                });
                start = word_start;
                used = 0;
                if stop(start).is_some() {
                    return;
                }
            }
            // A word wider than a row is cut where the row ends.
            for (i, g) in word.grapheme_indices(true) {
                let w = g.width();
                if used + w > room && used > 0 {
                    let at = word_start + i;
                    rows.push(Row { start, end: at });
                    start = at;
                    used = 0;
                    if stop(start).is_some() {
                        return;
                    }
                }
                used += w;
            }
            word_start += word.len();
        }
        rows.push(Row {
            start,
            end: line_end,
        });
        if line_end == text.len() {
            return;
        }
        line_start = line_end + 1;
        start = line_start;
        if stop(start).is_some() {
            return;
        }
    }
}

/// The rows `wrap` lays out, counted, for the tests to see how many an
/// edit lays out again.
#[cfg(test)]
struct Counted<'a>(&'a mut Vec<Row>);

#[cfg(test)]
thread_local! {
    static WRAPPED: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
}

#[cfg(test)]
impl Counted<'_> {
    fn push(&mut self, row: Row) {
        WRAPPED.with(|n| n.set(n.get() + 1));
        self.0.push(row);
    }
}

/// Lay `rows`, laid out for a text of `len` bytes, out again for `text`,
/// whose first `from` bytes and last `tail` bytes are as they were.
fn relayout(text: &str, width: usize, rows: &mut Vec<Row>, len: usize, from: usize, tail: usize) {
    // The row before the first the edit touched: a word made shorter may
    // now fit on it.
    let first = rows.partition_point(|row| row.end < from).saturating_sub(1);
    let start = rows.get(first).map_or(0, |row| row.start);
    let old = rows.split_off(first);
    // Rows of the old text that start in its unchanged end, by where they
    // start in the new: once a new row starts at one, the rest are those.
    let kept = old.partition_point(|row| row.start < len - tail);
    let moved = |at: usize| at + text.len() - len;
    let mut taken = None;
    wrap(text, start, width, rows, |at| {
        let from_new = text.len() - tail;
        if at < from_new {
            return None;
        }
        let found = old[kept..].binary_search_by(|row| moved(row.start).cmp(&at));
        let found = found.ok()?;
        taken = Some(kept + found);
        Some(())
    });
    if let Some(taken) = taken {
        rows.extend(old[taken..].iter().map(|row| Row {
            start: moved(row.start),
            end: moved(row.end),
        }));
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

/// The marks of the whole text, as [`marks_in`] reads them.
#[cfg(test)]
fn marks(text: &str) -> Vec<(Range<usize>, Style)> {
    marks_in(text, 0..text.len())
}

/// How the composer draws what is typed: Markdown's marks kept and dim,
/// what they mark styled as the conversation draws it (bold, italic,
/// struck, `code` in code's colour, a `#` line as a heading), so the
/// person sees what they will send and sends the plain text they typed.
/// Each styled piece as a byte range; the rest is plain. A slash command
/// and a `!` command are not Markdown. Read from the lines `range`
/// reaches alone: what a few rows drawn of a long text need.
fn marks_in(text: &str, range: Range<usize>) -> Vec<(Range<usize>, Style)> {
    let mut marked = Vec::new();
    if text.starts_with('/') || text.starts_with('!') {
        return marked;
    }
    let mut start = text[..range.start].rfind('\n').map_or(0, |i| i + 1);
    for line in text[start..].split('\n') {
        let (lead, base) = opening(line);
        if lead > 0 {
            marked.push((start..start + lead, dim()));
        }
        inline(line, lead, start, base, &mut marked);
        start += line.len() + 1;
        if start > range.end {
            break;
        }
    }
    marked
}

/// A line's block mark, as the conversation draws it, and the style of
/// what follows: a heading's `#`s (bold), a quote's `>` (italic), a
/// list's `-`, `*`, `+` or number; none, plain.
fn opening(line: &str) -> (usize, Style) {
    let hashes = line.len() - line.trim_start_matches('#').len();
    if (1..=6).contains(&hashes) && line[hashes..].starts_with(' ') {
        return (hashes + 1, Style::new().add_modifier(Modifier::BOLD));
    }
    let indent = line.len() - line.trim_start().len();
    let rest = &line[indent..];
    if rest.starts_with("> ") {
        return (indent + 2, Style::new().add_modifier(Modifier::ITALIC));
    }
    if ["- ", "* ", "+ "]
        .iter()
        .any(|bullet| rest.starts_with(bullet))
    {
        return (indent + 2, Style::new());
    }
    let digits = rest.len() - rest.trim_start_matches(|c: char| c.is_ascii_digit()).len();
    match (1..=3).contains(&digits) && rest[digits..].starts_with(". ") {
        true => (indent + digits + 2, Style::new()),
        false => (0, Style::new()),
    }
}

fn dim() -> Style {
    Style::new().add_modifier(Modifier::DIM)
}

/// A link starting at `at` in `line`, as `transcript::inline` takes one:
/// its label's length and the link's.
fn link(line: &str, at: usize, next: &mut Next) -> Option<(usize, usize)> {
    if !line[at..].starts_with('[') {
        return None;
    }
    let close = next.find(line, "](", at + 1)?;
    let end = next.find(line, ")", close + 2)? + 1;
    // No `[` in the label, no space in the URL, neither empty.
    let bracket = next.find(line, "[", at + 1).unwrap_or(usize::MAX);
    let space = next.find(line, " ", close + 2).unwrap_or(usize::MAX);
    (close > at + 1 && end > close + 3 && bracket > close && space >= end)
        .then_some((close - at - 1, end - at))
}

/// Where each mark is next found on a line, from where a search starts:
/// searches from later and later on one line read it once, not once per
/// mark that opens on it.
#[derive(Default)]
struct Next(Vec<(&'static str, Option<usize>)>);

impl Next {
    fn find(&mut self, line: &str, needle: &'static str, from: usize) -> Option<usize> {
        let known = self.0.iter_mut().find(|(n, _)| *n == needle);
        match known.as_deref() {
            // Not on the rest of the line: nor on any later part of it.
            Some((_, None)) => return None,
            Some((_, Some(found))) if *found >= from => return Some(*found),
            _ => {}
        }
        let found = (line.get(from..)?.find(needle)).map(|i| from + i);
        match known {
            Some(entry) => entry.1 = found,
            None => self.0.push((needle, found)),
        }
        found
    }
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
    let mut next = Next::default();
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
            let end = next.find(line, mark, at + mark.len())? - (at + mark.len());
            (end > 0).then_some((mark.len(), end, *style, *nested))
        });
        // A link, `[label](url)`: its label underlined, the rest dim.
        if found.is_none()
            && let Some((label, end)) = link(line, at, &mut next)
        {
            if plain < at && base != Style::new() {
                marked.push((offset + plain..offset + at, base));
            }
            marked.push((offset + at..offset + at + 1, dim()));
            inline(
                &line[..at + 1 + label],
                at + 1,
                offset,
                base.underlined(),
                marked,
            );
            marked.push((offset + at + 1 + label..offset + at + end, dim()));
            at += end;
            plain = at;
            continue;
        }
        let Some((mark, end, style, nested)) = found else {
            // Past this character to the next that may open a mark: the
            // plain words between are passed over at once.
            let step = rest.chars().next().map_or(1, char::len_utf8);
            at += step
                + rest[step..]
                    .find(['`', '*', '_', '~', '['])
                    .unwrap_or(rest.len() - step);
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
        // A list's mark and a quote's are dim, a quote italic; a link's
        // label underlined.
        assert_eq!(
            styled("- a\n  12. b\n> c"),
            [
                ("- ", false, false, true, false),
                ("  12. ", false, false, true, false),
                ("> ", false, false, true, false),
                ("c", false, true, false, false),
            ]
        );
        let link = marks("see [the docs](https://x.io) now");
        let pieces: Vec<(&str, bool, bool)> = (link.iter())
            .map(|(range, style)| {
                let has = |m| style.add_modifier.contains(m);
                (
                    &"see [the docs](https://x.io) now"[range.clone()],
                    has(Modifier::UNDERLINED),
                    has(Modifier::DIM),
                )
            })
            .collect();
        assert_eq!(
            pieces,
            [
                ("[", false, true),
                ("the docs", true, false),
                ("](https://x.io)", false, true),
            ]
        );
        assert!(styled("a [b] (c) and [](x) and [d](e f) and [g]()").is_empty());
        assert_eq!(styled("[a [b](c) d")[1].0, "b");
        // A long line of marks that never close is read once, not once per
        // mark: a paste of them stays quick to draw.
        let long = format!("`*{}", "[x] (y) ".repeat(40_000));
        let started = std::time::Instant::now();
        assert!(marks(&long).is_empty());
        assert!(started.elapsed() < std::time::Duration::from_secs(1));
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

    /// A small, fixed sequence of numbers, the same each run.
    struct Dice(u64);
    impl Dice {
        fn roll(&mut self, n: usize) -> usize {
            self.0 = self
                .0
                .wrapping_mul(6364136223846793005)
                .wrapping_add(1442695040888963407);
            ((self.0 >> 33) as usize) % n
        }
    }

    #[test]
    fn rows_laid_out_again_after_edits_are_the_rows_laid_out_anew() {
        let pieces = [
            "a",
            "word ",
            "longer words ",
            "\n",
            "\n\n",
            "  ",
            "🇬🇷",
            "👩‍👩‍👧",
            "é",
            "abcdefghijklmnopqrstuvwxyz0123456789",
            "x ",
            "中文字 ",
            "\t",
        ];
        let mut dice = Dice(7);
        for width in [13, 20, 41] {
            let mut prompt = Prompt::default();
            for _ in 0..1500 {
                let _ = prompt.layout(width);
                // A few edits between layouts, as keys come between frames.
                for _ in 0..1 + dice.roll(3) {
                    let at = dice.roll(prompt.len() + 1);
                    prompt.select(at, at);
                    prompt.unselect();
                    match dice.roll(8) {
                        0 => prompt.backspace(),
                        1 => prompt.delete(),
                        2 => prompt.delete_word(),
                        3 => prompt.delete_to_end(),
                        _ => prompt.insert(pieces[dice.roll(pieces.len())]),
                    }
                }
                let anew = typed(&prompt);
                assert_eq!(*prompt.layout(width), *anew.layout(width), "{:?}", &*prompt);
                assert_eq!(prompt.cursor(&prompt.layout(width)), {
                    let rows = anew.layout(width);
                    let row = rows
                        .iter()
                        .rposition(|row| row.start <= prompt.at && prompt.at <= row.end)
                        .unwrap_or(0);
                    (row, LEAD + prompt[rows[row].start..prompt.at].width())
                });
            }
        }
    }

    #[test]
    fn a_key_lays_out_the_same_few_rows_however_long_the_text() {
        // Doubling the text leaves the rows a key lays out again the same.
        let mut counts = Vec::new();
        for kib in [16, 32, 64, 128] {
            let words = [
                "a ", "word ", "other ", "longest ", "is ", "between ", "the ",
            ];
            let mut dice = Dice(11);
            let mut text = String::new();
            let mut n = 0;
            while text.len() < kib * 1024 {
                text.push_str(words[dice.roll(words.len())]);
                n += 1;
                if n % 20 == 0 {
                    text.push('\n');
                }
            }
            for line_breaks in [true, false] {
                let text = match line_breaks {
                    true => text.clone(),
                    false => text.replace('\n', " "),
                };
                let mut prompt = typed(&text);
                let half = text.len() / 2;
                prompt.select(half, half);
                prompt.unselect();
                let _ = prompt.layout(80);
                WRAPPED.with(|n| n.set(0));
                for key in "and then a few words typed into the middle of it "
                    .chars()
                    .cycle()
                    .take(200)
                {
                    prompt.insert(&key.to_string());
                    let _ = prompt.layout(80);
                }
                prompt.backspace();
                let _ = prompt.layout(80);
                counts.push((kib, line_breaks, WRAPPED.with(|n| n.get())));
            }
        }
        // About two rows a key on lines, a dozen in one long paragraph,
        // where a word pushed down moves the rows after it until one
        // starts where it did; 128 KiB as 16.
        for (kib, _, wrapped) in &counts {
            assert!(*wrapped < 200 * 20, "{kib} KiB: {counts:?}");
        }
    }

    #[test]
    fn the_marks_of_shown_lines_are_those_of_the_whole_text() {
        let text = "# Heading\nsome **bold** and _it_\n- a [link](u) `code`\nplain\n> quote ~~x~~";
        let all = marks(text);
        for start in 0..text.len() {
            if !text.is_char_boundary(start) {
                continue;
            }
            for end in start..=text.len() {
                let lines = text[..start].rfind('\n').map_or(0, |i| i + 1)
                    ..text[end..].find('\n').map_or(text.len(), |i| end + i);
                let within: Vec<_> = (all.iter())
                    .filter(|(piece, _)| lines.start <= piece.start && piece.end <= lines.end + 1)
                    .cloned()
                    .collect();
                assert_eq!(marks_in(text, start..end), within, "{start}..{end}");
            }
        }
    }
}
