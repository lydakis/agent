//! The follows folder (`STORE.lookout/`), watched with the kernel: inotify
//! on Linux, kqueue on macOS, so a written follow is read the moment it is
//! written and a quiet folder costs nothing (no timer looks at it). The
//! daemon's watch of its config (`src/server/file_watch.rs`) works the same
//! way; this is Sonny's own copy, for a folder rather than one file.
//!
//! A notice says only which files may have changed; the reader compares
//! each file's stamp with what it read last. Any number of changes before
//! the lookout looks are one notice.
//!
//! The kernel queues a notice in the write itself (the rename that puts a
//! follow in place), before the writer goes on: a write that ended before
//! the daemon sent an event is in the queue when the lookout takes that
//! event, so [`Watch::take`] before each event makes the change live
//! before the event is handled.
//!
//! On macOS the folder's watch says only that an entry came, went or was
//! renamed over, never which, and a file edited in place (`>>` by hand) is
//! not told at all: every Sonny writer writes a follow whole and renames
//! it into place, which is told.
use std::os::fd::{AsRawFd, OwnedFd};
use std::path::Path;
use std::sync::Arc;
use tokio::io::unix::AsyncFd;

/// What changed in the folder since the last look.
#[derive(Debug, PartialEq, Eq)]
pub(super) enum Changed {
    /// Nothing.
    None,
    /// These subscribers' files, by subscriber name.
    Files(std::collections::HashSet<String>),
    /// Any of them: the kernel did not say which (macOS, or a queue that
    /// overflowed), so every file is looked at.
    Any,
    /// The watch ended (the folder was removed or moved): every file is
    /// looked at, and the lookout goes back to its timer.
    Ended,
}

/// The folder's watch: a descriptor the kernel makes readable on a change.
pub(super) struct Watch {
    fd: Arc<AsyncFd<OwnedFd>>,
    /// The macOS watch needs the folder open as long as it watches.
    #[cfg(target_os = "macos")]
    _folder: OwnedFd,
}

impl Watch {
    /// The folder watched, or why not: a platform with no watch, or a
    /// folder that cannot be watched. Must run on the runtime.
    pub(super) fn new(folder: &Path) -> std::io::Result<Watch> {
        imp::new(folder)
    }

    /// What the select loop waits on: readable once a change is queued.
    pub(super) fn ready(&self) -> Arc<AsyncFd<OwnedFd>> {
        self.fd.clone()
    }

    /// Takes every notice queued without waiting.
    pub(super) fn take(&self) -> Changed {
        imp::take(self)
    }
}

/// The subscriber a folder entry's name is the file of: `NAME.json`, never
/// a partial write (`NAME.json.PID.partial`) or the lock.
fn subscriber(name: &[u8]) -> Option<String> {
    let name = std::str::from_utf8(name).ok()?;
    let bot = name.strip_suffix(".json")?;
    (!bot.is_empty() && !bot.starts_with('.')).then(|| bot.to_owned())
}

fn c_path(path: &Path) -> std::io::Result<std::ffi::CString> {
    use std::os::unix::ffi::OsStrExt;
    std::ffi::CString::new(path.as_os_str().as_bytes())
        .map_err(|_| std::io::Error::from(std::io::ErrorKind::InvalidInput))
}

#[cfg(target_os = "linux")]
mod imp {
    use super::{AsRawFd, AsyncFd, Changed, OwnedFd, Watch, c_path, subscriber};
    use std::os::fd::FromRawFd;
    use std::path::Path;
    use std::sync::Arc;

    /// A file written and closed, or an entry moved in, moved out or
    /// removed; and the folder itself going. Never a plain modify, so a
    /// write is told once, when it ends.
    const MASK: u32 = libc::IN_CLOSE_WRITE
        | libc::IN_MOVED_TO
        | libc::IN_MOVED_FROM
        | libc::IN_DELETE
        | libc::IN_DELETE_SELF
        | libc::IN_MOVE_SELF
        | libc::IN_ONLYDIR;

    pub(super) fn new(folder: &Path) -> std::io::Result<Watch> {
        // SAFETY: inotify_init1 takes flags only and returns a new fd or -1.
        let fd = unsafe { libc::inotify_init1(libc::IN_NONBLOCK | libc::IN_CLOEXEC) };
        if fd < 0 {
            return Err(std::io::Error::last_os_error());
        }
        // SAFETY: fd was just returned to this process and is owned here.
        let fd = unsafe { OwnedFd::from_raw_fd(fd) };
        let folder = c_path(folder)?;
        // SAFETY: fd is a live inotify fd and folder a NUL-terminated path.
        if unsafe { libc::inotify_add_watch(fd.as_raw_fd(), folder.as_ptr(), MASK) } < 0 {
            return Err(std::io::Error::last_os_error());
        }
        // SAFETY: the OwnedFd is open, and it keeps the one descriptor
        // until the AsyncFd that owns it drops.
        let fd = unsafe { AsyncFd::register_with_interest(fd, tokio::io::Interest::READABLE) }
            .map_err(std::io::Error::from)?;
        Ok(Watch { fd: Arc::new(fd) })
    }

    pub(super) fn take(watch: &Watch) -> Changed {
        // Room for many events at once; each is a header and a name.
        let mut buffer = [0u8; 16 * 1024];
        let mut files = std::collections::HashSet::new();
        let mut any = false;
        loop {
            // SAFETY: buffer is writable for its length.
            let read = unsafe {
                libc::read(
                    watch.fd.get_ref().as_raw_fd(),
                    buffer.as_mut_ptr().cast(),
                    buffer.len(),
                )
            };
            if read < 0 {
                let error = std::io::Error::last_os_error();
                if error.kind() == std::io::ErrorKind::WouldBlock {
                    break;
                }
                if error.kind() == std::io::ErrorKind::Interrupted {
                    continue;
                }
                return Changed::Ended;
            }
            let read = read as usize;
            let mut at = 0;
            const HEADER: usize = std::mem::size_of::<libc::inotify_event>();
            while at + HEADER <= read {
                // SAFETY: the kernel wrote a whole event at `at`; it may be
                // unaligned, so it is read unaligned.
                let event: libc::inotify_event =
                    unsafe { std::ptr::read_unaligned(buffer[at..].as_ptr().cast()) };
                let end = (at + HEADER + event.len as usize).min(read);
                let named = &buffer[at + HEADER..end];
                let named = &named[..named.iter().position(|b| *b == 0).unwrap_or(named.len())];
                at = end;
                let gone = libc::IN_IGNORED | libc::IN_DELETE_SELF | libc::IN_MOVE_SELF;
                if event.mask & gone != 0 {
                    return Changed::Ended;
                }
                if event.mask & libc::IN_Q_OVERFLOW != 0 {
                    any = true;
                    continue;
                }
                if let Some(bot) = subscriber(named) {
                    files.insert(bot);
                }
            }
        }
        match (any, files.is_empty()) {
            (true, _) => Changed::Any,
            (false, true) => Changed::None,
            (false, false) => Changed::Files(files),
        }
    }
}

#[cfg(target_os = "macos")]
mod imp {
    use super::{AsRawFd, AsyncFd, Changed, OwnedFd, Watch, c_path};
    use std::os::fd::FromRawFd;
    use std::path::Path;
    use std::sync::Arc;

    pub(super) fn new(folder: &Path) -> std::io::Result<Watch> {
        let path = c_path(folder)?;
        // SAFETY: path is NUL-terminated; O_EVTONLY opens for events only.
        let opened = unsafe { libc::open(path.as_ptr(), libc::O_EVTONLY | libc::O_CLOEXEC) };
        if opened < 0 {
            return Err(std::io::Error::last_os_error());
        }
        // SAFETY: opened was just returned to this process and is owned here.
        let opened = unsafe { OwnedFd::from_raw_fd(opened) };
        // SAFETY: kqueue takes no arguments and returns a new fd or -1.
        let queue = unsafe { libc::kqueue() };
        if queue < 0 {
            return Err(std::io::Error::last_os_error());
        }
        // SAFETY: queue was just returned to this process and is owned here.
        let queue = unsafe { OwnedFd::from_raw_fd(queue) };
        // SAFETY: fcntl on a live fd this process owns.
        unsafe {
            let flags = libc::fcntl(queue.as_raw_fd(), libc::F_GETFD);
            libc::fcntl(queue.as_raw_fd(), libc::F_SETFD, flags | libc::FD_CLOEXEC);
            let flags = libc::fcntl(queue.as_raw_fd(), libc::F_GETFL);
            libc::fcntl(queue.as_raw_fd(), libc::F_SETFL, flags | libc::O_NONBLOCK);
        }
        // SAFETY: an all-zero kevent is valid; the fields set below follow.
        let mut change: libc::kevent = unsafe { std::mem::zeroed() };
        change.ident = opened.as_raw_fd() as libc::uintptr_t;
        change.filter = libc::EVFILT_VNODE;
        change.flags = libc::EV_ADD | libc::EV_CLEAR;
        // An entry came, went or was renamed over (the folder's write);
        // the folder itself removed or moved.
        change.fflags =
            libc::NOTE_WRITE | libc::NOTE_DELETE | libc::NOTE_RENAME | libc::NOTE_REVOKE;
        // SAFETY: one change in, no events out, on a live kqueue.
        let added = unsafe {
            libc::kevent(
                queue.as_raw_fd(),
                &change,
                1,
                std::ptr::null_mut(),
                0,
                std::ptr::null(),
            )
        };
        if added != 0 {
            return Err(std::io::Error::last_os_error());
        }
        // Readable only: a kqueue has no write side, and asking for one
        // fails the registration.
        // SAFETY: the OwnedFd is open, and it keeps the one descriptor
        // until the AsyncFd that owns it drops.
        let fd = unsafe { AsyncFd::register_with_interest(queue, tokio::io::Interest::READABLE) }
            .map_err(std::io::Error::from)?;
        Ok(Watch {
            fd: Arc::new(fd),
            _folder: opened,
        })
    }

    pub(super) fn take(watch: &Watch) -> Changed {
        // SAFETY: an all-zero kevent array is valid output space.
        let mut events: [libc::kevent; 8] = unsafe { std::mem::zeroed() };
        let none = libc::timespec {
            tv_sec: 0,
            tv_nsec: 0,
        };
        let mut changed = Changed::None;
        loop {
            // SAFETY: no changes in, up to 8 events out, no wait.
            let count = unsafe {
                libc::kevent(
                    watch.fd.get_ref().as_raw_fd(),
                    std::ptr::null(),
                    0,
                    events.as_mut_ptr(),
                    events.len() as libc::c_int,
                    &none,
                )
            };
            match count {
                ..0 if std::io::Error::last_os_error().kind()
                    == std::io::ErrorKind::Interrupted =>
                {
                    continue;
                }
                ..0 => return Changed::Ended,
                0 => return changed,
                _ => {}
            }
            let gone = libc::NOTE_DELETE | libc::NOTE_RENAME | libc::NOTE_REVOKE;
            if events[..count as usize]
                .iter()
                .any(|e| e.fflags & gone != 0)
            {
                return Changed::Ended;
            }
            changed = Changed::Any;
        }
    }
}

#[cfg(not(any(target_os = "linux", target_os = "macos")))]
mod imp {
    use super::{Changed, Watch};
    use std::path::Path;

    pub(super) fn new(_: &Path) -> std::io::Result<Watch> {
        Err(std::io::Error::from(std::io::ErrorKind::Unsupported))
    }

    pub(super) fn take(_: &Watch) -> Changed {
        Changed::None
    }
}

#[cfg(test)]
mod tests {
    use super::{Changed, Watch};
    use std::time::Duration;

    fn folder(name: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!("follows-watch-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    /// What the watch says once it is readable, or within a second.
    async fn told(watch: &Watch) -> Changed {
        let ready = watch.ready();
        if let Ok(Ok(mut guard)) =
            tokio::time::timeout(Duration::from_secs(1), ready.readable()).await
        {
            guard.clear_ready();
        }
        watch.take()
    }

    fn named(names: &[&str]) -> Changed {
        Changed::Files(names.iter().map(|n| n.to_string()).collect())
    }

    #[tokio::test]
    async fn a_follow_renamed_into_place_is_told_at_once_and_partials_are_not() {
        let dir = folder("rename");
        let watch = Watch::new(&dir).unwrap();
        assert_eq!(watch.take(), Changed::None);
        // As every Sonny writer writes: whole, then renamed over.
        std::fs::write(dir.join("p.lead.json.7.partial"), "{}").unwrap();
        std::fs::rename(dir.join("p.lead.json.7.partial"), dir.join("p.lead.json")).unwrap();
        // Queued in the rename itself: no wait.
        let said = watch.take();
        if cfg!(target_os = "linux") {
            assert_eq!(said, named(&["p.lead"]));
        } else {
            assert_eq!(said, Changed::Any);
        }
        // Many writes before a look are one notice.
        for bot in ["a", "b", "a"] {
            std::fs::write(dir.join(format!("{bot}.json.1.partial")), "{}").unwrap();
            std::fs::rename(
                dir.join(format!("{bot}.json.1.partial")),
                dir.join(format!("{bot}.json")),
            )
            .unwrap();
        }
        let said = told(&watch).await;
        if cfg!(target_os = "linux") {
            assert_eq!(said, named(&["a", "b"]));
        } else {
            assert_eq!(said, Changed::Any);
        }
        assert_eq!(watch.take(), Changed::None);
        // The counts folder inside is the lookout's own: not told.
        std::fs::create_dir_all(dir.join(".counts")).unwrap();
        let _ = watch.take();
        std::fs::write(dir.join(".counts").join("a.json"), "{}").unwrap();
        assert_eq!(watch.take(), Changed::None);
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[tokio::test]
    async fn a_removed_follow_file_is_told() {
        let dir = folder("removed");
        std::fs::write(dir.join("x.json"), "{}").unwrap();
        let watch = Watch::new(&dir).unwrap();
        std::fs::remove_file(dir.join("x.json")).unwrap();
        let said = watch.take();
        if cfg!(target_os = "linux") {
            assert_eq!(said, named(&["x"]));
        } else {
            assert_eq!(said, Changed::Any);
        }
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[cfg(target_os = "linux")]
    #[tokio::test]
    async fn an_edit_in_place_is_told_on_linux() {
        let dir = folder("in-place");
        std::fs::write(dir.join("x.json"), "{}").unwrap();
        let watch = Watch::new(&dir).unwrap();
        use std::io::Write;
        let mut file = std::fs::OpenOptions::new()
            .append(true)
            .open(dir.join("x.json"))
            .unwrap();
        file.write_all(b"\n").unwrap();
        drop(file);
        assert_eq!(watch.take(), named(&["x"]));
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[tokio::test]
    async fn the_folder_going_ends_the_watch() {
        let dir = folder("gone");
        let watch = Watch::new(&dir).unwrap();
        std::fs::remove_dir_all(&dir).unwrap();
        assert_eq!(told(&watch).await, Changed::Ended);
    }
}
