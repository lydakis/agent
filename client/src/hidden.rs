//! The folders kept from agents: no sandboxed command reads, writes, links
//! or renames what is in them, and nothing read for an agent outside its
//! sandbox (the daemon's `read` tool, a client's template `file()`) gives
//! it back. `~/.agent/keys`, the keys the CLI starts the daemon with, is
//! always one; the person adds more as `[sandbox] unreadable`.
use std::path::PathBuf;

/// `~/.agent/keys` and the folders `config` lists, as named: the agent CLI
/// starts the daemon with each as `--unreadable`, which makes the missing.
pub fn folders(config: &crate::config::Config) -> Vec<PathBuf> {
    (crate::env_file::folder().into_iter())
        .chain(crate::config::unreadable(config))
        .collect()
}

/// Whether the file open as `file` is, or is in, one of `hidden`, known by
/// their real paths. The file opened is judged, not the path it was opened
/// by, so no link, swapped or not, reaches one; a file whose place the
/// system can't say counts as in one.
pub fn holds(hidden: &[PathBuf], file: &impl std::os::fd::AsRawFd) -> bool {
    !hidden.is_empty()
        && opened(file).is_none_or(|opened| hidden.iter().any(|folder| opened.starts_with(folder)))
}

/// The path the kernel holds for an open file: where it is, whatever
/// links led to it or were swapped since. `None` where the system can't
/// say.
#[cfg(target_os = "linux")]
pub fn opened(file: &impl std::os::fd::AsRawFd) -> Option<PathBuf> {
    std::fs::read_link(format!("/proc/self/fd/{}", file.as_raw_fd())).ok()
}

#[cfg(target_os = "macos")]
pub fn opened(file: &impl std::os::fd::AsRawFd) -> Option<PathBuf> {
    use std::os::unix::ffi::OsStrExt;
    let mut path = [0u8; libc::PATH_MAX as usize];
    // SAFETY: F_GETPATH writes at most MAXPATHLEN (PATH_MAX) bytes,
    // NUL-terminated, into the buffer.
    if unsafe { libc::fcntl(file.as_raw_fd(), libc::F_GETPATH, path.as_mut_ptr()) } == -1 {
        return None;
    }
    let end = path.iter().position(|byte| *byte == 0)?;
    Some(PathBuf::from(std::ffi::OsStr::from_bytes(&path[..end])))
}

#[cfg(not(any(target_os = "linux", target_os = "macos")))]
pub fn opened(_: &impl std::os::fd::AsRawFd) -> Option<PathBuf> {
    None
}
