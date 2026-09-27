//! A bot's branch, when its folder is a linked git worktree: what the app
//! shows after a task's name. Read from the worktree's files, so drawing a
//! head never runs git.
use std::path::Path;

/// The branch checked out in `dir` when it is a linked worktree; `None` for
/// a main checkout, a detached head, or anything else.
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
    use super::linked_branch;
    use std::process::Command;

    fn git(dir: &std::path::Path, args: &[&str]) {
        let status = Command::new("git")
            .arg("-C")
            .arg(dir)
            .args(args)
            .status()
            .unwrap();
        assert!(status.success(), "git {args:?}");
    }

    #[test]
    fn a_linked_worktree_names_its_branch_and_a_main_checkout_does_not() {
        let base = std::env::temp_dir().join(format!("agent-app-worktree-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&base);
        let repo = base.join("repo");
        std::fs::create_dir_all(&repo).unwrap();
        git(&repo, &["init", "--quiet", "-b", "main"]);
        std::fs::write(repo.join("a.txt"), "one\n").unwrap();
        git(&repo, &["add", "."]);
        git(
            &repo,
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
        let tree = base.join("worktrees").join("app.build");
        git(
            &repo,
            &[
                "worktree",
                "add",
                "--quiet",
                "-b",
                "agent/app.build",
                tree.to_str().unwrap(),
                "HEAD",
            ],
        );
        assert_eq!(linked_branch(&tree).as_deref(), Some("agent/app.build"));
        assert_eq!(linked_branch(&repo), None);
        assert_eq!(linked_branch(&base.join("missing")), None);
        let _ = std::fs::remove_dir_all(&base);
    }
}
