//! A file the kernel tells the daemon about when it may have changed:
//! inotify on Linux and kqueue on macOS, so a watch costs nothing between
//! changes (no polling). The folder is watched, not only the file, since an
//! editor or a settings writer usually replaces a file by renaming another
//! over it. A notice says only that the file may have changed; the reader
//! compares what it reads now with what it read last.
use std::path::{Path, PathBuf};
use tokio::sync::mpsc;

/// A receiver told, at most one notice pending, each time `path` may have
/// changed. None where the platform has no watch or the folder cannot be
/// watched; the file is then read at start only.
pub async fn watch(path: &Path) -> Option<mpsc::Receiver<()>> {
    // What touches the filesystem runs off the runtime thread; only the
    // descriptor's registration with the runtime stays on it.
    let given = path.to_owned();
    let prepared = tokio::task::spawn_blocking(move || imp::prepare(&given))
        .await
        .ok()?
        .ok()?;
    let (sender, receiver) = mpsc::channel(1);
    imp::spawn(path.to_owned(), prepared, sender).ok()?;
    Some(receiver)
}

/// Tells the receiver without waiting: one pending notice covers any number
/// of changes, since each reads the file whole.
fn notify(sender: &mpsc::Sender<()>) -> bool {
    !matches!(
        sender.try_send(()),
        Err(mpsc::error::TrySendError::Closed(_))
    )
}

fn split(path: &Path) -> std::io::Result<(PathBuf, std::ffi::OsString)> {
    match (path.parent(), path.file_name()) {
        (Some(folder), Some(name)) if !folder.as_os_str().is_empty() => {
            Ok((folder.to_owned(), name.to_owned()))
        }
        _ => Err(std::io::Error::from(std::io::ErrorKind::InvalidInput)),
    }
}

fn c_path(path: &Path) -> std::io::Result<std::ffi::CString> {
    use std::os::unix::ffi::OsStrExt;
    std::ffi::CString::new(path.as_os_str().as_bytes())
        .map_err(|_| std::io::Error::from(std::io::ErrorKind::InvalidInput))
}

#[cfg(target_os = "linux")]
mod imp {
    use super::{c_path, notify, split};
    use std::collections::HashMap;
    use std::ffi::OsString;
    use std::os::fd::{AsRawFd, FromRawFd, OwnedFd, RawFd};
    use std::path::{Path, PathBuf};
    use tokio::{io::unix::AsyncFd, sync::mpsc};

    /// A write closed, or an entry made, moved or removed in a folder:
    /// never a plain modify, so a log appended beside it costs no wakeup.
    const MASK: u32 = libc::IN_CLOSE_WRITE
        | libc::IN_MOVED_TO
        | libc::IN_MOVED_FROM
        | libc::IN_CREATE
        | libc::IN_DELETE;

    /// Watches the folder `path` is in for entries named as it is: the
    /// watch descriptor and the name.
    fn add(fd: RawFd, path: &Path) -> std::io::Result<(i32, OsString)> {
        let (folder, name) = split(path)?;
        let folder = c_path(&folder)?;
        // SAFETY: fd is a live inotify fd and folder a NUL-terminated path.
        let wd = unsafe { libc::inotify_add_watch(fd, folder.as_ptr(), MASK) };
        if wd < 0 {
            return Err(std::io::Error::last_os_error());
        }
        Ok((wd, name))
    }

    /// The descriptor and its watches, made off the runtime thread: adding
    /// a watch resolves a path.
    pub struct Prepared {
        fd: OwnedFd,
        names: HashMap<i32, Vec<OsString>>,
        own: i32,
        target: Option<PathBuf>,
    }

    pub fn prepare(path: &Path) -> std::io::Result<Prepared> {
        // SAFETY: inotify_init1 takes flags only and returns a new fd or -1.
        let fd = unsafe { libc::inotify_init1(libc::IN_NONBLOCK | libc::IN_CLOEXEC) };
        if fd < 0 {
            return Err(std::io::Error::last_os_error());
        }
        // SAFETY: fd was just returned to this process and is owned here.
        let fd = unsafe { OwnedFd::from_raw_fd(fd) };
        // The name given, which a link swapped or a file renamed over
        // changes, and what it resolves to, which an edit through a link
        // (into a dotfiles checkout) changes without touching the name.
        let mut names: HashMap<i32, Vec<OsString>> = HashMap::new();
        let (own, name) = add(fd.as_raw_fd(), path)?;
        names.entry(own).or_default().push(name);
        let target = std::fs::canonicalize(path).ok();
        if let Some(resolved) = &target
            && let Ok((wd, name)) = add(fd.as_raw_fd(), resolved)
        {
            names.entry(wd).or_default().push(name);
        }
        Ok(Prepared {
            fd,
            names,
            own,
            target,
        })
    }

    pub fn spawn(
        path: PathBuf,
        prepared: Prepared,
        sender: mpsc::Sender<()>,
    ) -> std::io::Result<()> {
        let Prepared {
            fd,
            mut names,
            own,
            mut target,
        } = prepared;
        // SAFETY: the OwnedFd is open, and it keeps the one descriptor
        // until the AsyncFd that owns it drops.
        let fd = unsafe { AsyncFd::register_with_interest(fd, tokio::io::Interest::READABLE) }
            .map_err(std::io::Error::from)?;
        tokio::spawn(async move {
            let mut buffer = vec![0u8; 4096];
            loop {
                let Ok(mut ready) = fd.readable().await else {
                    return;
                };
                let mut changed = false;
                loop {
                    // SAFETY: buffer is writable for its length.
                    let read = unsafe {
                        libc::read(
                            fd.get_ref().as_raw_fd(),
                            buffer.as_mut_ptr().cast(),
                            buffer.len(),
                        )
                    };
                    if read < 0 {
                        let error = std::io::Error::last_os_error();
                        if error.kind() == std::io::ErrorKind::WouldBlock {
                            ready.clear_ready();
                            break;
                        }
                        return;
                    }
                    let read = read as usize;
                    let mut at = 0;
                    const HEADER: usize = std::mem::size_of::<libc::inotify_event>();
                    while at + HEADER <= read {
                        // SAFETY: the kernel wrote a whole event at `at`; it
                        // may be unaligned, so it is read unaligned.
                        let event: libc::inotify_event =
                            unsafe { std::ptr::read_unaligned(buffer[at..].as_ptr().cast()) };
                        let end = (at + HEADER + event.len as usize).min(read);
                        let named = &buffer[at + HEADER..end];
                        let named =
                            &named[..named.iter().position(|b| *b == 0).unwrap_or(named.len())];
                        at = end;
                        if event.mask & libc::IN_Q_OVERFLOW != 0 {
                            changed = true;
                            continue;
                        }
                        if event.mask & libc::IN_IGNORED != 0 {
                            names.remove(&event.wd);
                            // The folder of the name given went away:
                            // nothing more will come.
                            if event.wd == own {
                                return;
                            }
                            continue;
                        }
                        let watched = names.get(&event.wd).is_some_and(|names| {
                            names.iter().any(|name| {
                                named == std::os::unix::ffi::OsStrExt::as_bytes(name.as_os_str())
                            })
                        });
                        changed |= watched;
                    }
                }
                if !changed {
                    continue;
                }
                // The link may name another file now: watch that one too.
                // Resolving and watching touch the filesystem: off the
                // runtime thread. The descriptor outlives the wait.
                let (raw, given, was) = (fd.get_ref().as_raw_fd(), path.clone(), target.clone());
                let found = tokio::task::spawn_blocking(move || {
                    let resolved = std::fs::canonicalize(&given).ok()?;
                    let added = (Some(&resolved) != was.as_ref())
                        .then(|| add(raw, &resolved).ok())
                        .flatten();
                    Some((resolved, added))
                })
                .await
                .ok()
                .flatten();
                if let Some((resolved, added)) = found {
                    target = Some(resolved);
                    if let Some((wd, name)) = added {
                        let names = names.entry(wd).or_default();
                        if !names.contains(&name) {
                            names.push(name);
                        }
                    }
                }
                if !notify(&sender) {
                    return;
                }
            }
        });
        Ok(())
    }
}

#[cfg(target_os = "macos")]
mod imp {
    use super::{c_path, notify, split};
    use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
    use std::path::{Path, PathBuf};
    use tokio::{io::unix::AsyncFd, sync::mpsc};

    fn open_events(path: &Path) -> std::io::Result<OwnedFd> {
        let path = c_path(path)?;
        // SAFETY: path is NUL-terminated; O_EVTONLY opens for events only.
        let fd = unsafe { libc::open(path.as_ptr(), libc::O_EVTONLY | libc::O_CLOEXEC) };
        if fd < 0 {
            return Err(std::io::Error::last_os_error());
        }
        // SAFETY: fd was just returned to this process and is owned here.
        Ok(unsafe { OwnedFd::from_raw_fd(fd) })
    }

    fn register(queue: &OwnedFd, watched: &OwnedFd, flags: u32) -> std::io::Result<()> {
        // SAFETY: an all-zero kevent is valid; the fields set below follow.
        let mut change: libc::kevent = unsafe { std::mem::zeroed() };
        change.ident = watched.as_raw_fd() as libc::uintptr_t;
        change.filter = libc::EVFILT_VNODE;
        change.flags = libc::EV_ADD | libc::EV_CLEAR;
        change.fflags = flags;
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
        match added {
            0 => Ok(()),
            _ => Err(std::io::Error::last_os_error()),
        }
    }

    /// Takes every event the queue holds without waiting: whether there
    /// were any, or none if the queue failed.
    fn drain(queue: &OwnedFd) -> Option<bool> {
        // SAFETY: an all-zero kevent array is valid output space.
        let mut events: [libc::kevent; 8] = unsafe { std::mem::zeroed() };
        let none = libc::timespec {
            tv_sec: 0,
            tv_nsec: 0,
        };
        let mut changed = false;
        loop {
            // SAFETY: no changes in, up to 8 events out, no wait.
            let count = unsafe {
                libc::kevent(
                    queue.as_raw_fd(),
                    std::ptr::null(),
                    0,
                    events.as_mut_ptr(),
                    events.len() as libc::c_int,
                    &none,
                )
            };
            match count {
                ..0 => return None,
                0 => return Some(changed),
                _ => changed = true,
            }
        }
    }

    const FILE: u32 = libc::NOTE_WRITE
        | libc::NOTE_EXTEND
        | libc::NOTE_ATTRIB
        | libc::NOTE_DELETE
        | libc::NOTE_RENAME;

    /// The queue and the descriptors it watches, made off the runtime
    /// thread: opening them resolves paths.
    pub struct Prepared {
        queue: OwnedFd,
        folder: OwnedFd,
        file: Option<OwnedFd>,
    }

    pub fn prepare(path: &Path) -> std::io::Result<Prepared> {
        let (folder, _) = split(path)?;
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
        // The folder's entries (a file renamed into place), and the file
        // itself while it is there (written in place). The file is opened
        // through a link, so an edit to the file a link names is told too,
        // and a file renamed over that one ends its watch and is reopened.
        let folder = open_events(&folder)?;
        register(&queue, &folder, libc::NOTE_WRITE)?;
        let file = open_events(path).ok();
        if let Some(file) = &file {
            register(&queue, file, FILE)?;
        }
        Ok(Prepared {
            queue,
            folder,
            file,
        })
    }

    pub fn spawn(
        path: PathBuf,
        prepared: Prepared,
        sender: mpsc::Sender<()>,
    ) -> std::io::Result<()> {
        let Prepared {
            queue,
            folder,
            mut file,
        } = prepared;
        // SAFETY: the OwnedFd is open, and it keeps the one descriptor
        // until the AsyncFd that owns it drops. Readable only: a kqueue
        // has no write side, and asking for one fails the registration.
        let queue =
            unsafe { AsyncFd::register_with_interest(queue, tokio::io::Interest::READABLE) }
                .map_err(std::io::Error::from)?;
        tokio::spawn(async move {
            let _folder = folder;
            loop {
                let Ok(mut ready) = queue.readable().await else {
                    return;
                };
                let Some(changed) = drain(queue.get_ref()) else {
                    return;
                };
                ready.clear_ready();
                drop(ready);
                if changed {
                    // A write in place is told as it happens, not as it
                    // ends: let it finish before the file is read.
                    tokio::time::sleep(std::time::Duration::from_millis(100)).await;
                    // The file may be another inode now: watch the one
                    // there, the old one's watch ending as it closes.
                    drop(file.take());
                    // Opening resolves the path: off the runtime thread.
                    let given = path.clone();
                    file = tokio::task::spawn_blocking(move || open_events(&given).ok())
                        .await
                        .ok()
                        .flatten();
                    if let Some(file) = &file
                        && register(queue.get_ref(), file, FILE).is_err()
                    {
                        return;
                    }
                    if !notify(&sender) {
                        return;
                    }
                }
            }
        });
        Ok(())
    }
}

#[cfg(not(any(target_os = "linux", target_os = "macos")))]
mod imp {
    use std::path::PathBuf;
    use tokio::sync::mpsc;

    pub struct Prepared;

    pub fn prepare(_: &std::path::Path) -> std::io::Result<Prepared> {
        Err(std::io::Error::from(std::io::ErrorKind::Unsupported))
    }

    pub fn spawn(_: PathBuf, _: Prepared, _: mpsc::Sender<()>) -> std::io::Result<()> {
        Err(std::io::Error::from(std::io::ErrorKind::Unsupported))
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    async fn told(receiver: &mut tokio::sync::mpsc::Receiver<()>) -> bool {
        // A watcher that ended closes the channel, which tells nothing.
        matches!(
            tokio::time::timeout(Duration::from_secs(5), receiver.recv()).await,
            Ok(Some(()))
        )
    }

    #[tokio::test]
    async fn a_write_in_place_and_a_rename_over_the_file_are_both_told() {
        let dir = std::env::temp_dir().join(format!("watch-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("config.toml");
        std::fs::write(&path, "a = 1\n").unwrap();
        let mut receiver = super::watch(&path).await.unwrap();
        std::fs::write(&path, "a = 2\n").unwrap();
        assert!(told(&mut receiver).await, "write in place");
        // Drain what one write may have told twice.
        while matches!(
            tokio::time::timeout(Duration::from_millis(200), receiver.recv()).await,
            Ok(Some(()))
        ) {}
        std::fs::write(dir.join(".config.toml.tmp"), "a = 3\n").unwrap();
        std::fs::rename(dir.join(".config.toml.tmp"), &path).unwrap();
        assert!(told(&mut receiver).await, "rename over");
        while matches!(
            tokio::time::timeout(Duration::from_millis(200), receiver.recv()).await,
            Ok(Some(()))
        ) {}
        // A rewrite after the rename is still told: the new file is watched.
        std::fs::write(&path, "a = 4\n").unwrap();
        assert!(told(&mut receiver).await, "write after rename");
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[tokio::test]
    async fn an_edit_to_the_file_a_link_names_is_told() {
        let dir = std::env::temp_dir().join(format!("watch-link-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("home")).unwrap();
        std::fs::create_dir_all(dir.join("dotfiles")).unwrap();
        let target = dir.join("dotfiles/config.toml");
        std::fs::write(&target, "a = 1\n").unwrap();
        let path = dir.join("home/config.toml");
        std::os::unix::fs::symlink(&target, &path).unwrap();
        let mut receiver = super::watch(&path).await.unwrap();
        std::fs::write(&target, "a = 2\n").unwrap();
        assert!(told(&mut receiver).await, "write to the target");
        while matches!(
            tokio::time::timeout(Duration::from_millis(200), receiver.recv()).await,
            Ok(Some(()))
        ) {}
        // The link pointed elsewhere: edits there are told from then on.
        let other = dir.join("dotfiles/other.toml");
        std::fs::write(&other, "a = 3\n").unwrap();
        std::fs::remove_file(&path).unwrap();
        std::os::unix::fs::symlink(&other, &path).unwrap();
        assert!(told(&mut receiver).await, "link swapped");
        while matches!(
            tokio::time::timeout(Duration::from_millis(200), receiver.recv()).await,
            Ok(Some(()))
        ) {}
        std::fs::write(&other, "a = 4\n").unwrap();
        assert!(told(&mut receiver).await, "write to the new target");
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
