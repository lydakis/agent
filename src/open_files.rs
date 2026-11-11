//! How many files the daemon may hold open. Every session, bot socket,
//! pipe, terminal and store connection is one, so a daemon started where
//! the soft limit is low (a systemd unit's 1024, a Mac's 256) runs out
//! long before the system would let it. At start the daemon raises its
//! soft limit to the hard one, as Go's runtime, Node and the JVM do.
//! Commands it starts inherit the raised limit: handing them the old one
//! back takes a hook between fork and exec, which costs every command
//! posix_spawn's speed, and posix_spawn has no way to set a limit.

/// Raise this process's soft limit on open files to its hard limit (on
/// macOS, to the most one process may open, which a higher soft limit is
/// refused over). Called once, before any thread starts.
pub fn raise() {
    // SAFETY: a zeroed rlimit is valid; getrlimit fills it.
    let mut limit: libc::rlimit = unsafe { std::mem::zeroed() };
    // SAFETY: a valid pointer to an rlimit this function owns.
    if unsafe { libc::getrlimit(libc::RLIMIT_NOFILE, &mut limit) } != 0 {
        return;
    }
    let most = most(limit.rlim_max);
    if limit.rlim_cur >= most {
        return;
    }
    let raised = libc::rlimit {
        rlim_cur: most,
        rlim_max: limit.rlim_max,
    };
    // SAFETY: a valid pointer to an rlimit this function owns.
    unsafe { libc::setrlimit(libc::RLIMIT_NOFILE, &raised) };
}

#[cfg(target_os = "macos")]
fn most(hard: libc::rlim_t) -> libc::rlim_t {
    let mut most: libc::c_int = 0;
    let mut size = std::mem::size_of::<libc::c_int>();
    // SAFETY: the name is NUL-terminated and `most` is an int of `size` bytes.
    let read = unsafe {
        libc::sysctlbyname(
            c"kern.maxfilesperproc".as_ptr(),
            (&mut most as *mut libc::c_int).cast(),
            &mut size,
            std::ptr::null_mut(),
            0,
        )
    };
    match read == 0 && most > 0 {
        true => hard.min(most as libc::rlim_t),
        false => hard.min(libc::OPEN_MAX as libc::rlim_t),
    }
}

#[cfg(not(target_os = "macos"))]
fn most(hard: libc::rlim_t) -> libc::rlim_t {
    hard
}

/// The open files of this process and its soft limit, for `stats`.
pub fn count() -> (Option<usize>, Option<u64>) {
    #[cfg(target_os = "linux")]
    const OPEN: &str = "/proc/self/fd";
    #[cfg(not(target_os = "linux"))]
    const OPEN: &str = "/dev/fd";
    // Less the descriptor reading the folder holds while it does.
    let open = std::fs::read_dir(OPEN)
        .ok()
        .map(|entries| entries.count().saturating_sub(1));
    // SAFETY: a zeroed rlimit is valid; getrlimit fills it.
    let mut limit: libc::rlimit = unsafe { std::mem::zeroed() };
    // SAFETY: a valid pointer to an rlimit this function owns.
    let soft = (unsafe { libc::getrlimit(libc::RLIMIT_NOFILE, &mut limit) } == 0)
        .then_some(limit.rlim_cur as u64);
    (open, soft)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_count_reads_this_process_s_files_and_limit() {
        let (open, soft) = count();
        let (open, soft) = (open.unwrap(), soft.unwrap());
        // At least stdin, stdout and stderr, within the limit.
        assert!(open >= 3 && soft as usize >= open, "{open} of {soft}");
    }
}
