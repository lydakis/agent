//! What a piece of the conversation looks like: a prompt, a reply, a tool
//! call, each a list of lines already wrapped to a width. Markdown is
//! drawn here for every surface, the one way it is drawn: [`markdown`],
//! or [`Markdown`] for a reply as it streams, is what the conversation, a
//! file in the pane, a note in the margin and a card all read what was
//! written with, its pictures, diagrams, pages, math, tables and code
//! included; [`crate::screen::Screen`] shows the lines, scrolls, selects,
//! copies and opens their links, wherever they are.
use ratatui::{
    layout::Alignment,
    style::{Color, Modifier, Style, Stylize},
    text::{Line, Span},
};
use serde_json::Value;
use unicode_width::UnicodeWidthStr;

/// Markdown read one line at a time, as a reply streams in. Only what a
/// line can say on its own is styled; a fence is remembered between lines.
#[derive(Default, Clone)]
pub struct Markdown {
    fence: bool,
    /// An SVG or Mermaid fence's language and text, gathered to be drawn
    /// when it closes.
    picture: Option<(String, String)>,
    ready: Option<(String, String)>,
    /// How far the last line's wrapped rows are indented: under a list
    /// item's text, so a continuation does not read as a new item.
    hang: usize,
    /// A table's rows, held until it ends, so its columns can be fitted.
    table: Vec<String>,
    /// Inside display math, gathered in `picture`: what closes it,
    /// `$$` or `\]`.
    math: Option<&'static str>,
    /// The open fence's code, in its language's colors when known.
    code: Option<crate::highlight::Code>,
    /// The folder a picture's relative path is read from: the file's,
    /// or the agent's workspace; else where Sonny runs.
    folder: Option<std::path::PathBuf>,
    /// A file read as a document, not a reply: an ```html or ```svg
    /// fence in it is code to read, as GitHub shows it, not drawn.
    document: bool,
    /// Inside an HTML comment that spans lines.
    comment: bool,
}

impl Markdown {
    /// A reply's Markdown, its pictures' paths read from `folder`.
    pub fn in_folder(folder: &std::path::Path) -> Self {
        Self {
            folder: Some(folder.to_owned()),
            ..Self::default()
        }
    }

    /// A Markdown file in `folder`, read as a document.
    pub fn document(folder: &std::path::Path) -> Self {
        Self {
            document: true,
            ..Self::in_folder(folder)
        }
    }

    /// A streamed line as rows: a table that ended first, then the line,
    /// then a fence's picture or diff when the line closed one. `first`
    /// says nothing of the reply is shown yet, so it gets the bullet.
    pub fn feed(&mut self, text: &str, width: usize, first: bool) -> Vec<Line<'static>> {
        let mut text = text;
        if !self.fence && self.math.is_none() {
            // An HTML comment over lines says nothing; what follows it does.
            if self.comment {
                match text.split_once("-->") {
                    Some((_, after)) => {
                        self.comment = false;
                        text = after;
                        if text.trim().is_empty() {
                            return Vec::new();
                        }
                    }
                    None => return Vec::new(),
                }
            }
            if let Some(after) = text.trim_start().strip_prefix("<!--")
                && !after.contains("-->")
            {
                self.comment = true;
                return Vec::new();
            }
            if let Some(pictures) = pictures(text) {
                let mut out = self.finish(width);
                out.extend(self.pictures(&pictures, width));
                return out;
            }
        }
        if !self.fence && table_row(text) {
            self.table.push(text.trim().to_owned());
            return Vec::new();
        }
        let mut out = self.finish(width);
        if let Some(styled) = self.line(text) {
            let lead = if first && out.is_empty() {
                "• "
            } else {
                "  "
            };
            out.extend(wrap(styled, width, Span::raw(lead), self.rest()));
        }
        out.extend(self.picture(width));
        out
    }

    /// What is still held when a table ends.
    pub fn finish(&mut self, width: usize) -> Vec<Line<'static>> {
        if self.table.is_empty() {
            return Vec::new();
        }
        crate::table::draw(&std::mem::take(&mut self.table), width)
    }

    /// What is still held when the reply ends: a table, and a fence or
    /// `$$` block that never closed, shown as the code it is.
    pub fn end(&mut self, width: usize, first: bool) -> Vec<Line<'static>> {
        let mut out = self.finish(width);
        if let Some((_, source)) = self.picture.take() {
            for text in source.trim_start_matches('\n').lines() {
                let lead = if first && out.is_empty() {
                    "• "
                } else {
                    "  "
                };
                let line = Line::from(vec![
                    Span::raw("  "),
                    Span::styled(text.to_owned(), Style::new().fg(Color::Cyan)),
                ]);
                out.extend(wrap(line, width, Span::raw(lead), Span::raw("    ")));
            }
        }
        *self = Self {
            folder: self.folder.take(),
            document: self.document,
            ..Self::default()
        };
        out
    }

    /// What the last line's wrapped rows start with, inside a reply.
    pub fn rest(&self) -> Span<'static> {
        Span::raw(format!("  {}", " ".repeat(self.hang)))
    }

    /// The picture a fence that just closed drew, as rows of the
    /// terminal's image cells; or, when it could not be drawn, its source
    /// and why.
    pub fn picture(&mut self, width: usize) -> Vec<Line<'static>> {
        let Some((lang, source)) = self.ready.take() else {
            return Vec::new();
        };
        if matches!(lang.as_str(), "diff" | "patch") {
            return crate::diff::fence(&source, width);
        }
        if lang == "html" {
            return page(&source, width);
        }
        // Math where pictures cannot be drawn: as text, line by line.
        if lang == "math" && !crate::term::graphics() {
            return source
                .lines()
                .filter(|l| !l.trim().is_empty())
                .flat_map(|l| {
                    let line = Line::from(crate::math::unicode(l.trim())).italic();
                    wrap(line, width, Span::raw("    "), Span::raw("    "))
                })
                .collect();
        }
        // Kept in a file a click on the picture opens in the pane, bigger:
        // the diagram's source, or the formula as MathJax drew it.
        let kept = match lang.as_str() {
            "mermaid" => keep(&source, "mmd", |path| std::fs::write(path, &source)),
            "svg" => keep(&source, "svg", |path| std::fs::write(path, &source)),
            _ => None,
        };
        let math = (lang == "math").then(|| kept_path(&(&source, crate::term::light()), "svg"));
        let drawn = crate::cache::drawn(&lang, &source, width, || {
            let svg = match lang.as_str() {
                "mermaid" => crate::mermaid::svg(&source)?,
                "math" => {
                    let svg = crate::math::svg(&source)?;
                    if let Some(Some(path)) = &math {
                        let _ = std::fs::write(path, &svg);
                    }
                    svg
                }
                _ => source.clone(),
            };
            crate::image::render(&svg, width)
        });
        let kept = kept.or(math.flatten().filter(|path| path.is_file()));
        let url = kept.and_then(|path| crate::links::file_url(&path));
        match drawn.and_then(|drawn| crate::image::show_linked(&drawn, url)) {
            Ok(rows) => rows,
            Err(error) => {
                let mut out: Vec<Line<'static>> = source
                    .lines()
                    .flat_map(|line| {
                        let code = Line::from(line.to_owned()).fg(Color::Cyan);
                        wrap(code, width, Span::raw("    "), Span::raw("    "))
                    })
                    .collect();
                out.push(Line::from(format!("    could not draw it: {error}")).dim());
                out
            }
        }
    }

    /// The styled line, or none for a fence marker, which is not shown.
    pub fn line(&mut self, text: &str) -> Option<Line<'static>> {
        let trimmed = text.trim_start();
        self.hang = 0;
        // $$ display math $$, on one line or around several, and the
        // \[ display math \] many models write.
        let line = trimmed.trim_end();
        if !self.fence && self.math.is_none() {
            let open = [("$$", "$$"), ("\\[", "\\]")]
                .into_iter()
                .find(|(open, _)| line.starts_with(open));
            if let Some((open, close)) = open {
                let inner = &line[open.len()..];
                if let Some(math) = inner.strip_suffix(close) {
                    self.ready = Some(("math".into(), math.to_owned()));
                    return None;
                }
                // `\[` opens display math only alone on its line: one that
                // runs on is an escaped bracket.
                if open == "$$" || inner.is_empty() {
                    let held = self.picture.take();
                    self.picture = held.or(Some(("math".into(), format!("{inner}\n"))));
                    self.math = Some(close);
                    return None;
                }
            }
        }
        if let Some(close) = self.math
            && line.starts_with(close)
        {
            self.math = None;
            self.ready = self.picture.take();
            return None;
        }
        if self.math.is_some()
            && let Some((_, math)) = &mut self.picture
        {
            math.push_str(text);
            math.push('\n');
            return None;
        }
        if let Some(lang) = trimmed.strip_prefix("```") {
            self.fence = !self.fence;
            let lang = lang.trim();
            let lang = match lang {
                "latex" | "tex" | "katex" => "math",
                lang => lang,
            };
            let drawn = match lang {
                "mermaid" => crate::term::graphics(),
                "svg" => crate::term::graphics() && !self.document,
                "html" => !self.document,
                _ => false,
            };
            if self.fence && (drawn || matches!(lang, "diff" | "patch" | "math")) {
                self.picture = Some((lang.to_owned(), String::new()));
            } else if self.fence {
                self.code = crate::highlight::Code::language(lang);
            } else {
                self.ready = self.picture.take();
                self.code = None;
            }
            return None;
        }
        if let Some((_, picture)) = &mut self.picture {
            picture.push_str(text);
            picture.push('\n');
            return None;
        }
        if self.fence {
            self.hang = 2;
            let mut spans = vec![Span::raw("  ")];
            match &mut self.code {
                Some(code) => spans.extend(code.line(text)),
                None => spans.push(Span::styled(text.to_owned(), Style::new().fg(Color::Cyan))),
            }
            return Some(Line::from(spans));
        }
        let hashes = trimmed.bytes().take_while(|b| *b == b'#').count();
        if (1..=6).contains(&hashes) && trimmed[hashes..].starts_with(' ') {
            let style = Style::new().add_modifier(Modifier::BOLD);
            return Some(Line::from(inline(trimmed[hashes + 1..].trim(), style)));
        }
        let indent = &text[..text.len() - trimmed.len()];
        let bare = trimmed.trim_end();
        if (bare.len() >= 3
            && ["-", "*", "_"]
                .iter()
                .any(|c| bare.chars().all(|x| x.to_string() == *c || x == ' ')))
            || matches!(bare, "<hr>" | "<hr/>" | "<hr />")
        {
            return Some(Line::from("─".repeat(width_hint())).dim());
        }
        for marker in ["- ", "* ", "+ "] {
            if let Some(rest) = trimmed.strip_prefix(marker) {
                self.hang = indent.len() + 2;
                let (mark, rest) = match rest.get(..4) {
                    Some("[ ] ") => ("☐ ", &rest[4..]),
                    Some("[x] " | "[X] ") => ("☑ ", &rest[4..]),
                    _ => ("- ", rest),
                };
                let mut spans = vec![Span::raw(format!("{indent}{mark}")).dim()];
                spans.extend(inline(rest, Style::new()));
                return Some(Line::from(spans));
            }
        }
        let digits = trimmed.bytes().take_while(u8::is_ascii_digit).count();
        if (1..=3).contains(&digits)
            && let Some(rest) = trimmed[digits..].strip_prefix(". ")
        {
            self.hang = indent.len() + digits + 2;
            let mut spans = vec![Span::raw(format!("{indent}{}. ", &trimmed[..digits])).dim()];
            spans.extend(inline(rest, Style::new()));
            return Some(Line::from(spans));
        }
        if let Some(rest) = trimmed.strip_prefix("> ") {
            let mut spans = vec![Span::raw("▌ ").fg(Color::Green)];
            spans.extend(inline(rest, Style::new().italic()));
            return Some(Line::from(spans));
        }
        let spans = inline(text, Style::new());
        // A line of HTML that only lays the page out (`<p align=…>`,
        // `</div>`, `<details>`) leaves no row of its own.
        if text.contains('<') && spans.iter().all(|span| span.content.trim().is_empty()) {
            return None;
        }
        Some(Line::from(spans))
    }

    /// Pictures a line holds and nothing else, one under another; those
    /// that cannot be drawn here (another machine's, or a terminal with no
    /// pictures) as their words, together on a row.
    fn pictures(&self, pictures: &[Picture], width: usize) -> Vec<Line<'static>> {
        let mut out = Vec::new();
        let mut words: Vec<Span<'static>> = Vec::new();
        let flush = |words: &mut Vec<Span<'static>>, out: &mut Vec<Line<'static>>| {
            if !words.is_empty() {
                let line = Line::from(std::mem::take(words));
                out.extend(wrap(line, width, Span::raw("  "), Span::raw("  ")));
            }
        };
        for picture in pictures {
            let path = self.path(&picture.src);
            match path.as_deref().and_then(|path| picture_file(path, width)) {
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

    /// The file a picture's address names on this machine, if it does.
    fn path(&self, src: &str) -> Option<std::path::PathBuf> {
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
            None => match &self.folder {
                Some(folder) => folder.join(src),
                None => std::env::current_dir().ok()?.join(src),
            },
        };
        path.is_file().then_some(path)
    }
}

/// A picture Markdown or HTML names: `![alt](src)` or `<img src alt>`.
#[derive(Debug, PartialEq)]
struct Picture {
    alt: String,
    src: String,
}

/// The pictures in a line that holds nothing else: Markdown's `![…](…)`,
/// those inside a link (`[![…](…)](…)`, as badges are), and HTML's
/// `<img>`, among the tags that only lay them out (`<p>`, `<a>`,
/// `<picture>`).
fn pictures(text: &str) -> Option<Vec<Picture>> {
    let mut found = Vec::new();
    let mut rest = text.trim();
    while !rest.is_empty() {
        if let Some((taken, picture)) = markdown_picture(rest) {
            found.push(picture);
            rest = rest[taken..].trim_start();
            continue;
        }
        // A picture as a link's text, and where the link goes.
        if let Some(inner) = rest.strip_prefix('[')
            && let Some((taken, picture)) = markdown_picture(inner)
            && let Some(after) = inner[taken..].strip_prefix("](")
            && let Some(end) = after.find(')')
        {
            found.push(picture);
            rest = after[end + 1..].trim_start();
            continue;
        }
        let (taken, tag) = html_tag(rest)?;
        match tag.name.as_str() {
            "img" => found.push(Picture {
                alt: tag.attribute("alt").unwrap_or_default(),
                src: tag.attribute("src")?,
            }),
            "p" | "div" | "a" | "picture" | "source" | "center" | "br" | "span" | "!--" => {}
            _ => return None,
        }
        rest = rest[taken..].trim_start();
    }
    (!found.is_empty()).then_some(found)
}

/// `![alt](src "title")` at the start of `text`: how long it is, and
/// the picture.
fn markdown_picture(text: &str) -> Option<(usize, Picture)> {
    let after = text.strip_prefix("![")?;
    let close = after.find("](")?;
    let alt = &after[..close];
    let target = &after[close + 2..];
    let end = target.find(')')?;
    let src = target[..end].split_whitespace().next()?;
    let src = src.trim_start_matches('<').trim_end_matches('>');
    Some((
        2 + close + 2 + end + 1,
        Picture {
            alt: alt.to_owned(),
            src: src.to_owned(),
        },
    ))
}

/// A picture file drawn, `path`'s rows of the terminal's cells that a
/// click opens in the pane; none where it cannot be.
fn picture_file(path: &std::path::Path, width: usize) -> Option<Vec<Line<'static>>> {
    if !crate::term::graphics() {
        return None;
    }
    let extension = (path.extension().and_then(|e| e.to_str()))
        .unwrap_or_default()
        .to_lowercase();
    let meta = std::fs::metadata(path).ok()?;
    // Drawn again only when the file changed.
    let key = format!(
        "{}\0{}\0{:?}",
        path.display(),
        meta.len(),
        meta.modified().ok()
    );
    let drawn = match extension.as_str() {
        "svg" => crate::cache::drawn("svg file", &key, width, || {
            let text = std::fs::read_to_string(path).map_err(|e| e.to_string())?;
            crate::image::render(&text, width)
        }),
        "png" | "jpg" | "jpeg" | "gif" | "webp" => {
            crate::cache::drawn("picture file", &key, width, || {
                crate::image::file(path, width, crate::image::ROWS)
            })
        }
        _ => return None,
    };
    let url = crate::links::file_url(path);
    drawn
        .and_then(|drawn| crate::image::show_linked(&drawn, url))
        .ok()
}

/// A picture as words where it is not drawn: a mark, its description
/// (or its file's name) and, when it is a file here, where it is.
fn picture_words(picture: &Picture, here: bool) -> Vec<Span<'static>> {
    let name = match picture.alt.trim() {
        "" => picture.src.rsplit('/').next().unwrap_or(&picture.src),
        alt => alt,
    };
    let mut words = vec![
        Span::raw("▣ ").dim(),
        Span::raw(name.to_owned()).underlined(),
    ];
    if here && picture.src != name {
        words.push(Span::raw(format!(" ({})", picture.src)).dim());
    }
    words
}

/// An HTML tag: its name, lowercased (`!--` for a comment), whether it
/// closes, and what is written after the name.
struct Tag {
    name: String,
    closing: bool,
    attributes: String,
}

impl Tag {
    /// An attribute's value, quoted or not.
    fn attribute(&self, wanted: &str) -> Option<String> {
        let mut rest = self.attributes.as_str();
        loop {
            rest = rest.trim_start_matches(|c: char| c.is_whitespace() || c == '/');
            if rest.is_empty() {
                return None;
            }
            let name_end = rest
                .find(|c: char| c == '=' || c.is_whitespace() || c == '/')
                .unwrap_or(rest.len());
            let name = rest[..name_end].to_ascii_lowercase();
            rest = rest[name_end..].trim_start();
            let Some(after) = rest.strip_prefix('=') else {
                if name.is_empty() {
                    return None;
                }
                continue;
            };
            let after = after.trim_start();
            let (value, left) = match after.chars().next() {
                Some(quote @ ('"' | '\'')) => {
                    let inner = &after[1..];
                    let end = inner.find(quote).unwrap_or(inner.len());
                    (&inner[..end], inner.get(end + 1..).unwrap_or(""))
                }
                _ => {
                    let end = after.find(char::is_whitespace).unwrap_or(after.len());
                    (&after[..end], &after[end..])
                }
            };
            if name == wanted {
                return Some(value.to_owned());
            }
            rest = left;
        }
    }
}

/// The HTML elements a README is written with, whose tags are read for
/// what they mean, not shown as text.
const ELEMENTS: &[&str] = &[
    "a",
    "abbr",
    "b",
    "big",
    "blockquote",
    "br",
    "center",
    "cite",
    "code",
    "dd",
    "del",
    "details",
    "div",
    "dl",
    "dt",
    "em",
    "figcaption",
    "figure",
    "font",
    "h1",
    "h2",
    "h3",
    "h4",
    "h5",
    "h6",
    "hr",
    "i",
    "img",
    "ins",
    "kbd",
    "li",
    "mark",
    "ol",
    "p",
    "picture",
    "pre",
    "q",
    "s",
    "samp",
    "section",
    "small",
    "source",
    "span",
    "strike",
    "strong",
    "sub",
    "summary",
    "sup",
    "table",
    "tbody",
    "td",
    "tfoot",
    "th",
    "thead",
    "tr",
    "tt",
    "u",
    "ul",
    "var",
    "video",
];

/// A tag (or `<!-- comment -->`) of an element in [`ELEMENTS`] at the
/// start of `text`, and how long it is.
fn html_tag(text: &str) -> Option<(usize, Tag)> {
    if let Some(after) = text.strip_prefix("<!--") {
        let end = after.find("-->")?;
        let tag = Tag {
            name: "!--".into(),
            closing: false,
            attributes: String::new(),
        };
        return Some((4 + end + 3, tag));
    }
    let after = text.strip_prefix('<')?;
    let (closing, after) = match after.strip_prefix('/') {
        Some(after) => (true, after),
        None => (false, after),
    };
    let name_end = after
        .find(|c: char| !c.is_ascii_alphanumeric())
        .unwrap_or(after.len());
    let name = after[..name_end].to_ascii_lowercase();
    if !ELEMENTS.contains(&name.as_str()) {
        return None;
    }
    let rest = &after[name_end..];
    // A name, then attributes after a space, or the tag's end.
    if !rest.starts_with(|c: char| c == '>' || c == '/' || c.is_whitespace()) {
        return None;
    }
    let end = rest.find('>')?;
    if rest[..end].contains('<') {
        return None;
    }
    let taken = text.len() - rest.len() + end + 1;
    let attributes = rest[..end].to_owned();
    Some((
        taken,
        Tag {
            name,
            closing,
            attributes,
        },
    ))
}

/// Where the element `name` that opened just before `text` closes: the
/// text inside it, and how far past its closing tag is.
fn inside<'a>(text: &'a str, name: &str) -> Option<(&'a str, usize)> {
    let lower = text.to_ascii_lowercase();
    let close = format!("</{name}>");
    let end = lower.find(&close)?;
    Some((&text[..end], end + close.len()))
}

/// A rule's width: no wider than a turn's closing rule.
fn width_hint() -> usize {
    40
}

/// Inline marks inside a line: `code`, **bold**, *italic* or _italic_,
/// ~~struck~~, [links](url) and $math$ (as Unicode), and the HTML tags
/// models use (<b>, <i>, <code>, <kbd>, <br>).
pub fn inline(text: &str, base: Style) -> Vec<Span<'static>> {
    let mut spans: Vec<Span<'static>> = Vec::new();
    let mut plain = String::new();
    let mut rest = text;
    let flush = |plain: &mut String, spans: &mut Vec<Span<'static>>| {
        if !plain.is_empty() {
            spans.push(Span::styled(std::mem::take(plain), base));
        }
    };
    while let Some(c) = rest.chars().next() {
        // \(math\), as many models write it.
        if let Some(after) = rest.strip_prefix("\\(")
            && let Some(end) = after.find("\\)")
            && end > 0
        {
            flush(&mut plain, &mut spans);
            let math = crate::math::unicode(after[..end].trim());
            spans.push(Span::styled(math, base.italic()));
            rest = &after[end + 2..];
            continue;
        }
        // A mark and its closer, with the style of what is between.
        let pairs: [(&str, &str, Style, bool); 11] = [
            ("`", "`", base.fg(Color::Cyan), false),
            ("**", "**", base.add_modifier(Modifier::BOLD), true),
            ("__", "__", base.add_modifier(Modifier::BOLD), true),
            ("~~", "~~", base.add_modifier(Modifier::CROSSED_OUT), true),
            ("*", "*", base.add_modifier(Modifier::ITALIC), true),
            ("_", "_", base.add_modifier(Modifier::ITALIC), true),
            ("<b>", "</b>", base.add_modifier(Modifier::BOLD), true),
            (
                "<strong>",
                "</strong>",
                base.add_modifier(Modifier::BOLD),
                true,
            ),
            ("<i>", "</i>", base.add_modifier(Modifier::ITALIC), true),
            ("<em>", "</em>", base.add_modifier(Modifier::ITALIC), true),
            ("<code>", "</code>", base.fg(Color::Cyan), false),
        ];
        let found = pairs.iter().find_map(|(open, close, style, nested)| {
            let after = rest.strip_prefix(open)?;
            // `_` and `*` mark emphasis only around a word, not in
            // snake_case or 2*3*4.
            if matches!(*open, "*" | "_")
                && (after.starts_with(' ')
                    || plain.chars().last().is_some_and(char::is_alphanumeric))
            {
                return None;
            }
            let end = after.find(close)?;
            (end > 0).then(|| {
                (
                    open.len() + end + close.len(),
                    &after[..end],
                    *style,
                    *nested,
                )
            })
        });
        if let Some((taken, inner, style, nested)) = found {
            flush(&mut plain, &mut spans);
            if nested {
                spans.extend(inline(inner, style));
            } else {
                spans.push(Span::styled(inner.to_owned(), style));
            }
            rest = &rest[taken..];
            continue;
        }
        if let Some(after) = rest.strip_prefix("<kbd>")
            && let Some(end) = after.find("</kbd>")
        {
            flush(&mut plain, &mut spans);
            spans.push(Span::styled(
                format!(" {} ", &after[..end]),
                base.reversed(),
            ));
            rest = &after[end + "</kbd>".len()..];
            continue;
        }
        if let Some(after) = ["<br>", "<br/>", "<br />"]
            .iter()
            .find_map(|tag| rest.strip_prefix(tag))
        {
            plain.push(' ');
            rest = after;
            continue;
        }
        // Other HTML a README is written with, read for what it means.
        if c == '<'
            && let Some((taken, tag)) = html_tag(rest)
        {
            flush(&mut plain, &mut spans);
            rest = &rest[taken..];
            if tag.closing {
                continue;
            }
            let mut within = |style: Style, spans: &mut Vec<Span<'static>>| -> bool {
                match inside(rest, &tag.name) {
                    Some((inner, taken)) => {
                        spans.extend(inline(inner, style));
                        rest = &rest[taken..];
                        true
                    }
                    None => false,
                }
            };
            match tag.name.as_str() {
                "a" => {
                    let href = tag.attribute("href").unwrap_or_default();
                    let before = spans.len();
                    if within(base.underlined(), &mut spans) {
                        let label: String =
                            spans[before..].iter().map(|s| s.content.as_ref()).collect();
                        if !href.is_empty() && href != label && !href.starts_with('#') {
                            spans.push(Span::styled(format!(" ({href})"), base.dim()));
                        }
                    }
                }
                "img" => spans.extend(picture_words(
                    &Picture {
                        alt: tag.attribute("alt").unwrap_or_default(),
                        src: tag.attribute("src").unwrap_or_default(),
                    },
                    false,
                )),
                "h1" | "h2" | "h3" | "h4" | "h5" | "h6" | "summary" => {
                    if tag.name == "summary" {
                        spans.push(Span::styled("▸ ", base.dim()));
                    }
                    within(base.add_modifier(Modifier::BOLD), &mut spans);
                }
                "del" | "s" | "strike" => {
                    within(base.add_modifier(Modifier::CROSSED_OUT), &mut spans);
                }
                "u" | "ins" => {
                    within(base.underlined(), &mut spans);
                }
                "li" => spans.push(Span::styled("- ", base.dim())),
                _ => {}
            }
            continue;
        }
        // [text](url): the text, underlined, and the address when it says
        // more; ![text](src), a picture among words, the same.
        let link = match rest.strip_prefix("![") {
            Some(_) => &rest[1..],
            None => rest,
        };
        if link.starts_with('[')
            && let Some(close) = link.find("](")
            && let Some(end) = link[close + 2..].find(')')
        {
            let label = &link[1..close];
            let url = &link[close + 2..close + 2 + end];
            if !label.contains('[') && !url.contains(' ') {
                flush(&mut plain, &mut spans);
                if link.len() < rest.len() {
                    spans.push(Span::styled("▣ ", base.dim()));
                }
                spans.extend(inline(label, base.underlined()));
                if url != label && !url.starts_with('#') {
                    spans.push(Span::styled(format!(" ({url})"), base.dim()));
                }
                rest = &link[close + 2 + end + 1..];
                continue;
            }
        }
        // $math$, not a price: no space just inside either dollar.
        if c == '$'
            && let Some(end) = rest[1..].find('$')
            && end > 0
            && !rest[1..].starts_with(' ')
            && !rest[..end + 1].ends_with(' ')
            && !rest[1..].starts_with(|d: char| d.is_ascii_digit())
        {
            flush(&mut plain, &mut spans);
            let math = crate::math::unicode(&rest[1..end + 1]);
            spans.push(Span::styled(math, base.italic()));
            rest = &rest[end + 2..];
            continue;
        }
        plain.push(c);
        rest = &rest[c.len_utf8()..];
    }
    flush(&mut plain, &mut spans);
    spans
}

/// The HTML page a reply showed last, for `/open`.
pub static PAGE: std::sync::Mutex<Option<std::path::PathBuf>> = std::sync::Mutex::new(None);

/// A ```html fence, drawn as the page by a headless browser and kept in
/// a file `/open` shows in the browser; its first lines as code where it
/// cannot be drawn.
fn page(source: &str, width: usize) -> Vec<Line<'static>> {
    let dir = std::env::temp_dir().join("sonny-pages");
    let name = format!(
        "{}-{}.html",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_millis()
    );
    let path = dir.join(name);
    if let Err(error) = std::fs::create_dir_all(&dir).and_then(|()| std::fs::write(&path, source)) {
        return vec![Line::from(format!("    could not keep the page: {error}")).dim()];
    }
    let drawn = match crate::term::graphics() {
        true => crate::cache::drawn("html", source, width, || {
            let png = crate::html::render(&path, crate::image::page_width(width))?;
            crate::image::page(&png, width)
        })
        .and_then(|drawn| {
            let url = crate::links::file_url(&path);
            Ok((crate::image::show_linked(&drawn, url)?, drawn.cut))
        }),
        false => Err(String::new()),
    };
    let (mut out, more) = match drawn {
        Ok((rows, cut)) => (rows, if cut { " for the rest" } else { "" }),
        Err(error) => {
            let lines: Vec<&str> = source.lines().collect();
            let shown = if lines.len() > 8 { 6 } else { lines.len() };
            let mut out: Vec<Line<'static>> = lines[..shown]
                .iter()
                .flat_map(|line| {
                    let code = Line::from(line.to_string()).fg(Color::Cyan);
                    wrap(code, width, Span::raw("    "), Span::raw("    "))
                })
                .collect();
            if lines.len() > shown {
                out.push(Line::from(format!("    … +{} lines", lines.len() - shown)).dim());
            }
            if !error.is_empty() {
                out.push(Line::from(format!("    could not draw it: {error}")).dim());
            }
            (out, "")
        }
    };
    out.push(Line::from(vec![
        Span::raw("    ↗ ").fg(Color::Cyan),
        Span::raw("/open").bold(),
        Span::raw(more).dim(),
    ]));
    if let Ok(mut page) = PAGE.lock() {
        *page = Some(path);
    }
    out
}

/// Where a drawn fence's source is kept for the pane, named by what it
/// holds, so drawing it again keeps the same file.
fn kept_path(what: &impl std::hash::Hash, extension: &str) -> Option<std::path::PathBuf> {
    use std::hash::{Hash, Hasher};
    let dir = std::env::temp_dir().join("sonny-pages");
    std::fs::create_dir_all(&dir).ok()?;
    let mut hasher = std::hash::DefaultHasher::new();
    (what, extension).hash(&mut hasher);
    Some(dir.join(format!("{:016x}.{extension}", hasher.finish())))
}

/// `source` kept in its file, written by `write` unless it is there.
fn keep(
    source: &str,
    extension: &str,
    write: impl FnOnce(&std::path::Path) -> std::io::Result<()>,
) -> Option<std::path::PathBuf> {
    let path = kept_path(&source, extension)?;
    if !path.is_file() {
        write(&path).ok()?;
    }
    Some(path)
}

/// `| a | b |`: a table's row, its separator included.
fn table_row(text: &str) -> bool {
    let t = text.trim();
    t.starts_with('|') && t.len() > 1 && t[1..].contains('|')
}

/// How a row says it goes on with a word cut at the end of the row above,
/// so an address there can be followed: an explicit left alignment, which
/// draws as the default does.
pub const CUT: Alignment = Alignment::Left;

/// A line wrapped at word boundaries to `width` columns, the first row
/// after `first` and the others after `rest`, which should be as wide.
/// A word wider than a row is cut, and the rows it goes on in are [`CUT`].
pub fn wrap(
    line: Line<'static>,
    width: usize,
    first: Span<'static>,
    rest: Span<'static>,
) -> Vec<Line<'static>> {
    wrap_lines(line, width, Line::from(first), Line::from(rest))
}

/// [`wrap`], with leads of more than one style.
pub fn wrap_lines(
    line: Line<'static>,
    width: usize,
    first: Line<'static>,
    rest: Line<'static>,
) -> Vec<Line<'static>> {
    // The room after each row's lead: the rows after the first may have
    // a wider one.
    let mut room = width.saturating_sub(first.width()).max(8);
    let rest_room = width.saturating_sub(rest.width()).max(8);
    // A row this line itself was wrapped onto stays one.
    let first = match lead(&line) {
        Some(columns) => Line::from([vec![goes_on(columns, true)], first.spans].concat()),
        // A row wrapped again keeps the lead it opened with.
        None => {
            let opens = opens(&line).unwrap_or(0) + first.width();
            Line::from([vec![goes_on(opens, false)], first.spans].concat())
        }
    };
    let rest = [vec![goes_on(rest.width(), true)], rest.spans].concat();
    // Each row, and whether it goes on with a word cut in the row above.
    let mut rows: Vec<(Vec<Span<'static>>, bool)> =
        vec![(first.spans, line.alignment == Some(CUT))];
    let mut used = 0;
    for span in line.spans {
        let style = line.style.patch(span.style);
        for word in words(&span.content) {
            let mut word = word.to_owned();
            let mut wide = word.width();
            // A word's trailing spaces may hang past the edge.
            if used + word.trim_end().width() > room && used > 0 {
                rows.push((rest.clone(), false));
                room = rest_room;
                used = 0;
                if word.trim().is_empty() {
                    continue;
                }
            }
            // A word wider than a row is cut where the row ends.
            while wide > room {
                let mut cut = 0;
                let mut cols = 0;
                for (i, c) in word.char_indices() {
                    let w = unicode_width::UnicodeWidthChar::width(c).unwrap_or(0);
                    if cols + w > room - used {
                        break;
                    }
                    cols += w;
                    cut = i + c.len_utf8();
                }
                if cut == 0 {
                    break;
                }
                rows.last_mut()
                    .unwrap()
                    .0
                    .push(Span::styled(word[..cut].to_owned(), style));
                rows.push((rest.clone(), true));
                room = rest_room;
                used = 0;
                word = word[cut..].to_owned();
                wide = word.width();
            }
            used += wide;
            rows.last_mut().unwrap().0.push(Span::styled(word, style));
        }
    }
    rows.into_iter()
        .map(|(spans, cut)| Line {
            alignment: cut.then_some(CUT),
            ..Line::from(spans)
        })
        .collect()
}

/// How a wrapped row tells a copy what to leave out and join: an empty
/// first span, which draws nothing, whose underline colour is how many
/// columns the row's lead takes (a bullet or an indent, not the text), and
/// whose blink, never drawn, says the row goes on with the line above.
fn goes_on(lead: usize, joins: bool) -> Span<'static> {
    let mut style = Style::new()
        .underline_color(Color::Indexed(lead.min(255) as u8))
        .add_modifier(Modifier::HIDDEN);
    if joins {
        style = style.add_modifier(Modifier::SLOW_BLINK);
    }
    Span::styled("", style)
}

/// A wrapped row's lead, in columns, and whether the row goes on with
/// the line above.
fn mark(line: &Line) -> Option<(usize, bool)> {
    let marks = line.spans.iter().take_while(|span| span.content.is_empty());
    marks
        .filter(|span| span.style.add_modifier.contains(Modifier::HIDDEN))
        .find_map(|span| match span.style.underline_color {
            Some(Color::Indexed(lead)) => Some((
                lead as usize,
                span.style.add_modifier.contains(Modifier::SLOW_BLINK),
            )),
            _ => None,
        })
}

/// The columns of a row's lead, when a line was wrapped onto the row.
pub fn lead(line: &Line) -> Option<usize> {
    mark(line).filter(|(_, joins)| *joins).map(|(lead, _)| lead)
}

/// The columns of the lead a wrapped line starts with, if any.
pub fn opens(line: &Line) -> Option<usize> {
    mark(line).filter(|(_, joins)| !joins).map(|(lead, _)| lead)
}

/// Words with the spaces after each kept, so wrapping loses nothing.
fn words(text: &str) -> impl Iterator<Item = &str> {
    let mut rest = text;
    std::iter::from_fn(move || {
        if rest.is_empty() {
            return None;
        }
        let end = rest
            .find(' ')
            .map(|i| i + rest[i..].find(|c| c != ' ').unwrap_or(rest.len() - i))
            .unwrap_or(rest.len());
        let (word, after) = rest.split_at(end);
        rest = after;
        Some(word)
    })
}

/// A prompt, as the person (or another bot) sent it.
pub fn prompt(text: &str, from: Option<&str>, width: usize) -> Vec<Line<'static>> {
    let mut out = Vec::new();
    // What Sonny (or the app) told the lead of its tasks, or what the
    // lookout woke it for: news, not words of the person's, kept to one
    // quiet row; the lead's answer says the rest.
    if let Some(news) = news(text) {
        return vec![news_row(&news, width)];
    }
    if let Some(from) = from {
        out.push(Line::from(format!("  from {from}")).dim());
    }
    // An answer to the lead's question says which, and shows the answer.
    let text = match crate::asks::split(text) {
        Some((question, answer)) => {
            out.push(crate::asks::answering(question, width));
            answer
        }
        None => text,
    };
    // A long paste shows its start; the rest is in the conversation.
    let lines: Vec<&str> = text.lines().collect();
    let shown = if lines.len() > PROMPT_LINES + 2 {
        PROMPT_LINES
    } else {
        lines.len()
    };
    for (i, line) in lines[..shown].iter().enumerate() {
        let lead = if i == 0 { "› " } else { "  " };
        out.extend(wrap(
            Line::from(line.to_string()),
            width,
            Span::raw(lead).bold().fg(Color::Magenta),
            Span::raw("  "),
        ));
    }
    if lines.len() > shown {
        out.push(Line::from(format!("  … +{} lines", lines.len() - shown)).dim());
    }
    out
}

/// Whether a message is from another bot or tells the lead of its tasks:
/// words between agents, drawn [`relayed`] and not as the person's.
pub fn relays(text: &str, from: Option<&str>) -> bool {
    from.is_some() || news(text).is_some()
}

/// Words between agents, kept quiet: one dim row, who sent it and its
/// first words (or the news it brings), `▸` to open it; open, the same
/// row with `▾` and the whole message under it.
pub fn relayed(text: &str, from: Option<&str>, open: bool, width: usize) -> Vec<Line<'static>> {
    if !open {
        return relayed_shut(&relayed_head(text, from), width);
    }
    let head = match (news(text), from) {
        (Some(parts), _) => parts[0].clone(),
        (None, Some(from)) => format!("from {from}"),
        (None, None) => text.lines().next().unwrap_or("").to_owned(),
    };
    let head = cut(&format!("↳ {}", head.trim()), width.saturating_sub(2));
    let mut out = vec![Line::from(format!("{head} ▾"))];
    for line in text.lines() {
        out.extend(wrap(
            Line::from(line.to_string()),
            width,
            Span::raw("  "),
            Span::raw("  "),
        ));
    }
    out.into_iter().map(|line| line.dim()).collect()
}

/// What the shut row of words between agents says: who sent them and
/// their first words, or the news they bring, cut short enough to keep.
pub fn relayed_head(text: &str, from: Option<&str>) -> String {
    let head = match (news(text), from) {
        (Some(parts), _) => parts.join(" · "),
        (None, Some(from)) => format!("{from}: {}", text.lines().next().unwrap_or("")),
        (None, None) => text.lines().next().unwrap_or("").to_owned(),
    };
    cut(head.trim(), RELAYED_HEAD)
}

/// The most columns of a shut row kept: wider than any conversation.
const RELAYED_HEAD: usize = 200;

/// The shut row of words between agents, from [`relayed_head`].
pub fn relayed_shut(head: &str, width: usize) -> Vec<Line<'static>> {
    let head = cut(&format!("↳ {head}"), width.saturating_sub(2));
    vec![Line::from(format!("{head} ▸")).dim()]
}

/// The most lines of a prompt shown above its reply.
const PROMPT_LINES: usize = 6;

/// How a message telling a lead of its tasks starts.
const TASK_UPDATES: &str = "Task updates: ";

/// A message that tells the lead of its tasks, as its kind and a few
/// words for each thing it tells.
fn news(text: &str) -> Option<Vec<String>> {
    let rest = text.strip_prefix(TASK_UPDATES)?;
    let kind = "Task updates";
    let mut parts = vec![kind.to_owned()];
    let mut memory = false;
    for line in rest.lines() {
        if line.starts_with("Memory changed") {
            memory = true;
        }
        let Some(item) = line.strip_prefix("- ") else {
            continue;
        };
        // "- 2 more tasks in the next update", "- 2 more in the next note"
        if let Some((count, _)) = item.split_once(" more ")
            && count.parse::<usize>().is_ok()
        {
            parts.push(format!("+{count} more"));
        } else if let Some((name, turn)) = item.split_once(": turn:") {
            // "- shop.fix: turn:shop.fix/5 failed, asked by you", and from
            // the judge "- shop.fix: turn:shop.fix/5, turn failed (why; todo)"
            let status = turn.split_once(' ').map_or("", |(_, s)| s);
            // A message in it: "the person wrote: \"…\"", "shop.api wrote
            // in it", "3 messages in it since turn:…, from a, b".
            if status.starts_with("the person wrote") {
                parts.push(format!("you wrote in {name}"));
            } else if let Some(who) = status.strip_suffix(" wrote in it") {
                parts.push(format!("{who} wrote in {name}"));
            } else if let Some((count, _)) = status.split_once(" messages in it")
                && count.parse::<usize>().is_ok()
            {
                parts.push(format!("{count} messages in {name}"));
            } else {
                let status = status.split([',', '(']).next().unwrap_or_default().trim();
                parts.push(format!("{name} {status}"));
            }
        }
    }
    if memory {
        parts.push("memory changed".to_owned());
    }
    Some(parts)
}

/// News on one dim row, its kind first, cut at the width.
fn news_row(parts: &[String], width: usize) -> Line<'static> {
    Line::from(cut(&format!("↳ {}", parts.join(" · ")), width)).dim()
}

/// `text` cut to `width` columns, with an ellipsis when it was longer.
pub fn cut(text: &str, width: usize) -> String {
    if text.width() <= width {
        return text.to_owned();
    }
    let mut cut = String::new();
    for c in text.chars() {
        if cut.width() + unicode_width::UnicodeWidthChar::width(c).unwrap_or(0) + 1 > width {
            break;
        }
        cut.push(c);
    }
    format!("{}…", cut.trim_end())
}

/// Words that are only an emoji or a few, trimmed: the person's message
/// that is only that is a reaction to what the bot said last.
pub fn reaction(text: &str) -> Option<&str> {
    let text = text.trim();
    let emoji = |c: char| {
        matches!(c as u32,
            0x1F000..=0x1FAFF // pictographs, faces, flags, skin tones
            | 0x2600..=0x27BF // ☀ ✅ ❤ ✨
            | 0x2B00..=0x2BFF // ⭐ ⬆
            | 0x2300..=0x23FF // ⌛ ⏰
            | 0x203C | 0x2049 // ‼ ⁉
            | 0xFE0F | 0x200D | 0x20E3) // emoji presentation, joiner, keycap
    };
    let (mut any, mut count) = (false, 0);
    for c in text.chars() {
        if c == ' ' {
            continue;
        }
        if !emoji(c) {
            return None;
        }
        any |= !matches!(c as u32, 0xFE0F | 0x200D | 0x20E3);
        count += 1;
    }
    (any && count <= REACTION_CHARS).then_some(text)
}

/// The most characters a reaction holds, joiners and modifiers counted:
/// a family emoji is seven.
const REACTION_CHARS: usize = 12;

/// The emoji typed as Slack's shortcodes, such as `:+1:`, that a message
/// made only of them is a reaction of: `+1` and `:+1:` both read 👍.
pub fn shortcodes(text: &str) -> Option<String> {
    let text = text.trim();
    if text.is_empty() {
        return None;
    }
    let mut out = String::new();
    for word in text.split(' ').filter(|w| !w.is_empty()) {
        let code = word.strip_prefix(':').and_then(|w| w.strip_suffix(':'));
        let emoji = match code.unwrap_or(word) {
            "+1" | "thumbsup" => "👍",
            "-1" | "thumbsdown" => "👎",
            _ if code.is_none() => return None,
            "heart" => "❤️",
            "tada" => "🎉",
            "eyes" => "👀",
            "pray" => "🙏",
            "joy" => "😂",
            "fire" => "🔥",
            "rocket" => "🚀",
            "100" => "💯",
            "white_check_mark" | "check" => "✅",
            "ok_hand" => "👌",
            "thinking" | "thinking_face" => "🤔",
            "smile" => "😄",
            _ => return None,
        };
        if !out.is_empty() {
            out.push(' ');
        }
        out.push_str(emoji);
    }
    Some(out)
}

/// A reaction, as a pill under the message it answers.
pub fn reacted(emoji: &str) -> Line<'static> {
    let pill = match crate::term::light() {
        true => Color::Rgb(232, 232, 238),
        false => Color::Rgb(50, 50, 58),
    };
    Line::from(vec![
        Span::raw("  "),
        Span::styled(format!(" {emoji} "), Style::new().bg(pill)),
    ])
}

/// A whole reply, as replayed history shows it, its pictures' paths
/// read from `folder`.
pub fn reply(text: &str, width: usize, folder: &std::path::Path) -> Vec<Line<'static>> {
    let leads = (Line::from("• "), Line::from("  "));
    markdown(&mut Markdown::in_folder(folder), text, width, leads)
}

/// Markdown as every surface draws it, the one way it is drawn: the
/// conversation, the pane, the margin, a card. `text` at `width`, read
/// by `markdown` (the folder its pictures are in, whether a document),
/// its first row after the first of `leads` and the rest after the
/// other, in place of the two columns every row of it starts with.
pub fn markdown(
    markdown: &mut Markdown,
    text: &str,
    width: usize,
    (first, rest): (Line<'static>, Line<'static>),
) -> Vec<Line<'static>> {
    let wide = first.width().max(rest.width());
    let inner = (width + 2).saturating_sub(wide).max(10);
    let mut rows = Vec::new();
    for line in text.lines() {
        rows.extend(markdown.feed(line, inner, false));
    }
    rows.extend(markdown.end(inner, false));
    let mut out = Vec::with_capacity(rows.len());
    for (at, row) in rows.into_iter().enumerate() {
        let lead = if at == 0 { &first } else { &rest };
        out.push(relead(row, lead));
    }
    out
}

/// `row` with its first two columns, the lead Markdown draws, swapped
/// for `lead`; where it says a wrapped row's lead is, moved to match.
fn relead(mut row: Line<'static>, lead: &Line<'static>) -> Line<'static> {
    let more = lead.width() as isize - 2;
    let marks = row
        .spans
        .iter()
        .take_while(|span| span.content.is_empty())
        .count();
    let mut spans: Vec<Span<'static>> = Vec::with_capacity(row.spans.len() + lead.spans.len());
    for span in row.spans.drain(..marks) {
        match mark(&Line::from(vec![span.clone()])) {
            Some((columns, joins)) => {
                spans.push(goes_on(columns.saturating_add_signed(more), joins))
            }
            None => spans.push(span),
        }
    }
    spans.extend(lead.spans.iter().cloned());
    let mut cut = 2usize;
    for mut span in row.spans.drain(..) {
        while cut > 0 && !span.content.is_empty() {
            let c = span.content.chars().next().expect("a character");
            let columns = unicode_width::UnicodeWidthChar::width(c).unwrap_or(0);
            cut = cut.saturating_sub(columns.max(1));
            span.content = span.content[c.len_utf8()..].to_owned().into();
        }
        if !span.content.is_empty() {
            spans.push(span);
        }
    }
    Line { spans, ..row }
}

/// A step on the way to the reply, a call or what the bot said before
/// one, drawn quieter than the reply: dim and never bold. What a failure
/// shows in red, and a diff's colored rows, stay as they are, to be read.
pub fn quiet(lines: Vec<Line<'static>>) -> Vec<Line<'static>> {
    let keep = |style: Style| style.bg.is_some() || style.fg == Some(Color::Red);
    lines
        .into_iter()
        .map(|mut line| {
            if keep(line.style) {
                return line;
            }
            line.style = line.style.add_modifier(Modifier::DIM);
            for span in &mut line.spans {
                if !keep(span.style) {
                    span.style = span
                        .style
                        .add_modifier(Modifier::DIM)
                        .remove_modifier(Modifier::BOLD);
                }
            }
            line
        })
        .collect()
}

/// What a call would have done, in a few words: a command's first line,
/// a file's path, else the tool's name.
pub fn action(name: &str, arguments: &Value, workspace: &str) -> String {
    match (arguments["command"].as_str(), arguments["path"].as_str()) {
        (Some(command), _) => command.lines().next().unwrap_or("").trim().to_owned(),
        (None, Some(path)) => format!("{name} {}", shown(path, workspace)),
        _ => name.to_owned(),
    }
}

/// The most of a denial's reason drawn under it.
const DENIAL_ROWS: usize = 4;

/// A call that was denied, as Codex draws one: what it would have done,
/// then why, when a reason was given. Kept in the clean view too.
pub fn denied(
    name: &str,
    arguments: &Value,
    reason: &str,
    workspace: &str,
    width: usize,
) -> Vec<Line<'static>> {
    let what = action(name, arguments, workspace);
    let mut lines = vec![Line::from(vec![
        Span::raw("✗ ").fg(Color::Red),
        Span::raw("Request "),
        Span::raw("denied").bold(),
        Span::raw(" for "),
        Span::raw(cut(&what, width.saturating_sub(22).max(8))).dim(),
    ])];
    let mut rows = Vec::new();
    for paragraph in reason.lines().filter(|line| !line.trim().is_empty()) {
        rows.extend(wrap(
            Line::from(paragraph.trim().to_owned()),
            width,
            Span::raw(if rows.is_empty() { "  └ " } else { "    " }),
            Span::raw("    "),
        ));
    }
    if rows.len() > DENIAL_ROWS {
        rows.truncate(DENIAL_ROWS);
        if let Some(last) = rows.last_mut() {
            if let Some(span) = last.spans.last_mut() {
                span.content = span.content.trim_end().to_owned().into();
            }
            last.spans.push(Span::raw(" …"));
        }
    }
    lines.extend(rows.into_iter().map(|line| line.dim()));
    lines
}

/// What a tool call did: one header line, then a short look at its result.
pub fn tool(
    name: &str,
    arguments: &Value,
    output: Option<&str>,
    failed: bool,
    workspace: &str,
    width: usize,
) -> Vec<Line<'static>> {
    drawn(name, arguments, output, failed, workspace, width, false)
}

/// What the clean view keeps of a call: the calls that move the work on,
/// said as [`tool`] says them (an agent started, messaged or waited for,
/// a question to the person, the plan, a task resolved, memory, a
/// trigger, a window opened), and nothing of the rest. A command line keeps only those of
/// its commands.
pub fn clean(
    name: &str,
    arguments: &Value,
    output: Option<&str>,
    failed: bool,
    workspace: &str,
    width: usize,
) -> Vec<Line<'static>> {
    drawn(name, arguments, output, failed, workspace, width, true)
}

/// What the clean view keeps, by what Sonny says a call did.
const KEPT: [&str; 26] = [
    "Started",
    "Messaged",
    "Steered",
    "Queued for",
    "Forked",
    "Interrupted",
    "Removed",
    "Waited for",
    "Asked you",
    "Updated the plan",
    "Cleared the plan",
    "Resolved",
    "Saved to memory",
    "Updated in memory",
    "Removed from memory",
    "Scheduled",
    "Removed the trigger",
    "Fired the trigger",
    "Added the project",
    "Moved",
    "Opened",
    "Discarded",
    "Subscribed to",
    "Unsubscribed from",
    "Opened a tab",
    "Opened a split",
];

fn kept(said: &crate::calls::Said) -> bool {
    KEPT.contains(&said.verb.as_str())
}

/// The calls that fold into one when several come in a row: those that
/// start, message or change agents and what runs for them, which a lead
/// often makes a few of at once.
const FOLDS: [&str; 21] = [
    "Started",
    "Messaged",
    "Steered",
    "Queued for",
    "Forked",
    "Interrupted",
    "Removed",
    "Discarded",
    "Resolved",
    "Saved to memory",
    "Updated in memory",
    "Removed from memory",
    "Scheduled",
    "Removed the trigger",
    "Fired the trigger",
    "Subscribed to",
    "Unsubscribed from",
    "Moved",
    "Opened",
    "Opened a tab",
    "Opened a split",
];

/// What a call that folds with others like it did, said as Sonny says it
/// (`Started`) and whom (`calc.fix-avg`): a call of one of Sonny's tools,
/// or one command Sonny knows, that did not fail.
pub fn folds(
    name: &str,
    arguments: &Value,
    output: Option<&str>,
    failed: bool,
) -> Option<(String, String)> {
    if failed {
        return None;
    }
    let said = match name {
        "shell" => crate::calls::said(arguments["command"].as_str()?, output.unwrap_or(""), false)?,
        _ => crate::calls::declared(name, arguments, false)?,
    };
    // A task's name, not the turn it started.
    let what = said
        .what
        .split(" · ")
        .next()
        .unwrap_or("")
        .trim()
        .to_owned();
    (FOLDS.contains(&said.verb.as_str()) && !what.is_empty()).then_some((said.verb, what))
}

/// Calls folded into one: what they did, said once with whom each did
/// it to, `▸` to open them; open, the same row with `▾`, and each call's
/// own rows under it.
pub fn folded(
    verb: &str,
    whats: &[String],
    calls: &[Vec<Line<'static>>],
    open: bool,
    width: usize,
) -> Vec<Line<'static>> {
    let mut names = whats.join(", ");
    if let Some(at) = names.rfind(", ") {
        names.replace_range(at..at + 2, " and ");
    }
    let line = Line::from(vec![
        Span::raw(verb.to_owned()).bold(),
        Span::raw(" "),
        Span::raw(names),
        Span::raw(match open {
            true => " ▾",
            false => " ▸",
        })
        .dim(),
    ]);
    let mut out = wrap(line, width, Span::raw("• "), Span::raw("    "));
    if open {
        out.extend(calls.iter().flatten().cloned());
    }
    out
}

fn drawn(
    name: &str,
    arguments: &Value,
    output: Option<&str>,
    failed: bool,
    workspace: &str,
    width: usize,
    clean: bool,
) -> Vec<Line<'static>> {
    let bullet = if failed {
        Span::raw("• ").fg(Color::Red)
    } else {
        Span::raw("• ")
    };
    let field = |key: &str| arguments[key].as_str().unwrap_or("").to_owned();
    let elbow = || Span::raw("  └ ").dim();
    let gap = || Span::raw("    ");
    // A tool Sonny declares, said as what it does.
    if let Some(mut said) = crate::calls::declared(name, arguments, failed) {
        if clean && !kept(&said) {
            return Vec::new();
        }
        // One Sonny answered with an error says why, so the call is not
        // read as done.
        if let Some(why) = failed.then(|| refusal(output)).flatten() {
            said.under.insert(0, Line::from(why).red());
        }
        return said_lines(said, width);
    }
    // A call Sonny knows the meaning of, said as what it does.
    if name == "shell"
        && let Some(said) = crate::calls::said(&field("command"), output.unwrap_or(""), failed)
    {
        if clean && !kept(&said) {
            return Vec::new();
        }
        return said_lines(said, width);
    }
    // Several commands in one call, some of them known: each on its own
    // line, said or shown, with what they printed together under the last.
    let parts = crate::calls::parts(&field("command"));
    if name == "shell"
        && parts.len() > 1
        && parts
            .iter()
            .any(|p| crate::calls::said(p, "", false).is_some())
    {
        let mut out = Vec::new();
        // A block's closing words say nothing by themselves.
        let parts: Vec<&String> = parts
            .iter()
            .filter(|p| {
                !matches!(
                    p.as_str(),
                    "done" | "fi" | "esac" | "then" | "else" | "do" | "{" | "}"
                ) && !assignment(p)
            })
            .collect();
        // What the agent calls printed goes back under the call that
        // printed it; the rest goes under the last command.
        let mut printed: Vec<&str> = output.unwrap_or("").lines().collect();
        fn claim(printed: &mut Vec<&str>, is: &dyn Fn(&Value) -> bool) -> Option<String> {
            let at = printed.iter().position(|line| {
                serde_json::from_str::<Value>(line.trim()).is_ok_and(|v| is(&v))
            })?;
            Some(printed.remove(at).to_owned())
        }
        let mut said: Vec<Option<crate::calls::Said>> = Vec::new();
        for part in &parts {
            let mut own = String::new();
            if let Some(first) = crate::calls::said(part, "", false) {
                if first.verb == "Listed agents" {
                    let mut rows = Vec::new();
                    while let Some(at) = printed
                        .iter()
                        .position(|l| crate::calls::listed_row(l).is_some())
                    {
                        rows.push(printed.remove(at));
                    }
                    own = rows.join("\n");
                } else if matches!(
                    first.verb.as_str(),
                    "Started" | "Messaged" | "Queued for" | "Steered"
                ) {
                    own = claim(&mut printed, &|v| {
                        v.get("handle").is_some() || v.get("error").is_some()
                    })
                    .unwrap_or_default();
                } else if first.verb == "Moved" {
                    claim(&mut printed, &|v| {
                        v.get("session").is_some() && v.get("bot").is_some()
                    });
                } else if first.verb == "Scheduled" {
                    own = claim(&mut printed, &|v| {
                        v.get("when").is_some() || v.get("error").is_some()
                    })
                    .unwrap_or_default();
                } else if first.verb.ends_with("memory") {
                    own = claim(&mut printed, &|v| {
                        v.get("saved").is_some()
                            || v.get("removed").is_some()
                            || v.get("error").is_some()
                    })
                    .unwrap_or_default();
                }
            }
            said.push(crate::calls::said(part, &own, false));
        }
        // What the known calls print says nothing their line does not.
        printed.retain(|line| {
            let line = line.trim();
            !serde_json::from_str::<Value>(line)
                .is_ok_and(|v| v.get("project").is_some() && v.get("user").is_some())
        });
        // Commands next to each other that are only run read as one, and
        // the rest of the output goes under the last of them.
        let mut groups: Vec<Result<crate::calls::Said, Vec<&str>>> = Vec::new();
        for (part, said) in parts.iter().zip(said) {
            match (said, groups.last_mut()) {
                (Some(said), _) => groups.push(Ok(said)),
                (None, Some(Err(ran))) => ran.push(part.as_str()),
                (None, _) => groups.push(Err(vec![part.as_str()])),
            }
        }
        let count = groups.len();
        let last_ran = groups.iter().rposition(Result::is_err);
        let rest = printed.join("\n");
        for (i, group) in groups.into_iter().enumerate() {
            match group {
                Ok(said) if clean && !kept(&said) => {}
                Err(_) if clean => {}
                Ok(mut said) => {
                    said.failed |= failed && i + 1 == count;
                    out.extend(said_lines(said, width));
                }
                Err(ran) => {
                    let bullet = if failed && i + 1 == count {
                        Span::raw("• ").fg(Color::Red)
                    } else {
                        Span::raw("• ")
                    };
                    let commands: Vec<&str> = ran
                        .iter()
                        .map(|part| part.lines().next().unwrap_or(""))
                        .collect();
                    let line = Line::from(vec![
                        Span::raw("Ran").bold(),
                        Span::raw(" "),
                        Span::raw(commands.join("; ")),
                    ]);
                    out.extend(wrap(line, width, bullet, gap()));
                    if Some(i) == last_ran && !rest.trim().is_empty() {
                        out.extend(preview(&rest, width));
                    }
                }
            }
        }
        if !clean && last_ran.is_none() && !rest.trim().is_empty() {
            out.extend(preview(&rest, width));
        }
        return out;
    }
    // The wait tool says what the tasks it waited for did; the rest of the
    // calls are hidden from the clean view.
    if clean && !(name == "wait" && waited(output).is_some()) {
        return Vec::new();
    }
    // A skill's instructions, read before following them.
    if name == "read"
        && let Some(skill) = field("path")
            .strip_suffix("/SKILL.md")
            .and_then(|dir| dir.rsplit_once("/skills/"))
            .map(|(_, skill)| skill.to_owned())
    {
        let line = Line::from(vec![
            Span::raw("Loaded the ").dim(),
            Span::raw(skill).dim().bold(),
            Span::raw(" skill").dim(),
        ]);
        return wrap(line, width, bullet, gap());
    }
    let (verb, what) = match name {
        "shell" => ("Ran", field("command")),
        "read" => ("Read", shown(&field("path"), workspace)),
        "write" => ("Wrote", shown(&field("path"), workspace)),
        "edit" => ("Edited", shown(&field("path"), workspace)),
        "apply_patch" => (
            "Patched",
            patch_files(&field("input"))
                .iter()
                .map(|path| shown(path, workspace))
                .collect::<Vec<_>>()
                .join(", "),
        ),
        "wait" => (
            "Waited for",
            arguments["handles"]
                .as_array()
                .into_iter()
                .flatten()
                .filter_map(Value::as_str)
                .map(crate::calls::handle_bot)
                .collect::<Vec<_>>()
                .join(", "),
        ),
        "echo" | "note" => ("Noted", field("text")),
        "exec" => ("Ran", scripted(output.unwrap_or(""))),
        _ => ("Called", format!("{name} {arguments}")),
    };
    let mut header = Line::from(vec![
        Span::raw(verb).bold(),
        Span::raw(" "),
        Span::raw(what.lines().next().unwrap_or("").to_owned()),
    ]);
    // A file tool that failed changed nothing: what it was asked to do is
    // not drawn, its error is.
    if failed && matches!(name, "edit" | "write" | "read" | "apply_patch") {
        let mut out = wrap(header, width, bullet, Span::raw("    "));
        out.extend(preview(output.unwrap_or(""), width));
        return out;
    }
    let edit = (name == "edit").then(|| diff(arguments, workspace));
    if let Some(edit) = &edit {
        let (removed, added) = edit.counts();
        header.spans.push(Span::raw("  "));
        header.spans.extend(crate::diff::counts(removed, added));
    }
    let patch = (name == "apply_patch").then(|| as_unified(&field("input"), workspace));
    if let Some(patch) = &patch {
        let (removed, added) = patch.lines().fold((0, 0), |(removed, added), line| {
            match (
                line.starts_with("--- "),
                line.starts_with("+++ "),
                line.chars().next(),
            ) {
                (false, false, Some('-')) => (removed + 1, added),
                (false, false, Some('+')) => (removed, added + 1),
                _ => (removed, added),
            }
        });
        header.spans.push(Span::raw("  "));
        header.spans.extend(crate::diff::counts(removed, added));
    }
    let mut out = wrap(header, width, bullet, Span::raw("    "));
    match name {
        "edit" => out.extend(edit.map(|e| e.lines(width, EDIT_ROWS)).unwrap_or_default()),
        "apply_patch" => out.extend(
            patch
                .and_then(|text| crate::diff::unified(&text, width, EDIT_ROWS))
                .unwrap_or_default(),
        ),
        "write" => {
            let lines = field("content").lines().count();
            out.push(Line::from(vec![
                elbow(),
                Span::raw(format!("{lines} lines")).dim(),
            ]));
        }
        "read" => {}
        // What `agent run --detach` printed: which agent, which turn.
        "shell" if started(output).is_some() => {
            let line = started(output).unwrap_or_default();
            out.push(Line::from(vec![elbow(), Span::raw(line).dim()]));
        }
        // What a wait resolved: each turn's ending and its reply's first line.
        "wait" if waited(output).is_some() => {
            for (i, line) in waited(output).unwrap_or_default().into_iter().enumerate() {
                let lead = if i == 0 { elbow() } else { gap() };
                out.extend(
                    wrap(Line::from(line).dim(), width, lead, gap())
                        .into_iter()
                        .take(2),
                );
            }
        }
        "shell" | "wait" => out.extend(preview(output.unwrap_or(""), width)),
        "exec" => {
            let output = output.unwrap_or("");
            let printed = output
                .split_once("Output:\n")
                .map_or(output, |(_, printed)| printed);
            out.extend(preview(printed, width));
        }
        _ => {}
    }
    out
}

/// An `exec` call as one line: the MCP calls its script made, by server,
/// from the head of its result, and how it ended when not well.
fn scripted(output: &str) -> String {
    let head = output.split_once("Output:\n").map_or("", |(head, _)| head);
    let mut line = match head.lines().find_map(|l| l.strip_prefix("MCP calls: ")) {
        Some(calls) => {
            let total: usize = (calls.split(", "))
                .filter_map(|each| each.rsplit_once(' ')?.1.parse::<usize>().ok())
                .sum();
            let noun = if total == 1 { "call" } else { "calls" };
            format!("a script: {total} MCP {noun} ({calls})")
        }
        None => "a script".to_owned(),
    };
    if let Some(status) = head.lines().next().filter(|l| *l != "Script completed")
        && let Some(how) = status.strip_prefix("Script ")
    {
        line.push_str(&format!(", {how}"));
    }
    line
}

/// The most rows an edit's diff, or a diff a command printed, takes.
const EDIT_ROWS: usize = 40;
const DIFF_ROWS: usize = 40;

/// A command that only sets a variable, as `result=$?`.
fn assignment(part: &str) -> bool {
    crate::shell::simple(part).is_some_and(|simple| {
        simple.words.iter().all(|word| {
            word.split_once('=').is_some_and(|(name, _)| {
                !name.is_empty() && name.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_')
            })
        })
    })
}

/// A call said as an action, with up to two rows of each line under it.
/// The first line of a failed call's `{"error","detail"}` output: its
/// detail, else its code.
fn refusal(output: Option<&str>) -> Option<String> {
    let error: Value = serde_json::from_str(output?).ok()?;
    let why = error["detail"].as_str().or(error["error"].as_str())?;
    Some(why.lines().next().unwrap_or("").to_owned())
}

fn said_lines(said: crate::calls::Said, width: usize) -> Vec<Line<'static>> {
    let bullet = if said.failed {
        Span::raw("• ").fg(Color::Red)
    } else {
        Span::raw("• ")
    };
    let mut spans = vec![Span::raw(said.verb).bold()];
    if !said.what.is_empty() {
        spans.extend([Span::raw(" "), Span::raw(said.what)]);
    }
    let mut out = wrap(Line::from(spans), width, bullet, Span::raw("    "));
    for (i, line) in said.under.into_iter().enumerate() {
        let lead = if i == 0 {
            Span::raw("  └ ").dim()
        } else {
            Span::raw("    ")
        };
        out.extend(capped(line, width, lead, 2));
    }
    // A part of a longer command line has its output shown after the last.
    if let Some(output) = said.output.filter(|o| !o.trim().is_empty()) {
        out.extend(preview(&output, width));
    }
    out
}

/// A line wrapped to at most `rows` rows, the last ending in `…` when
/// some was left out.
pub fn capped(
    line: Line<'static>,
    width: usize,
    lead: Span<'static>,
    rows: usize,
) -> Vec<Line<'static>> {
    let style = line.style;
    let mut wrapped = wrap(
        line,
        width.saturating_sub(1).max(8),
        lead,
        Span::raw("    "),
    );
    if wrapped.len() > rows {
        wrapped.truncate(rows);
        if let Some(last) = wrapped.last_mut() {
            last.spans.push(Span::styled("…", style));
        }
    }
    wrapped
}

/// A command's output: its first lines, or all of a few, and how many
/// more there were. An agent's start, printed among them, reads as one.
fn preview(output: &str, width: usize) -> Vec<Line<'static>> {
    if let Some(diff) = crate::diff::unified(output, width, DIFF_ROWS) {
        return diff;
    }
    let elbow = || Span::raw("  └ ").dim();
    let gap = || Span::raw("    ");
    let mut out = Vec::new();
    let lines: Vec<&str> = output.lines().filter(|l| !l.trim().is_empty()).collect();
    if lines.is_empty() {
        out.push(Line::from(vec![elbow(), Span::raw("(no output)").dim()]));
    }
    let shown = if lines.len() > 5 { 3 } else { lines.len() };
    for (i, line) in lines.iter().take(shown).enumerate() {
        let lead = if i == 0 { elbow() } else { gap() };
        let line = Line::from(started(Some(line)).unwrap_or_else(|| line.to_string())).dim();
        out.extend(wrap(line, width, lead, gap()).into_iter().take(2));
    }
    if lines.len() > shown {
        out.push(Line::from(vec![
            gap(),
            Span::raw(format!("… +{} lines", lines.len() - shown)).dim(),
        ]));
    }
    out
}

/// `agent run --detach`'s reply, as a line: the agent and its turn.
fn started(output: Option<&str>) -> Option<String> {
    let reply: Value = serde_json::from_str(output?.trim()).ok()?;
    // A refusal, as the CLI writes it: its code and what it says.
    if let Some(error) = reply["error"].as_str() {
        return Some(match reply["detail"].as_str().filter(|d| !d.is_empty()) {
            Some(detail) => format!("refused: {detail} ({error})"),
            None => format!("refused: {}", error.replace('_', " ")),
        });
    }
    let bot = reply["bot"].as_str()?;
    let turn = reply["turn"].as_i64()?;
    reply["handle"].as_str()?;
    Some(format!(
        "{} {bot}, turn {turn}",
        if reply["status"] == "queued" {
            "queued for"
        } else {
            "started"
        }
    ))
}

/// The wait tool's reply as lines: `BOT completed: first line of its reply`.
fn waited(output: Option<&str>) -> Option<Vec<String>> {
    crate::calls::waited(&serde_json::from_str(output?.trim()).ok()?)
}

/// The text of the file at `path` as it is now, when it is a regular file
/// no larger than the daemon writes: never a FIFO or a device, whose
/// read could block the screen.
fn current(path: &std::path::Path) -> Option<String> {
    let metadata = std::fs::metadata(path).ok()?;
    if !metadata.is_file() || metadata.len() > 4 << 20 {
        return None;
    }
    std::fs::read_to_string(path).ok()
}

/// An edit as diffs.com draws one: the whole lines it touched, the words
/// that changed within them, and two lines either side, numbered where
/// the file now has them when the new text can be found there.
fn diff(arguments: &Value, workspace: &str) -> crate::diff::Edit {
    let (mut old, mut new) = (
        arguments["old"].as_str().unwrap_or("").to_owned(),
        arguments["new"].as_str().unwrap_or("").to_owned(),
    );
    let path = arguments["path"].as_str().unwrap_or("");
    let path = std::path::Path::new(workspace).join(path);
    let (mut start, mut before, mut after) = (None, Vec::new(), Vec::new());
    // An edit replaces text, not lines: show the whole lines it touched,
    // read from the file as it is now, so indentation and numbers are right.
    if !new.is_empty()
        && let Some(now) = current(&path)
        && let Some(at) = now.find(&new)
    {
        let line_start = now[..at].rfind('\n').map_or(0, |i| i + 1);
        let end = at + new.len();
        let line_end = now[end..].find('\n').map_or(now.len(), |i| end + i);
        old = format!("{}{old}{}", &now[line_start..at], &now[end..line_end]);
        new = now[line_start..line_end].to_owned();
        let lines: Vec<&str> = now.lines().collect();
        let first = now[..at].matches('\n').count();
        let last = first + new.lines().count().max(1);
        before = lines[first.saturating_sub(2)..first]
            .iter()
            .map(|l| l.to_string())
            .collect();
        after = lines[last.min(lines.len())..(last + 2).min(lines.len())]
            .iter()
            .map(|l| l.to_string())
            .collect();
        // Blank lines at the edges say nothing and take rows.
        while after.last().is_some_and(|l: &String| l.trim().is_empty()) {
            after.pop();
        }
        while before.first().is_some_and(|l: &String| l.trim().is_empty()) {
            before.remove(0);
        }
        start = Some(first + 1);
    }
    crate::diff::Edit {
        old,
        new,
        start,
        before,
        after,
    }
}

/// The files a patch changes, each once: where an update moves a file,
/// the file it becomes.
fn patch_files(patch: &str) -> Vec<String> {
    let mut files: Vec<String> = Vec::new();
    for line in patch.lines() {
        let named = ["*** Add File: ", "*** Delete File: ", "*** Update File: "]
            .iter()
            .find_map(|marker| line.strip_prefix(marker));
        if let Some(path) = named {
            files.push(path.trim().to_owned());
        } else if let Some(path) = line.strip_prefix("*** Move to: ")
            && let Some(last) = files.last_mut()
        {
            *last = path.trim().to_owned();
        }
    }
    files.dedup();
    files
}

/// A patch (Codex's format) as a unified diff, to draw as one. Its chunks
/// carry no line numbers: each is placed where its new side is in the
/// file as it is now, after the patch; a deleted file shows no lines.
fn as_unified(patch: &str, workspace: &str) -> String {
    let mut out = String::new();
    let mut lines = patch.lines().peekable();
    let ends = |line: &str| line.starts_with("*** ") && line != "*** End of File";
    while let Some(line) = lines.next() {
        if let Some(path) = line.strip_prefix("*** Add File: ") {
            let added: Vec<&str> = std::iter::from_fn(|| lines.next_if(|l| !ends(l)))
                .map(|l| l.strip_prefix('+').unwrap_or(l))
                .collect();
            out.push_str(&format!("--- /dev/null\n+++ {}\n", path.trim()));
            out.push_str(&format!("@@ -0,0 +1,{} @@\n", added.len()));
            for line in added {
                out.push_str(&format!("+{line}\n"));
            }
        } else if let Some(path) = line.strip_prefix("*** Delete File: ") {
            out.push_str(&format!(
                "--- {}\n+++ /dev/null\n@@ -1,0 +0,0 @@\n",
                path.trim()
            ));
        } else if let Some(path) = line.strip_prefix("*** Update File: ") {
            let mut path = path.trim().to_owned();
            if let Some(to) = lines.next_if(|l| l.starts_with("*** Move to: ")) {
                path = to["*** Move to: ".len()..].trim().to_owned();
            }
            let now = current(&std::path::Path::new(workspace).join(&path)).unwrap_or_default();
            let now: Vec<&str> = now.lines().collect();
            out.push_str(&format!("--- {path}\n+++ {path}\n"));
            // Each chunk's lines, then its place: the line where its new
            // side starts now, and that less what earlier chunks added.
            let mut chunks: Vec<Vec<String>> = Vec::new();
            while let Some(line) = lines.next_if(|l| !ends(l)) {
                if line.starts_with("@@") || chunks.is_empty() {
                    chunks.push(Vec::new());
                }
                if line.starts_with("@@") || line == "*** End of File" {
                    continue;
                }
                // A line with no change mark, as in a patch the daemon
                // refused, is drawn as context.
                let line = match line.chars().next() {
                    Some('+' | '-' | ' ') => line.to_owned(),
                    _ => format!(" {line}"),
                };
                if let Some(chunk) = chunks.last_mut() {
                    chunk.push(line);
                }
            }
            let (mut grown, mut from, mut budget) = (0isize, 0, PLACING);
            for chunk in chunks.into_iter().filter(|c| !c.is_empty()) {
                let new: Vec<&str> = chunk
                    .iter()
                    .filter(|l| !l.starts_with('-'))
                    .map(|l| &l[1..])
                    .collect();
                let old = chunk.iter().filter(|l| !l.starts_with('+')).count();
                let start = find_lines(&now, &new, from, &mut budget).unwrap_or(from);
                from = start + new.len();
                let old_start = (start as isize - grown).max(0) as usize;
                grown += new.len() as isize - old as isize;
                out.push_str(&format!(
                    "@@ -{},{old} +{},{} @@\n",
                    old_start + 1,
                    start + 1,
                    new.len()
                ));
                for line in chunk {
                    out.push_str(&line);
                    out.push('\n');
                }
            }
        }
    }
    out
}

/// Line comparisons placing one file's chunks may make. Drawing waits on
/// them, so past this the rest are placed where they would otherwise be.
const PLACING: usize = 1 << 20;

/// The first line, at or after `from`, where `lines` holds `wanted` in a
/// row, by content with ends trimmed. Each comparison comes out of
/// `budget`, and once it runs out nothing more is found.
fn find_lines(lines: &[&str], wanted: &[&str], from: usize, budget: &mut usize) -> Option<usize> {
    if wanted.is_empty() || wanted.len() > lines.len() {
        return None;
    }
    let last = wanted.len() - 1;
    (from..=lines.len() - wanted.len()).find_map(|at| {
        if *budget == 0 {
            return Some(None);
        }
        *budget -= 1;
        // The last line first: a long run rarely ends the same by chance.
        if wanted[last].trim_end() != lines[at + last].trim_end() {
            return None;
        }
        let same = wanted.iter().zip(&lines[at..]).take_while(|(want, have)| {
            *budget = budget.saturating_sub(1);
            want.trim_end() == have.trim_end()
        });
        (same.count() == wanted.len()).then_some(Some(at))
    })?
}

/// The rule under a finished turn: how it ended, how long it took, what it cost.
/// A path as the person would say it: inside the bot's folder, from
/// there (macOS's /tmp is /private/tmp, either way round); else from ~.
fn shown(path: &str, workspace: &str) -> String {
    let bare = |p: &str| p.strip_prefix("/private").unwrap_or(p).to_owned();
    let (path, workspace) = (bare(path), bare(workspace.trim_end_matches('/')));
    if !workspace.is_empty()
        && let Some(inside) = path.strip_prefix(&format!("{workspace}/"))
    {
        return inside.to_owned();
    }
    match std::env::var("HOME") {
        Ok(home) if !home.is_empty() && path.starts_with(&format!("{home}/")) => {
            format!("~{}", &path[home.len()..])
        }
        _ => path,
    }
}

pub fn finished(status: &str, detail: &str, seconds: Option<u64>, width: usize) -> Line<'static> {
    let mut label = match status {
        "completed" => "Worked".to_owned(),
        "steered" => "Steered".to_owned(),
        other => other.to_owned(),
    };
    if let Some(seconds) = seconds {
        label.push_str(&format!(" for {}", elapsed(seconds)));
    }
    if !detail.is_empty() {
        label.push_str(&format!(" · {detail}"));
    }
    let text = format!("─ {label} ");
    // No wider than 80 columns, so a pane that narrows later rarely re-wraps it.
    let fill = width.min(80).saturating_sub(text.width());
    let style = if matches!(status, "completed" | "steered") {
        Style::new().dim()
    } else {
        Style::new().fg(Color::Red)
    };
    Line::from(Span::styled(format!("{text}{}", "─".repeat(fill)), style))
}

pub fn tokens(n: u64) -> String {
    if n >= 1000 {
        format!("{:.1}k", n as f64 / 1000.0)
    } else {
        n.to_string()
    }
}

/// Codex's compact clock: 9s, 1m 05s, 1h 02m 03s.
pub fn elapsed(seconds: u64) -> String {
    match seconds {
        0..60 => format!("{seconds}s"),
        60..3600 => format!("{}m {:02}s", seconds / 60, seconds % 60),
        _ => format!(
            "{}h {:02}m {:02}s",
            seconds / 3600,
            (seconds % 3600) / 60,
            seconds % 60
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn text(lines: &[Line]) -> Vec<String> {
        lines.iter().map(|l| l.to_string()).collect()
    }

    #[test]
    fn a_line_of_pictures_is_read_as_markdown_and_html_write_them() {
        let picture = |alt: &str, src: &str| Picture {
            alt: alt.into(),
            src: src.into(),
        };
        assert_eq!(
            pictures("![The demo](docs/demo.gif \"title\")"),
            Some(vec![picture("The demo", "docs/demo.gif")])
        );
        // Badges: pictures that are links.
        assert_eq!(
            pictures("[![CI](https://x/ci.svg)](https://x/ci) [![Docs](d.svg)](https://d)"),
            Some(vec![
                picture("CI", "https://x/ci.svg"),
                picture("Docs", "d.svg")
            ])
        );
        assert_eq!(
            pictures(r#"<p align="center"><img src='a b.png' width=200 alt="A logo"/></p>"#),
            Some(vec![picture("A logo", "a b.png")])
        );
        assert_eq!(
            pictures("  <img width=\"800\" src=\"d.gif\">"),
            Some(vec![picture("", "d.gif")])
        );
        assert_eq!(pictures("See ![a](a.png) here"), None);
        assert_eq!(pictures("<p align=\"center\">"), None);
        assert_eq!(pictures("Vec<String> is ![a](a.png)"), None);
    }

    #[test]
    fn a_picture_not_drawn_reads_as_its_words_and_where_it_is() {
        let dir = std::env::temp_dir().join(format!("sonny-pictures-md-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("shot.png"), b"not drawn in tests").unwrap();
        std::fs::write(dir.join("a b.png"), b"not drawn in tests").unwrap();
        let mut markdown = Markdown::document(&dir);
        let rows = markdown.feed(
            "![A shot](shot.png) [![CI](https://x/ci.svg)](https://x)",
            80,
            false,
        );
        // A file here keeps its path, to be clicked; another's only its words.
        assert_eq!(text(&rows), vec!["  ▣ A shot (shot.png)  ▣ CI"]);
        // An escaped name reads back as the file's.
        let rows = markdown.feed("![Spaced](a%20b.png)", 80, false);
        assert_eq!(text(&rows), vec!["  ▣ Spaced (a%20b.png)"]);
        // Among words, a picture is a link to it.
        let rows = markdown.feed("Look: ![plot](out/plot.png).", 80, false);
        assert_eq!(text(&rows), vec!["  Look: ▣ plot (out/plot.png)."]);
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn html_in_markdown_reads_as_the_page_would_show_it() {
        let shown = |source: &str| {
            let mut markdown = Markdown::document(std::path::Path::new("."));
            let mut rows = Vec::new();
            for line in source.lines() {
                rows.extend(markdown.feed(line, 80, false));
            }
            rows.extend(markdown.end(80, false));
            text(&rows)
        };
        assert_eq!(
            shown("<p align=\"center\">\n  <b>Agent</b>\n</p>\n<h1 align=\"center\">Title</h1>"),
            vec!["    Agent", "  Title"]
        );
        assert_eq!(
            shown(
                "<details>\n<summary>More</summary>\n\nHidden <a href=\"https://x\">here</a>.\n</details>"
            ),
            vec!["  ▸ More", "  ", "  Hidden here (https://x)."]
        );
        // Comments say nothing, on one line or over several.
        assert_eq!(
            shown("a <!-- note --> b\n<!--\nlong\n-->\nc\n<hr>"),
            vec!["  a  b", "  c", &format!("  {}", "─".repeat(40))]
        );
        // What is not an element stays as written.
        assert_eq!(shown("Vec<String> and a<b"), vec!["  Vec<String> and a<b"]);
        // In a document, an html fence is code to read.
        assert_eq!(shown("```html\n<p>x</p>\n```"), vec!["    <p>x</p>"]);
    }

    #[test]
    fn a_step_is_dim_and_plain_but_a_failure_stays_red() {
        let ran = super::quiet(super::tool(
            "shell",
            &serde_json::json!({"command": "false"}),
            Some("boom"),
            true,
            "/w",
            80,
        ));
        let header = &ran[0];
        // What draws: a wrapped row opens with an empty mark for copies.
        let spans: Vec<_> = header
            .spans
            .iter()
            .filter(|s| !s.content.is_empty())
            .collect();
        let bullet = spans[0].style;
        assert_eq!(
            (bullet.fg, bullet.add_modifier),
            (Some(Color::Red), Modifier::empty())
        );
        for span in &spans[1..] {
            let style = header.style.patch(span.style);
            assert!(style.add_modifier.contains(Modifier::DIM), "{span:?}");
            assert!(!style.add_modifier.contains(Modifier::BOLD), "{span:?}");
        }
    }

    #[test]
    fn an_answer_shows_itself_under_the_question_it_answers() {
        let shown: Vec<String> = prompt("You asked: Ship it?\n\nNot yet", None, 80)
            .iter()
            .map(|row| row.to_string())
            .collect();
        assert_eq!(shown, ["  answering “Ship it?”", "› Not yet"]);
    }

    #[test]
    fn task_updates_and_lookout_notes_read_as_one_quiet_row() {
        let told = "Task updates: since you last heard, tasks you started ended turns or wait for an approval. The wait tool reads each one's final reply by its handle.\n\
                    - shop.fix: turn:shop.fix/5 failed, asked by you\n\
                    The person asked for these turns in the task themselves, so they are theirs:\n\
                    - shop.docs: turn:shop.docs/2 completed, asked by the person\n\
                    - 2 more tasks in the next update\n\n\
                    Memory changed since you last heard; each fact is a file, read before relying on it:\n\
                    - saved: /m/flow.md: Reviews go through gft";
        assert_eq!(
            text(&super::prompt(told, None, 90)),
            ["↳ Task updates · shop.fix failed · shop.docs completed · +2 more · memory changed"]
        );
        assert_eq!(
            text(&super::prompt(told, None, 40)),
            ["↳ Task updates · shop.fix failed · shop…"]
        );
        let note = "Task updates: what happened in your tasks since you last heard; the wait tool reads each turn by its handle.\n\
             Picked by your lookout:\n\
             - shop.fix: turn:shop.fix/5, turn failed (interrupt 0.91; it cannot wait for your turn to end)\n\
             In your tasks:\n\
             - shop.docs: turn:shop.docs/2, the person wrote: \"Use tabs.\"\n\
             - shop.api: turn:shop.api/4, shop.fix wrote in it\n\
             - shop.web: turn:shop.web/3, 2 messages in it since turn:shop.web/1, from shop.fix, a trigger\n\
             - 1 more in the next note\n\
             A quoted message is the person's own words.";
        assert_eq!(
            text(&super::prompt(note, None, 160)),
            [
                "↳ Task updates · shop.fix turn failed · you wrote in shop.docs · shop.fix wrote in shop.api · 2 messages in shop.web · +1 more"
            ]
        );
    }

    #[test]
    fn a_trigger_and_a_memory_save_read_as_what_they_did() {
        let command = r#"git worktree list; "$HOME/.agent/trigger" add --name watch --every 10m -- 'Check the tasks, then report'; "$HOME/.agent/memory" save flow --type project --description d --source s -- 'text'"#;
        let output = "/repo d7e2b9e [main]\n\
            {\"name\":\"watch\",\"when\":\"every 10m\"}\n\
            {\"duplicate\":false,\"saved\":\"flow\",\"replaced\":false,\"path\":\"/m/flow.md\"}\n";
        let shown = text(&tool(
            "shell",
            &serde_json::json!({"command": command}),
            Some(output),
            false,
            "/repo",
            100,
        ));
        assert_eq!(
            shown,
            [
                "• Ran git worktree list",
                "  └ /repo d7e2b9e [main]",
                "• Scheduled watch · every 10m",
                "  └ Check the tasks, then report",
                "• Saved to memory flow",
            ],
        );
    }

    #[test]
    fn a_fence_left_open_shows_what_it_had() {
        let lines = reply("Here:\n```diff\n-old\n+new", 80, std::path::Path::new("."));
        let shown: Vec<String> = lines.iter().map(|l| l.to_string()).collect();
        assert!(shown.iter().any(|l| l.contains("-old")), "{shown:?}");
        assert!(shown.iter().any(|l| l.contains("+new")), "{shown:?}");
    }

    #[test]
    fn wrapping_keeps_every_word_and_the_hanging_indent() {
        let line = Line::from("one two three four five six");
        let rows = wrap(line, 12, Span::raw("• "), Span::raw("  "));
        assert_eq!(text(&rows), ["• one two ", "  three four ", "  five six"]);
    }

    #[test]
    fn a_reply_styles_fences_headings_and_code_spans() {
        let rows = reply(
            "Hi `x`\n## Head\n```rust\nlet a = 1;\n```\n- item",
            40,
            std::path::Path::new("."),
        );
        assert_eq!(
            text(&rows),
            ["• Hi x", "  Head", "    let a = 1;", "  - item"]
        );
    }

    #[test]
    fn a_fence_that_never_closes_still_shows_as_code() {
        let rows = reply("Here:\n```diff\n-a\n+b", 40, std::path::Path::new("."));
        assert_eq!(text(&rows), ["• Here:", "    -a", "    +b"]);
        let rows = reply("$$\nx^2", 40, std::path::Path::new("."));
        assert_eq!(text(&rows), ["•   x^2"]);
    }

    #[test]
    fn markdown_reads_the_same_after_any_lead() {
        let folder = std::path::Path::new(".");
        let said = "Pick **one** of the `two` that still pass\n- a list";
        let leads = (Line::from("? "), Line::from("     "));
        let rows = markdown(&mut Markdown::in_folder(folder), said, 24, leads);
        assert_eq!(
            text(&rows),
            [
                "? Pick one of the two ",
                "     that still pass",
                "     - a list"
            ]
        );
        // Where a wrapped row's lead ends moves with the lead, so the rows
        // join again as the line they were.
        assert_eq!(lead(&rows[1]), Some(5));
        // A reply is Markdown after a bullet.
        let reply = reply(said, 24, folder);
        let bullet = (Line::from("• "), Line::from("  "));
        let same = markdown(&mut Markdown::in_folder(folder), said, 24, bullet);
        assert_eq!(reply, same);
    }

    #[test]
    fn math_in_brackets_reads_as_math_in_dollars_does() {
        let folder = std::path::Path::new(".");
        let dollars = reply("$$\nx^2\n$$\nafter", 40, folder);
        let brackets = reply("\\[\nx^2\n\\]\nafter", 40, folder);
        assert_eq!(text(&brackets), text(&dollars));
        assert_eq!(
            text(&reply("\\[ x^2 \\]", 40, folder)),
            text(&reply("$$ x^2 $$", 40, folder))
        );
        assert_eq!(
            text(&reply("so \\(x^2\\) holds", 40, folder)),
            text(&reply("so $x^2$ holds", 40, folder))
        );
        // An escaped bracket that runs on is words.
        assert_eq!(text(&reply("\\[not math", 40, folder)), ["• \\[not math"]);
    }

    #[test]
    fn a_long_shell_output_shows_its_head_and_the_count_left() {
        let output = (1..=9)
            .map(|n| format!("line {n}"))
            .collect::<Vec<_>>()
            .join("\n");
        let rows = tool(
            "shell",
            &serde_json::json!({"command":"ls"}),
            Some(&output),
            false,
            "/",
            40,
        );
        assert_eq!(
            text(&rows),
            [
                "• Ran ls",
                "  └ line 1",
                "    line 2",
                "    line 3",
                "    … +6 lines"
            ]
        );
    }

    #[test]
    fn a_wrapped_list_item_hangs_under_its_text() {
        let rows = reply(
            "- divide by len(values), not len(values) - 1",
            30,
            std::path::Path::new("."),
        );
        assert_eq!(
            text(&rows),
            ["• - divide by len(values), not ", "    len(values) - 1"]
        );
    }

    #[test]
    fn a_command_line_says_each_known_command() {
        let output = r#"{"bot":"calc.fix","bot_id":2,"handle":"turn:calc.fix/2","status":"running","turn":2}"#;
        let rows = tool(
            "shell",
            &serde_json::json!({"command":"cd /tmp && agent run --detach --bot calc.fix -- 'Go'"}),
            Some(output),
            false,
            "/",
            60,
        );
        assert_eq!(
            text(&rows),
            ["• Ran cd /tmp", "• Messaged calc.fix · turn 2", "  └ Go"]
        );
    }

    #[test]
    fn an_edit_shows_the_whole_lines_it_touched_at_their_numbers() {
        let dir = std::env::temp_dir().join(format!("sonny-diff-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(
            dir.join("calc.py"),
            "def average(values):\n    return sum(values) / len(values)\n\nprint(average([2, 4]))\nx = 1\ny = 2\n",
        )
        .unwrap();
        let edit = serde_json::json!({"path": "calc.py", "old": "return sum(values) / (len(values) - 1)",
            "new": "return sum(values) / len(values)"});
        let rows = tool("edit", &edit, None, false, dir.to_str().unwrap(), 80);
        assert_eq!(
            text(&rows),
            [
                "• Edited calc.py  -1 +1",
                "   1   def average(values):",
                "   2 ▎     return sum(values) / (len(values) - 1)",
                "   2 ▎     return sum(values) / len(values)",
                "   3   ",
                "   4   print(average([2, 4]))",
            ]
        );
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn a_long_repeated_context_is_placed_quickly() {
        let mut lines = vec!["x"; 100_000];
        lines.push("y");
        let mut wanted = vec!["x"; 10_000];
        wanted.push("y");
        let started = std::time::Instant::now();
        let found = find_lines(&lines, &wanted, 0, &mut { PLACING });
        assert!(started.elapsed() < std::time::Duration::from_secs(2));
        assert!(found.is_none_or(|at| at == 90_000), "{found:?}");
        let find = |wanted: &[&str]| find_lines(&["a", "b", "c"], wanted, 0, &mut { PLACING });
        assert_eq!(find(&["b", "c"]), Some(1));
        assert_eq!(find(&["c", "b"]), None);
    }

    #[test]
    fn many_chunks_against_a_long_file_are_drawn_quickly() {
        let dir = std::env::temp_dir().join(format!("sonny-chunks-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("f"), "x\n".repeat(100_000)).unwrap();
        let patch = format!(
            "*** Begin Patch\n*** Update File: f\n{}*** End Patch\n",
            "@@\n-x\n+x\n".repeat(5_000)
        );
        let started = std::time::Instant::now();
        let unified = as_unified(&patch, dir.to_str().unwrap());
        assert!(started.elapsed() < std::time::Duration::from_secs(2));
        assert_eq!(unified.matches("@@ -").count(), 5_000);
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn a_file_that_could_block_is_not_read_for_a_diff() {
        let dir = std::env::temp_dir().join(format!("sonny-fifo-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let fifo = dir.join("pipe");
        let made = std::process::Command::new("mkfifo")
            .arg(&fifo)
            .status()
            .unwrap();
        assert!(made.success());
        assert_eq!(current(&fifo), None);
        assert_eq!(current(&dir), None);
        std::fs::write(dir.join("f"), "a\n").unwrap();
        assert_eq!(current(&dir.join("f")).as_deref(), Some("a\n"));
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn a_script_is_one_line_of_its_mcp_calls_over_what_it_printed() {
        let input = serde_json::json!({"input": "await tools.mcp__github__a({});"});
        let output = "Script completed\nWall time 1.2 seconds\nMCP calls: github 10, linear 2\nOutput:\n3 open issues";
        let rows = text(&tool("exec", &input, Some(output), false, "/", 80));
        assert_eq!(
            rows[0],
            "• Ran a script: 12 MCP calls (github 10, linear 2)"
        );
        assert!(rows[1].contains("3 open issues"), "{rows:#?}");
        let failed = "Script terminated: it ran out of time\nWall time 600.0 seconds\nOutput:\n";
        let rows = text(&tool("exec", &input, Some(failed), true, "/", 80));
        assert_eq!(rows[0], "• Ran a script, terminated: it ran out of time");
    }

    #[test]
    fn a_refused_patch_with_an_unmarked_line_still_draws() {
        let unified = as_unified(
            "*** Begin Patch\n*** Update File: f\n@@\nα\n-a\n+b\n*** End Patch",
            "/nonexistent",
        );
        assert!(unified.contains("\n α\n-a\n+b\n"), "{unified}");
    }

    #[test]
    fn a_patch_shows_each_file_as_a_diff_at_its_lines() {
        let dir = std::env::temp_dir().join(format!("sonny-patch-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(
            dir.join("calc.py"),
            "def average(values):\n    return sum(values) / len(values)\n\nprint(average([2, 4]))\n",
        )
        .unwrap();
        let patch = "*** Begin Patch\n*** Update File: calc.py\n@@ def average(values):\n-    return sum(values) / (len(values) - 1)\n+    return sum(values) / len(values)\n \n*** Add File: notes.md\n+# Notes\n*** Delete File: old.py\n*** End Patch";
        let rows = tool(
            "apply_patch",
            &serde_json::json!({"input": patch}),
            None,
            false,
            dir.to_str().unwrap(),
            80,
        );
        let rows = text(&rows);
        assert_eq!(rows[0], "• Patched calc.py, notes.md, old.py  -1 +2");
        assert!(
            rows.contains(&"   2 ▎     return sum(values) / (len(values) - 1)".to_owned()),
            "{rows:#?}"
        );
        assert!(
            rows.contains(&"   2 ▎     return sum(values) / len(values)".to_owned()),
            "{rows:#?}"
        );
        assert!(
            rows.iter()
                .any(|row| row.contains("notes.md") && row.contains("+1")),
            "{rows:#?}"
        );
        assert_eq!(rows.last().unwrap(), "    old.py  deleted");
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn a_wait_reads_as_each_turns_ending() {
        let output = r#"{"pending":[],"results":{"turn:c.fix/2":{"bot":"c.fix","status":"completed","text":"\nFixed it.\nMore."}}}"#;
        let rows = tool(
            "wait",
            &serde_json::json!({"handles":["turn:c.fix/2"]}),
            Some(output),
            false,
            "/",
            60,
        );
        assert_eq!(text(&rows)[1], "  └ c.fix completed: Fixed it.");
    }

    #[test]
    fn a_failed_edit_shows_its_error_not_a_diff() {
        let rows: Vec<String> = tool(
            "edit",
            &serde_json::json!({"path": "/w/a.rs", "old": "x", "new": "y"}),
            Some("edit_target_not_found"),
            true,
            "/w",
            80,
        )
        .iter()
        .map(|line| line.spans.iter().map(|s| s.content.as_ref()).collect())
        .collect();
        assert_eq!(rows.len(), 2, "{rows:?}");
        assert!(rows[0].contains("Edited a.rs") && !rows[0].contains('+'));
        assert!(rows[1].contains("edit_target_not_found"));
    }

    #[test]
    fn the_clean_view_keeps_the_plan_and_tasks_but_not_commands() {
        let clean = |name: &str, arguments: Value, output: &str| {
            text(&super::clean(
                name,
                &arguments,
                Some(output),
                false,
                "/w",
                80,
            ))
        };
        let shell = |command: &str, output: &str| {
            clean("shell", serde_json::json!({"command": command}), output)
        };
        assert!(shell("cargo test", "ok").is_empty());
        assert!(shell(r#""$AGENT_BIN" ls --pretty"#, "calc.lead  running").is_empty());
        assert!(clean("read", serde_json::json!({"path": "/w/a.rs"}), "fn a() {}").is_empty());
        assert!(
            clean(
                "edit",
                serde_json::json!({"path": "/w/a.rs", "old": "a", "new": "b"}),
                ""
            )
            .is_empty()
        );
        let plan = clean(
            "plan",
            serde_json::json!({"steps": ["[x] Read the tests", "[>] Fix them"]}),
            "Plan set.",
        );
        assert_eq!(
            plan,
            [
                "• Updated the plan",
                "  └ ✔ Read the tests",
                "    → Fix them"
            ]
        );
        let asked = clean(
            "ask",
            serde_json::json!({"question": "Ship it?", "options": [
                {"label": "yes", "explanation": "e"}, {"label": "no", "explanation": "e"}]}),
            "Asked.",
        );
        assert_eq!(asked, ["• Asked you Ship it?", "  └ 1 yes · 2 no"]);
        let started = shell(
            r#""$AGENT_BIN" run --detach --new --bot calc.fix -- 'Fix the tests.'"#,
            r#"{"bot":"calc.fix","handle":"turn:calc.fix/2","turn":2}"#,
        );
        assert_eq!(started[0], "• Started calc.fix · turn 2");
        // How a lead in Sonny starts a task.
        let task = clean(
            "start_task",
            serde_json::json!({"name": "fix", "brief": "Fix the tests.", "setup": true}),
            "Started calc.fix: {\"handle\":\"turn:calc.fix/3\"}",
        );
        assert_eq!(task, ["• Started fix", "  └ Fix the tests."]);
        // One that failed says why first.
        let shown = text(&super::clean(
            "show_agent",
            &serde_json::json!({"agent": "calc.fix", "split": "right"}),
            Some(r#"{"error":"not_shown","detail":"neither rex nor herdr was found"}"#),
            true,
            "/w",
            80,
        ));
        assert_eq!(
            shown,
            [
                "• Moved calc.fix to a split right of you",
                "  └ neither rex nor herdr was found"
            ]
        );
        // Memory and triggers through their tools read as their scripts do.
        let saved = clean(
            "memory",
            serde_json::json!({"save": {"name": "deploys", "type": "feedback",
                "description": "Deploys wait for review.", "source": "s", "text": "t"}}),
            "{}",
        );
        assert_eq!(
            saved,
            ["• Saved to memory deploys", "  └ Deploys wait for review."]
        );
        assert!(clean("memory", serde_json::json!({}), "{}").is_empty());
        let scheduled = text(&super::clean(
            "triggers",
            &serde_json::json!({"add": {"every": "30m", "message": "Check the queue."}}),
            Some(r#"{"error":"trigger_refused","detail":"triggers_unsupported: only macOS"}"#),
            true,
            "/w",
            80,
        ));
        assert_eq!(
            scheduled,
            [
                "• Scheduled every 30m",
                "  └ triggers_unsupported: only macOS",
                "    Check the queue."
            ]
        );
    }

    #[test]
    fn the_clock_reads_like_codex() {
        assert_eq!(elapsed(9), "9s");
        assert_eq!(elapsed(65), "1m 05s");
        assert_eq!(elapsed(3723), "1h 02m 03s");
    }

    #[test]
    fn only_emoji_are_a_reaction() {
        for said in ["👍", " 🎉 🚀\n", "❤️", "👍🏽", "👨‍👩‍👧‍👦", "✅"]
        {
            assert_eq!(reaction(said), Some(said.trim()), "{said:?}");
        }
        for said in [
            "",
            "ok",
            "👍 thanks",
            "1",
            "→",
            "👍👍👍👍👍👍👍👍👍👍👍👍👍",
        ] {
            assert_eq!(reaction(said), None, "{said:?}");
        }
        assert_eq!(shortcodes(":+1:").as_deref(), Some("👍"));
        assert_eq!(shortcodes("+1").as_deref(), Some("👍"));
        assert_eq!(shortcodes(":tada: :eyes:").as_deref(), Some("🎉 👀"));
        for typed in ["tada", ":nope:", "+1 thanks", "100", ""] {
            assert_eq!(shortcodes(typed), None, "{typed:?}");
        }
        assert_eq!(reacted("👍").to_string(), "   👍 ");
    }
}

#[cfg(test)]
mod real_calls {
    use super::*;

    /// No agent call is left showing as the shell command it was.
    fn show(command: &str, stdout: &str, failed: bool) {
        let rows = tool(
            "shell",
            &serde_json::json!({"command": command}),
            Some(stdout),
            failed,
            "/",
            72,
        );
        for row in rows {
            let text: String = row.spans.iter().map(|s| s.content.as_ref()).collect();
            assert!(!text.contains("AGENT_BIN"), "{command}\n  drew: {text}");
        }
    }

    /// The calls the lead and its tasks made in the first live runs.
    #[test]
    fn every_agent_call_from_a_real_run_reads_as_an_action() {
        let effort = r#"${AGENT_EFFORT:+--effort "$AGENT_EFFORT"}"#;
        show(
            &format!(
                r#""$AGENT_BIN" run --detach --new --agents --model "$AGENT_MODEL" {effort} --bot calc.fix-tests --workspace "$PWD" -- 'The person says tests in this folder fail and asks you to fix them. This folder is not a git repository.'"#
            ),
            r#"{"bot":"calc.fix-tests","bot_id":2,"cursor":33,"duplicate":false,"handle":"turn:calc.fix-tests/2","request_id":"r","status":"running","turn":2}"#,
            false,
        );
        show(
            r#""$AGENT_BIN" ls --pretty | head -100"#,
            "calc.lead                running     chatgpt/gpt-6-sol  /private/tmp/sonny-live/calc\n",
            false,
        );
        show(
            &format!(
                r#""$AGENT_BIN" ls --pretty; cat test_calc.py; "$AGENT_BIN" run --detach --delivery queue --model "$AGENT_MODEL" {effort} --bot calc.fix-tests -- 'Please add an explicit test for average of a single number.'"#
            ),
            "calc.fix-tests           completed   chatgpt/gpt-6-sol  /tmp/calc\ncalc.lead                running     chatgpt/gpt-6-sol  /tmp/calc\nfrom calc import average\ncases = []\n{\"bot\":\"calc.fix-tests\",\"handle\":\"turn:calc.fix-tests/4\",\"status\":\"queued\",\"turn\":4}",
            false,
        );
        show(
            &format!(
                r#""$AGENT_BIN" run --detach --delivery queue --model "$AGENT_MODEL" {effort} --bot calc.fix-tests -- 'Add a test.'"#
            ),
            r#"{"detail":null,"error":"bot_not_found"}"#,
            true,
        );
        show(
            r#"rex __internal skill; "$AGENT_BIN" ls --pretty; "$AGENT_BIN" run --help"#,
            "calc.lead                running     chatgpt/gpt-6-sol  /tmp/calc\nUsage: agent run [OPTIONS] [--] PROMPT...\n/bin/sh: rex: command not found",
            false,
        );
        show(
            r#""$AGENT_BIN" --help; ls /Applications | grep -Ei 'rex|agent|sonny'"#,
            "Usage: agent COMMAND [OPTIONS]\n",
            false,
        );
        show(
            &format!(
                r#""$AGENT_BIN" run --detach --new --agents --model "$AGENT_MODEL" {effort} --bot calc.single-negative --workspace "$PWD" -- 'The person explicitly requests a new task.'"#
            ),
            "{\"bot\":\"calc.single-negative\",\"handle\":\"turn:calc.single-negative/13\",\"status\":\"running\",\"turn\":13}",
            false,
        );
    }
}
