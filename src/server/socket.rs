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

/// Each sandboxed bot's own socket, in a folder beside the daemon's: opened
/// the first time one of its commands runs, and closed when the bot is
/// deleted.
/// A connection on it is a session answered as that bot. `None` when the
/// folder's sockets wouldn't fit a socket path.
pub fn capabilities(
    socket: &Path,
    sender: super::session::Sender,
    next: std::sync::Arc<std::sync::atomic::AtomicU64>,
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
    type Opened = std::collections::HashMap<i64, (PathBuf, tokio::task::AbortHandle)>;
    let opened = std::sync::Arc::new(std::sync::Mutex::new(None::<Opened>));
    let (place, own) = (folder.clone(), socket.to_owned());
    let closing = opened.clone();
    Some(agent_runtime::sandbox::Capabilities {
        folder,
        close: std::sync::Arc::new(move |bot| {
            let mut opened = closing.lock().unwrap_or_else(|e| e.into_inner());
            if let Some((path, accepting)) = opened.as_mut().and_then(|opened| opened.remove(&bot))
            {
                accepting.abort();
                let _ = std::fs::remove_file(path);
            }
        }),
        open: std::sync::Arc::new(move |bot| {
            let mut opened = opened.lock().unwrap_or_else(|e| e.into_inner());
            if opened.is_none() {
                prepare(&place, &own)?;
            }
            let opened = opened.get_or_insert_default();
            if let Some((path, _)) = opened.get(&bot) {
                return Ok(path.clone());
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
            opened.insert(bot, (path.clone(), accepting.abort_handle()));
            Ok(path)
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
        let listener = UnixListener::bind(&path).map_err(|_| Error::new("socket_bind_failed"))?;
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

    #[tokio::test]
    async fn shutdown_does_not_unlink_a_replacement_socket() {
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
