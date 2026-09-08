//! One database worker for all agents. Durable writes never block the I/O runtime.
use crate::{Error, Result};
use rusqlite::Connection;
use std::{
    fs::{File, OpenOptions},
    path::Path,
};
use tokio::sync::{mpsc, oneshot};

mod db;
pub use db::{Bot, Database, Started};

type Job = Box<dyn FnOnce(&mut Database) + Send>;
#[derive(Clone)]
pub struct Store {
    sender: mpsc::Sender<Job>,
}

impl From<rusqlite::Error> for Error {
    fn from(_: rusqlite::Error) -> Self {
        Self("storage_error".into())
    }
}

impl Store {
    pub async fn open(path: &Path, configuration: String) -> Result<Self> {
        let path = path.to_path_buf();
        let (sender, mut receiver) = mpsc::channel::<Job>(32);
        let (ready, opened) = oneshot::channel();
        std::thread::Builder::new()
            .name("agent-storage".into())
            .spawn(move || {
                let opened: Result<(Database, File)> = (|| {
                    if let Some(parent) = path.parent() {
                        std::fs::create_dir_all(parent)?;
                    }
                    // Resolve existing files and parent-directory aliases without
                    // opening an extra handle to SQLite's own lock target.
                    let path = match std::fs::canonicalize(&path) {
                        Ok(path) => path,
                        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                            if std::fs::symlink_metadata(&path).is_ok() {
                                return crate::fail("store_path_unresolved");
                            }
                            let parent = path
                                .parent()
                                .filter(|p| !p.as_os_str().is_empty())
                                .unwrap_or(Path::new("."));
                            std::fs::canonicalize(parent)?
                                .join(path.file_name().ok_or(Error("invalid_store_path".into()))?)
                        }
                        Err(error) => return Err(error.into()),
                    };
                    let mut lock_path = path.as_os_str().to_os_string();
                    lock_path.push(".owner-lock");
                    // Append rather than replace the suffix, so distinct files
                    // sharing a stem still have distinct ownership locks.
                    let lock = OpenOptions::new()
                        .create(true)
                        .truncate(false)
                        .read(true)
                        .write(true)
                        .open(lock_path)?;
                    lock.try_lock()
                        .map_err(|_| Error("store_already_owned".into()))?;
                    // SQLite WAL sidecars cannot safely follow hard-link aliases.
                    #[cfg(unix)]
                    match std::fs::metadata(&path) {
                        Ok(metadata) => {
                            use std::os::unix::fs::MetadataExt;
                            if metadata.nlink() != 1 {
                                return crate::fail("store_hard_links_unsupported");
                            }
                        }
                        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                        Err(error) => return Err(error.into()),
                    }
                    let db = Database::initialize(Connection::open(&path)?, &configuration)?;
                    Ok((db, lock))
                })();
                match opened {
                    Ok((mut db, _lock)) => {
                        let _ = ready.send(Ok(()));
                        while let Some(job) = receiver.blocking_recv() {
                            job(&mut db);
                        }
                    }
                    Err(error) => {
                        let _ = ready.send(Err(error));
                    }
                }
            })?;
        opened
            .await
            .map_err(|_| Error("storage_worker_failed".into()))??;
        Ok(Self { sender })
    }

    pub async fn call<T: Send + 'static>(
        &self,
        operation: impl FnOnce(&mut Database) -> Result<T> + Send + 'static,
    ) -> Result<T> {
        let (sender, receiver) = oneshot::channel();
        self.sender
            .send(Box::new(move |db| {
                let _ = sender.send(operation(db));
            }))
            .await
            .map_err(|_| Error("storage_worker_failed".into()))?;
        receiver
            .await
            .map_err(|_| Error("storage_worker_failed".into()))?
    }
}
