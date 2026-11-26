//! Files and folders the kernel tells about when they may have changed:
//! inotify on Linux and kqueue on macOS, so a watch costs nothing between
//! changes (no polling). The daemon watches its providers' config
//! ([`file`]); Sonny's lookout watches the follows folder ([`Folder`]).
//!
//! A notice says only that something may have changed; the reader compares
//! what it reads now with what it read last. Any number of changes before
//! the reader looks are one notice.
//!
//! The kernel queues a notice in the write itself (the rename that puts a
//! file in place, or the close that ends a write), before the writer goes
//! on. So whatever a writer finished before it told anyone, by a socket or
//! otherwise, is in the queue when the telling arrives: [`Folder::take`]
//! reads the queue without waiting.
//!
//! On macOS a folder's watch says only that an entry came, went or was
//! renamed over, never which; and a file edited in place (`>>` by hand) is
//! told only where the file itself is watched, as [`file`] does.
use std::collections::HashSet;
use std::ffi::OsString;
use std::os::fd::{AsRawFd, OwnedFd};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use tokio::io::unix::AsyncFd;
use tokio::sync::mpsc;

/// A receiver told, at most one notice pending, each time `path` may have
/// changed. The folder is watched, not only the file, since an editor or a
/// settings writer usually replaces a file by renaming another over it.
/// None where the platform has no watch or the folder cannot be watched;
/// the file is then read at start only.
pub async fn file(path: &Path) -> Option<mpsc::Receiver<()>> {
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

/// What changed in a watched folder since the last look.
#[derive(Debug, PartialEq, Eq)]
pub enum Changed {
    /// Nothing.
    None,
    /// The entries of these names: written, renamed in or out, or removed.
    Names(HashSet<OsString>),
    /// Any of them: the kernel did not say which (macOS, or a queue that
    /// overflowed), so every entry is to be looked at.
    Any,
    /// The watch ended, as when the folder was removed or moved: every
    /// entry is to be looked at, and the folder watched again if it can be.
    Ended,
}

/// A folder's watch: a descriptor the kernel makes readable once a change
/// is queued, and read without waiting by [`Folder::take`]. Entries inside
/// a folder within it are not told.
pub struct Folder {
    fd: Arc<AsyncFd<OwnedFd>>,
    /// The macOS watch needs the folder open as long as it watches.
    #[cfg(target_os = "macos")]
    _folder: OwnedFd,
}

impl Folder {
    /// The folder watched, or why not: a platform with no watch, or a
    /// folder that cannot be watched. Must run on the runtime.
    pub fn new(folder: &Path) -> std::io::Result<Folder> {
        imp::folder(folder)
    }

    /// What to wait on: readable once a change is queued. Clear its
    /// readiness before [`Folder::take`], so a change after the take makes
    /// it readable again.
    pub fn ready(&self) -> Arc<AsyncFd<OwnedFd>> {
        self.fd.clone()
    }

    /// Takes every notice queued, without waiting.
    pub fn take(&self) -> Changed {
        imp::take(self)
    }
}

/// Tells the receiver without waiting: one pending notice covers any number
/// of changes, since each reads the file whole.
fn notify(sender: &mpsc::Sender<()>) -> bool {
    !matches!(
        sender.try_send(()),
        Err(mpsc::error::TrySendError::Closed(_))
    )
}

fn split(path: &Path) -> std::io::Result<(PathBuf, OsString)> {
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
    use super::{AsRawFd, AsyncFd, Changed, Folder, OwnedFd, c_path, notify, split};
    use std::collections::HashMap;
    use std::ffi::OsString;
    use std::os::fd::{FromRawFd, RawFd};
    use std::os::unix::ffi::OsStrExt;
    use std::path::{Path, PathBuf};
    use std::sync::Arc;
    use tokio::sync::mpsc;

    /// A write closed, or an entry moved in, moved out or removed in a
    /// folder: never a plain modify, so a write is told once, as it ends,
    /// and a log appended beside it costs no wakeup.
    const ENTRIES: u32 =
        libc::IN_CLOSE_WRITE | libc::IN_MOVED_TO | libc::IN_MOVED_FROM | libc::IN_DELETE;

    fn inotify() -> std::io::Result<OwnedFd> {
        // SAFETY: inotify_init1 takes flags only and returns a new fd or -1.
        let fd = unsafe { libc::inotify_init1(libc::IN_NONBLOCK | libc::IN_CLOEXEC) };
        if fd < 0 {
            return Err(std::io::Error::last_os_error());
        }
        // SAFETY: fd was just returned to this process and is owned here.
        Ok(unsafe { OwnedFd::from_raw_fd(fd) })
    }

    /// Watches `folder` for `mask`: the watch descriptor.
    fn watch(fd: RawFd, folder: &Path, mask: u32) -> std::io::Result<i32> {
        let folder = c_path(folder)?;
        // SAFETY: fd is a live inotify fd and folder a NUL-terminated path.
        let wd = unsafe { libc::inotify_add_watch(fd, folder.as_ptr(), mask) };
        if wd < 0 {
            return Err(std::io::Error::last_os_error());
        }
        Ok(wd)
    }

    /// Every event queued, read without waiting, each handed to `each` as
    /// its watch, mask and name: an error but the queue running dry.
    fn read(fd: RawFd, mut each: impl FnMut(i32, u32, &[u8])) -> std::io::Result<()> {
        // Room for many events at once; each is a header and a name.
        let mut buffer = [0u8; 16 * 1024];
        loop {
            // SAFETY: buffer is writable for its length.
            let read = unsafe { libc::read(fd, buffer.as_mut_ptr().cast(), buffer.len()) };
            if read < 0 {
                let error = std::io::Error::last_os_error();
                match error.kind() {
                    std::io::ErrorKind::WouldBlock => return Ok(()),
                    std::io::ErrorKind::Interrupted => continue,
                    _ => return Err(error),
                }
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
                each(event.wd, event.mask, named);
            }
        }
    }

    pub(super) fn folder(folder: &Path) -> std::io::Result<Folder> {
        let fd = inotify()?;
        let mask = ENTRIES | libc::IN_DELETE_SELF | libc::IN_MOVE_SELF | libc::IN_ONLYDIR;
        watch(fd.as_raw_fd(), folder, mask)?;
        // SAFETY: the OwnedFd is open, and it keeps the one descriptor
        // until the AsyncFd that owns it drops.
        let fd = unsafe { AsyncFd::register_with_interest(fd, tokio::io::Interest::READABLE) }
            .map_err(std::io::Error::from)?;
        Ok(Folder { fd: Arc::new(fd) })
    }

    pub(super) fn take(folder: &Folder) -> Changed {
        let mut names = std::collections::HashSet::new();
        let (mut any, mut ended) = (false, false);
        let read = read(folder.fd.get_ref().as_raw_fd(), |_, mask, named| {
            let gone = libc::IN_IGNORED | libc::IN_DELETE_SELF | libc::IN_MOVE_SELF;
            if mask & gone != 0 {
                ended = true;
            } else if mask & libc::IN_Q_OVERFLOW != 0 {
                any = true;
            } else if !named.is_empty() {
                names.insert(OsString::from(std::ffi::OsStr::from_bytes(named)));
            }
        });
        match (read.is_err() || ended, any, names.is_empty()) {
            (true, ..) => Changed::Ended,
            (_, true, _) => Changed::Any,
            (_, _, true) => Changed::None,
            _ => Changed::Names(names),
        }
    }

    /// Watches the folder `path` is in for entries named as it is: the
    /// watch descriptor and the name.
    fn add(fd: RawFd, path: &Path) -> std::io::Result<(i32, OsString)> {
        let (folder, name) = split(path)?;
        Ok((watch(fd, &folder, ENTRIES | libc::IN_CREATE)?, name))
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
        let fd = inotify()?;
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
            loop {
                let Ok(mut ready) = fd.readable().await else {
                    return;
                };
                let (mut changed, mut gone) = (false, false);
                let read = read(fd.get_ref().as_raw_fd(), |wd, mask, named| {
                    if mask & libc::IN_Q_OVERFLOW != 0 {
                        changed = true;
                        return;
                    }
                    if mask & libc::IN_IGNORED != 0 {
                        names.remove(&wd);
                        // The folder of the name given went away: nothing
                        // more will come.
                        gone |= wd == own;
                        return;
                    }
                    changed |= names
                        .get(&wd)
                        .is_some_and(|names| names.iter().any(|name| named == name.as_bytes()));
                });
                if read.is_err() || gone {
                    return;
                }
                ready.clear_ready();
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
    use super::{AsRawFd, AsyncFd, Changed, Folder, OwnedFd, c_path, notify, split};
    use std::os::fd::FromRawFd;
    use std::path::{Path, PathBuf};
    use std::sync::Arc;
    use tokio::sync::mpsc;

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

    fn kqueue() -> std::io::Result<OwnedFd> {
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
        Ok(queue)
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

    /// Takes every event the queue holds without waiting: what they said,
    /// all their flags together (0 for none), or none if the queue failed.
    fn drain(queue: &OwnedFd) -> Option<u32> {
        // SAFETY: an all-zero kevent array is valid output space.
        let mut events: [libc::kevent; 8] = unsafe { std::mem::zeroed() };
        let none = libc::timespec {
            tv_sec: 0,
            tv_nsec: 0,
        };
        let mut said = 0;
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
                ..0 if std::io::Error::last_os_error().kind()
                    == std::io::ErrorKind::Interrupted =>
                {
                    continue;
                }
                ..0 => return None,
                // An event with no flags still says something changed.
                0 => return Some(said),
                _ => {
                    for event in &events[..count as usize] {
                        said |= event.fflags | libc::NOTE_WRITE;
                    }
                }
            }
        }
    }

    /// The folder itself removed, moved or unmounted.
    const GONE: u32 = libc::NOTE_DELETE | libc::NOTE_RENAME | libc::NOTE_REVOKE;

    pub(super) fn folder(folder: &Path) -> std::io::Result<Folder> {
        let opened = open_events(folder)?;
        let queue = kqueue()?;
        // An entry came, went or was renamed over (the folder's write).
        register(&queue, &opened, libc::NOTE_WRITE | GONE)?;
        // SAFETY: the OwnedFd is open, and it keeps the one descriptor
        // until the AsyncFd that owns it drops. Readable only: a kqueue
        // has no write side, and asking for one fails the registration.
        let fd = unsafe { AsyncFd::register_with_interest(queue, tokio::io::Interest::READABLE) }
            .map_err(std::io::Error::from)?;
        Ok(Folder {
            fd: Arc::new(fd),
            _folder: opened,
        })
    }

    pub(super) fn take(folder: &Folder) -> Changed {
        match drain(folder.fd.get_ref()) {
            None => Changed::Ended,
            Some(said) if said & GONE != 0 => Changed::Ended,
            Some(0) => Changed::None,
            Some(_) => Changed::Any,
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
        let queue = kqueue()?;
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
                let Some(said) = drain(queue.get_ref()) else {
                    return;
                };
                ready.clear_ready();
                drop(ready);
                if said != 0 {
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
    use super::{Changed, Folder};
    use std::path::{Path, PathBuf};
    use tokio::sync::mpsc;

    pub struct Prepared;

    pub fn prepare(_: &Path) -> std::io::Result<Prepared> {
        Err(std::io::Error::from(std::io::ErrorKind::Unsupported))
    }

    pub fn spawn(_: PathBuf, _: Prepared, _: mpsc::Sender<()>) -> std::io::Result<()> {
        Err(std::io::Error::from(std::io::ErrorKind::Unsupported))
    }

    pub(super) fn folder(_: &Path) -> std::io::Result<Folder> {
        Err(std::io::Error::from(std::io::ErrorKind::Unsupported))
    }

    pub(super) fn take(_: &Folder) -> Changed {
        Changed::None
    }
}

#[cfg(test)]
mod tests {
    use super::{Changed, Folder};
    use std::time::Duration;

    async fn told(receiver: &mut tokio::sync::mpsc::Receiver<()>) -> bool {
        // A watcher that ended closes the channel, which tells nothing.
        matches!(
            tokio::time::timeout(Duration::from_secs(5), receiver.recv()).await,
            Ok(Some(()))
        )
    }

    async fn drained(receiver: &mut tokio::sync::mpsc::Receiver<()>) {
        while matches!(
            tokio::time::timeout(Duration::from_millis(200), receiver.recv()).await,
            Ok(Some(()))
        ) {}
    }

    fn scratch(name: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!("watch-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[tokio::test]
    async fn a_write_in_place_and_a_rename_over_the_file_are_both_told() {
        let dir = scratch("file");
        let path = dir.join("config.toml");
        std::fs::write(&path, "a = 1\n").unwrap();
        let mut receiver = super::file(&path).await.unwrap();
        std::fs::write(&path, "a = 2\n").unwrap();
        assert!(told(&mut receiver).await, "write in place");
        // Drain what one write may have told twice.
        drained(&mut receiver).await;
        std::fs::write(dir.join(".config.toml.tmp"), "a = 3\n").unwrap();
        std::fs::rename(dir.join(".config.toml.tmp"), &path).unwrap();
        assert!(told(&mut receiver).await, "rename over");
        drained(&mut receiver).await;
        // A rewrite after the rename is still told: the new file is watched.
        std::fs::write(&path, "a = 4\n").unwrap();
        assert!(told(&mut receiver).await, "write after rename");
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[tokio::test]
    async fn an_edit_to_the_file_a_link_names_is_told() {
        let dir = scratch("link");
        std::fs::create_dir_all(dir.join("home")).unwrap();
        std::fs::create_dir_all(dir.join("dotfiles")).unwrap();
        let target = dir.join("dotfiles/config.toml");
        std::fs::write(&target, "a = 1\n").unwrap();
        let path = dir.join("home/config.toml");
        std::os::unix::fs::symlink(&target, &path).unwrap();
        let mut receiver = super::file(&path).await.unwrap();
        std::fs::write(&target, "a = 2\n").unwrap();
        assert!(told(&mut receiver).await, "write to the target");
        drained(&mut receiver).await;
        // The link pointed elsewhere: edits there are told from then on.
        let other = dir.join("dotfiles/other.toml");
        std::fs::write(&other, "a = 3\n").unwrap();
        std::fs::remove_file(&path).unwrap();
        std::os::unix::fs::symlink(&other, &path).unwrap();
        assert!(told(&mut receiver).await, "link swapped");
        drained(&mut receiver).await;
        std::fs::write(&other, "a = 4\n").unwrap();
        assert!(told(&mut receiver).await, "write to the new target");
        std::fs::remove_dir_all(&dir).unwrap();
    }

    /// What the folder's watch says once it is readable, or within a second.
    async fn taken(folder: &Folder) -> Changed {
        let ready = folder.ready();
        if let Ok(Ok(mut guard)) =
            tokio::time::timeout(Duration::from_secs(1), ready.readable()).await
        {
            guard.clear_ready();
        }
        folder.take()
    }

    fn named(names: &[&str]) -> Changed {
        Changed::Names(names.iter().map(Into::into).collect())
    }

    /// What a folder's watch says of a change to `names`: the names where
    /// the kernel says them, else that something changed.
    fn said(names: &[&str]) -> Changed {
        match cfg!(target_os = "linux") {
            true => named(names),
            false => Changed::Any,
        }
    }

    #[tokio::test]
    async fn an_entry_renamed_into_a_folder_is_queued_by_the_rename_itself() {
        let dir = scratch("folder");
        let folder = Folder::new(&dir).unwrap();
        assert_eq!(folder.take(), Changed::None);
        std::fs::write(dir.join("a.json.7.partial"), "{}").unwrap();
        std::fs::rename(dir.join("a.json.7.partial"), dir.join("a.json")).unwrap();
        // Read with no wait: the rename queued it. The partial's write
        // closed, then it moved away as a.json came.
        assert_eq!(folder.take(), said(&["a.json.7.partial", "a.json"]));
        // Many changes before a look are one.
        for name in ["b", "c", "b"] {
            std::fs::write(dir.join(name), "{}").unwrap();
        }
        assert_eq!(taken(&folder).await, said(&["b", "c"]));
        assert_eq!(folder.take(), Changed::None);
        // A folder inside is not watched.
        std::fs::create_dir_all(dir.join("inner")).unwrap();
        let _ = folder.take();
        std::fs::write(dir.join("inner").join("a.json"), "{}").unwrap();
        assert_eq!(folder.take(), Changed::None);
        // A removal is told.
        std::fs::remove_file(dir.join("a.json")).unwrap();
        assert_eq!(folder.take(), said(&["a.json"]));
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[cfg(target_os = "linux")]
    #[tokio::test]
    async fn an_entry_edited_in_place_is_named_on_linux() {
        let dir = scratch("in-place");
        std::fs::write(dir.join("x.json"), "{}").unwrap();
        let folder = Folder::new(&dir).unwrap();
        use std::io::Write;
        let mut file = (std::fs::OpenOptions::new().append(true))
            .open(dir.join("x.json"))
            .unwrap();
        file.write_all(b"\n").unwrap();
        drop(file);
        assert_eq!(folder.take(), named(&["x.json"]));
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[tokio::test]
    async fn a_watched_folder_removed_ends_its_watch() {
        let dir = scratch("gone");
        let folder = Folder::new(&dir).unwrap();
        std::fs::remove_dir_all(&dir).unwrap();
        assert_eq!(taken(&folder).await, Changed::Ended);
    }
}
