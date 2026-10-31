//! Where a daemon for a store is reached: the socket adjacent to the store,
//! or, when that path exceeds the Unix socket address limit, a stable short
//! rendezvous under /tmp. The CLI, the daemon and the app all resolve it
//! here, so a client never looks for a daemon where none could listen.
use crate::{Error, Result};
use std::{
    os::unix::{
        fs::{DirBuilderExt, MetadataExt},
        net::SocketAddr,
    },
    path::{Path, PathBuf},
};

/// The store's own path, with symlinks resolved, so every client finds
/// the same daemon and what sits beside its store.
pub fn canonical_store(store: &Path) -> Result<PathBuf> {
    let absolute = std::path::absolute(store)?;
    let mut existing = absolute.as_path();
    let mut suffix = Vec::new();
    loop {
        match existing.canonicalize() {
            Ok(mut path) => {
                for component in suffix.iter().rev() {
                    path.push(component);
                }
                return Ok(path);
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                suffix.push(
                    existing
                        .file_name()
                        .ok_or_else(|| std::io::Error::other("invalid store path"))?,
                );
                existing = existing
                    .parent()
                    .ok_or_else(|| std::io::Error::other("invalid store path"))?;
            }
            Err(error) => return Err(error.into()),
        }
    }
}

pub fn default_socket(store: &Path) -> Result<PathBuf> {
    let store = canonical_store(store)?;
    let mut adjacent = store.clone().into_os_string();
    adjacent.push(".sock");
    let adjacent = PathBuf::from(adjacent);
    if SocketAddr::from_pathname(&adjacent).is_ok() {
        return Ok(adjacent);
    }

    // Versioned FNV-1a-128 gives deterministic path names across processes and
    // Rust releases. This is a rendezvous identifier, not an authentication hash.
    let mut hash = 0x6c62272e07bb014262b821756295c58du128;
    for byte in store.as_os_str().as_encoded_bytes() {
        hash ^= u128::from(*byte);
        hash = hash.wrapping_mul(0x0000000001000000000000000000013b);
    }
    // Do not use TMPDIR: snapshot runners may set it to another long path.
    // SAFETY: geteuid takes no arguments and cannot fail.
    let uid = unsafe { libc::geteuid() };
    let directory = PathBuf::from(format!("/tmp/agent-{uid}"));
    match std::fs::DirBuilder::new().mode(0o700).create(&directory) {
        Ok(()) => {}
        Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {}
        Err(error) => return Err(error.into()),
    }
    let metadata = std::fs::symlink_metadata(&directory)?;
    if !metadata.is_dir() || metadata.uid() != uid || metadata.mode() & 0o077 != 0 {
        return Err(Error::new("unsafe_socket_directory"));
    }
    Ok(directory.join(format!("v1-{hash:032x}.sock")))
}

/// The variable naming the descriptor a sandboxed command asks its daemon
/// for connections on, where its sandbox lets it make no Unix socket.
pub const HANDED: &str = "AGENT_SOCKET_FD";

/// How long a command waits for the daemon to hand it a connection.
#[cfg(target_os = "linux")]
const HANDED_WAIT_MS: i32 = 5000;

/// A connection to the daemon at `socket` that it handed this command: in
/// a Linux sandbox, whose seccomp filter refuses a command a Unix socket
/// of its own, the daemon passes each command a descriptor (`HANDED`) on
/// which it answers every request with a connection to the bot's own
/// socket, as a broker hands a sandboxed renderer its channels. `None`
/// when this command has none, or `socket` is not its daemon's.
#[cfg(target_os = "linux")]
pub fn handed(socket: &Path) -> std::io::Result<Option<std::os::unix::net::UnixStream>> {
    use std::os::fd::FromRawFd;
    let Some(fd) = (std::env::var(HANDED).ok()).and_then(|fd| fd.parse::<i32>().ok()) else {
        return Ok(None);
    };
    let own = std::env::var_os("AGENT_SOCKET").map(PathBuf::from);
    let same = |path: &Path| std::path::absolute(path).ok();
    if own.as_deref().and_then(same) != same(socket) {
        return Ok(None);
    }
    if !handing(fd) {
        // A launcher that closes what it didn't open, as Python's
        // subprocess does, closed it: outside a sandbox the path still
        // serves, and inside one the command learns what went missing.
        return match std::os::unix::net::UnixStream::connect(socket) {
            Err(error) if error.raw_os_error() == Some(libc::EPERM) => {
                Err(std::io::Error::other(format!(
                    "the sandbox reaches the daemon only by descriptor {fd} ({HANDED}), \
                     which a launcher closed; keep it open, as Python's subprocess does \
                     with pass_fds"
                )))
            }
            connected => connected.map(Some),
        };
    }
    // SAFETY: the byte outlives the call.
    let sent = unsafe { libc::send(fd, [0u8].as_ptr().cast(), 1, libc::MSG_NOSIGNAL) };
    if sent < 0 {
        return Err(std::io::Error::last_os_error());
    }
    // Others in the same command ask on the same descriptor, and each
    // answer is as good as any other: whichever comes is this one's.
    let deadline =
        std::time::Instant::now() + std::time::Duration::from_millis(HANDED_WAIT_MS as u64);
    loop {
        let left = deadline.saturating_duration_since(std::time::Instant::now());
        let mut ready = libc::pollfd {
            fd,
            events: libc::POLLIN,
            revents: 0,
        };
        // SAFETY: one pollfd, for the call's length.
        let polled = unsafe { libc::poll(&mut ready, 1, left.as_millis() as i32) };
        if polled < 0 {
            let error = std::io::Error::last_os_error();
            if error.kind() == std::io::ErrorKind::Interrupted {
                continue;
            }
            return Err(error);
        }
        if polled == 0 {
            return Err(std::io::ErrorKind::TimedOut.into());
        }
        match received(fd)? {
            Received::Stream(stream) => {
                // SAFETY: the descriptor came over SCM_RIGHTS and is ours.
                return Ok(Some(unsafe {
                    std::os::unix::net::UnixStream::from_raw_fd(stream)
                }));
            }
            Received::Refused => return Err(std::io::ErrorKind::ConnectionRefused.into()),
            Received::Closed => return Err(std::io::ErrorKind::NotConnected.into()),
            Received::Later => continue,
        }
    }
}

/// Whether `fd` is still the descriptor the daemon hands connections on:
/// a Unix socket of packets. Once closed, its number may name anything.
#[cfg(target_os = "linux")]
fn handing(fd: i32) -> bool {
    let option = |name| {
        let mut value: libc::c_int = 0;
        let mut length = std::mem::size_of::<libc::c_int>() as libc::socklen_t;
        // SAFETY: an int and its length, for the call's length.
        let got = unsafe {
            libc::getsockopt(
                fd,
                libc::SOL_SOCKET,
                name,
                (&mut value as *mut libc::c_int).cast(),
                &mut length,
            )
        };
        (got == 0).then_some(value)
    };
    option(libc::SO_DOMAIN) == Some(libc::AF_UNIX)
        && option(libc::SO_TYPE) == Some(libc::SOCK_SEQPACKET)
}

/// Elsewhere a sandbox reaches the bot's own socket by its path.
#[cfg(not(target_os = "linux"))]
pub fn handed(_socket: &Path) -> std::io::Result<Option<std::os::unix::net::UnixStream>> {
    Ok(None)
}

#[cfg(target_os = "linux")]
enum Received {
    Stream(i32),
    /// An answer without a connection: the daemon could not open one.
    Refused,
    Closed,
    /// Another asker took the answer first.
    Later,
}

/// One answer on `fd`, without waiting.
#[cfg(target_os = "linux")]
fn received(fd: i32) -> std::io::Result<Received> {
    let mut byte = [0u8; 1];
    let mut iov = libc::iovec {
        iov_base: byte.as_mut_ptr().cast(),
        iov_len: 1,
    };
    // Room for one descriptor's control message, aligned for its header.
    let mut control = [0u64; 8];
    // SAFETY: a zeroed msghdr is valid; its pointers are set below.
    let mut message: libc::msghdr = unsafe { std::mem::zeroed() };
    message.msg_iov = &mut iov;
    message.msg_iovlen = 1;
    message.msg_control = control.as_mut_ptr().cast();
    message.msg_controllen = std::mem::size_of_val(&control) as _;
    // SAFETY: every buffer the header names outlives the call.
    let read = unsafe {
        libc::recvmsg(
            fd,
            &mut message,
            libc::MSG_DONTWAIT | libc::MSG_CMSG_CLOEXEC,
        )
    };
    if read < 0 {
        let error = std::io::Error::last_os_error();
        return match error.kind() {
            std::io::ErrorKind::WouldBlock | std::io::ErrorKind::Interrupted => Ok(Received::Later),
            _ => Err(error),
        };
    }
    if read == 0 {
        return Ok(Received::Closed);
    }
    // SAFETY: the header was filled by recvmsg, its control within `control`.
    unsafe {
        let header = libc::CMSG_FIRSTHDR(&message);
        if !header.is_null()
            && (*header).cmsg_level == libc::SOL_SOCKET
            && (*header).cmsg_type == libc::SCM_RIGHTS
        {
            let stream = std::ptr::read_unaligned(libc::CMSG_DATA(header).cast::<i32>());
            return Ok(Received::Stream(stream));
        }
    }
    Ok(Received::Refused)
}

/// Answer one request on `fd` with `stream`, or, with none, an answer that
/// says the daemon could not open one.
#[cfg(target_os = "linux")]
pub fn hand(fd: i32, stream: Option<i32>) -> std::io::Result<()> {
    let mut byte = [0u8; 1];
    let mut iov = libc::iovec {
        iov_base: byte.as_mut_ptr().cast(),
        iov_len: 1,
    };
    let mut control = [0u64; 8];
    // SAFETY: a zeroed msghdr is valid; its pointers are set below.
    let mut message: libc::msghdr = unsafe { std::mem::zeroed() };
    message.msg_iov = &mut iov;
    message.msg_iovlen = 1;
    if let Some(stream) = stream {
        // SAFETY: the control buffer holds one header and one descriptor.
        unsafe {
            message.msg_control = control.as_mut_ptr().cast();
            message.msg_controllen = libc::CMSG_SPACE(std::mem::size_of::<i32>() as u32) as _;
            let header = libc::CMSG_FIRSTHDR(&message);
            (*header).cmsg_level = libc::SOL_SOCKET;
            (*header).cmsg_type = libc::SCM_RIGHTS;
            (*header).cmsg_len = libc::CMSG_LEN(std::mem::size_of::<i32>() as u32) as _;
            std::ptr::write_unaligned(libc::CMSG_DATA(header).cast::<i32>(), stream);
        }
    }
    // SAFETY: every buffer the header names outlives the call.
    let sent = unsafe { libc::sendmsg(fd, &message, libc::MSG_NOSIGNAL | libc::MSG_DONTWAIT) };
    if sent < 0 {
        return Err(std::io::Error::last_os_error());
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn long_paths_and_aliases_have_a_stable_short_rendezvous() {
        let root = std::env::temp_dir().join(format!("agent-path-test-{}", std::process::id()));
        std::fs::create_dir_all(&root).unwrap();
        let long = root.join("nested".repeat(25)).join("state.sqlite");
        let before = default_socket(&long).unwrap();
        assert!(SocketAddr::from_pathname(&before).is_ok());
        std::fs::create_dir_all(long.parent().unwrap()).unwrap();
        std::fs::write(&long, b"").unwrap();
        assert_eq!(before, default_socket(&long).unwrap());
        let alias = root.join("alias");
        std::os::unix::fs::symlink(long.parent().unwrap(), &alias).unwrap();
        assert_eq!(before, default_socket(&alias.join("state.sqlite")).unwrap());
        assert_ne!(
            before,
            default_socket(&long.with_file_name("other.sqlite")).unwrap()
        );
        let short = root.join("short.sqlite");
        assert_eq!(
            default_socket(&short).unwrap(),
            root.canonicalize().unwrap().join("short.sqlite.sock")
        );
        std::fs::remove_dir_all(root).unwrap();
    }
}
