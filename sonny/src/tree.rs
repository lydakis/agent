//! A folder drawn as a tree in the pane, the way Pierre's Trees
//! (trees.software) draws one: folders first, a folder opened or closed
//! in place, a chain of folders that each hold only the next one on one
//! row, and what git says of each file at the row's end.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};

use ratatui::style::{Color, Style, Stylize};
use ratatui::text::{Line, Span};
use unicode_width::UnicodeWidthStr;

/// The most entries a folder lists, and rows a tree draws, so a folder
/// of thousands stays quick.
const ENTRIES: usize = 500;
const ROWS: usize = 5000;

/// What git says of a path.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Mark {
    Modified,
    Added,
    Deleted,
    Renamed,
    Untracked,
    Ignored,
}

impl Mark {
    /// Its letter and color, as Trees shows them; none for an ignored
    /// path, which reads dim instead.
    fn badge(self) -> Option<(&'static str, Color)> {
        Some(match self {
            Mark::Modified => ("M", Color::Yellow),
            Mark::Added => ("A", Color::Green),
            Mark::Deleted => ("D", Color::Red),
            Mark::Renamed => ("R", Color::Reset),
            Mark::Untracked => ("U", Color::Green),
            Mark::Ignored => return None,
        })
    }
}

pub struct Tree {
    root: PathBuf,
    /// The folders opened, the root always.
    open: HashSet<PathBuf>,
    marks: HashMap<PathBuf, Mark>,
    /// The folders that hold a change, which show a dot.
    changed: HashSet<PathBuf>,
}

impl Tree {
    /// `root` with its own entries shown, and what git says of them when
    /// it is in a repository.
    pub fn new(root: &Path) -> Self {
        let marks = git_marks(root);
        let mut changed = HashSet::new();
        for (path, mark) in &marks {
            if *mark == Mark::Ignored {
                continue;
            }
            for folder in path.ancestors().skip(1) {
                if !folder.starts_with(root) || !changed.insert(folder.to_path_buf()) {
                    break;
                }
            }
        }
        Self {
            root: root.to_path_buf(),
            open: HashSet::from([root.to_path_buf()]),
            marks,
            changed,
        }
    }

    /// Opens a closed folder in the tree or closes an open one; whether
    /// `path` is one of its folders.
    pub fn toggle(&mut self, path: &Path) -> bool {
        if path == self.root || !path.starts_with(&self.root) || !path.is_dir() {
            return false;
        }
        if !self.open.remove(path) {
            self.open.insert(path.to_path_buf());
        }
        true
    }

    /// Its rows at `width`, each with the address a click on it opens:
    /// a file's to show it, a folder's to open or close it.
    pub fn lines(&self, width: usize) -> Vec<(Line<'static>, Option<String>)> {
        let mut out = Vec::new();
        self.folder(&self.root, "", width, &mut out);
        if out.is_empty() {
            out.push((Line::from(" an empty folder").dim(), None));
        }
        out
    }

    fn folder(
        &self,
        path: &Path,
        guides: &str,
        width: usize,
        out: &mut Vec<(Line<'static>, Option<String>)>,
    ) {
        let entries = entries(path);
        let more = entries.len().saturating_sub(ENTRIES);
        for (dir, name) in entries.into_iter().take(ENTRIES) {
            if out.len() >= ROWS {
                return;
            }
            let mut full = path.join(&name);
            let mut shown = name;
            // A chain of folders that each hold only the next: one row.
            while dir && let [(true, only)] = &entries_of_one(&full)[..] {
                full.push(only);
                shown = format!("{shown}/{only}");
            }
            let mark = self.mark(&full);
            let ignored = mark == Some(Mark::Ignored);
            let name_style = match (ignored, mark.and_then(Mark::badge)) {
                (true, _) => Style::new().dim(),
                (false, Some((_, color))) if !dir => Style::new().fg(color),
                _ => Style::new(),
            };
            let open = dir && self.open.contains(&full);
            let mut spans = vec![Span::raw(" "), Span::raw(guides.to_owned()).dark_gray()];
            spans.push(match (dir, open) {
                (true, true) => Span::raw("▾ ").dark_gray(),
                (true, false) => Span::raw("▸ ").dark_gray(),
                (false, _) => Span::raw("  "),
            });
            spans.push(Span::styled(shown, name_style));
            if dir {
                spans.push(Span::styled("/", name_style.dim()));
            }
            // What git says, at the row's end: a file's letter, a dot on a
            // folder that holds a change.
            let badge = match (dir, mark.and_then(Mark::badge)) {
                (false, Some((letter, color))) => Some(Span::raw(letter).fg(color)),
                (true, Some((letter, color))) if !self.changed.contains(&full) => {
                    Some(Span::raw(letter).fg(color))
                }
                (true, _) if self.changed.contains(&full) => Some(Span::raw("•").fg(Color::Yellow)),
                _ => None,
            };
            if let Some(badge) = badge {
                let used: usize = spans.iter().map(|s| s.content.width()).sum();
                let gap = width.saturating_sub(used + 2).max(1);
                spans.push(Span::raw(" ".repeat(gap)));
                spans.push(badge);
            }
            out.push((Line::from(spans), crate::links::file_url(&full)));
            if open {
                self.folder(&full, &format!("{guides}│ "), width, out);
            }
        }
        if more > 0 {
            let note = format!(" {guides}  … {more} more");
            out.push((Line::from(note).dim(), None));
        }
    }

    /// What git says of `path`, or of a folder it is in when git says it
    /// of the whole folder, as of one untracked or ignored.
    fn mark(&self, path: &Path) -> Option<Mark> {
        if let Some(mark) = self.marks.get(path) {
            return Some(*mark);
        }
        (path.ancestors().skip(1)).find_map(|folder| {
            (self.marks.get(folder).copied())
                .filter(|mark| matches!(mark, Mark::Untracked | Mark::Ignored))
        })
    }
}

/// A folder's entries, folders first, without `.git`.
fn entries(path: &Path) -> Vec<(bool, String)> {
    let Ok(read) = std::fs::read_dir(path) else {
        return Vec::new();
    };
    let mut entries: Vec<(bool, String)> = read
        .filter_map(|entry| {
            let entry = entry.ok()?;
            let name = entry.file_name().to_string_lossy().into_owned();
            // A link to a folder opens as one.
            let dir = std::fs::metadata(entry.path()).ok()?.is_dir();
            (name != ".git").then_some((dir, name))
        })
        .collect();
    entries.sort_by(|a, b| (!a.0, a.1.to_lowercase(), &a.1).cmp(&(!b.0, b.1.to_lowercase(), &b.1)));
    entries
}

/// A folder's entries when it holds no more than one, to see whether it
/// only leads on to another.
fn entries_of_one(path: &Path) -> Vec<(bool, String)> {
    let Ok(mut read) = std::fs::read_dir(path) else {
        return Vec::new();
    };
    let (Some(Ok(only)), None) = (read.next(), read.next()) else {
        return Vec::new();
    };
    let name = only.file_name().to_string_lossy().into_owned();
    let dir = std::fs::metadata(only.path()).is_ok_and(|m| m.is_dir());
    match name == ".git" {
        true => Vec::new(),
        false => vec![(dir, name)],
    }
}

/// What `git status` says of the paths under `root`, by their full path;
/// nothing outside a repository.
fn git_marks(root: &Path) -> HashMap<PathBuf, Mark> {
    let git = |args: &[&str]| {
        let out = std::process::Command::new("git")
            .arg("-C")
            .arg(root)
            .args(args)
            .stderr(std::process::Stdio::null())
            .output()
            .ok()?;
        out.status.success().then_some(out.stdout)
    };
    let Some(top) = git(&["rev-parse", "--show-toplevel"]) else {
        return HashMap::new();
    };
    let top = PathBuf::from(String::from_utf8_lossy(&top).trim_end_matches('\n'));
    let Some(status) = git(&[
        "status",
        "--porcelain",
        "-z",
        "--ignored=matching",
        "--",
        ".",
    ]) else {
        return HashMap::new();
    };
    marks(&top, &status)
}

/// `git status --porcelain -z` read: each path, from the top of the
/// repository at `top`, with its mark.
fn marks(top: &Path, status: &[u8]) -> HashMap<PathBuf, Mark> {
    let mut marks = HashMap::new();
    let mut fields = status.split(|b| *b == 0).filter(|f| !f.is_empty());
    while let Some(field) = fields.next() {
        let Some((code, path)) = field.split_at_checked(3) else {
            continue;
        };
        let (x, y) = (code[0], code[1]);
        let mark = match (x, y) {
            (b'?', b'?') => Mark::Untracked,
            (b'!', b'!') => Mark::Ignored,
            (b'R', _) | (_, b'R') => Mark::Renamed,
            (b'D', _) | (_, b'D') => Mark::Deleted,
            (b'A', _) => Mark::Added,
            _ => Mark::Modified,
        };
        // A rename names where it came from after it.
        if matches!(x, b'R' | b'C') {
            fields.next();
        }
        let path = String::from_utf8_lossy(path);
        marks.insert(top.join(path.trim_end_matches('/')), mark);
    }
    marks
}

#[cfg(test)]
mod tests {
    use super::*;

    fn text(lines: &[(Line<'static>, Option<String>)]) -> Vec<String> {
        lines
            .iter()
            .map(|(line, _)| {
                let text: String = line.spans.iter().map(|s| s.content.as_ref()).collect();
                text.trim_end().to_owned()
            })
            .collect()
    }

    #[test]
    fn git_status_reads_as_each_paths_mark() {
        let top = Path::new("/w");
        let status = b" M src/a.rs\0?? new/\0!! target/\0R  b.rs\0old.rs\0A  c.rs\0 D d.rs\0";
        let marks = marks(top, status);
        assert_eq!(marks[Path::new("/w/src/a.rs")], Mark::Modified);
        assert_eq!(marks[Path::new("/w/new")], Mark::Untracked);
        assert_eq!(marks[Path::new("/w/target")], Mark::Ignored);
        assert_eq!(marks[Path::new("/w/b.rs")], Mark::Renamed);
        assert!(!marks.contains_key(Path::new("/w/old.rs")));
        assert_eq!(marks[Path::new("/w/c.rs")], Mark::Added);
        assert_eq!(marks[Path::new("/w/d.rs")], Mark::Deleted);
    }

    #[test]
    fn a_folder_reads_as_a_tree_its_folders_opened_in_place() {
        let root = std::env::temp_dir().join(format!("sonny-tree-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(root.join("src/deep/er")).unwrap();
        std::fs::create_dir_all(root.join("docs")).unwrap();
        std::fs::write(root.join("src/deep/er/x.rs"), "").unwrap();
        std::fs::write(root.join("src/main.rs"), "").unwrap();
        std::fs::write(root.join("docs/a.md"), "").unwrap();
        std::fs::write(root.join("Cargo.toml"), "").unwrap();
        let mut tree = Tree::new(&root);
        let lines = tree.lines(40);
        assert_eq!(text(&lines), [" ▸ docs/", " ▸ src/", "   Cargo.toml"]);
        // A click on a folder's row opens it under its row.
        let src = crate::pane::file_url(lines[1].1.as_ref().unwrap())
            .unwrap()
            .0;
        assert!(tree.toggle(&src));
        let lines = tree.lines(40);
        assert_eq!(
            text(&lines),
            [
                " ▸ docs/",
                " ▾ src/",
                " │ ▸ deep/er/",
                " │   main.rs",
                "   Cargo.toml"
            ]
        );
        // The chain of folders opens as the one it shows.
        let deep = crate::pane::file_url(lines[2].1.as_ref().unwrap())
            .unwrap()
            .0;
        assert_eq!(deep, root.join("src/deep/er"));
        assert!(tree.toggle(&deep));
        assert_eq!(text(&tree.lines(40))[3], " │ │   x.rs");
        assert!(tree.toggle(&src));
        assert_eq!(text(&tree.lines(40)).len(), 3);
        // A file, or the root, is not opened in place.
        assert!(!tree.toggle(&root.join("Cargo.toml")));
        assert!(!tree.toggle(&root));
        std::fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn what_git_says_shows_at_the_rows_end() {
        let root = std::env::temp_dir().join(format!("sonny-tree-git-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(root.join("src")).unwrap();
        std::fs::write(root.join("src/a.rs"), "").unwrap();
        std::fs::write(root.join("b.rs"), "").unwrap();
        let mut tree = Tree::new(&root);
        tree.marks = marks(&root, b" M src/a.rs\0?? b.rs\0");
        tree.changed = HashSet::from([root.join("src")]);
        let lines = text(&tree.lines(20));
        assert_eq!(lines, [" ▸ src/           •", "   b.rs           U"]);
        std::fs::remove_dir_all(&root).unwrap();
    }
}
