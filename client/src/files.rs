//! Files clients write whole.
use std::{
    io::Write,
    path::{Path, PathBuf},
};

/// Open a new sibling for this write alone. Concurrent callers and files
/// left by interrupted processes never share a staging file.
pub(crate) fn temporary(path: &Path, mode: u32) -> std::io::Result<(PathBuf, std::fs::File)> {
    use std::os::unix::fs::OpenOptionsExt;
    use std::sync::atomic::{AtomicU64, Ordering};
    static NEXT: AtomicU64 = AtomicU64::new(0);

    let mut name = std::ffi::OsString::from(".");
    name.push(path.file_name().ok_or(std::io::ErrorKind::InvalidInput)?);
    name.push(format!(".{}", std::process::id()));
    let mut staged = path.with_file_name(&name);
    loop {
        match std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .custom_flags(libc::O_NOFOLLOW)
            .mode(mode)
            .open(&staged)
        {
            Ok(file) => return Ok((staged, file)),
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {
                let mut next = name.clone();
                next.push(format!(".{}", NEXT.fetch_add(1, Ordering::Relaxed)));
                staged.set_file_name(next);
            }
            Err(error) => return Err(error),
        }
    }
}

/// Write a file whole beside its place, then rename it there, with its mode
/// set from creation, so the new name never has any other.
pub fn replace_mode(path: &Path, text: &[u8], mode: u32) -> Result<(), String> {
    let dir = path.parent().ok_or("no folder")?;
    let dir = if dir.as_os_str().is_empty() {
        Path::new(".")
    } else {
        dir
    };
    std::fs::create_dir_all(dir).map_err(|e| format!("{}: {e}", dir.display()))?;
    let (temporary, mut file) =
        temporary(path, mode).map_err(|e| format!("{}: {e}", path.display()))?;
    let written = (|| {
        // Past the umask, on the file opened, not on its name.
        file.set_permissions(std::os::unix::fs::PermissionsExt::from_mode(mode))?;
        file.write_all(text)?;
        file.sync_all()?;
        std::fs::rename(&temporary, path)
    })();
    if written.is_err() {
        let _ = std::fs::remove_file(&temporary);
    }
    // Once renamed, another writer can own the staging name, even if
    // syncing the directory fails.
    written
        .and_then(|()| std::fs::File::open(dir)?.sync_all())
        .map_err(|e| format!("{}: {e}", path.display()))
}

/// `path` with what exists of it resolved to its real path and the rest
/// appended; `None` when the rest climbs out with `..`, which can't be
/// known without making it. A symlink whose target is missing is followed
/// to that target, as a write through it would be. The daemon keeps a
/// sandbox's paths so, and a client that compares them does the same.
pub fn real(path: &Path) -> Option<PathBuf> {
    real_within(path, 40)
}

fn real_within(path: &Path, links: u32) -> Option<PathBuf> {
    let mut existing = path.to_owned();
    let mut rest = Vec::new();
    loop {
        let found = std::fs::canonicalize(&existing).ok().or_else(|| {
            // canonicalize fails on a dangling link as on a missing name.
            let link = std::fs::read_link(&existing).ok()?;
            let mut target = existing.parent()?.join(link);
            for part in rest.iter().rev() {
                target.push(part);
            }
            rest.clear();
            real_within(&target, links.checked_sub(1)?)
        });
        if let Some(mut resolved) = found {
            for part in rest.iter().rev() {
                resolved.push(part);
            }
            return Some(resolved);
        }
        if std::fs::symlink_metadata(&existing).is_ok_and(|found| found.file_type().is_symlink()) {
            return None;
        }
        let name = existing.file_name()?.to_owned();
        if !matches!(
            Path::new(&name).components().next(),
            Some(std::path::Component::Normal(_))
        ) {
            return None;
        }
        rest.push(name);
        existing = existing.parent()?.to_owned();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn concurrent_replacements_each_publish_a_whole_file() {
        let dir =
            std::env::temp_dir().join(format!("agent-replace-threads-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("focus");
        let start = std::sync::Barrier::new(8);
        std::thread::scope(|scope| {
            for byte in 0..8 {
                let (path, start) = (&path, &start);
                scope.spawn(move || {
                    start.wait();
                    replace_mode(path, &[byte; 8192], 0o600).unwrap();
                });
            }
        });
        let content = std::fs::read(path).unwrap();
        assert_eq!(content.len(), 8192);
        assert!(content.iter().all(|&byte| byte == content[0]));
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn a_link_left_by_the_temporary_s_name_is_never_written_through() {
        let dir = std::env::temp_dir().join(format!("agent-replace-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let outside = dir.join("outside");
        std::fs::write(&outside, "kept").unwrap();
        let path = dir.join("focus");
        let temporary = dir.join(format!(".focus.{}", std::process::id()));
        std::os::unix::fs::symlink(&outside, &temporary).unwrap();
        replace_mode(&path, b"the tests", 0o600).unwrap();
        assert_eq!(std::fs::read_to_string(&outside).unwrap(), "kept");
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "the tests");
        use std::os::unix::fs::PermissionsExt;
        let mode = std::fs::metadata(&path).unwrap().permissions().mode();
        assert_eq!(mode & 0o777, 0o600);
        std::fs::remove_dir_all(dir).unwrap();
    }
}
