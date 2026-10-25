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
    /// How far the last line's wrapped rows are indented: under a list
    /// item's text, so a continuation does not read as a new item.
    hang: usize,
}

impl Markdown {
    /// What the last line's wrapped rows start with, inside a reply.
    pub fn rest(&self) -> Span<'static> {
        Span::raw(format!("  {}", " ".repeat(self.hang)))
    }

    /// The styled line, or none for a fence marker, which is not shown.
    pub fn line(&mut self, text: &str) -> Option<Line<'static>> {
        let trimmed = text.trim_start();
        self.hang = 0;
        if trimmed.starts_with("```") {
            self.fence = !self.fence;
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
    let room = width.saturating_sub(first.width()).max(8);
    let mut rows: Vec<Vec<Span<'static>>> = vec![vec![first]];
    let mut used = 0;
    for span in line.spans {
        let style = line.style.patch(span.style);
        for word in words(&span.content) {
            let mut word = word.to_owned();
            let mut wide = word.width();
            // A word's trailing spaces may hang past the edge.
            if used + word.trim_end().width() > room && used > 0 {
                rows.push(vec![rest.clone()]);
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
                rows.push(vec![rest.clone()]);
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
    let (verb, what) = match name {
        "shell" => ("Ran", field("command")),
        "read" => ("Read", field("path")),
        "write" => ("Wrote", field("path")),
        "edit" => ("Edited", field("path")),
        "wait" => ("Waited for", arguments["handles"].to_string()),
        "echo" | "note" => ("Noted", field("text")),
        _ => ("Called", format!("{name} {arguments}")),
    };
    let header = Line::from(vec![
        Span::raw(verb).bold(),
        Span::raw(" "),
        Span::raw(what.lines().next().unwrap_or("").to_owned()),
    ]);
    let mut out = wrap(header, width, bullet, Span::raw("    "));
    let elbow = || Span::raw("  └ ").dim();
    let gap = || Span::raw("    ");
    match name {
        "edit" => out.extend(diff(arguments, workspace, width)),
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
        "shell" | "wait" => {
            let output = output.unwrap_or("");
            let lines: Vec<&str> = output.lines().filter(|l| !l.trim().is_empty()).collect();
            if lines.is_empty() {
                out.push(Line::from(vec![elbow(), Span::raw("(no output)").dim()]));
            }
            let shown = if lines.len() > 5 { 3 } else { lines.len() };
            for (i, line) in lines.iter().take(shown).enumerate() {
                let lead = if i == 0 { elbow() } else { gap() };
                let line = Line::from(line.to_string()).dim();
                out.extend(wrap(line, width, lead, gap()).into_iter().take(2));
            }
            if lines.len() > shown {
                out.push(Line::from(vec![
                    gap(),
                    Span::raw(format!("… +{} lines", lines.len() - shown)).dim(),
                ]));
            }
        }
        _ => {}
    }
    out
}

/// `agent run --detach`'s reply, as a line: the agent and its turn.
fn started(output: Option<&str>) -> Option<String> {
    let reply: Value = serde_json::from_str(output?.trim()).ok()?;
    // A refusal, as the CLI writes it: its code and what it says.
    if let Some(error) = reply["error"].as_str() {
        let detail = reply["detail"].as_str().unwrap_or("");
        return Some(format!("refused: {detail} ({error})"));
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

/// An edit as removed and added lines, numbered where the file now has
/// them when the new text can be found there.
fn diff(arguments: &Value, workspace: &str, width: usize) -> Vec<Line<'static>> {
    let old = arguments["old"].as_str().unwrap_or("");
    let new = arguments["new"].as_str().unwrap_or("");
    let path = arguments["path"].as_str().unwrap_or("");
    let path = std::path::Path::new(workspace).join(path);
    let start = std::fs::read_to_string(path)
        .ok()
        .and_then(|now| now.find(new).map(|at| now[..at].lines().count() + 1));
    let number = |n: Option<usize>| match n {
        Some(n) => format!("{n:>4} "),
        None => "     ".to_owned(),
    };
    let mut out = Vec::new();
    for (i, line) in old.lines().enumerate() {
        let text = Line::from(format!("-{line}")).fg(Color::Red);
        let lead = Span::raw(number(start.map(|s| s + i))).dim();
        out.extend(wrap(text, width, lead, Span::raw("     ")));
    }
    for (i, line) in new.lines().enumerate() {
        let text = Line::from(format!("+{line}")).fg(Color::Green);
        let lead = Span::raw(number(start.map(|s| s + i))).dim();
        out.extend(wrap(text, width, lead, Span::raw("     ")));
    }
    out
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
    fn starting_an_agent_reads_as_one_line() {
        let output = r#"{"bot":"calc.fix","bot_id":2,"handle":"turn:calc.fix/2","status":"running","turn":2}"#;
        let rows = tool(
            "shell",
            &serde_json::json!({"command":"agent run --detach"}),
            Some(output),
            false,
            "/",
            60,
        );
        assert_eq!(
            text(&rows),
            ["• Ran agent run --detach", "  └ started calc.fix, turn 2"]
        );
    }

    #[test]
    fn the_clock_reads_like_codex() {
        assert_eq!(elapsed(9), "9s");
        assert_eq!(elapsed(65), "1m 05s");
        assert_eq!(elapsed(3723), "1h 02m 03s");
    }
}
