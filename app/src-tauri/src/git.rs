//! What the Git tab shows of a folder's repository: its changes, its last
//! commits and its worktrees, and the diff of one change or one commit. It
//! runs plain git and only reads: an agent commits its own work, so the tab
//! shows it and passes a note on a line back to the agent.
use crate::files::{git, top};
use std::io::{BufRead, BufReader, Read};
use std::path::Path;
use std::process::{Command, Stdio};

/// At most this many changes, commits and worktrees; past the first the
/// listing says so.
pub const MAX_CHANGES: usize = 2_000;
const MAX_COMMITS: usize = 50;
const MAX_WORKTREES: usize = 200;
/// What `git status` may print before the listing stops.
const MAX_STATUS_BYTES: u64 = 4 * 1024 * 1024;
/// What a diff may print before it is cut.
pub const MAX_DIFF: u64 = 1024 * 1024;

#[derive(Debug, PartialEq)]
pub struct View {
    /// The repository's top folder; each change is a path under it.
    pub root: String,
    /// The branch line of `git status`: the branch, its upstream, and how
    /// far ahead and behind it is.
    pub branch: String,
    pub changes: Vec<Change>,
    /// More changes than listed.
    pub more: bool,
    pub commits: Vec<Commit>,
    pub worktrees: Vec<Worktree>,
}

#[derive(Debug, PartialEq)]
pub struct Change {
    /// The two status letters, index then folder (`M `, ` M`, `??`, …).
    pub code: String,
    pub path: String,
    /// The path it was renamed or copied from.
    pub from: Option<String>,
}

#[derive(Debug, PartialEq)]
pub struct Commit {
    pub sha: String,
    pub subject: String,
    pub author: String,
    /// When, as git says it relative to now.
    pub when: String,
}

#[derive(Debug, PartialEq)]
pub struct Worktree {
    pub path: String,
    /// The branch checked out there; none when its HEAD is detached.
    pub branch: Option<String>,
}

pub fn view(dir: &Path) -> Result<View, String> {
    let root = top(dir).map_err(|_| {
        format!(
            "{}: not in a git repository; the Git tab shows a repository's changes",
            dir.display()
        )
    })?;
    let at = Path::new(&root);
    let (branch, changes, more) = status(at)?;
    // A repository with no commit yet has no log.
    let log = run(at)
        .args(["log", "-z", "--no-color"])
        .arg(format!("-n{MAX_COMMITS}"))
        .arg("--format=%H%x1f%an%x1f%ar%x1f%s")
        .stderr(Stdio::null())
        .output()
        .map_err(|e| format!("git log: {e}"))?;
    let commits = if log.status.success() {
        log.stdout.split(|b| *b == 0).filter_map(commit).collect()
    } else {
        Vec::new()
    };
    let list = run(at)
        .args(["worktree", "list", "--porcelain", "-z"])
        .stderr(Stdio::null())
        .output()
        .map_err(|e| format!("git worktree: {e}"))?;
    let worktrees = worktrees(&list.stdout);
    Ok(View {
        root,
        branch,
        changes,
        more,
        commits,
        worktrees,
    })
}

/// `git status`, read record by record up to the bounds.
fn status(at: &Path) -> Result<(String, Vec<Change>, bool), String> {
    let mut child = run(at)
        .args([
            "status",
            "--porcelain=v1",
            "-z",
            "--branch",
            "--untracked-files=all",
        ])
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .map_err(|e| format!("git: {e}"))?;
    let stdout = child.stdout.take().expect("piped");
    let mut out = BufReader::new(stdout.take(MAX_STATUS_BYTES + 1));
    let (mut branch, mut changes, mut record) = (String::new(), Vec::new(), Vec::new());
    let mut next = |record: &mut Vec<u8>| -> Result<bool, String> {
        record.clear();
        out.read_until(0, record)
            .map_err(|e| format!("git status: {e}"))?;
        Ok(record.pop() == Some(0))
    };
    let more = loop {
        if !next(&mut record)? {
            break !record.is_empty();
        }
        if let Some(line) = record.strip_prefix(b"## ") {
            branch = String::from_utf8_lossy(line).into_owned();
            continue;
        }
        if record.len() < 4 || record[2] != b' ' {
            continue;
        }
        let code = String::from_utf8_lossy(&record[..2]).into_owned();
        let path = std::str::from_utf8(&record[3..]).ok().map(str::to_owned);
        // A rename or a copy names where it came from next.
        let from = if matches!(record[0], b'R' | b'C') {
            let mut old = Vec::new();
            if !next(&mut old)? {
                break true;
            }
            std::str::from_utf8(&old).ok().map(str::to_owned)
        } else {
            None
        };
        if changes.len() == MAX_CHANGES {
            break true;
        }
        // A name that is not UTF-8 cannot be named back to git from the page.
        if let Some(path) = path {
            changes.push(Change { code, path, from });
        }
    };
    let more = more || out.get_ref().limit() == 0;
    if more {
        let _ = child.kill();
    }
    let status = child.wait().map_err(|e| format!("git status: {e}"))?;
    if !more && !status.success() {
        return Err(format!("{}: git status failed ({status})", at.display()));
    }
    Ok((branch, changes, more))
}

fn commit(record: &[u8]) -> Option<Commit> {
    let text = String::from_utf8_lossy(record);
    let mut parts = text.trim_start_matches('\n').splitn(4, '\u{1f}');
    let sha = parts.next()?.to_owned();
    if sha.is_empty() {
        return None;
    }
    Some(Commit {
        sha,
        author: parts.next()?.to_owned(),
        when: parts.next()?.to_owned(),
        subject: parts.next()?.to_owned(),
    })
}

/// `worktree list --porcelain -z`: lines ended by NUL, a worktree ended by
/// an empty line. A bare repository has no folder to show.
fn worktrees(out: &[u8]) -> Vec<Worktree> {
    let mut list = Vec::new();
    let mut current: Option<Worktree> = None;
    for line in out.split(|b| *b == 0) {
        let line = String::from_utf8_lossy(line);
        if let Some(path) = line.strip_prefix("worktree ") {
            current = Some(Worktree {
                path: path.to_owned(),
                branch: None,
            });
        } else if let Some(name) = line.strip_prefix("branch ") {
            if let Some(w) = current.as_mut() {
                w.branch = Some(name.strip_prefix("refs/heads/").unwrap_or(name).to_owned());
            }
        } else if line == "bare" {
            current = None;
        } else if line.is_empty()
            && let Some(w) = current.take()
            && list.len() < MAX_WORKTREES
        {
            list.push(w);
        }
    }
    list
}

/// What a diff is of.
pub enum Target {
    /// A change in the folder, against the last commit: staged and not.
    Change {
        path: String,
        from: Option<String>,
        untracked: bool,
    },
    /// What one commit changed, against its first parent.
    Commit(String),
}

#[derive(Debug, PartialEq)]
pub struct Diff {
    pub text: String,
    /// Longer than shown.
    pub cut: bool,
}

pub fn diff(root: &Path, target: &Target) -> Result<Diff, String> {
    let mut command = run(root);
    command.args(["--literal-pathspecs", "-c", "diff.noprefix=false"]);
    match target {
        Target::Change {
            path,
            untracked: true,
            ..
        } => {
            command.args(["diff", "--no-index", "--no-color", "--no-ext-diff", "--"]);
            command.arg("/dev/null").arg(path);
        }
        Target::Change { path, from, .. } => {
            command.args(["diff", "--no-color", "--no-ext-diff", "--no-textconv", "-M"]);
            command.arg(base(root)?).arg("--");
            if let Some(from) = from {
                command.arg(from);
            }
            command.arg(path);
        }
        Target::Commit(sha) => {
            if !(4..=64).contains(&sha.len()) || !sha.bytes().all(|b| b.is_ascii_hexdigit()) {
                return Err(format!("{sha}: not a commit"));
            }
            command.args([
                "show",
                "--no-color",
                "--no-ext-diff",
                "--no-textconv",
                "--format=",
                "-M",
                "--diff-merges=first-parent",
                sha,
                "--",
            ]);
        }
    }
    read(command, MAX_DIFF)
}

/// The last commit, or for a repository with none yet the empty tree, so a
/// change shows against nothing.
fn base(root: &Path) -> Result<String, String> {
    let head = run(root)
        .args(["rev-parse", "--verify", "-q", "HEAD^{commit}"])
        .stderr(Stdio::null())
        .output()
        .map_err(|e| format!("git: {e}"))?;
    if head.status.success() {
        return Ok("HEAD".to_owned());
    }
    let empty = run(root)
        .args(["hash-object", "-t", "tree", "/dev/null"])
        .stderr(Stdio::null())
        .output()
        .map_err(|e| format!("git: {e}"))?;
    Ok(String::from_utf8_lossy(&empty.stdout).trim().to_owned())
}

/// What git prints, up to `limit` bytes, cut at a line's end; git is
/// stopped there rather than left to print the rest. `diff --no-index`
/// exits 1 when the files differ, so only a failure with nothing printed
/// is one.
fn read(mut command: Command, limit: u64) -> Result<Diff, String> {
    let mut child = command
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|e| format!("git: {e}"))?;
    let mut bytes = Vec::new();
    child
        .stdout
        .take()
        .expect("piped")
        .take(limit + 1)
        .read_to_end(&mut bytes)
        .map_err(|e| format!("git: {e}"))?;
    let cut = bytes.len() as u64 > limit;
    if cut {
        let _ = child.kill();
        bytes.truncate(limit as usize);
        let end = bytes
            .iter()
            .rposition(|b| *b == b'\n')
            .map_or(0, |at| at + 1);
        bytes.truncate(end);
    }
    let mut error = String::new();
    if let Some(mut err) = child.stderr.take() {
        let _ = err.by_ref().take(4096).read_to_string(&mut error);
    }
    let status = child.wait().map_err(|e| format!("git: {e}"))?;
    if !cut && !status.success() && bytes.is_empty() {
        let error = error.trim();
        return Err(if error.is_empty() {
            format!("git failed ({status})")
        } else {
            error.to_owned()
        });
    }
    Ok(Diff {
        text: String::from_utf8_lossy(&bytes).into_owned(),
        cut,
    })
}

/// git for the tab: it takes no lock an agent's own git would wait on, and
/// names paths as they are.
fn run(at: &Path) -> Command {
    let mut command = git(at);
    command.env("GIT_OPTIONAL_LOCKS", "0").args([
        "-c",
        "core.quotePath=false",
        "-c",
        "color.ui=false",
    ]);
    command
}

#[cfg(test)]
mod tests {
    use super::{Target, diff, view};
    use std::process::Command;

    fn run(dir: &std::path::Path, args: &[&str]) {
        assert!(
            Command::new("git")
                .arg("-C")
                .arg(dir)
                .args(["-c", "user.name=T", "-c", "user.email=t@example.com"])
                .args(args)
                .output()
                .unwrap()
                .status
                .success(),
            "git {args:?}"
        );
    }

    #[test]
    fn shows_changes_commits_worktrees_and_diffs() {
        let tmp = std::env::temp_dir().join(format!("agent-git-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&tmp);
        std::fs::create_dir_all(tmp.join("src")).unwrap();
        run(&tmp, &["init", "-q", "-b", "main"]);
        // Before any commit: a staged new file shows against nothing.
        std::fs::write(tmp.join("a.txt"), "one\ntwo\n").unwrap();
        run(&tmp, &["add", "a.txt"]);
        let first = view(&tmp.join("src")).unwrap();
        assert_eq!(first.commits, []);
        let Ok(d) = diff(
            std::path::Path::new(&first.root),
            &Target::Change {
                path: "a.txt".into(),
                from: None,
                untracked: false,
            },
        ) else {
            panic!("diff before a commit")
        };
        assert!(d.text.contains("+one"), "{}", d.text);
        run(&tmp, &["commit", "-qm", "First"]);
        std::fs::write(tmp.join("a.txt"), "one\nTWO\n").unwrap();
        std::fs::write(tmp.join("b.txt"), "b\n").unwrap();
        run(&tmp, &["add", "b.txt"]);
        run(&tmp, &["commit", "-qm", "Second"]);
        run(&tmp, &["mv", "b.txt", "c.txt"]);
        std::fs::write(tmp.join("src/new file.rs"), "fn main() {}\n").unwrap();
        let wt = std::env::temp_dir().join(format!("agent-git-wt-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&wt);
        run(
            &tmp,
            &[
                "worktree",
                "add",
                "-q",
                "-b",
                "agent/x",
                wt.to_str().unwrap(),
            ],
        );
        let v = view(&tmp).unwrap();
        assert!(v.branch.starts_with("main"), "{}", v.branch);
        let codes: Vec<_> = v
            .changes
            .iter()
            .map(|c| (c.code.as_str(), c.path.as_str(), c.from.as_deref()))
            .collect();
        assert_eq!(
            codes,
            [
                (" M", "a.txt", None),
                ("R ", "c.txt", Some("b.txt")),
                ("??", "src/new file.rs", None)
            ]
        );
        assert!(!v.more);
        assert_eq!(
            v.commits
                .iter()
                .map(|c| c.subject.as_str())
                .collect::<Vec<_>>(),
            ["Second", "First"]
        );
        let trees: Vec<_> = v.worktrees.iter().map(|w| w.branch.as_deref()).collect();
        assert_eq!(trees, [Some("main"), Some("agent/x")]);
        let root = std::path::Path::new(&v.root);
        let changed = diff(
            root,
            &Target::Change {
                path: "a.txt".into(),
                from: None,
                untracked: false,
            },
        )
        .unwrap();
        assert!(changed.text.contains("-two\n+TWO\n"), "{}", changed.text);
        let renamed = diff(
            root,
            &Target::Change {
                path: "c.txt".into(),
                from: Some("b.txt".into()),
                untracked: false,
            },
        )
        .unwrap();
        assert!(
            renamed.text.contains("rename from b.txt"),
            "{}",
            renamed.text
        );
        let new = diff(
            root,
            &Target::Change {
                path: "src/new file.rs".into(),
                from: None,
                untracked: true,
            },
        )
        .unwrap();
        assert!(new.text.contains("+fn main() {}"), "{}", new.text);
        let shown = diff(root, &Target::Commit(v.commits[0].sha.clone())).unwrap();
        assert!(shown.text.contains("+b\n") && !shown.text.contains("TWO"));
        assert!(diff(root, &Target::Commit("HEAD~1".into())).is_err());
        // A path is a path, not a pattern.
        let star = diff(
            root,
            &Target::Change {
                path: "*.txt".into(),
                from: None,
                untracked: false,
            },
        )
        .unwrap();
        assert_eq!(star.text, "");
        let _ = std::fs::remove_dir_all(&tmp);
        let _ = std::fs::remove_dir_all(&wt);
    }

    #[test]
    fn a_long_diff_is_cut_at_a_line() {
        let tmp = std::env::temp_dir().join(format!("agent-git-long-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&tmp);
        std::fs::create_dir_all(&tmp).unwrap();
        run(&tmp, &["init", "-q"]);
        let long: String = (0..200_000).map(|i| format!("line {i}\n")).collect();
        std::fs::write(tmp.join("big.txt"), long).unwrap();
        let d = diff(
            &tmp,
            &Target::Change {
                path: "big.txt".into(),
                from: None,
                untracked: true,
            },
        )
        .unwrap();
        assert!(d.cut);
        assert!(d.text.len() as u64 <= super::MAX_DIFF && d.text.ends_with('\n'));
        let _ = std::fs::remove_dir_all(&tmp);
    }
}
