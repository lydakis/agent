//! Where the bot works, as the footer names it beside the folder: the git
//! branch checked out there, and the folder a sandfs copy was made from.
//! Read off the screen's thread, since both ask outside programs or files.
use std::path::Path;

#[derive(Clone, Debug, Default, PartialEq)]
pub struct Place {
    /// The branch checked out, or the commit a detached checkout is on.
    pub branch: Option<String>,
    /// What the folder is a sandfs copy of, when it is one, as sandfs
    /// names its source: the folder it was made from, usually.
    pub copy_of: Option<String>,
}

pub fn read(dir: &Path) -> Place {
    let branch = crate::task::git(dir, &["branch", "--show-current"])
        .ok()
        .filter(|branch| !branch.is_empty())
        .or_else(|| crate::task::git(dir, &["rev-parse", "--short", "HEAD"]).ok());
    let copy_of = crate::task::mount_of(dir)
        .ok()
        .flatten()
        .map(|workspace| workspace.source);
    Place { branch, copy_of }
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
        git(&["checkout", "-q", "--detach"]);
        let detached = read(&dir).branch.unwrap();
        assert!(detached.len() >= 7 && detached.chars().all(|c| c.is_ascii_hexdigit()));
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
