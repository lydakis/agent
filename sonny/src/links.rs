//! Web addresses the person can click: rows written into the scrollback
//! are drawn again over themselves with an OSC 8 hyperlink around each
//! address, which terminals open on a click (Cmd-click in most). Ratatui
//! drops control characters from text, so the escapes go on the cells
//! once the rows are drawn, and only those cells are written again.
use ratatui::buffer::{Buffer, Cell};

/// The cells of `buf` that show a web address, each address's first cell
/// opening its hyperlink and its last closing it, by column and row. An
/// address cut at the edge of a row goes on in the next one, past its
/// indent, and both parts open the whole address.
pub fn cells(buf: &Buffer) -> Vec<(u16, u16, Cell)> {
    let area = buf.area;
    let rows: Vec<Vec<&str>> = (0..area.height)
        .map(|y| {
            (0..area.width)
                .map(|x| buf[(area.x + x, area.y + y)].symbol())
                .collect()
        })
        .collect();
    let mut out = Vec::new();
    // Where the next row's search starts: past the rest of an address.
    let mut carried = 0;
    for y in 0..rows.len() {
        let row = &rows[y];
        let mut x = std::mem::take(&mut carried);
        while let Some(start) = find(row, x) {
            let end = run(row, start);
            let mut parts = vec![(y, start, end)];
            let mut url: String = row[start..end].concat();
            // Cut at the edge: the rest starts the next row, after spaces.
            if end == row.len()
                && let Some(next) = rows.get(y + 1)
            {
                let from = next.iter().take_while(|s| **s == " ").count();
                let to = run(next, from);
                if from > 0 && to > from {
                    url.push_str(&next[from..to].concat());
                    parts.push((y + 1, from, to));
                    carried = to;
                }
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
            x = end;
        }
    }
    out
}

/// Where the next address starts in `row`, from `from`.
fn find(row: &[&str], from: usize) -> Option<usize> {
    (from..row.len()).find(|&at| {
        let rest: String = row[at..row.len().min(at + 8)].concat();
        (rest.starts_with("https://") || rest.starts_with("http://"))
            && (at == 0 || !row[at - 1].chars().all(char::is_alphanumeric))
    })
}

/// Where the address starting at `start` ends: at a space or a character
/// an address does not hold.
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
        let mut buf = Buffer::empty(Rect::new(0, 0, width, rows.len() as u16));
        let lines: Vec<Line> = rows.iter().map(|r| Line::from(*r)).collect();
        Paragraph::new(lines).render(buf.area, &mut buf);
        buf
    }

    /// Each link as its row, columns and address.
    fn links(buf: &Buffer) -> Vec<(u16, u16, u16, String)> {
        let mut out = Vec::new();
        for (x, y, cell) in cells(buf) {
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
            links(&buf),
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
    fn an_address_cut_at_the_edge_links_both_parts() {
        let buf = drawn(
            &["  go https://example.com/a/very", "    /long/path then"],
            31,
        );
        assert_eq!(
            links(&buf),
            [
                (0, 5, 30, "https://example.com/a/very/long/path".to_owned()),
                (1, 4, 13, "https://example.com/a/very/long/path".to_owned()),
            ]
        );
    }

    #[test]
    fn text_without_an_address_has_no_links() {
        assert!(cells(&drawn(&["http is a protocol; xhttps://no"], 40)).is_empty());
    }
}
