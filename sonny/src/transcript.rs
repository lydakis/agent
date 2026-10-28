//! What a finished piece of the conversation looks like once it is written
//! into the terminal's scrollback: a prompt, a reply, a tool call. Each
//! piece is a list of lines already wrapped to the terminal's width, so it
//! can be inserted above the live area and left to the terminal.
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
    /// Inside `$$` display math, gathered in `picture`.
    math: bool,
}

impl Markdown {
    /// A streamed line as rows: a table that ended first, then the line,
    /// then a fence's picture or diff when the line closed one. `first`
    /// says nothing of the reply is shown yet, so it gets the bullet.
    pub fn feed(&mut self, text: &str, width: usize, first: bool) -> Vec<Line<'static>> {
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
        *self = Self::default();
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
        let drawn = crate::cache::drawn(&lang, &source, width, || {
            let svg = match lang.as_str() {
                "mermaid" => crate::mermaid::svg(&source)?,
                "math" => crate::math::svg(&source)?,
                _ => source.clone(),
            };
            crate::image::render(&svg, width)
        });
        match drawn.and_then(|drawn| crate::image::show(&drawn)) {
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
        // $$ display math $$, on one line or around several.
        if !self.fence && trimmed.trim_end().starts_with("$$") {
            let inner = trimmed.trim_end().trim_start_matches("$$");
            if let Some(math) = inner.strip_suffix("$$") {
                self.ready = Some(("math".into(), math.to_owned()));
                return None;
            }
            match self.picture.take() {
                Some((lang, math)) if lang == "math" && self.math => {
                    self.math = false;
                    self.ready = Some((lang, math));
                }
                held => {
                    self.picture = held.or(Some(("math".into(), format!("{inner}\n"))));
                    self.math = true;
                }
            }
            return None;
        }
        if self.math
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
            let drawn = matches!(lang, "svg" | "mermaid") && crate::term::graphics();
            if self.fence && (drawn || matches!(lang, "diff" | "patch" | "math" | "html")) {
                self.picture = Some((lang.to_owned(), String::new()));
            } else if !self.fence {
                self.ready = self.picture.take();
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
            return Some(Line::from(vec![
                Span::raw("  "),
                Span::styled(text.to_owned(), Style::new().fg(Color::Cyan)),
            ]));
        }
        let hashes = trimmed.bytes().take_while(|b| *b == b'#').count();
        if (1..=6).contains(&hashes) && trimmed[hashes..].starts_with(' ') {
            let style = Style::new().add_modifier(Modifier::BOLD);
            return Some(Line::from(inline(trimmed[hashes + 1..].trim(), style)));
        }
        let indent = &text[..text.len() - trimmed.len()];
        let bare = trimmed.trim_end();
        if bare.len() >= 3
            && ["-", "*", "_"]
                .iter()
                .any(|c| bare.chars().all(|x| x.to_string() == *c || x == ' '))
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
        Some(Line::from(inline(text, Style::new())))
    }
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
        // [text](url): the text, underlined, and the address when it says more.
        if c == '['
            && let Some(close) = rest.find("](")
            && let Some(end) = rest[close + 2..].find(')')
        {
            let label = &rest[1..close];
            let url = &rest[close + 2..close + 2 + end];
            if !label.contains('[') && !url.contains(' ') {
                flush(&mut plain, &mut spans);
                spans.extend(inline(label, base.underlined()));
                if url != label && !url.starts_with('#') {
                    spans.push(Span::styled(format!(" ({url})"), base.dim()));
                }
                rest = &rest[close + 2 + end + 1..];
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
        .and_then(|drawn| Ok((crate::image::show(&drawn)?, drawn.cut))),
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

/// A whole reply, as replayed history shows it.
pub fn reply(text: &str, width: usize) -> Vec<Line<'static>> {
    let mut markdown = Markdown::default();
    let mut out = Vec::new();
    for line in text.lines() {
        let first = out.is_empty();
        out.extend(markdown.feed(line, width, first));
    }
    let first = out.is_empty();
    out.extend(markdown.end(width, first));
    out
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
const KEPT: [&str; 25] = [
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
    "Opened a tab",
    "Opened a split",
    "Stopped Errand job",
    "Applied the changes of Errand job",
    "Applied the changes of",
];

fn kept(said: &crate::calls::Said) -> bool {
    KEPT.contains(&said.verb.as_str())
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
    if let Some(said) = crate::calls::declared(name, arguments, failed) {
        if clean && !kept(&said) {
            return Vec::new();
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
        _ => ("Called", format!("{name} {arguments}")),
    };
    let mut header = Line::from(vec![
        Span::raw(verb).bold(),
        Span::raw(" "),
        Span::raw(what.lines().next().unwrap_or("").to_owned()),
    ]);
    // A file tool that failed changed nothing: what it was asked to do is
    // not drawn, its error is.
    if failed && matches!(name, "edit" | "write" | "read") {
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
    let mut out = wrap(header, width, bullet, Span::raw("    "));
    match name {
        "edit" => out.extend(edit.map(|e| e.lines(width, EDIT_ROWS)).unwrap_or_default()),
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
        _ => {}
    }
    out
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
        && let Ok(now) = std::fs::read_to_string(path)
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
        let lines = reply("Here:\n```diff\n-old\n+new", 80);
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
        let rows = reply("Hi `x`\n## Head\n```rust\nlet a = 1;\n```\n- item", 40);
        assert_eq!(
            text(&rows),
            ["• Hi x", "  Head", "    let a = 1;", "  - item"]
        );
    }

    #[test]
    fn a_fence_that_never_closes_still_shows_as_code() {
        let rows = reply("Here:\n```diff\n-a\n+b", 40);
        assert_eq!(text(&rows), ["• Here:", "    -a", "    +b"]);
        let rows = reply("$$\nx^2", 40);
        assert_eq!(text(&rows), ["•   x^2"]);
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
        let rows = reply("- divide by len(values), not len(values) - 1", 30);
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
            serde_json::json!({"question": "Ship it?", "choices": ["yes", "no"]}),
            "Asked.",
        );
        assert_eq!(asked, ["• Asked you Ship it?", "  └ 1 yes · 2 no"]);
        let started = shell(
            r#""$AGENT_BIN" run --detach --new --bot calc.fix -- 'Fix the tests.'"#,
            r#"{"bot":"calc.fix","handle":"turn:calc.fix/2","turn":2}"#,
        );
        assert_eq!(started[0], "• Started calc.fix · turn 2");
        // How a lead in Sonny starts a task, and the note it prints after.
        let task = shell(
            r#""/Applications/Sonny/sonny" task calc.fix --agents --setup --model "$AGENT_MODEL" -- 'Fix the tests.'"#,
            "{\"bot\":\"calc.fix\",\"handle\":\"turn:calc.fix/3\",\"turn\":3}\nIts work comes back on branch agent/calc.fix.",
        );
        assert_eq!(task, ["• Started calc.fix · turn 3", "  └ Fix the tests."]);
        assert!(shell(r#"sonny task changes calc.fix"#, "M calc.py").is_empty());
        assert_eq!(
            shell(r#"sonny task changes calc.fix --apply"#, ""),
            ["• Applied the changes of calc.fix"]
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
            assert!(
                !text.contains("AGENT_BIN") && !text.contains("sonny show"),
                "{command}\n  drew: {text}"
            );
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
            r#""$AGENT_BIN" --help; "$HOME/.agent/project" --help; ls /Applications | grep -Ei 'rex|agent|sonny'"#,
            "Usage: agent COMMAND [OPTIONS]\n",
            false,
        );
        show(
            &format!(
                r#""$AGENT_BIN" run --detach --new --agents --model "$AGENT_MODEL" {effort} --bot calc.single-negative --workspace "$PWD" -- 'The person explicitly requests a new task.'; "/Users/x/sonny" show calc.single-negative --split right"#
            ),
            "{\"bot\":\"calc.single-negative\",\"handle\":\"turn:calc.single-negative/13\",\"status\":\"running\",\"turn\":13}\n{\"bot\":\"calc.single-negative\",\"session\":\"calc\"}",
            false,
        );
    }
}
