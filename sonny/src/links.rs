//! Web addresses the person can click: rows written into the scrollback
//! are drawn again over themselves with an OSC 8 hyperlink around each
//! address, which terminals open on a click (Cmd-click in most). Ratatui
//! drops control characters from text, so the escapes go on the cells
//! once the rows are drawn, and only those cells are written again.
use ratatui::buffer::{Buffer, Cell};

/// The cells of `buf` that show a web address, each address's first cell
/// opening its hyperlink and its last closing it, by column and row. An
/// address that runs to a row's end goes on past the indent of each row
/// `cut` marks as going on with it, and every part opens the whole
/// address. One with a character an address does not hold as such, like
/// `é`, is left unlinked rather than linked short.
pub fn cells(buf: &Buffer, cut: &[bool]) -> Vec<(u16, u16, Cell)> {
    let area = buf.area;
    let rows: Vec<Vec<&str>> = (0..area.height)
        .map(|y| {
            (0..area.width)
                .map(|x| buf[(area.x + x, area.y + y)].symbol())
                .collect()
        })
        .collect();
    let mut out = Vec::new();
    // Where each row's search starts: past the part of an address above.
    let mut from_x = vec![0; rows.len()];
    for y in 0..rows.len() {
        let row = &rows[y];
        let mut x = from_x[y];
        while let Some(start) = find(row, x) {
            let end = run(row, start);
            x = end.max(start + 1);
            let mut parts = vec![(y, start, end)];
            let mut url: String = row[start..end].concat();
            // Cut at a row's end: the rest goes on after the next's indent.
            let (mut last_row, mut last_end) = (y, end);
            while rows[last_row][last_end..].iter().all(|s| *s == " ")
                && cut.get(last_row + 1) == Some(&true)
            {
                let next = &rows[last_row + 1];
                let from = next.iter().take_while(|s| **s == " ").count();
                let to = run(next, from);
                if to == from {
                    break;
                }
                url.push_str(&next[from..to].concat());
                parts.push((last_row + 1, from, to));
                (last_row, last_end) = (last_row + 1, to);
                from_x[last_row] = to;
            }
            if !ends(&rows[last_row], last_end) {
                continue;
            }
            let url = trim(&url);
            // What trimming took off comes off the last part's end.
            let mut over = parts.iter().map(|(_, s, e)| e - s).sum::<usize>() - url.chars().count();
            while over > 0 {
                let last = parts.last_mut().unwrap();
                let take = over.min(last.2 - last.1);
                last.2 -= take;
                over -= take;
                if last.2 == last.1 {
                    parts.pop();
                }
            }
            for (row_at, from, to) in parts {
                for at in from..to {
                    let mut cell = buf[(area.x + at as u16, area.y + row_at as u16)].clone();
                    let mut symbol = cell.symbol().to_owned();
                    if at == from {
                        symbol = format!("\x1b]8;;{url}\x1b\\{symbol}");
                    }
                    if at + 1 == to {
                        symbol.push_str("\x1b]8;;\x1b\\");
                    }
                    cell.set_symbol(&symbol);
                    out.push((area.x + at as u16, area.y + row_at as u16, cell));
                }
            }
        }
    }
    out
}

/// Whether an address that [`run`] stopped at `end` of `row` ends there,
/// rather than at a character it holds that `run` does not follow.
fn ends(row: &[&str], end: usize) -> bool {
    row.get(end)
        .is_none_or(|symbol| symbol.is_ascii() || symbol.trim().is_empty())
}

/// Where the next address starts in `row`, from `from`.
fn find(row: &[&str], from: usize) -> Option<usize> {
    (from..row.len()).find(|&at| {
        let rest: String = row[at..row.len().min(at + 8)].concat();
        (rest.starts_with("https://") || rest.starts_with("http://"))
            && (at == 0 || !row[at - 1].chars().all(char::is_alphanumeric))
    })
}

/// Where the address starting at `start` ends: at a space, or at a
/// character an address does not hold or is not plain ASCII.
fn run(row: &[&str], start: usize) -> usize {
    (start..row.len())
        .find(|&at| {
            let symbol = row[at];
            symbol.chars().count() != 1
                || symbol
                    .chars()
                    .any(|c| c.is_whitespace() || !c.is_ascii_graphic() || "<>\"`".contains(c))
        })
        .unwrap_or(row.len())
}

/// The address without what ends its sentence: trailing punctuation, and
/// a closing bracket it does not open.
fn trim(url: &str) -> String {
    let mut url = url.to_owned();
    while let Some(last) = url.chars().last() {
        let opens = |open| url.matches(open).count() >= url.matches(last).count();
        let keep = match last {
            '.' | ',' | ';' | ':' | '!' | '?' | '\'' => false,
            ')' => opens('('),
            ']' => opens('['),
            _ => true,
        };
        if keep {
            break;
        }
        url.pop();
    }
    url
}

#[cfg(test)]
mod tests {
    use super::*;
    use ratatui::{layout::Rect, text::Line, widgets::Paragraph, widgets::Widget};

    fn drawn(rows: &[&str], width: u16) -> Buffer {
        draw(
            rows.iter().map(|r| Line::from(r.to_string())).collect(),
            width,
        )
    }

    fn draw(lines: Vec<Line<'static>>, width: u16) -> Buffer {
        let mut buf = Buffer::empty(Rect::new(0, 0, width, lines.len() as u16));
        Paragraph::new(lines).render(buf.area, &mut buf);
        buf
    }

    /// Each link as its row, columns and address, with the rows after the
    /// first marked as cut where `cut` says.
    fn links(buf: &Buffer, cut: &[bool]) -> Vec<(u16, u16, u16, String)> {
        let mut out = Vec::new();
        for (x, y, cell) in cells(buf, cut) {
            if let Some(rest) = cell.symbol().strip_prefix("\x1b]8;;") {
                let url = rest.split('\x1b').next().unwrap().to_owned();
                out.push((y, x, x, url));
            }
            if cell.symbol().ends_with("\x1b]8;;\x1b\\") {
                out.last_mut().unwrap().2 = x;
            }
        }
        out
    }

    #[test]
    fn an_address_in_a_row_is_linked_without_its_sentence_end() {
        let buf = drawn(
            &["• See https://github.com/lydakis/agent/pull/172. Or (https://x.io/a_(b))."],
            80,
        );
        assert_eq!(
            links(&buf, &[false]),
            [
                (
                    0,
                    6,
                    46,
                    "https://github.com/lydakis/agent/pull/172".to_owned()
                ),
                (0, 53, 70, "https://x.io/a_(b)".to_owned()),
            ]
        );
    }

    #[test]
    fn an_address_cut_over_rows_links_every_part() {
        let url = "https://example.com/a/very/long/path/that/takes/three/rows?x=1";
        let line = Line::from(format!("  see {url} then"));
        let rows = crate::transcript::wrap(line, 24, "".into(), "    ".into());
        let cut: Vec<bool> = rows
            .iter()
            .map(|row| row.alignment == Some(crate::transcript::CUT))
            .collect();
        assert_eq!(cut, [false, false, true, true, true]);
        let found = links(&draw(rows, 24), &cut);
        assert_eq!(
            found.iter().map(|(y, ..)| *y).collect::<Vec<_>>(),
            [1, 2, 3, 4]
        );
        assert!(found.iter().all(|link| link.3 == url), "{found:?}");
    }

    #[test]
    fn an_address_at_a_row_end_takes_no_next_line_it_was_not_cut_into() {
        let buf = drawn(&["  https://example.com/123456789", "  Next step"], 31);
        assert_eq!(
            links(&buf, &[false, false]),
            [(0, 2, 30, "https://example.com/123456789".to_owned())]
        );
    }

    #[test]
    fn an_address_with_a_character_beyond_ascii_is_not_linked_short() {
        let buf = drawn(&["https://example.com/café and https://ok.io"], 50);
        assert_eq!(
            links(&buf, &[false]),
            [(0, 29, 41, "https://ok.io".to_owned())]
        );
        assert!(cells(&drawn(&["https://münchen.de/path"], 40), &[false]).is_empty());
    }

    #[test]
    fn text_without_an_address_has_no_links() {
        assert!(cells(&drawn(&["http is a protocol; xhttps://no"], 40), &[false]).is_empty());
    }
}
