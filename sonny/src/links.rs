//! Addresses the person can click: rows written into the scrollback are
//! drawn again over themselves with an OSC 8 hyperlink around each web
//! address, each path of a file that is there, and the underlined text of
//! a markdown link to either, which terminals open on a click (Cmd-click
//! in most, Ctrl-click in Herdr). Ratatui drops control characters from
//! text, so the escapes go on the cells once the rows are drawn, and only
//! those cells are written again.
use ratatui::{
    buffer::{Buffer, Cell},
    style::Modifier,
};
use std::path::Path;

/// Where a link shows: its row, and its first column and the one after its
/// last.
type Part = (usize, usize, usize);

/// The cells of `buf` that show a link, each part's first cell opening its
/// hyperlink and its last closing it, by column and row. An address or
/// path that runs to a row's end goes on past the indent of each row `cut`
/// marks as going on with it, and every part opens the whole address. One
/// with a character an address does not hold as such, like `é`, is left
/// unlinked rather than linked short. `file` is where a word that may be a
/// path opens, if it is one.
pub fn cells(
    buf: &Buffer,
    cut: &[bool],
    file: impl Fn(&str) -> Option<String>,
) -> Vec<(u16, u16, Cell)> {
    let area = buf.area;
    let at = |x: usize, y: usize| &buf[(area.x + x as u16, area.y + y as u16)];
    let rows: Vec<Vec<&str>> = (0..area.height as usize)
        .map(|y| {
            (0..area.width as usize)
                .map(|x| at(x, y).symbol())
                .collect()
        })
        .collect();
    let mut links: Vec<(Vec<Part>, String)> = Vec::new();
    // Where each row's search starts: past the part of an address above.
    let mut from_x = vec![0; rows.len()];
    for y in 0..rows.len() {
        let row = &rows[y];
        let mut x = from_x[y];
        while let Some(start) = (x..row.len()).find(|&at| run(row, at) > at) {
            let end = run(row, start);
            x = end;
            let mut parts = vec![(y, start, end)];
            let mut word: String = row[start..end].concat();
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
                word.push_str(&next[from..to].concat());
                parts.push((last_row + 1, from, to));
                (last_row, last_end) = (last_row + 1, to);
                from_x[last_row] = to;
            }
            if !ends(&rows[last_row], last_end) {
                continue;
            }
            // A web address where one starts, else a path once what opens
            // and closes around it is off.
            let (lead, target, kept) = match web(&word) {
                Some(lead) => {
                    let url = trim(&word[lead..]);
                    (lead, url.clone(), url.len())
                }
                None => {
                    let lead = word.len() - word.trim_start_matches(['(', '[', '{', '\'']).len();
                    let path = trim(&word[lead..]);
                    match file(&path) {
                        Some(target) => (lead, target, path.len()),
                        None => continue,
                    }
                }
            };
            shorten(&mut parts, lead, word.len() - lead - kept);
            if !parts.is_empty() {
                links.push((parts, target));
            }
        }
    }
    // A markdown link's text, underlined, before the address it is drawn
    // with in brackets: `text (address)`.
    let underlined = |x: usize, y: usize| at(x, y).modifier.contains(Modifier::UNDERLINED);
    for (parts, _) in &mut links {
        let (y, start, _) = parts[0];
        let (last_y, _, last_end) = parts[parts.len() - 1];
        if start == 0 || rows[y][start - 1] != "(" || rows[last_y].get(last_end) != Some(&")") {
            continue;
        }
        parts.splice(0..0, label(&rows, start - 1, y, &underlined));
    }
    let mut out = Vec::new();
    for (parts, target) in links {
        for (y, from, to) in parts {
            for x in from..to {
                let mut cell = at(x, y).clone();
                let mut symbol = cell.symbol().to_owned();
                if x == from {
                    symbol = format!("\x1b]8;;{target}\x1b\\{symbol}");
                }
                if x + 1 == to {
                    symbol.push_str("\x1b]8;;\x1b\\");
                }
                cell.set_symbol(&symbol);
                out.push((area.x + x as u16, area.y + y as u16, cell));
            }
        }
    }
    out
}

/// The underlined text that ends just before the `(` at `open` of row `y`,
/// a space apart, or at the end of the row above when the bracket starts
/// its row: each row of it, the first first. A text wrapped over rows goes
/// on at the end of the row above where it starts a row.
fn label(
    rows: &[Vec<&str>],
    open: usize,
    y: usize,
    underlined: &dyn Fn(usize, usize) -> bool,
) -> Vec<Part> {
    let starts_row = |y: usize, x: usize| rows[y][..x].iter().all(|s| *s == " ");
    let last_drawn = |y: usize| rows[y].iter().rposition(|s| *s != " ");
    let mut end = if open >= 2 && rows[y][open - 1] == " " && underlined(open - 2, y) {
        Some((y, open - 1))
    } else if y > 0 && starts_row(y, open) {
        last_drawn(y - 1)
            .filter(|&x| underlined(x, y - 1))
            .map(|x| (y - 1, x + 1))
    } else {
        None
    };
    let mut parts = Vec::new();
    while let Some((y, to)) = end.take() {
        let mut from = to;
        while from > 0 && underlined(from - 1, y) {
            from -= 1;
        }
        parts.insert(0, (y, from, to));
        if y > 0 && from > 0 && starts_row(y, from) {
            end = last_drawn(y - 1)
                .filter(|&x| underlined(x, y - 1))
                .map(|x| (y - 1, x + 1));
        }
    }
    parts
}

/// Where a web address starts in `word`, if one does: not inside a word,
/// as in `xhttps://`.
fn web(word: &str) -> Option<usize> {
    let at = ["https://", "http://"]
        .iter()
        .filter_map(|scheme| word.find(scheme))
        .min()?;
    (at == 0 || !word[..at].ends_with(|c: char| c.is_ascii_alphanumeric())).then_some(at)
}

/// `parts` without `lead` cells at the start and `over` at the end.
fn shorten(parts: &mut Vec<Part>, mut lead: usize, mut over: usize) {
    while lead > 0 && !parts.is_empty() {
        let first = &mut parts[0];
        let take = lead.min(first.2 - first.1);
        first.1 += take;
        lead -= take;
        if first.1 == first.2 {
            parts.remove(0);
        }
    }
    while over > 0 && !parts.is_empty() {
        let last = parts.last_mut().unwrap();
        let take = over.min(last.2 - last.1);
        last.2 -= take;
        over -= take;
        if last.2 == last.1 {
            parts.pop();
        }
    }
}

/// Where `word` opens, if it names a file or folder that is there:
/// relative to `workspace`, absolute, or under `~/`, with any `:LINE`,
/// `:LINE:COL` or `#L…` after it left off. A `file://` address.
pub fn file(workspace: &Path, word: &str) -> Option<String> {
    let path = word.split('#').next().unwrap_or(word);
    let mut path = path;
    for _ in 0..2 {
        if let Some((head, tail)) = path.rsplit_once(':')
            && !tail.is_empty()
            && tail.bytes().all(|b| b.is_ascii_digit())
        {
            path = head;
        }
    }
    // A word with no folder in it or file kind after a dot is not a path,
    // nor is one that is only dots, or an address of another kind.
    if !(path.contains('/') || path.contains('.'))
        || !path.contains(|c: char| c.is_ascii_alphanumeric())
        || path.contains("://")
        || path.len() > 1024
    {
        return None;
    }
    let full = match path.strip_prefix("~/") {
        Some(rest) => Path::new(&std::env::var_os("HOME")?).join(rest),
        None => workspace.join(path),
    };
    if !full.is_absolute() || !full.exists() {
        return None;
    }
    file_url(&full)
}

/// The `file://` address of the absolute path `full`, every byte but
/// the plainest escaped, so a space, `#` or `%` in it stays the file's.
pub fn file_url(full: &Path) -> Option<String> {
    let mut url = String::from("file://");
    for byte in full.to_str()?.bytes() {
        if byte.is_ascii_alphanumeric() || b"/-._~".contains(&byte) {
            url.push(byte as char);
        } else {
            url.push_str(&format!("%{byte:02X}"));
        }
    }
    Some(url)
}

/// [`file`], with the line `word` names after the path, if any, as
/// `#L…`: where the full screen's preview of it opens.
pub fn file_line(workspace: &Path, word: &str) -> Option<String> {
    let url = file(workspace, word)?;
    let line = match word.split_once("#L") {
        Some((_, line)) => line.split(|c: char| !c.is_ascii_digit()).next(),
        None => {
            let numbers: Vec<&str> = (word.rsplit(':'))
                .take(2)
                .take_while(|part| !part.is_empty() && part.bytes().all(|b| b.is_ascii_digit()))
                .collect();
            numbers.last().copied()
        }
    };
    Some(match line.filter(|line| !line.is_empty()) {
        Some(line) => format!("{url}#L{line}"),
        None => url,
    })
}

/// Whether an address that [`run`] stopped at `end` of `row` ends there,
/// rather than at a character it holds that `run` does not follow.
fn ends(row: &[&str], end: usize) -> bool {
    row.get(end)
        .is_none_or(|symbol| symbol.is_ascii() || symbol.trim().is_empty())
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
        for (x, y, cell) in cells(buf, cut, |_| None) {
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
        assert!(
            cells(&drawn(&["https://münchen.de/path"], 40), &[false], |_| {
                None
            })
            .is_empty()
        );
    }

    /// A markdown link's line as a reply draws it, wrapped to `width`.
    fn markdown(text: &str, width: usize) -> (Buffer, Vec<bool>) {
        let line = Line::from(crate::transcript::inline(text, Default::default()));
        let rows = crate::transcript::wrap(line, width, "• ".into(), "  ".into());
        let cut = rows
            .iter()
            .map(|row| row.alignment == Some(crate::transcript::CUT))
            .collect();
        (draw(rows, width as u16), cut)
    }

    #[test]
    fn a_markdown_links_text_opens_its_address() {
        let (buf, cut) = markdown("Opened [PR #231](https://example.com/pull/231).", 80);
        let url = "https://example.com/pull/231".to_owned();
        assert_eq!(
            links(&buf, &cut),
            [(0, 9, 15, url.clone()), (0, 18, 45, url)],
            "the underlined text and the address after it"
        );
        // Wrapped: its text over two rows, the address on the next.
        let (buf, cut) = markdown("See [the long pull request](https://example.com/p) now", 20);
        let rows: Vec<String> = (0..buf.area.height)
            .map(|y| (0..20).map(|x| buf[(x, y)].symbol()).collect())
            .collect();
        assert_eq!(
            rows,
            [
                "• See the long pull ",
                "  request           ",
                "  (https://example.c",
                "  om/p) now         "
            ]
        );
        let found = links(&buf, &cut);
        let rows: Vec<u16> = found.iter().map(|link| link.0).collect();
        assert_eq!(rows, [0, 1, 2, 3]);
        assert_eq!((found[0].1, found[0].2), (6, 18), "from `the` to `pull`");
        assert!(found.iter().all(|link| link.3 == "https://example.com/p"));
    }

    #[test]
    fn a_path_that_is_there_opens_as_a_file() {
        let dir = std::env::temp_dir().join(format!("sonny-links-{}", std::process::id()));
        std::fs::create_dir_all(dir.join("src")).unwrap();
        std::fs::write(dir.join("src/main.rs"), "").unwrap();
        std::fs::write(dir.join("a b.md"), "").unwrap();
        let main = format!("file://{}/src/main.rs", dir.display());
        let opens = |text: &str, width: usize| {
            let (buf, cut) = markdown(text, width);
            cells(&buf, &cut, |word| file(&dir, word))
                .iter()
                .filter_map(|(x, _, cell)| {
                    let rest = cell.symbol().strip_prefix("\x1b]8;;")?;
                    Some((*x, rest.split('\x1b').next().unwrap().to_owned()))
                })
                .collect::<Vec<_>>()
        };
        // In code, with a line after it; and a markdown link's text.
        assert_eq!(
            opens("In `src/main.rs:12:3`, done.", 80),
            [(5, main.clone())]
        );
        assert_eq!(
            opens("See [main](src/main.rs).", 80),
            [(6, main.clone()), (12, main.clone())]
        );
        let absolute = format!("{}/src/main.rs#L4", dir.display());
        assert_eq!(opens(&absolute, absolute.len() + 2), [(2, main.clone())]);
        let wrapped = opens(&absolute, 20);
        assert!(wrapped.len() > 1, "the path wraps over several rows");
        assert!(wrapped.iter().all(|(x, target)| *x == 2 && target == &main));
        // Not there, not a path, or only part of a word.
        assert!(opens("src/gone.rs, e.g. 0.4.1 and .. or ./", 80).is_empty());
        assert_eq!(
            file(&dir, "a b.md"),
            Some(format!("file://{}/a%20b.md", dir.display()))
        );
        // For the preview, the line goes with it.
        let main = format!("file://{}/src/main.rs", dir.display());
        assert_eq!(
            file_line(&dir, "src/main.rs:12:3"),
            Some(format!("{main}#L12"))
        );
        assert_eq!(file_line(&dir, "src/main.rs:7"), Some(format!("{main}#L7")));
        assert_eq!(
            file_line(&dir, "src/main.rs#L40-L44"),
            Some(format!("{main}#L40"))
        );
        assert_eq!(file_line(&dir, "src/main.rs"), Some(main));
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn text_without_an_address_has_no_links() {
        assert!(
            cells(
                &drawn(&["http is a protocol; xhttps://no"], 40),
                &[false],
                |_| None
            )
            .is_empty()
        );
    }
}
