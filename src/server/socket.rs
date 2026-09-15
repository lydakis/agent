//! Ownership of the rendezvous path is independent of database ownership.
use agent_runtime::{Error, Result, fail};
use std::{
    fs::{File, OpenOptions},
    os::unix::fs::{FileTypeExt, MetadataExt, OpenOptionsExt},
    path::{Path, PathBuf},
    time::Duration,
};
use tokio::net::{UnixListener, UnixStream};

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
