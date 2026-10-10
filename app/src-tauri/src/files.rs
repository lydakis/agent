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
    // The top named from `dir` (`--show-cdup`, not `--show-toplevel`), so
    // a path found is spelled as the agent's folder is, through any
    // symlink, and matches the paths its steps write.
    let up = git(dir)
        .args(["rev-parse", "--show-cdup"])
        .stderr(Stdio::null())
        .output()
        .map_err(|e| format!("git: {e}"))?;
    if !up.status.success() {
        return Err(format!(
            "{}: not in a git repository; ⌘P lists a repository's files",
            dir.display()
        ));
    }
    let mut top = dir.to_path_buf();
    for _ in String::from_utf8_lossy(&up.stdout).matches("../") {
        top.pop();
    }
    let root = top.to_string_lossy().into_owned();
    // A tracked file deleted from the folder but not yet from the index
    // would open as "no such file", so it is left out.
    let deleted = answer(&root, &["ls-files", "-z", "--deleted"])?;
    let deleted: HashSet<&[u8]> = deleted.split(|b| *b == 0).collect();
    // New files first, by name, then the index's entries with their modes
    // (`--stage`), so that what is not a file can be left out.
    let mut child = git(Path::new(&root))
        .args([
            "ls-files",
            "-z",
            "--stage",
            "--others",
            "--exclude-standard",
        ])
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .map_err(|e| format!("git: {e}"))?;
    // Read name by name, so git stops at either bound rather than listing
    // the whole repository first.
    let stdout = child.stdout.take().expect("piped");
    let mut out = BufReader::new(stdout.take(MAX_BYTES + 1));
    let (mut files, mut record) = (Vec::<String>::new(), Vec::new());
    let more = loop {
        record.clear();
        out.read_until(0, &mut record)
            .map_err(|e| format!("git ls-files: {e}"))?;
        // The end, or a record cut at the byte bound.
        if record.pop() != Some(0) {
            break !record.is_empty();
        }
        if files.len() == MAX_FILES {
            break true;
        }
        let Some(name) = openable(&record) else {
            continue;
        };
        // A name that is not UTF-8 cannot be opened by its path from the
        // page, so it is left out too; a file in conflict is listed once
        // for each side, and kept once.
        if !deleted.contains(name)
            && let Ok(text) = std::str::from_utf8(name)
            && files.last().is_none_or(|last| last != text)
        {
            files.push(text.to_owned());
        }
    };
    // A cut that fell just after a record's end reads as the end; the
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

/// The path in one record of `ls-files --stage --others`, if it names a
/// file: an index entry (`<mode> <object> <stage>\t<path>`) that is a file
/// or a symlink, not a submodule (`160000`); a new file, not a folder
/// holding another repository (`nested/`).
fn openable(record: &[u8]) -> Option<&[u8]> {
    let staged =
        record.len() > 7 && record[6] == b' ' && record[..6].iter().all(u8::is_ascii_digit);
    if !staged {
        return (record.last() != Some(&b'/')).then_some(record);
    }
    let tab = record.iter().position(|b| *b == b'\t')?;
    (record.starts_with(b"100") || record.starts_with(b"120")).then(|| &record[tab + 1..])
}

/// What git prints.
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
        run(&tmp, &["add", "README.md", "gone.md"]);
        let gitlink = "160000,1111111111111111111111111111111111111111,lib/sub";
        run(&tmp, &["update-index", "--add", "--cacheinfo", gitlink]);
        // An untracked clone inside, which git names as `nested/`.
        std::fs::create_dir_all(tmp.join("nested")).unwrap();
        run(&tmp.join("nested"), &["init", "-q"]);
        std::fs::write(tmp.join("nested/x"), "").unwrap();
        std::fs::remove_file(tmp.join("gone.md")).unwrap();
        // The top is spelled from the folder asked about, through a symlink.
        let link = std::env::temp_dir().join(format!("agent-files-link-{}", std::process::id()));
        let _ = std::fs::remove_file(&link);
        std::os::unix::fs::symlink(&tmp, &link).unwrap();
        let listing = list(&link.join("src/deep")).unwrap();
        assert_eq!(listing.root, link.to_string_lossy());
        let mut files = listing.files;
        files.sort();
        // Tracked, new and not ignored; the ignored build output, a
        // tracked file deleted from the folder, a submodule and another
        // repository inside are left out.
        assert_eq!(files, [".gitignore", "README.md", "src/deep/a.rs"]);
        assert!(!listing.more);
        let outside = std::env::temp_dir().join(format!("agent-nogit-{}", std::process::id()));
        std::fs::create_dir_all(&outside).unwrap();
        assert!(
            list(&outside)
                .unwrap_err()
                .contains("not in a git repository")
        );
        let _ = std::fs::remove_dir_all(&tmp);
        let _ = std::fs::remove_file(&link);
        let _ = std::fs::remove_dir_all(&outside);
    }
}
