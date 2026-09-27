//! A new bot's own git worktree. `run --new --worktree` checks out the
//! source folder's `HEAD` on a new branch `agent/NAME` in
//! `worktrees/NAME` beside the store, runs the project's setup script
//! there, and the bot is created in it. Git does the work; the daemon only
//! sees a folder. A worktree outlives its bot: it holds the bot's branch.
use crate::Error;
use std::{
    path::{Path, PathBuf},
    process::{Command, Stdio},
};

/// Run in each new worktree, from the source folder's repository:
/// executed when it is executable, else read by `sh`.
pub const SETUP: &str = ".agent/setup";

/// Where a store's worktrees live: `worktrees/` beside it, so separate
/// stores, like their bot names, never share one.
pub fn root(store: &Path) -> PathBuf {
    store
        .parent()
        .unwrap_or_else(|| Path::new("."))
        .join("worktrees")
}

#[derive(Debug)]
pub struct Worktree {
    pub path: PathBuf,
    pub branch: String,
    /// The source repository's top level, where the branch lives.
    repo: PathBuf,
}

fn git(dir: &Path, args: &[&str]) -> Result<String, Error> {
    let out = Command::new("git")
        .arg("-C")
        .arg(dir)
        .args(args)
        .stdin(Stdio::null())
        .output()
        .map_err(|e| Error::with("worktree_failed", &format!("git: {e}")))?;
    if !out.status.success() {
        let stderr = String::from_utf8_lossy(&out.stderr);
        return Err(Error::with("worktree_failed", stderr.trim()));
    }
    Ok(String::from_utf8_lossy(&out.stdout).trim().to_owned())
}

/// A bot name that is also a safe folder and branch name.
fn usable(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= 128
        && name
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"-_.".contains(&b))
        && !name.starts_with('.')
        && !name.contains("..")
        && !name.ends_with(".lock")
}

/// Check out `source`'s `HEAD` for bot `name` and run the setup script
/// there. Setup's output goes to `setup_output`, never to the caller's
/// stdout, which may be carrying a turn handle. On any failure nothing is
/// left behind: no folder, no branch.
pub fn create(
    root: &Path,
    source: &Path,
    name: &str,
    setup_output: Stdio,
) -> Result<Worktree, Error> {
    if !usable(name) {
        return Err(Error::with(
            "invalid_name",
            "a worktree bot's name is 1-128 of A-Z a-z 0-9 - _ ., not starting with . or holding ..",
        ));
    }
    let repo = git(source, &["rev-parse", "--show-toplevel"])
        .map_err(|e| Error::with("worktree_needs_git", e.detail.as_deref().unwrap_or("")))?;
    let repo = PathBuf::from(repo);
    std::fs::create_dir_all(root)
        .map_err(|e| Error::with("worktree_failed", &format!("{}: {e}", root.display())))?;
    let path = root.join(name);
    if path.symlink_metadata().is_ok() {
        return Err(Error::with(
            "worktree_exists",
            &format!(
                "{} is left from an earlier bot; remove it with git worktree remove",
                path.display()
            ),
        ));
    }
    let branch = format!("agent/{name}");
    let path_arg = path.to_str().ok_or(Error::new("workspace_not_utf8"))?;
    git(
        &repo,
        &[
            "worktree", "add", "--quiet", "-b", &branch, path_arg, "HEAD",
        ],
    )?;
    let worktree = Worktree { path, branch, repo };
    if let Err(error) = worktree.setup(setup_output) {
        worktree.remove();
        return Err(error);
    }
    Ok(worktree)
}

impl Worktree {
    fn setup(&self, output: Stdio) -> Result<(), Error> {
        let script = self.repo.join(SETUP);
        let Ok(meta) = std::fs::metadata(&script) else {
            return Ok(());
        };
        use std::os::unix::fs::PermissionsExt;
        let mut command = if meta.permissions().mode() & 0o111 != 0 {
            Command::new(&script)
        } else {
            let mut sh = Command::new("sh");
            sh.arg(&script);
            sh
        };
        let status = command
            .current_dir(&self.path)
            .env("AGENT_SOURCE", &self.repo)
            .stdin(Stdio::null())
            .stdout(output)
            .status()
            .map_err(|e| Error::with("setup_failed", &format!("{}: {e}", script.display())))?;
        if !status.success() {
            return Err(Error::with(
                "setup_failed",
                &format!("{} exited with {status}", script.display()),
            ));
        }
        Ok(())
    }

    /// Undo `create` before the bot has done anything: the folder and its
    /// branch both go. Best effort; a failure here leaves what git kept.
    pub fn remove(&self) {
        let path = self.path.to_string_lossy();
        let _ = git(&self.repo, &["worktree", "remove", "--force", &path]);
        let _ = git(&self.repo, &["branch", "-D", &self.branch]);
    }
}

/// The branch checked out in `dir` when it is a linked worktree, read from
/// its files without running git; `None` for a main checkout, a detached
/// head, or anything else.
pub fn linked_branch(dir: &Path) -> Option<String> {
    let pointer = std::fs::read_to_string(dir.join(".git")).ok()?;
    let gitdir = Path::new(pointer.strip_prefix("gitdir:")?.trim());
    let gitdir = if gitdir.is_absolute() {
        gitdir.to_owned()
    } else {
        dir.join(gitdir)
    };
    let head = std::fs::read_to_string(gitdir.join("HEAD")).ok()?;
    head.trim()
        .strip_prefix("ref: refs/heads/")
        .map(str::to_owned)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn run(dir: &Path, args: &[&str]) {
        let status = Command::new("git")
            .arg("-C")
            .arg(dir)
            .args(args)
            .status()
            .unwrap();
        assert!(status.success(), "git {args:?}");
    }

    fn repo(name: &str) -> PathBuf {
        let base =
            std::env::temp_dir().join(format!("agent-worktree-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&base);
        let src = base.join("src");
        std::fs::create_dir_all(&src).unwrap();
        run(&src, &["init", "--quiet", "-b", "main"]);
        std::fs::write(src.join("a.txt"), "one\n").unwrap();
        run(&src, &["add", "."]);
        run(
            &src,
            &[
                "-c",
                "user.name=t",
                "-c",
                "user.email=t@example.com",
                "commit",
                "--quiet",
                "-m",
                "one",
            ],
        );
        base
    }

    #[test]
    fn a_worktree_checks_out_head_on_its_own_branch_and_runs_setup_there() {
        let base = repo("setup");
        let src = base.join("src");
        std::fs::create_dir_all(src.join(".agent")).unwrap();
        // Not executable, so it is read by sh; it sees the source folder.
        std::fs::write(src.join(SETUP), "printf '%s' \"$AGENT_SOURCE\" > from\n").unwrap();
        std::fs::write(src.join("a.txt"), "uncommitted\n").unwrap();
        let root = base.join("worktrees");
        let wt = create(&root, &src.join(".agent"), "app.build", Stdio::null()).unwrap();
        assert_eq!(wt.path, root.join("app.build"));
        assert_eq!(wt.branch, "agent/app.build");
        assert_eq!(
            std::fs::read_to_string(wt.path.join("a.txt")).unwrap(),
            "one\n",
            "HEAD, not the source's edits"
        );
        let source = std::fs::canonicalize(&src).unwrap();
        assert_eq!(
            std::fs::read_to_string(wt.path.join("from")).unwrap(),
            source.to_str().unwrap()
        );
        assert_eq!(linked_branch(&wt.path).as_deref(), Some("agent/app.build"));
        assert_eq!(
            linked_branch(&src),
            None,
            "a main checkout is not a linked worktree"
        );
        let again = create(&root, &src, "app.build", Stdio::null()).unwrap_err();
        assert_eq!(again.code, "worktree_exists");
        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn a_failed_setup_leaves_no_folder_and_no_branch() {
        let base = repo("fail");
        let src = base.join("src");
        std::fs::create_dir_all(src.join(".agent")).unwrap();
        std::fs::write(src.join(SETUP), "exit 3\n").unwrap();
        let root = base.join("worktrees");
        let error = create(&root, &src, "t1", Stdio::null()).unwrap_err();
        assert_eq!(error.code, "setup_failed");
        assert!(!root.join("t1").exists());
        assert_eq!(git(&src, &["branch", "--list", "agent/t1"]).unwrap(), "");
        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn names_that_are_not_safe_folders_and_folders_outside_git_are_refused() {
        let base =
            std::env::temp_dir().join(format!("agent-worktree-plain-{}", std::process::id()));
        std::fs::create_dir_all(&base).unwrap();
        for name in ["", "..", ".hidden", "a..b", "x.lock", "a/b"] {
            assert_eq!(
                create(&base.join("w"), &base, name, Stdio::null())
                    .unwrap_err()
                    .code,
                "invalid_name",
                "{name}"
            );
        }
        let plain = tempdir_outside_git();
        assert_eq!(
            create(&base.join("w"), &plain, "ok", Stdio::null())
                .unwrap_err()
                .code,
            "worktree_needs_git"
        );
        let _ = std::fs::remove_dir_all(&base);
        let _ = std::fs::remove_dir_all(&plain);
    }

    /// A folder no parent repository claims, whatever the temp dir sits in.
    fn tempdir_outside_git() -> PathBuf {
        let dir = std::env::temp_dir().join(format!("agent-worktree-nogit-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }
}
