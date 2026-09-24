//! One database worker for all agents, plus one reader for bulk context
//! reads. Durable writes never block the I/O runtime.
use crate::{Error, Result};
use rusqlite::Connection;
use std::{
    fs::{File, OpenOptions},
    path::Path,
};
use tokio::sync::{mpsc, oneshot};

mod artifact;
mod context;
mod db;
pub use context::{ContextPrefix, ContextUsage, pinned_item};
pub use db::{
    Absorbed, Binding, Bot, CatchUp, CompactionPlan, CompactionView, Database, Delivery, Fork,
    Planning, Publication, Started, TurnContext, TurnOptions, Waiting, Window, cache_hit,
};

type Job = Box<dyn FnOnce(&mut Database) + Send>;
type ReadJob = Box<dyn FnOnce(&Database) + Send>;
/// Storage worker counters: how long jobs queued for the worker versus how
/// long they ran on it, in total and per operation. The split says whether
/// the worker or the disk is the bottleneck; the per-operation histograms
/// say which jobs make the tail. Three clock reads and one short lock per
/// job; no allocation once an operation has been seen.
#[derive(Default)]
pub struct Counters {
    operations: std::sync::Mutex<std::collections::HashMap<&'static str, Operation>>,
}
/// Upper bounds of the latency buckets, in microseconds; the last bucket is
/// everything above the last bound. Log-spaced, so a histogram of fourteen
/// counters covers a microsecond read and a second-long retention pass.
pub const BUCKETS_US: [u64; 13] = [
    100, 250, 500, 1_000, 2_500, 5_000, 10_000, 25_000, 50_000, 100_000, 250_000, 500_000,
    1_000_000,
];
#[derive(Default, Clone)]
struct Operation {
    count: u64,
    queued_ns: u64,
    ran_ns: u64,
    slowest_ns: u64,
    ran: [u64; BUCKETS_US.len() + 1],
    queued: [u64; BUCKETS_US.len() + 1],
}
fn bucket(ns: u64) -> usize {
    let us = ns / 1_000;
    BUCKETS_US
        .iter()
        .position(|bound| us < *bound)
        .unwrap_or(BUCKETS_US.len())
}
impl Counters {
    fn record(&self, label: &'static str, queued_ns: u64, ran_ns: u64) {
        let mut operations = self.operations.lock().unwrap();
        let operation = operations.entry(label).or_default();
        operation.count += 1;
        operation.queued_ns += queued_ns;
        operation.ran_ns += ran_ns;
        operation.slowest_ns = operation.slowest_ns.max(ran_ns);
        operation.ran[bucket(ran_ns)] += 1;
        operation.queued[bucket(queued_ns)] += 1;
    }
    fn snapshot(&self) -> serde_json::Value {
        // Copy the small fixed-size records while locked; JSON construction
        // and aggregate sums cannot stall the worker or observe later writes.
        let operations = self.operations.lock().unwrap().clone();
        let (mut jobs, mut queued_ns, mut ran_ns) = (0u64, 0u64, 0u64);
        let mut out = serde_json::Map::new();
        for (label, o) in operations.iter() {
            jobs += o.count;
            queued_ns += o.queued_ns;
            ran_ns += o.ran_ns;
            out.insert(
                (*label).to_owned(),
                serde_json::json!({"count": o.count, "queued_ms": o.queued_ns / 1_000_000,
                    "ran_ms": o.ran_ns / 1_000_000, "slowest_ms": o.slowest_ns / 1_000_000,
                    "ran": o.ran, "queued": o.queued}),
            );
        }
        serde_json::json!({
            "jobs": jobs,
            "queued_ms": queued_ns / 1_000_000,
            "ran_ms": ran_ns / 1_000_000,
            "buckets_us": BUCKETS_US,
            "operations": out,
        })
    }
}
#[derive(Clone)]
pub struct Store {
    sender: mpsc::Sender<Job>,
    reader: mpsc::Sender<ReadJob>,
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
        // A second connection on its own thread for reads that carry bytes
        // rather than decide anything: streaming a context window out of
        // the store must not hold every other bot's commit behind it. It
        // opens after the worker, so the file, its WAL, and the current
        // schema exist, and it sees each job's commit once that job is done.
        let (reader, mut reads) = mpsc::channel::<ReadJob>(32);
        let (ready, opened) = oneshot::channel();
        let reader_path = store_path.clone();
        std::thread::Builder::new()
            .name("agent-storage-reader".into())
            .spawn(move || {
                let db = Connection::open(&reader_path)
                    .map_err(Error::from)
                    .and_then(Database::reader);
                match db {
                    Ok(db) => {
                        let _ = ready.send(Ok(()));
                        while let Some(job) = reads.blocking_recv() {
                            job(&db);
                        }
                    }
                    Err(error) => {
                        let _ = ready.send(Err(error));
                    }
                }
            })?;
        opened
            .await
            .map_err(|_| Error::new("storage_worker_failed"))??;
        Ok((
            Self {
                sender,
                reader,
                path: std::sync::Arc::new(store_path),
                counters: std::sync::Arc::default(),
            },
            publications,
        ))
    }

    /// Worker counters and on-disk size, for `stats`.
    pub fn stats(&self) -> serde_json::Value {
        let size = |suffix: &str| {
            let mut name = self.path.as_os_str().to_owned();
            name.push(suffix);
            std::fs::metadata(name).map(|m| m.len()).unwrap_or(0)
        };
        let mut stats = self.counters.snapshot();
        stats["bytes"] = size("").into();
        stats["wal_bytes"] = size("-wal").into();
        stats
    }

    /// Run a job on the storage worker, counted under `label`: the store
    /// method it performs, as `stats` reports it.
    pub async fn op<T: Send + 'static>(
        &self,
        label: &'static str,
        operation: impl FnOnce(&mut Database) -> Result<T> + Send + 'static,
    ) -> Result<T> {
        let (sender, receiver) = oneshot::channel();
        let counters = self.counters.clone();
        let queued = std::time::Instant::now();
        self.sender
            .send(Box::new(move |db| {
                let started = std::time::Instant::now();
                let _ = sender.send(operation(db));
                counters.record(
                    label,
                    (started - queued).as_nanos() as u64,
                    started.elapsed().as_nanos() as u64,
                );
            }))
            .await
            .map_err(|_| Error::new("storage_worker_failed"))?;
        receiver
            .await
            .map_err(|_| Error::new("storage_worker_failed"))?
    }
    /// Run a read on the reader connection, counted like any job. Only for
    /// reads whose result is bytes for a caller, never for decisions that
    /// must see the write the caller is about to make.
    pub async fn read<T: Send + 'static>(
        &self,
        label: &'static str,
        operation: impl FnOnce(&Database) -> Result<T> + Send + 'static,
    ) -> Result<T> {
        let (sender, receiver) = oneshot::channel();
        let counters = self.counters.clone();
        let queued = std::time::Instant::now();
        self.reader
            .send(Box::new(move |db| {
                let started = std::time::Instant::now();
                let _ = sender.send(operation(db));
                counters.record(
                    label,
                    (started - queued).as_nanos() as u64,
                    started.elapsed().as_nanos() as u64,
                );
            }))
            .await
            .map_err(|_| Error::new("storage_worker_failed"))?;
        receiver
            .await
            .map_err(|_| Error::new("storage_worker_failed"))?
    }
    /// A job without a named operation, counted as `other`.
    pub async fn call<T: Send + 'static>(
        &self,
        operation: impl FnOnce(&mut Database) -> Result<T> + Send + 'static,
    ) -> Result<T> {
        self.op("other", operation).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{Arc, Barrier};

    #[test]
    fn live_counter_snapshots_reconcile_totals_and_histograms() {
        let (sender, _receiver) = mpsc::channel(1);
        let (reader, _reads) = mpsc::channel(1);
        let store = Store {
            sender,
            reader,
            path: Arc::new(std::path::PathBuf::new()),
            counters: Arc::default(),
        };
        let start = Arc::new(Barrier::new(2));
        std::thread::scope(|scope| {
            let counters = store.counters.clone();
            let ready = start.clone();
            scope.spawn(move || {
                ready.wait();
                for index in 0..100_000 {
                    counters.record(
                        if index % 2 == 0 { "read" } else { "write" },
                        1_000_000,
                        2_000_000,
                    );
                }
            });
            start.wait();
            for _ in 0..2_000 {
                let stats = store.stats();
                let operations = stats["operations"].as_object().unwrap();
                for (total, field) in [
                    ("jobs", "count"),
                    ("queued_ms", "queued_ms"),
                    ("ran_ms", "ran_ms"),
                ] {
                    let sum: u64 = operations
                        .values()
                        .map(|o| o[field].as_u64().unwrap())
                        .sum();
                    assert_eq!(stats[total].as_u64().unwrap(), sum, "{total}");
                }
                for operation in operations.values() {
                    for histogram in ["ran", "queued"] {
                        let sum: u64 = operation[histogram]
                            .as_array()
                            .unwrap()
                            .iter()
                            .map(|n| n.as_u64().unwrap())
                            .sum();
                        assert_eq!(sum, operation["count"].as_u64().unwrap());
                    }
                }
            }
        });
        assert_eq!(store.stats()["jobs"], 100_000);
    }
}
