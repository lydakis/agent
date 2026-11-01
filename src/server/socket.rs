//! Ownership of the rendezvous path is independent of database ownership.
use agent_runtime::{Error, Result, fail, fail_with};
use std::{
    fs::{File, OpenOptions},
    os::unix::fs::{FileTypeExt, MetadataExt, OpenOptionsExt},
    path::{Path, PathBuf},
    time::Duration,
};
use tokio::net::{UnixListener, UnixStream};

/// The longest socket path every system takes: macOS holds 104 bytes with
/// the terminating NUL, Linux 108.
const SOCKET_PATH: usize = 103;

/// How long a bot's socket keeps listening after the last command or turn
/// that could use it is gone, so back-to-back commands don't bind again.
const LISTEN_GRACE: Duration = Duration::from_secs(30);

/// Each sandboxed bot's own socket, in a folder beside the daemon's: opened
/// when one of its commands needs it, listening while any of its live turns
/// or running commands holds a share, and closed a grace after the last one
/// goes, as systemd's socket activation keeps a unit's socket, or when the
/// bot is deleted. Connections already accepted keep their sessions.
/// A connection on it is a session answered as that bot. `None` when the
/// folder's sockets wouldn't fit a socket path.
pub fn capabilities(
    socket: &Path,
    sender: super::session::Sender,
    next: std::sync::Arc<std::sync::atomic::AtomicU64>,
) -> Option<agent_runtime::sandbox::Capabilities> {
    capabilities_with(socket, sender, next, LISTEN_GRACE)
}

/// One bot's listening socket, and the share that keeps it open.
struct Open {
    path: PathBuf,
    accepting: tokio::task::AbortHandle,
    share: std::sync::Weak<Share>,
    /// Which share last opened it, so a grace that ends after a new share
    /// was taken leaves it listening.
    generation: u64,
}
type Opened = std::sync::Arc<std::sync::Mutex<Option<std::collections::HashMap<i64, Open>>>>;

/// A share in one bot's socket; the last one dropped starts its grace.
struct Share {
    bot: i64,
    generation: u64,
    expiries: std::sync::Arc<Expiries>,
}

impl Drop for Share {
    fn drop(&mut self) {
        self.expiries.push(self.bot, self.generation);
    }
}

/// When each bot's socket may close: one queue for every bot, in the order
/// the grace started, which is the order it ends since every grace is the
/// same. One task sleeps until the first, so a fleet's socket graces take
/// no task or timer each.
struct Expiries {
    due: std::sync::Mutex<std::collections::VecDeque<(tokio::time::Instant, i64, u64)>>,
    pushed: tokio::sync::Notify,
    grace: Duration,
}

impl Expiries {
    fn push(&self, bot: i64, generation: u64) {
        let deadline = tokio::time::Instant::now() + self.grace;
        let mut due = self.due.lock().unwrap_or_else(|e| e.into_inner());
        due.push_back((deadline, bot, generation));
        if due.len() == 1 {
            self.pushed.notify_one();
        }
    }

    /// Close each socket whose grace has ended with no share taken since.
    /// The check and the unlink run off the runtime thread, under the lock
    /// an open takes, so a socket opened again in the meantime is not the
    /// one removed.
    async fn run(self: std::sync::Arc<Self>, opened: Opened) {
        loop {
            let first = self
                .due
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .front()
                .map(|(deadline, ..)| *deadline);
            match first {
                None => self.pushed.notified().await,
                Some(deadline) => tokio::time::sleep_until(deadline).await,
            }
            let now = tokio::time::Instant::now();
            let ended: Vec<(i64, u64)> = {
                let mut due = self.due.lock().unwrap_or_else(|e| e.into_inner());
                let ready = due.partition_point(|(deadline, ..)| *deadline <= now);
                due.drain(..ready)
                    .map(|(_, bot, generation)| (bot, generation))
                    .collect()
            };
            if ended.is_empty() {
                continue;
            }
            let opened = opened.clone();
            let _ = tokio::task::spawn_blocking(move || {
                let mut opened = opened.lock().unwrap_or_else(|e| e.into_inner());
                let Some(map) = opened.as_mut() else {
                    return;
                };
                for (bot, generation) in ended {
                    if map.get(&bot).is_some_and(|open| {
                        open.generation == generation && open.share.strong_count() == 0
                    }) && let Some(open) = map.remove(&bot)
                    {
                        open.accepting.abort();
                        let _ = std::fs::remove_file(open.path);
                    }
                }
            })
            .await;
        }
    }
}

fn capabilities_with(
    socket: &Path,
    sender: super::session::Sender,
    next: std::sync::Arc<std::sync::atomic::AtomicU64>,
    grace: Duration,
) -> Option<agent_runtime::sandbox::Capabilities> {
    let mut folder = socket.as_os_str().to_owned();
    folder.push(".d");
    let folder = PathBuf::from(folder);
    if folder.as_os_str().len() + 1 + i64::MAX.to_string().len() > SOCKET_PATH {
        return None;
    }
    let runtime = tokio::runtime::Handle::current();
    // The folder is made ready by the first bot to need it, once the
    // daemon owns its socket.
    let opened: Opened = std::sync::Arc::default();
    let expiries = std::sync::Arc::new(Expiries {
        due: Default::default(),
        pushed: tokio::sync::Notify::new(),
        grace,
    });
    runtime.spawn(expiries.clone().run(opened.clone()));
    let generations = std::sync::atomic::AtomicU64::new(0);
    let (place, own) = (folder.clone(), socket.to_owned());
    let closing = opened.clone();
    Some(agent_runtime::sandbox::Capabilities {
        folder,
        close: std::sync::Arc::new(move |bot| {
            let mut opened = closing.lock().unwrap_or_else(|e| e.into_inner());
            if let Some(open) = opened.as_mut().and_then(|opened| opened.remove(&bot)) {
                open.accepting.abort();
                let _ = std::fs::remove_file(open.path);
            }
        }),
        open: std::sync::Arc::new(move |bot| {
            let mut guard = opened.lock().unwrap_or_else(|e| e.into_inner());
            if guard.is_none() {
                prepare(&place, &own)?;
            }
            let map = guard.get_or_insert_default();
            let share = |generation| {
                std::sync::Arc::new(Share {
                    bot,
                    generation,
                    expiries: expiries.clone(),
                })
            };
            let generation = generations.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            if let Some(open) = map.get_mut(&bot) {
                let held = match open.share.upgrade() {
                    Some(held) => held,
                    None => {
                        let held = share(generation);
                        open.share = std::sync::Arc::downgrade(&held);
                        open.generation = generation;
                        held
                    }
                };
                return Ok((open.path.clone(), agent_runtime::sandbox::Listening(held)));
            }
            let path = place.join(bot.to_string());
            let listener = std::os::unix::net::UnixListener::bind(&path)
                .map_err(|_| Error::new("socket_bind_failed"))?;
            listener.set_nonblocking(true)?;
            let listener = {
                let _entered = runtime.enter();
                UnixListener::from_std(listener)?
            };
            let accepting = runtime.spawn(super::session::accept(
                listener,
                sender.clone(),
                next.clone(),
                Some(bot),
                None,
            ));
            let held = share(generation);
            map.insert(
                bot,
                Open {
                    path: path.clone(),
                    accepting: accepting.abort_handle(),
                    share: std::sync::Arc::downgrade(&held),
                    generation,
                },
            );
            Ok((path, agent_runtime::sandbox::Listening(held)))
        }),
    })
}

/// The bots' socket folder, the daemon user's alone (whoever owns the
/// daemon's `socket`), with a previous daemon's sockets gone. Another user's
/// folder could hand one bot another's socket, so it is refused.
fn prepare(folder: &Path, socket: &Path) -> Result<()> {
    use std::os::unix::fs::{DirBuilderExt, PermissionsExt};
    match std::fs::symlink_metadata(folder) {
        Ok(found) => {
            if !found.is_dir()
                || found.uid() != std::fs::symlink_metadata(socket)?.uid()
                || found.permissions().mode() & 0o077 != 0
            {
                return fail_with(
                    "sandbox_unavailable",
                    format!(
                        "{} must be a folder only the daemon's user may use",
                        folder.display()
                    ),
                );
            }
            for entry in std::fs::read_dir(folder)? {
                let entry = entry?;
                if entry.file_type()?.is_socket() {
                    std::fs::remove_file(entry.path())?;
                }
            }
            Ok(())
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            Ok(std::fs::DirBuilder::new().mode(0o700).create(folder)?)
        }
        Err(error) => Err(error.into()),
    }
}

/// The process umask set to `177` until dropped, which restores it: a
/// socket is made `0777` less the mask, as a folder is, so `077` would
/// leave it `0700`.
struct Umask(libc::mode_t);
impl Umask {
    fn owner_only() -> Self {
        // SAFETY: umask only replaces the calling process's mask and
        // returns the old one.
        Self(unsafe { libc::umask(0o177) })
    }
}
impl Drop for Umask {
    fn drop(&mut self) {
        // SAFETY: as above, restoring what was taken.
        unsafe { libc::umask(self.0) };
    }
}

pub struct Owner {
    path: PathBuf,
    identity: (u64, u64),
    _lock: File,
}
impl Owner {
    pub async fn bind(path: &Path) -> Result<(Self, UnixListener)> {
        let parent = path
            .parent()
            .filter(|p| !p.as_os_str().is_empty())
            .unwrap_or(Path::new("."));
        let path = std::fs::canonicalize(parent)?
            .join(path.file_name().ok_or(Error::new("invalid_socket_path"))?);
        let mut lock_path = path.as_os_str().to_os_string();
        lock_path.push(".owner-lock");
        let lock = OpenOptions::new()
            .create(true)
            .truncate(false)
            .read(true)
            .write(true)
            .mode(0o600)
            .custom_flags(libc::O_NOFOLLOW)
            .open(lock_path)?;
        lock.try_lock()
            .map_err(|_| Error::new("socket_already_owned"))?;
        match std::fs::symlink_metadata(&path) {
            Ok(metadata) => {
                if !metadata.file_type().is_socket() {
                    return fail("socket_path_not_socket");
                }
                match tokio::time::timeout(Duration::from_millis(250), UnixStream::connect(&path))
                    .await
                {
                    Ok(Err(error)) if error.kind() == std::io::ErrorKind::ConnectionRefused => {
                        std::fs::remove_file(&path)?;
                    }
                    // An unresponsive or foreign listener must not be displaced.
                    _ => return fail("socket_already_owned"),
                }
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => return Err(error.into()),
        }
        // Owner-only from the moment the name exists. Whoever can connect
        // has every bot, so the mode is set with the umask around the
        // bind, not with a chmod after it: a chmod would leave an instant
        // in which another user's connect lands in the backlog and is
        // accepted later. The umask is the process's, but the daemon
        // binds before it accepts a session or starts a companion, so no
        // child is spawned in that instant, and a file another thread
        // makes then is only tighter than it would be.
        let listener = {
            let _owner_only = Umask::owner_only();
            UnixListener::bind(&path).map_err(|_| Error::new("socket_bind_failed"))?
        };
        let metadata = std::fs::symlink_metadata(&path)?;
        Ok((
            Self {
                path,
                identity: (metadata.dev(), metadata.ino()),
                _lock: lock,
            },
            listener,
        ))
    }
}
impl Drop for Owner {
    fn drop(&mut self) {
        if let Ok(metadata) = std::fs::symlink_metadata(&self.path)
            && metadata.file_type().is_socket()
            && (metadata.dev(), metadata.ino()) == self.identity
        {
            let _ = std::fs::remove_file(&self.path);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The umask is the process's: tests that bind take turns, so one
    /// does not see another's mask.
    static BINDING: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

    #[tokio::test]
    async fn the_socket_is_its_owner_s_alone_and_the_umask_is_given_back() {
        use std::os::unix::fs::PermissionsExt;
        let _turn = BINDING.lock().await;
        let mode = |path: &Path| {
            std::fs::symlink_metadata(path)
                .unwrap()
                .permissions()
                .mode()
        };
        let directory =
            std::env::temp_dir().join(format!("agent-socket-mode-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&directory);
        std::fs::create_dir_all(&directory).unwrap();
        let before = directory.join("before");
        File::create(&before).unwrap();
        let path = directory.join("endpoint");
        let (owner, listener) = Owner::bind(&path).await.unwrap();
        assert_eq!(mode(&path) & 0o777, 0o600);
        assert_eq!(mode(&directory.join("endpoint.owner-lock")) & 0o777, 0o600);
        // What the process makes afterwards takes the umask it had.
        let after = directory.join("after");
        File::create(&after).unwrap();
        assert_eq!(mode(&after), mode(&before));
        drop((owner, listener));
        std::fs::remove_dir_all(directory).unwrap();
    }

    #[tokio::test]
    async fn a_bot_s_socket_listens_while_a_share_lives_and_a_grace_after() {
        let directory =
            std::env::temp_dir().join(format!("agent-socket-shares-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&directory);
        std::fs::create_dir_all(&directory).unwrap();
        let socket = directory.join("d.sock");
        let _daemon = std::os::unix::net::UnixListener::bind(&socket).unwrap();
        let (sender, _receiver) = super::super::session::channel(64);
        let grace = Duration::from_millis(200);
        let capabilities = capabilities_with(&socket, sender, Default::default(), grace).unwrap();
        let (path, first) = (capabilities.open)(7).unwrap();
        let (again, second) = (capabilities.open)(7).unwrap();
        assert_eq!(path, again);
        assert!(UnixStream::connect(&path).await.is_ok());
        // One share gone: the other keeps it listening past the grace.
        drop(first);
        tokio::time::sleep(grace * 2).await;
        assert!(UnixStream::connect(&path).await.is_ok());
        // A share taken again within the grace keeps it too, and the same
        // listener serves it.
        drop(second);
        tokio::time::sleep(grace / 4).await;
        let (_, third) = (capabilities.open)(7).unwrap();
        tokio::time::sleep(grace * 2).await;
        assert!(UnixStream::connect(&path).await.is_ok());
        // The last share gone: closed after the grace, and opened again on
        // the next use.
        drop(third);
        tokio::time::sleep(grace / 4).await;
        assert!(UnixStream::connect(&path).await.is_ok());
        tokio::time::sleep(grace * 2).await;
        assert!(UnixStream::connect(&path).await.is_err());
        assert!(!path.exists());
        let (path, fourth) = (capabilities.open)(7).unwrap();
        assert!(UnixStream::connect(&path).await.is_ok());
        // Deletion closes it at once, shares or not.
        (capabilities.close)(7);
        assert!(UnixStream::connect(&path).await.is_err());
        drop(fourth);
        std::fs::remove_dir_all(directory).unwrap();
    }

    #[tokio::test]
    async fn shutdown_does_not_unlink_a_replacement_socket() {
        let _turn = BINDING.lock().await;
        let directory =
            std::env::temp_dir().join(format!("agent-socket-owner-{}", std::process::id()));
        std::fs::create_dir_all(&directory).unwrap();
        let path = directory.join("endpoint");
        let (owner, listener) = Owner::bind(&path).await.unwrap();
        let original = directory.join("original");
        std::fs::rename(&path, &original).unwrap();
        let replacement = UnixListener::bind(&path).unwrap();
        drop(owner);
        assert!(UnixStream::connect(&path).await.is_ok());
        drop((listener, replacement));
        std::fs::remove_dir_all(directory).unwrap();
    }
}
