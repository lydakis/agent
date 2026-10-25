//! A Markdown table, drawn with box lines: columns as wide as their
//! cells, narrowed to the pane by wrapping the widest, a bold header and
//! each column aligned as its separator says.
use ratatui::{
    style::{Style, Stylize},
    text::{Line, Span},
};

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
    for row in &mut rows {
        row.resize(columns, String::new());
    }
    let styled = |text: &str, bold: bool| {
        let base = if bold {
            Style::new().bold()
        } else {
            Style::new()
        };
        Line::from(crate::transcript::inline(text, base))
    };
    // Each column as wide as its widest cell, then the widest narrowed
    // until the table fits: two of indent, a border per column and one
    // more, and a space either side of each cell.
    let mut widths: Vec<usize> = (0..columns)
        .map(|c| {
            rows.iter()
                .map(|row| styled(&row[c], false).width())
                .max()
                .unwrap_or(0)
                .max(1)
        })
        .collect();
    let room = width.saturating_sub(2 + columns * 3 + 1);
    while widths.iter().sum::<usize>() > room {
        let (widest, &most) = widths
            .iter()
            .enumerate()
            .max_by_key(|(_, w)| **w)
            .expect("a column");
        if most <= 8 {
            break;
        }
        widths[widest] = most - 1;
    }
    let rule = |left: &str, mid: &str, right: &str| {
        let parts: Vec<String> = widths.iter().map(|w| "─".repeat(w + 2)).collect();
        Line::from(format!("  {left}{}{right}", parts.join(mid))).dim()
    };
    let mut out = vec![rule("┌", "┬", "┐")];
    for (r, row) in rows.iter().enumerate() {
        let bold = header && r == 0;
        // Each cell wrapped to its column, then the row as tall as its tallest.
        let wrapped: Vec<Vec<Line<'static>>> = row
            .iter()
            .zip(&widths)
            .map(|(cell, w)| {
                let lines = crate::transcript::wrap_lines(
                    styled(cell, bold),
                    *w,
                    Line::default(),
                    Line::default(),
                );
                lines
                    .into_iter()
                    .map(|line| Line::from(trim_start(line.spans)))
                    .collect()
            })
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
        if bold {
            out.push(rule("├", "┼", "┤"));
        }
    }
    out.push(rule("└", "┴", "┘"));
    out
}

/// A wrapped row's leading space, left where a word broke, dropped.
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
        assert!(lines.len() > 6, "{:?}", text(&lines));
    }
}
