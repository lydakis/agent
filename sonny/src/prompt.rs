//! The composer's text and where the cursor is in it, laid out the way
//! the composer draws it, so moving up and down follows the wrapped rows.
//!
//! The cursor moves, and rows break, between graphemes: what the terminal
//! draws as one character, such as an emoji made of several.
use std::ops::Deref;
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
        self.text.clear();
        self.at = 0;
    }

    /// Replace the text, with the cursor at its end.
    pub fn set(&mut self, text: &str) {
        self.goal = None;
        self.text = text.to_owned();
        self.at = self.text.len();
    }

    pub fn insert(&mut self, text: &str) {
        self.goal = None;
        self.text.insert_str(self.at, text);
        self.at += text.len();
        self.settle();
    }

    pub fn backspace(&mut self) {
        self.goal = None;
        let start = self.before(self.at);
        self.text.drain(start..self.at);
        self.at = start;
        self.settle();
    }

    pub fn delete(&mut self) {
        self.goal = None;
        let end = self.after(self.at);
        self.text.drain(self.at..end);
        self.settle();
    }

    pub fn left(&mut self) {
        self.goal = None;
        self.at = self.before(self.at);
    }

    pub fn right(&mut self) {
        self.goal = None;
        self.at = self.after(self.at);
    }

    /// The start of the cursor's line.
    pub fn home(&mut self) {
        self.goal = None;
        self.at = self.line_start();
        self.settle();
    }

    /// The end of the cursor's line.
    pub fn end(&mut self) {
        self.goal = None;
        self.at = self.line_end();
    }

    pub fn word_left(&mut self) {
        self.goal = None;
        self.at = self.word_start();
        self.settle();
    }

    pub fn word_right(&mut self) {
        self.goal = None;
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
        let start = self.word_start();
        self.kill(start..self.at);
    }

    /// Delete to the end of the word after the cursor.
    pub fn delete_word_forward(&mut self) {
        self.goal = None;
        let start = self.at;
        self.word_right();
        self.kill(start..self.at);
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
        let rows = self.layout(width);
        let (row, col) = self.cursor(&rows);
        let col = *self.goal.get_or_insert(col);
        let target = match down {
            true if row + 1 < rows.len() => row + 1,
            false if row > 0 => row - 1,
            _ => return false,
        };
        let Row { start, end } = rows[target];
        // The last column at or before the cursor's, but never past the
        // row's end, where the next row starts.
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
        self.at = at.max(start);
        true
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

#[cfg(test)]
mod tests {
    use super::*;

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
    fn a_word_wider_than_a_row_is_cut() {
        let prompt = typed("abcdefghijklmnop");
        let rows = prompt.layout(13);
        let shown: Vec<_> = rows.iter().map(|r| &prompt[r.start..r.end]).collect();
        assert_eq!(shown, ["abcdefghij", "klmnop"]);
        assert_eq!(prompt.cursor(&rows), (1, LEAD + 6));
    }
}
