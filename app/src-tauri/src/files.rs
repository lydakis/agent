//! The files ⌘P searches: those git tracks in the repository a folder is
//! in, and those it would track (new, not ignored), as an editor lists
//! them. git reads the index and `.gitignore`, so the app walks no folder.
use std::collections::HashSet;
use std::io::{BufRead, BufReader, Read};
use std::path::Path;
use std::process::{Command, Stdio};

/// At most this many paths, from at most this many bytes of git's answer;
/// past either the listing stops and says so.
pub const MAX_FILES: usize = 100_000;
const MAX_BYTES: u64 = 16 * 1024 * 1024;

#[derive(Debug, PartialEq)]
pub struct Listing {
    /// The repository's top folder; each file is a path under it.
    pub root: String,
    pub files: Vec<String>,
    /// More files than listed.
    pub more: bool,
}

pub fn list(dir: &Path) -> Result<Listing, String> {
    let top = git(dir)
        .args(["rev-parse", "--show-toplevel"])
        .stderr(Stdio::null())
        .output()
        .map_err(|e| format!("git: {e}"))?;
    if !top.status.success() {
        return Err(format!(
            "{}: not in a git repository; ⌘P lists a repository's files",
            dir.display()
        ));
    }
    let root = String::from_utf8_lossy(&top.stdout);
    let root = root.strip_suffix('\n').unwrap_or(&root).to_owned();
    // Left out, as each would open as "no such file": a tracked file
    // deleted from the folder but not yet from the index, and a submodule,
    // which git lists as one entry but is a folder.
    let deleted = answer(&root, &["ls-files", "-z", "--deleted"])?;
    let modules = answer(
        &root,
        &[
            "config",
            "-z",
            "--file",
            ".gitmodules",
            "--get-regexp",
            r"^submodule\..*\.path$",
        ],
    )?;
    let mut skip: HashSet<&[u8]> = deleted.split(|b| *b == 0).collect();
    // Each entry is its key, a newline, then the path.
    skip.extend(modules.split(|b| *b == 0).filter_map(|entry| {
        let at = entry.iter().position(|b| *b == b'\n')?;
        Some(&entry[at + 1..])
    }));
    let mut child = git(Path::new(&root))
        .args([
            "ls-files",
            "-z",
            "--cached",
            "--others",
            "--exclude-standard",
            "--deduplicate",
        ])
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .map_err(|e| format!("git: {e}"))?;
    // Read name by name, so git stops at either bound rather than listing
    // the whole repository first.
    let stdout = child.stdout.take().expect("piped");
    let mut out = BufReader::new(stdout.take(MAX_BYTES + 1));
    let (mut files, mut name) = (Vec::new(), Vec::new());
    let more = loop {
        name.clear();
        out.read_until(0, &mut name)
            .map_err(|e| format!("git ls-files: {e}"))?;
        // The end, or a name cut at the byte bound.
        if name.pop() != Some(0) {
            break !name.is_empty();
        }
        if files.len() == MAX_FILES {
            break true;
        }
        // A name that is not UTF-8 cannot be opened by its path from the
        // page, so it is left out too.
        if !skip.contains(&name[..])
            && let Ok(text) = std::str::from_utf8(&name)
        {
            files.push(text.to_owned());
        }
    };
    // A cut that fell just after a name's end reads as the end; the
    // spent byte bound still says git had more to give.
    let more = more || out.get_ref().limit() == 0;
    if more {
        let _ = child.kill();
    }
    let status = child.wait().map_err(|e| format!("git ls-files: {e}"))?;
    if !more && !status.success() {
        return Err(format!("{root}: git ls-files failed ({status})"));
    }
    Ok(Listing { root, files, more })
}

/// What git prints, whether or not it succeeds: no `.gitmodules` is no
/// submodules.
fn answer(root: &str, args: &[&str]) -> Result<Vec<u8>, String> {
    let out = git(Path::new(root))
        .args(args)
        .stderr(Stdio::null())
        .output()
        .map_err(|e| format!("git: {e}"))?;
    Ok(out.stdout)
}

/// git about `dir` alone: a `GIT_DIR` and the like from the app's own
/// environment would name another repository.
fn git(dir: &Path) -> Command {
    let mut command = Command::new("git");
    for key in [
        "GIT_DIR",
        "GIT_WORK_TREE",
        "GIT_COMMON_DIR",
        "GIT_INDEX_FILE",
        "GIT_OBJECT_DIRECTORY",
        "GIT_ALTERNATE_OBJECT_DIRECTORIES",
        "GIT_NAMESPACE",
    ] {
        command.env_remove(key);
    }
    command.arg("-C").arg(dir).stdin(Stdio::null());
    command
}

#[cfg(test)]
mod tests {
    use super::list;
    use std::process::Command;

    fn run(dir: &std::path::Path, args: &[&str]) {
        assert!(
            Command::new("git")
                .arg("-C")
                .arg(dir)
                .args(args)
                .status()
                .unwrap()
                .success()
        );
    }

    #[test]
    fn lists_tracked_and_new_files_from_any_folder_in_the_repository() {
        let tmp = std::env::temp_dir().join(format!("agent-files-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&tmp);
        std::fs::create_dir_all(tmp.join("src/deep")).unwrap();
        run(&tmp, &["init", "-q"]);
        std::fs::write(tmp.join(".gitignore"), "target/\n").unwrap();
        std::fs::write(tmp.join("README.md"), "hi").unwrap();
        std::fs::write(tmp.join("src/deep/a.rs"), "").unwrap();
        std::fs::create_dir_all(tmp.join("target")).unwrap();
        std::fs::write(tmp.join("target/out.bin"), "").unwrap();
        std::fs::write(tmp.join("gone.md"), "").unwrap();
        std::fs::write(
            tmp.join(".gitmodules"),
            "[submodule \"sub\"]\n\tpath = lib/sub\n\turl = ./sub\n",
        )
        .unwrap();
        run(&tmp, &["add", "README.md", "gone.md"]);
        let gitlink = "160000,1111111111111111111111111111111111111111,lib/sub";
        run(&tmp, &["update-index", "--add", "--cacheinfo", gitlink]);
        std::fs::remove_file(tmp.join("gone.md")).unwrap();
        let root = std::fs::canonicalize(&tmp).unwrap();
        let listing = list(&tmp.join("src/deep")).unwrap();
        assert_eq!(listing.root, root.to_string_lossy());
        let mut files = listing.files;
        files.sort();
        // Tracked, new and not ignored; the ignored build output, a
        // tracked file deleted from the folder and a submodule are left out.
        assert_eq!(
            files,
            [".gitignore", ".gitmodules", "README.md", "src/deep/a.rs"]
        );
        assert!(!listing.more);
        let outside = std::env::temp_dir().join(format!("agent-nogit-{}", std::process::id()));
        std::fs::create_dir_all(&outside).unwrap();
        assert!(
            list(&outside)
                .unwrap_err()
                .contains("not in a git repository")
        );
        let _ = std::fs::remove_dir_all(&tmp);
        let _ = std::fs::remove_dir_all(&outside);
    }
}
