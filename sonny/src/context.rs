//! `/context`: what fills an agent's context, read-only, as Claude Code's
//! `/context` shows it. Built from what the window already has: the
//! context the last model round read and the model's window (each `usage`
//! event), the agent's record (`inspect`: its instructions and tools), and
//! its last compaction (the `compacted` event's room, #718). No field of
//! the daemon's is added for it.
//!
//! The daemon counts the conversation in tokens, its parts in bytes: a
//! part's tokens here are its bytes over [`BYTES_PER_TOKEN`], an estimate
//! said as one (`≈`); the conversation is what the measured total leaves.
//!
//! Above the parts, Claude Code's grid: ten by ten cells, each a hundredth
//! of the window, filled part by part in the list's order. Each part has
//! its own shape (`▣` instructions, `▦` tools, `▨` a summary, `■` the
//! conversation, `□` free), so the grid reads with no color; its hue
//! repeats it, as the usage chart's fills do.
use ratatui::style::{Color, Stylize};
use ratatui::text::{Line, Span};
use serde_json::Value;

/// About how many bytes a token takes in English and code, as tokenizers
/// for these models average it.
const BYTES_PER_TOKEN: u64 = 4;

/// What `/context` reads for one agent.
#[derive(Default)]
pub struct Parts {
    /// The tokens its last model round read and wrote: what the next reads.
    pub used: Option<u64>,
    /// The model's context window, in tokens, when the daemon knew it.
    pub window: Option<u64>,
    /// Its instructions' bytes.
    pub instructions: u64,
    /// The tools it declares (Sonny's and MCP's): how many, their bytes.
    pub declared: (usize, u64),
    /// The daemon's own tools it may call, by name; their schemas are the
    /// daemon's, so only counted.
    pub built_in: usize,
    /// Whether its record names a compaction: what it reads starts with
    /// that compaction's summary.
    pub compaction: bool,
    /// That compaction's `compacted` data, when it could be read.
    pub compacted: Option<Value>,
}

impl Parts {
    /// The agent's record as `inspect` answers it.
    pub fn from_record(record: &Value) -> Self {
        let declared = record["declared"].as_array().map_or(0, Vec::len);
        let declared_bytes = record["declared"].to_string().len() as u64;
        Parts {
            instructions: record["instructions"].as_str().map_or(0, str::len) as u64,
            declared: (declared, if declared == 0 { 0 } else { declared_bytes }),
            built_in: record["tools"].as_array().map_or(0, Vec::len),
            compaction: record["compaction"].is_i64(),
            ..Parts::default()
        }
    }
}

fn tokens(bytes: u64) -> u64 {
    bytes.div_ceil(BYTES_PER_TOKEN)
}

/// Thousands apart, as `/usage` writes counts.
fn count(n: u64) -> String {
    let digits = n.to_string();
    let mut out = String::new();
    for (at, c) in digits.chars().enumerate() {
        if at > 0 && (digits.len() - at).is_multiple_of(3) {
            out.push(',');
        }
        out.push(c);
    }
    out
}

fn kb(bytes: u64) -> String {
    match bytes {
        0..1_024 => format!("{bytes} B"),
        _ => format!("{:.1} KB", bytes as f64 / 1024.0),
    }
}

/// A part of what fills the context, as the grid draws it: its own
/// shape first, which says it with no color, then its hue.
#[derive(Clone, Copy, Debug, PartialEq)]
enum Part {
    Instructions,
    Tools,
    Summary,
    Conversation,
    Free,
}

impl Part {
    fn cell(self) -> Span<'static> {
        match self {
            Part::Instructions => Span::raw("▣").fg(Color::Cyan),
            Part::Tools => Span::raw("▦").fg(Color::Blue),
            Part::Summary => Span::raw("▨").fg(Color::Magenta),
            Part::Conversation => Span::raw("■"),
            Part::Free => Span::raw("□").dim(),
        }
    }
}

/// The grid's cells, row by row: [`CELLS`] of them, each about a
/// hundredth of the window, as Claude Code's `/context` draws it.
const COLUMNS: usize = 10;
const CELLS: usize = 100;

/// Each part's cells of [`CELLS`], in order, for its tokens of `window`:
/// each part ends where the tokens up to its end round to, so together
/// they fill what the total does, each within a cell of its share; a
/// part that holds anything takes one cell at least; the rest is free.
fn cells(parts: &[(Part, u64)], window: u64) -> Vec<Part> {
    let window = u128::from(window.max(1));
    let mut out = Vec::with_capacity(CELLS);
    let mut sum = 0u128;
    for &(part, tokens) in parts {
        sum += u128::from(tokens);
        let end = ((sum * CELLS as u128 + window / 2) / window).min(CELLS as u128) as usize;
        let end = match tokens > 0 {
            true => end.max(out.len() + 1).min(CELLS),
            false => end,
        };
        out.resize(end.max(out.len()), part);
    }
    out.resize(CELLS, Part::Free);
    out
}

/// The grid's rows: [`COLUMNS`] cells each, a space between.
fn grid(cells: &[Part]) -> Vec<Line<'static>> {
    cells
        .chunks(COLUMNS)
        .map(|row| {
            let mut spans = Vec::new();
            for (at, part) in row.iter().enumerate() {
                if at > 0 {
                    spans.push(Span::raw(" "));
                }
                spans.push(part.cell());
            }
            Line::from(spans)
        })
        .collect()
}

/// The grid's width in columns.
const GRID_WIDTH: usize = COLUMNS * 2 - 1;
/// Columns between the grid and the list beside it.
const GAP: usize = 3;

/// The card at `width`: the total against the window; a grid of what
/// fills it, a cell to each hundredth, as Claude Code's `/context` draws
/// one, with the parts listed beside it (under it when too narrow), each
/// after its cell; then each part's tokens and what it is.
pub fn card(name: &str, parts: &Parts, width: usize) -> Vec<Line<'static>> {
    let mut lines = vec![Line::from(vec![
        Span::raw(" "),
        Span::raw(name.to_owned()).bold(),
    ])];
    let summary = (parts.compacted.as_ref()).and_then(|data| data["summary_bytes"].as_u64());
    let fixed = tokens(parts.instructions) + tokens(parts.declared.1) + summary.map_or(0, tokens);
    let total = match (parts.used, parts.window) {
        (Some(used), Some(window)) if window > 0 => Line::from(format!(
            "{} of {} tokens used ({}%), {} free",
            count(used),
            count(window),
            used * 100 / window,
            count(window.saturating_sub(used))
        )),
        (Some(used), _) => Line::from(format!(
            "{} tokens used; the model's window is not known yet",
            count(used)
        )),
        (None, _) => {
            Line::from("No model call yet: the parts below are what its first one reads.").dim()
        }
    };
    // Each part's row: what it is, its tokens in a column, and what it holds.
    let mut rows: Vec<(Part, &str, String, String)> = Vec::new();
    rows.push((
        Part::Instructions,
        "Instructions",
        format!("≈{}", count(tokens(parts.instructions))),
        kb(parts.instructions),
    ));
    let (declared, declared_bytes) = parts.declared;
    let built_in = match parts.built_in {
        0 => String::new(),
        n => format!(", {n} built in"),
    };
    rows.push((
        Part::Tools,
        "Tools",
        format!("≈{}", count(tokens(declared_bytes))),
        format!("{declared} declared, {}{built_in}", kb(declared_bytes)),
    ));
    match (summary, parts.compacted.as_ref()) {
        (Some(bytes), Some(data)) => {
            let turns = &data["covered_turns"];
            let covered = match (turns[0].as_i64(), turns[1].as_i64()) {
                (Some(from), Some(to)) if from < to => format!("turns {from} to {to}"),
                (_, Some(to)) => format!("turn {to}"),
                _ => "earlier turns".to_owned(),
            };
            rows.push((
                Part::Summary,
                "Summary",
                format!("≈{}", count(tokens(bytes))),
                format!("{covered}, {}", kb(bytes)),
            ));
        }
        // Named and not found: the summary is read, its size unknown here,
        // and counted with the conversation.
        _ if parts.compaction => rows.push((
            Part::Summary,
            "Summary",
            "?".into(),
            "a compaction's, its size not recorded; in the conversation".into(),
        )),
        _ => rows.push((
            Part::Summary,
            "Summary",
            "0".into(),
            "no compaction yet".into(),
        )),
    }
    if let Some(used) = parts.used {
        rows.push((
            Part::Conversation,
            "Conversation",
            format!("≈{}", count(used.saturating_sub(fixed))),
            "messages, calls and results".into(),
        ));
    }
    // The grid, once both the total and the window are known.
    let drawn = match (parts.used, parts.window) {
        (Some(used), Some(window)) if window > 0 => {
            rows.push((
                Part::Free,
                "Free",
                count(window.saturating_sub(used)),
                "before the window fills".into(),
            ));
            let filled = [
                (Part::Instructions, tokens(parts.instructions)),
                (Part::Tools, tokens(declared_bytes)),
                (Part::Summary, summary.map_or(0, tokens)),
                (Part::Conversation, used.saturating_sub(fixed)),
            ];
            Some((grid(&cells(&filled, window)), window))
        }
        _ => None,
    };
    let row = |(part, label, tokens, about): &(Part, &str, String, String)| {
        let mut spans = Vec::new();
        if drawn.is_some() {
            spans.extend([part.cell(), Span::raw(" ")]);
        }
        spans.extend([
            Span::raw(format!("{label:<13}")),
            Span::raw(format!("{tokens:>8}")),
            Span::raw(format!("  {about}")).dim(),
        ]);
        Line::from(spans)
    };
    let mut beside: Vec<Line<'static>> = vec![total, Line::default()];
    beside.extend(rows.iter().map(row));
    let widest = beside.iter().map(Line::width).max().unwrap_or(0);
    match &drawn {
        // The list beside the grid, its total level with the grid's top.
        Some((grid, _)) if 1 + GRID_WIDTH + GAP + widest <= width => {
            for at in 0..grid.len().max(beside.len()) {
                let mut spans = vec![Span::raw(" ")];
                match grid.get(at) {
                    Some(cells) => spans.extend(cells.spans.iter().cloned()),
                    None => spans.push(Span::raw(" ".repeat(GRID_WIDTH))),
                }
                if let Some(said) = beside.get(at) {
                    spans.push(Span::raw(" ".repeat(GAP)));
                    spans.extend(said.spans.iter().cloned().map(|span| {
                        let style = said.style.patch(span.style);
                        span.style(style)
                    }));
                }
                lines.push(Line::from(spans));
            }
        }
        // Too narrow for both: the total, the grid, then the list.
        Some((grid, _)) => {
            let mut said = beside.into_iter();
            lines.extend(said.next().map(indent));
            lines.push(Line::default());
            lines.extend(grid.iter().cloned().map(indent));
            lines.extend(said.map(indent));
        }
        None => lines.extend(beside.into_iter().map(indent)),
    }
    if let Some(data) = &parts.compacted {
        lines.push(Line::default());
        let mut said = format!(
            " Last compaction freed {}",
            kb(data["reclaimed_bytes"].as_u64().unwrap_or(0))
        );
        if let Some(room) = data["room_bytes"].as_i64().filter(|room| *room > 0) {
            said.push_str(&format!(", and left {} before the next", kb(room as u64)));
        }
        said.push('.');
        lines.push(Line::from(said).dim());
    }
    lines.push(Line::default());
    if let Some((_, window)) = &drawn {
        lines.push(
            Line::from(format!(
                " A cell is a hundredth of the window, {} tokens.",
                count(window.div_ceil(CELLS as u64))
            ))
            .dim(),
        );
    }
    lines.push(
        Line::from(format!(
            " ≈ at {BYTES_PER_TOKEN} bytes a token; the total is the model's count."
        ))
        .dim(),
    );
    lines
}

/// A line a column in, as the card's rows are.
fn indent(line: Line<'static>) -> Line<'static> {
    let style = line.style;
    let mut spans = vec![Span::raw(" ")];
    spans.extend(line.spans);
    Line::from(spans).style(style)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn text(lines: &[Line]) -> Vec<String> {
        lines
            .iter()
            .map(|line| line.to_string().trim_end().to_owned())
            .collect()
    }

    #[test]
    fn the_card_counts_each_part_and_leaves_the_conversation_the_rest() {
        let record = json!({"instructions": "x".repeat(8_000), "tools": ["shell", "apply_patch"],
            "declared": [{"name": "browser", "description": "y".repeat(3_000)}]});
        let mut parts = Parts::from_record(&record);
        parts.used = Some(50_000);
        parts.window = Some(200_000);
        parts.compacted = Some(json!({"summary_bytes": 4_000, "covered_turns": [1, 12],
            "reclaimed_bytes": 120_000, "room_bytes": 300_000}));
        // Too narrow for the list beside the grid: the list under it.
        let shown = text(&card("tally.lead", &parts, 60));
        assert_eq!(
            shown[1],
            " 50,000 of 200,000 tokens used (25%), 150,000 free"
        );
        let at = |label: &str| {
            (shown.iter())
                .position(|row| row.starts_with(&format!(" {label}")))
                .unwrap_or_else(|| panic!("{label}: {shown:?}"))
        };
        assert!(shown[at("▣ Instructions")].ends_with("≈2,000  7.8 KB"));
        let tools = &shown[at("▦ Tools")];
        assert!(tools.contains("1 declared") && tools.ends_with("2 built in"));
        assert!(shown[at("▨ Summary")].contains("≈1,000  turns 1 to 12, 3.9 KB"));
        // The rest of the measured total, after the parts' estimates.
        let tools = tokens(parts.declared.1);
        let rest = 50_000 - 2_000 - tools - 1_000;
        assert!(shown[at("■ Conversation")].contains(&format!("≈{}", count(rest))));
        assert!(shown[at("□ Free")].contains("150,000"));
        assert!(shown.iter().any(
            |row| row == " Last compaction freed 117.2 KB, and left 293.0 KB before the next."
        ));
        assert!(shown.iter().any(|row| row == " A cell is a hundredth of the window, 2,000 tokens."));
        // A grid of ten by ten under the total: a quarter used.
        let grid: Vec<&String> = shown[3..13].iter().collect();
        assert!(grid.iter().all(|row| row.chars().filter(|c| *c != ' ').count() == 10));
        let filled: usize = (grid.iter())
            .map(|row| row.chars().filter(|c| !matches!(c, ' ' | '□')).count())
            .sum();
        assert_eq!(filled, 25);
        assert_eq!(grid[0].trim(), "▣ ▦ ▨ ■ ■ ■ ■ ■ ■ ■");
        assert_eq!(grid[9].trim(), "□ □ □ □ □ □ □ □ □ □");
        // Wide enough: the list beside the grid, its total at the top.
        let wide = text(&card("tally.lead", &parts, 100));
        assert_eq!(
            wide[1],
            " ▣ ▦ ▨ ■ ■ ■ ■ ■ ■ ■   50,000 of 200,000 tokens used (25%), 150,000 free"
        );
        assert!(wide[3].starts_with(" ■ ■ ■ ■ ■ □ □ □ □ □   ▣ Instructions"));
        assert!(wide[7].starts_with(" □ □ □ □ □ □ □ □ □ □   □ Free"));
        assert_eq!(wide[10].trim(), "□ □ □ □ □ □ □ □ □ □");
        // Each cell says its part by its shape, its hue repeating it.
        let lines = card("tally.lead", &parts, 100);
        let hue = |cell: &str| {
            (lines[1].spans.iter())
                .find(|span| span.content == cell)
                .and_then(|span| span.style.fg)
        };
        assert_eq!(hue("▣"), Some(Color::Cyan));
        assert_eq!(hue("▦"), Some(Color::Blue));
        assert_eq!(hue("▨"), Some(Color::Magenta));
        assert_eq!(hue("■"), None);
    }

    #[test]
    fn the_grid_fills_what_the_total_does_each_part_one_cell_at_least() {
        let filled = |parts: &[(Part, u64)], window| {
            let cells = cells(parts, window);
            assert_eq!(cells.len(), CELLS);
            [
                Part::Instructions,
                Part::Tools,
                Part::Summary,
                Part::Conversation,
                Part::Free,
            ]
            .map(|part| cells.iter().filter(|cell| **cell == part).count())
        };
        // A tiny part still shows; together they round as the total does.
        assert_eq!(
            filled(
                &[
                    (Part::Instructions, 300),
                    (Part::Tools, 0),
                    (Part::Summary, 0),
                    (Part::Conversation, 239_700),
                ],
                1_000_000
            ),
            [1, 0, 0, 23, 76]
        );
        assert_eq!(
            filled(
                &[(Part::Instructions, 2_000), (Part::Conversation, 47_400)],
                200_000
            ),
            [1, 0, 0, 24, 75]
        );
        // Over the window, an estimate too high: no free cells, never more.
        assert_eq!(
            filled(&[(Part::Conversation, 300_000)], 200_000),
            [0, 0, 0, 100, 0]
        );
        assert_eq!(filled(&[], 0), [0, 0, 0, 0, 100]);
    }

    #[test]
    fn a_compaction_named_and_not_found_is_not_said_to_be_none() {
        let record = json!({"instructions": "", "tools": [], "declared": [], "compaction": 9});
        let mut parts = Parts::from_record(&record);
        parts.used = Some(1_000);
        let shown = text(&card("shop.side", &parts, 100));
        assert!(
            shown
                .iter()
                .any(|row| row.starts_with(" Summary") && row.contains("?  a compaction's")),
            "{shown:?}"
        );
        assert!(!shown.iter().any(|row| row.contains("no compaction")));
    }

    #[test]
    fn before_any_call_it_says_so_and_counts_what_the_first_reads() {
        let parts =
            Parts::from_record(&json!({"instructions": "abcd", "tools": [], "declared": []}));
        let shown = text(&card("calc.fix", &parts, 100));
        assert!(shown[1].starts_with(" No model call yet"));
        assert!(
            shown
                .iter()
                .any(|row| row.starts_with(" Tools") && row.contains("0 declared, 0 B"))
        );
        assert!(shown.iter().any(|row| row.contains("no compaction yet")));
        assert!(!shown.iter().any(|row| row.starts_with(" Free")));
    }
}
