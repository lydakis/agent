//! A new task's worktree starts with clones of the lead's build and
//! dependency folders, so its first build reuses the lead's work instead of
//! starting from nothing. Only copy-on-write clones are made (APFS on macOS,
//! reflinks on Linux): the clone shares the lead's blocks until either side
//! writes, and where the disk cannot clone, the task builds from scratch.
use std::io;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

/// Clone the build folders of the checkout at `top` into the new worktree
/// `copy`, then touch the worktree's tracked files, so a build tool that
/// compares times rebuilds whatever those files hold rather than taking the
/// lead's outputs for theirs. Returns the folders cloned, relative to both.
pub fn seed(top: &Path, copy: &Path) -> Vec<PathBuf> {
    seed_with(top, copy, clone)
}

fn seed_with(
    top: &Path,
    copy: &Path,
    clone: impl Fn(&Path, &Path) -> io::Result<()>,
) -> Vec<PathBuf> {
    let mut seeded = Vec::new();
    for folder in folders(top) {
        let to = copy.join(&folder);
        if to.symlink_metadata().is_ok()
            || to
                .parent()
                .is_some_and(|up| std::fs::create_dir_all(up).is_err())
        {
            continue;
        }
        if clone(&top.join(&folder), &to).is_err() {
            // A disk that cannot clone one folder clones none: take away
            // what this made, and leave the task to build as before.
            let _ = std::fs::remove_dir_all(&to);
            break;
        }
        seeded.push(folder);
    }
    if !seeded.is_empty() {
        touch_tracked(copy);
    }
    seeded
}

/// The folders at `top` git ignores that a build fills: each one a cache
/// says it is with a CACHEDIR.TAG (as Cargo's target does), and each
/// node_modules, among the ignored folders and one level inside them (a
/// `.local/target`). Never a Python environment, which names its own path.
fn folders(top: &Path) -> Vec<PathBuf> {
    // In a read-only sandbox, as every listing of a checkout Sonny makes:
    // git runs the repository's fsmonitor to list it (#597).
    let Ok(listed) = crate::task::confined_bytes(
        top,
        &[
            "ls-files",
            "-z",
            "--others",
            "--ignored",
            "--exclude-standard",
            "--directory",
        ],
        &[],
    ) else {
        return Vec::new();
    };
    let folder = |path: &Path| {
        path.symlink_metadata().is_ok_and(|found| found.is_dir())
            && !path.join("pyvenv.cfg").exists()
            && (path.join("CACHEDIR.TAG").is_file()
                || path.file_name().is_some_and(|name| name == "node_modules"))
    };
    let mut found = Vec::new();
    for entry in listed.split(|byte| *byte == 0) {
        // Folders are the entries that end in a slash.
        let Some(dir) = std::str::from_utf8(entry)
            .ok()
            .and_then(|entry| entry.strip_suffix('/'))
        else {
            continue;
        };
        let dir = PathBuf::from(dir);
        let path = top.join(&dir);
        if folder(&path) {
            found.push(dir);
        } else if path.symlink_metadata().is_ok_and(|found| found.is_dir()) {
            let inside = std::fs::read_dir(&path).into_iter().flatten().flatten();
            found.extend(
                inside
                    .filter(|child| folder(&child.path()))
                    .map(|child| dir.join(child.file_name())),
            );
        }
    }
    found
}

/// Set every tracked file in the checkout at `dir` to now, later than any
/// output cloned beside it.
fn touch_tracked(dir: &Path) {
    use std::os::unix::ffi::OsStrExt;
    let Ok(listed) = crate::task::confined_bytes(dir, &["ls-files", "-z"], &[]) else {
        return;
    };
    for file in listed.split(|byte| *byte == 0) {
        if file.is_empty() {
            continue;
        }
        let path = dir.join(std::ffi::OsStr::from_bytes(file));
        let Ok(path) = std::ffi::CString::new(path.as_os_str().as_bytes()) else {
            continue;
        };
        // SAFETY: `path` is a NUL-terminated string that outlives the call,
        // and null times mean now.
        unsafe {
            libc::utimensat(
                libc::AT_FDCWD,
                path.as_ptr(),
                std::ptr::null(),
                libc::AT_SYMLINK_NOFOLLOW,
            )
        };
    }
}

/// One call clones the whole folder, its files sharing their blocks.
#[cfg(target_os = "macos")]
fn clone(from: &Path, to: &Path) -> io::Result<()> {
    use std::os::unix::ffi::OsStrExt;
    /// sys/clonefile.h: clone a symbolic link itself, not what it names.
    const CLONE_NOFOLLOW: u32 = 0x0001;
    let from = std::ffi::CString::new(from.as_os_str().as_bytes())?;
    let to = std::ffi::CString::new(to.as_os_str().as_bytes())?;
    // SAFETY: both are NUL-terminated strings that outlive the call.
    match unsafe { libc::clonefile(from.as_ptr(), to.as_ptr(), CLONE_NOFOLLOW) } {
        0 => Ok(()),
        _ => Err(io::Error::last_os_error()),
    }
}

/// Reflinks where the disk has them (Btrfs, XFS); never a full copy.
#[cfg(not(target_os = "macos"))]
fn clone(from: &Path, to: &Path) -> io::Result<()> {
    let status = Command::new("cp")
        .args(["-R", "--reflink=always", "--preserve=timestamps,links"])
        .arg(from)
        .arg(to)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()?;
    match status.success() {
        true => Ok(()),
        false => Err(io::Error::other("this disk cannot clone")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{Duration, SystemTime};

    fn git(dir: &Path, args: &[&str]) {
        let done = Command::new("git")
            .arg("-C")
            .arg(dir)
            .args(["-c", "user.name=t", "-c", "user.email=t@t"])
            .args(args)
            .output()
            .unwrap();
        assert!(done.status.success(), "{done:?}");
    }

    fn write(path: &Path, text: &str) {
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, text).unwrap();
    }

    /// A plain copy standing in for a clone, keeping times as one does.
    fn copied(from: &Path, to: &Path) -> io::Result<()> {
        std::fs::create_dir_all(to)?;
        for entry in std::fs::read_dir(from)? {
            let entry = entry?;
            let target = to.join(entry.file_name());
            if entry.file_type()?.is_dir() {
                copied(&entry.path(), &target)?;
            } else {
                std::fs::copy(entry.path(), &target)?;
                let when = entry.metadata()?.modified()?;
                std::fs::File::options()
                    .write(true)
                    .open(&target)?
                    .set_modified(when)?;
            }
        }
        Ok(())
    }

    #[test]
    fn a_new_worktree_starts_with_the_lead_s_build_folders_and_newer_sources() {
        let lab = std::env::temp_dir().join(format!("sonny-seed-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&lab);
        let (lead, task) = (lab.join("lead"), lab.join("task"));
        std::fs::create_dir_all(&lab).unwrap();
        git(&lab, &["init", "-q", "lead"]);
        write(
            &lead.join(".gitignore"),
            "/target\n.local/\nnode_modules/\n.env\n",
        );
        write(&lead.join("src/main.rs"), "fn main() {}\n");
        write(&lead.join("web/package.json"), "{}\n");
        git(&lead, &["add", "."]);
        git(&lead, &["commit", "-qm", "one"]);
        // What builds left: Cargo's targets, one where a project's config
        // puts it, packages, and things that are no build's.
        let tag = "Signature: 8a477f597d28d172789f06886806bc55\n";
        write(&lead.join("target/CACHEDIR.TAG"), tag);
        write(&lead.join("target/debug/app"), "built");
        write(&lead.join(".local/target/CACHEDIR.TAG"), tag);
        write(&lead.join(".local/venv/pyvenv.cfg"), "home = /usr\n");
        write(&lead.join(".local/venv/CACHEDIR.TAG"), tag);
        write(&lead.join(".local/bench/out.json"), "{}");
        write(&lead.join("web/node_modules/left-pad/index.js"), "x");
        write(&lead.join(".env"), "KEY=secret\n");
        git(
            &lead,
            &["worktree", "add", "-q", "-b", "t", task.to_str().unwrap()],
        );
        // The lead's build finished after the worktree was checked out.
        let set = |path: &Path, when: SystemTime| {
            let file = std::fs::File::options().write(true).open(path).unwrap();
            file.set_modified(when).unwrap();
        };
        let now = SystemTime::now();
        set(&task.join("src/main.rs"), now - Duration::from_secs(3600));
        set(
            &lead.join("target/debug/app"),
            now - Duration::from_secs(60),
        );

        let mut seeded = seed_with(&lead, &task, copied);
        seeded.sort();
        assert_eq!(
            seeded,
            [".local/target", "target", "web/node_modules"].map(PathBuf::from)
        );
        assert_eq!(
            std::fs::read_to_string(task.join("target/debug/app")).unwrap(),
            "built"
        );
        for left in [".local/venv", ".local/bench", ".env"] {
            assert!(!task.join(left).exists(), "{left}");
        }
        // Even an output newer than the checkout is older than its sources.
        let modified = |path: &Path| std::fs::metadata(path).unwrap().modified().unwrap();
        assert!(modified(&task.join("src/main.rs")) > modified(&task.join("target/debug/app")));

        // A disk that cannot clone leaves the worktree as git made it.
        let bare = lab.join("bare");
        git(
            &lead,
            &["worktree", "add", "-q", "-b", "b", bare.to_str().unwrap()],
        );
        let refused = seed_with(&lead, &bare, |_: &Path, to: &Path| {
            std::fs::create_dir_all(to)?;
            Err(io::Error::other("no clones here"))
        });
        assert!(refused.is_empty());
        assert!(!bare.join("target").exists() && !bare.join(".local/target").exists());
        let _ = std::fs::remove_dir_all(&lab);
    }
}
