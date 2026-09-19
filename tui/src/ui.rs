//! Rendering, following the Thread concept exactly: a centered reading
//! column with the title at the left edge, rounded boxes for anything a bot
//! waits on, plain dim tool output, a panel band holding the composer and the
//! key line. Colors are the terminal's own sixteen plus one near-background
//! gray for panels when the terminal says whether it is light or dark.
use crate::app::{App, Item, Transcript, fmt_secs};
use ratatui::{
    Frame,
    layout::{Constraint, Layout, Rect},
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Borders, Clear, Paragraph},
};
use std::time::{Duration, Instant};

/// The concept's column: 84 characters, centered when the pane is wider.
const COLUMN: usize = 84;
const EDGE: u16 = 1;

pub struct Palette;
impl Palette {
    pub const ACCENT: Color = Color::Yellow;
    pub const RUN: Color = Color::Green;
    pub const WAIT: Color = Color::Yellow;
    pub const PACE: Color = Color::Magenta;
    pub const FAIL: Color = Color::Red;
    pub const USER: Color = Color::Cyan;
    pub const RULE: Color = Color::DarkGray;
}
fn dim() -> Style {
    Style::default().add_modifier(Modifier::DIM)
}
fn faint() -> Style {
    Style::default().fg(Palette::RULE)
}
fn shaded(style: Style, shade: Option<Color>) -> Style {
    match shade {
        Some(color) => style.bg(color),
        None => style,
    }
}

pub fn glyph(status: &str, pulse: bool) -> Span<'static> {
    let (g, color) = match status {
        "running" => ("●", Palette::RUN),
        "waiting" => ("◐", Palette::WAIT),
        "paced" => ("◔", Palette::PACE),
        "queued" | "ready" => ("◌", Color::Blue),
        "idle" => ("○", Palette::RULE),
        _ => ("✘", Palette::FAIL),
    };
    let mut style = Style::default().fg(color);
    if status == "running" && pulse {
        style = style.add_modifier(Modifier::DIM);
    }
    Span::styled(g, style)
}
pub fn label(status: &str) -> &'static str {
    match status {
        "running" => "working",
        "waiting" => "waiting",
        "paced" => "rate limited",
        "idle" => "idle",
        "interrupted" => "interrupted",
        "queued" | "ready" => "queued",
        _ => "failed",
    }
}

/// Wrap one logical line into rows of at most `width` cells. The first row
/// starts as written; continuation rows get `indent`.
fn wrap(text: &str, width: usize, style: Style, indent: &str) -> Vec<Line<'static>> {
    let width = width.max(8);
    let mut rows = Vec::new();
    for raw in text.split('\n') {
        let mut current = String::new();
        let mut first = true;
        for word in raw.split(' ') {
            let piece = if first {
                first = false;
                word.to_owned()
            } else {
                format!(" {word}")
            };
            if current.chars().count() + piece.chars().count() > width && !current.is_empty() {
                rows.push(Line::from(Span::styled(
                    std::mem::take(&mut current),
                    style,
                )));
                current = format!("{indent}{word}");
            } else {
                current.push_str(&piece);
            }
            while current.chars().count() > width {
                let head: String = current.chars().take(width).collect();
                let tail: String = current.chars().skip(width).collect();
                rows.push(Line::from(Span::styled(head, style)));
                current = format!("{indent}{tail}");
            }
        }
        rows.push(Line::from(Span::styled(current, style)));
    }
    rows
}

fn line_width(line: &Line) -> usize {
    line.spans.iter().map(|s| s.content.chars().count()).sum()
}

/// The concept's box: rounded border, panel ground, one cell of padding.
/// `right` is drawn at the right edge of the first row (elapsed, language).
fn boxed(
    lines: Vec<Line<'static>>,
    width: usize,
    shade: Option<Color>,
    border: Style,
    right: Option<(String, Style)>,
) -> Vec<Line<'static>> {
    let inner = width.saturating_sub(2);
    let fill = shaded(Style::default(), shade);
    let mut out = vec![Line::from(Span::styled(
        format!("╭{}╮", "─".repeat(inner)),
        border,
    ))];
    for (i, line) in lines.into_iter().enumerate() {
        let mut spans = vec![Span::styled("│", border), Span::styled(" ", fill)];
        let mut used = line_width(&line) + 1;
        spans.extend(line.spans.into_iter().map(|s| s.patch_style(fill)));
        if i == 0
            && let Some((text, style)) = &right
            && used + text.chars().count() + 2 <= inner
        {
            let pad = inner - used - text.chars().count() - 1;
            spans.push(Span::styled(" ".repeat(pad), fill));
            spans.push(Span::styled(text.clone(), fill.patch(*style)));
            used = inner - 1;
        }
        spans.push(Span::styled(" ".repeat(inner.saturating_sub(used)), fill));
        spans.push(Span::styled("│", border));
        out.push(Line::from(spans));
    }
    out.push(Line::from(Span::styled(
        format!("╰{}╯", "─".repeat(inner)),
        border,
    )));
    out
}

#[allow(clippy::too_many_arguments)]
fn card(
    status: &str,
    name: &str,
    elapsed: Option<String>,
    last: &str,
    selected: bool,
    width: usize,
    shade: Option<Color>,
    pulse: bool,
) -> Vec<Line<'static>> {
    let mut lines = vec![Line::from(vec![
        glyph(status, pulse),
        Span::styled(
            format!(" {name}"),
            Style::default().add_modifier(Modifier::BOLD),
        ),
    ])];
    if !last.is_empty() {
        let room = width.saturating_sub(4);
        let one = last.lines().next().unwrap_or("");
        let text: String = if one.chars().count() > room {
            one.chars().take(room.saturating_sub(1)).collect::<String>() + "…"
        } else {
            one.to_owned()
        };
        lines.push(Line::from(Span::styled(text, dim())));
    }
    let border = if selected {
        Style::default().fg(Palette::ACCENT)
    } else {
        faint()
    };
    boxed(lines, width, shade, border, elapsed.map(|e| (e, faint())))
}

/// Inline `**bold**` and `` `code` ``; nothing else. Unbalanced markers stay literal.
fn inline_spans(text: &str, base: Style, shade: Option<Color>) -> Vec<Span<'static>> {
    let mut spans = Vec::new();
    let mut rest = text;
    while !rest.is_empty() {
        let bold = rest
            .find("**")
            .and_then(|i| rest[i + 2..].find("**").map(|j| (i, i + 2 + j)));
        let code = rest
            .find('`')
            .and_then(|i| rest[i + 1..].find('`').map(|j| (i, i + 1 + j)));
        match (bold, code) {
            (Some((bi, bj)), c) if c.is_none_or(|(ci, _)| bi <= ci) => {
                spans.push(Span::styled(rest[..bi].to_owned(), base));
                spans.push(Span::styled(
                    rest[bi + 2..bj].to_owned(),
                    base.add_modifier(Modifier::BOLD),
                ));
                rest = &rest[bj + 2..];
            }
            (_, Some((ci, cj))) => {
                spans.push(Span::styled(rest[..ci].to_owned(), base));
                spans.push(Span::styled(
                    rest[ci + 1..cj].to_owned(),
                    shaded(base, shade),
                ));
                rest = &rest[cj + 1..];
            }
            _ => {
                spans.push(Span::styled(rest.to_owned(), base));
                break;
            }
        }
    }
    spans
}

/// Assistant text: fenced code becomes a box like a card, paragraphs wrap
/// with inline bold and code, headings are bold. Nothing else is interpreted.
fn markdown_rows(text: &str, width: usize, shade: Option<Color>) -> Vec<Line<'static>> {
    let mut rows = Vec::new();
    let mut fence: Option<(String, Vec<Line<'static>>)> = None;
    let flush = |rows: &mut Vec<Line<'static>>, lang: String, body: Vec<Line<'static>>| {
        let right = (!lang.is_empty()).then(|| (lang, faint()));
        rows.extend(boxed(body, width, shade, faint(), right));
    };
    for raw in text.lines() {
        if let Some(rest) = raw.trim_start().strip_prefix("```") {
            match fence.take() {
                Some((lang, body)) => flush(&mut rows, lang, body),
                None => fence = Some((rest.trim().to_owned(), Vec::new())),
            }
            continue;
        }
        if let Some((_, body)) = &mut fence {
            body.extend(wrap(raw, width.saturating_sub(4), Style::default(), "  "));
            continue;
        }
        if raw.trim().is_empty() {
            rows.push(Line::from(""));
            continue;
        }
        let (line, base) = match raw.trim_start().strip_prefix('#') {
            Some(h) => (
                h.trim_start_matches('#').trim().to_owned(),
                Style::default().add_modifier(Modifier::BOLD),
            ),
            None => (raw.to_owned(), Style::default()),
        };
        for row in wrap(&line, width, base, "") {
            let plain: String = row
                .spans
                .into_iter()
                .map(|s| s.content.into_owned())
                .collect();
            rows.push(Line::from(inline_spans(&plain, base, shade)));
        }
    }
    if let Some((lang, body)) = fence.take() {
        flush(&mut rows, lang, body);
    }
    rows
}

fn transcript_rows(app: &App, name: &str, width: usize, pulse: bool) -> Vec<Line<'static>> {
    let Some(t) = app.transcripts.get(name) else {
        return Vec::new();
    };
    let shade = app.ui.shade;
    let mut rows: Vec<Line> = Vec::new();
    let mut last_turn: Option<i64> = None;
    for (turn, item) in &t.items {
        if turn.is_some() && *turn != last_turn && !rows.is_empty() {
            rows.push(Line::from(""));
        }
        if turn.is_some() {
            last_turn = *turn;
        }
        match item {
            Item::User(s) => rows.extend(wrap(
                &format!("› {s}"),
                width,
                Style::default().fg(Palette::USER),
                "  ",
            )),
            Item::Text(s) => rows.extend(markdown_rows(s, width, shade)),
            Item::Thought { text, secs } => {
                if app.ui.thoughts {
                    rows.extend(wrap(
                        &format!("  {text}"),
                        width,
                        dim().add_modifier(Modifier::ITALIC),
                        "  ",
                    ));
                } else {
                    rows.push(Line::from(Span::styled(
                        format!("thought {}", fmt_secs(Duration::from_secs(*secs))),
                        faint().add_modifier(Modifier::ITALIC),
                    )));
                }
            }
            Item::Tool {
                name,
                summary,
                started,
                took,
                ..
            } => {
                let accent = Style::default().fg(Palette::ACCENT);
                let mut lines = wrap(&format!("▸ {name} {summary}"), width, accent, "  ");
                let stamp = match (started, took) {
                    (Some(s), _) => Some(fmt_secs(s.elapsed())),
                    (None, Some(d)) if *d >= Duration::from_millis(1500) => Some(fmt_secs(*d)),
                    _ => None,
                };
                let single = lines.len() == 1;
                if let Some(first) = lines.first_mut() {
                    // The tool's name is the bold part of the line.
                    let head = format!("▸ {name}");
                    let rest: String = first
                        .spans
                        .iter()
                        .map(|s| s.content.as_ref())
                        .collect::<String>()
                        .chars()
                        .skip(head.chars().count())
                        .collect();
                    let mut spans = vec![
                        Span::styled("▸ ", accent),
                        Span::styled(name.clone(), accent.add_modifier(Modifier::BOLD)),
                        Span::styled(rest, accent),
                    ];
                    if let Some(stamp) = stamp
                        && single
                    {
                        spans.push(Span::styled(format!("  {stamp}"), faint()));
                    }
                    *first = Line::from(spans);
                }
                rows.extend(lines);
            }
            Item::Output(s) => {
                let lines: Vec<&str> = s.lines().filter(|l| !l.trim().is_empty()).collect();
                let shown = if !app.ui.output && lines.len() > 2 {
                    &lines[..2]
                } else {
                    &lines[..]
                };
                for l in shown {
                    rows.extend(wrap(&format!("  {l}"), width, dim(), "  "));
                }
                if shown.len() < lines.len() {
                    rows.push(Line::from(Span::styled(
                        format!("  +{} lines", lines.len() - shown.len()),
                        faint(),
                    )));
                }
            }
            Item::Note(s) => {
                rows.extend(wrap(s, width, faint().add_modifier(Modifier::ITALIC), ""))
            }
            Item::Peer(peer) => {
                if let Some(b) = app.bots.get(peer) {
                    let last = app.transcripts.get(peer).map(last_line).unwrap_or_default();
                    let elapsed = b
                        .turn_started
                        .map(|s| fmt_secs(s.elapsed()))
                        .or_else(|| b.elapsed.map(fmt_secs));
                    rows.extend(card(
                        &b.status,
                        peer,
                        elapsed,
                        &last,
                        app.ui.peek.as_deref() == Some(peer.as_str()),
                        width,
                        shade,
                        pulse,
                    ));
                }
            }
            Item::Proc {
                handle,
                cmd,
                done,
                open,
            } => {
                let status = if done.is_some() { "idle" } else { "running" };
                let last = match done {
                    Some(d) if d.is_empty() => "done".to_owned(),
                    Some(d) => d.clone(),
                    None => handle.clone(),
                };
                rows.extend(card(
                    status,
                    &format!("$ {cmd}"),
                    None,
                    &last,
                    *open,
                    width,
                    shade,
                    pulse,
                ));
            }
            Item::Node { .. } => rows.push(Line::from(Span::styled("…", faint()))),
        }
    }
    if !t.thinking.is_empty() {
        let tail = t.thinking.rsplit(". ").next().unwrap_or("").trim();
        rows.extend(wrap(
            &format!("  {tail}▍"),
            width,
            dim().add_modifier(Modifier::ITALIC),
            "  ",
        ));
    } else if !t.text.is_empty() {
        rows.extend(markdown_rows(&format!("{}▍", t.text), width, shade));
    } else if app.bots.get(name).is_some_and(|b| b.status == "running") {
        rows.push(Line::from(Span::styled(
            "▍",
            Style::default().fg(Palette::ACCENT),
        )));
    }
    rows
}

fn last_line(t: &Transcript) -> String {
    if !t.text.is_empty() {
        return t.text.clone();
    }
    if !t.thinking.is_empty() {
        return t.thinking.rsplit(". ").next().unwrap_or("").to_owned();
    }
    t.items
        .iter()
        .rev()
        .find_map(|(_, i)| match i {
            Item::Text(s) => Some(s.clone()),
            Item::Tool { name, summary, .. } => Some(format!("▸ {name} {summary}")),
            _ => None,
        })
        .unwrap_or_default()
}

fn title(app: &App, name: &str, pulse: bool, hint: &str) -> Line<'static> {
    let Some(b) = app.bots.get(name) else {
        return Line::from(Span::styled("no bots · /new NAME creates one", dim()));
    };
    let mut spans = vec![
        glyph(&b.status, pulse),
        Span::styled(
            format!(" {}", b.name),
            Style::default().add_modifier(Modifier::BOLD),
        ),
        Span::styled(format!(" {}", label(&b.status)), dim()),
    ];
    if !hint.is_empty() {
        spans.push(Span::styled(format!("   {hint}"), faint()));
    }
    Line::from(spans)
}

/// A transcript pane: title at the left edge, the reading column centered
/// when there is room (the thread) or left with a small gutter (a peek).
fn pane(
    frame: &mut Frame,
    area: Rect,
    app: &App,
    name: &str,
    pulse: bool,
    hint: &str,
    centered: bool,
) {
    if area.width < 6 || area.height < 3 {
        return;
    }
    let [head, body] = Layout::vertical([Constraint::Length(2), Constraint::Min(1)]).areas(area);
    let head = Rect {
        x: head.x + EDGE,
        width: head.width.saturating_sub(EDGE),
        ..head
    };
    frame.render_widget(Paragraph::new(title(app, name, pulse, hint)), head);
    let usable = body.width.saturating_sub(EDGE * 2) as usize;
    let width = usable.min(COLUMN);
    let x = if centered {
        body.x + EDGE + ((usable - width) / 2) as u16
    } else {
        body.x + EDGE
    };
    let inner = Rect {
        x,
        y: body.y,
        width: width as u16,
        height: body.height.saturating_sub(1),
    };
    let rows = transcript_rows(app, name, width, pulse);
    let height = inner.height as usize;
    let wanted = if centered {
        app.ui.scroll
    } else {
        app.ui.peek_scroll
    } as usize;
    let scroll = wanted.min(rows.len().saturating_sub(height));
    let bottom = rows.len().saturating_sub(scroll);
    let start = bottom.saturating_sub(height);
    let visible: Vec<Line> = rows.into_iter().skip(start).take(bottom - start).collect();
    frame.render_widget(Paragraph::new(visible), inner);
}

fn rail(frame: &mut Frame, area: Rect, app: &App, pulse: bool) {
    if area.width < 6 {
        return;
    }
    let mut rows: Vec<Line> = vec![Line::from(Span::styled(" bots", dim())), Line::from("")];
    for (b, depth, last, trail) in app.tree() {
        let mut prefix = String::new();
        if depth > 0 {
            for l in trail.iter().skip(1) {
                prefix.push_str(if *l { "  " } else { "│ " });
            }
            prefix.push_str(if last { "└ " } else { "├ " });
        }
        let sel = b.name == app.selected;
        let mark = Span::styled(
            if sel { "▎" } else { " " },
            Style::default().fg(Palette::ACCENT),
        );
        let name = Span::styled(
            format!(" {}", b.name),
            if sel {
                Style::default().add_modifier(Modifier::BOLD)
            } else {
                Style::default()
            },
        );
        rows.push(Line::from(vec![
            mark,
            Span::styled(prefix, faint()),
            glyph(&b.status, pulse),
            name,
        ]));
        if !b.waiting_on.is_empty() {
            let handles: Vec<String> = b
                .waiting_on
                .iter()
                .map(|h| {
                    h.trim_start_matches("turn:")
                        .split('/')
                        .next()
                        .unwrap_or(h)
                        .to_owned()
                })
                .collect();
            rows.push(Line::from(Span::styled(
                format!("{}⏳ {}", " ".repeat(3 + depth * 2), handles.join(" ")),
                Style::default()
                    .fg(Palette::WAIT)
                    .add_modifier(Modifier::DIM),
            )));
        }
    }
    let block = Block::default()
        .borders(Borders::RIGHT)
        .border_style(faint())
        .style(shaded(Style::default(), app.ui.shade));
    frame.render_widget(Paragraph::new(rows).block(block), area);
}

/// The panel band: a rule, the composer, breathing room, the key line.
fn band(frame: &mut Frame, area: Rect, app: &App, pulse: bool) {
    let fill = shaded(Style::default(), app.ui.shade);
    frame.render_widget(Block::default().style(fill), area);
    let [rule, composer, _, keys] = Layout::vertical([
        Constraint::Length(1),
        Constraint::Length(1),
        Constraint::Length(1),
        Constraint::Length(1),
    ])
    .areas(area);
    frame.render_widget(
        Block::default().borders(Borders::TOP).border_style(faint()),
        rule,
    );

    let who = if app.selected.is_empty() {
        "›".to_owned()
    } else {
        format!("{} ›", app.selected)
    };
    let line = Line::from(vec![
        Span::styled(
            format!(" {who} "),
            fill.fg(Palette::ACCENT).add_modifier(Modifier::BOLD),
        ),
        Span::styled(app.input.clone(), fill),
    ]);
    frame.render_widget(Paragraph::new(line), composer);
    if app.ui.picker.is_none() && !app.ui.help {
        frame.set_cursor_position((
            composer.x + 2 + who.chars().count() as u16 + app.input.chars().count() as u16,
            composer.y,
        ));
    }

    let b = app.bot();
    let busy = b.is_some_and(|b| b.status != "idle");
    let mut spans = vec![
        Span::styled(
            " ● ",
            fill.fg(if busy { Palette::WAIT } else { Palette::RUN })
                .add_modifier(if busy && pulse {
                    Modifier::DIM
                } else {
                    Modifier::empty()
                }),
        ),
        Span::styled(
            if busy {
                label(b.map(|b| b.status.as_str()).unwrap_or("idle")).to_owned()
            } else {
                "live".to_owned()
            },
            fill.add_modifier(Modifier::DIM),
        ),
    ];
    if let Some(b) = b {
        spans.push(Span::styled(format!("  {}", b.name), fill));
    }
    if let Some((text, at)) = &app.ui.toast
        && at.elapsed() < crate::app::TOAST
    {
        spans.push(Span::styled(format!("   {text}"), fill.fg(Palette::ACCENT)));
    }
    let mut keys_list: Vec<(&str, &str)> = Vec::new();
    if app.ui.picker.is_some() {
        keys_list.extend([("↑↓", "choose"), ("Enter", "switch"), ("Esc", "cancel")]);
    } else if app.ui.peek.is_some() {
        keys_list.extend([("Esc", "close"), ("^p", "next peer"), ("^k", "switch")]);
    } else {
        keys_list.push(("^k", "switch"));
        if !app.peers().is_empty() {
            keys_list.push(("^p", "peek"));
        }
        keys_list.push(("^b", "bots"));
        let t = app.transcripts.get(&app.selected);
        if t.is_some_and(|t| {
            t.items
                .iter()
                .any(|(_, i)| matches!(i, Item::Thought { .. }))
        }) {
            keys_list.push(("^t", if app.ui.thoughts { "fold" } else { "thoughts" }));
        }
        if t.is_some_and(|t| {
            t.items
                .iter()
                .any(|(_, i)| matches!(i, Item::Output(s) if s.lines().count() > 2))
        }) {
            keys_list.push(("^o", if app.ui.output { "fold" } else { "output" }));
        }
        if busy {
            keys_list.push(("Esc", "interrupt"));
        }
        keys_list.push(("^d", "detach"));
    }
    keys_list.push(("?", "keys"));
    let used: usize = spans.iter().map(|s| s.content.chars().count()).sum();
    let keys_text: usize = keys_list
        .iter()
        .map(|(k, v)| k.chars().count() + v.len() + 4)
        .sum::<usize>();
    let pad = (keys.width as usize).saturating_sub(used + keys_text);
    spans.push(Span::styled(" ".repeat(pad), fill));
    for (k, v) in keys_list {
        spans.push(Span::styled(
            k.to_owned(),
            fill.add_modifier(Modifier::BOLD),
        ));
        spans.push(Span::styled(
            format!(" {v}   "),
            fill.add_modifier(Modifier::DIM),
        ));
    }
    frame.render_widget(Paragraph::new(Line::from(spans)), keys);
}

/// A one-cell shadow below and to the right of an overlay, then its ground.
fn raise(frame: &mut Frame, popup: Rect, area: Rect) {
    let shadow = Rect {
        x: popup.x + 1,
        y: popup.y + 1,
        width: popup.width.min(area.right().saturating_sub(popup.x + 1)),
        height: popup.height.min(area.bottom().saturating_sub(popup.y + 1)),
    };
    frame.render_widget(Clear, shadow);
    frame.render_widget(
        Block::default().style(Style::default().bg(Color::DarkGray)),
        shadow,
    );
    frame.render_widget(Clear, popup);
    frame.render_widget(
        Block::default().style(Style::default().bg(Color::Reset)),
        popup,
    );
}

fn picker(frame: &mut Frame, area: Rect, app: &App, pulse: bool) {
    let Some(p) = &app.ui.picker else { return };
    let width = 52.min(area.width.saturating_sub(4));
    let rows = crate::picker_rows(app);
    let height = (rows.len() as u16 + 3).min(area.height.saturating_sub(4));
    let popup = Rect {
        x: area.x + (area.width - width) / 2,
        y: area.y + 3.min(area.height / 4),
        width,
        height,
    };
    raise(frame, popup, area);
    let mut lines = vec![Line::from(vec![
        Span::styled(
            " › ",
            Style::default()
                .fg(Palette::ACCENT)
                .add_modifier(Modifier::BOLD),
        ),
        Span::raw(p.query.clone()),
        Span::styled("▍", Style::default().fg(Palette::ACCENT)),
    ])];
    if rows.is_empty() {
        lines.push(Line::from(Span::styled(
            " no bot matches",
            faint().add_modifier(Modifier::ITALIC),
        )));
    }
    for (i, (name, prefix, hint)) in rows.iter().enumerate() {
        let b = &app.bots[name];
        let sel = i == p.sel;
        let mut spans = vec![
            Span::styled(
                if sel { "▎" } else { " " },
                Style::default().fg(Palette::ACCENT),
            ),
            Span::styled(prefix.clone(), faint()),
            glyph(&b.status, pulse),
            Span::styled(
                format!(" {name}"),
                if sel {
                    Style::default().add_modifier(Modifier::BOLD)
                } else {
                    Style::default()
                },
            ),
        ];
        if !hint.is_empty() {
            spans.push(Span::styled(format!("  {hint}"), faint()));
        }
        lines.push(Line::from(spans));
    }
    frame.render_widget(
        Paragraph::new(lines).block(Block::default().borders(Borders::ALL).border_style(faint())),
        popup,
    );
}

fn help(frame: &mut Frame, area: Rect) {
    let text = [
        "^k   switch bot          ^b   bot rail",
        "^p   peek next peer      Esc  close, then interrupt",
        "^t   unfold thoughts     ^o   unfold tool output",
        "↑ ↓  prev / next bot     ^d   detach (exit)",
        "",
        "/new NAME [PROVIDER/MODEL]   create a bot",
        "/mouse                       toggle mouse capture",
        "",
        "any key closes this",
    ];
    let width = 60.min(area.width.saturating_sub(2));
    let height = text.len() as u16 + 2;
    let popup = Rect {
        x: area.x + (area.width - width) / 2,
        y: area.y + (area.height.saturating_sub(height)) / 2,
        width,
        height,
    };
    raise(frame, popup, area);
    let lines: Vec<Line> = text.iter().map(|l| Line::from(format!(" {l}"))).collect();
    frame.render_widget(
        Paragraph::new(lines).block(Block::default().borders(Borders::ALL).border_style(faint())),
        popup,
    );
}

pub fn render(frame: &mut Frame, app: &App) {
    let area = frame.area();
    let pulse = (Instant::now().duration_since(app_epoch()).as_millis() / 500) % 2 == 1;
    let [main, bottom] = Layout::vertical([Constraint::Min(3), Constraint::Length(4)]).areas(area);
    let rail_w = app.ui.rail.value();
    let peek_w = (area.width as u32 * app.ui.peek_w.value() as u32 / 100) as u16;
    let [rail_area, main_area, peek_area] = Layout::horizontal([
        Constraint::Length(rail_w),
        Constraint::Min(20),
        Constraint::Length(peek_w),
    ])
    .areas(main);
    if rail_w > 0 {
        rail(frame, rail_area, app, pulse);
    }
    let sel = app.selected.clone();
    pane(frame, main_area, app, &sel, pulse, "", true);
    if peek_w > 0
        && let Some(peer) = &app.ui.peek
    {
        frame.render_widget(
            Block::default()
                .borders(Borders::LEFT)
                .border_style(faint())
                .style(shaded(Style::default(), app.ui.shade)),
            peek_area,
        );
        let inner = Rect {
            x: peek_area.x + 1,
            y: peek_area.y,
            width: peek_area.width.saturating_sub(1),
            height: peek_area.height,
        };
        pane(frame, inner, app, peer, pulse, "Esc closes", false);
    }
    band(frame, bottom, app, pulse);
    if app.ui.picker.is_some() {
        picker(frame, area, app, pulse);
    }
    if app.ui.help {
        help(frame, area);
    }
}

fn app_epoch() -> Instant {
    static EPOCH: std::sync::OnceLock<Instant> = std::sync::OnceLock::new();
    *EPOCH.get_or_init(Instant::now)
}
