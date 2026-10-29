//! The pane beside the conversation, where what the
//! person opens to look at goes instead of over the conversation: a file
//! they clicked, as a preview, `/status`, or a side chat. Its rows are a [`Screen`] of
//! their own, so they scroll, select, copy and follow links as the
//! conversation's do.
use std::path::{Path, PathBuf};

use ratatui::{
    buffer::Buffer,
    layout::Rect,
    style::{Color, Modifier, Style, Stylize},
    text::{Line, Span},
};
use unicode_width::UnicodeWidthStr;

use crate::screen::Screen;

/// The narrowest the conversation and the pane are side by side; a
/// window narrower than both shows one of them at a time.
pub const NARROWEST: u16 = 50;
/// The most of a file read for its preview, and the most lines shown.
const MOST_BYTES: u64 = 4 << 20;
const MOST_LINES: usize = 9_000;
/// Rows kept above a line a link pointed at, so it reads in its place.
const ABOVE: usize = 3;

/// How wide the pane is in a window `width` columns wide, beside a
/// conversation written at most `prose` wide: half the window, or more
/// when the conversation has all it is written at beside it; and all of
/// a window too narrow for both, where it is shown while keys go to it.
pub fn width(width: u16, prose: u16) -> u16 {
    if width < 2 * NARROWEST + 1 {
        return width;
    }
    let rest = width - 1;
    (rest / 2).max(rest.saturating_sub(prose))
}

pub struct Pane {
    /// What the pane shows, as its first row says it.
    title: String,
    /// The file's address, as its link held it, opened outside Sonny
    /// when the title is clicked.
    link: Option<String>,
    /// The folder the file is in, which paths in it are read from.
    folder: Option<PathBuf>,
    pub screen: Screen,
    /// Where it was drawn last: its title row and its rows.
    pub area: Rect,
    /// A card's lines as last set, drawn anew from the app's own state
    /// (`/status`), rather than a file read once.
    card: Option<Vec<Line<'static>>>,
    /// What the keys do here, after the title while they go to it.
    keys: &'static str,
    /// A program drawn here instead of rows: a side chat's Sonny.
    pub program: Option<crate::embed::Embedded>,
    /// A line kept on the last row under a card, however it is scrolled:
    /// `/background`'s keys, or what its last key did.
    pub foot: Option<Line<'static>>,
    /// Its rows are prose, read at the width the conversation is written
    /// at however wide the pane: Markdown and plain text. Code, pictures,
    /// diagrams and pages go as wide as the pane.
    prose: bool,
    /// What the pane showed before, as a browser keeps it, the latest
    /// last, each as it was left; and what it went back from.
    back: Vec<Pane>,
    forward: Vec<Pane>,
    /// A folder, drawn as a tree its folders open and close in.
    tree: Option<crate::tree::Tree>,
    width: usize,
}

impl Pane {
    /// A file's preview, from a `file://` address as a link holds it,
    /// with a `#L…` line to show, at `width`. Paths are shown from
    /// `workspace` when they are in it.
    pub fn preview(url: &str, workspace: &Path, width: usize) -> Result<Self, String> {
        let (path, line) = file_url(url).ok_or_else(|| format!("{url} is no file"))?;
        // `docs/../a.md`, as a link between files may name it, is `a.md`.
        let path = path.components().fold(PathBuf::new(), |mut path, part| {
            match part {
                std::path::Component::ParentDir => _ = path.pop(),
                std::path::Component::CurDir => {}
                part => path.push(part),
            }
            path
        });
        let title = match path.strip_prefix(workspace) {
            Ok(inside) if !inside.as_os_str().is_empty() => inside.display().to_string(),
            _ => path.display().to_string(),
        };
        let mut screen = Screen::default();
        let tree = std::fs::metadata(&path)
            .is_ok_and(|meta| meta.is_dir())
            .then(|| crate::tree::Tree::new(&path));
        // A tree reads at the conversation's width, its marks near its names.
        let width = match tree {
            Some(_) => width.min(crate::PROSE),
            None => width,
        };
        let (target, prose) = match &tree {
            Some(tree) => {
                for (line, url) in tree.lines(width) {
                    list(&mut screen, line, url);
                }
                (0, true)
            }
            None => preview(&path, width, line, &mut screen)?,
        };
        screen.show_row(target, ABOVE);
        // Its escapes kept, as the path read back may hold `#` or `%`;
        // the line left off.
        let link = url.split('#').next().unwrap_or(url);
        Ok(Self {
            title,
            link: Some(link.to_owned()),
            folder: match path.is_dir() {
                true => Some(path.clone()),
                false => path.parent().map(Path::to_path_buf),
            },
            screen,
            area: Rect::default(),
            card: None,
            keys: "esc closes",
            program: None,
            foot: None,
            prose,
            back: Vec::new(),
            forward: Vec::new(),
            tree,
            width,
        })
    }

    /// Shown in place of `before`, after it as a browser goes: what was
    /// gone back from left behind.
    pub fn after(mut self, mut before: Pane) -> Self {
        self.back = std::mem::take(&mut before.back);
        before.forward.clear();
        self.back.push(before);
        if self.back.len() > HISTORY {
            self.back.remove(0);
        }
        self
    }

    /// Whether there is something to go back (`back`) or forward to.
    pub fn can_go(&self, back: bool) -> bool {
        match back {
            true => !self.back.is_empty(),
            false => !self.forward.is_empty(),
        }
    }

    /// What the pane showed before (`back`), or went back from, as it was
    /// left, this kept to come back to; or this again, with nowhere to go.
    pub fn go(mut self, back: bool) -> Self {
        let Some(mut next) = (match back {
            true => self.back.pop(),
            false => self.forward.pop(),
        }) else {
            return self;
        };
        next.back = std::mem::take(&mut self.back);
        next.forward = std::mem::take(&mut self.forward);
        match back {
            true => next.forward.push(self),
            false => next.back.push(self),
        }
        next
    }

    /// A folder in its tree opened or closed, if `url` names one; the
    /// view kept where it was.
    pub fn toggle(&mut self, url: &str) -> bool {
        let Some(tree) = &mut self.tree else {
            return false;
        };
        let Some((path, _)) = file_url(url) else {
            return false;
        };
        if !tree.toggle(&path) {
            return false;
        }
        // A tree reads at the conversation's width, as it is drawn.
        let width = (self.area.width as usize).min(crate::PROSE);
        let back = self.screen.back();
        let before = self.screen.height(width);
        self.screen = Screen::default();
        for (line, url) in tree.lines(self.width) {
            list(&mut self.screen, line, url);
        }
        // Rows open and close under the view: its top stays put.
        let after = self.screen.height(width);
        self.screen.hold((back + after).saturating_sub(before));
        true
    }

    /// Whether `(column, row)` is on the title's ‹ (true) or › (false),
    /// shown while there is somewhere to go back or forward to.
    pub fn on_arrow(&self, column: u16, row: u16) -> Option<bool> {
        if row != self.area.y || (self.back.is_empty() && self.forward.is_empty()) {
            return None;
        }
        match column.checked_sub(self.area.x)? {
            1 => Some(true),
            3 => Some(false),
            _ => None,
        }
    }

    /// A card titled `title` whose lines the app sets as they change,
    /// as `/status` counts on; `keys` says what the keys do on it.
    pub fn card(title: &str, keys: &'static str) -> Self {
        Self {
            title: title.to_owned(),
            link: None,
            folder: None,
            screen: Screen::default(),
            area: Rect::default(),
            card: Some(Vec::new()),
            keys,
            program: None,
            foot: None,
            prose: false,
            back: Vec::new(),
            forward: Vec::new(),
            tree: None,
            width: 0,
        }
    }

    /// `program` drawn in the pane under `title`, every key going to it
    /// but Ctrl+\, which goes back to the conversation.
    pub fn program(title: &str, program: crate::embed::Embedded) -> Self {
        Self {
            title: title.to_owned(),
            link: None,
            folder: None,
            screen: Screen::default(),
            area: Rect::default(),
            card: None,
            keys: "ctrl+\\ chat",
            program: Some(program),
            foot: None,
            prose: false,
            back: Vec::new(),
            forward: Vec::new(),
            tree: None,
            width: 0,
        }
    }

    /// Where what the pane holds starts, under its title: the program's
    /// first cell.
    pub fn origin(&self) -> (u16, u16) {
        (self.area.x + 1, self.area.y + 2)
    }

    /// Whether this is a card, not a file.
    pub fn is_card(&self) -> bool {
        self.card.is_some()
    }

    /// A card's lines, put in only when they changed, so a selection in
    /// them lasts while they do not, and the view stays where it was.
    pub fn set_card(&mut self, lines: Vec<Line<'static>>, keys: &'static str) {
        self.keys = keys;
        if self.card.as_ref() != Some(&lines) {
            self.screen.replace(lines.clone());
            self.card = Some(lines);
        }
    }

    /// The folder of the file shown, if a file is.
    pub fn folder(&self) -> Option<&Path> {
        self.folder.as_deref()
    }

    /// The file's address, to open it outside, if it shows one.
    pub fn url(&self) -> Option<String> {
        self.link.clone()
    }

    /// Whether `(column, row)` is on the title, which opens the file.
    pub fn on_title(&self, column: u16, row: u16) -> bool {
        row == self.area.y && (self.area.x..self.area.right()).contains(&column)
    }

    /// The pane in `area`: its title over its rows, the title bright
    /// while keys go to the pane. `file` is where a word that may be a
    /// path opens, as in the conversation. Where a program's cursor is,
    /// when it shows one.
    pub fn render(
        &mut self,
        area: Rect,
        buf: &mut Buffer,
        focused: bool,
        file: impl Fn(&str) -> Option<String>,
    ) -> Option<(u16, u16)> {
        self.area = area;
        if area.height == 0 {
            return None;
        }
        let keys = match focused {
            true if !self.back.is_empty() => format!("  ← back  {} ", self.keys),
            true => format!("  {} ", self.keys),
            false => "  tab ".to_owned(),
        };
        let mut style = Style::new();
        // A file's name is a link to it.
        if self.link.is_some() {
            style = style.add_modifier(Modifier::UNDERLINED);
        }
        if !focused {
            style = style.add_modifier(Modifier::DIM);
        } else {
            style = style.add_modifier(Modifier::BOLD);
        }
        // ‹ › go back and forward, while there is somewhere to go.
        let mut head = vec![Span::raw(" ")];
        if !self.back.is_empty() || !self.forward.is_empty() {
            let arrow = |text, there: bool| match there {
                true => Span::raw(text),
                false => Span::raw(text).dark_gray(),
            };
            head.push(arrow("‹", !self.back.is_empty()));
            head.push(Span::raw(" "));
            head.push(arrow("›", !self.forward.is_empty()));
            head.push(Span::raw(" "));
        }
        let used: usize = head.iter().map(|s| s.content.width()).sum();
        let room = (area.width as usize).saturating_sub(used + keys.width());
        let title = crate::transcript::cut(&self.title, room);
        let gap = (area.width as usize).saturating_sub(used + title.width() + keys.width());
        head.push(Span::styled(title, style));
        head.push(Span::raw(" ".repeat(gap)));
        head.push(Span::raw(keys).dim());
        let head = Line::from(head);
        buf.set_line(area.x, area.y, &head, area.width);
        // A row between the name and what it holds, and above the foot.
        let mut rows = Rect {
            y: area.y + 2,
            height: area.height.saturating_sub(2),
            ..area
        };
        if let Some(foot) = &self.foot
            && rows.height > 2
        {
            rows.height -= 2;
            buf.set_line(area.x, rows.bottom() + 1, foot, area.width);
        }
        match &mut self.program {
            // A column off the line between, as the title is.
            Some(program) => {
                let rows = Rect {
                    x: rows.x + 1,
                    width: rows.width.saturating_sub(1),
                    ..rows
                };
                program.render(rows, buf)
            }
            None => {
                // Prose reads at the width the conversation is written at.
                let rows = match self.prose {
                    true => Rect {
                        width: rows.width.min(crate::PROSE as u16),
                        ..rows
                    },
                    false => rows,
                };
                self.screen.render(rows, buf, file);
                None
            }
        }
    }
}

/// The path and line a `file://` address names, its `%XX` escapes read.
pub fn file_url(url: &str) -> Option<(PathBuf, Option<usize>)> {
    let rest = url.strip_prefix("file://")?;
    let (path, line) = match rest.split_once("#L") {
        Some((path, line)) => (path, line.parse().ok()),
        None => (rest.split('#').next().unwrap_or(rest), None),
    };
    let bytes = path.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut at = 0;
    while at < bytes.len() {
        let hex = bytes
            .get(at + 1..at + 3)
            .and_then(|h| std::str::from_utf8(h).ok());
        match (bytes[at], hex.and_then(|h| u8::from_str_radix(h, 16).ok())) {
            (b'%', Some(byte)) => {
                out.push(byte);
                at += 3;
            }
            (byte, _) => {
                out.push(byte);
                at += 1;
            }
        }
    }
    Some((PathBuf::from(String::from_utf8(out).ok()?), line))
}

/// `path` written into `screen` at `width`: a picture drawn, Markdown
/// as replies read, code in its colors with its line numbers. The row to show first: that of `line`, if any; and
/// whether its rows are prose, to be read no wider than the conversation.
fn preview(
    path: &Path,
    width: usize,
    line: Option<usize>,
    screen: &mut Screen,
) -> Result<(usize, bool), String> {
    let meta = std::fs::metadata(path).map_err(|e| format!("{}: {e}", path.display()))?;
    // A pipe, socket or device may never end, or never start.
    if !meta.is_file() {
        screen.push(vec![note(
            "not a file a preview reads: a pipe, socket or device",
        )]);
        return Ok((0, false));
    }
    let size = meta.len();
    let extension = (path.extension().and_then(|e| e.to_str()))
        .unwrap_or_default()
        .to_lowercase();
    if matches!(extension.as_str(), "png" | "jpg" | "jpeg" | "gif" | "webp") {
        let shown = match crate::term::graphics() {
            true => crate::image::file(path, width, crate::image::tall())
                .and_then(|drawn| crate::image::show(&drawn)),
            false => Err("this terminal draws no pictures (Kitty and Ghostty do)".into()),
        };
        match shown {
            Ok(rows) => screen.push(rows),
            Err(error) => screen.push(vec![note(&format!(
                "a picture, {}: {error}; click its name to open it",
                bytes_said(size)
            ))]),
        }
        return Ok((0, false));
    }
    let mut bytes = Vec::new();
    use std::io::Read;
    std::fs::File::open(path)
        .and_then(|f| f.take(MOST_BYTES).read_to_end(&mut bytes))
        .map_err(|e| format!("{}: {e}", path.display()))?;
    let text = match String::from_utf8(bytes) {
        Ok(text) if !text.contains('\0') => text,
        // Cut in the middle of a character at the end of what was read.
        Err(error) if size > MOST_BYTES && error.utf8_error().error_len().is_none() => {
            let valid = error.utf8_error().valid_up_to();
            let mut bytes = error.into_bytes();
            bytes.truncate(valid);
            String::from_utf8(bytes).unwrap_or_default()
        }
        _ => {
            screen.push(vec![note(&format!(
                "a binary file, {}; click its name to open it",
                bytes_said(size)
            ))]);
            return Ok((0, false));
        }
    };
    // Drawn only where pictures can be, as tall as the pane; elsewhere,
    // or when it cannot be drawn, its source, and why.
    if line.is_none() && crate::term::graphics() {
        let drawn =
            match extension.as_str() {
                "svg" => Some(crate::image::render_in(
                    &text,
                    width,
                    crate::image::tall(),
                    true,
                )),
                "mmd" | "mermaid" => Some(crate::mermaid::svg(&text).and_then(|svg| {
                    crate::image::render_in(&svg, width, crate::image::tall(), true)
                })),
                "html" | "htm" => Some(
                    crate::html::render(path, crate::image::page_width(width))
                        .and_then(|png| crate::image::page_in(&png, width, crate::image::tall())),
                ),
                _ => None,
            };
        match drawn.map(|drawn| drawn.and_then(|drawn| crate::image::show(&drawn))) {
            Some(Ok(rows)) => {
                screen.push(rows);
                return Ok((0, false));
            }
            Some(Err(error)) => screen.push(vec![
                note(&format!("could not draw it: {error}")),
                Line::default(),
            ]),
            None => {}
        }
    }
    let lines: Vec<&str> = text.lines().collect();
    let shown = lines.len().min(MOST_LINES);
    let target = line.map(|line| line.saturating_sub(1).min(shown.saturating_sub(1)));
    let mut first_row = 0;
    let prose;
    if matches!(extension.as_str(), "md" | "markdown" | "mdx") && line.is_none() {
        let folder = path.parent().unwrap_or(Path::new("/"));
        // A document reads at the conversation's width, its pictures too.
        prose = true;
        let mut markdown = crate::transcript::Markdown::document(folder);
        let leads = (Line::from("  "), Line::from("  "));
        let text = lines[..shown].join("\n");
        let rows =
            crate::transcript::markdown(&mut markdown, &text, width.min(crate::PROSE), leads);
        screen.push(rows);
    } else {
        let mut code = crate::highlight::Code::file(path, lines.first().unwrap_or(&""));
        // Text no language colors is read as prose.
        prose = code.is_none();
        let digits = shown.max(1).to_string().len();
        for (at, text) in lines[..shown].iter().enumerate() {
            let text = text.replace('\t', "    ");
            let number = format!("{:>digits$}  ", at + 1);
            let number = match Some(at) == target {
                true => Span::raw(number).fg(Color::Yellow).bold(),
                false => Span::raw(number).dim(),
            };
            let mut spans = vec![Span::raw(" "), number];
            match &mut code {
                Some(code) => spans.extend(code.line(&text)),
                None => spans.push(Span::raw(text)),
            }
            let rest = Line::from(" ".repeat(digits + 3));
            screen.push_hanging(Line::from(spans), rest);
        }
        first_row = target.unwrap_or(0);
    }
    if lines.len() > shown || size > MOST_BYTES {
        screen.push(vec![
            Line::default(),
            note("… the rest is past what a preview shows; click its name to open it"),
        ]);
    }
    Ok((first_row, prose))
}

/// The most the pane keeps to go back to.
const HISTORY: usize = 50;

/// A row of a list that opens `url` when there is one.
fn list(screen: &mut Screen, line: Line<'static>, url: Option<String>) {
    match url {
        Some(url) => screen.push_linked(line, url),
        None => screen.push(vec![line]),
    }
}

fn note(text: &str) -> Line<'static> {
    Line::from(format!(" {text}")).dim()
}

fn bytes_said(size: u64) -> String {
    match size {
        0..1024 => format!("{size} bytes"),
        1024..1_048_576 => format!("{} KB", size / 1024),
        _ => format!("{:.1} MB", size as f64 / 1_048_576.0),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rows(pane: &mut Pane, width: u16, height: u16) -> Vec<String> {
        let area = Rect::new(0, 0, width, height);
        let mut buf = Buffer::empty(area);
        pane.render(area, &mut buf, true, |_| None);
        (0..height)
            .map(|y| {
                (0..width)
                    .map(|x| buf[(x, y)].symbol())
                    .collect::<String>()
                    .trim_end()
                    .to_owned()
            })
            .collect()
    }

    #[test]
    fn a_cards_foot_stays_on_the_last_row_however_it_is_scrolled() {
        let mut pane = Pane::card("/background", "esc closes");
        let lines = (0..20).map(|i| Line::from(format!("row {i}"))).collect();
        pane.set_card(lines, "esc closes");
        pane.foot = Some(Line::from("d again removes nightly"));
        let shown = rows(&mut pane, 30, 8);
        assert_eq!(shown[0], " /background       esc closes");
        assert_eq!(shown[2], "row 0");
        assert_eq!(shown[5], "row 3");
        assert_eq!(shown[6], "");
        assert_eq!(shown[7], "d again removes nightly");
        pane.screen.latest();
        let shown = rows(&mut pane, 30, 8);
        assert_eq!(shown[5], "row 19");
        assert_eq!(shown[7], "d again removes nightly");
    }

    #[test]
    fn the_pane_takes_half_or_what_the_conversation_leaves() {
        // Half, a column kept for the line between.
        assert_eq!(width(161, 100), 80);
        // A wide window: the conversation keeps its 100 columns.
        assert_eq!(width(260, 100), 159);
        // Too narrow for both: all of it.
        assert_eq!(width(100, 100), 100);
        assert_eq!(width(101, 100), 50);
    }

    #[test]
    fn an_address_reads_back_as_its_path_and_line() {
        let (path, line) = file_url("file:///w/a%20b.rs#L12").unwrap();
        assert_eq!(path, PathBuf::from("/w/a b.rs"));
        assert_eq!(line, Some(12));
        assert_eq!(file_url("file:///w/c.rs").unwrap().1, None);
        // The title opens the file by the address as the link wrote it.
        let dir = std::env::temp_dir().join(format!("sonny-url-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("a#1%20.rs"), "x\n").unwrap();
        let url = format!("file://{}/a%231%2520.rs", dir.display());
        let pane = Pane::preview(&format!("{url}#L1"), &dir, 40).unwrap();
        assert_eq!(pane.url(), Some(url));
        std::fs::remove_dir_all(&dir).unwrap();
        assert!(file_url("https://x").is_none());
    }

    #[test]
    fn code_shows_numbered_from_the_line_asked_for() {
        let dir = std::env::temp_dir().join(format!("sonny-pane-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let file = dir.join("a.rs");
        let source: String = (1..=40).map(|n| format!("let x{n} = {n};\n")).collect();
        std::fs::write(&file, source).unwrap();
        let url = format!("file://{}#L20", file.display());
        let mut pane = Pane::preview(&url, &dir, 40).unwrap();
        let shown = rows(&mut pane, 40, 7);
        assert!(shown[0].starts_with(" a.rs"), "{shown:?}");
        assert!(shown[0].ends_with("esc closes"), "{shown:?}");
        // Three rows above the line linked to, so it reads in place.
        assert_eq!(shown[2], " 17  let x17 = 17;");
        assert_eq!(shown[5], " 20  let x20 = 20;");
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn the_line_asked_for_shows_under_a_long_line_above() {
        let dir = std::env::temp_dir().join(format!("sonny-above-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let file = dir.join("a.txt");
        let long = "word ".repeat(40);
        let source: String = (1..=30)
            .map(|n| match n {
                19 => format!("{long}\n"),
                n => format!("line {n}\n"),
            })
            .collect();
        std::fs::write(&file, source).unwrap();
        let url = format!("file://{}#L20", file.display());
        let mut pane = Pane::preview(&url, &dir, 30).unwrap();
        let shown = rows(&mut pane, 30, 8);
        // Three drawn rows above it, however many lines they are.
        assert_eq!(shown[5], " 20  line 20", "{shown:?}");
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn a_pipe_is_not_read() {
        let dir = std::env::temp_dir().join(format!("sonny-fifo-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let fifo = dir.join("pipe");
        let made = std::process::Command::new("mkfifo").arg(&fifo).status();
        if made.is_ok_and(|s| s.success()) {
            // Opening it would wait for a writer that never comes.
            let mut pane = Pane::preview(&format!("file://{}", fifo.display()), &dir, 60).unwrap();
            let shown = rows(&mut pane, 60, 3);
            assert!(shown[2].contains("not a file a preview reads"), "{shown:?}");
        }
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn a_folders_entries_open_whatever_their_names() {
        let dir = std::env::temp_dir().join(format!("sonny-dir-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("a b.rs"), "x\n").unwrap();
        let mut pane = Pane::preview(&format!("file://{}", dir.display()), &dir, 80).unwrap();
        let area = Rect::new(0, 0, 80, 4);
        let mut buf = Buffer::empty(area);
        pane.render(area, &mut buf, true, |_| None);
        use ratatui::crossterm::event::{KeyModifiers, MouseButton, MouseEvent, MouseEventKind};
        let ev = |kind| MouseEvent {
            kind,
            column: 117,
            row: 4,
            modifiers: KeyModifiers::NONE,
        };
        println!(
            "down {:?}",
            pane.screen
                .mouse(ev(MouseEventKind::Down(MouseButton::Left)))
                .is_some()
        );
        println!(
            "up {:?}",
            pane.screen
                .mouse(ev(MouseEventKind::Up(MouseButton::Left)))
                .map(|a| match a {
                    crate::screen::Action::Open(u) => u,
                    _ => "copy".into(),
                })
        );
        let cells = pane.screen.link_cells(&buf);
        let url = format!("file://{}/a%20b.rs", dir.display());
        // One link, over the whole name, spaces and all.
        assert!(cells.iter().all(|(_, y, _)| *y == 2));
        assert!(
            cells[0].2.symbol().contains(&url),
            "{:?}",
            cells[0].2.symbol()
        );
        let row: String = (0..80).map(|x| buf[(x, 2)].symbol()).collect();
        assert_eq!(cells.len(), row.trim().width());
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn the_pane_goes_back_and_forward_as_a_browser_does() {
        let dir = std::env::temp_dir().join(format!("sonny-back-{}", std::process::id()));
        std::fs::create_dir_all(dir.join("src")).unwrap();
        std::fs::write(dir.join("a.md"), "# A\n").unwrap();
        std::fs::write(dir.join("src/b.rs"), "fn b() {}\n").unwrap();
        let url = |name: &str| format!("file://{}", dir.join(name).display());
        let mut folder = Pane::preview(&url(""), &dir, 60).unwrap();
        // A folder opened in its tree stays open when it is come back to.
        assert!(folder.toggle(&url("src")));
        let a = Pane::preview(&url("a.md"), &dir, 60).unwrap().after(folder);
        let b = Pane::preview(&url("src/b.rs"), &dir, 60).unwrap().after(a);
        assert!(b.can_go(true) && !b.can_go(false));
        let mut shown = b.go(true).go(true);
        assert!(!shown.can_go(true) && shown.can_go(false));
        let shown_rows = rows(&mut shown, 60, 6);
        assert!(
            shown_rows[2..5].contains(&" ▾ src/".to_owned()),
            "{shown_rows:?}"
        );
        assert!(
            shown_rows[2..5].contains(&" │   b.rs".to_owned()),
            "{shown_rows:?}"
        );
        // The title's arrows: ‹ dim with nowhere back to go, › forward.
        assert!(shown_rows[0].starts_with(" ‹ › "), "{shown_rows:?}");
        assert_eq!(shown.on_arrow(1, 0), Some(true));
        assert_eq!(shown.on_arrow(3, 0), Some(false));
        assert_eq!(shown.on_arrow(5, 0), None);
        let mut a = shown.go(false);
        assert_eq!(a.title, "a.md");
        // Somewhere new from here: forward is gone, as a browser forgets it.
        let c = Pane::preview(&url("src/b.rs"), &dir, 60).unwrap().after(a);
        assert!(!c.can_go(false));
        a = c.go(true);
        assert_eq!(a.title, "a.md");
        assert!(rows(&mut a, 60, 3)[0].contains("← back"));
        // A pane with nowhere to go has no arrows.
        let mut card = Pane::card("/status", "esc closes");
        assert!(!rows(&mut card, 30, 2)[0].contains('‹'));
        assert_eq!(card.on_arrow(1, 0), None);
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn a_wide_name_is_written_again_a_character_at_a_time() {
        let dir = std::env::temp_dir().join(format!("sonny-wide-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("日本.rs"), "x\n").unwrap();
        let mut pane = Pane::preview(&format!("file://{}", dir.display()), &dir, 80).unwrap();
        let area = Rect::new(0, 0, 80, 4);
        let mut buf = Buffer::empty(area);
        pane.render(area, &mut buf, true, |_| None);
        use ratatui::crossterm::event::{KeyModifiers, MouseButton, MouseEvent, MouseEventKind};
        let ev = |kind| MouseEvent {
            kind,
            column: 117,
            row: 4,
            modifiers: KeyModifiers::NONE,
        };
        println!(
            "down {:?}",
            pane.screen
                .mouse(ev(MouseEventKind::Down(MouseButton::Left)))
                .is_some()
        );
        println!(
            "up {:?}",
            pane.screen
                .mouse(ev(MouseEventKind::Up(MouseButton::Left)))
                .map(|a| match a {
                    crate::screen::Action::Open(u) => u,
                    _ => "copy".into(),
                })
        );
        let cells = pane.screen.link_cells(&buf);
        // No cell a wide character goes on into is written over.
        for (x, y, _) in &cells {
            assert_ne!(buf[(*x, *y)].symbol(), " ", "column {x}");
        }
        let text: String = cells.iter().map(|(_, _, c)| c.symbol()).collect();
        assert!(text.contains("日") && text.contains("本"));
        assert!(text.ends_with("s\x1b]8;;\x1b\\"), "{text:?}");
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn a_long_code_line_wraps_under_its_code() {
        let dir = std::env::temp_dir().join(format!("sonny-wrap-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let file = dir.join("a.txt");
        std::fs::write(&file, "one two three four five six\n").unwrap();
        let mut pane = Pane::preview(&format!("file://{}", file.display()), &dir, 20).unwrap();
        let shown = rows(&mut pane, 20, 4);
        assert_eq!(shown[2], " 1  one two three");
        assert_eq!(shown[3], "    four five six");
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn a_file_reads_at_the_conversations_width_in_a_wide_pane() {
        let dir = std::env::temp_dir().join(format!("sonny-cap-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let file = dir.join("a.txt");
        std::fs::write(&file, format!("{}\n", "word ".repeat(60))).unwrap();
        let mut pane = Pane::preview(&format!("file://{}", file.display()), &dir, 100).unwrap();
        let shown = rows(&mut pane, 150, 5);
        // The title spans the pane; the file's rows stop at 100 columns.
        assert!(shown[0].ends_with("esc closes"), "{shown:?}");
        assert!(shown[2].width() <= crate::PROSE, "{}", shown[2].width());
        assert!(shown[3].starts_with("    word"), "{shown:?}");
        // Code goes as wide as the pane.
        let code = dir.join("a.rs");
        std::fs::write(&code, format!("let x = \"{}\";\n", "w".repeat(120))).unwrap();
        let mut pane = Pane::preview(&format!("file://{}", code.display()), &dir, 150).unwrap();
        assert!(rows(&mut pane, 150, 4)[2].width() > crate::PROSE);
        // Markdown is prose.
        let doc = dir.join("a.md");
        std::fs::write(&doc, format!("{}\n", "word ".repeat(40))).unwrap();
        let mut pane = Pane::preview(&format!("file://{}", doc.display()), &dir, 150).unwrap();
        let shown = rows(&mut pane, 150, 4);
        assert!(shown[2].width() <= crate::PROSE, "{shown:?}");
        // A card is not a file, and keeps the pane's width.
        let mut card = Pane::card("/status", "esc closes");
        card.set_card(vec![Line::from("x".repeat(140))], "esc closes");
        assert_eq!(rows(&mut card, 150, 3)[2].width(), 140);
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn a_picture_file_says_what_it_is_where_none_are_drawn() {
        let dir = std::env::temp_dir().join(format!("sonny-png-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let file = dir.join("shot.png");
        image::RgbaImage::new(4, 4).save(&file).unwrap();
        let mut pane = Pane::preview(&format!("file://{}", file.display()), &dir, 60).unwrap();
        let shown = rows(&mut pane, 60, 3);
        assert!(shown[2].starts_with(" a picture, "), "{shown:?}");
        assert!(shown[2].contains("draws no pictures"), "{shown:?}");
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn markdown_reads_as_a_reply_does() {
        let dir = std::env::temp_dir().join(format!("sonny-md-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let file = dir.join("README.md");
        std::fs::write(
            &file,
            "# Title\n\nSome `code`.\n\n```rust\nfn a() {}\n```\n",
        )
        .unwrap();
        let mut pane = Pane::preview(&format!("file://{}", file.display()), &dir, 30).unwrap();
        let shown = rows(&mut pane, 30, 8);
        assert_eq!(shown[2], "  Title");
        assert_eq!(shown[4], "  Some code.");
        assert_eq!(shown[6], "    fn a() {}");
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
