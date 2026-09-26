//! One database worker for all agents, plus one reader for bulk context
//! reads. Durable writes never block the I/O runtime, and jobs that queue
//! together commit together: one sync per group, not per job. Retention jobs
//! for the same bot cross a publication boundary before they can prune each other.
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
pub use context::{ContextPrefix, ContextUsage, pinned_item, thinking_bytes, without_thinking};
pub use db::{
    Absorbed, Binding, Bot, CatchUp, CompactionPlan, CompactionView, Database, Delivery,
    ElisionPlan, Fork, Planning, Publication, Started, Strip, TurnContext, TurnOptions, Waiting,
    Window, cache_hit,
};

type ReadJob = Box<dyn FnOnce(&Database) + Send>;
/// Most jobs one commit carries: one full queue. Under load the worker runs
/// everything already queued, up to this many, inside one transaction and
/// syncs once for all of them; idle, a group is one job and nothing waits
/// for company. The bound caps how long the group's first job waits on the
/// ones behind it.
const GROUP_JOBS: usize = 32;
/// A storage job: run inside its group's transaction, then answered once
/// that group's commit is known, so no caller hears of a write before it
/// is durable.
trait Job: Send {
    fn run(&mut self, db: &mut Database);
    fn pruning_bot(&self) -> Option<&str>;
    fn error(&self) -> Option<&Error>;
    fn answer(self: Box<Self>, failure: Option<&Error>);
}
struct Queued<F, T> {
    operation: Option<F>,
    pruning_bot: Option<String>,
    outcome: Option<Result<T>>,
    reply: oneshot::Sender<Result<T>>,
    label: &'static str,
    queued: std::time::Instant,
    counters: std::sync::Arc<Counters>,
}
impl<F, T> Job for Queued<F, T>
where
    F: FnOnce(&mut Database) -> Result<T> + Send,
    T: Send,
{
    fn run(&mut self, db: &mut Database) {
        let started = std::time::Instant::now();
        if let Some(operation) = self.operation.take() {
            self.outcome = Some(operation(db));
        }
        self.counters.record(
            self.label,
            (started - self.queued).as_nanos() as u64,
            started.elapsed().as_nanos() as u64,
        );
    }
    fn pruning_bot(&self) -> Option<&str> {
        self.pruning_bot.as_deref()
    }
    fn error(&self) -> Option<&Error> {
        self.outcome
            .as_ref()
            .and_then(|outcome| outcome.as_ref().err())
    }
    fn answer(self: Box<Self>, failure: Option<&Error>) {
        // A job's own error was decided against writes that the failed
        // commit took back, so it no longer describes the store.
        let outcome = match (failure, self.outcome) {
            (Some(error), _) => Err(error.clone()),
            (None, Some(outcome)) => outcome,
            _ => Err(Error::new("storage_error")),
        };
        let _ = self.reply.send(outcome);
    }
}
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
    sender: mpsc::Sender<Box<dyn Job>>,
    reader: mpsc::Sender<ReadJob>,
    path: std::sync::Arc<std::path::PathBuf>,
    counters: std::sync::Arc<Counters>,
}

impl From<rusqlite::Error> for Error {
    fn from(error: rusqlite::Error) -> Self {
        // SQLite's message and other rusqlite variants can contain SQL,
        // paths or caller data. Numeric engine codes are safe diagnostics.
        match error {
            rusqlite::Error::SqliteFailure(code, _)
            | rusqlite::Error::SqlInputError { error: code, .. } => Self::with(
                "storage_error",
                format!(
                    "sqlite_primary={} sqlite_extended={}",
                    code.extended_code & 255,
                    code.extended_code
                ),
            ),
            _ => Self::new("storage_error"),
        }
    }
}

impl Store {
    /// Cache namespace for this physical store file. The persisted lineage
    /// survives a copy; the file identity keeps live copies from sharing a
    /// provider cache lane while preserving affinity across daemon restarts.
    pub fn instance_identity(&self, lineage: i64) -> Result<u128> {
        #[cfg(unix)]
        {
            use std::os::unix::fs::MetadataExt;
            let metadata = std::fs::metadata(self.path.as_path())?;
            let mut input = [0u8; 24];
            input[..8].copy_from_slice(&lineage.to_be_bytes());
            input[8..16].copy_from_slice(&metadata.dev().to_be_bytes());
            input[16..].copy_from_slice(&metadata.ino().to_be_bytes());
            let hash = aws_lc_rs::digest::digest(&aws_lc_rs::digest::SHA256, &input);
            Ok(u128::from_be_bytes(hash.as_ref()[..16].try_into().unwrap()))
        }
        #[cfg(not(unix))]
        {
            let _ = lineage;
            crate::fail("store_platform_unsupported")
        }
    }

    /// Open the store and its publication stream. The worker publishes
    /// what each group of jobs committed, in commit order, before taking
    /// the next group; the stream is bounded, so a publisher that stops
    /// reading eventually holds the worker, never memory.
    pub async fn open(path: &Path) -> Result<(Self, mpsc::Receiver<Publication>)> {
        let path = path.to_path_buf();
        let (sender, mut receiver) = mpsc::channel::<Box<dyn Job>>(GROUP_JOBS);
        let counters = std::sync::Arc::<Counters>::default();
        let worker_counters = counters.clone();
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
                        let mut group: Vec<Box<dyn Job>> = Vec::with_capacity(GROUP_JOBS);
                        let mut deferred = None;
                        while let Some(job) = deferred.take().or_else(|| receiver.blocking_recv()) {
                            group.push(job);
                            let begun = db.begin_group();
                            if begun.is_ok() {
                                let mut ran = 0;
                                // Whatever queued while the last group
                                // synced shares this one's sync.
                                loop {
                                    group[ran].run(&mut db);
                                    ran += 1;
                                    if !db.in_group() || group.len() == GROUP_JOBS {
                                        break;
                                    }
                                    match receiver.try_recv() {
                                        Ok(job) => {
                                            // Retention must see prior same-bot completions
                                            // only after their events have been published.
                                            // The group is bounded; no map or SQL lookup is
                                            // needed, and independent bots still batch.
                                            if job.pruning_bot().is_some_and(|bot| {
                                                group
                                                    .iter()
                                                    .any(|prior| prior.pruning_bot() == Some(bot))
                                            }) {
                                                deferred = Some(job);
                                                break;
                                            }
                                            group.push(job);
                                        }
                                        Err(_) => break,
                                    }
                                }
                            }
                            let committed = begun.and_then(|()| {
                                let started = std::time::Instant::now();
                                let committed = db.commit_group();
                                worker_counters.record(
                                    "commit",
                                    0,
                                    started.elapsed().as_nanos() as u64,
                                );
                                committed
                            });
                            let failure = committed.err().map(|error| {
                                // A statement can make SQLite roll back the entire
                                // transaction. Preserve that statement's error,
                                // not just the subsequent missing-transaction check.
                                let error = if error.code == "storage_group_rolled_back" {
                                    group
                                        .last()
                                        .and_then(|job| job.error())
                                        .cloned()
                                        .unwrap_or(error)
                                } else {
                                    error
                                };
                                if error.code == "storage_error" {
                                    error
                                } else {
                                    Error::new("storage_error")
                                }
                            });
                            // A group that did not commit leaves nothing
                            // durable; its bookkeeping goes with it. If the
                            // recount fails too, the next group retries it
                            // before running anything.
                            if failure.is_some() {
                                let _ = db.abandon_group();
                            }
                            for job in group.drain(..) {
                                job.answer(failure.as_ref());
                            }
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
        // schema exist, and it sees a job's writes once that job is answered.
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
                counters,
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
        self.enqueue(label, None, operation).await
    }
    /// Run a job that can prune a bot's events. Jobs for different bots may
    /// share a commit; another pruning job for this bot must follow publication
    /// of the first, so it cannot erase a terminal event before live delivery.
    /// Completion jobs use this even without automatic retention: a later
    /// explicit prune or deletion must preserve their publication too.
    pub async fn op_pruning<T: Send + 'static>(
        &self,
        label: &'static str,
        bot: String,
        operation: impl FnOnce(&mut Database) -> Result<T> + Send + 'static,
    ) -> Result<T> {
        self.enqueue(label, Some(bot), operation).await
    }
    async fn enqueue<T: Send + 'static>(
        &self,
        label: &'static str,
        pruning_bot: Option<String>,
        operation: impl FnOnce(&mut Database) -> Result<T> + Send + 'static,
    ) -> Result<T> {
        let (reply, receiver) = oneshot::channel();
        self.sender
            .send(Box::new(Queued {
                operation: Some(operation),
                pruning_bot,
                outcome: None,
                reply,
                label,
                queued: std::time::Instant::now(),
                counters: self.counters.clone(),
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

    fn scratch_path(name: &str) -> std::path::PathBuf {
        std::env::temp_dir()
            .join(format!("agent-group-{name}-{}", std::process::id()))
            .join("state.sqlite")
    }
    async fn scratch_store(name: &str) -> Store {
        let path = scratch_path(name);
        let _ = std::fs::remove_dir_all(path.parent().unwrap());
        let (store, _publications) = Store::open(&path).await.unwrap();
        store
            .call(|db| Ok(db.connection().execute_batch("CREATE TABLE t(x INTEGER)")?))
            .await
            .unwrap();
        store
    }
    /// Hold the worker inside a job until the returned gate is dropped or
    /// fed, so the jobs queued meanwhile run as one group behind it.
    async fn hold(
        store: &Store,
    ) -> (
        tokio::task::JoinHandle<Result<()>>,
        std::sync::mpsc::Sender<()>,
    ) {
        let (gate, wait) = std::sync::mpsc::channel::<()>();
        let (entered, inside) = oneshot::channel();
        let store = store.clone();
        let held = tokio::spawn(async move {
            store
                .call(move |_| {
                    let _ = entered.send(());
                    let _ = wait.recv();
                    Ok(())
                })
                .await
        });
        inside.await.unwrap();
        (held, gate)
    }
    /// Queue a job and return once it waits in the worker's channel.
    async fn queue<T: Send + 'static>(
        store: &Store,
        operation: impl FnOnce(&mut Database) -> Result<T> + Send + 'static,
    ) -> tokio::task::JoinHandle<Result<T>> {
        queue_pruning(store, None, operation).await
    }
    async fn queue_pruning<T: Send + 'static>(
        store: &Store,
        bot: Option<&str>,
        operation: impl FnOnce(&mut Database) -> Result<T> + Send + 'static,
    ) -> tokio::task::JoinHandle<Result<T>> {
        let bot = bot.map(str::to_owned);
        let queued = store.sender.max_capacity() - store.sender.capacity() + 1;
        let job = tokio::spawn({
            let store = store.clone();
            async move { store.enqueue("test", bot, operation).await }
        });
        while store.sender.max_capacity() - store.sender.capacity() < queued {
            tokio::task::yield_now().await;
        }
        job
    }
    fn insert(x: i64) -> impl FnOnce(&mut Database) -> Result<()> + Send + 'static {
        move |db| {
            db.connection().execute("INSERT INTO t VALUES (?)", [x])?;
            Ok(())
        }
    }
    async fn rows(store: &Store) -> Vec<i64> {
        store
            .call(|db| {
                let mut statement = db.connection().prepare("SELECT x FROM t ORDER BY x")?;
                let rows = statement.query_map([], |r| r.get(0))?;
                Ok(rows.collect::<rusqlite::Result<Vec<i64>>>()?)
            })
            .await
            .unwrap()
    }
    fn commits(store: &Store) -> u64 {
        store.stats()["operations"]["commit"]["count"]
            .as_u64()
            .unwrap()
    }

    #[tokio::test]
    async fn jobs_queued_together_share_one_commit_and_fail_alone() {
        let store = scratch_store("share").await;
        let before = commits(&store);
        let (held, gate) = hold(&store).await;
        let one = queue(&store, insert(1)).await;
        // A job whose own transaction fails rolls back alone.
        let failed = queue(&store, |db| {
            let tx = db.connection().savepoint()?;
            tx.execute("INSERT INTO t VALUES (2)", [])?;
            crate::fail::<()>("job_failed")
        })
        .await;
        let three = queue(&store, insert(3)).await;
        gate.send(()).unwrap();
        held.await.unwrap().unwrap();
        one.await.unwrap().unwrap();
        assert_eq!(failed.await.unwrap().unwrap_err().code, "job_failed");
        three.await.unwrap().unwrap();
        assert_eq!(commits(&store), before + 1, "four jobs, one commit");
        assert_eq!(rows(&store).await, [1, 3]);
    }

    #[tokio::test]
    async fn same_bot_retention_publishes_before_pruning_without_serializing_other_bots() {
        let path = scratch_path("retention-publication");
        let _ = std::fs::remove_dir_all(path.parent().unwrap());
        let (store, mut publications) = Store::open(&path).await.unwrap();
        let (first, queued, other) = store
            .call(|db| {
                for name in ["Bob", "Alice"] {
                    db.create(
                        name,
                        Some("/synthetic"),
                        Binding {
                            provider: "openai",
                            family: crate::codec::Family::Responses,
                            model: "synthetic",
                            instructions: "",
                            reasoning: None,
                            budget_tokens: None,
                            tools: &[],
                            created_by: None,
                            created_by_id: None,
                            compaction_instructions: None,
                            compaction_model: None,
                            fallbacks: false,
                        },
                    )?;
                }
                let first = db
                    .begin("Bob", "a", "work", true, &TurnOptions::default(), |_, _| {
                        Ok(())
                    })?
                    .turn;
                let queued = db
                    .begin(
                        "Bob",
                        "b",
                        "next",
                        true,
                        &TurnOptions {
                            delivery: Delivery::Queue,
                            ..TurnOptions::default()
                        },
                        |_, _| Ok(()),
                    )?
                    .turn;
                let other = db
                    .begin(
                        "Alice",
                        "c",
                        "work",
                        true,
                        &TurnOptions::default(),
                        |_, _| Ok(()),
                    )?
                    .turn;
                Ok((first, queued, other))
            })
            .await
            .unwrap();
        let before = commits(&store);
        let (held, gate) = hold(&store).await;
        let finish = queue_pruning(&store, Some("Bob"), move |db| {
            db.finish(first, None)?;
            db.announce("Bob", first, db.turn_outcome("Bob", first)?.unwrap());
            db.prune_except("Bob", 1, Some(first))?;
            Ok(())
        })
        .await;
        let independent = queue_pruning(&store, Some("Alice"), move |db| {
            db.finish(other, None)?;
            db.prune_except("Alice", 1, Some(other))?;
            Ok(())
        })
        .await;
        let cancel = queue_pruning(&store, Some("Bob"), move |db| {
            db.end_queued(queued, &Error::new("cancelled"))?;
            db.prune_except("Bob", 1, Some(queued))?;
            Ok(())
        })
        .await;
        gate.send(()).unwrap();
        for job in [held, finish, independent, cancel] {
            job.await.unwrap().unwrap();
        }
        // This read also waits for the preceding group's publication to drain.
        assert_eq!(
            store
                .call(move |db| db.turn_outcome("Bob", first))
                .await
                .unwrap_err()
                .code,
            "turn_result_pruned"
        );
        let mut terminal = Vec::new();
        while let Ok(publication) = publications.try_recv() {
            if let Publication::Event(event) = publication
                && event["event"] == "turn_finished"
            {
                terminal.push(event["turn"].as_i64().unwrap());
            }
        }
        assert_eq!(
            terminal,
            [first, other, queued],
            "pruning must not erase live completion"
        );
        assert_eq!(
            commits(&store),
            before + 3,
            "two mutation groups plus the final read; independent finishes share a commit"
        );
    }

    #[tokio::test]
    async fn sqlite_diagnostics_survive_commit_and_transaction_rollback() {
        for (name, setup, write, extended) in [
            (
                "diagnostic-commit",
                "CREATE TABLE parent(x PRIMARY KEY); CREATE TABLE child(x REFERENCES parent(x) DEFERRABLE INITIALLY DEFERRED);",
                "INSERT INTO child VALUES (9)",
                rusqlite::ffi::SQLITE_CONSTRAINT_FOREIGNKEY,
            ),
            (
                "diagnostic-rollback",
                "CREATE TRIGGER reject_two BEFORE INSERT ON t WHEN NEW.x=2 BEGIN SELECT RAISE(ROLLBACK,'private SQL message'); END;",
                "INSERT INTO t VALUES (2)",
                rusqlite::ffi::SQLITE_CONSTRAINT_TRIGGER,
            ),
        ] {
            let store = scratch_store(name).await;
            store
                .call(move |db| Ok(db.connection().execute_batch(setup)?))
                .await
                .unwrap();
            let (held, gate) = hold(&store).await;
            let one = queue(&store, insert(1)).await;
            let refused = queue(&store, |_| crate::fail::<()>("bot_busy")).await;
            let failed = queue(&store, move |db| {
                Ok(db.connection().execute_batch(write)?)
            })
            .await;
            gate.send(()).unwrap();
            for job in [held, one, refused, failed] {
                let error = job.await.unwrap().unwrap_err();
                assert_eq!(error.code, "storage_error");
                assert_eq!(
                    error.detail,
                    Some(format!("sqlite_primary=19 sqlite_extended={extended}"))
                );
            }
            assert!(rows(&store).await.is_empty());
            store.call(insert(3)).await.unwrap();
            assert_eq!(rows(&store).await, [3]);
        }
    }

    #[test]
    fn sqlite_diagnostics_exclude_messages_and_sql() {
        for code in [
            rusqlite::ffi::SQLITE_FULL,
            rusqlite::ffi::SQLITE_IOERR_FSYNC,
            rusqlite::ffi::SQLITE_BUSY,
        ] {
            let error: Error = rusqlite::Error::SqliteFailure(
                rusqlite::ffi::Error::new(code),
                Some("private SQL, path and message".into()),
            )
            .into();
            assert_eq!(error.code, "storage_error");
            assert_eq!(
                error.detail,
                Some(format!(
                    "sqlite_primary={} sqlite_extended={code}",
                    code & 255
                ))
            );
        }
        let error: Error = rusqlite::Error::InvalidParameterName("private parameter".into()).into();
        assert_eq!(error, Error::new("storage_error"));
        let error: Error = rusqlite::Error::SqlInputError {
            error: rusqlite::ffi::Error::new(rusqlite::ffi::SQLITE_ERROR),
            msg: "private message".into(),
            sql: "private SQL".into(),
            offset: 1,
        }
        .into();
        assert_eq!(
            error.detail.as_deref(),
            Some("sqlite_primary=1 sqlite_extended=1")
        );
    }

    #[tokio::test]
    async fn a_group_rolled_back_under_its_jobs_answers_none_of_them_ok() {
        let store = scratch_store("rollback").await;
        let (held, gate) = hold(&store).await;
        let one = queue(&store, insert(1)).await;
        // A refusal decided against writes the group then loses no longer
        // describes the store either.
        let refused = queue(&store, |_| crate::fail::<()>("bot_busy")).await;
        // What SQLite does to the whole transaction on a full disk or an
        // I/O error: nothing the group ran may be reported as written.
        let lost = queue(&store, |db| {
            db.connection().execute_batch("ROLLBACK")?;
            Ok(())
        })
        .await;
        gate.send(()).unwrap();
        for answer in [
            held.await.unwrap(),
            one.await.unwrap(),
            refused.await.unwrap(),
            lost.await.unwrap(),
        ] {
            assert_eq!(answer.unwrap_err().code, "storage_error");
        }
        assert!(rows(&store).await.is_empty());
        // The worker recovers: the next group commits normally.
        store.call(insert(4)).await.unwrap();
        assert_eq!(rows(&store).await, [4]);
    }

    #[tokio::test]
    async fn no_job_runs_until_a_failed_recount_succeeds() {
        let store = scratch_store("recount").await;
        let waiting = store
            .call(|db| {
                db.create(
                    "Bob",
                    Some("/synthetic"),
                    Binding {
                        provider: "openai",
                        family: crate::codec::Family::Responses,
                        model: "synthetic-model",
                        instructions: "test",
                        reasoning: None,
                        budget_tokens: None,
                        tools: &[],
                        created_by: None,
                        created_by_id: None,
                        compaction_instructions: None,
                        compaction_model: None,
                        fallbacks: false,
                    },
                )?;
                let options = TurnOptions {
                    delivery: Delivery::Queue,
                    ..TurnOptions::default()
                };
                for (id, prompt) in [("first", "work"), ("second", "more")] {
                    db.begin("Bob", id, prompt, true, &options, |_, _| Ok(()))?;
                }
                db.pending()
            })
            .await
            .unwrap();
        assert_eq!(waiting, (1, 4));
        // The group is lost, and its recount cannot read the turns.
        let lost = store
            .call(|db| {
                db.connection()
                    .execute_batch("ROLLBACK; ALTER TABLE turns RENAME TO turns_away;")?;
                Ok(())
            })
            .await;
        assert_eq!(lost.unwrap_err().code, "storage_error");
        let refused = store.call(|db| db.pending()).await;
        let refused = refused.unwrap_err();
        assert_eq!(refused.code, "storage_error");
        assert_eq!(
            refused.detail.as_deref(),
            Some("sqlite_primary=1 sqlite_extended=1")
        );
        Connection::open(scratch_path("recount"))
            .unwrap()
            .execute_batch("ALTER TABLE turns_away RENAME TO turns;")
            .unwrap();
        assert_eq!(store.call(|db| db.pending()).await.unwrap(), (1, 4));
    }

    #[tokio::test]
    async fn both_connections_flush_the_drive_when_they_sync() {
        let store = scratch_store("fullfsync").await;
        let writer = store
            .call(|db| Ok((db.pragma("fullfsync"), db.pragma("checkpoint_fullfsync"))))
            .await
            .unwrap();
        assert_eq!(writer, (1, 1));
        let reader = store
            .read("pragma", |db| Ok(db.pragma("checkpoint_fullfsync")))
            .await
            .unwrap();
        assert_eq!(reader, 1);
    }

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
