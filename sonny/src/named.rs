//! Sonny's long-running processes by names of their own, so a process
//! list (`top`, `ps`, Activity Monitor) tells the daemon Sonny starts and
//! each of its companions apart, rather than showing `agent` and a row of
//! `sonny`. A process list shows the name of the file a process was
//! started from, which a changed `argv[0]` or a symbolic link does not
//! change everywhere, so each name is a hard link to its program in
//! `~/.agent/sonny/programs/names/`, where no sandboxed agent writes. Where
//! a link cannot be made (another file system, a program the person does
//! not own), it is a link to one private copy of the program instead, and
//! where neither can be made, the program runs by its own name as before.
//! Each name is made again when its program changes, and only then.
use std::path::{Path, PathBuf};

/// The daemon Sonny starts.
pub const DAEMON: &str = "sonny-daemon";
/// The companions the daemon keeps for Sonny, and the scheduler.
pub const LOOKOUT: &str = "sonny-lookout";
pub const TOOL_HOST: &str = "sonny-tool-host";
pub const REVIEWER: &str = "sonny-reviewer";
pub const SCHEDULER: &str = "sonny-scheduler";

/// What a named Sonny process is told its own program is: the `sonny` it
/// was started for, so what it runs or finds beside itself is Sonny's,
/// not the name's.
pub const PROGRAM: &str = "SONNY_PROGRAM";

/// This Sonny's own program. A process started by one of the names is
/// given its program in [`PROGRAM`], which is taken while it names a file;
/// any other process is the program it was started from.
pub fn me() -> std::io::Result<PathBuf> {
    let exe = std::env::current_exe()?;
    if exe.parent() == Some(dir().as_path())
        && let Some(program) = std::env::var_os(PROGRAM).map(PathBuf::from)
        && program.is_absolute()
        && program.is_file()
    {
        return Ok(program);
    }
    Ok(exe)
}

/// `program` by `name`: its link in the names folder, made or made again
/// when it is not `program` as it is now; `program` itself when neither a
/// link nor a copy can be made.
pub fn named(program: &Path, name: &str) -> PathBuf {
    named_in(&dir(), program, name).unwrap_or_else(|_| program.to_path_buf())
}

/// Whether `program`'s name `name` is made and is `program` as it is now,
/// so [`named`] would make nothing.
pub fn current(program: &Path, name: &str) -> bool {
    std::fs::canonicalize(program).is_ok_and(|program| same(&dir().join(name), &program))
}

/// Where the name `name` is, made or not.
pub fn path(name: &str) -> PathBuf {
    dir().join(name)
}

/// The names folder: `~/.agent/sonny/programs/names`.
fn dir() -> PathBuf {
    std::env::var_os("HOME")
        .map(PathBuf::from)
        .unwrap_or_else(std::env::temp_dir)
        .join(".agent/sonny/programs/names")
}

fn named_in(dir: &Path, program: &Path, name: &str) -> std::io::Result<PathBuf> {
    use std::os::unix::fs::PermissionsExt;
    let program = std::fs::canonicalize(program)?;
    let link = dir.join(name);
    if same(&link, &program) {
        return Ok(link);
    }
    std::fs::create_dir_all(dir)?;
    std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o700))?;
    sweep(dir);
    let partial = |of: &str| dir.join(format!(".{of}.{}.partial", std::process::id()));
    let made = partial(name);
    let _ = std::fs::remove_file(&made);
    if std::fs::hard_link(&program, &made).is_err() {
        // One copy of each program, which its names all link to.
        let file = program
            .file_name()
            .ok_or(std::io::ErrorKind::InvalidInput)?;
        let copy = dir.join(format!(".copy-{}", file.to_string_lossy()));
        if !same(&copy, &program) {
            let copying = partial(&format!("copy-{}", file.to_string_lossy()));
            copy_as(&program, &copying)?;
            std::fs::rename(&copying, &copy)?;
        }
        std::fs::hard_link(&copy, &made)?;
    }
    // Moved there whole: a process started from the name before runs on
    // as it was.
    std::fs::rename(&made, &link)?;
    Ok(link)
}

/// Remove what a process left half made in `dir` when it ended between
/// making a name and moving it in place, `.NAME.PID.partial`: a copy is a
/// whole program. Only a process that is gone leaves one; this one's own
/// are its to finish. Read only when a name is made, so at most once a
/// program change.
fn sweep(dir: &Path) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    let me = std::process::id();
    for entry in entries.flatten() {
        let name = entry.file_name();
        let pid = (name.to_str())
            .and_then(|name| name.strip_prefix('.')?.strip_suffix(".partial"))
            .and_then(|made| made.rsplit_once('.')?.1.parse::<u32>().ok());
        if let Some(pid) = pid
            && pid != me
            && !alive(pid)
        {
            let _ = std::fs::remove_file(entry.path());
        }
    }
}

/// Whether process `pid` exists, as far as this user can tell.
fn alive(pid: u32) -> bool {
    let Ok(pid) = i32::try_from(pid) else {
        return false;
    };
    // SAFETY: signal 0 only checks that the process exists.
    pid > 0
        && (unsafe { libc::kill(pid, 0) } == 0
            || std::io::Error::last_os_error().raw_os_error() == Some(libc::EPERM))
}

/// `program` copied to `to`, runnable by its owner only, with `program`'s
/// time of writing, which [`same`] reads.
fn copy_as(program: &Path, to: &Path) -> std::io::Result<()> {
    use std::os::unix::fs::PermissionsExt;
    std::fs::copy(program, to)?;
    let written = std::fs::metadata(program)?.modified()?;
    let file = std::fs::OpenOptions::new().write(true).open(to)?;
    file.set_modified(written)?;
    std::fs::set_permissions(to, std::fs::Permissions::from_mode(0o700))
}

/// Whether `name` is `program` as it is now: the same file, or a copy of
/// it of the same length and time of writing.
fn same(name: &Path, program: &Path) -> bool {
    use std::os::unix::fs::MetadataExt;
    let (Ok(name), Ok(program)) = (std::fs::metadata(name), std::fs::metadata(program)) else {
        return false;
    };
    (name.dev(), name.ino()) == (program.dev(), program.ino())
        || (name.len() == program.len()
            && name.modified().ok().is_some()
            && name.modified().ok() == program.modified().ok())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::MetadataExt;

    fn temp() -> PathBuf {
        static TEMPS: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        let dir = std::env::temp_dir().join(format!(
            "sonny-named-{}-{}",
            std::process::id(),
            TEMPS.fetch_add(1, std::sync::atomic::Ordering::Relaxed),
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn program(dir: &Path, name: &str, text: &str) -> PathBuf {
        use std::os::unix::fs::PermissionsExt;
        let path = dir.join(name);
        std::fs::write(&path, text).unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
        path
    }

    #[test]
    fn a_name_is_its_program_and_is_made_again_only_when_the_program_changes() {
        let home = temp();
        let names = home.as_path().join("names");
        let sonny = program(home.as_path(), "sonny", "#!/bin/sh\necho one\n");
        let link = named_in(&names, &sonny, LOOKOUT).unwrap();
        assert_eq!(link, names.join(LOOKOUT));
        let (made, of) = (
            std::fs::metadata(&link).unwrap(),
            std::fs::metadata(&sonny).unwrap(),
        );
        assert_eq!((made.dev(), made.ino()), (of.dev(), of.ino()));
        // Asked again, nothing is made.
        named_in(&names, &sonny, LOOKOUT).unwrap();
        assert_eq!(std::fs::metadata(&link).unwrap().ino(), made.ino());
        // Two names of one program are one file.
        let host = named_in(&names, &sonny, TOOL_HOST).unwrap();
        assert_eq!(std::fs::metadata(&host).unwrap().ino(), made.ino());
        // An upgrade writes a new program in its place: the name follows,
        // and a process started from the old name keeps the old file.
        let held = std::fs::File::open(&link).unwrap();
        let upgraded = program(home.as_path(), ".sonny.new", "#!/bin/sh\necho two\n");
        std::fs::rename(&upgraded, &sonny).unwrap();
        named_in(&names, &sonny, LOOKOUT).unwrap();
        assert_eq!(
            std::fs::read_to_string(&link).unwrap(),
            "#!/bin/sh\necho two\n"
        );
        assert_eq!(held.metadata().unwrap().ino(), made.ino());
        // Nothing half-made is left behind.
        let left: Vec<_> = std::fs::read_dir(&names)
            .unwrap()
            .map(|entry| entry.unwrap().file_name().into_string().unwrap())
            .filter(|name| name.ends_with(".partial"))
            .collect();
        assert!(left.is_empty(), "{left:?}");
    }

    #[test]
    fn what_a_process_that_is_gone_left_half_made_goes_when_a_name_is_made() {
        let home = temp();
        let names = home.as_path().join("names");
        std::fs::create_dir_all(&names).unwrap();
        // A process that is gone: a child that has exited and been reaped.
        let mut gone = std::process::Command::new("true").spawn().unwrap();
        let gone = (gone.id(), gone.wait().unwrap()).0;
        let left = [
            format!(".{LOOKOUT}.{gone}.partial"),
            format!(".copy-sonny.{gone}.partial"),
        ];
        // One a running process is making, and a file that is no partial.
        let running = format!(".{TOOL_HOST}.1.partial");
        let kept = [running.as_str(), ".copy-sonny", "notes.partial"];
        for name in left.iter().map(String::as_str).chain(kept) {
            std::fs::write(names.join(name), "x").unwrap();
        }
        let sonny = program(home.as_path(), "sonny", "#!/bin/sh\necho one\n");
        named_in(&names, &sonny, LOOKOUT).unwrap();
        for name in &left {
            assert!(!names.join(name).exists(), "{name}");
        }
        for name in kept {
            assert!(names.join(name).exists(), "{name}");
        }
    }

    #[test]
    fn a_program_that_cannot_be_linked_is_named_by_one_copy() {
        let home = temp();
        let names = home.as_path().join("names");
        let sonny = program(home.as_path(), "sonny", "#!/bin/sh\necho one\n");
        // A copy made earlier, as for a program on another file system.
        std::fs::create_dir_all(&names).unwrap();
        copy_as(&sonny, &names.join(".copy-sonny")).unwrap();
        std::fs::hard_link(names.join(".copy-sonny"), names.join(LOOKOUT)).unwrap();
        assert!(same(&names.join(LOOKOUT), &sonny));
        // A changed program is no longer the copy.
        std::fs::write(&sonny, "#!/bin/sh\necho two, longer\n").unwrap();
        assert!(!same(&names.join(LOOKOUT), &sonny));
        named_in(&names, &sonny, LOOKOUT).unwrap();
        assert!(same(&names.join(LOOKOUT), &sonny));
    }

    #[test]
    fn a_name_that_cannot_be_made_leaves_the_program_as_it_is() {
        let home = temp();
        let sonny = program(home.as_path(), "sonny", "#!/bin/sh\n");
        // The names folder's place is a file: nothing can be made there.
        let blocked = home.as_path().join("names");
        std::fs::write(&blocked, "").unwrap();
        assert!(named_in(&blocked, &sonny, DAEMON).is_err());
        let missing = home.as_path().join("gone");
        assert!(named_in(&home.as_path().join("n"), &missing, DAEMON).is_err());
    }
}
