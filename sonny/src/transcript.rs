//! What a finished piece of the conversation looks like once it is written
//! into the terminal's scrollback: a prompt, a reply, a tool call. Each
//! piece is a list of lines already wrapped to the terminal's width, so it
//! can be inserted above the live area and left to the terminal.
use ratatui::{
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
    /// An SVG fence's text, gathered to be drawn when it closes.
    picture: Option<String>,
    ready: Option<String>,
    /// How far the last line's wrapped rows are indented: under a list
    /// item's text, so a continuation does not read as a new item.
    hang: usize,
}

impl Markdown {
    /// What the last line's wrapped rows start with, inside a reply.
    pub fn rest(&self) -> Span<'static> {
        Span::raw(format!("  {}", " ".repeat(self.hang)))
    }

    /// The picture a fence that just closed drew, as rows of the
    /// terminal's image cells, or why it could not be drawn.
    pub fn picture(&mut self, width: usize) -> Vec<Line<'static>> {
        let Some(svg) = self.ready.take() else {
            return Vec::new();
        };
        crate::image::svg(&svg, width).unwrap_or_else(|error| {
            vec![Line::from(format!("  (could not draw the SVG: {error})")).dim()]
        })
    }

    /// The styled line, or none for a fence marker, which is not shown.
    pub fn line(&mut self, text: &str) -> Option<Line<'static>> {
        let trimmed = text.trim_start();
        self.hang = 0;
        if let Some(lang) = trimmed.strip_prefix("```") {
            self.fence = !self.fence;
            if self.fence && lang.trim() == "svg" && crate::term::graphics() {
                self.picture = Some(String::new());
            } else if !self.fence {
                self.ready = self.picture.take();
            }
            return None;
        }
        if let Some(picture) = &mut self.picture {
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
        for marker in ["- ", "* ", "+ "] {
            if let Some(rest) = trimmed.strip_prefix(marker) {
                self.hang = indent.len() + 2;
                let mut spans = vec![Span::raw(format!("{indent}- ")).dim()];
                spans.extend(inline(rest, Style::new()));
                return Some(Line::from(spans));
            }
        }
        if let Some(rest) = trimmed.strip_prefix("> ") {
            let mut spans = vec![Span::raw("▌ ").fg(Color::Green)];
            spans.extend(inline(rest, Style::new().italic()));
            return Some(Line::from(spans));
        }
        Some(Line::from(inline(text, Style::new())))
    }
}

/// `code` and **bold** inside a line.
fn inline(text: &str, base: Style) -> Vec<Span<'static>> {
    let mut spans = Vec::new();
    let mut rest = text;
    while !rest.is_empty() {
        let tick = rest.find('`');
        let star = rest.find("**");
        let (at, code) = match (tick, star) {
            (Some(t), Some(s)) if s < t => (s, false),
            (Some(t), _) => (t, true),
            (None, Some(s)) => (s, false),
            (None, None) => break,
        };
        let open = if code { 1 } else { 2 };
        let close = if code { "`" } else { "**" };
        let Some(end) = rest[at + open..].find(close) else {
            break;
        };
        if at > 0 {
            spans.push(Span::styled(rest[..at].to_owned(), base));
        }
        let inner = rest[at + open..at + open + end].to_owned();
        spans.push(if code {
            Span::styled(inner, base.fg(Color::Cyan))
        } else {
            Span::styled(inner, base.add_modifier(Modifier::BOLD))
        });
        rest = &rest[at + open + end + close.len()..];
    }
    if !rest.is_empty() {
        spans.push(Span::styled(rest.to_owned(), base));
    }
    spans
}

/// A line wrapped at word boundaries to `width` columns, the first row
/// after `first` and the others after `rest`, which should be as wide.
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
    let room = width.saturating_sub(first.width()).max(8);
    let rest = rest.spans;
    let mut rows: Vec<Vec<Span<'static>>> = vec![first.spans];
    let mut used = 0;
    for span in line.spans {
        let style = line.style.patch(span.style);
        for word in words(&span.content) {
            let mut word = word.to_owned();
            let mut wide = word.width();
            // A word's trailing spaces may hang past the edge.
            if used + word.trim_end().width() > room && used > 0 {
                rows.push(rest.clone());
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
                    .push(Span::styled(word[..cut].to_owned(), style));
                rows.push(rest.clone());
                used = 0;
                word = word[cut..].to_owned();
                wide = word.width();
            }
            used += wide;
            rows.last_mut().unwrap().push(Span::styled(word, style));
        }
    }
    rows.into_iter().map(Line::from).collect()
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
    if let Some(from) = from {
        out.push(Line::from(format!("  from {from}")).dim());
    }
    for (i, line) in text.lines().enumerate() {
        let lead = if i == 0 { "› " } else { "  " };
        out.extend(wrap(
            Line::from(line.to_owned()),
            width,
            Span::raw(lead).bold().fg(Color::Magenta),
            Span::raw("  "),
        ));
    }
    out
}

/// A whole reply, as replayed history shows it.
pub fn reply(text: &str, width: usize) -> Vec<Line<'static>> {
    let mut markdown = Markdown::default();
    let mut out = Vec::new();
    for line in text.lines() {
        if let Some(styled) = markdown.line(line) {
            let lead = if out.is_empty() { "• " } else { "  " };
            out.extend(wrap(styled, width, Span::raw(lead), markdown.rest()));
        }
        out.extend(markdown.picture(width));
    }
    out
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
    let bullet = if failed {
        Span::raw("• ").fg(Color::Red)
    } else {
        Span::raw("• ").fg(Color::Green)
    };
    let field = |key: &str| arguments[key].as_str().unwrap_or("").to_owned();
    let elbow = || Span::raw("  └ ").dim();
    let gap = || Span::raw("    ");
    // A call Sonny knows the meaning of, said as what it does.
    if name == "shell"
        && let Some(said) = crate::calls::said(&field("command"), output.unwrap_or(""), failed)
    {
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
                }
            }
            said.push(crate::calls::said(part, &own, false));
        }
        // What the known calls print says nothing their line does not.
        printed.retain(|line| {
            let line = line.trim();
            !(line.starts_with("plan saved: ") || line == "plan cleared")
                && !serde_json::from_str::<Value>(line)
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
                Ok(mut said) => {
                    said.failed |= failed && i + 1 == count;
                    out.extend(said_lines(said, width));
                }
                Err(ran) => {
                    let bullet = if failed && i + 1 == count {
                        Span::raw("• ").fg(Color::Red)
                    } else {
                        Span::raw("• ").fg(Color::Green)
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
        if last_ran.is_none() && !rest.trim().is_empty() {
            out.extend(preview(&rest, width));
        }
        return out;
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
        "read" => ("Read", field("path")),
        "write" => ("Wrote", field("path")),
        "edit" => ("Edited", field("path")),
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
        Span::raw("• ").fg(Color::Green)
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
pub fn finished(
    status: &str,
    detail: &str,
    seconds: Option<u64>,
    usage: (u64, u64, u64),
    width: usize,
) -> Line<'static> {
    let mut label = match status {
        "completed" => "Worked".to_owned(),
        "steered" => "Steered".to_owned(),
        other => other.to_owned(),
    };
    if let Some(seconds) = seconds {
        label.push_str(&format!(" for {}", elapsed(seconds)));
    }
    let (input, output, cached) = usage;
    if input + output > 0 {
        label.push_str(&format!(
            " · {} in ({} cached) · {} out",
            tokens(input),
            tokens(cached),
            tokens(output)
        ));
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
    fn the_clock_reads_like_codex() {
        assert_eq!(elapsed(9), "9s");
        assert_eq!(elapsed(65), "1m 05s");
        assert_eq!(elapsed(3723), "1h 02m 03s");
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
                !text.contains("AGENT_BIN")
                    && !text.contains("skills/plan")
                    && !text.contains("sonny show"),
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
        show(
            r#"sh "$HOME/.agents/skills/plan/plan" '[x] Read memory and existing tests' '[x] Add single-negative regression case' '[>] Run tests and report exact changes'; python3 test_calc.py; result=$?; printf 'Exit code: %s\n' "$result"; sh "$HOME/.agents/skills/plan/plan" --clear"#,
            "plan saved: 3 steps\n6 passed, 0 failed\nExit code: 0\nplan cleared",
            false,
        );
        show(
            r#""$HOME/.agent/memory" show; cat test_calc.py; sh "$HOME/.agents/skills/plan/plan" '[>] Inspect current calculator tests' '[ ] Add negative-number case only'"#,
            "{}\nfrom calc import average",
            false,
        );
    }
}
