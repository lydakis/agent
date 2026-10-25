//! Diffs drawn the way diffs.com draws them, in a terminal: numbered rows,
//! a coloured bar for each changed line, a tint on the line and a stronger
//! one on the words that changed, nearby lines dim for context, and
//! `⋯ N unmodified lines` between hunks. An edit's change and a unified
//! diff a command printed (`git diff`) read the same.
use ratatui::{
    style::{Color, Modifier, Style, Stylize},
    text::{Line, Span},
};
use similar::{ChangeTag, TextDiff};

/// The bar, the line's tint and the changed words' tint, for removed
/// and added lines, on the terminal's background.
struct Palette {
    removed: Color,
    added: Color,
    removed_line: Color,
    added_line: Color,
    removed_word: Color,
    added_word: Color,
}

const DARK: Palette = Palette {
    removed: Color::Rgb(232, 98, 98),
    added: Color::Rgb(94, 201, 120),
    removed_line: Color::Rgb(58, 26, 30),
    added_line: Color::Rgb(22, 50, 32),
    removed_word: Color::Rgb(120, 40, 48),
    added_word: Color::Rgb(36, 100, 56),
};

const LIGHT: Palette = Palette {
    removed: Color::Rgb(207, 34, 46),
    added: Color::Rgb(26, 127, 55),
    removed_line: Color::Rgb(255, 235, 233),
    added_line: Color::Rgb(230, 255, 236),
    removed_word: Color::Rgb(255, 196, 192),
    added_word: Color::Rgb(171, 242, 188),
};

fn palette() -> &'static Palette {
    if crate::theme::light() { &LIGHT } else { &DARK }
}

#[derive(Clone, Copy, PartialEq)]
enum Kind {
    Context,
    Removed,
    Added,
}

/// `-N +M`, coloured, for a header.
pub fn counts(removed: usize, added: usize) -> Vec<Span<'static>> {
    vec![
        Span::raw(format!("-{removed}")).fg(palette().removed),
        Span::raw(" "),
        Span::raw(format!("+{added}")).fg(palette().added),
    ]
}

/// One row, wrapped: number, bar, then the text in pieces, each marked
/// as changed or not.
fn row(
    kind: Kind,
    number: Option<usize>,
    pieces: &[(bool, String)],
    digits: usize,
    width: usize,
) -> Vec<Line<'static>> {
    let number = number.map_or(" ".repeat(digits), |n| format!("{n:>digits$}"));
    let p = palette();
    let (bar, line, word) = match kind {
        Kind::Context => (
            Span::raw("  "),
            Style::new().add_modifier(Modifier::DIM),
            None,
        ),
        Kind::Removed => (
            Span::raw("▎ ").fg(p.removed),
            Style::new().bg(p.removed_line),
            Some(Style::new().bg(p.removed_word)),
        ),
        Kind::Added => (
            Span::raw("▎ ").fg(p.added),
            Style::new().bg(p.added_line),
            Some(Style::new().bg(p.added_word)),
        ),
    };
    let spans: Vec<Span<'static>> = pieces
        .iter()
        .map(|(changed, text)| {
            let text = text.replace('\t', "    ");
            match (changed, word) {
                (true, Some(word)) => Span::styled(text, word),
                _ => Span::styled(text, line),
            }
        })
        .collect();
    let lead = Line::from(vec![Span::raw(format!("  {number} ")).dim(), bar.clone()]);
    let rest = Line::from(vec![Span::raw(" ".repeat(digits + 3)), bar]);
    crate::transcript::wrap_lines(Line::from(spans), width, lead, rest)
}

/// Old and new lines as rows, the changed words within a changed line
/// marked, numbered from where each starts.
fn rows(
    old: &str,
    new: &str,
    old_start: usize,
    new_start: usize,
    digits: usize,
    width: usize,
) -> Vec<Line<'static>> {
    let diff = TextDiff::from_lines(old, new);
    let mut out = Vec::new();
    for op in diff.ops() {
        for change in diff.iter_inline_changes(op) {
            let (kind, number) = match change.tag() {
                ChangeTag::Equal => (Kind::Context, change.new_index().map(|i| new_start + i)),
                ChangeTag::Delete => (Kind::Removed, change.old_index().map(|i| old_start + i)),
                ChangeTag::Insert => (Kind::Added, change.new_index().map(|i| new_start + i)),
            };
            let pieces: Vec<(bool, String)> = change
                .iter_strings_lossy()
                .map(|(changed, text)| (changed, text.trim_end_matches('\n').to_owned()))
                .filter(|(_, text)| !text.is_empty())
                .collect();
            out.extend(row(kind, number, &pieces, digits, width));
        }
    }
    out
}

fn context(lines: &[String], start: usize, digits: usize, width: usize) -> Vec<Line<'static>> {
    lines
        .iter()
        .enumerate()
        .flat_map(|(i, line)| {
            row(
                Kind::Context,
                Some(start + i),
                &[(false, line.clone())],
                digits,
                width,
            )
        })
        .collect()
}

/// An edit: the whole lines it touched, with up to two lines either side
/// from the file as it is now.
pub struct Edit {
    pub old: String,
    pub new: String,
    /// The line the edit starts at, 1-based, when known.
    pub start: Option<usize>,
    pub before: Vec<String>,
    pub after: Vec<String>,
}

impl Edit {
    pub fn counts(&self) -> (usize, usize) {
        let diff = TextDiff::from_lines(&self.old, &self.new);
        let removed = diff
            .iter_all_changes()
            .filter(|c| c.tag() == ChangeTag::Delete)
            .count();
        let added = diff
            .iter_all_changes()
            .filter(|c| c.tag() == ChangeTag::Insert)
            .count();
        (removed, added)
    }

    /// At most `most` rows, then a count of the rest.
    pub fn lines(&self, width: usize, most: usize) -> Vec<Line<'static>> {
        let start = self.start.unwrap_or(1);
        let last =
            start + self.new.lines().count().max(self.old.lines().count()) + self.after.len();
        let digits = last.to_string().len().max(2);
        let mut out = context(
            &self.before,
            start.saturating_sub(self.before.len()),
            digits,
            width,
        );
        out.extend(rows(&self.old, &self.new, start, start, digits, width));
        let after_start = start + self.new.lines().count();
        out.extend(context(&self.after, after_start, digits, width));
        if self.start.is_none() {
            // Numbers are a guess without the file: leave them off.
            for line in &mut out {
                if let Some(first) = line.spans.first_mut()
                    && first.content.trim().chars().all(|c| c.is_ascii_digit())
                {
                    first.content = " ".repeat(first.content.chars().count()).into();
                }
            }
        }
        cap(out, most)
    }
}

/// A unified diff (`git diff`, `diff -u`) as files with their hunks, or
/// none when the text is not one. At most `most` rows, then a count.
pub fn unified(text: &str, width: usize, most: usize) -> Option<Vec<Line<'static>>> {
    if !text.lines().any(|l| l.starts_with("@@ ")) || !text.lines().any(|l| l.starts_with("--- ")) {
        return None;
    }
    let mut out: Vec<Line<'static>> = Vec::new();
    let mut files: Vec<(String, Vec<Hunk>)> = Vec::new();
    let mut lines = text.lines().peekable();
    while let Some(line) = lines.next() {
        if let Some(path) = line.strip_prefix("+++ ") {
            let path = path.strip_prefix("b/").unwrap_or(path).to_owned();
            files.push((path, Vec::new()));
        } else if let Some(header) = line.strip_prefix("@@ ")
            && let Some((_, hunks)) = files.last_mut()
        {
            let mut ranges = header.split_whitespace();
            let start = |range: Option<&str>| -> usize {
                range
                    .and_then(|r| r[1..].split(',').next()?.parse().ok())
                    .unwrap_or(1)
            };
            let (old_start, new_start) = (start(ranges.next()), start(ranges.next()));
            let mut hunk = Hunk {
                old_start,
                new_start,
                lines: Vec::new(),
            };
            while let Some(next) = lines.peek() {
                if next.starts_with("@@ ") || next.starts_with("diff ") || next.starts_with("--- ")
                {
                    break;
                }
                hunk.lines.push(lines.next().unwrap_or_default().to_owned());
            }
            hunks.push(hunk);
        }
    }
    if files.is_empty() {
        return None;
    }
    for (i, (path, hunks)) in files.iter().enumerate() {
        let removed: usize = hunks.iter().map(|h| h.count('-')).sum();
        let added: usize = hunks.iter().map(|h| h.count('+')).sum();
        let lead = if i == 0 { "  └ " } else { "    " };
        let mut head = vec![
            Span::raw(lead).dim(),
            Span::raw(path.clone()).bold(),
            Span::raw("  "),
        ];
        head.extend(counts(removed, added));
        out.push(Line::from(head));
        let digits = hunks
            .iter()
            .map(|h| (h.new_start + h.lines.len()).max(h.old_start + h.lines.len()))
            .max()
            .unwrap_or(1)
            .to_string()
            .len()
            .max(2);
        let mut next_old = 1;
        for hunk in hunks {
            let skipped = hunk.old_start.saturating_sub(next_old);
            if skipped > 0 {
                out.push(
                    Line::from(format!("  {:digits$} ⋯ {skipped} unmodified lines", "")).dim(),
                );
            }
            out.extend(hunk.rows(digits, width));
            next_old = hunk.old_start + hunk.lines.iter().filter(|l| !l.starts_with('+')).count();
        }
    }
    Some(cap(out, most))
}

fn cap(mut out: Vec<Line<'static>>, most: usize) -> Vec<Line<'static>> {
    if out.len() > most {
        let more = out.len() - most;
        out.truncate(most);
        out.push(Line::from(format!("    … +{more} lines")).dim());
    }
    out
}

struct Hunk {
    old_start: usize,
    new_start: usize,
    lines: Vec<String>,
}

impl Hunk {
    fn count(&self, sign: char) -> usize {
        self.lines.iter().filter(|l| l.starts_with(sign)).count()
    }

    /// Context as is; each run of removed then added lines diffed by word.
    fn rows(&self, digits: usize, width: usize) -> Vec<Line<'static>> {
        let mut out = Vec::new();
        let (mut old_no, mut new_no) = (self.old_start, self.new_start);
        let mut i = 0;
        while i < self.lines.len() {
            let line = &self.lines[i];
            if line.starts_with('-') || line.starts_with('+') {
                let (mut old, mut new) = (String::new(), String::new());
                while let Some(line) = self.lines.get(i).filter(|l| l.starts_with('-')) {
                    old.push_str(&line[1..]);
                    old.push('\n');
                    i += 1;
                }
                while let Some(line) = self.lines.get(i).filter(|l| l.starts_with('+')) {
                    new.push_str(&line[1..]);
                    new.push('\n');
                    i += 1;
                }
                out.extend(rows(&old, &new, old_no, new_no, digits, width));
                old_no += old.lines().count();
                new_no += new.lines().count();
            } else {
                if !line.starts_with('\\') {
                    let text = line.strip_prefix(' ').unwrap_or(line);
                    out.extend(row(
                        Kind::Context,
                        Some(new_no),
                        &[(false, text.to_owned())],
                        digits,
                        width,
                    ));
                    old_no += 1;
                    new_no += 1;
                }
                i += 1;
            }
        }
        out
    }
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
    fn an_edit_marks_its_lines_and_the_words_that_changed() {
        let edit = Edit {
            old: "    return sum(values) / (len(values) - 1)\n".into(),
            new: "    return sum(values) / len(values)\n".into(),
            start: Some(2),
            before: vec!["def average(values):".into()],
            after: vec![],
        };
        assert_eq!(edit.counts(), (1, 1));
        let lines = edit.lines(80, 40);
        assert_eq!(
            text(&lines),
            [
                "   1   def average(values):",
                "   2 ▎     return sum(values) / (len(values) - 1)",
                "   2 ▎     return sum(values) / len(values)",
            ]
        );
        // The removed words carry the stronger tint.
        let strong: Vec<&str> = lines[1]
            .spans
            .iter()
            .filter(|s| s.style.bg == Some(DARK.removed_word))
            .map(|s| s.content.as_ref())
            .collect();
        assert!(strong.concat().contains("(len(values) - 1)"), "{strong:?}");
    }

    #[test]
    fn a_unified_diff_reads_as_files_and_hunks() {
        let git = "diff --git a/calc.py b/calc.py\n--- a/calc.py\n+++ b/calc.py\n@@ -1,3 +1,3 @@\n def average(values):\n-    return 1\n+    return 2\n x = 1\n@@ -20,2 +20,3 @@\n y = 2\n+z = 3\n w = 4\n";
        let lines = unified(git, 80, 100).unwrap();
        let text = text(&lines);
        assert_eq!(text[0], "  └ calc.py  -1 +2");
        assert!(
            text.iter().any(|l| l.contains("⋯ 16 unmodified lines")),
            "{text:?}"
        );
        assert!(
            text.iter().any(|l| l.starts_with("  21 ▎ z = 3")),
            "{text:?}"
        );
        assert!(unified("just text\n", 80, 100).is_none());
    }
}
