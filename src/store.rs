//! One database worker for all agents. Durable writes never block the I/O runtime.
use crate::{Error, Result};
use rusqlite::Connection;
use std::{
    fs::{File, OpenOptions},
    path::Path,
};
use tokio::sync::{mpsc, oneshot};

mod db;
pub use db::{
    Absorbed, Binding, Bot, Database, Delivery, Fork, Publication, Started, TurnContext,
    TurnOptions, Waiting, Window, cache_hit,
};

type Job = Box<dyn FnOnce(&mut Database) + Send>;
/// Storage worker counters: how long jobs queued for the worker versus how
/// long they ran on it. The split says whether the worker or the disk is the
/// bottleneck; three clock reads per job.
#[derive(Default)]
pub struct Counters {
    pub jobs: std::sync::atomic::AtomicU64,
    pub queued_ns: std::sync::atomic::AtomicU64,
    pub ran_ns: std::sync::atomic::AtomicU64,
}
#[derive(Clone)]
pub struct Store {
    sender: mpsc::Sender<Job>,
    path: std::sync::Arc<std::path::PathBuf>,
    counters: std::sync::Arc<Counters>,
}

impl From<rusqlite::Error> for Error {
    fn from(_: rusqlite::Error) -> Self {
        Self::new("storage_error")
    }
}

impl Store {
    /// Open the store and its publication stream. The worker publishes
    /// what each job committed, in commit order, before taking the next
    /// job; the stream is bounded, so a publisher that stops reading
    /// eventually holds the worker, never memory.
    pub async fn open(path: &Path) -> Result<(Self, mpsc::Receiver<Publication>)> {
        let path = path.to_path_buf();
        let (sender, mut receiver) = mpsc::channel::<Job>(32);
        let (publisher, publications) = mpsc::channel::<Publication>(1024);
        let (ready, opened) = oneshot::channel();
        std::thread::Builder::new()
            .name("agent-storage".into())
            .spawn(move || {
                let opened: Result<(Database, File, std::path::PathBuf)> = (|| {
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
                                .join(path.file_name().ok_or(Error::new("invalid_store_path"))?)
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
                        .map_err(|_| Error::new("store_already_owned"))?;
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
                    let db = Database::initialize(Connection::open(&path)?)?;
                    Ok((db, lock, path))
                })();
                match opened {
                    Ok((mut db, _lock, path)) => {
                        let mut watermark = match db.last_event_id() {
                            Ok(id) => id,
                            Err(error) => {
                                let _ = ready.send(Err(error));
                                return;
                            }
                        };
                        let _ = ready.send(Ok(path));
                        while let Some(job) = receiver.blocking_recv() {
                            job(&mut db);
                            // A storage error here has no caller to answer;
                            // the next job's read reports it.
                            let _ = db.publish_since(&mut watermark, |publication| {
                                publisher.blocking_send(publication).is_ok()
                            });
                        }
                    }
                    Err(error) => {
                        let _ = ready.send(Err(error));
                    }
                }
            })?;
        let store_path = opened
            .await
            .map_err(|_| Error::new("storage_worker_failed"))??;
        Ok((
            Self {
                sender,
                path: std::sync::Arc::new(store_path),
                counters: std::sync::Arc::default(),
            },
            publications,
        ))
    }

    /// Worker counters and on-disk size, for `stats`.
    pub fn stats(&self) -> serde_json::Value {
        use std::sync::atomic::Ordering::Relaxed;
        let size = |suffix: &str| {
            let mut name = self.path.as_os_str().to_owned();
            name.push(suffix);
            std::fs::metadata(name).map(|m| m.len()).unwrap_or(0)
        };
        serde_json::json!({
            "bytes": size(""),
            "wal_bytes": size("-wal"),
            "jobs": self.counters.jobs.load(Relaxed),
            "queued_ms": self.counters.queued_ns.load(Relaxed) / 1_000_000,
            "ran_ms": self.counters.ran_ns.load(Relaxed) / 1_000_000,
        })
    }

    pub async fn call<T: Send + 'static>(
        &self,
        operation: impl FnOnce(&mut Database) -> Result<T> + Send + 'static,
    ) -> Result<T> {
        use std::sync::atomic::Ordering::Relaxed;
        let (sender, receiver) = oneshot::channel();
        let counters = self.counters.clone();
        let queued = std::time::Instant::now();
        self.sender
            .send(Box::new(move |db| {
                let started = std::time::Instant::now();
                counters
                    .queued_ns
                    .fetch_add((started - queued).as_nanos() as u64, Relaxed);
                let _ = sender.send(operation(db));
                counters
                    .ran_ns
                    .fetch_add(started.elapsed().as_nanos() as u64, Relaxed);
                counters.jobs.fetch_add(1, Relaxed);
            }))
            .await
            .map_err(|_| Error::new("storage_worker_failed"))?;
        receiver
            .await
            .map_err(|_| Error::new("storage_worker_failed"))?
    }
}
