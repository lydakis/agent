//! git as Sonny runs it on a checkout a task may have written: the git
//! found from the root of `PATH`, with no hook, fsmonitor or transport of
//! the repository's, and none of the keys Agent keeps; and folders below a
//! worktree opened without following a link.

use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

/// The git Sonny runs: the first on `PATH` in a folder named from the
/// root, found once. A folder named relative to where git runs, `.` say,
/// would be inside the checkout, where a task may put a git of its own.
pub(crate) fn git_program() -> &'static Path {
    static FOUND: std::sync::LazyLock<PathBuf> = std::sync::LazyLock::new(|| {
        // None found runs nothing: an absolute path that is not there.
        find_git(&std::env::var_os("PATH").unwrap_or_default())
            .unwrap_or_else(|| PathBuf::from("/nonexistent/git"))
    });
    &FOUND
}

fn find_git(path: &std::ffi::OsStr) -> Option<PathBuf> {
    use std::os::unix::fs::PermissionsExt;
    (std::env::split_paths(path))
        .filter(|dir| dir.is_absolute())
        .map(|dir| dir.join("git"))
        .find(|git| {
            std::fs::metadata(git).is_ok_and(|m| m.is_file() && m.permissions().mode() & 0o111 != 0)
        })
}

fn git(dir: &Path, args: &[&str]) -> Result<String, String> {
    let mut command = Command::new(git_program());
    command.arg("-C").arg(dir);
    // The folder's own code never runs here, where the daemon's keys are:
    // no hook, no fsmonitor, and none of the keys in its environment.
    command
        .args([
            "-c",
            "core.hooksPath=/dev/null",
            "-c",
            "core.fsmonitor=false",
        ])
        .args(args)
        .env("GIT_TERMINAL_PROMPT", "0")
        // Nor any transport: an object missing from a partial clone is not
        // fetched, through a program a remote of its config names.
        .env("GIT_NO_LAZY_FETCH", "1")
        .env("GIT_ALLOW_PROTOCOL", "")
        .stdin(Stdio::null());
    for key in crate::task::kept_keys() {
        command.env_remove(key);
    }
    let output = command.output().map_err(|e| format!("running git: {e}"))?;
    if !output.status.success() {
        let said = String::from_utf8_lossy(&output.stderr);
        return Err(format!("git {}: {}", args.join(" "), said.trim()));
    }
    Ok(String::from_utf8_lossy(&output.stdout).trim().to_owned())
}

/// git in `dir` as Sonny runs it where it reads a checkout's files: no
/// hook and no fsmonitor of the repository's runs. For what reads no
/// files git would filter, such as removing a worktree with `--force`,
/// which checks none, or deleting a branch.
pub(crate) fn plain(dir: &Path, args: &[&str]) -> Result<String, String> {
    git(dir, args)
}

/// Take away the worktree at `path` of the repository `here` is in, and
/// its branch, as `plain` runs git: so no hook or program of the folder's
/// config runs, as it would where a task wrote it.
pub(crate) fn unwind(here: &Path, path: &Path, branch: &str) {
    let path = path.to_string_lossy();
    let _ = git(here, &["worktree", "remove", "--force", "--force", &path]);
    let _ = git(here, &["branch", "-D", branch]);
}

/// `name` as C takes it.
fn c(name: &[u8]) -> std::io::Result<std::ffi::CString> {
    std::ffi::CString::new(name)
        .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidInput, e))
}

/// The folder `name` in the folder `at`, opened without following a link.
fn folder(at: std::os::fd::RawFd, name: &std::ffi::CStr) -> std::io::Result<std::os::fd::OwnedFd> {
    use std::os::fd::FromRawFd;
    let flags = libc::O_RDONLY | libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC;
    // SAFETY: `name` is NUL-terminated and outlives the call.
    let fd = unsafe { libc::openat(at, name.as_ptr(), flags) };
    if fd < 0 {
        return Err(std::io::Error::last_os_error());
    }
    // SAFETY: `fd` was just opened, and nothing else owns it.
    Ok(unsafe { std::os::fd::OwnedFd::from_raw_fd(fd) })
}

/// The folder `parts` names below `root`, each opened without following a
/// link, so none can lead out of what a task may write; one missing is
/// made where `make`.
pub(crate) fn folders(
    root: &Path,
    parts: &[&std::ffi::OsStr],
    make: bool,
) -> std::io::Result<std::os::fd::OwnedFd> {
    use std::os::fd::AsRawFd;
    use std::os::unix::ffi::OsStrExt;
    let mut at = folder(libc::AT_FDCWD, &c(root.as_os_str().as_bytes())?)?;
    for part in parts {
        let name = c(part.as_bytes())?;
        if make {
            // SAFETY: as above. One already there is opened as it is.
            unsafe { libc::mkdirat(at.as_raw_fd(), name.as_ptr(), 0o755) };
        }
        at = folder(at.as_raw_fd(), &name)?;
    }
    Ok(at)
}

/// Before 0.9.0 the lookout kept a task's files after each turn it
/// completed, for a fork from that turn, as commits under
/// `refs/agent/snapshots` in its repository, staged through indexes in a
/// folder beside the store. That folder, Sonny's own, goes once a store.
pub const NO_SNAPSHOTS: &str = "no-snapshots";

/// Drop the snapshot indexes beside `store`, once.
pub fn drop_snapshot_indexes(store: &Path) {
    let (mut record, mut indexes) = (store.as_os_str().to_owned(), store.as_os_str().to_owned());
    record.push(".migrated");
    indexes.push(".snapshot-indexes");
    crate::upgrade::once(
        Path::new(&record),
        NO_SNAPSHOTS,
        || match std::fs::remove_dir_all(&indexes) {
            Ok(()) => true,
            Err(e) => e.kind() == std::io::ErrorKind::NotFound,
        },
    );
}

/// Whether no task worktree's repository holds snapshot refs, which are
/// the person's repository's to drop: read from its files, running no git.
pub fn no_snapshot_refs() -> bool {
    let Some(home) = std::env::var_os("HOME").map(PathBuf::from) else {
        return true;
    };
    let Ok(worktrees) = std::fs::read_dir(home.join(".agent/worktrees")) else {
        return true;
    };
    worktrees
        .flatten()
        .all(|entry| !snapshot_refs_in(&entry.path()))
}

/// Whether the repository of the worktree `worktree` holds snapshot refs,
/// loose or packed.
fn snapshot_refs_in(worktree: &Path) -> bool {
    let Ok(link) = std::fs::read_to_string(worktree.join(".git")) else {
        return false;
    };
    let Some(own) = link.trim().strip_prefix("gitdir: ") else {
        return false;
    };
    let own = worktree.join(own);
    let common = std::fs::read_to_string(own.join("commondir"))
        .map_or(own.clone(), |common| own.join(common.trim()));
    common.join("refs/agent/snapshots").exists()
        || std::fs::read_to_string(common.join("packed-refs"))
            .is_ok_and(|packed| packed.contains(" refs/agent/snapshots/"))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A folder of the test's own, emptied first.
    fn scratch(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("sonny-git-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::canonicalize(dir).unwrap()
    }

    #[test]
    fn the_git_run_is_found_in_a_folder_named_from_the_root() {
        use std::os::unix::fs::PermissionsExt;
        let dir = scratch("program");
        let (planted, real) = (dir.join("checkout"), dir.join("bin"));
        for folder in [&planted, &real] {
            std::fs::create_dir(folder).unwrap();
            std::fs::write(folder.join("git"), "#!/bin/sh\n").unwrap();
            std::fs::set_permissions(folder.join("git"), std::fs::Permissions::from_mode(0o755))
                .unwrap();
        }
        // The planted one, named from where this runs.
        let here = std::env::current_dir().unwrap();
        let mut relative: PathBuf = here.components().skip(1).map(|_| "..").collect();
        relative.push(planted.strip_prefix("/").unwrap());
        assert!(relative.join("git").exists());
        let path = std::env::join_paths([relative.as_path(), &real]).unwrap();
        assert_eq!(find_git(&path), Some(real.join("git")));
        assert_eq!(find_git(relative.as_os_str()), None);
        assert!(git_program().is_absolute());
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn no_hook_of_the_folder_runs() {
        let dir = scratch("hooks");
        let top = dir.join("repo");
        std::fs::create_dir(&top).unwrap();
        plain(&top, &["init", "-q", "-b", "main"]).unwrap();
        let ran = dir.join("ran");
        for hook in ["reference-transaction", "post-checkout"] {
            let path = top.join(".git/hooks").join(hook);
            std::fs::write(&path, format!("#!/bin/sh\ntouch '{}'\n", ran.display())).unwrap();
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
        }
        let id = ["-c", "user.name=t", "-c", "user.email=t@t"];
        plain(
            &top,
            &[&id[..], &["commit", "-q", "--allow-empty", "-m", "one"]].concat(),
        )
        .unwrap();
        let copy = dir.join("copy").to_string_lossy().into_owned();
        plain(&top, &["worktree", "add", "-q", "-b", "agent/copy", &copy]).unwrap();
        unwind(&top, Path::new(&copy), "agent/copy");
        assert!(!ran.exists());
        assert!(!Path::new(&copy).exists());
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn snapshot_refs_are_found_loose_or_packed_through_a_worktree() {
        let dir = scratch("refs");
        let (top, copy) = (dir.join("repo"), dir.join("copy"));
        std::fs::create_dir(&top).unwrap();
        plain(&top, &["init", "-q", "-b", "main"]).unwrap();
        let id = ["-c", "user.name=t", "-c", "user.email=t@t"];
        plain(
            &top,
            &[&id[..], &["commit", "-q", "--allow-empty", "-m", "one"]].concat(),
        )
        .unwrap();
        let path = copy.to_string_lossy().into_owned();
        plain(&top, &["worktree", "add", "-q", "-b", "agent/copy", &path]).unwrap();
        assert!(!snapshot_refs_in(&copy));
        plain(&top, &["update-ref", "refs/agent/snapshots/a/1/2", "HEAD"]).unwrap();
        assert!(snapshot_refs_in(&copy));
        plain(&top, &["pack-refs", "--all"]).unwrap();
        assert!(snapshot_refs_in(&copy));
        plain(&top, &["update-ref", "-d", "refs/agent/snapshots/a/1/2"]).unwrap();
        assert!(!snapshot_refs_in(&copy));
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn a_store_s_snapshot_indexes_go_once() {
        let dir = scratch("indexes");
        let store = dir.join("state.sqlite");
        std::fs::create_dir_all(dir.join("state.sqlite.snapshot-indexes/x")).unwrap();
        drop_snapshot_indexes(&store);
        assert!(!dir.join("state.sqlite.snapshot-indexes").exists());
        assert!(
            std::fs::read_to_string(dir.join("state.sqlite.migrated"))
                .unwrap()
                .contains(NO_SNAPSHOTS)
        );
        let _ = std::fs::remove_dir_all(dir);
    }
}
