//! A Markdown table, drawn with box lines: columns as wide as their
//! cells, narrowed to the pane by wrapping the widest, a bold header and
//! each column aligned as its separator says. A table too wide to read as
//! a grid at the pane's width is drawn as records instead, as Codex draws
//! one: each row a block of its columns' names and values.
use ratatui::{
    style::{Style, Stylize},
    text::{Line, Span},
};
use unicode_width::UnicodeWidthStr;

#[derive(Clone, Copy, PartialEq)]
enum Align {
    Left,
    Center,
    Right,
}

/// A row's cells, the outer pipes dropped; `\|` stays in its cell.
fn cells(row: &str) -> Vec<String> {
    let row = row.trim();
    let row = row.strip_prefix('|').unwrap_or(row);
    let row = row.strip_suffix('|').unwrap_or(row);
    let mut out = vec![String::new()];
    let mut chars = row.chars().peekable();
    while let Some(c) = chars.next() {
        match c {
            '\\' if chars.peek() == Some(&'|') => {
                out.last_mut().unwrap().push('|');
                chars.next();
            }
            '|' => out.push(String::new()),
            c => out.last_mut().unwrap().push(c),
        }
    }
    out.into_iter().map(|cell| cell.trim().to_owned()).collect()
}

/// `---`, `:--`, `--:` or `:-:` in every cell.
fn separator(cells: &[String]) -> Option<Vec<Align>> {
    cells
        .iter()
        .map(|cell| {
            let dashes = cell.trim_matches(':');
            (!dashes.is_empty() && dashes.chars().all(|c| c == '-')).then(|| {
                match (cell.starts_with(':'), cell.ends_with(':')) {
                    (true, true) => Align::Center,
                    (false, true) => Align::Right,
                    _ => Align::Left,
                }
            })
        })
        .collect()
}

/// The narrowest a column is drawn: past it, the table is read as records.
const LEAST: usize = 3;
/// The width a long column keeps while others give up theirs: enough for
/// a few words of prose or most of a path.
const FLOOR: usize = 16;
/// Under this width, a column whose cells run tall reads as a strip.
const STRIP: usize = 12;
/// How many rows tall a cell in a strip may run.
const STRIP_ROWS: usize = 7;
/// The least room a record's values get beside its labels; with less, each
/// label goes above its value.
const VALUES: usize = 24;

/// A table at `width`: a grid when its columns fit, a word cut only where
/// a column has room to spare; else, as Codex draws it, one record per
/// row, each value beside its column's name.
pub fn draw(rows: &[String], width: usize) -> Vec<Line<'static>> {
    let mut rows: Vec<Vec<String>> = rows.iter().map(|row| cells(row)).collect();
    let (header, aligns) = match rows.get(1).and_then(|row| separator(row)) {
        Some(aligns) => {
            rows.remove(1);
            (true, aligns)
        }
        None => (false, Vec::new()),
    };
    let columns = rows.iter().map(Vec::len).max().unwrap_or(0);
    if columns == 0 {
        return Vec::new();
    }
    let rows: Vec<Vec<Line<'static>>> = rows
        .into_iter()
        .enumerate()
        .map(|(r, mut row)| {
            row.resize(columns, String::new());
            let base = match header && r == 0 {
                true => Style::new().bold(),
                false => Style::new(),
            };
            (row.iter())
                .map(|cell| Line::from(crate::transcript::inline(cell, base)))
                .collect()
        })
        .collect();
    // Each column as wide as its widest cell, then narrowed until the
    // table fits: two of indent, a border per column and one more, and a
    // space either side of each cell.
    let natural: Vec<usize> = (0..columns)
        .map(|c| {
            rows.iter()
                .map(|row| row[c].width())
                .max()
                .unwrap_or(0)
                .max(1)
        })
        .collect();
    // What a column keeps while a wider one gives: all of a short one, a
    // long one's first words, and its name's longest word.
    let floors: Vec<usize> = (0..columns)
        .map(|c| {
            let named = match header {
                true => longest_word(&rows[0][c]),
                false => 0,
            };
            natural[c].min(FLOOR.max(named))
        })
        .collect();
    // What a column keeps before a word in it is cut: its longest word.
    let words: Vec<usize> = (0..columns)
        .map(|c| {
            rows.iter()
                .map(|row| longest_word(&row[c]))
                .max()
                .unwrap_or(0)
        })
        .collect();
    let room = width.saturating_sub(2 + columns * 3 + 1);
    let body = &rows[usize::from(header)..];
    match fit(&natural, &[&floors, &words], room) {
        Some(widths) if !cramped(body, &widths, &floors) && !(header && cut(&rows[0], &widths)) => {
            grid(&rows, header, &aligns, &widths)
        }
        _ if header && !body.is_empty() => records(Some(&rows[0]), body, width),
        _ => records(None, &rows, width),
    }
}

/// The columns' widths, `natural` narrowed until they sum to `room`, to
/// each of `floors` in turn and then to [`LEAST`]: the ones with most to
/// spare above a floor first. None when even [`LEAST`] is too wide.
fn fit(natural: &[usize], floors: &[&[usize]], room: usize) -> Option<Vec<usize>> {
    if natural.len() * LEAST > room {
        return None;
    }
    let mut widths = natural.to_vec();
    for floors in floors {
        let floors: Vec<usize> = floors.iter().map(|f| (*f).max(LEAST)).collect();
        narrow(&mut widths, &floors, room);
    }
    narrow(&mut widths, &vec![LEAST; natural.len()], room);
    Some(widths)
}

/// `widths` narrowed until they sum to `room` or each is at its floor,
/// the ones furthest above their floors first, evened out: each held to
/// the least spare above its floor that frees enough, found by halving,
/// so a cell of any length costs no more than a short one.
fn narrow(widths: &mut [usize], floors: &[usize], room: usize) {
    let total: usize = widths.iter().sum();
    let spare = |c: usize, widths: &[usize]| widths[c].saturating_sub(floors[c]);
    let all: usize = (0..widths.len()).map(|c| spare(c, widths)).sum();
    let excess = total.saturating_sub(room).min(all);
    if excess == 0 {
        return;
    }
    let freed = |cap: usize, widths: &[usize]| -> usize {
        (0..widths.len())
            .map(|c| spare(c, widths).saturating_sub(cap))
            .sum()
    };
    let (mut low, mut high) = (
        0,
        (0..widths.len())
            .map(|c| spare(c, widths))
            .max()
            .unwrap_or(0),
    );
    while low < high {
        let cap = low + (high - low) / 2;
        match freed(cap, widths) > excess {
            true => low = cap + 1,
            false => high = cap,
        }
    }
    let mut left = excess - freed(low, widths);
    for c in 0..widths.len() {
        widths[c] -= spare(c, widths).saturating_sub(low);
    }
    // What the cap leaves, a column at a time from the left, among those
    // still at it.
    for c in 0..widths.len() {
        if left > 0 && low > 0 && spare(c, widths) == low {
            widths[c] -= 1;
            left -= 1;
        }
    }
}

/// Whether the grid would no longer read, as Codex judges it: enough rows
/// (the only one, or two and a third of them) with a word cut in a column
/// narrowed past its floor, or a cell run into a tall, thin strip.
fn cramped(body: &[Vec<Line<'static>>], widths: &[usize], floors: &[usize]) -> bool {
    let hurt = body
        .iter()
        .filter(|row| {
            row.iter().enumerate().any(|(c, cell)| {
                let cut = widths[c] < floors[c] && longest_word(cell) > widths[c];
                let strip = widths[c] < STRIP
                    && crate::transcript::wrap_cell(cell.clone(), widths[c]).len() >= STRIP_ROWS;
                cut || strip
            })
        })
        .count();
    let enough = match body.len() {
        1 => 1,
        rows => rows.div_ceil(3).max(2),
    };
    !body.is_empty() && hurt >= enough
}

/// Whether a word of a row's is cut by its column: a column's name is
/// read whole, or the table as records.
fn cut(row: &[Line<'static>], widths: &[usize]) -> bool {
    row.iter()
        .zip(widths)
        .any(|(cell, w)| longest_word(cell) > *w)
}

/// The widest word in a cell.
fn longest_word(cell: &Line) -> usize {
    let text: String = cell.spans.iter().map(|s| s.content.as_ref()).collect();
    text.split_whitespace()
        .map(|w| w.width())
        .max()
        .unwrap_or(0)
}

/// The table in box lines, each cell wrapped to its column.
fn grid(
    rows: &[Vec<Line<'static>>],
    header: bool,
    aligns: &[Align],
    widths: &[usize],
) -> Vec<Line<'static>> {
    let rule = |left: &str, mid: &str, right: &str| {
        let parts: Vec<String> = widths.iter().map(|w| "─".repeat(w + 2)).collect();
        Line::from(format!("  {left}{}{right}", parts.join(mid))).dim()
    };
    let mut out = vec![rule("┌", "┬", "┐")];
    for (r, row) in rows.iter().enumerate() {
        // Each cell wrapped to its column, then the row as tall as its tallest.
        let wrapped: Vec<Vec<Line<'static>>> = row
            .iter()
            .zip(widths)
            .map(|(cell, w)| fitted(cell, *w))
            .collect();
        let height = wrapped.iter().map(Vec::len).max().unwrap_or(1);
        for i in 0..height {
            let mut spans = vec![Span::raw("  "), Span::raw("│").dim()];
            for (c, lines) in wrapped.iter().enumerate() {
                let line = lines.get(i).cloned().unwrap_or_default();
                let fill = widths[c].saturating_sub(line.width());
                let (before, after) = match aligns.get(c).copied().unwrap_or(Align::Left) {
                    Align::Left => (0, fill),
                    Align::Right => (fill, 0),
                    Align::Center => (fill / 2, fill - fill / 2),
                };
                spans.push(Span::raw(" ".repeat(before + 1)));
                spans.extend(line.spans);
                spans.push(Span::raw(" ".repeat(after + 1)));
                spans.push(Span::raw("│").dim());
            }
            out.push(Line::from(spans));
        }
        if header && r == 0 {
            out.push(rule("├", "┼", "┤"));
        }
    }
    out.push(rule("└", "┴", "┘"));
    out
}

/// A cell's text wrapped to `width`, each row's ends trimmed.
fn fitted(cell: &Line<'static>, width: usize) -> Vec<Line<'static>> {
    crate::transcript::wrap_cell(cell.clone(), width)
        .into_iter()
        .map(|line| Line::from(trim_start(line.spans)))
        .collect()
}

/// A table too wide for a grid, one record per row between rules: each
/// value beside its column's name, or under it when the names leave the
/// values too little room.
fn records(
    names: Option<&[Line<'static>]>,
    body: &[Vec<Line<'static>>],
    width: usize,
) -> Vec<Line<'static>> {
    let inner = width.saturating_sub(2).max(1);
    let named = names.map_or(0, |names| names.iter().map(Line::width).max().unwrap_or(0));
    let beside = named + 2 + VALUES <= inner;
    let mut out = Vec::new();
    // A rule above each record and below the last, so the records stand
    // apart from the prose around them as a grid's borders do.
    let rule = || Line::from(format!("  {}", "─".repeat(inner))).dim();
    for row in body {
        out.push(rule());
        for (c, value) in row.iter().enumerate() {
            let name = names
                .and_then(|names| names.get(c))
                .filter(|n| n.width() > 0);
            match (name.cloned(), beside) {
                (Some(name), true) => {
                    let pad = named - name.width() + 2;
                    let mut name = name.spans;
                    name.push(Span::raw(" ".repeat(pad)));
                    for (i, line) in fitted(value, inner - named - 2).into_iter().enumerate() {
                        let mut spans = vec![Span::raw("  ")];
                        match i {
                            0 => spans.extend(name.clone()),
                            _ => spans.push(Span::raw(" ".repeat(named + 2))),
                        }
                        spans.extend(line.spans);
                        out.push(Line::from(spans));
                    }
                }
                (name, beside) => {
                    let indent = match name {
                        Some(name) => {
                            for line in fitted(&name, inner) {
                                out.push(Line::from([vec![Span::raw("  ")], line.spans].concat()));
                            }
                            4
                        }
                        // A column with no name keeps to the others' values.
                        None if beside && named > 0 => named + 4,
                        None => 2,
                    };
                    let room = width.saturating_sub(indent).max(1);
                    for line in fitted(value, room) {
                        let spans = [vec![Span::raw(" ".repeat(indent))], line.spans].concat();
                        out.push(Line::from(spans));
                    }
                }
            }
        }
    }
    out.push(rule());
    out
}

/// A wrapped row's spaces at either end, left where a word broke,
/// dropped: a trailing one would push the cell past its column.
fn trim_start(mut spans: Vec<Span<'static>>) -> Vec<Span<'static>> {
    while let Some(first) = spans.first_mut() {
        let trimmed = first.content.trim_start().to_owned();
        if trimmed.is_empty() {
            spans.remove(0);
        } else {
            first.content = trimmed.into();
            break;
        }
    }
    while let Some(last) = spans.last_mut() {
        let trimmed = last.content.trim_end().to_owned();
        if trimmed.is_empty() {
            spans.pop();
        } else {
            last.content = trimmed.into();
            break;
        }
    }
    spans
}

#[cfg(test)]
mod tests {
    use super::*;

    fn text(lines: &[Line]) -> Vec<String> {
        lines
            .iter()
            .map(|l| l.spans.iter().map(|s| s.content.as_ref()).collect())
            .collect()
    }

    #[test]
    fn a_table_is_boxed_with_its_header_and_alignment() {
        let rows = [
            "| Agent | Turns |",
            "|:--|--:|",
            "| lead | 3 |",
            "| `zero` | 12 |",
        ]
        .map(String::from);
        assert_eq!(
            text(&draw(&rows, 80)),
            [
                "  ┌───────┬───────┐",
                "  │ Agent │ Turns │",
                "  ├───────┼───────┤",
                "  │ lead  │     3 │",
                "  │ zero  │    12 │",
                "  └───────┴───────┘",
            ]
        );
    }

    #[test]
    fn a_wide_table_wraps_its_widest_column_to_fit() {
        let rows = [
            "| Name | Note |",
            "|---|---|",
            "| lead | coordinates every task in this project and reports back |",
        ]
        .map(String::from);
        let lines = draw(&rows, 40);
        assert!(lines.iter().all(|l| l.width() <= 40), "{:?}", text(&lines));
        let widths: Vec<usize> = lines.iter().map(|l| l.width()).collect();
        assert!(widths.iter().all(|w| *w == widths[0]), "{:?}", text(&lines));
        assert!(lines.len() > 6, "{:?}", text(&lines));
    }

    fn sample(rows: &[&str]) -> Vec<String> {
        rows.iter().map(|row| row.to_string()).collect()
    }

    #[test]
    fn no_table_runs_past_its_width() {
        let tables = [
            sample(&[
                "| Item | Status | Owner | PR | Notes |",
                "|---|---|---|---|---|",
                "| Scroll fix | ✅ merged | lead | #961 | Faster wheel scrolling in the conversation |",
                "| Startup | ❌ todo | — | — | `sonny/src/boot.rs` reads its config twice at start |",
            ]),
            sample(&[
                "| a | b | c | d | e | f | g | h | i | j | k | l |",
                "|---|---|---|---|---|---|---|---|---|---|---|---|",
                "| alpha beta | gamma delta | epsilon | eta theta | iota | lambda mu | nu | omicron | rho | tau | phi | psi |",
            ]),
            sample(&[
                "| Command | Example |",
                "|---|---|",
                "| `sonny sky` | `~/Developer/agent/a/really/long/path/that/never/breaks/at/all` |",
            ]),
            sample(&["| no | header | row |", "| just | cells | here |"]),
        ];
        for rows in &tables {
            for width in 12..=120 {
                let lines = draw(rows, width);
                assert!(
                    lines.iter().all(|l| l.width() <= width),
                    "width {width}: {:#?}",
                    text(&lines)
                );
            }
        }
    }

    #[test]
    fn a_table_too_wide_for_a_grid_reads_as_records() {
        let rows = sample(&[
            "| Name | Status | Owner | Branch | Checks | Reviewer | Merged | Notes |",
            "|---|---|---|---|---|---|---|---|",
            "| scroll | merged | lead | claude/scroll | passing | codex | yesterday | wheel |",
            "| queued | open | worker | claude/queued | failing | codex | never | notice |",
        ]);
        assert_eq!(
            text(&draw(&rows, 50)),
            [
                "  ────────────────────────────────────────────────",
                "  Name      scroll",
                "  Status    merged",
                "  Owner     lead",
                "  Branch    claude/scroll",
                "  Checks    passing",
                "  Reviewer  codex",
                "  Merged    yesterday",
                "  Notes     wheel",
                "  ────────────────────────────────────────────────",
                "  Name      queued",
                "  Status    open",
                "  Owner     worker",
                "  Branch    claude/queued",
                "  Checks    failing",
                "  Reviewer  codex",
                "  Merged    never",
                "  Notes     notice",
                "  ────────────────────────────────────────────────",
            ]
        );
        // At a width it fits, the same table is a grid.
        assert!(text(&draw(&rows, 120))[0].starts_with("  ┌"));
    }

    #[test]
    fn records_put_long_names_above_their_values() {
        let rows = sample(&[
            "| The first column's long name | The second column's long name |",
            "|---|---|",
            "| one two three four five six seven | eight |",
        ]);
        assert_eq!(
            text(&draw(&rows, 24)),
            [
                "  ──────────────────────",
                "  The first column's",
                "  long name",
                "    one two three four",
                "    five six seven",
                "  The second column's",
                "  long name",
                "    eight",
                "  ──────────────────────",
            ]
        );
    }

    #[test]
    fn a_long_path_in_a_wide_column_is_cut_in_the_grid() {
        let rows = sample(&[
            "| File | Lines |",
            "|---|--:|",
            "| sonny/src/some/deeply/nested/folder/with/a/long/name.rs | 12 |",
        ]);
        let lines = text(&draw(&rows, 40));
        assert!(lines[0].starts_with("  ┌"), "{lines:#?}");
        assert!(lines.iter().all(|l| l.chars().count() <= 40), "{lines:#?}");
    }

    #[test]
    fn columns_narrow_evenly_from_the_widest() {
        let mut widths = [10, 20, 30];
        narrow(&mut widths, &[3, 3, 3], 30);
        assert_eq!(widths, [10, 10, 10]);
        let mut widths = [10, 20, 31];
        narrow(&mut widths, &[3, 3, 3], 30);
        assert_eq!(widths, [10, 10, 10]);
        let mut widths = [10, 20, 30];
        narrow(&mut widths, &[3, 3, 3], 29);
        assert_eq!(widths, [9, 10, 10]);
        let mut widths = [5, 1_000_000];
        narrow(&mut widths, &[5, 16], 20);
        assert_eq!(widths, [5, 16]);
    }
}
