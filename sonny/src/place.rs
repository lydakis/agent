//! Where the bot works, as the footer names it beside the folder: the git
//! branch checked out there. Read off the screen's thread, every few
//! seconds, from the checkout's own `HEAD` file: no git to run each time.
use std::path::{Path, PathBuf};

#[derive(Clone, Debug, Default, PartialEq)]
pub struct Place {
    /// The branch checked out, or the commit a detached checkout is on.
    pub branch: Option<String>,
}

pub fn read(dir: &Path) -> Place {
    let Some(git) = git_dir(dir) else {
        return Place::default();
    };
    let head = std::fs::read_to_string(git.join("HEAD")).unwrap_or_default();
    let head = head.trim();
    let branch = match head.strip_prefix("ref: ") {
        // A reftable store keeps a stand-in here, and git knows the rest.
        Some("refs/heads/.invalid") => None,
        Some(name) => name.strip_prefix("refs/heads/").map(str::to_owned),
        None => is_commit(head).then(|| head[..7].to_owned()),
    };
    match branch {
        Some(_) => Place { branch },
        None => asked(dir),
    }
}

/// The git folder of the checkout holding `dir`: the nearest `.git` above
/// it, or the folder a `.git` file names, as a worktree's or submodule's does.
fn git_dir(dir: &Path) -> Option<PathBuf> {
    let found = dir
        .ancestors()
        .map(|at| at.join(".git"))
        .find(|git| git.exists())?;
    if found.is_dir() {
        return Some(found);
    }
    let text = std::fs::read_to_string(&found).ok()?;
    let named = Path::new(text.trim().strip_prefix("gitdir: ")?);
    Some(found.parent()?.join(named))
}

fn is_commit(head: &str) -> bool {
    matches!(head.len(), 40 | 64) && head.bytes().all(|b| b.is_ascii_hexdigit())
}

/// The branch as git says it, for a `HEAD` this cannot read.
fn asked(dir: &Path) -> Place {
    let branch = crate::task::git(dir, &["branch", "--show-current"])
        .ok()
        .filter(|branch| !branch.is_empty())
        .or_else(|| crate::task::git(dir, &["rev-parse", "--short", "HEAD"]).ok());
    Place { branch }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::process::Command;

    #[test]
    fn a_checkout_names_its_branch_and_a_plain_folder_nothing() {
        let dir = std::env::temp_dir().join(format!("sonny-place-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        assert_eq!(read(&dir).branch, None);
        let git = |args: &[&str]| {
            let done = Command::new("git")
                .arg("-C")
                .arg(&dir)
                .args(args)
                .output()
                .unwrap();
            assert!(done.status.success(), "{done:?}");
        };
        git(&["init", "-q", "-b", "agent/fix-cart"]);
        assert_eq!(read(&dir).branch.as_deref(), Some("agent/fix-cart"));
        git(&[
            "-c",
            "user.name=t",
            "-c",
            "user.email=t@t",
            "commit",
            "-q",
            "--allow-empty",
            "-m",
            "a",
        ]);
        std::fs::create_dir_all(dir.join("src/deep")).unwrap();
        assert_eq!(
            read(&dir.join("src/deep")).branch.as_deref(),
            Some("agent/fix-cart")
        );
        let tree = dir.with_extension("tree");
        let _ = std::fs::remove_dir_all(&tree);
        git(&[
            "worktree",
            "add",
            "-q",
            "-b",
            "agent/other",
            tree.to_str().unwrap(),
        ]);
        assert_eq!(read(&tree).branch.as_deref(), Some("agent/other"));
        git(&["checkout", "-q", "--detach"]);
        let short = Command::new("git")
            .arg("-C")
            .arg(&dir)
            .args(["rev-parse", "--short=7", "HEAD"])
            .output()
            .unwrap();
        let short = String::from_utf8(short.stdout).unwrap();
        assert_eq!(read(&dir).branch.as_deref(), Some(short.trim()));
        std::fs::remove_dir_all(&tree).unwrap();
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
