//! Markdown read as CommonMark is, with GitHub's tables, task lists,
//! footnotes and strikethrough and `$` math, by pulldown-cmark, the parser
//! Codex's TUI reads it with, and drawn as Sonny's rows: what every surface
//! shows a model's words and a Markdown file with ([`crate::transcript`]
//! names the surfaces).
//!
//! A reply streams in a line at a time. Its blocks that have ended are
//! drawn once and handed out; the block still open is read again as each
//! line comes, as Words and Codex read one, and its rows are handed out as
//! soon as a new line leaves them as they were. A fence that is still open
//! is read a line at a time, so a long one costs its length once.
use pulldown_cmark::{
    BrokenLink, CodeBlockKind, Event, HeadingLevel, LinkType, Options, Parser, Tag, TagEnd,
};
use ratatui::{
    style::{Color, Modifier, Style, Stylize},
    text::{Line, Span},
};
use std::ops::Range;
use unicode_width::UnicodeWidthStr;

use crate::transcript::{
    Picture, bars, fence_picture, html_tag, html_words, picture_file, picture_words, pictures,
    relead, width_hint, wrap, wrap_lines,
};

/// The most bars a quote draws, as deep as it is nested: a deeper one
/// draws as deep as this.
const QUOTE_DEPTH: usize = 8;

/// What GitHub's Markdown reads besides CommonMark's, and `$` math.
fn options() -> Options {
    Options::ENABLE_TABLES
        | Options::ENABLE_FOOTNOTES
        | Options::ENABLE_STRIKETHROUGH
        | Options::ENABLE_TASKLISTS
        | Options::ENABLE_MATH
}

/// Where the words are read: the folder a picture's relative path is read
/// from (the file's, or the agent's workspace; else where Sonny runs), and
/// whether they are a file read as a document, not a reply: an ```html or
/// ```svg fence in one is code to read, as GitHub shows it, not drawn.
#[derive(Clone, Default)]
pub(crate) struct Where {
    pub folder: Option<std::path::PathBuf>,
    pub document: bool,
}

/// Footnotes numbered in the order they are first named.
#[derive(Clone, Default)]
struct Notes(Vec<String>);

impl Notes {
    fn number(&mut self, name: &str) -> usize {
        match self.0.iter().position(|n| n == name) {
            Some(at) => at + 1,
            None => {
                self.0.push(name.to_owned());
                self.0.len()
            }
        }
    }
}

/// A top-level block drawn: where it is in the text, whether a blank line
/// was written before it, its rows, and what streaming needs to know.
struct Block {
    start: usize,
    end: usize,
    /// Where in the text each row's line starts.
    offsets: Vec<usize>,
    gap: bool,
    rows: Vec<Line<'static>>,
    /// A table, whose columns a later row can widen: none of its rows is
    /// handed out until it ends.
    holds: bool,
    /// A footnote's definition, which text after it can point at.
    note: bool,
    /// A list's items after its first, where each can be read on from.
    items: Vec<Item>,
    /// A paragraph, and the marks in it that run across lines: it is
    /// read on from no line inside one.
    paragraph: bool,
    across: Vec<Range<usize>>,
    /// It points at a link or footnote not defined yet.
    broken: bool,
    /// A fence at the top level, open where the text ends: its marks and
    /// language.
    fence: Option<Fence>,
}

/// An item of a list at the top level, after its first: the items before
/// it are done, so the list can be read on from here, as the list it starts.
#[derive(Clone, Copy)]
struct Item {
    /// Past the words before it, where the text read on from starts.
    cut: usize,
    /// The rows before it, and before its blank row.
    rows: usize,
    gap_rows: usize,
    /// Its number, in a numbered list.
    number: Option<u64>,
}

/// An open fence's closing marks and language.
#[derive(Clone)]
struct Fence {
    mark: char,
    length: usize,
    lang: String,
}

impl Fence {
    /// The fence `line` opens, if it opens one at the top level.
    fn opened_by(line: &str) -> Option<Self> {
        let line = line.split('\n').next().unwrap_or(line);
        let trimmed = line.trim_start_matches(' ');
        if line.len() - trimmed.len() > 3 {
            return None;
        }
        let mark = trimmed.chars().next().filter(|c| matches!(c, '`' | '~'))?;
        let length = trimmed.chars().take_while(|c| *c == mark).count();
        let info = &trimmed[length..];
        (length >= 3 && !(mark == '`' && info.contains('`'))).then(|| Fence {
            mark,
            length,
            lang: language(info),
        })
    }

    /// Whether `line` closes it.
    fn closed_by(&self, line: &str) -> bool {
        let trimmed = line.trim_start_matches(' ');
        let marks = trimmed.chars().take_while(|c| *c == self.mark).count();
        line.len() - trimmed.len() <= 3
            && marks >= self.length
            && trimmed[marks * self.mark.len_utf8()..].trim().is_empty()
    }
}

/// A fence's language, as it is drawn: TeX in any of its names is math.
fn language(info: &str) -> String {
    let lang = info.split_whitespace().next().unwrap_or("");
    match lang {
        "latex" | "tex" | "katex" => "math".to_owned(),
        lang => lang.to_owned(),
    }
}

/// Whether a fence in `lang` is drawn as a picture where it closes, rather
/// than shown as its code.
fn drawn(lang: &str, document: bool) -> bool {
    match lang {
        "mermaid" => crate::term::graphics(),
        "svg" => crate::term::graphics() && !document,
        "html" => !document,
        "diff" | "patch" | "math" => true,
        _ => false,
    }
}

/// One container the block being drawn is inside.
enum Holder {
    Quote,
    List(Option<u64>),
    /// A list item or a footnote: the mark its first row starts with until
    /// that row is drawn, and the columns its other rows hang under it.
    Item {
        mark: Option<Vec<Span<'static>>>,
        hang: usize,
    },
}

/// What is being read inside a block that holds words.
enum Leaf {
    Words,
    Code {
        fence: Option<Fence>,
        range: Range<usize>,
        text: String,
    },
    Html(String),
    Table(Range<usize>),
}

/// The text's blocks drawn at `width`: each top-level block's rows, every
/// row starting with the two columns a reply's lead takes. `end` says the
/// text is all there is: a fence it leaves open shows as code.
fn draw(source: &str, width: usize, at: &Where, notes: &mut Notes, end: bool) -> Vec<Block> {
    draw_after(source, 0, width, at, notes, end).0
}

/// `source` drawn from `skip` on, what is before it read only for its
/// definitions; and where the link definitions from `skip` on are.
fn draw_after(
    source: &str,
    skip: usize,
    width: usize,
    at: &Where,
    notes: &mut Notes,
    end: bool,
) -> (Vec<Block>, Vec<Range<usize>>) {
    let mut draw = Draw {
        source,
        width,
        at,
        notes,
        end,
        rows: Vec::new(),
        offsets: Vec::new(),
        here: 0,
        starts: Vec::new(),
        holders: Vec::new(),
        quotes: 0,
        firsts: vec![true],
        last_end: 0,
        leaf: None,
        lines: Vec::new(),
        spans: Vec::new(),
        styles: Vec::new(),
        links: Vec::new(),
        html: Vec::new(),
        image: None,
        blocks: Vec::new(),
        top: None,
        top_holds: false,
        top_note: false,
        top_paragraph: false,
        across: Vec::new(),
        top_notes: 0,
        skip,
        loose: None,
        items: Vec::new(),
    };
    // Links to a label not defined yet, not as written alone: `[a][b]`.
    let broken = std::cell::RefCell::new(Vec::new());
    let note = |link: BrokenLink<'_>| {
        if link.link_type != LinkType::Shortcut {
            broken.borrow_mut().push(link.span.start);
        }
        None
    };
    let mut events =
        Parser::new_with_broken_link_callback(source, options(), Some(note)).into_offset_iter();
    for (event, range) in events.by_ref() {
        // The definitions read before are read again only for what they
        // define: they are not drawn.
        if range.end <= skip {
            continue;
        }
        draw.event(event, range);
    }
    let links = (events.reference_definitions().iter())
        .map(|(_, link)| link.span.clone())
        .filter(|span| span.start >= skip)
        .collect();
    let mut blocks = draw.blocks;
    for block in &mut blocks {
        block.broken |= (broken.borrow().iter()).any(|at| (block.start..block.end).contains(at));
    }
    (blocks, links)
}

/// Where a block that ended at `end` ends as lines do: past its line's end.
fn line_end(source: &str, end: usize) -> usize {
    if end == 0 || source.as_bytes().get(end - 1) == Some(&b'\n') {
        return end;
    }
    source[end..]
        .find('\n')
        .map_or(source.len(), |at| end + at + 1)
}

/// Whether a blank line is written between `from`, where a block ended,
/// and `to`, where the next begins.
fn blank_between(source: &str, from: usize, to: usize) -> bool {
    let Some(between) = source.get(from..to) else {
        return false;
    };
    let whole_first = from == 0 || source.as_bytes()[from - 1] == b'\n';
    let mut parts: Vec<&str> = between.split('\n').collect();
    parts.pop();
    (parts.iter().enumerate())
        .filter(|(at, _)| *at > 0 || whole_first)
        .any(|(_, part)| {
            part.trim_matches(|c: char| c.is_whitespace() || c == '>')
                .is_empty()
        })
}

struct Draw<'a> {
    source: &'a str,
    width: usize,
    at: &'a Where,
    notes: &'a mut Notes,
    end: bool,
    /// Rows of the top-level block being drawn, and where in the text
    /// each one's line starts.
    rows: Vec<Line<'static>>,
    offsets: Vec<usize>,
    /// Where the line being drawn starts, and where each of the words'
    /// lines does.
    here: usize,
    starts: Vec<usize>,
    holders: Vec<Holder>,
    quotes: usize,
    /// Whether the next block in each container is its first.
    firsts: Vec<bool>,
    /// Where the last block that holds words ended.
    last_end: usize,
    leaf: Option<Leaf>,
    /// The words of a block, a line for each hard break.
    lines: Vec<Vec<Span<'static>>>,
    spans: Vec<Span<'static>>,
    styles: Vec<Style>,
    /// Each link open: where it goes, where its words start, and whether
    /// its address is shown after them.
    links: Vec<(String, usize, bool)>,
    /// Each HTML element open: its name, where its words start, how many
    /// styles were on before it, and an `<a>`'s address.
    html: Vec<(String, usize, usize, Option<String>)>,
    /// A picture's address and description, while its description is read.
    image: Option<(String, String)>,
    blocks: Vec<Block>,
    /// The top-level block open: where it starts, whether a blank line
    /// is before it, and its fence.
    top: Option<(usize, bool, Option<Fence>)>,
    /// The top-level block holds a table.
    top_holds: bool,
    /// The top-level block is a footnote's definition.
    top_note: bool,
    /// The top-level block is a paragraph.
    top_paragraph: bool,
    /// Its marks that run across lines.
    across: Vec<Range<usize>>,
    /// The footnotes it points at that are defined.
    top_notes: usize,
    /// Where the text drawn starts: before it, definitions read earlier,
    /// drawn only for what they define.
    skip: usize,
    /// Words with no paragraph around them, as a tight list's items hold:
    /// where they start and, as far as read, end.
    loose: Option<Range<usize>>,
    /// The top-level list's items after its first.
    items: Vec<Item>,
}

impl Draw<'_> {
    fn style(&self) -> Style {
        self.styles.last().copied().unwrap_or_default()
    }

    /// Rows drawn since the last mark: from the line at `here`.
    fn mark(&mut self) {
        let here = self.here;
        self.offsets.resize(self.rows.len(), here);
    }

    /// The style words start in: italic in a quote.
    fn base(&self) -> Style {
        match self.quotes {
            0 => Style::new(),
            _ => Style::new().italic(),
        }
    }

    /// Words with no paragraph around them end where a block begins or
    /// their container ends.
    fn close_loose(&mut self) {
        if let Some(range) = self.loose.take() {
            self.leaf = None;
            let mut lines = std::mem::take(&mut self.lines);
            lines.push(std::mem::take(&mut self.spans));
            self.styles.clear();
            self.links.clear();
            self.html.clear();
            self.words(lines);
            self.ended(range, true);
        }
    }

    fn event(&mut self, event: Event<'_>, range: Range<usize>) {
        if self.top_paragraph
            && matches!(
                event,
                Event::Start(_) | Event::Code(_) | Event::InlineMath(_) | Event::InlineHtml(_)
            )
            && self.source[range.clone()].contains('\n')
        {
            self.across.push(range.clone());
        }
        if let Some(Leaf::Table(_)) = &self.leaf {
            if let Event::End(TagEnd::Table) = event {
                let Some(Leaf::Table(table)) = self.leaf.take() else {
                    return;
                };
                self.table(table.clone());
                self.ended(table, true);
            }
            return;
        }
        if self.leaf.is_none() && words(&event) {
            self.begin(range.start);
            self.leaf = Some(Leaf::Words);
            self.styles = vec![self.base()];
            self.loose = Some(range.clone());
        }
        if let Some(loose) = &mut self.loose
            && words(&event)
        {
            loose.end = loose.end.max(range.end);
        }
        match event {
            Event::Start(tag) => self.start(tag, range),
            Event::End(tag) => self.stop(tag, range),
            Event::Text(text) => match (&mut self.leaf, &mut self.image) {
                (Some(Leaf::Code { text: code, .. }), _) => code.push_str(&text),
                (Some(Leaf::Html(html)), _) => html.push_str(&text),
                (_, Some((_, alt))) => alt.push_str(&text),
                _ => {
                    let style = self.style();
                    self.spans.push(Span::styled(text.into_string(), style));
                }
            },
            Event::Code(code) => {
                let style = self.style().fg(Color::Cyan);
                self.spans.push(Span::styled(code.into_string(), style));
            }
            Event::InlineMath(tex) | Event::DisplayMath(tex) => {
                let style = self.style().italic();
                let math = crate::math::unicode(tex.trim());
                self.spans.push(Span::styled(math, style));
            }
            Event::Html(html) => {
                if let Some(Leaf::Html(held)) = &mut self.leaf {
                    held.push_str(&html);
                }
            }
            Event::InlineHtml(html) => self.inline_html(&html),
            Event::FootnoteReference(name) => {
                self.top_notes += 1;
                let number = self.notes.number(&name);
                self.spans
                    .push(Span::styled(format!("[{number}]"), self.style().dim()));
            }
            // A line break in a reply, a message or a prompt is kept, as
            // Codex keeps it and as GitHub shows a comment; a document's
            // lines join, as GitHub shows a file.
            Event::SoftBreak if self.at.document => {
                let style = self.style();
                self.spans.push(Span::styled(" ", style));
            }
            Event::SoftBreak | Event::HardBreak => {
                let line = std::mem::take(&mut self.spans);
                self.lines.push(line);
                self.starts.push(range.end);
            }
            Event::Rule => {
                self.close_loose();
                self.begin(range.start);
                let rule = self.rule();
                self.words(vec![vec![rule]]);
                self.ended(range, true);
            }
            Event::TaskListMarker(done) => {
                if let Some(Holder::Item { mark, .. }) = self.holders.last_mut() {
                    let check = if done { "☑ " } else { "☐ " };
                    *mark = Some(vec![Span::raw(check).dim()]);
                }
            }
        }
    }

    /// A block begins at `start`: a blank row before it when one was
    /// written before it, unless it is the first in its container.
    fn begin(&mut self, start: usize) {
        self.here = start;
        self.starts = vec![start];
        let first = std::mem::replace(self.firsts.last_mut().expect("a level"), false);
        let gap = blank_between(self.source, self.last_end.max(self.skip), start);
        if self.holders.is_empty() {
            self.top = Some((start, gap, None));
            self.items.clear();
        } else if gap && !first {
            let mut row = vec![Span::raw("  ")];
            row.extend(bars(self.quotes.min(QUOTE_DEPTH)));
            self.rows.push(Line::from(row));
            self.mark();
        }
    }

    /// A block that holds words ended at `range`: past it, where the next
    /// one's blank line is looked for; at the top level, its rows go out.
    fn ended(&mut self, range: Range<usize>, leaf: bool) {
        if leaf {
            self.last_end = range.end;
        }
        if self.holders.is_empty()
            && let Some((start, gap, fence)) = self.top.take()
        {
            let note = std::mem::take(&mut self.top_note);
            let items = std::mem::take(&mut self.items);
            let paragraph = std::mem::take(&mut self.top_paragraph);
            let across = std::mem::take(&mut self.across);
            let notes = std::mem::take(&mut self.top_notes);
            let holds = std::mem::take(&mut self.top_holds);
            if start < self.skip {
                self.rows.clear();
                self.offsets.clear();
                return;
            }
            // Past its last words, not the blank lines a list's range takes
            // in after them: those go before the next block.
            let end = line_end(
                self.source,
                match self.last_end > start {
                    true => self.last_end,
                    false => range.end,
                },
            );
            let broken = footnote_marks(&self.source[start..end]) > notes;
            self.blocks.push(Block {
                start,
                end,
                gap,
                rows: std::mem::take(&mut self.rows),
                offsets: std::mem::take(&mut self.offsets),
                holds,
                note,
                items,
                paragraph,
                across,
                broken,
                fence,
            });
        }
    }

    fn start(&mut self, tag: Tag<'_>, range: Range<usize>) {
        if !matches!(
            tag,
            Tag::Emphasis | Tag::Strong | Tag::Strikethrough | Tag::Link { .. } | Tag::Image { .. }
        ) {
            self.close_loose();
        }
        match tag {
            Tag::Paragraph => {
                self.top_paragraph |= self.holders.is_empty();
                self.begin(range.start);
                self.leaf = Some(Leaf::Words);
                self.styles = vec![self.base()];
            }
            Tag::Heading { level, .. } => {
                self.begin(range.start);
                self.leaf = Some(Leaf::Words);
                let level = match level {
                    HeadingLevel::H1 => 1,
                    HeadingLevel::H2 => 2,
                    HeadingLevel::H3 => 3,
                    HeadingLevel::H4 => 4,
                    HeadingLevel::H5 => 5,
                    HeadingLevel::H6 => 6,
                };
                // Prose takes no hue: a heading is bold, and its marks stay,
                // dim, so its level reads.
                self.spans
                    .push(Span::raw(format!("{} ", "#".repeat(level))).dim());
                self.styles = vec![Style::new().add_modifier(Modifier::BOLD)];
            }
            Tag::BlockQuote(_) => {
                self.begin(range.start);
                self.holders.push(Holder::Quote);
                self.quotes += 1;
                self.firsts.push(true);
            }
            Tag::CodeBlock(kind) => {
                self.begin(range.start);
                let fence = match kind {
                    CodeBlockKind::Fenced(_) => {
                        let line = self.source[range.start..].trim_start_matches([' ', '\t']);
                        Fence::opened_by(line)
                    }
                    CodeBlockKind::Indented => None,
                };
                self.leaf = Some(Leaf::Code {
                    fence,
                    range,
                    text: String::new(),
                });
            }
            Tag::HtmlBlock => {
                self.begin(range.start);
                self.leaf = Some(Leaf::Html(String::new()));
            }
            Tag::List(start) => {
                self.begin(range.start);
                self.holders.push(Holder::List(start));
                self.firsts.push(true);
            }
            Tag::Item => {
                let next = !self.firsts.last().copied().unwrap_or(true);
                let cut = line_end(self.source, self.last_end);
                let rows = self.rows.len();
                self.begin(range.start);
                if let [Holder::List(number)] = self.holders[..]
                    && next
                    && cut <= range.start
                    && self.top.as_ref().is_some_and(|(start, ..)| *start < cut)
                {
                    self.items.push(Item {
                        cut,
                        rows,
                        gap_rows: self.rows.len() - rows,
                        number,
                    });
                }
                let mark = match self.holders.last_mut() {
                    Some(Holder::List(Some(number))) => {
                        let mark = format!("{number}. ");
                        *number += 1;
                        mark
                    }
                    _ => "- ".to_owned(),
                };
                let hang = mark.width();
                self.holders.push(Holder::Item {
                    mark: Some(vec![Span::raw(mark).dim()]),
                    hang,
                });
                self.firsts.push(true);
            }
            Tag::FootnoteDefinition(name) => {
                self.top_note |= self.holders.is_empty();
                self.begin(range.start);
                let mark = format!("[{}] ", self.notes.number(&name));
                let hang = mark.width();
                self.holders.push(Holder::Item {
                    mark: Some(vec![Span::raw(mark).dim()]),
                    hang,
                });
                self.firsts.push(true);
            }
            Tag::Table(_) => {
                self.begin(range.start);
                self.top_holds = true;
                self.leaf = Some(Leaf::Table(range));
            }
            Tag::Emphasis => self.styles.push(self.style().italic()),
            Tag::Strong => self.styles.push(self.style().bold()),
            Tag::Strikethrough => self.styles.push(self.style().crossed_out()),
            Tag::Link {
                link_type,
                dest_url,
                ..
            } => {
                let shown = !matches!(link_type, LinkType::Autolink | LinkType::Email);
                self.links
                    .push((dest_url.into_string(), self.spans.len(), shown));
                self.styles.push(self.style().underlined());
            }
            Tag::Image { dest_url, .. } => {
                self.image = Some((dest_url.into_string(), String::new()));
            }
            _ => {}
        }
    }

    fn stop(&mut self, tag: TagEnd, range: Range<usize>) {
        if matches!(
            tag,
            TagEnd::Item | TagEnd::FootnoteDefinition | TagEnd::BlockQuote(_) | TagEnd::List(_)
        ) {
            self.close_loose();
        }
        match tag {
            TagEnd::Paragraph | TagEnd::Heading(_) => {
                self.leaf = None;
                let mut lines = std::mem::take(&mut self.lines);
                lines.push(std::mem::take(&mut self.spans));
                self.styles.clear();
                self.links.clear();
                self.html.clear();
                // A paragraph of pictures alone draws them.
                if matches!(tag, TagEnd::Paragraph)
                    && self.quotes == 0
                    && let Some(found) = pictures(&self.source[range.clone()].replace('\n', " "))
                {
                    let rows = self.pictures(&found);
                    self.framed(rows);
                } else {
                    self.words(lines);
                }
                self.ended(range, true);
            }
            TagEnd::CodeBlock => {
                if let Some(Leaf::Code { fence, range, text }) = self.leaf.take() {
                    // Closed: its block has a line past its code for the
                    // closing marks.
                    let closed = fence.is_some()
                        && self.source[range.clone()].lines().count() >= text.lines().count() + 2;
                    // Open at the top level, where the text ends.
                    if !closed
                        && self.holders.is_empty()
                        && let Some(top) = &mut self.top
                    {
                        top.2 = fence.clone();
                    }
                    self.here =
                        range_start(&self.source[range.clone()], range.start, fence.is_some());
                    self.code(fence, closed, &text);
                }
                self.ended(range, true);
            }
            TagEnd::HtmlBlock => {
                if let Some(Leaf::Html(html)) = self.leaf.take() {
                    self.here = range.start;
                    self.html_block(&html);
                }
                self.ended(range, true);
            }
            TagEnd::BlockQuote(_) => {
                self.holders.pop();
                self.quotes -= 1;
                self.firsts.pop();
                self.ended(range, false);
            }
            TagEnd::List(_) | TagEnd::Item | TagEnd::FootnoteDefinition => {
                // An item with nothing in it still shows its mark.
                if let Some(Holder::Item { mark: Some(_), .. }) = self.holders.last() {
                    self.words(vec![Vec::new()]);
                }
                self.holders.pop();
                self.firsts.pop();
                self.ended(range, false);
            }
            TagEnd::Emphasis | TagEnd::Strong | TagEnd::Strikethrough => {
                self.styles.pop();
            }
            TagEnd::Link => {
                self.styles.pop();
                if let Some((url, at, shown)) = self.links.pop() {
                    self.address(&url, at, shown);
                }
            }
            TagEnd::Image => {
                if let Some((src, alt)) = self.image.take() {
                    let base = self.style();
                    self.spans.push(Span::styled("▣ ", base.dim()));
                    let at = self.spans.len();
                    self.spans.push(Span::styled(alt, base.underlined()));
                    self.address(&src, at, true);
                }
            }
            _ => {}
        }
    }

    /// A link's address after its words, when it says more than they do.
    fn address(&mut self, url: &str, at: usize, shown: bool) {
        let label: String = (self.spans[at.min(self.spans.len())..].iter())
            .map(|s| s.content.as_ref())
            .collect();
        if shown && !url.is_empty() && url != label && !url.starts_with('#') {
            let base = self.styles.first().copied().unwrap_or_default();
            self.spans
                .push(Span::styled(format!(" ({url})"), base.dim()));
        }
    }

    /// The marks a row starts with inside its containers: the first row's,
    /// which takes an item's mark if it has not been drawn, and the rest's.
    fn leads(&mut self) -> (Vec<Span<'static>>, Vec<Span<'static>>) {
        let (mut first, mut rest) = (Vec::new(), Vec::new());
        let mut quotes = 0;
        for holder in &mut self.holders {
            match holder {
                Holder::Quote => {
                    quotes += 1;
                    if quotes <= QUOTE_DEPTH {
                        first.extend(bars(1));
                        rest.extend(bars(1));
                    }
                }
                Holder::List(_) => {}
                Holder::Item { mark, hang } => {
                    let pad = " ".repeat(*hang);
                    match mark.take() {
                        Some(mark) => first.extend(mark),
                        None => first.push(Span::raw(pad.clone())),
                    }
                    rest.push(Span::raw(pad));
                }
            }
        }
        (first, rest)
    }

    /// Lines of words wrapped at the width, under their containers' marks.
    fn words(&mut self, lines: Vec<Vec<Span<'static>>>) {
        let (first, rest) = self.leads();
        let mut hang = vec![Span::raw("  ")];
        hang.extend(rest.iter().cloned());
        let starts = std::mem::take(&mut self.starts);
        for (at, spans) in lines.into_iter().enumerate() {
            if let Some(start) = starts.get(at) {
                self.here = *start;
            }
            let mut line = match at {
                0 => first.clone(),
                _ => rest.clone(),
            };
            line.extend(spans);
            let rows = wrap_lines(
                Line::from(line),
                self.width,
                Line::from("  "),
                Line::from(hang.clone()),
            );
            self.rows.extend(rows);
            self.mark();
        }
    }

    /// Rows drawn on their own (a table, a picture), each starting with a
    /// reply's two columns: moved under their containers' marks.
    fn framed(&mut self, rows: Vec<Line<'static>>) {
        if self.holders.is_empty() {
            self.rows.extend(rows);
            self.mark();
            return;
        }
        let (first, rest) = self.leads();
        for (at, row) in rows.into_iter().enumerate() {
            let mut lead = vec![Span::raw("  ")];
            lead.extend(if at == 0 { first.clone() } else { rest.clone() });
            self.rows.push(relead(row, &Line::from(lead)));
        }
        self.mark();
    }

    /// A rule, as wide as a turn's closing rule where there is room.
    fn rule(&self) -> Span<'static> {
        let wide = width_hint().min(self.inner().saturating_sub(2));
        Span::raw("─".repeat(wide)).dim()
    }

    /// The width rows drawn on their own take inside the containers.
    fn inner(&self) -> usize {
        let marks: usize = (self.holders.iter())
            .map(|holder| match holder {
                Holder::Quote => 2,
                Holder::List(_) => 0,
                Holder::Item { hang, .. } => *hang,
            })
            .sum();
        self.width.saturating_sub(marks).max(10)
    }

    fn table(&mut self, range: Range<usize>) {
        let rows: Vec<String> = (self.source[range].lines())
            .map(|line| {
                let mut line = line.trim();
                for _ in 0..self.quotes {
                    line = line.strip_prefix('>').unwrap_or(line).trim_start();
                }
                line.to_owned()
            })
            .filter(|line| !line.is_empty())
            .collect();
        let drawn = crate::table::draw(&rows, self.inner());
        self.framed(drawn);
    }

    fn code(&mut self, fence: Option<Fence>, closed: bool, text: &str) {
        let lang = fence.as_ref().map_or("", |fence| fence.lang.as_str());
        if drawn(lang, self.at.document) {
            if closed {
                let rows = fence_picture(lang.to_owned(), text.to_owned(), self.inner());
                self.framed(rows);
                return;
            }
            // Gathered until it closes; shown as the code it is where the
            // words end without closing it.
            if !self.end {
                return;
            }
            self.code_lines(None, text);
            return;
        }
        let code = crate::highlight::Code::language(lang);
        self.code_lines(code, text);
    }

    fn code_lines(&mut self, mut code: Option<crate::highlight::Code>, text: &str) {
        for line in text.lines() {
            let spans = code_line(&mut code, line);
            self.words_hung(spans);
            self.here = self.source[self.here..]
                .find('\n')
                .map_or(self.here, |at| self.here + at + 1);
        }
    }

    /// A line of code: wrapped rows hang two columns in, under its text.
    fn words_hung(&mut self, spans: Vec<Span<'static>>) {
        let (first, rest) = self.leads();
        let mut hang = vec![Span::raw("  ")];
        hang.extend(rest);
        hang.push(Span::raw("  "));
        let mut line = first;
        line.extend(spans);
        let rows = wrap_lines(
            Line::from(line),
            self.width,
            Line::from("  "),
            Line::from(hang),
        );
        self.rows.extend(rows);
        self.mark();
    }

    /// HTML a README is written with, a line at a time, read for what it
    /// means: pictures drawn, a rule, its words; tags that only lay the
    /// page out, and comments, leave no row.
    fn html_block(&mut self, html: &str) {
        let mut comment = false;
        for (at, line) in html.lines().enumerate() {
            if at > 0 {
                self.here = self.source[self.here..]
                    .find('\n')
                    .map_or(self.here, |at| self.here + at + 1);
            }
            let mut line = line;
            if comment {
                match line.split_once("-->") {
                    Some((_, after)) => {
                        comment = false;
                        line = after;
                    }
                    None => continue,
                }
            }
            if let Some(after) = line.trim_start().strip_prefix("<!--")
                && !after.contains("-->")
            {
                comment = true;
                continue;
            }
            if line.trim().is_empty() {
                continue;
            }
            if let Some(found) = pictures(line) {
                let rows = self.pictures(&found);
                self.framed(rows);
                continue;
            }
            if matches!(line.trim(), "<hr>" | "<hr/>" | "<hr />") {
                let rule = self.rule();
                self.words(vec![vec![rule]]);
                continue;
            }
            let spans = html_words(line, Style::new());
            if line.contains('<') && spans.iter().all(|span| span.content.trim().is_empty()) {
                continue;
            }
            self.words(vec![spans]);
        }
    }

    /// An HTML tag among words: what it means read, as a page shows it; a
    /// tag of no element a page knows is the words it is.
    fn inline_html(&mut self, raw: &str) {
        let tag = html_tag(raw).filter(|(taken, _)| *taken == raw.len());
        let Some((_, tag)) = tag else {
            let style = self.style();
            self.spans.push(Span::styled(raw.to_owned(), style));
            return;
        };
        let base = self.style();
        if tag.closing {
            let Some(at) = self.html.iter().rposition(|(name, ..)| *name == tag.name) else {
                return;
            };
            let (_, words_at, styles, href) = self.html.remove(at);
            self.html.truncate(at);
            self.styles.truncate(styles.max(1));
            match tag.name.as_str() {
                "a" => {
                    if let Some(href) = href {
                        self.address(&href, words_at, true);
                    }
                }
                "kbd" => self.spans.push(Span::styled(" ", base)),
                _ => {}
            }
            return;
        }
        let styles = self.styles.len();
        let open = |draw: &mut Self, style: Option<Style>, href: Option<String>| {
            if let Some(style) = style {
                draw.styles.push(style);
            }
            draw.html
                .push((tag.name.clone(), draw.spans.len(), styles, href));
        };
        match tag.name.as_str() {
            "b" | "strong" => open(self, Some(base.bold()), None),
            "i" | "em" => open(self, Some(base.italic()), None),
            "code" | "tt" | "samp" => open(self, Some(base.fg(Color::Cyan)), None),
            "kbd" => {
                self.spans.push(Span::styled(" ", base.reversed()));
                open(self, Some(base.reversed()), None);
            }
            "del" | "s" | "strike" => open(self, Some(base.crossed_out()), None),
            "u" | "ins" => open(self, Some(base.underlined()), None),
            "a" => {
                let href = tag.attribute("href").filter(|href| !href.is_empty());
                open(self, Some(base.underlined()), href);
            }
            "summary" => {
                self.spans.push(Span::styled("▸ ", base.dim()));
                open(self, Some(base.bold()), None);
            }
            "h1" | "h2" | "h3" | "h4" | "h5" | "h6" => {
                let level = usize::from(tag.name.as_bytes()[1] - b'0');
                self.spans
                    .push(Span::styled(format!("{} ", "#".repeat(level)), base.dim()));
                open(self, Some(base.bold()), None);
            }
            "br" => self.spans.push(Span::styled(" ", base)),
            "li" => self.spans.push(Span::styled("- ", base.dim())),
            "img" => {
                let picture = Picture {
                    alt: tag.attribute("alt").unwrap_or_default(),
                    src: tag.attribute("src").unwrap_or_default(),
                };
                self.spans.extend(picture_words(&picture, false));
            }
            "!--" => {}
            _ => open(self, None, None),
        }
    }

    /// Pictures a block holds and nothing else, one under another; those
    /// that cannot be drawn here (another machine's, or a terminal with no
    /// pictures) as their words, together on a row.
    fn pictures(&self, found: &[Picture]) -> Vec<Line<'static>> {
        let width = self.inner();
        let mut out = Vec::new();
        let mut words: Vec<Span<'static>> = Vec::new();
        let flush = |words: &mut Vec<Span<'static>>, out: &mut Vec<Line<'static>>| {
            if !words.is_empty() {
                let line = Line::from(std::mem::take(words));
                out.extend(wrap(line, width, Span::raw("  "), Span::raw("  ")));
            }
        };
        for picture in found {
            let path = path(self.at, &picture.src);
            match (path.as_deref()).and_then(|path| picture_file(path, width, picture)) {
                Some(rows) => {
                    flush(&mut words, &mut out);
                    out.extend(rows);
                }
                None => {
                    if !words.is_empty() {
                        words.push(Span::raw("  "));
                    }
                    words.extend(picture_words(picture, path.is_some()));
                }
            }
        }
        flush(&mut words, &mut out);
        out
    }
}

/// Where a code block's first line of code starts: past a fence's line.
fn range_start(block: &str, start: usize, fenced: bool) -> usize {
    match fenced {
        true => block
            .find('\n')
            .map_or(start + block.len(), |at| start + at + 1),
        false => start,
    }
}

/// Whether an event is words, or marks among them.
fn words(event: &Event<'_>) -> bool {
    matches!(
        event,
        Event::Text(_)
            | Event::Code(_)
            | Event::InlineMath(_)
            | Event::DisplayMath(_)
            | Event::InlineHtml(_)
            | Event::FootnoteReference(_)
            | Event::SoftBreak
            | Event::HardBreak
            | Event::Start(
                Tag::Emphasis
                    | Tag::Strong
                    | Tag::Strikethrough
                    | Tag::Link { .. }
                    | Tag::Image { .. }
            )
    )
}

/// A line of code in its language's colors, or cyan where none is known,
/// two columns in.
fn code_line(code: &mut Option<crate::highlight::Code>, line: &str) -> Vec<Span<'static>> {
    let mut spans = vec![Span::raw("  ")];
    match code {
        Some(code) => spans.extend(code.line(line)),
        None => spans.push(Span::styled(line.to_owned(), Style::new().fg(Color::Cyan))),
    }
    spans
}

/// The file a picture's address names on this machine, if it does.
fn path(at: &Where, src: &str) -> Option<std::path::PathBuf> {
    let src = src.trim();
    if let Some(url) = src.strip_prefix("file://") {
        return crate::pane::file_url(&format!("file://{url}")).map(|(path, _)| path);
    }
    if src.contains("://") || src.starts_with("data:") || src.starts_with('#') {
        return None;
    }
    let src = src.split(['?', '#']).next().unwrap_or(src);
    // `%20` and the like read back, as a browser reads them.
    let (src, _) = crate::pane::file_url(&format!("file://{src}"))?;
    let src = src.to_str()?;
    let path = match src.strip_prefix("~/") {
        Some(rest) => std::path::Path::new(&std::env::var_os("HOME")?).join(rest),
        None => match &at.folder {
            Some(folder) => folder.join(src),
            None => std::env::current_dir().ok()?.join(src),
        },
    };
    path.is_file().then_some(path)
}

/// Math written as many models write it, read as Markdown's: `\(…\)` as
/// `$…$`, and `$$` or `\[` display math, on its own lines or one, as a
/// ```math fence, as a `$$` line has always drawn.
#[derive(Clone, Default)]
struct Rewrite {
    /// The fence open, so math marks inside code stay code.
    fence: Option<Fence>,
    /// Inside display math: what closes it, `$$` or `\]`.
    math: Option<&'static str>,
}

impl Rewrite {
    /// `line` as Markdown reads it: one line, or a math fence's lines.
    fn line(&mut self, line: &str, out: &mut String) {
        if let Some(fence) = &self.fence {
            if fence.closed_by(line) {
                self.fence = None;
            }
            push_line(out, line);
            return;
        }
        let trimmed = line.trim_start();
        let indent = &line[..line.len() - trimmed.len()];
        if let Some(close) = self.math {
            if let Some(after) = trimmed.strip_prefix(close) {
                let _ = after;
                self.math = None;
                push_line(out, &format!("{indent}```"));
            } else {
                push_line(out, line);
            }
            return;
        }
        if let Some(fence) = Fence::opened_by(line) {
            self.fence = Some(fence);
            push_line(out, line);
            return;
        }
        let bare = trimmed.trim_end();
        let open = [("$$", "$$"), ("\\[", "\\]")]
            .into_iter()
            .find(|(open, _)| bare.starts_with(open));
        if let Some((open, close)) = open {
            let inner = &bare[open.len()..];
            if let Some(math) = inner.strip_suffix(close) {
                push_line(out, &format!("{indent}```math"));
                push_line(out, &format!("{indent}{math}"));
                push_line(out, &format!("{indent}```"));
                return;
            }
            // `\[` opens display math only alone on its line: one that
            // runs on is an escaped bracket.
            if open == "$$" || inner.is_empty() {
                self.math = Some(close);
                push_line(out, &format!("{indent}```math"));
                if !inner.trim().is_empty() {
                    push_line(out, &format!("{indent}{inner}"));
                }
                return;
            }
        }
        push_line(out, &parens(line));
    }
}

fn push_line(out: &mut String, line: &str) {
    out.push_str(line);
    out.push('\n');
}

/// `\(math\)` as `$math$`, outside code spans.
fn parens(line: &str) -> std::borrow::Cow<'_, str> {
    if !line.contains("\\(") {
        return line.into();
    }
    let mut out = String::with_capacity(line.len());
    let mut rest = line;
    while !rest.is_empty() {
        if rest.starts_with('`') {
            // A code span, to its closing run, as written.
            let run = rest.len() - rest.trim_start_matches('`').len();
            let ticks = &rest[..run];
            match rest[run..].find(ticks) {
                Some(end) => {
                    let taken = run + end + run;
                    out.push_str(&rest[..taken]);
                    rest = &rest[taken..];
                }
                None => {
                    out.push_str(rest);
                    rest = "";
                }
            }
            continue;
        }
        if let Some(after) = rest.strip_prefix("\\(") {
            // No closer after this opener: none after any later one.
            let Some(end) = after.find("\\)") else {
                out.push_str(rest);
                break;
            };
            if end == 0 {
                out.push_str("\\(");
                rest = after;
                continue;
            }
            out.push('$');
            out.push_str(after[..end].trim());
            out.push('$');
            rest = &after[end + 2..];
            continue;
        }
        let c = rest.chars().next().expect("a character");
        out.push(c);
        rest = &rest[c.len_utf8()..];
    }
    out.into()
}

/// An unfinished line as it will read once its marks close: an open code
/// span, emphasis or link closed, so it shows styled before its closer
/// arrives. Only what is shown; the words themselves are never changed.
fn closed(line: &str) -> String {
    let bytes = line.as_bytes();
    let mut open: Vec<&str> = Vec::new();
    let mut code: Option<usize> = None;
    let mut at = 0;
    while at < bytes.len() {
        let c = bytes[at];
        if c == b'\\' {
            at += 2;
            continue;
        }
        if c == b'`' {
            let run = bytes[at..].iter().take_while(|b| **b == b'`').count();
            match code {
                Some(open) if open == run => code = None,
                None => code = Some(run),
                _ => {}
            }
            at += run;
            continue;
        }
        if code.is_some() {
            at += 1;
            continue;
        }
        if matches!(c, b'*' | b'_' | b'~') {
            let run = bytes[at..].iter().take_while(|b| **b == c).count();
            let before = at.checked_sub(1).map(|b| bytes[b]);
            let after = bytes.get(at + run).copied();
            let mark = &line[at..at + run.min(if c == b'~' { 2 } else { 3 })];
            let closes = open.last() == Some(&mark) && before.is_some_and(|b| b != b' ');
            let word_before = before.is_some_and(|b| b.is_ascii_alphanumeric());
            if closes {
                open.pop();
            } else if after.is_some_and(|b| b != b' ')
                && !(c == b'_' && word_before)
                && (c != b'~' || run == 2)
            {
                open.push(mark);
            }
            at += run;
            continue;
        }
        at += 1;
    }
    let mut out = line.trim_end().to_owned();
    if let Some(run) = code {
        out.push_str(&"`".repeat(run));
        return out;
    }
    while let Some(mark) = open.pop() {
        out.push_str(mark);
    }
    if let Some(start) = out.rfind("](")
        && !out[start..].contains(')')
    {
        out.push(')');
    }
    out
}

/// Markdown read one line at a time, as a reply streams in: each line
/// [`Markdown::feed`] reads hands out the rows that will not change, the
/// rows of the block still open shown by [`Markdown::open_last`] until they
/// are, and [`Markdown::end`] the rest.
#[derive(Default, Clone)]
pub struct Markdown {
    at: Where,
    notes: Notes,
    rewrite: Rewrite,
    /// The text read since the last block that ended.
    source: String,
    /// The link and footnote definitions in the blocks that ended, which
    /// the text still coming can point at.
    defs: String,
    /// The open block's rows already handed out, and its last drawing.
    given: usize,
    last: Vec<Line<'static>>,
    /// Whether the open block's blank row went out before its rows.
    gapped: bool,
    /// Whether any row went out: the first takes no blank row before it.
    any: bool,
    /// The open block holds its rows until it ends, as a table does, and
    /// whether a blank row goes before them.
    held: Option<bool>,
    /// The last preview: the line it showed, at what width, whether first,
    /// how many rows at most, and its rows. A frame drawn with no new words
    /// draws none again.
    shown: std::cell::RefCell<Option<Shown>>,
    /// A fence at the top level open: read a line at a time, its code
    /// colored as it comes, or a picture's source gathered until it closes.
    fence: Option<Open>,
}

#[derive(Clone)]
struct Shown {
    partial: String,
    width: usize,
    first: bool,
    most: usize,
    rows: Vec<Line<'static>>,
}

#[derive(Clone)]
struct Open {
    fence: Fence,
    /// A blank row goes before its first row.
    gap: bool,
    code: Option<crate::highlight::Code>,
    picture: Option<String>,
}

impl Markdown {
    /// A reply's Markdown, its pictures' paths read from `folder`.
    pub fn in_folder(folder: &std::path::Path) -> Self {
        Self {
            at: Where {
                folder: Some(folder.to_owned()),
                document: false,
            },
            ..Self::default()
        }
    }

    /// A Markdown file in `folder`, read as a document.
    pub fn document(folder: &std::path::Path) -> Self {
        let mut markdown = Self::in_folder(folder);
        markdown.at.document = true;
        markdown
    }

    /// A streamed line: the rows it leaves that will not change. `first`
    /// says nothing of the reply is shown yet, so its first row gets the
    /// bullet.
    pub fn feed(&mut self, text: &str, width: usize, first: bool) -> Vec<Line<'static>> {
        *self.shown.get_mut() = None;
        let mut lines = String::new();
        self.rewrite.line(text, &mut lines);
        let mut out = Vec::new();
        for line in lines.lines() {
            self.read(line, width, &mut out);
        }
        bullet(&mut out, first);
        out
    }

    fn read(&mut self, line: &str, width: usize, out: &mut Vec<Line<'static>>) {
        if let Some(open) = &mut self.fence {
            if open.fence.closed_by(line) {
                let open = self.fence.take().expect("open");
                if let Some(source) = open.picture {
                    let rows = fence_picture(open.fence.lang, source, width);
                    self.give(&rows, open.gap, out);
                }
                return;
            }
            match &mut open.picture {
                Some(source) => push_line(source, line),
                None => {
                    let spans = code_line(&mut open.code, line);
                    let rows = wrap_lines(
                        Line::from(spans),
                        width,
                        Line::from("  "),
                        Line::from("    "),
                    );
                    let gap = std::mem::take(&mut open.gap);
                    self.give(&rows, gap, out);
                }
            }
            return;
        }
        push_line(&mut self.source, line);
        let (mut blocks, links) = draw_defined(
            &self.defs,
            &self.source,
            width,
            &self.at,
            &mut self.notes,
            false,
        );
        let Some(open) = blocks.pop() else {
            return;
        };
        // A block pointing at a link or footnote not defined yet waits for
        // it, and what comes after with it: its words go out as they read
        // once it is, or when the reply ends.
        let waits = (blocks.iter())
            .position(|block| block.broken)
            .unwrap_or(blocks.len());
        let waiting = waits < blocks.len() || open.broken;
        blocks.truncate(waits);
        let mut drained = 0;
        if let Some(ended) = blocks.last().map(|block| block.end) {
            drained = ended;
            for (at, block) in blocks.iter().enumerate() {
                let from = if at == 0 { self.given } else { 0 };
                let gap = block.gap && !(at == 0 && self.gapped);
                self.give(&block.rows[from.min(block.rows.len())..], gap, out);
            }
            let notes = (blocks.iter())
                .filter(|block| block.note)
                .map(|block| block.start..block.end);
            for span in (links.iter())
                .filter(|span| span.end <= ended)
                .cloned()
                .chain(notes)
            {
                self.defs.push_str(&self.source[span]);
                self.defs.push_str("\n\n");
            }
            self.source.drain(..ended);
            (self.given, self.gapped) = (0, false);
            self.last.clear();
        }
        if waiting {
            self.last.clear();
            self.held = None;
            return;
        }
        // A fence open at the top level is read a line at a time from here.
        if let Some(fence) = open.fence {
            let gap = open.gap && !self.gapped;
            let rows = &open.rows[self.given.min(open.rows.len())..];
            self.give(rows, gap, out);
            let gap = gap && rows.is_empty();
            let opened = &self.source[open.start.min(self.source.len())..];
            let body = opened.split_once('\n').map_or("", |(_, body)| body);
            let picture = drawn(&fence.lang, self.at.document).then(|| body.to_owned());
            let mut code = match picture {
                Some(_) => None,
                None => crate::highlight::Code::language(&fence.lang),
            };
            if code.is_some() {
                for line in body.lines() {
                    code_line(&mut code, line);
                }
            }
            self.fence = Some(Open {
                fence,
                gap,
                code,
                picture,
            });
            self.source.clear();
            (self.given, self.gapped) = (0, false);
            self.last.clear();
            self.held = None;
            return;
        }
        let (mut rows, mut gap) = (open.rows, open.gap);
        // A list at the top level reads on from its last item, as the list
        // that item starts: the items before it are done.
        if let Some(item) = open.items.last().filter(|_| !open.holds) {
            let cut = item.cut - drained;
            if item.rows > self.given {
                let gap = gap && !self.gapped;
                self.give(&rows[self.given..item.rows], gap, out);
            }
            for span in links.iter().filter(|span| span.end <= item.cut) {
                self.defs
                    .push_str(&self.source[span.start - drained..span.end - drained]);
                self.defs.push_str("\n\n");
            }
            self.source.drain(..cut);
            if let Some(number) = item.number {
                renumber(&mut self.source, number);
            }
            let done = item.rows + item.gap_rows;
            self.gapped = self.given > item.rows;
            self.given = self.given.saturating_sub(done);
            self.last = self.last.split_off(done.min(self.last.len()));
            rows = rows.split_off(done);
            gap = item.gap_rows > 0;
        }
        // The open block's rows a new line left as they were will not
        // change: they go out.
        let settled = match open.holds {
            true => 0,
            false => (self.last.iter().zip(&rows))
                .take_while(|(was, is)| was == is)
                .count(),
        };
        if settled > self.given {
            let gap = gap && !self.gapped;
            self.give(&rows[self.given..settled], gap, out);
            self.gapped = true;
            self.given = settled;
        }
        self.last = rows;
        self.held = open.holds.then_some(gap);
        // A reply's paragraph, whose every line is a row of its own, reads
        // on from its first line not out, when that line starts with a word
        // and so goes on as the words it is: the lines out are done.
        if open.paragraph
            && !self.at.document
            && self.given > 0
            && let Some(&cut) = open.offsets.get(self.given)
            && open.offsets[self.given - 1] < cut
            && !(open.across.iter()).any(|marks| marks.start < cut && cut < marks.end)
            && let Some(from) = cut.checked_sub(drained)
            && starts_with_word(&self.source[from..])
        {
            self.source.drain(..from);
            self.last.drain(..self.given);
            self.given = 0;
        }
    }

    /// Rows going out, after a blank row when one was written before them.
    fn give(&mut self, rows: &[Line<'static>], gap: bool, out: &mut Vec<Line<'static>>) {
        if rows.is_empty() {
            return;
        }
        if gap && self.any {
            out.push(Line::from("  "));
        }
        out.extend(rows.iter().cloned());
        self.any = true;
    }

    /// All the rows not handed out yet.
    #[cfg(test)]
    pub fn open(&self, partial: &str, width: usize, first: bool) -> Vec<Line<'static>> {
        self.open_last(partial, width, first, usize::MAX)
    }

    /// The last `most` of the rows not handed out yet, as they read now,
    /// with `partial`, the line still coming, read as it will once its marks
    /// close: as many as a screen shows.
    pub fn open_last(
        &self,
        partial: &str,
        width: usize,
        first: bool,
        most: usize,
    ) -> Vec<Line<'static>> {
        // A table's rows wait for its lines to end: the line still coming
        // changes nothing.
        let partial = if self.held.is_some() { "" } else { partial };
        if let Some(shown) = &*self.shown.borrow()
            && shown.partial == partial
            && (shown.width, shown.first, shown.most) == (width, first, most)
        {
            return shown.rows.clone();
        }
        let rows = self.preview(partial, width, first, most);
        *self.shown.borrow_mut() = Some(Shown {
            partial: partial.to_owned(),
            width,
            first,
            most,
            rows: rows.clone(),
        });
        rows
    }

    fn preview(&self, partial: &str, width: usize, first: bool, most: usize) -> Vec<Line<'static>> {
        let mut out = Vec::new();
        if let Some(open) = &self.fence {
            if open.picture.is_none() && !partial.is_empty() {
                let mut code = open.code.clone();
                let spans = code_line(&mut code, partial);
                out = wrap_lines(
                    Line::from(spans),
                    width,
                    Line::from("  "),
                    Line::from("    "),
                );
                if open.gap && self.any {
                    out.insert(0, Line::from("  "));
                }
            }
            bullet(&mut out, first && !self.any);
            return out;
        }
        // A table is drawn as its lines end: the line still coming waits
        // for its own end, so no column widens and narrows as it is typed.
        if let Some(gap) = self.held {
            let gap = usize::from(gap && self.any && !self.last.is_empty());
            let from = (self.last.len() + gap).saturating_sub(most);
            if gap > from {
                out.push(Line::from("  "));
            }
            out.extend(self.last[from.saturating_sub(gap)..].iter().cloned());
            bullet(&mut out, from == 0 && first && !self.any);
            return out;
        }
        let mut source = self.source.clone();
        if !partial.is_empty() {
            let shown = match Fence::opened_by(partial) {
                Some(_) => partial.to_owned(),
                None => closed(partial),
            };
            self.rewrite.clone().line(&shown, &mut source);
        }
        let mut notes = self.notes.clone();
        let (blocks, _) = draw_defined(&self.defs, &source, width, &self.at, &mut notes, false);
        let mut any = self.any;
        for (at, block) in blocks.iter().enumerate() {
            let from = if at == 0 { self.given } else { 0 };
            let rows = &block.rows[from.min(block.rows.len())..];
            if rows.is_empty() {
                continue;
            }
            if block.gap && any && !(at == 0 && self.gapped) {
                out.push(Line::from("  "));
            }
            out.extend(rows.iter().cloned());
            any = true;
        }
        bullet(&mut out, first && !self.any);
        out.drain(..out.len().saturating_sub(most));
        out
    }

    /// The rest when the reply ends: the open block, and a fence or `$$`
    /// block that never closed, shown as the code it is.
    pub fn end(&mut self, width: usize, first: bool) -> Vec<Line<'static>> {
        let mut out = Vec::new();
        if let Some(open) = self.fence.take()
            && let Some(source) = open.picture
        {
            let mut code = None;
            let rows: Vec<Line<'static>> = (source.lines())
                .flat_map(|line| {
                    wrap_lines(
                        Line::from(code_line(&mut code, line)),
                        width,
                        Line::from("  "),
                        Line::from("    "),
                    )
                })
                .collect();
            self.give(&rows, open.gap, &mut out);
        }
        let (blocks, _) = draw_defined(
            &self.defs,
            &self.source,
            width,
            &self.at,
            &mut self.notes,
            true,
        );
        for (at, block) in blocks.iter().enumerate() {
            let from = if at == 0 { self.given } else { 0 };
            let gap = block.gap && !(at == 0 && self.gapped);
            self.give(&block.rows[from.min(block.rows.len())..], gap, &mut out);
        }
        bullet(&mut out, first);
        *self = Self {
            at: std::mem::take(&mut self.at),
            ..Self::default()
        };
        out
    }
}

/// `source` drawn after `defs`, the definitions read before it: its
/// blocks' places, and its link definitions' spans, in `source`.
fn draw_defined(
    defs: &str,
    source: &str,
    width: usize,
    at: &Where,
    notes: &mut Notes,
    end: bool,
) -> (Vec<Block>, Vec<Range<usize>>) {
    if defs.is_empty() {
        return draw_after(source, 0, width, at, notes, end);
    }
    let skip = defs.len();
    let text = format!("{defs}{source}");
    let (mut blocks, links) = draw_after(&text, skip, width, at, notes, end);
    for block in &mut blocks {
        block.start -= skip;
        block.end -= skip;
        for item in &mut block.items {
            item.cut -= skip;
        }
        for marks in &mut block.across {
            *marks = marks.start - skip..marks.end - skip;
        }
        for offset in &mut block.offsets {
            *offset -= skip;
        }
    }
    let links = links
        .into_iter()
        .map(|span| span.start - skip..span.end - skip);
    (blocks, links.collect())
}

/// How many footnotes `text` points at, as `[^name]` with no `:` after it.
fn footnote_marks(text: &str) -> usize {
    (text.match_indices("[^"))
        .filter(|(at, _)| {
            let rest = &text[at + 2..];
            let name = rest.find(']').map(|end| &rest[..end]);
            name.is_some_and(|name| {
                !name.is_empty()
                    && !name.contains(char::is_whitespace)
                    && !rest[name.len() + 1..].starts_with(':')
            })
        })
        .count()
}

/// Whether a line starts with a letter, at most three spaces in: words
/// that start no block of their own.
fn starts_with_word(line: &str) -> bool {
    let words = line.trim_start_matches(' ');
    line.len() - words.len() <= 3 && words.chars().next().is_some_and(char::is_alphabetic)
}

/// The list item `source` starts with, after its blank lines, numbered
/// `number`.
fn renumber(source: &mut String, number: u64) {
    let line = source.len() - source.trim_start_matches(['\n', ' ', '\t']).len();
    let line = source[..line].rfind('\n').map_or(0, |at| at + 1);
    let digits = line + source[line..].len() - source[line..].trim_start_matches(' ').len();
    let end = digits + source[digits..].len()
        - source[digits..]
            .trim_start_matches(|c: char| c.is_ascii_digit())
            .len();
    if end > digits {
        source.replace_range(digits..end, &number.to_string());
    }
}

/// The first row of a reply wears its bullet, unless it is a heading's.
fn bullet(rows: &mut [Line<'static>], first: bool) {
    if let Some(row) = rows.first_mut()
        && first
        && !heading_row(row)
    {
        *row = relead(std::mem::take(row), &Line::from("• "));
    }
}

/// Whether a row is a heading's first: its level's marks after the lead.
fn heading_row(row: &Line) -> bool {
    let text: String = (row.spans.iter().take(3))
        .map(|span| span.content.as_ref())
        .collect();
    let marks = text.trim_start();
    marks.starts_with('#') && marks.trim_start_matches('#').starts_with(' ')
}

/// Markdown as every surface draws it: `text` at `width`, read where
/// `markdown` says, each row with the line of `text` its block starts on.
pub(crate) fn rows(markdown: &Markdown, text: &str, width: usize) -> Vec<(usize, Line<'static>)> {
    let mut rewrite = Rewrite::default();
    let mut source = String::with_capacity(text.len() + 16);
    // Where each line read starts.
    let mut starts: Vec<usize> = Vec::new();
    for line in text.lines() {
        starts.push(source.len());
        rewrite.line(line, &mut source);
    }
    let mut notes = markdown.notes.clone();
    let blocks = draw(&source, width, &markdown.at, &mut notes, true);
    let mut out = Vec::new();
    let line = |offset: usize| {
        starts
            .partition_point(|start| *start <= offset)
            .saturating_sub(1)
    };
    for block in blocks {
        if block.gap && !out.is_empty() && !block.rows.is_empty() {
            out.push((line(block.start), Line::from("  ")));
        }
        let offsets = block
            .offsets
            .into_iter()
            .chain(std::iter::repeat(block.start));
        out.extend(
            block
                .rows
                .into_iter()
                .zip(offsets)
                .map(|(row, at)| (line(at), row)),
        );
    }
    out
}

/// Inline marks in a line of words, as a paragraph reads them: what a
/// table's cell and a question's line are drawn with.
pub fn inline(text: &str, base: Style) -> Vec<Span<'static>> {
    let indent = &text[..text.len() - text.trim_start().len()];
    let words = text.trim_start();
    // Words read as words, never as a block's marks.
    let block = {
        let mut chars = words.chars();
        let first = chars.next();
        let second = chars.next();
        let digits = words.bytes().take_while(u8::is_ascii_digit).count();
        match first {
            Some('#' | '>' | '|' | '=') => true,
            Some('-' | '+' | '*') => second.is_none_or(|c| c == ' ' || c == first.unwrap()),
            Some('`' | '~') => words.starts_with("```") || words.starts_with("~~~"),
            Some(_) => {
                digits > 0
                    && matches!(words.as_bytes().get(digits), Some(b'.' | b')'))
                    && words.as_bytes().get(digits + 1).is_none_or(|b| *b == b' ')
            }
            None => false,
        }
    };
    let source = match block {
        true => format!("\\{}", parens(words)),
        false => parens(words).into_owned(),
    };
    let at = Where::default();
    let mut notes = Notes::default();
    let mut draw = Draw {
        source: &source,
        width: usize::MAX,
        at: &at,
        notes: &mut notes,
        end: true,
        rows: Vec::new(),
        offsets: Vec::new(),
        here: 0,
        starts: Vec::new(),
        holders: Vec::new(),
        quotes: 0,
        firsts: vec![true],
        last_end: 0,
        leaf: Some(Leaf::Words),
        lines: Vec::new(),
        spans: Vec::new(),
        styles: vec![base],
        links: Vec::new(),
        html: Vec::new(),
        image: None,
        blocks: Vec::new(),
        top: None,
        top_holds: false,
        top_note: false,
        top_paragraph: false,
        across: Vec::new(),
        top_notes: 0,
        skip: 0,
        loose: None,
        items: Vec::new(),
    };
    if !indent.is_empty() {
        draw.spans.push(Span::styled(indent.to_owned(), base));
    }
    for (event, range) in Parser::new_ext(&source, options()).into_offset_iter() {
        match event {
            Event::Start(Tag::Paragraph) | Event::End(TagEnd::Paragraph) => {}
            // HTML that makes a block of its own: read as a line of it.
            Event::Html(_) | Event::Start(Tag::HtmlBlock) => {
                return html_words(text, base);
            }
            Event::HardBreak => draw.spans.push(Span::styled(" ", base)),
            event => draw.event(event, range),
        }
    }
    draw.spans
}

#[cfg(test)]
mod tests {
    use super::*;

    fn text(rows: &[Line]) -> Vec<String> {
        rows.iter().map(crate::transcript::seen).collect()
    }

    /// `source` drawn whole, as history and a file are.
    fn whole(markdown: &Markdown, source: &str, width: usize) -> Vec<String> {
        let rows: Vec<Line> = rows(markdown, source, width)
            .into_iter()
            .map(|(_, row)| row)
            .collect();
        text(&rows)
    }

    /// `source` streamed a line at a time, as a reply comes.
    fn streamed(mut markdown: Markdown, source: &str, width: usize) -> Vec<String> {
        let mut rows = Vec::new();
        for line in source.lines() {
            rows.extend(markdown.feed(line, width, false));
        }
        rows.extend(markdown.end(width, false));
        text(&rows)
    }

    fn styled<'a>(rows: &'a [Line<'static>], words: &str) -> &'a Span<'static> {
        (rows.iter().flat_map(|row| &row.spans))
            .find(|span| span.content == words)
            .unwrap_or_else(|| panic!("no {words:?} in {:?}", text(rows)))
    }

    #[test]
    fn markdown_reads_as_commonmark_does() {
        let reply = Markdown::default();
        let cases: &[(&str, &[&str])] = &[
            // Escapes and entities read as what they stand for.
            (
                "Not \\*italic\\*, \\`code\\` or 1\\. a list.",
                &["  Not *italic*, `code` or 1. a list."],
            ),
            (
                "Tom &amp; Jerry &lt;3 &copy; &mdash;",
                &["  Tom & Jerry <3 © —"],
            ),
            // Setext headings, and a closing `##`.
            ("Title\n=====", &["  # Title"]),
            ("## Title ##", &["  ## Title"]),
            // A code span with a backtick inside it.
            ("Use ``a ` b`` here.", &["  Use a ` b here."]),
            // `~~~` fences and indented code.
            ("~~~\nfn main() {}\n~~~", &["    fn main() {}"]),
            (
                "Run:\n\n    cargo build",
                &["  Run:", "  ", "    cargo build"],
            ),
            // Autolinks and reference links.
            (
                "See <https://example.com/a>.",
                &["  See https://example.com/a."],
            ),
            (
                "Read [the docs][d].\n\n[d]: https://example.com/d",
                &["  Read the docs (https://example.com/d)."],
            ),
            // Footnotes, numbered as named.
            (
                "A claim.[^n]\n\n[^n]: The source.",
                &["  A claim.[1]", "  ", "  [1] The source."],
            ),
            // Hard breaks, and bold italic.
            ("one  \ntwo\\\nthree", &["  one", "  two", "  three"]),
            ("***both***", &["  both"]),
            // Lists: `)` ordered, nested, tasks.
            ("1) one\n2) two", &["  1. one", "  2. two"]),
            (
                "- a\n  - b\n- [x] done\n- [ ] not",
                &["  - a", "    - b", "  ☑ done", "  ☐ not"],
            ),
            // A table's escaped pipe stays in its cell.
            ("| a | b |\n|---|---|\n| x \\| y | z |", &[]),
        ];
        for (source, rows) in cases {
            if rows.is_empty() {
                continue;
            }
            assert_eq!(whole(&reply, source, 40), *rows, "{source:?}");
        }
        let table = whole(&reply, "| a | b |\n|---|---|\n| x \\| y | z |", 40);
        assert!(table.iter().any(|row| row.contains("x | y")), "{table:?}");
        // Bold italic is both.
        let rows: Vec<Line> = rows(&reply, "***both***", 40)
            .into_iter()
            .map(|(_, r)| r)
            .collect();
        let both = styled(&rows, "both").style.add_modifier;
        assert!(both.contains(Modifier::BOLD) && both.contains(Modifier::ITALIC));
        // Emphasis over a line break still closes.
        let rows: Vec<Line> = (rows_of(&reply, "Some **bold\ntext** here."))
            .into_iter()
            .collect();
        assert!(
            styled(&rows, "text")
                .style
                .add_modifier
                .contains(Modifier::BOLD)
        );
    }

    fn rows_of(markdown: &Markdown, source: &str) -> Vec<Line<'static>> {
        rows(markdown, source, 40)
            .into_iter()
            .map(|(_, r)| r)
            .collect()
    }

    #[test]
    fn a_reply_keeps_its_line_breaks_and_a_document_joins_them() {
        let wrapped =
            "This paragraph was written by a tool that\nwraps its lines at a fixed column.";
        // A reply, a message or a prompt keeps the person's line breaks.
        assert_eq!(
            whole(&Markdown::default(), wrapped, 60),
            [
                "  This paragraph was written by a tool that",
                "  wraps its lines at a fixed column."
            ]
        );
        // A file reads as one paragraph, wrapped at the pane's width.
        let document = Markdown::document(std::path::Path::new("."));
        assert_eq!(
            whole(&document, wrapped, 40),
            [
                "  This paragraph was written by a tool ",
                "  that wraps its lines at a fixed ",
                "  column."
            ]
        );
    }

    #[test]
    fn blank_lines_between_blocks_are_kept_as_written() {
        let markdown = Markdown::default();
        assert_eq!(
            whole(&markdown, "Intro\n- a\n- b\n\nAfter\n\n\n## Head", 40),
            [
                "  Intro",
                "  - a",
                "  - b",
                "  ",
                "  After",
                "  ",
                "  ## Head"
            ]
        );
        // A loose list keeps a blank row between its items.
        assert_eq!(
            whole(&markdown, "- a\n\n- b\n  more\n\n  again", 40),
            ["  - a", "  ", "  - b", "    more", "  ", "    again"]
        );
    }

    #[test]
    fn a_reply_streamed_reads_as_it_does_whole() {
        let replies = [
            "Hi `x`\n## Head\n```rust\nlet a = 1;\nlet b = 2;\n```\n- item\n- [x] done",
            "Intro.\n\n| a | b |\n|---|---|\n| 1 | 2 |\n| 333 | 4 |\n\nAfter the table.",
            "> quoted\n> more\n>\n> > nested\n\n1. one\n2. two\n\n   still two\n\nEnd.",
            "Math: $x^2$ and\n\n$$\ny = 1\n$$\n\n\\[ z \\]\n\nso \\(a\\) holds.",
            "<p align=\"center\">\n  <b>Agent</b>\n</p>\n\nText with <kbd>K</kbd>.\n\n---\n\n[^1]: note\n\nA[^1]",
            "```diff\n-a\n+b\n```\nafter\n```\nopen fence\nnever closed",
            "Setext\n======\n\nPara\nover lines\n\n    indented\n    code",
            "[docs]: https://example.com\n\nSee [the docs][docs].\n\n> [q]: /q\n\n[q] and [docs].",
            "1. a\n1. b\n1. c\n\n   more c\n1. d\n\nAfter.",
            "[x]: /x\n- a\n\n- b [x][x]\n- c\n  lazy\n  - nested\n\n    deep\n- [ ] task\n\nEnd.",
            "7) seven\n8) eight\n- other list\n- again\n\n  10. inner\n  11. inner",
            "Words\n2. not a list\n    not code\nword *em\nphasis* and `co\nde` end\nmore words\nlast",
            "- a\n\n- b [x][x]\n- c\n\n```\ncode\n```\n\n[x]: /x\n\nEnd.",
            "A note.[^n] And `[^code]`.\n\nB\n\n[^n]: The note.\n\nC [^gone]",
        ];
        for reply in replies {
            for markdown in [
                Markdown::default(),
                Markdown::document(std::path::Path::new(".")),
            ] {
                assert_eq!(
                    streamed(markdown.clone(), reply, 40),
                    whole(&markdown, reply, 40),
                    "{reply:?}"
                );
            }
        }
    }

    #[test]
    fn rows_go_out_as_soon_as_they_will_not_change() {
        let mut markdown = Markdown::default();
        // A paragraph's line is held while it may still go on.
        assert!(markdown.feed("A line.", 40, true).is_empty());
        assert_eq!(text(&markdown.open("", 40, true)), ["• A line."]);
        // A blank line leaves it as it was: it goes out, with its bullet.
        assert_eq!(text(&markdown.feed("", 40, true)), ["• A line."]);
        // A fence's lines go out as they come.
        assert!(markdown.feed("```rust", 40, false).is_empty());
        assert_eq!(
            text(&markdown.feed("let a = 1;", 40, false)),
            ["  ", "    let a = 1;"]
        );
        assert_eq!(
            text(&markdown.feed("let b = 2;", 40, false)),
            ["    let b = 2;"]
        );
        assert!(markdown.feed("```", 40, false).is_empty());
        // A list's items go out once the next one comes.
        assert!(markdown.feed("- one", 40, false).is_empty());
        assert_eq!(text(&markdown.feed("- two", 40, false)), ["  - one"]);
        // A table is held until it ends: a later row can widen its columns.
        markdown.feed("", 40, false);
        assert!(markdown.feed("| a | b |", 40, false).is_empty());
        assert!(markdown.feed("|---|---|", 40, false).is_empty());
        assert!(markdown.feed("| 1 | 2 |", 40, false).is_empty());
        let open = text(&markdown.open("", 40, false));
        assert!(open.iter().any(|row| row.contains("│ 1")), "{open:?}");
        let rest = text(&markdown.end(40, false));
        assert!(rest.iter().any(|row| row.contains("│ 1")), "{rest:?}");
    }

    #[test]
    fn the_line_still_coming_shows_its_marks_closed() {
        let markdown = Markdown::default();
        let rows = markdown.open("Streaming **bold that has not clo", 60, false);
        assert!(
            styled(&rows, "clo")
                .style
                .add_modifier
                .contains(Modifier::BOLD)
        );
        let rows = markdown.open("Run `cargo te", 60, false);
        assert_eq!(styled(&rows, "te").style.fg, Some(Color::Cyan));
        let rows = markdown.open("See [the docs](https://exa", 60, false);
        assert!(
            styled(&rows, "docs")
                .style
                .add_modifier
                .contains(Modifier::UNDERLINED)
        );
        // Marks that are not emphasis stay as written.
        assert_eq!(closed("2 * 3 and snake_case"), "2 * 3 and snake_case");
        assert_eq!(closed("- item"), "- item");
        assert_eq!(closed("**done** and *more"), "**done** and *more*");
    }

    #[test]
    fn a_long_fence_streamed_costs_its_length_once() {
        let time = |lines: usize| {
            let mut markdown = Markdown::default();
            let started = std::time::Instant::now();
            markdown.feed("```", 80, false);
            for at in 0..lines {
                markdown.feed(&format!("line {at} of the code"), 80, false);
            }
            markdown.feed("```", 80, false);
            started.elapsed()
        };
        let (mut once, mut twice) = (std::time::Duration::MAX, std::time::Duration::MAX);
        for _ in 0..5 {
            once = once.min(time(4_000));
            twice = twice.min(time(8_000));
            if twice < once * 3 + std::time::Duration::from_millis(50) {
                return;
            }
        }
        panic!("{once:?} then {twice:?}");
    }

    #[test]
    fn a_long_paragraph_streamed_costs_its_length_once() {
        let time = |lines: usize| {
            let mut markdown = Markdown::default();
            let started = std::time::Instant::now();
            for at in 0..lines {
                markdown.open(&format!("line {at} of the"), 80, false);
                markdown.feed(&format!("line {at} of the `long` paragraph"), 80, false);
            }
            markdown.end(80, false);
            started.elapsed()
        };
        let (mut once, mut twice) = (std::time::Duration::MAX, std::time::Duration::MAX);
        for _ in 0..5 {
            once = once.min(time(2_000));
            twice = twice.min(time(4_000));
            if twice < once * 3 + std::time::Duration::from_millis(50) {
                return;
            }
        }
        panic!("{once:?} then {twice:?}");
    }

    #[test]
    fn a_long_list_streamed_costs_its_length_once() {
        let time = |items: usize| {
            let mut markdown = Markdown::default();
            let started = std::time::Instant::now();
            for at in 0..items {
                markdown.open(&format!("1. item {at} of the"), 80, false);
                markdown.feed(&format!("1. item {at} of the **list**"), 80, false);
                markdown.open("   its words go on", 80, false);
                markdown.feed("   its words go on", 80, false);
            }
            markdown.end(80, false);
            started.elapsed()
        };
        let (mut once, mut twice) = (std::time::Duration::MAX, std::time::Duration::MAX);
        for _ in 0..5 {
            once = once.min(time(2_000));
            twice = twice.min(time(4_000));
            if twice < once * 3 + std::time::Duration::from_millis(50) {
                return;
            }
        }
        panic!("{once:?} then {twice:?}");
    }
}
