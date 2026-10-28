//! The pane beside the conversation on the full screen, where what the
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
}

impl Pane {
    /// A file's preview, from a `file://` address as a link holds it,
    /// with a `#L…` line to show, at `width`. Paths are shown from
    /// `workspace` when they are in it.
    pub fn preview(url: &str, workspace: &Path, width: usize) -> Result<Self, String> {
        let (path, line) = file_url(url).ok_or_else(|| format!("{url} is no file"))?;
        let title = match path.strip_prefix(workspace) {
            Ok(inside) if !inside.as_os_str().is_empty() => inside.display().to_string(),
            _ => path.display().to_string(),
        };
        let mut screen = Screen::default();
        let target = preview(&path, width, line, &mut screen)?;
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
        })
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
        }
    }

    /// `program` drawn in the pane under `title`, every key going to it
    /// but Shift+Tab, which goes back to the conversation.
    pub fn program(title: &str, program: crate::embed::Embedded) -> Self {
        Self {
            title: title.to_owned(),
            link: None,
            folder: None,
            screen: Screen::default(),
            area: Rect::default(),
            card: None,
            keys: "shift+tab back",
            program: Some(program),
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
            true => format!("  {} ", self.keys),
            false => "  tab ".to_owned(),
        };
        let room = (area.width as usize).saturating_sub(keys.width() + 1);
        let title = crate::transcript::cut(&self.title, room);
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
        let gap = (area.width as usize).saturating_sub(title.width() + keys.width() + 1);
        let head = Line::from(vec![
            Span::raw(" "),
            Span::styled(title, style),
            Span::raw(" ".repeat(gap)),
            Span::raw(keys).dim(),
        ]);
        buf.set_line(area.x, area.y, &head, area.width);
        // A row between the name and what it holds.
        let rows = Rect {
            y: area.y + 2,
            height: area.height.saturating_sub(2),
            ..area
        };
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

/// `path` written into `screen` at `width`: a folder's entries, a
/// picture drawn, Markdown as replies read, code in its colors with its
/// line numbers. The row to show first: that of `line`, if any.
fn preview(
    path: &Path,
    width: usize,
    line: Option<usize>,
    screen: &mut Screen,
) -> Result<usize, String> {
    let meta = std::fs::metadata(path).map_err(|e| format!("{}: {e}", path.display()))?;
    if meta.is_dir() {
        for (line, url) in folder(path)? {
            match url {
                Some(url) => screen.push_linked(line, url),
                None => screen.push(vec![line]),
            }
        }
        return Ok(0);
    }
    // A pipe, socket or device may never end, or never start.
    if !meta.is_file() {
        screen.push(vec![note(
            "not a file a preview reads: a pipe, socket or device",
        )]);
        return Ok(0);
    }
    let size = meta.len();
    let mut bytes = Vec::new();
    use std::io::Read;
    std::fs::File::open(path)
        .and_then(|f| f.take(MOST_BYTES).read_to_end(&mut bytes))
        .map_err(|e| format!("{}: {e}", path.display()))?;
    let extension = (path.extension().and_then(|e| e.to_str()))
        .unwrap_or_default()
        .to_lowercase();
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
            return Ok(0);
        }
    };
    // Drawn only where pictures can be; elsewhere, its source.
    if extension == "svg"
        && crate::term::graphics()
        && let Ok(rows) = crate::image::render(&text, width).and_then(|d| crate::image::show(&d))
    {
        screen.push(rows);
        return Ok(0);
    }
    let lines: Vec<&str> = text.lines().collect();
    let shown = lines.len().min(MOST_LINES);
    let target = line.map(|line| line.saturating_sub(1).min(shown.saturating_sub(1)));
    let mut first_row = 0;
    if matches!(extension.as_str(), "md" | "markdown" | "mdx") && line.is_none() {
        let mut markdown = crate::transcript::Markdown::default();
        let mut rows = Vec::new();
        for text in &lines[..shown] {
            rows.extend(markdown.feed(text, width, false));
        }
        rows.extend(markdown.end(width, false));
        screen.push(rows);
    } else {
        let mut code = crate::highlight::Code::file(path, lines.first().unwrap_or(&""));
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
    Ok(first_row)
}

/// A folder's entries, folders first, each with the address it opens.
fn folder(path: &Path) -> Result<Vec<(Line<'static>, Option<String>)>, String> {
    let mut entries: Vec<(bool, String)> = std::fs::read_dir(path)
        .map_err(|e| format!("{}: {e}", path.display()))?
        .filter_map(|entry| {
            let entry = entry.ok()?;
            let dir = entry.file_type().ok()?.is_dir();
            Some((!dir, entry.file_name().to_string_lossy().into_owned()))
        })
        .collect();
    entries.sort();
    if entries.is_empty() {
        return Ok(vec![(note("an empty folder"), None)]);
    }
    Ok(entries
        .into_iter()
        .map(|(file, name)| {
            let full = path.join(&name);
            let url = crate::links::file_url(&full);
            let full = full.display().to_string();
            let line = match file {
                true => Line::from(format!(" {full}")),
                false => Line::from(vec![Span::raw(format!(" {full}")), Span::raw("/").dim()]),
            };
            (line, url)
        })
        .collect())
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
