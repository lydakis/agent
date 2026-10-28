//! The full screen (`/fullscreen`): Sonny keeps the conversation's rows
//! itself and draws them above the rows that move, on the alternate
//! screen, instead of handing them to the terminal's scrollback. So the
//! wheel scrolls them, and the mouse selects and copies them with no key
//! held, as Codex's full-screen transcript does (openai/codex,
//! codex-rs/tui/src/transcript_view): a drag selects and copies when let
//! go, a double click takes a word, a triple click a line, and a click
//! that does not drag opens the link under it. What is copied is the text
//! as written: a line wrapped over rows comes back as one line, without
//! the rows' leads or the indent every copied line shares.
use std::time::{Duration, Instant};

use ratatui::{
    buffer::Buffer,
    layout::Rect,
    style::{Color, Modifier, Style, Stylize},
    text::{Line, Span},
};
use unicode_segmentation::UnicodeSegmentation;
use unicode_width::UnicodeWidthStr;

/// How far one notch of the wheel scrolls.
const WHEEL: usize = 3;
/// How soon a second click counts as a double click.
const DOUBLE: Duration = Duration::from_millis(400);
/// How many of the conversation's rows are kept to scroll back to, as a
/// terminal keeps its scrollback; the whole conversation stays in the
/// bot's history.
const KEEP: usize = 10_000;
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

/// A row on the screen: one of the conversation's rows, or the part of
/// one that did not fit the width.
struct Shown {
    line: Line<'static>,
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
    line: Line<'static>,
    /// The lead the rows it goes on to start with, as it was written.
    rest: Option<Line<'static>>,
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
    /// how many: the bottom one is a note while scrolled back.
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
}

impl Screen {
    /// The conversation drawn again from the start, as a repaint does.
    pub fn clear(&mut self) {
        *self = Self::default();
    }

    pub fn push(&mut self, lines: Vec<Line<'static>>) {
        for line in lines {
            let Some(lead) = crate::transcript::lead(&line).filter(|_| !self.rows.is_empty())
            else {
                self.rows.push(Row { line, rest: None });
                continue;
            };
            // A row the line above was wrapped onto joins it again.
            if self.wrapped == self.rows.len() {
                self.unlay();
            }
            let (rest, text) = split(line, lead);
            let row = self.rows.last_mut().expect("a row to join");
            // Spans alike join too, so a word cut over rows is one again.
            for span in text {
                match row.line.spans.last_mut() {
                    Some(last) if last.style == span.style => {
                        last.content.to_mut().push_str(&span.content)
                    }
                    _ => row.line.spans.push(span),
                }
            }
            row.rest.get_or_insert(rest);
        }
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

    /// Up (`rows` > 0) or down the conversation.
    pub fn scroll(&mut self, rows: isize) {
        let most = self.most(self.area.height as usize);
        self.back = self.back.saturating_add_signed(rows).min(most);
    }

    /// How far back the view can go with `height` rows: to the first row,
    /// with the note under the rows.
    fn most(&self, height: usize) -> usize {
        (self.shown.len() + 1)
            .saturating_sub(height)
            .min(self.shown.len())
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
    /// the view is scrolled up.
    fn layout(&mut self, width: usize) {
        if width != self.width {
            self.width = width;
            self.shown.clear();
            self.wrapped = 0;
            self.relaid = 0;
            self.selection = None;
            self.linked = None;
        }
        let before = self.shown.len();
        for row in &self.rows[self.wrapped..] {
            let line = row.line.clone();
            let rows = match line.width() <= width {
                true => vec![line],
                false => {
                    let rest = row.rest.clone().unwrap_or(Line::from("    "));
                    crate::transcript::wrap_lines(line, width, Line::default(), rest)
                }
            };
            for (at, line) in rows.into_iter().enumerate() {
                let joins = crate::transcript::lead(&line);
                let opens = crate::transcript::opens(&line).unwrap_or(0);
                let first = at == 0;
                self.shown.push(Shown {
                    line,
                    joins,
                    opens,
                    first,
                });
            }
        }
        self.wrapped = self.rows.len();
        let relaid = std::mem::take(&mut self.relaid);
        if self.back > 0 && before > 0 {
            self.back = (self.back + self.shown.len() - before).saturating_sub(relaid);
        }
        // The oldest rows go, a batch at a time.
        if self.rows.len() > KEEP + KEEP / 4 {
            let drop = self.rows.len() - KEEP;
            self.rows.drain(..drop);
            self.wrapped -= drop;
            let mut firsts = 0;
            let gone = (self.shown.iter())
                .position(|shown| {
                    firsts += usize::from(shown.first);
                    firsts > drop
                })
                .unwrap_or(self.shown.len());
            self.shown.drain(..gone);
            self.selection = None;
            self.linked = None;
        }
    }

    /// Draw the rows that fit `area` of `buf`: the newest, or those the
    /// view is scrolled to, the selection reversed and links underlined.
    /// `file` is where a word that may be a path opens, if it is one.
    pub fn render(&mut self, area: Rect, buf: &mut Buffer, file: impl Fn(&str) -> Option<String>) {
        self.layout(area.width as usize);
        let height = area.height as usize;
        self.back = self.back.min(self.most(height));
        // While scrolled back, the bottom row is the note, over no row.
        let rows = height - usize::from(self.back > 0 && height > 1);
        let bottom = self.shown.len() - self.back;
        let top = bottom.saturating_sub(rows);
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
        for (y, from, to, _) in &self.links {
            for x in *from..*to {
                let cell = &mut buf[(x, *y)];
                cell.set_style(cell.style().add_modifier(Modifier::UNDERLINED));
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
        if self.back > 0 && height > 0 {
            let y = area.y + area.height - 1;
            let note = Line::from(vec![
                Span::raw(" ↓ "),
                Span::raw(format!("{} newer rows", self.back)),
                Span::raw(" · click, PgDn or Enter for the latest ").dim(),
            ])
            .style(Style::new().reversed());
            let row = Rect {
                y,
                height: 1,
                ..area
            };
            for x in row.left()..row.right() {
                buf[(x, y)].reset();
            }
            buf.set_line(area.x, y, &note, area.width);
            buf.set_style(row, Style::new().reversed());
        }
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
                // The "newer rows" note is a button back to the latest.
                if self.back > 0 && event.row + 1 == self.area.bottom() {
                    self.back = 0;
                    self.selection = None;
                    return None;
                }
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
                start.1 = self.word(start, true);
                end.1 = self.word(end, false);
                // A word cut at a row's end goes on in the row below.
                while start.0 > 0 && self.cut(start.0) && self.leads(start) {
                    let above = start.0 - 1;
                    start = (above, self.word((above, self.width), true));
                }
                while self.cut(end.0 + 1) && self.ends(end.0) {
                    let below = end.0 + 1;
                    let lead = self.shown[below].joins.unwrap_or(0);
                    end = (below, self.word((below, lead), false));
                }
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

    /// Nothing but the row's lead is before `column`.
    fn leads(&self, (row, column): Spot) -> bool {
        let cells = cells(&self.shown[row].line);
        cells[..column.min(cells.len())]
            .iter()
            .all(|g| g.trim().is_empty())
    }

    /// The row's last cell is part of a word.
    fn ends(&self, row: usize) -> bool {
        cells(&self.shown[row].line)
            .last()
            .is_some_and(|g| !g.trim().is_empty())
    }

    /// Where the word at `spot` starts, or ends: a run without spaces, so
    /// a path or an address is one word, less the punctuation around it.
    fn word(&self, (row, column): Spot, starts: bool) -> usize {
        let cells = cells(&self.shown[row].line);
        let word = |at: usize| cells.get(at).is_some_and(|g| !g.trim().is_empty());
        let around = |at: usize, set: &str| {
            cells
                .get(at)
                .is_some_and(|g| !g.is_empty() && set.contains(g.as_str()))
        };
        let mut at = column.min(cells.len());
        if starts {
            while at > 0 && word(at - 1) {
                at -= 1;
            }
            while word(at) && around(at, "(\"'[`<") && at < column {
                at += 1;
            }
        } else {
            while word(at) {
                at += 1;
            }
            let first = self.word((row, column), true);
            // A closing bracket the word opens stays: `len(values)`.
            let opened = |at: usize, open: &str| cells[first..at].iter().any(|g| g == open);
            while at > column + 1
                && around(at - 1, ".,;:!?)\"']`>")
                && !(around(at - 1, ")") && opened(at - 1, "("))
                && !(around(at - 1, "]") && opened(at - 1, "["))
            {
                at -= 1;
            }
        }
        at
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
        let reply = crate::transcript::reply(text, 30);
        assert!(reply.len() > 2);
        let (screen, buf) = screen(reply, 80, 5);
        assert_eq!(screen.shown.len(), 1);
        assert_eq!(row(&buf, 0).trim_end(), format!("• {text}"));
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
    fn the_note_over_the_bottom_row_is_not_selected() {
        let lines: Vec<Line> = (0..20).map(|i| Line::from(format!("row {i}"))).collect();
        let (mut screen, _) = screen(lines, 20, 5);
        screen.mouse(mouse(MouseEventKind::ScrollUp, 0, 0));
        let area = Rect::new(0, 0, 20, 5);
        screen.render(area, &mut Buffer::empty(area), |_| None);
        assert_eq!(
            drag(&mut screen, (0, 0), (19, 4)),
            Some(Action::Copy("row 13\nrow 14\nrow 15\nrow 16".into()))
        );
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
        let reply = crate::transcript::reply(text, 80);
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
        assert_eq!(row(&buf, 0).trim_end(), "row 13");
        assert!(row(&buf, 4).contains("3 newer rows"));
        screen.push(vec![Line::from("row 20")]);
        let mut buf = Buffer::empty(area);
        screen.render(area, &mut buf, |_| None);
        assert_eq!(row(&buf, 0).trim_end(), "row 13");
        // The note is a way back.
        screen.mouse(mouse(MouseEventKind::Down(MouseButton::Left), 3, 4));
        let mut buf = Buffer::empty(area);
        screen.render(area, &mut buf, |_| None);
        assert_eq!(row(&buf, 4).trim_end(), "row 20");
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
}
