//! The conversation on the screen Sonny owns: it keeps the rows
//! itself and draws them above the rows that move, on the alternate
//! screen, instead of handing them to the terminal's scrollback. So the
//! wheel scrolls them, and the mouse selects and copies them with no key
//! held, as Codex's full-screen transcript does (openai/codex,
//! codex-rs/tui/src/transcript_view): a drag selects and copies when let
//! go, a double click takes a word, a triple click a line, and a click
//! that does not drag opens the link under it. What is copied is the text
//! as written: a line wrapped over rows comes back as one line, without
//! the rows' leads or the indent every copied line shares.
use std::{
    sync::Arc,
    time::{Duration, Instant},
};

use ratatui::{
    buffer::{Buffer, Cell},
    layout::Rect,
    style::{Color, Modifier, Style, Stylize},
    text::{Line, Span},
};
use unicode_segmentation::UnicodeSegmentation;
use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

/// How far one notch of the wheel scrolls.
const WHEEL: usize = 3;
/// How soon a second click counts as a double click.
pub const DOUBLE: Duration = Duration::from_millis(400);
/// How many of the conversation's rows new ones leave to scroll back to,
/// as a terminal keeps its scrollback; the whole conversation stays in
/// the bot's history. Pages of older rows drawn above on asking for them
/// are kept by [`PAGES_KEPT`] instead, until these go.
const KEEP: usize = 10_000;
/// How many screens above the view the older rows are asked for, so the
/// wheel finds them drawn rather than waits at the first row.
const AHEAD: usize = 3;
/// How many screens either side of the view a page let go is drawn again
/// in, and kept: far enough that a held Page Down finds it drawn.
const NEAR: usize = 8;
/// How many rows the pages of older items keep drawn, about 32 MB at the
/// 2.7 KB a drawn row takes: past it, those farthest from the view are let
/// go for blank rows as tall, and drawn again as the view nears them.
const PAGES_KEPT: usize = 12_000;
/// How far past a link's rows on screen to look for the rest of it.
const LINK_ROWS: usize = 16;

/// What the person asked for with the mouse.
#[derive(Debug, PartialEq)]
pub enum Action {
    /// Text they selected, to put on the clipboard.
    Copy(String),
    /// A link they clicked.
    Open(String),
}

/// How a row's link names something Sonny does when it is clicked (a
/// button), rather than a page or a file: never underlined, and never
/// written out for the terminal to open.
pub const BUTTON: &str = "sonny:";

/// What a row is, so the client finds it again to change what is drawn
/// around it: where a message ends, the reactions under it, the rows of a
/// group of calls drawn as one.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Tag {
    /// The last row of the message at this node, the bot's or the person's.
    End(i64),
    /// The reactions under the message at this node.
    Reactions(i64),
    /// Other agents' reactions under the person's message at this node.
    Picked(i64),
    /// A group of calls, by the client's number for it.
    Group(u64),
    /// The rows of the message at this node that one bot sent another.
    Relayed(i64),
    /// Tasks the lead started together, by the client's number for them.
    Tasks(u64),
}

/// What a line pushed is besides its text: a [`Tag`], or a link its row
/// opens across its text, a [`BUTTON`] or a page.
#[derive(Clone, Debug, PartialEq)]
pub enum Mark {
    Tag(Tag),
    Link(String),
}

/// A row on the screen: one of the conversation's rows, or the part of
/// one that did not fit the width.
struct Shown {
    /// The row's line itself when it fits, so it is kept once.
    line: Arc<Line<'static>>,
    /// Where its text opens, when its line said so rather than its words.
    link: Option<String>,
    /// The columns of the row's lead, when it goes on with the row above.
    joins: Option<usize>,
    /// The columns of its lead (a bullet, an indent) otherwise.
    opens: usize,
    /// It is the first of its row's.
    first: bool,
}

/// A line of the conversation, its wrapped rows joined again, so it can
/// be wrapped anew at any width.
struct Row {
    line: Arc<Line<'static>>,
    /// The lead the rows it goes on to start with, as it was written.
    rest: Option<Line<'static>>,
    /// Where its text opens, whatever its words are.
    link: Option<String>,
    tag: Option<Tag>,
}

impl Row {
    fn new(line: Line<'static>) -> Self {
        Row {
            line: Arc::new(line),
            rest: None,
            link: None,
            tag: None,
        }
    }

    fn mark(&mut self, mark: Mark) {
        match mark {
            Mark::Tag(tag) => self.tag = Some(tag),
            Mark::Link(url) => self.link = Some(url),
        }
    }
}

/// A place in the rows shown: a row and a column.
type Spot = (usize, usize);

#[derive(Clone, Copy, PartialEq)]
enum Unit {
    Cell,
    Word,
    Line,
}

struct Selection {
    anchor: Spot,
    head: Spot,
    unit: Unit,
    /// The mouse moved with its button down, so this is no click.
    dragged: bool,
}

/// A page of older items drawn above: how many of the rows it is, and
/// whether they are drawn or blank rows standing in for them.
struct Page {
    id: u64,
    rows: usize,
    drawn: bool,
}

#[derive(Default)]
pub struct Screen {
    /// The conversation's lines, as written.
    rows: Vec<Row>,
    /// Those rows wrapped to `width`, as drawn.
    shown: Vec<Shown>,
    /// How many of `rows` are in `shown`.
    wrapped: usize,
    /// How many rows of `shown` were taken off to be wrapped again.
    relaid: usize,
    width: usize,
    /// How many rows the view is scrolled up from the newest.
    back: usize,
    /// Where the rows were drawn last, the first one drawn there, and
    /// how many.
    area: Rect,
    top: usize,
    drawn: usize,
    selection: Option<Selection>,
    /// The last press: when, where, and how many presses in a row.
    clicks: Option<(Instant, u16, u16, u8)>,
    /// The links drawn last, by screen row and columns, and what was
    /// drawn when they were found.
    links: Vec<(u16, u16, u16, String)>,
    linked: Option<(usize, usize, usize, Rect)>,
    /// What Ctrl+F looks for, and where it is.
    finding: Option<Finding>,
    /// Counts each change to the rows shown, so what was found in them is
    /// looked for again.
    laid: usize,
    /// A line to show at the top once the rows are laid out; or a row,
    /// as they were drawn.
    go_to: Option<(usize, usize)>,
    top_row: Option<usize>,
    /// A line to scroll to, as little as it takes, with rows under it.
    keep: Option<(usize, usize)>,
    /// The oldest rows went to make room: those above them are not drawn
    /// again.
    trimmed: bool,
    /// Rows were written under the newest since the oldest last went.
    grew: bool,
    /// The pages of older items over the rest, from the first row down.
    pages: Vec<Page>,
    /// Where each page is drawn in `shown`, as of `laid`.
    placed: Option<(usize, Vec<std::ops::Range<usize>>)>,
}

/// The place found is to be picked again, the rows having been drawn anew.
const AGAIN: usize = usize::MAX;

/// The newest of `hits` that starts above `bottom`, else the newest.
fn nearest(hits: &[(Spot, Spot)], bottom: usize) -> usize {
    (hits.iter())
        .rposition(|(start, _)| start.0 < bottom)
        .unwrap_or(hits.len().saturating_sub(1))
}

/// Text looked for in the rows (Ctrl+F): each place it is, from the
/// oldest, as the rows it starts and ends on, and the one shown.
struct Finding {
    query: String,
    hits: Vec<(Spot, Spot)>,
    at: usize,
    laid: usize,
}

impl Screen {
    /// The conversation drawn again from the start, as a repaint does.
    /// What is being found stays, looked for again in the rows drawn anew.
    pub fn clear(&mut self) {
        let finding = self.finding.take().map(|finding| Finding {
            at: AGAIN,
            laid: usize::MAX,
            ..finding
        });
        *self = Self::default();
        self.finding = finding;
    }

    pub fn push(&mut self, lines: Vec<Line<'static>>) {
        self.push_marked(lines, Vec::new());
    }

    /// [`Screen::push`], each of `marks` on the row the line at its place
    /// in `lines` was pushed into.
    pub fn push_marked(&mut self, lines: Vec<Line<'static>>, mut marks: Vec<(usize, Mark)>) {
        marks.sort_by_key(|(at, _)| *at);
        let mut marks = marks.into_iter().peekable();
        self.grew |= !lines.is_empty();
        for (at, line) in lines.into_iter().enumerate() {
            self.push_line(line);
            while let Some((_, mark)) = marks.next_if(|(on, _)| *on == at) {
                if let Some(row) = self.rows.last_mut() {
                    row.mark(mark);
                }
            }
        }
    }

    /// `mark` on the last row pushed; whether there was one.
    pub fn mark_last(&mut self, mark: Mark) -> bool {
        let laid = self.wrapped == self.rows.len();
        let Some(row) = self.rows.last_mut() else {
            return false;
        };
        row.mark(mark);
        // Its rows drawn already link where it does now.
        if laid {
            let link = row.link.clone();
            let from = (self.shown.iter()).rposition(|shown| shown.first);
            for shown in &mut self.shown[from.unwrap_or(0)..] {
                shown.link = link.clone();
            }
            self.linked = None;
        }
        true
    }

    fn push_line(&mut self, line: Line<'static>) {
        let Some(lead) = crate::transcript::lead(&line).filter(|_| !self.rows.is_empty()) else {
            self.rows.push(Row::new(line));
            return;
        };
        // A row the line above was wrapped onto joins it again.
        if self.wrapped == self.rows.len() {
            self.unlay();
        }
        let (rest, text) = split(line, lead);
        let row = self.rows.last_mut().expect("a row to join");
        let line = Arc::make_mut(&mut row.line);
        // Spans alike join too, so a word cut over rows is one again.
        for span in text {
            match line.spans.last_mut() {
                Some(last) if last.style == span.style => {
                    last.content.to_mut().push_str(&span.content)
                }
                _ => line.spans.push(span),
            }
        }
        row.rest.get_or_insert(rest);
    }

    /// The row tagged `tag`, the newest if more are.
    fn tagged(&self, tag: Tag) -> Option<usize> {
        self.rows.iter().rposition(|row| row.tag == Some(tag))
    }

    /// Whether a row is tagged `tag`.
    pub fn has(&self, tag: Tag) -> bool {
        self.tagged(tag).is_some()
    }

    /// The row tagged `tag` (`line`, or none) put right under the row
    /// tagged `under`, in place of the one there: reactions under their
    /// message. Whether `under` was found.
    pub fn put_under(&mut self, under: Tag, tag: Tag, line: Option<Line<'static>>) -> bool {
        let Some(at) = self.tagged(under).map(|at| at + 1) else {
            return false;
        };
        let shown = self.shown_at(at);
        let there = (self.rows.get(at)).is_some_and(|row| row.tag == Some(tag));
        match (line, there) {
            (Some(line), true) => self.rows[at].line = Arc::new(line),
            (Some(line), false) => {
                let mut row = Row::new(line);
                row.tag = Some(tag);
                self.rows.insert(at, row);
                self.grow_page(at - 1, 1);
            }
            (None, true) => {
                self.rows.remove(at);
                self.grow_page(at - 1, -1);
            }
            (None, false) => return true,
        }
        self.relay(at, shown);
        true
    }

    /// The rows tagged `tag` replaced by `lines`, each line's `marks` on
    /// its row and every row tagged `tag`: a group of calls drawn again.
    /// Whether there were any.
    pub fn replace_tagged(
        &mut self,
        tag: Tag,
        lines: Vec<Line<'static>>,
        marks: Vec<(usize, Mark)>,
    ) -> bool {
        let Some(first) = self.rows.iter().position(|row| row.tag == Some(tag)) else {
            return false;
        };
        let end = (self.rows[first..].iter())
            .position(|row| row.tag != Some(tag))
            .map_or(self.rows.len(), |n| first + n);
        let shown = self.shown_at(first);
        let mut fresh = Screen::default();
        fresh.push_marked(lines, marks);
        for row in &mut fresh.rows {
            row.tag = Some(tag);
        }
        let grown = fresh.rows.len() as isize - (end - first) as isize;
        self.rows.splice(first..end, fresh.rows);
        self.grow_page(first, grown);
        self.relay(first, shown);
        true
    }

    /// The page of older items the row `row` is in, if any, `by` rows
    /// longer: rows put into it or taken out, so it still ends where it
    /// does.
    fn grow_page(&mut self, row: usize, by: isize) {
        let mut start = 0;
        for page in &mut self.pages {
            if row < start + page.rows {
                page.rows = page.rows.saturating_add_signed(by);
                self.placed = None;
                return;
            }
            start += page.rows;
        }
    }

    /// Each row's text, as written: what a test reads of the conversation.
    #[cfg(test)]
    pub fn texts(&self) -> Vec<String> {
        self.rows.iter().map(|row| row.line.to_string()).collect()
    }

    /// Where each row's link goes, if it has one.
    #[cfg(test)]
    pub fn links(&self) -> Vec<Option<String>> {
        self.rows.iter().map(|row| row.link.clone()).collect()
    }

    /// Where the row `row` starts among the rows drawn, if it was laid out.
    fn shown_at(&self, row: usize) -> Option<usize> {
        if row >= self.wrapped {
            return None;
        }
        let mut firsts = 0;
        (self.shown.iter()).position(|shown| {
            firsts += usize::from(shown.first);
            firsts > row
        })
    }

    /// The rows from `row` on laid out again, as they changed: those
    /// drawn from `shown` on taken off first. The view stays on the rows
    /// above a change in it or under it, and on the newest rows when it
    /// was there.
    fn relay(&mut self, row: usize, shown: Option<usize>) {
        self.selection = None;
        self.linked = None;
        self.laid += 1;
        let Some(at) = shown else {
            // Not laid out yet: the next layout takes them as they are.
            self.wrapped = self.wrapped.min(row);
            return;
        };
        let before = self.shown.len();
        let hold = self.back > 0 && at >= self.top;
        self.shown.truncate(at);
        self.wrapped = row;
        if self.width == 0 {
            return;
        }
        let back = std::mem::take(&mut self.back);
        self.wrap(self.width);
        self.back = match hold {
            true => (back + self.shown.len()).saturating_sub(before),
            false => back,
        };
    }

    /// A line whose rows after its first start with `rest` when wrapped,
    /// as a numbered line of code goes on under its code.
    pub fn push_hanging(&mut self, line: Line<'static>, rest: Line<'static>) {
        self.grew = true;
        let mut row = Row::new(line);
        row.rest = Some(rest);
        self.rows.push(row);
    }

    /// A line whose text opens `url`, a name that may hold spaces or
    /// anything else a link read from words would stop at: a row of a
    /// list each row of opens, as a folder's entries, so not underlined.
    pub fn push_linked(&mut self, line: Line<'static>, url: String) {
        self.grew = true;
        let mut row = Row::new(line);
        row.link = Some(url);
        self.rows.push(row);
    }

    /// Its lines replaced by `lines`, the view kept as far down from the
    /// first row as it was.
    pub fn replace(&mut self, lines: Vec<Line<'static>>) {
        let top = self.top;
        *self = Self {
            area: self.area,
            clicks: self.clicks.take(),
            ..Self::default()
        };
        self.push(lines);
        self.top_row = Some(top);
    }

    /// The view put with the line `row` of those pushed `above` drawn
    /// rows under its top.
    pub fn show_row(&mut self, row: usize, above: usize) {
        self.go_to = Some((row, above));
    }

    /// The view scrolled as little as it takes to show the line `row` of
    /// those pushed and `below` drawn rows under it, or to the top when
    /// that shows it: a picked row in a list.
    pub fn keep_in_view(&mut self, row: usize, below: usize) {
        self.keep = Some((row, below));
    }

    /// The last line's rows taken off the screen, to be wrapped again.
    fn unlay(&mut self) {
        let at = (self.shown.iter())
            .rposition(|shown| shown.first)
            .unwrap_or(0);
        self.relaid += self.shown.len() - at;
        self.shown.truncate(at);
        self.wrapped -= 1;
        if (self.selection.as_ref()).is_some_and(|s| s.anchor.0.max(s.head.0) >= at) {
            self.selection = None;
        }
        self.linked = None;
    }

    /// Back to the newest rows.
    pub fn latest(&mut self) {
        self.back = 0;
    }

    /// How many rows the view is up from the newest.
    pub fn back(&self) -> usize {
        self.back
    }

    /// The view `back` rows up from the newest, as it was before the rows
    /// were drawn again.
    pub fn hold(&mut self, back: usize) {
        self.back = back;
    }

    /// The view is a few screens from the first row, or nearer, and the
    /// rows above it can still be drawn: time to ask for older ones.
    pub fn near_top(&self) -> bool {
        let height = self.area.height as usize;
        !self.trimmed && self.back + AHEAD * height >= self.most(height)
    }

    /// The rows `older` was written with drawn above the first row, older
    /// than any shown, their marks kept, as the page `id`: the view left
    /// on the rows it shows.
    pub fn prepend(&mut self, id: u64, older: Screen) {
        if older.rows.is_empty() {
            return;
        }
        self.pages.insert(
            0,
            Page {
                id,
                rows: 0,
                drawn: true,
            },
        );
        self.placed = None;
        self.swap(0, older.rows);
        self.let_go();
    }

    /// The page `id`, let go before, drawn again with the rows `older` was
    /// written with.
    pub fn fill(&mut self, id: u64, older: Screen) {
        let Some(page) = self.pages.iter().position(|p| p.id == id && !p.drawn) else {
            return;
        };
        self.pages[page].drawn = true;
        self.swap(page, older.rows);
        self.let_go();
    }

    /// The pages of older items, from the first, and how many rows each
    /// is drawn on: to stand in for them once the rows are drawn anew.
    pub fn pages(&mut self) -> Vec<(u64, usize)> {
        let placed = self.placed();
        (self.pages.iter().enumerate())
            .map(|(p, page)| (page.id, placed.get(p).map_or(page.rows, |at| at.len())))
            .collect()
    }

    /// Blank rows over the rest for each of `pages`, as tall as it was,
    /// to be drawn again as the view nears it: the view stays as many
    /// rows up from the newest.
    pub fn blank(&mut self, pages: Vec<(u64, usize)>) {
        let blank = Arc::new(Line::default());
        let rows: usize = pages.iter().map(|(_, rows)| rows).sum();
        self.rows.splice(
            0..0,
            (0..rows).map(|_| Row {
                line: blank.clone(),
                rest: None,
                link: None,
                tag: None,
            }),
        );
        // Rows wrapped already are wrapped again under them.
        self.shown.clear();
        self.wrapped = 0;
        self.relaid = 0;
        self.selection = None;
        self.linked = None;
        for spot in [&mut self.go_to, &mut self.keep].into_iter().flatten() {
            spot.0 += rows;
        }
        if let Some(top) = &mut self.top_row {
            *top += rows;
        }
        let pages = pages.into_iter().map(|(id, rows)| Page {
            id,
            rows,
            drawn: false,
        });
        self.pages.splice(0..0, pages);
        self.placed = None;
        self.laid += 1;
    }

    /// A page let go that the view is near, the nearest: to draw again.
    pub fn wanted(&mut self) -> Option<u64> {
        let height = self.area.height as usize;
        let bottom = self.shown.len() - self.back.min(self.shown.len());
        let near = bottom.saturating_sub(height * (NEAR + 1))..bottom + height * NEAR;
        let placed = self.placed();
        (self.pages.iter().zip(placed))
            .filter(|(page, at)| !page.drawn && at.start < near.end && near.start < at.end)
            .min_by_key(|(_, at)| at.start.abs_diff(bottom).min(at.end.abs_diff(bottom)))
            .map(|(page, _)| page.id)
    }

    /// The rows of the page at `page` put in place of `rows`, all but its
    /// first row and the view where they were: those under the view count
    /// from the newest row, so the view moves with what is over them.
    fn swap(&mut self, page: usize, lines: Vec<Row>) {
        let row = self.pages[..page].iter().map(|p| p.rows).sum::<usize>();
        let rows = self.pages[page].rows;
        let at = self.placed().get(page).cloned().unwrap_or(0..0);
        let mut new = Self {
            rows: lines,
            ..Self::default()
        };
        // Rows already wrapped take theirs at the same width.
        let wrapped = self.wrapped >= row + rows && self.wrapped > 0;
        let mut shown = 0;
        if wrapped {
            new.wrap(self.width);
            shown = new.shown.len();
            self.shown.splice(at.clone(), new.shown);
            self.wrapped = self.wrapped + new.rows.len() - rows;
        }
        let added = new.rows.len();
        self.rows.splice(row..row + rows, new.rows);
        self.pages[page].rows = added;
        let grown = shown as isize - at.len() as isize;
        let moved = |spot: &mut usize, by: isize| *spot = spot.saturating_add_signed(by);
        // The view's first row stays: under the page, as far into it as it
        // was, or over it.
        let height = self.area.height as usize;
        let total = self.shown.len().saturating_add_signed(-grown);
        let first = total.saturating_sub(self.back + height);
        if wrapped && first < at.end {
            let first = match first < at.start {
                true => first,
                false => at.start + (first - at.start) * shown / at.len(),
            };
            self.back = self.shown.len().saturating_sub(first + height);
        }
        if self.top >= at.end {
            moved(&mut self.top, grown);
        }
        if let Some(selection) = &mut self.selection {
            let first = selection.anchor.0.min(selection.head.0);
            let last = selection.anchor.0.max(selection.head.0);
            if first >= at.end {
                moved(&mut selection.anchor.0, grown);
                moved(&mut selection.head.0, grown);
            } else if last >= at.start {
                self.selection = None;
            }
        }
        for spot in [&mut self.go_to, &mut self.keep].into_iter().flatten() {
            if spot.0 >= row + rows {
                moved(&mut spot.0, added as isize - rows as isize);
            }
        }
        if let Some(top) = &mut self.top_row
            && *top >= at.end
        {
            moved(top, grown);
        }
        self.linked = None;
        // The place found stays the one shown, those in the page counted.
        let found = (self.finding.as_ref())
            .filter(|f| wrapped && f.at != AGAIN && f.laid == self.laid)
            .map(|f| {
                let before = f.hits.partition_point(|(start, _)| start.0 < at.start);
                let after = f.hits.partition_point(|(start, _)| start.0 < at.end);
                (f.query.clone(), f.at, before, after - before)
            });
        self.laid += 1;
        if let Some((query, was, before, inside)) = found {
            let hits = self.hits(&query);
            let end = at.start + shown;
            let now = hits.partition_point(|(start, _)| start.0 < end) - before;
            let laid = self.laid;
            let finding = self.finding.as_mut().expect("a finding");
            finding.at = match was {
                was if was < before => was,
                was if was >= before + inside => was + now - inside,
                _ => before.min(hits.len().saturating_sub(1)),
            };
            finding.hits = hits;
            finding.laid = laid;
        }
    }

    /// Past what pages keep drawn, those farthest from the view let go,
    /// each for blank rows as tall, so nothing moves.
    fn let_go(&mut self) {
        let placed = self.placed();
        let drawn = |pages: &[Page]| -> usize {
            (pages.iter().zip(&placed))
                .filter(|(page, _)| page.drawn)
                .map(|(_, at)| at.len())
                .sum()
        };
        if placed.len() < self.pages.len() || drawn(&self.pages) <= PAGES_KEPT {
            return;
        }
        let height = self.area.height as usize;
        let bottom = self.shown.len() - self.back.min(self.shown.len());
        let near = bottom.saturating_sub(height * (NEAR + 1))..bottom + height * NEAR;
        let mut far: Vec<usize> = (0..self.pages.len())
            .filter(|&p| {
                let at = &placed[p];
                self.pages[p].drawn && !(at.start < near.end && near.start < at.end)
            })
            .collect();
        far.sort_by_key(|&p| std::cmp::Reverse(placed[p].start.abs_diff(bottom)));
        let blank = Arc::new(Line::default());
        for p in far {
            if drawn(&self.pages) <= PAGES_KEPT {
                break;
            }
            let at = placed[p].clone();
            let row = self.pages[..p].iter().map(|p| p.rows).sum::<usize>();
            let rows = self.pages[p].rows;
            self.rows.splice(
                row..row + rows,
                (0..at.len()).map(|_| Row {
                    line: blank.clone(),
                    rest: None,
                    link: None,
                    tag: None,
                }),
            );
            self.shown.splice(
                at.clone(),
                (0..at.len()).map(|_| Shown {
                    line: blank.clone(),
                    link: None,
                    joins: None,
                    opens: 0,
                    first: true,
                }),
            );
            self.wrapped = self.wrapped + at.len() - rows;
            self.pages[p].rows = at.len();
            self.pages[p].drawn = false;
            if let Some(selection) = &self.selection
                && selection.anchor.0.max(selection.head.0) >= at.start
                && selection.anchor.0.min(selection.head.0) < at.end
            {
                self.selection = None;
            }
            // The place found, if in the page, is the next one out of it.
            if let Some(f) = &mut self.finding
                && f.laid == self.laid
                && f.at != AGAIN
            {
                let before = f.hits.partition_point(|(start, _)| start.0 < at.start);
                let after = f.hits.partition_point(|(start, _)| start.0 < at.end);
                f.hits.drain(before..after);
                f.at = match f.at {
                    at if at < before => at,
                    at if at >= after => at - (after - before),
                    _ => before.min(f.hits.len().saturating_sub(1)),
                };
                f.laid = self.laid + 1;
            }
            self.linked = None;
            self.laid += 1;
        }
        self.placed = None;
    }

    /// Where each page is drawn in `shown`, once its rows are wrapped.
    fn placed(&mut self) -> Vec<std::ops::Range<usize>> {
        if let Some((laid, placed)) = &self.placed
            && *laid == self.laid
        {
            return placed.clone();
        }
        let mut placed = Vec::new();
        let (mut row, mut at) = (0, 0);
        for page in &self.pages {
            let end = row + page.rows;
            if end > self.wrapped {
                break;
            }
            let start = at;
            while row < end {
                at += 1;
                while self.shown.get(at).is_some_and(|shown| !shown.first) {
                    at += 1;
                }
                row += 1;
            }
            placed.push(start..at);
        }
        self.placed = Some((self.laid, placed.clone()));
        placed
    }

    /// Up (`rows` > 0) or down the conversation.
    pub fn scroll(&mut self, rows: isize) {
        let most = self.most(self.area.height as usize);
        self.back = self.back.saturating_add_signed(rows).min(most);
    }

    /// How far back the view can go with `height` rows: to the first row.
    fn most(&self, height: usize) -> usize {
        self.shown.len().saturating_sub(height)
    }

    /// A page up or down.
    pub fn page(&mut self, up: bool) {
        let page = (self.area.height as isize - 2).max(1);
        self.scroll(if up { page } else { -page });
    }

    /// Let go of the selection; whether there was one.
    pub fn unselect(&mut self) -> bool {
        self.selection.take().is_some()
    }

    /// The selected text, as it was written.
    pub fn selected(&self) -> Option<String> {
        let selection = self.selection.as_ref()?;
        let (start, end) = self.extent(selection);
        let text = self.text(start, end);
        (!text.trim().is_empty()).then_some(text)
    }

    /// How many rows the conversation takes at `width`.
    pub fn height(&mut self, width: usize) -> usize {
        self.layout(width);
        self.shown.len()
    }

    /// The rows wrapped to `width`, those written since kept in view while
    /// the view is scrolled up, and the oldest let go once there are too
    /// many.
    fn layout(&mut self, width: usize) {
        self.wrap(width);
        self.trim();
    }

    /// The rows not yet wrapped to `width` wrapped, all of them when it
    /// changed.
    fn wrap(&mut self, width: usize) {
        if width != self.width {
            self.width = width;
            self.shown.clear();
            self.wrapped = 0;
            self.relaid = 0;
            self.selection = None;
            self.linked = None;
            self.laid += 1;
        }
        let before = self.shown.len();
        self.laid += usize::from(self.wrapped < self.rows.len());
        for row in &self.rows[self.wrapped..] {
            let line = row.line.clone();
            // A picture's row is cut at the edge, not wrapped into pieces:
            // Kitty's placeholders, or one drawn in half blocks.
            let picture = (line.spans.iter()).any(|span| {
                span.content.starts_with(crate::image::PLACEHOLDER)
                    || span.content.contains(['▀', '▄'])
            });
            let link = row.link.clone();
            let rows = match line.width() <= width || picture {
                true => vec![line],
                false => {
                    let rest = row.rest.clone().unwrap_or(Line::from("    "));
                    let line = Arc::unwrap_or_clone(line);
                    (crate::transcript::wrap_lines(line, width, Line::default(), rest))
                        .into_iter()
                        .map(Arc::new)
                        .collect()
                }
            };
            for (at, line) in rows.into_iter().enumerate() {
                let joins = crate::transcript::lead(&line);
                let opens = crate::transcript::opens(&line).unwrap_or(0);
                let first = at == 0;
                self.shown.push(Shown {
                    line,
                    link: link.clone(),
                    joins,
                    opens,
                    first,
                });
            }
        }
        self.wrapped = self.rows.len();
        let relaid = std::mem::take(&mut self.relaid);
        if self.back > 0 && before > 0 {
            self.back =
                (self.back.saturating_add(self.shown.len() - before)).saturating_sub(relaid);
        }
    }

    /// The oldest rows let go, a batch at a time, once new ones written
    /// pass what is kept and the view is below them: not for older ones
    /// drawn above, nor for the rows wrapped anew at another width.
    fn trim(&mut self) {
        let paged = self.pages.iter().map(|p| p.rows).sum::<usize>();
        let rows = self.rows.len() - paged;
        if std::mem::take(&mut self.grew) && rows > KEEP + KEEP / 4 {
            // The pages over the rows that go, go with them.
            let drop = paged + rows - KEEP;
            let mut firsts = 0;
            let gone = (self.shown.iter())
                .position(|shown| {
                    firsts += usize::from(shown.first);
                    firsts > drop
                })
                .unwrap_or(self.shown.len());
            let view = self
                .shown
                .len()
                .saturating_sub(self.back + self.area.height as usize);
            if gone <= view {
                self.rows.drain(..drop);
                self.wrapped -= drop;
                self.shown.drain(..gone);
                self.pages.clear();
                self.placed = None;
                self.trimmed = true;
                self.selection = None;
                self.linked = None;
                self.laid += 1;
            }
        }
    }

    /// Look for `query` in the rows, case aside unless it has a capital,
    /// and show the newest place it is, at the bottom of the view or above.
    pub fn find(&mut self, query: &str) {
        let bottom = self.shown.len() - self.back.min(self.shown.len());
        let hits = self.hits(query);
        let at = nearest(&hits, bottom);
        self.finding = Some(Finding {
            query: query.to_owned(),
            hits,
            at,
            laid: self.laid,
        });
        self.reveal();
    }

    /// The next place the text found is: older, or newer, round to the
    /// other end past the last.
    pub fn find_next(&mut self, older: bool) {
        let Some(finding) = &mut self.finding else {
            return;
        };
        let count = finding.hits.len();
        if count == 0 {
            return;
        }
        finding.at = match older {
            true => (finding.at + count - 1) % count,
            false => (finding.at + 1) % count,
        };
        self.reveal();
    }

    /// Stop finding, the view left where it is.
    pub fn end_find(&mut self) {
        self.finding = None;
    }

    /// Which place of how many the view shows, when finding.
    pub fn found(&self) -> Option<(usize, usize)> {
        let finding = self.finding.as_ref()?;
        let count = finding.hits.len();
        Some(((finding.at + 1).min(count), count))
    }

    /// The view moved so the place found is in it, midway if it was out.
    fn reveal(&mut self) {
        let Some(&(start, end)) = (self.finding.as_ref()).and_then(|f| f.hits.get(f.at)) else {
            return;
        };
        let height = self.area.height as usize;
        let bottom = self.shown.len() - self.back.min(self.shown.len());
        if start.0 >= bottom.saturating_sub(height) && end.0 < bottom {
            return;
        }
        let bottom = (start.0 + height.div_ceil(2)).min(self.shown.len());
        self.back = (self.shown.len() - bottom).min(self.most(height));
    }

    /// Every place `query` is in the rows, read whole over the rows a line
    /// was wrapped onto, without their leads.
    fn hits(&self, query: &str) -> Vec<(Spot, Spot)> {
        if query.is_empty() {
            return Vec::new();
        }
        let fold = !query.chars().any(char::is_uppercase);
        let query = match fold {
            true => query.to_lowercase(),
            false => query.to_owned(),
        };
        let mut hits = Vec::new();
        // Each line's text, read whole over its rows; only a line the
        // query is in is read again for where its characters are drawn.
        let mut text = String::new();
        let mut first = 0;
        for row in 0..=self.shown.len() {
            let shown = self.shown.get(row);
            if shown.is_none_or(|shown| shown.joins.is_none()) {
                if text.contains(&query) {
                    self.place(&query, fold, first..row, &mut hits);
                }
                text.clear();
                first = row;
            }
            let Some(shown) = shown else {
                break;
            };
            let spans = shown.line.spans.iter();
            if spans
                .clone()
                .any(|s| s.content.contains(crate::image::PLACEHOLDER))
            {
                continue;
            }
            // Past the row's lead, as its cells are below.
            let mut column = 0;
            let lead = shown.joins.unwrap_or(0);
            for span in spans {
                let content = span.content.as_ref();
                if column >= lead && content.is_ascii() {
                    let from = text.len();
                    text.push_str(content);
                    if fold {
                        text[from..].make_ascii_lowercase();
                    }
                    continue;
                }
                for c in content.chars() {
                    if column < lead {
                        column += c.width().unwrap_or(0);
                        continue;
                    }
                    match fold {
                        true => text.extend(c.to_lowercase()),
                        false => text.push(c),
                    }
                }
            }
        }
        hits
    }

    /// Where `query` is in the line drawn over `rows`, added to `hits`.
    fn place(
        &self,
        query: &str,
        fold: bool,
        rows: std::ops::Range<usize>,
        hits: &mut Vec<(Spot, Spot)>,
    ) {
        // The line's text, and where each of its characters is drawn.
        let (mut text, mut at) = (String::new(), Vec::<(usize, Spot, usize)>::new());
        for row in rows {
            let shown = &self.shown[row];
            let spans = shown.line.spans.iter();
            if spans
                .clone()
                .any(|s| s.content.contains(crate::image::PLACEHOLDER))
            {
                continue;
            }
            let lead = shown.joins.unwrap_or(0);
            let mut column = 0;
            for g in spans.flat_map(|span| span.content.graphemes(true)) {
                // A mark that takes no cell goes with the one before.
                let wide = match (g.width(), column) {
                    (0, 0) => 1,
                    (0, _) => 0,
                    (wide, _) => wide,
                };
                if column < lead || (wide == 0 && column == lead) {
                    column += wide;
                    continue;
                }
                if wide > 0 {
                    at.push((text.len(), (row, column), wide));
                }
                match fold {
                    true => text.extend(g.chars().flat_map(char::to_lowercase)),
                    false => text.push_str(g),
                }
                column += wide;
            }
        }
        for (from, found) in text.match_indices(query) {
            let cell = |byte| at.partition_point(|(b, _, _)| *b <= byte) - 1;
            let start = at[cell(from)].1;
            let (_, (row, column), wide) = at[cell(from + found.len() - 1)];
            hits.push((start, (row, column + wide)));
        }
    }

    /// Draw the rows that fit `area` of `buf`: the newest, or those the
    /// view is scrolled to, the selection reversed and links underlined.
    /// `file` is where a word that may be a path opens, if it is one.
    pub fn render(&mut self, area: Rect, buf: &mut Buffer, file: impl Fn(&str) -> Option<String>) {
        self.layout(area.width as usize);
        let height = area.height as usize;
        if let Some((row, above)) = self.go_to.take() {
            let at = (self.shown.iter().enumerate())
                .filter(|(_, shown)| shown.first)
                .nth(row)
                .map_or(0, |(at, _)| at)
                .saturating_sub(above);
            self.back = self.shown.len().saturating_sub(at + height);
        }
        if let Some(top) = self.top_row.take() {
            self.back = self.shown.len().saturating_sub(top + height);
        }
        if let Some((row, below)) = self.keep.take() {
            let at = (self.shown.iter().enumerate())
                .filter(|(_, shown)| shown.first)
                .nth(row)
                .map_or(0, |(at, _)| at);
            let bottom = self.shown.len() - self.back.min(self.shown.len());
            let top = bottom.saturating_sub(height);
            let end = (at + below + 1).min(self.shown.len());
            // Near the top it goes all the way, the heading above it shown.
            if at < top && end <= height {
                self.back = self.most(height);
            } else if at < top {
                self.back = self.shown.len().saturating_sub(at + height);
            } else if end > bottom {
                self.back = self.shown.len() - end;
            }
        }
        self.back = self.back.min(self.most(height));
        let bottom = self.shown.len() - self.back;
        let top = bottom.saturating_sub(height);
        self.area = area;
        self.top = top;
        self.drawn = bottom - top;
        for (y, shown) in self.shown[top..bottom].iter().enumerate() {
            buf.set_line(area.x, area.y + y as u16, &shown.line, area.width);
        }
        let drawn = Rect {
            height: (bottom - top) as u16,
            ..area
        };
        if self.linked != Some((top, bottom, self.width, area)) {
            self.linked = Some((top, bottom, self.width, area));
            self.links = self.find_links(drawn, &file);
        }
        for (y, from, to, url) in &self.links {
            if url.starts_with(BUTTON) {
                continue;
            }
            let row = top + (y - area.y) as usize;
            if self
                .shown
                .get(row)
                .is_some_and(|shown| shown.link.is_some())
            {
                continue;
            }
            for x in *from..*to {
                let cell = &mut buf[(x, *y)];
                // A picture is not underlined: the line would cross it.
                if !cell.symbol().starts_with(crate::image::PLACEHOLDER) {
                    cell.set_style(cell.style().add_modifier(Modifier::UNDERLINED));
                }
            }
        }
        if let Some(finding) = &self.finding
            && finding.laid != self.laid
        {
            let (query, at) = (finding.query.clone(), finding.at);
            let hits = self.hits(&query);
            self.finding = Some(Finding {
                at: match at {
                    AGAIN => nearest(&hits, bottom),
                    at => at.min(hits.len().saturating_sub(1)),
                },
                query,
                hits,
                laid: self.laid,
            });
        }
        if let Some(finding) = &self.finding {
            for (i, (start, end)) in finding.hits.iter().enumerate() {
                if end.0 < top || start.0 >= bottom {
                    continue;
                }
                let style = match i == finding.at {
                    true => Style::new().fg(Color::Black).bg(Color::Yellow),
                    false => Style::new().fg(Color::Yellow).underlined(),
                };
                for row in start.0.max(top)..=end.0.min(bottom - 1) {
                    let from = if row == start.0 { start.1 } else { 0 };
                    let to = match row == end.0 {
                        true => end.1,
                        false => self.shown[row].line.width(),
                    };
                    let y = area.y + (row - top) as u16;
                    for x in from..to.min(area.width as usize) {
                        buf[(area.x + x as u16, y)].set_style(style);
                    }
                }
            }
        }
        if let Some(selection) = &self.selection {
            let (start, end) = self.extent(selection);
            for row in start.0.max(top)..=end.0.min(bottom.saturating_sub(1)) {
                if row < start.0 || row >= bottom {
                    continue;
                }
                let from = if row == start.0 { start.1 } else { 0 };
                let to = match row == end.0 {
                    true => end.1,
                    false => self.shown[row].line.width().max(from + 1),
                };
                let y = area.y + (row - top) as u16;
                for x in from..to.min(area.width as usize) {
                    let cell = &mut buf[(area.x + x as u16, y)];
                    // One look whatever the text's colors, as a terminal's own.
                    cell.set_fg(Color::Reset).set_bg(Color::Reset);
                    cell.modifier = (cell.modifier - Modifier::DIM) | Modifier::REVERSED;
                }
            }
        }
    }

    /// The cells of the links drawn last, from `buf` they were drawn in,
    /// each link's first opening an OSC 8 hyperlink and its last closing
    /// it: written again over themselves, they let the terminal's own
    /// link click (Cmd+click) open the file or page outside Sonny.
    pub fn link_cells(&self, buf: &Buffer) -> Vec<(u16, u16, Cell)> {
        let mut cells = Vec::new();
        for (y, from, to, url) in self
            .links
            .iter()
            .filter(|(.., url)| !url.starts_with(BUTTON))
        {
            // The line a preview opens at is no part of the file's address.
            let url = match url.starts_with("file://") {
                true => url.split('#').next().unwrap_or(url),
                false => url,
            };
            // The cells a character starts in: the column a wide one
            // goes on into is drawn by it, and written over would cut it.
            let mut glyphs = Vec::new();
            let mut x = *from;
            while x < *to {
                glyphs.push(x);
                x += buf[(x, *y)].symbol().width().max(1) as u16;
            }
            let last = glyphs.len().saturating_sub(1);
            for (at, x) in glyphs.into_iter().enumerate() {
                let mut cell = buf[(x, *y)].clone();
                let mut symbol = cell.symbol().to_owned();
                if at == 0 {
                    symbol = format!("\x1b]8;;{url}\x1b\\{symbol}");
                }
                if at == last {
                    symbol.push_str("\x1b]8;;\x1b\\");
                }
                cell.set_symbol(&symbol);
                cells.push((x, *y, cell));
            }
        }
        cells
    }

    /// The links in the rows just drawn, as `links::cells` finds them.
    /// An address cut over rows is read whole, rows past the screen's
    /// edges included, and only its part on screen is kept.
    fn find_links(
        &self,
        drawn: Rect,
        file: impl Fn(&str) -> Option<String>,
    ) -> Vec<(u16, u16, u16, String)> {
        if drawn.height == 0 {
            return Vec::new();
        }
        let cut = |row: usize| self.cut(row);
        let (top, bottom) = (self.top, self.top + drawn.height as usize);
        let mut from = top;
        while from > 0 && top - from < LINK_ROWS && cut(from) {
            from -= 1;
        }
        let mut to = bottom;
        while to < self.shown.len() && to - bottom < LINK_ROWS && cut(to) {
            to += 1;
        }
        let mut rows = Buffer::empty(Rect::new(0, 0, drawn.width, (to - from) as u16));
        for row in from..to {
            rows.set_line(0, (row - from) as u16, &self.shown[row].line, drawn.width);
        }
        let cuts: Vec<bool> = (from..to).map(cut).collect();
        let mut links: Vec<(u16, u16, u16, String)> = Vec::new();
        for (x, y, cell) in crate::links::cells(&rows, &cuts, file) {
            let row = from + y as usize;
            if !(top..bottom).contains(&row) {
                continue;
            }
            let (x, y) = (drawn.x + x, drawn.y + (row - top) as u16);
            if let Some(rest) = cell.symbol().strip_prefix("\x1b]8;;") {
                let url = rest.split('\x1b').next().unwrap_or_default().to_owned();
                links.push((y, x, x + 1, url));
            }
            if let Some(last) = links.last_mut()
                && last.0 == y
            {
                last.2 = x + 1;
            }
        }
        // A row its line links opens there across its text, whatever was
        // read from its words.
        for row in top..bottom {
            let Some(url) = &self.shown[row].link else {
                continue;
            };
            let y = drawn.y + (row - top) as u16;
            let text: String = (self.shown[row].line.spans.iter())
                .map(|span| span.content.as_ref())
                .collect();
            let from = text.width() - text.trim_start().width();
            let to = text.trim_end().width().min(drawn.width as usize);
            links.retain(|link| link.0 != y);
            if from < to {
                links.push((y, drawn.x + from as u16, drawn.x + to as u16, url.clone()));
            }
        }
        // A picture opens what it was drawn from, bigger, in the pane.
        for row in top..bottom {
            let y = drawn.y + (row - top) as u16;
            let mut x = 0;
            for span in &self.shown[row].line.spans {
                let width = span.width();
                if let Some(url) = crate::image::id(span).and_then(crate::image::source) {
                    let to = (x + width).min(drawn.width as usize);
                    if x < to {
                        links.push((y, drawn.x + x as u16, drawn.x + to as u16, url));
                    }
                }
                x += width;
            }
        }
        links
    }

    /// Where on the screen's rows `(column, row)` falls, the column kept
    /// within the row's text.
    fn spot(&self, column: u16, row: u16) -> Option<Spot> {
        if self.shown.is_empty() {
            return None;
        }
        let y = (row.saturating_sub(self.area.y) as usize).min(self.drawn.saturating_sub(1));
        let at = (self.top + y).min(self.shown.len() - 1);
        let x = column.saturating_sub(self.area.x) as usize;
        Some((at, x.min(self.shown[at].line.width())))
    }

    /// The mouse, over the screen. Presses outside the conversation's
    /// rows let go of the selection.
    pub fn mouse(&mut self, event: ratatui::crossterm::event::MouseEvent) -> Option<Action> {
        use ratatui::crossterm::event::{KeyModifiers, MouseButton, MouseEventKind};
        let inside = self.area.contains((event.column, event.row).into());
        match event.kind {
            MouseEventKind::ScrollUp => self.scroll(WHEEL as isize),
            MouseEventKind::ScrollDown => self.scroll(-(WHEEL as isize)),
            MouseEventKind::Down(MouseButton::Left) if !inside => {
                self.selection = None;
            }
            MouseEventKind::Down(MouseButton::Left) => {
                let spot = self.spot(event.column, event.row)?;
                let now = Instant::now();
                let clicks = match self.clicks {
                    Some((at, x, y, n))
                        if now.duration_since(at) < DOUBLE
                            && (x, y) == (event.column, event.row) =>
                    {
                        n % 3 + 1
                    }
                    _ => 1,
                };
                self.clicks = Some((now, event.column, event.row, clicks));
                if clicks == 1
                    && event.modifiers.contains(KeyModifiers::SHIFT)
                    && let Some(selection) = &mut self.selection
                {
                    selection.head = spot;
                    selection.dragged = true;
                    return None;
                }
                let unit = match clicks {
                    2 => Unit::Word,
                    3 => Unit::Line,
                    _ => Unit::Cell,
                };
                self.selection = Some(Selection {
                    anchor: spot,
                    head: spot,
                    unit,
                    dragged: false,
                });
            }
            MouseEventKind::Drag(MouseButton::Left) => {
                // Past the top or bottom edge, the view moves with it.
                if event.row < self.area.y {
                    self.scroll(1);
                } else if event.row >= self.area.y + self.drawn as u16 {
                    self.scroll(-1);
                }
                let spot = self.spot(event.column, event.row)?;
                let selection = self.selection.as_mut()?;
                if spot != selection.head {
                    selection.head = spot;
                    selection.dragged = true;
                }
            }
            MouseEventKind::Up(MouseButton::Left) => {
                let selection = self.selection.as_ref()?;
                if selection.dragged || selection.unit != Unit::Cell {
                    return self.selected().map(Action::Copy);
                }
                // A click: the link under it, if any.
                self.selection = None;
                return (self.links.iter())
                    .find(|(y, from, to, _)| {
                        *y == event.row && (*from..*to).contains(&event.column)
                    })
                    .map(|(.., url)| Action::Open(url.clone()));
            }
            _ => {}
        }
        None
    }

    /// The selection's first spot and the one after its last, its unit
    /// taken in whole.
    fn extent(&self, selection: &Selection) -> (Spot, Spot) {
        let (mut start, mut end) = match selection.anchor <= selection.head {
            true => (selection.anchor, selection.head),
            false => (selection.head, selection.anchor),
        };
        match selection.unit {
            // The cell under the mouse is selected too.
            Unit::Cell => end.1 = (end.1 + 1).min(self.shown[end.0].line.width()),
            Unit::Word => {
                start = self.word(start, true);
                end = self.word(end, false);
            }
            Unit::Line => {
                while start.0 > 0 && self.shown[start.0].joins.is_some() {
                    start.0 -= 1;
                }
                while (self.shown.get(end.0 + 1)).is_some_and(|next| next.joins.is_some()) {
                    end.0 += 1;
                }
                start.1 = 0;
                end.1 = self.shown[end.0].line.width();
            }
        }
        (start, end)
    }

    /// The row goes on with a word cut at the end of the row above.
    fn cut(&self, row: usize) -> bool {
        (self.shown.get(row))
            .is_some_and(|shown| shown.line.alignment == Some(crate::transcript::CUT))
    }

    /// Where the word at `spot` starts, or ends: a run without spaces, so
    /// a path or an address is one word, a word cut over rows whole, less
    /// the punctuation around it.
    fn word(&self, (row, column): Spot, starts: bool) -> Spot {
        let row_cells = |r: usize| cells(&self.shown[r].line);
        // A wide character's second column is part of it.
        let word = |g: &String| g.is_empty() || !g.trim().is_empty();
        // The word's cells, in order, and how many are before `column`.
        let mut before: Vec<(Spot, String)> = Vec::new();
        let (mut r, mut here) = (row, row_cells(row));
        let mut c = column.min(here.len());
        loop {
            while c > 0 && word(&here[c - 1]) {
                c -= 1;
                before.push(((r, c), here[c].clone()));
            }
            // Only the row's lead before it, in a row a word was cut onto.
            if r == 0 || !self.cut(r) || here[..c].iter().any(word) {
                break;
            }
            r -= 1;
            here = row_cells(r);
            c = here.len();
            if !here.last().is_some_and(word) {
                break;
            }
        }
        before.reverse();
        let clicked = before.len();
        let mut cells = before;
        let (mut r, mut here) = (row, row_cells(row));
        let mut c = column.min(here.len());
        loop {
            while c < here.len() && word(&here[c]) {
                cells.push(((r, c), here[c].clone()));
                c += 1;
            }
            if c < here.len() || !self.cut(r + 1) {
                break;
            }
            r += 1;
            here = row_cells(r);
            c = self.shown[r].joins.unwrap_or(0);
        }
        let is = |at: usize, set: &str| {
            let g = cells[at].1.as_str();
            !g.is_empty() && set.contains(g)
        };
        let (mut first, mut last) = (0, cells.len());
        while first < clicked && is(first, "(\"'[`<") {
            first += 1;
        }
        // A closing bracket the word opens stays: `len(values)`.
        let opened = |end: usize, open: &str| cells[first..end].iter().any(|(_, g)| g == open);
        while last > clicked + 1
            && is(last - 1, ".,;:!?)\"']`>")
            && !(is(last - 1, ")") && opened(last - 1, "("))
            && !(is(last - 1, "]") && opened(last - 1, "["))
        {
            last -= 1;
        }
        match starts {
            true => cells.get(first).map_or((row, column), |(spot, _)| *spot),
            false if last > clicked => {
                let ((r, c), _) = cells[last - 1];
                // A wide character's second column goes with it.
                let here = row_cells(r);
                let mut end = c + 1;
                while here.get(end).is_some_and(|g| g.is_empty()) {
                    end += 1;
                }
                (r, end)
            }
            false => (row, column),
        }
    }

    /// The text from `start` to `end`: rows a line was wrapped onto join
    /// it again without their leads, pictures are left out, and the indent
    /// the whole lines share goes.
    fn text(&self, start: Spot, end: Spot) -> String {
        // Each line, and whether it starts at its row's start.
        let mut lines: Vec<(String, bool)> = Vec::new();
        for row in start.0..=end.0 {
            let shown = &self.shown[row];
            let cells = cells(&shown.line);
            if cells
                .iter()
                .any(|g| g.starts_with(crate::image::PLACEHOLDER))
            {
                continue;
            }
            let lead = shown.joins.unwrap_or(shown.opens);
            let picked = if row == start.0 { start.1 } else { 0 };
            let from = picked.max(lead).min(cells.len());
            let to = if row == end.0 { end.1 } else { cells.len() };
            let part: String = cells[from..to.clamp(from, cells.len())].concat();
            match lines.last_mut() {
                Some((line, _)) if row > start.0 && shown.joins.is_some() => line.push_str(&part),
                _ => lines.push((part, picked <= lead)),
            }
        }
        let indent = |line: &str| line.len() - line.trim_start_matches(' ').len();
        let shared = (lines.iter())
            .filter(|(line, whole)| *whole && !line.trim().is_empty())
            .map(|(line, _)| indent(line))
            .min()
            .unwrap_or(0);
        lines
            .into_iter()
            .map(|(line, whole)| {
                let line = if whole {
                    &line[shared.min(indent(&line))..]
                } else {
                    &line[..]
                };
                line.trim_end().to_owned()
            })
            .collect::<Vec<_>>()
            .join("\n")
    }
}

/// A row's characters, one per column: a wide character, then empty
/// strings for the columns it covers.
/// A row a line was wrapped onto, as its lead of `columns` and the text
/// after it.
fn split(line: Line<'static>, columns: usize) -> (Line<'static>, Vec<Span<'static>>) {
    let (mut lead, mut text) = (Vec::new(), Vec::new());
    let mut left = columns;
    for span in line.spans {
        let style = line.style.patch(span.style);
        if span.content.is_empty() {
            continue;
        }
        if left == 0 {
            text.push(Span::styled(span.content, style));
            continue;
        }
        let mut cut = span.content.len();
        let mut used = 0;
        for (at, grapheme) in span.content.grapheme_indices(true) {
            if used >= left {
                cut = at;
                break;
            }
            used += grapheme.width();
        }
        left -= used.min(left);
        let (before, after) = span.content.split_at(cut);
        lead.push(Span::styled(before.to_owned(), style));
        if !after.is_empty() {
            text.push(Span::styled(after.to_owned(), style));
        }
    }
    (Line::from(lead), text)
}

fn cells(line: &Line) -> Vec<String> {
    let mut out = Vec::new();
    for span in &line.spans {
        for g in span.content.graphemes(true) {
            let wide = g.width();
            if wide == 0 {
                match out.last_mut() {
                    Some(last) => *last += g,
                    None => out.push(g.to_owned()),
                }
                continue;
            }
            out.push(g.to_owned());
            out.extend(std::iter::repeat_n(String::new(), wide - 1));
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use ratatui::crossterm::event::{KeyModifiers, MouseButton, MouseEvent, MouseEventKind};

    fn screen(lines: Vec<Line<'static>>, width: u16, height: u16) -> (Screen, Buffer) {
        let mut screen = Screen::default();
        screen.push(lines);
        let area = Rect::new(0, 0, width, height);
        let mut buf = Buffer::empty(area);
        screen.render(area, &mut buf, |_| None);
        (screen, buf)
    }

    #[test]
    fn a_click_on_a_picture_opens_what_it_was_drawn_from() {
        let rows = crate::image::linked_rows(0x00FD_0001, 4, 2, "file:///w/a.mmd");
        let (mut screen, mut buf) = screen(rows, 20, 4);
        screen.render(Rect::new(0, 0, 20, 4), &mut buf, |_| None);
        // Not underlined: a line would cross the picture.
        assert!(!buf[(3, 0)].modifier.contains(Modifier::UNDERLINED));
        screen.mouse(mouse(MouseEventKind::Down(MouseButton::Left), 3, 1));
        let opened = screen.mouse(mouse(MouseEventKind::Up(MouseButton::Left), 3, 1));
        assert_eq!(opened, Some(Action::Open("file:///w/a.mmd".into())));
        // Its lead is no part of it.
        screen.mouse(mouse(MouseEventKind::Down(MouseButton::Left), 0, 1));
        assert_eq!(
            screen.mouse(mouse(MouseEventKind::Up(MouseButton::Left), 0, 1)),
            None
        );
    }

    fn mouse(kind: MouseEventKind, column: u16, row: u16) -> MouseEvent {
        MouseEvent {
            kind,
            column,
            row,
            modifiers: KeyModifiers::NONE,
        }
    }

    fn drag(screen: &mut Screen, from: (u16, u16), to: (u16, u16)) -> Option<Action> {
        screen.mouse(mouse(
            MouseEventKind::Down(MouseButton::Left),
            from.0,
            from.1,
        ));
        screen.mouse(mouse(MouseEventKind::Drag(MouseButton::Left), to.0, to.1));
        screen.mouse(mouse(MouseEventKind::Up(MouseButton::Left), to.0, to.1))
    }

    fn row(buf: &Buffer, y: u16) -> String {
        (0..buf.area.width).map(|x| buf[(x, y)].symbol()).collect()
    }

    #[test]
    fn a_wrapped_reply_copies_as_one_line_without_its_indent() {
        let reply = crate::transcript::reply(
            "The stub answers after the 2s timeout, so the test reads an empty cart.",
            30,
            std::path::Path::new("."),
        );
        let (mut screen, buf) = screen(reply, 30, 10);
        assert!(row(&buf, 1).starts_with("  "), "{:?}", row(&buf, 1));
        let last = screen.shown.len() as u16 - 1;
        let copied = drag(&mut screen, (0, 0), (29, last));
        assert_eq!(
            copied,
            Some(Action::Copy(
                "The stub answers after the 2s timeout, so the test reads an empty cart.".into()
            ))
        );
    }

    #[test]
    fn lines_keep_their_breaks_and_the_indent_they_do_not_share() {
        let lines = vec![
            Line::from("  fn main() {"),
            Line::from("      run();"),
            Line::from("  }"),
        ];
        let (mut screen, _) = screen(lines, 40, 5);
        assert_eq!(
            drag(&mut screen, (0, 0), (39, 2)),
            Some(Action::Copy("fn main() {\n    run();\n}".into()))
        );
    }

    #[test]
    fn a_double_click_takes_a_word_and_a_triple_click_its_line() {
        let lines = crate::transcript::wrap(
            Line::from("  Raise the timeout in tests/cart.rs:41 or make the stub answer at once."),
            30,
            Span::raw(""),
            Span::raw("  "),
        );
        let (mut screen, _) = screen(lines, 30, 5);
        let click = |screen: &mut Screen| {
            screen.mouse(mouse(MouseEventKind::Down(MouseButton::Left), 5, 1));
            screen.mouse(mouse(MouseEventKind::Up(MouseButton::Left), 5, 1))
        };
        assert_eq!(click(&mut screen), None);
        assert_eq!(
            click(&mut screen),
            Some(Action::Copy("tests/cart.rs:41".into()))
        );
        assert_eq!(
            click(&mut screen),
            Some(Action::Copy(
                "Raise the timeout in tests/cart.rs:41 or make the stub answer at once.".into()
            ))
        );
    }

    #[test]
    fn a_double_click_leaves_the_punctuation_around_a_word() {
        let lines = vec![Line::from("  - (calc.py): divide by len(values).")];
        let (mut screen, _) = screen(lines, 50, 3);
        let double = |screen: &mut Screen, x| {
            for _ in 0..2 {
                screen.mouse(mouse(MouseEventKind::Down(MouseButton::Left), x, 0));
                screen.mouse(mouse(MouseEventKind::Up(MouseButton::Left), x, 0));
            }
            screen.selected()
        };
        assert_eq!(double(&mut screen, 7).as_deref(), Some("calc.py"));
        screen.clicks = None;
        assert_eq!(double(&mut screen, 30).as_deref(), Some("len(values)"));
    }

    #[test]
    fn a_click_opens_the_link_under_it_and_a_drag_over_it_selects() {
        let lines = vec![Line::from("  see https://herdr.dev/docs for more")];
        let (mut screen, buf) = screen(lines, 50, 3);
        assert!(buf[(10, 0)].modifier.contains(Modifier::UNDERLINED));
        screen.mouse(mouse(MouseEventKind::Down(MouseButton::Left), 10, 0));
        assert_eq!(
            screen.mouse(mouse(MouseEventKind::Up(MouseButton::Left), 10, 0)),
            Some(Action::Open("https://herdr.dev/docs".into()))
        );
        assert_eq!(
            drag(&mut screen, (2, 0), (5, 0)),
            Some(Action::Copy("see".into()))
        );
    }

    #[test]
    fn a_link_cut_at_the_screens_edge_opens_whole() {
        let url = "https://example.com/a/very/long/path/to/a/page";
        let lines = crate::transcript::wrap(Line::from(url), 20, Span::raw(""), Span::raw(""));
        assert_eq!(lines.len(), 3);
        let click = |screen: &mut Screen| {
            screen.mouse(mouse(MouseEventKind::Down(MouseButton::Left), 2, 0));
            screen.mouse(mouse(MouseEventKind::Up(MouseButton::Left), 2, 0))
        };
        // Its start above the screen.
        let (mut screen, _) = screen(lines.clone(), 20, 2);
        assert_eq!(click(&mut screen), Some(Action::Open(url.into())));
        // Its end below.
        let mut screen = Screen::default();
        screen.push(lines);
        let area = Rect::new(0, 0, 20, 2);
        screen.render(area, &mut Buffer::empty(area), |_| None);
        screen.scroll(1);
        screen.render(area, &mut Buffer::empty(area), |_| None);
        assert_eq!(click(&mut screen), Some(Action::Open(url.into())));
    }

    #[test]
    fn a_wider_screen_joins_the_rows_a_line_was_wrapped_onto() {
        let text = "The stub answers after the 2s timeout, so the test reads an empty cart.";
        let reply = crate::transcript::reply(text, 30, std::path::Path::new("."));
        assert!(reply.len() > 2);
        let (screen, buf) = screen(reply, 80, 5);
        assert_eq!(screen.shown.len(), 1);
        assert_eq!(row(&buf, 0).trim_end(), format!("• {text}"));
    }

    #[test]
    fn a_word_cut_over_rows_keeps_the_bracket_it_opens() {
        let word = "len(abcdefghijklmnopqrstuvwxyz)";
        let lines = crate::transcript::wrap(
            Line::from(format!("{word}.")),
            20,
            Span::raw(""),
            Span::raw(""),
        );
        let (mut screen, _) = screen(lines, 20, 5);
        for _ in 0..2 {
            screen.mouse(mouse(MouseEventKind::Down(MouseButton::Left), 2, 0));
            screen.mouse(mouse(MouseEventKind::Up(MouseButton::Left), 2, 0));
        }
        assert_eq!(screen.selected().as_deref(), Some(word));
    }

    #[test]
    fn a_double_click_takes_a_word_with_wide_characters() {
        let lines = vec![Line::from("open src/你好.rs, then")];
        let (mut screen, _) = screen(lines, 40, 3);
        for _ in 0..2 {
            screen.mouse(mouse(MouseEventKind::Down(MouseButton::Left), 6, 0));
            screen.mouse(mouse(MouseEventKind::Up(MouseButton::Left), 6, 0));
        }
        assert_eq!(screen.selected().as_deref(), Some("src/你好.rs"));
    }

    #[test]
    fn a_double_click_takes_a_word_cut_over_rows() {
        let url = "https://example.com/a/very/long/path/to/a/page";
        let lines = crate::transcript::wrap(Line::from(url), 20, Span::raw(""), Span::raw(""));
        let (mut screen, _) = screen(lines, 20, 5);
        for _ in 0..2 {
            screen.mouse(mouse(MouseEventKind::Down(MouseButton::Left), 5, 1));
            screen.mouse(mouse(MouseEventKind::Up(MouseButton::Left), 5, 1));
        }
        assert_eq!(screen.selected().as_deref(), Some(url));
    }

    #[test]
    fn no_room_for_rows_draws_none() {
        let url = "https://example.com/a/very/long/path/to/a/page";
        let lines = crate::transcript::wrap(Line::from(url), 20, Span::raw(""), Span::raw(""));
        let (mut screen, _) = screen(lines, 20, 0);
        let area = Rect::new(0, 0, 20, 0);
        for _ in 0..3 {
            screen.mouse(mouse(MouseEventKind::ScrollUp, 2, 0));
            screen.page(true);
            screen.render(area, &mut Buffer::empty(area), |_| None);
        }
        assert!(screen.links.is_empty());
        let click = mouse(MouseEventKind::Down(MouseButton::Left), 2, 0);
        assert_eq!(screen.mouse(click), None);
    }

    #[test]
    fn a_reply_wrapped_again_copies_without_its_bullet() {
        let text = "The stub answers after the 2s timeout, so the test reads an empty cart.";
        let reply = crate::transcript::reply(text, 80, std::path::Path::new("."));
        assert_eq!(reply.len(), 1);
        let (mut screen, _) = screen(reply, 30, 10);
        let last = screen.shown.len() as u16 - 1;
        assert_eq!(
            drag(&mut screen, (0, 0), (29, last)),
            Some(Action::Copy(text.into()))
        );
    }

    #[test]
    fn the_oldest_rows_go_past_what_is_kept() {
        let lines: Vec<Line> = (0..KEEP + KEEP / 4 + 1)
            .map(|i| Line::from(format!("row {i}")))
            .collect();
        let (screen, _) = screen(lines, 20, 5);
        assert_eq!(screen.rows.len(), KEEP);
        assert_eq!(screen.shown.len(), KEEP);
        assert_eq!(
            screen.shown[0].line.to_string(),
            format!("row {}", KEEP / 4 + 1)
        );
    }

    #[test]
    fn the_view_scrolls_and_holds_still_while_rows_arrive() {
        let lines: Vec<Line> = (0..20).map(|i| Line::from(format!("row {i}"))).collect();
        let (mut screen, _) = screen(lines, 20, 5);
        screen.mouse(mouse(MouseEventKind::ScrollUp, 0, 0));
        let area = Rect::new(0, 0, 20, 5);
        let mut buf = Buffer::empty(area);
        screen.render(area, &mut buf, |_| None);
        assert_eq!(row(&buf, 0).trim_end(), "row 12");
        assert_eq!(row(&buf, 4).trim_end(), "row 16");
        screen.push(vec![Line::from("row 20")]);
        let mut buf = Buffer::empty(area);
        screen.render(area, &mut buf, |_| None);
        assert_eq!(row(&buf, 0).trim_end(), "row 12");
        screen.latest();
        let mut buf = Buffer::empty(area);
        screen.render(area, &mut buf, |_| None);
        assert_eq!(row(&buf, 4).trim_end(), "row 20");
    }

    #[test]
    fn find_shows_the_newest_place_and_steps_older() {
        let lines: Vec<Line> = (0..30)
            .map(|i| {
                Line::from(format!(
                    "row {i}{}",
                    if i % 10 == 3 { " Needle" } else { "" }
                ))
            })
            .collect();
        let (mut screen, _) = screen(lines, 20, 5);
        let area = Rect::new(0, 0, 20, 5);
        screen.find("needle");
        assert_eq!(screen.found(), Some((3, 3)));
        let mut buf = Buffer::empty(area);
        screen.render(area, &mut buf, |_| None);
        assert!((0..5).any(|y| row(&buf, y).trim_end() == "row 23 Needle"));
        screen.find_next(true);
        assert_eq!(screen.found(), Some((2, 3)));
        let mut buf = Buffer::empty(area);
        screen.render(area, &mut buf, |_| None);
        let y = (0..5)
            .find(|y| row(&buf, *y).trim_end() == "row 13 Needle")
            .unwrap();
        assert_eq!(buf[(7, y)].bg, Color::Yellow);
        // Typing on looks from where the view is.
        screen.find("needle");
        assert_eq!(screen.found(), Some((2, 3)));
        // A capital asks for that case.
        screen.find("NEEDLE");
        assert_eq!(screen.found(), Some((0, 0)));
        // Round from the oldest to the newest.
        screen.find("Needle");
        assert_eq!(screen.found(), Some((2, 3)));
        screen.find_next(true);
        screen.find_next(true);
        assert_eq!(screen.found(), Some((3, 3)));
    }

    #[test]
    fn find_goes_on_in_the_rows_drawn_again() {
        let lines = |from| (from..20).map(|i| Line::from(format!("row {i}"))).collect();
        let (mut screen, _) = screen(lines(10), 20, 5);
        screen.find("row 1");
        assert_eq!(screen.found(), Some((10, 10)));
        screen.clear();
        screen.push(lines(0));
        let area = Rect::new(0, 0, 20, 5);
        screen.render(area, &mut Buffer::empty(area), |_| None);
        assert_eq!(screen.found(), Some((11, 11)));
    }

    #[test]
    fn find_places_wide_characters_and_marks_in_their_cells() {
        let line = Line::from("  cafe\u{301} 日本語 TEST");
        let (mut screen, _) = screen(vec![line], 30, 3);
        for (query, text) in [
            ("日本", "日本"),
            ("e\u{301} 日", "e\u{301} 日"),
            ("test", "TEST"),
        ] {
            screen.find(query);
            assert_eq!(screen.found(), Some((1, 1)), "{query}");
            let (start, end) = screen.finding.as_ref().unwrap().hits[0];
            assert_eq!(screen.text(start, end), text);
        }
    }

    #[test]
    fn find_reads_a_line_whole_over_its_rows() {
        let text = "The stub answers after the 2s timeout, so the test reads an empty cart.";
        let reply = crate::transcript::reply(text, 80, std::path::Path::new("."));
        let (mut screen, _) = screen(reply, 30, 10);
        screen.find("test reads an");
        assert_eq!(screen.found(), Some((1, 1)));
        let (start, end) = screen.finding.as_ref().unwrap().hits[0];
        assert!(end.0 > start.0);
        assert_eq!(screen.text(start, end), "test reads an");
    }

    fn rows(range: std::ops::Range<usize>) -> Vec<Line<'static>> {
        range.map(|i| Line::from(format!("row {i}"))).collect()
    }

    /// A screen `lines` were written to.
    fn written(lines: Vec<Line<'static>>) -> Screen {
        let mut screen = Screen::default();
        screen.push(lines);
        screen
    }

    #[test]
    fn older_rows_drawn_above_leave_the_view_where_it_was() {
        let (mut screen, _) = screen(rows(100..160), 20, 5);
        assert!(!screen.near_top());
        screen.scroll(45);
        assert!(screen.near_top());
        let area = Rect::new(0, 0, 20, 5);
        let mut buf = Buffer::empty(area);
        screen.render(area, &mut buf, |_| None);
        let shown = row(&buf, 0);
        screen.prepend(0, written(rows(0..100)));
        screen.render(area, &mut buf, |_| None);
        assert_eq!(row(&buf, 0), shown);
        assert!(!screen.near_top());
    }

    #[test]
    fn older_rows_drawn_above_stay_at_another_width() {
        let (mut screen, _) = screen(rows(13_000..13_100), 20, 5);
        screen.prepend(0, written(rows(0..13_000)));
        let area = Rect::new(0, 0, 30, 5);
        screen.render(area, &mut Buffer::empty(area), |_| None);
        assert_eq!(screen.height(30), 13_100);
        screen.scroll(isize::MAX);
        assert!(screen.near_top());
    }

    #[test]
    fn the_place_found_stays_while_older_rows_are_drawn_above() {
        let (mut screen, _) = screen(rows(100..160), 20, 5);
        screen.find("row 15");
        assert_eq!(screen.found(), Some((10, 10)));
        screen.prepend(0, written(rows(0..100)));
        assert_eq!(screen.found(), Some((11, 11)));
        let area = Rect::new(0, 0, 20, 5);
        let mut buf = Buffer::empty(area);
        screen.render(area, &mut buf, |_| None);
        assert_eq!(screen.found(), Some((11, 11)));
        assert!((0..5).any(|y| row(&buf, y).trim_end() == "row 159"));
    }

    #[test]
    fn the_oldest_rows_go_only_from_under_the_view() {
        let area = Rect::new(0, 0, 20, 5);
        let mut buf = Buffer::empty(area);
        let (mut screen, _) = screen(rows(0..12_000), 20, 5);
        screen.scroll(isize::MAX);
        screen.push(rows(12_000..13_000));
        screen.render(area, &mut buf, |_| None);
        assert_eq!(row(&buf, 0).trim_end(), "row 0");
        assert!(screen.near_top());
        screen.latest();
        screen.push(rows(13_000..13_001));
        screen.render(area, &mut buf, |_| None);
        assert_eq!(screen.height(20), KEEP);
        assert_eq!(row(&buf, 4).trim_end(), "row 13000");
        screen.scroll(isize::MAX);
        assert!(!screen.near_top());
    }

    fn lines(range: std::ops::Range<usize>, word: &str) -> Vec<Line<'static>> {
        range.map(|i| Line::from(format!("{word} {i}"))).collect()
    }

    /// The rows `screen` draws in a fresh `area`.
    fn drawn(screen: &mut Screen, area: Rect) -> Buffer {
        let mut buf = Buffer::empty(area);
        screen.render(area, &mut buf, |_| None);
        buf
    }

    #[test]
    fn pages_far_from_the_view_are_let_go_and_drawn_again_near_it() {
        let area = Rect::new(0, 0, 20, 5);
        let (mut screen, _) = screen(rows(20_000..20_050), 20, 5);
        screen.prepend(1, written(rows(10_000..17_000)));
        screen.prepend(2, written(rows(0..7_000)));
        let buf = drawn(&mut screen, area);
        // The farthest page went for blank rows as tall: nothing moved.
        assert_eq!(row(&buf, 4).trim_end(), "row 20049");
        assert_eq!(screen.height(20), 14_050);
        assert!(!screen.pages[0].drawn && screen.pages[1].drawn);
        assert_eq!(screen.wanted(), None);
        // Up at the first row it is wanted, and drawn again in its place.
        screen.scroll(isize::MAX);
        assert_eq!(screen.wanted(), Some(2));
        screen.fill(2, written(rows(0..7_000)));
        let buf = drawn(&mut screen, area);
        assert_eq!(row(&buf, 0).trim_end(), "row 0");
        // Now the other page, farthest, went.
        assert!(screen.pages[0].drawn && !screen.pages[1].drawn);
        assert_eq!(screen.height(20), 14_050);
        assert_eq!(screen.wanted(), None);
    }

    #[test]
    fn a_page_drawn_again_taller_keeps_the_view_on_its_rows() {
        let area = Rect::new(0, 0, 20, 5);
        let (mut screen, _) = screen(rows(30_000..30_050), 20, 5);
        screen.prepend(1, written(rows(10_000..20_000)));
        screen.scroll(isize::MAX);
        screen.scroll(-100);
        drawn(&mut screen, area);
        screen.prepend(2, written(rows(0..10_000)));
        let buf = drawn(&mut screen, area);
        assert_eq!(row(&buf, 0).trim_end(), "row 10100");
        assert!(!screen.pages[0].drawn);
        // At the first row, the page comes back two rows a line, and the
        // view shows its first rows; the other page, far now, goes.
        screen.scroll(isize::MAX);
        assert_eq!(screen.wanted(), Some(2));
        screen.fill(2, written(lines(0..10_000, "a line long enough to wrap")));
        let buf = drawn(&mut screen, area);
        assert!(row(&buf, 0).starts_with("a line long"));
        assert!(row(&buf, 1).contains(" 0"));
        assert!(!screen.pages[1].drawn);
        // Down into the page let go, it comes back taller too, the view as
        // far into it as it was.
        screen.scroll(-20_100);
        drawn(&mut screen, area);
        assert_eq!(screen.wanted(), Some(1));
        screen.fill(1, written(lines(10_000..20_000, "another line that wraps")));
        let buf = drawn(&mut screen, area);
        assert!(row(&buf, 0).starts_with("another line"));
        assert!(row(&buf, 1).contains("10100"));
    }

    #[test]
    fn the_place_found_stays_while_a_page_is_let_go() {
        let area = Rect::new(0, 0, 20, 5);
        let (mut screen, _) = screen(rows(20_000..20_050), 20, 5);
        screen.prepend(1, written(lines(0..7_000, "page")));
        screen.find("row 20049");
        assert_eq!(screen.found(), Some((1, 1)));
        // Found above too, but let go at once, far from the view.
        screen.prepend(2, written(lines(0..7_000, "row 20049 again")));
        assert!(!screen.pages[0].drawn);
        assert_eq!(screen.found(), Some((1, 1)));
        let buf = drawn(&mut screen, area);
        assert_eq!(screen.found(), Some((1, 1)));
        assert!((0..5).any(|y| row(&buf, y).trim_end() == "row 20049"));
    }

    #[test]
    fn drawn_anew_the_pages_stand_as_blank_rows_until_the_view_nears() {
        let area = Rect::new(0, 0, 20, 5);
        let (mut screen, _) = screen(rows(20_000..20_050), 20, 5);
        screen.prepend(
            1,
            written(lines(10_000..13_000, "a line long enough to wrap")),
        );
        screen.prepend(2, written(rows(0..3_000)));
        drawn(&mut screen, area);
        let pages = screen.pages();
        assert_eq!(pages, vec![(2, 3_000), (1, 6_000)]);
        // Only the newest rows are drawn again; the pages stand as blank
        // rows as tall as they were, the view on the newest.
        screen.clear();
        screen.push(lines(20_000..20_050, "new"));
        screen.blank(pages);
        let buf = drawn(&mut screen, area);
        assert_eq!(row(&buf, 4).trim_end(), "new 20049");
        assert_eq!(screen.height(20), 9_050);
        assert_eq!(screen.wanted(), None);
        // Up near a page, it is wanted and drawn again in its place.
        screen.scroll(6_000);
        drawn(&mut screen, area);
        assert_eq!(screen.wanted(), Some(1));
        screen.fill(1, written(rows(10_000..13_000)));
        let buf = drawn(&mut screen, area);
        assert!(row(&buf, 0).starts_with("row 1"));
        screen.scroll(isize::MAX);
        drawn(&mut screen, area);
        assert_eq!(screen.wanted(), Some(2));
        screen.fill(2, written(rows(0..3_000)));
        let buf = drawn(&mut screen, area);
        assert_eq!(row(&buf, 0).trim_end(), "row 0");
    }

    #[test]
    fn a_picked_line_is_kept_in_view_with_rows_under_it() {
        let lines: Vec<_> = (0..20).map(|i| Line::from(format!("row {i}"))).collect();
        let (mut screen, _) = screen(lines, 10, 5);
        let area = Rect::new(0, 0, 10, 5);
        let mut buf = Buffer::empty(area);
        let mut keep = |screen: &mut Screen, line, below| {
            screen.keep_in_view(line, below);
            screen.render(area, &mut buf, |_| None);
            (
                row(&buf, 0).trim_end().to_owned(),
                row(&buf, 4).trim_end().to_owned(),
            )
        };
        // Down from the top: no scroll while it shows, then as little as it takes.
        screen.hold(usize::MAX);
        assert_eq!(keep(&mut screen, 2, 2).0, "row 0");
        assert_eq!(keep(&mut screen, 8, 2).1, "row 10");
        // Already in view: it stays put.
        assert_eq!(keep(&mut screen, 7, 2).1, "row 10");
        // Up: the line at the top, and near the top the whole way up.
        assert_eq!(keep(&mut screen, 4, 2).0, "row 4");
        assert_eq!(keep(&mut screen, 1, 2).0, "row 0");
    }

    #[test]
    fn a_card_put_in_anew_keeps_the_rows_scrolled_to() {
        // A long line wraps over rows above where the view is.
        let mut lines = vec![Line::from("one two three four five six seven")];
        lines.extend((0..20).map(|i| Line::from(format!("row {i}"))));
        let (mut screen, _) = screen(lines.clone(), 10, 5);
        screen.show_row(0, 0);
        let area = Rect::new(0, 0, 10, 5);
        let mut buf = Buffer::empty(area);
        screen.render(area, &mut buf, |_| None);
        screen.scroll(-6);
        screen.render(area, &mut buf, |_| None);
        let before = row(&buf, 0);
        for _ in 0..3 {
            screen.replace(lines.clone());
            screen.render(area, &mut buf, |_| None);
            assert_eq!(row(&buf, 0), before);
        }
    }

    #[test]
    fn a_pictures_rows_are_cut_at_the_edge_not_wrapped() {
        let cells: String = std::iter::repeat_n(crate::image::PLACEHOLDER, 30).collect();
        let picture = Line::from(vec![Span::raw("  "), Span::raw(cells)]);
        let (mut kitty, _) = screen(vec![picture.clone(), picture], 20, 5);
        assert_eq!(kitty.height(20), 2);
        let blocks = Line::from("  ▀▄▀▄ ▀▄▀▄ ▀▄▀▄ ▀▄▀▄ ▀▄▀▄ ▀▄▀▄");
        let (mut banner, _) = screen(vec![blocks], 20, 5);
        assert_eq!(banner.height(20), 1);
    }

    #[test]
    fn a_narrower_screen_wraps_the_rows_again() {
        let lines = vec![Line::from("one two three four five six")];
        let (mut screen, _) = screen(lines, 40, 5);
        let area = Rect::new(0, 0, 12, 5);
        let mut buf = Buffer::empty(area);
        screen.render(area, &mut buf, |_| None);
        assert_eq!(row(&buf, 0).trim_end(), "one two");
        let last = screen.shown.len() as u16 - 1;
        assert_eq!(
            drag(&mut screen, (0, 0), (11, last)),
            Some(Action::Copy("one two three four five six".into()))
        );
    }

    #[test]
    fn a_row_put_under_a_tagged_one_moves_what_is_below_it() {
        let mut screen = Screen::default();
        let lines: Vec<Line> = (0..20).map(|i| Line::from(format!("row {i}"))).collect();
        screen.push_marked(lines, vec![(5, Mark::Tag(Tag::End(5)))]);
        let area = Rect::new(0, 0, 20, 5);
        let draw = |screen: &mut Screen| {
            let mut buf = Buffer::empty(area);
            screen.render(area, &mut buf, |_| None);
            (0..5)
                .map(|y| row(&buf, y).trim_end().to_owned())
                .collect::<Vec<_>>()
        };
        draw(&mut screen);
        // Scrolled to the rows around it: they stay where they were, and
        // the row goes in under the tagged one.
        screen.hold(13);
        assert_eq!(draw(&mut screen)[..2], ["row 2", "row 3"]);
        let pill = || Some(Line::from("  👍"));
        assert!(screen.put_under(Tag::End(5), Tag::Reactions(5), pill()));
        assert_eq!(
            draw(&mut screen),
            ["row 2", "row 3", "row 4", "row 5", "  👍"]
        );
        // Put again, it is changed in place; none takes it off.
        assert!(screen.put_under(Tag::End(5), Tag::Reactions(5), Some(Line::from("  🎉"))));
        assert_eq!(draw(&mut screen)[4], "  🎉");
        assert!(screen.put_under(Tag::End(5), Tag::Reactions(5), None));
        assert_eq!(draw(&mut screen)[4], "row 6");
        assert_eq!(screen.texts().len(), 20);
        // At the newest rows, the view stays there.
        screen.latest();
        assert!(screen.put_under(Tag::End(5), Tag::Reactions(5), pill()));
        assert_eq!(draw(&mut screen)[4], "row 19");
        assert!(!screen.put_under(Tag::End(99), Tag::Reactions(99), pill()));
    }

    #[test]
    fn a_page_drawn_above_grows_and_shrinks_with_its_tagged_rows() {
        let (mut screen, _) = screen(rows(100..160), 20, 5);
        let mut older = Screen::default();
        older.push(rows(0..3));
        older.push_marked(
            vec![Line::from("• Started a, b ▸")],
            vec![(0, Mark::Tag(Tag::Group(1)))],
        );
        older.push(rows(4..6));
        screen.prepend(0, older);
        assert_eq!(screen.pages[0].rows, 6);
        let open = vec![Line::from("▾"), Line::from("a"), Line::from("b")];
        assert!(screen.replace_tagged(Tag::Group(1), open, Vec::new()));
        assert_eq!(screen.pages[0].rows, 8);
        assert!(screen.put_under(Tag::Group(1), Tag::Reactions(1), Some("👀".into())));
        assert_eq!(screen.pages[0].rows, 9);
        assert!(screen.put_under(Tag::Group(1), Tag::Reactions(1), None));
        assert!(screen.replace_tagged(Tag::Group(1), vec!["▸".into()], Vec::new()));
        assert_eq!(screen.pages[0].rows, 6);
        assert_eq!(screen.texts()[6], "row 100");
    }

    #[test]
    fn tagged_rows_are_replaced_whole_and_a_button_is_no_underlined_link() {
        let mut screen = Screen::default();
        screen.push(vec![Line::from("before")]);
        screen.push_marked(
            vec![Line::from("• Started a, b ▸")],
            vec![
                (0, Mark::Tag(Tag::Group(1))),
                (0, Mark::Link(format!("{BUTTON}fold/1"))),
            ],
        );
        screen.push(vec![Line::from("after")]);
        let area = Rect::new(0, 0, 30, 6);
        let mut buf = Buffer::empty(area);
        screen.render(area, &mut buf, |_| None);
        assert!(!buf[(3, 1)].modifier.contains(Modifier::UNDERLINED));
        assert!(screen.link_cells(&buf).is_empty(), "never an OSC 8 link");
        screen.mouse(mouse(MouseEventKind::Down(MouseButton::Left), 3, 1));
        assert_eq!(
            screen.mouse(mouse(MouseEventKind::Up(MouseButton::Left), 3, 1)),
            Some(Action::Open("sonny:fold/1".into()))
        );
        let open = vec![
            Line::from("• Started a, b ▾"),
            Line::from("• Started a"),
            Line::from("• Started b"),
        ];
        assert!(screen.replace_tagged(Tag::Group(1), open, Vec::new()));
        assert_eq!(
            screen.texts(),
            [
                "before",
                "• Started a, b ▾",
                "• Started a",
                "• Started b",
                "after"
            ]
        );
        assert!(screen.replace_tagged(
            Tag::Group(1),
            vec![Line::from("• Started a, b ▸")],
            Vec::new()
        ));
        assert_eq!(screen.texts(), ["before", "• Started a, b ▸", "after"]);
        assert!(!screen.replace_tagged(Tag::Group(2), Vec::new(), Vec::new()));
    }

    #[test]
    fn older_rows_drawn_above_keep_their_marks() {
        let mut screen = Screen::default();
        screen.push(vec![Line::from("newest")]);
        let mut older = Screen::default();
        older.push_marked(
            vec![Line::from("older"), Line::from("• Started a ▸")],
            vec![
                (0, Mark::Tag(Tag::End(1))),
                (1, Mark::Link(format!("{BUTTON}fold/1"))),
            ],
        );
        // Changed while it was written on its own, as an older page is.
        older.replace_tagged(
            Tag::End(1),
            vec![Line::from("older, again")],
            vec![(0, Mark::Tag(Tag::End(1)))],
        );
        screen.prepend(0, older);
        assert_eq!(screen.texts(), ["older, again", "• Started a ▸", "newest"]);
        assert!(screen.has(Tag::End(1)));
        assert_eq!(screen.links()[1].as_deref(), Some("sonny:fold/1"));
        assert!(screen.put_under(Tag::End(1), Tag::Reactions(1), Some(Line::from("  👍"))));
        assert_eq!(screen.texts()[1], "  👍");
    }
}
