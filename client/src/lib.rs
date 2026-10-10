//! The daemon's socket protocol for human-facing clients: one session,
//! JSONL requests correlated by `id`, notifications forwarded on a channel.
//! Shared by the terminal client and the desktop app; nothing here depends
//! on the runtime crate.
pub mod approver;
pub mod models;
pub mod policy;
pub mod socket;

use serde_json::{Value, json};
use std::{collections::HashMap, os::fd::AsRawFd, path::Path, sync::Arc, time::Duration};
use tokio::{
    io::{AsyncBufReadExt, AsyncWriteExt, BufReader},
    net::{UnixStream, unix::OwnedWriteHalf},
    sync::{Mutex, OwnedSemaphorePermit, Semaphore, mpsc, oneshot},
};

#[derive(Debug, Clone)]
pub struct Error {
    pub code: String,
    pub detail: Option<String>,
    /// Facts the daemon sent beside `detail`, such as `running_turn` and
    /// `fork_point` on `bot_busy`, so a caller need not parse the detail or
    /// ask again.
    pub facts: Option<Box<serde_json::Map<String, Value>>>,
}

impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match &self.detail {
            Some(detail) => write!(f, "{} ({detail})", self.code),
            None => f.write_str(&self.code),
        }
    }
}
impl Error {
    pub fn new(code: &str) -> Self {
        Self {
            code: code.to_owned(),
            detail: None,
            facts: None,
        }
    }
    pub fn with(code: &str, detail: &str) -> Self {
        Self {
            detail: Some(detail.to_owned()),
            ..Self::new(code)
        }
    }
}
impl From<std::io::Error> for Error {
    fn from(error: std::io::Error) -> Self {
        Self::with("io", &error.to_string())
    }
}
impl From<serde_json::Error> for Error {
    fn from(error: serde_json::Error) -> Self {
        Self::with("invalid_json", &error.to_string())
    }
}
pub type Result<T> = std::result::Result<T, Error>;

/// The socket protocol's version in `ready`. The daemon and its clients ship
/// together, so a client refuses a daemon of any other version.
pub const PROTOCOL: u64 = 11;

// No await occurs while this lock is held. Synchronous removal makes dropping
// a request release its registration immediately, without a cleanup task.
type Pending = Arc<std::sync::Mutex<HashMap<u64, oneshot::Sender<Value>>>>;
struct PendingRequest<'a> {
    pending: &'a Pending,
    id: u64,
}
impl Drop for PendingRequest<'_> {
    fn drop(&mut self) {
        self.pending.lock().unwrap().remove(&self.id);
    }
}

// Dropping write_all may leave a partial JSONL frame on the socket. Shut down
// both halves before releasing the writer lock, so nobody appends a new frame
// to that prefix and the reader releases other pending requests.
struct FrameWrite<'a> {
    fd: std::os::fd::RawFd,
    closed: &'a std::sync::atomic::AtomicBool,
    complete: bool,
}
impl Drop for FrameWrite<'_> {
    fn drop(&mut self) {
        if !self.complete {
            self.closed.store(true, std::sync::atomic::Ordering::SeqCst);
            // SAFETY: request holds the client and writer lock until after this
            // guard drops, so fd still names this socket. shutdown does not close fd.
            unsafe { libc::shutdown(self.fd, libc::SHUT_RDWR) };
        }
    }
}

/// Notifications queued ahead of the UI before the session is let go.
/// Larger than any replay page, so a normal attach never trips it.
const QUEUE: usize = 4096;
const QUEUE_BYTES: usize = 8 * 1024 * 1024;

struct Notification {
    encoded: String,
    _bytes: Option<OwnedSemaphorePermit>,
}

/// Encoded notifications share an 8 MiB budget, released as callers drain them.
/// Keeping the wire representation also bounds allocations for nested JSON.
pub struct Events(mpsc::Receiver<Notification>);
impl Events {
    pub async fn recv(&mut self) -> Option<Value> {
        self.0
            .recv()
            .await
            .map(|event| serde_json::from_str(&event.encoded).expect("validated notification"))
    }
    pub fn try_recv(&mut self) -> std::result::Result<Value, mpsc::error::TryRecvError> {
        self.0
            .try_recv()
            .map(|event| serde_json::from_str(&event.encoded).expect("validated notification"))
    }
}

pub struct Client {
    writer: Mutex<OwnedWriteHalf>,
    pending: Pending,
    next: std::sync::atomic::AtomicU64,
    /// Set by the reader on its way out. A request made after it can still
    /// be written, but nothing would ever answer; it fails here instead.
    closed: Arc<std::sync::atomic::AtomicBool>,
    /// The store's identity from `ready`: the same file under the same
    /// lineage, whichever socket its daemon listens on.
    store: Option<String>,
}

impl Client {
    /// Connect and wait for the daemon's `ready` line. Notifications (lines
    /// without an `id`) go to the returned receiver; a closed receiver means
    /// the daemon hung up.
    ///
    /// The reader never waits for the UI: a response the UI is awaiting must
    /// not sit behind notifications it has not drained, or both deadlock.
    /// The queue is bounded all the same, so a UI slower than the fleet
    /// cannot grow it without limit. When it fills, the reader does what the
    /// daemon does to a lagging follower: it drops the session and tells the
    /// UI `follow_lagged`, and the UI attaches again from its cursor.
    pub async fn connect(socket: &Path) -> Result<(Arc<Self>, Events)> {
        let stream = tokio::time::timeout(Duration::from_secs(5), UnixStream::connect(socket))
            .await
            .map_err(|_| Error::new("daemon_connect_timeout"))?
            .map_err(|error| {
                Error::with(
                    "daemon_unavailable",
                    &format!("{}: {error}", socket.display()),
                )
            })?;
        let (read, write) = stream.into_split();
        let mut lines = BufReader::new(read).lines();
        let ready = match tokio::time::timeout(Duration::from_secs(5), lines.next_line())
            .await
            .map_err(|_| Error::new("daemon_ready_timeout"))??
        {
            Some(line) => serde_json::from_str::<Value>(&line)?,
            None => return Err(Error::new("daemon_disconnected")),
        };
        if ready["event"] != "ready" {
            // Not a daemon's greeting, so nothing it says about itself counts.
            return Err(Error::with(
                "daemon_greeting_invalid",
                &format!("{} did not greet as a daemon", socket.display()),
            ));
        }
        if ready["protocol"].as_u64() != Some(PROTOCOL) {
            // What the daemon announced, so a client that starts daemons can
            // tell an older one it may replace from a newer one it must not.
            let mut error = Error::with(
                "daemon_protocol_mismatch",
                &format!(
                    "the daemon speaks protocol {}, this client {PROTOCOL}",
                    ready["protocol"]
                ),
            );
            let facts = ["protocol", "pid"]
                .into_iter()
                .filter_map(|key| Some((key.to_owned(), ready.get(key)?.clone())));
            error.facts = Some(Box::new(facts.collect()));
            return Err(error);
        }
        let store = ready["store"]["identity"].as_str().map(str::to_owned);
        let pending: Pending = Arc::default();
        let closed = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let (events, receiver) = mpsc::channel(QUEUE);
        let budget = Arc::new(Semaphore::new(QUEUE_BYTES));
        let routed = pending.clone();
        let gone = closed.clone();
        tokio::spawn(async move {
            let mut lagged = false;
            while let Ok(Some(line)) = lines.next_line().await {
                let Ok(message) = serde_json::from_str::<Value>(&line) else {
                    continue;
                };
                match message.get("id").and_then(Value::as_u64) {
                    Some(id) => {
                        if let Some(sender) = routed.lock().unwrap().remove(&id) {
                            let _ = sender.send(message);
                        }
                    }
                    None => {
                        let Ok(size) = u32::try_from(line.len()) else {
                            lagged = true;
                            break;
                        };
                        let Ok(bytes) = budget.clone().try_acquire_many_owned(size) else {
                            lagged = true;
                            break;
                        };
                        drop(message);
                        match events.try_send(Notification {
                            encoded: line,
                            _bytes: Some(bytes),
                        }) {
                            Ok(()) => {}
                            Err(mpsc::error::TrySendError::Closed(_)) => break,
                            Err(mpsc::error::TrySendError::Full(_)) => {
                                lagged = true;
                                break;
                            }
                        }
                    }
                }
            }
            // The socket is gone, or we let go of it: every request still
            // waiting fails with `daemon_disconnected` now, and every later
            // one fails as it is made. The flag is set under the lock a
            // request registers under, so none slips between. A lag is
            // announced after the queue drains, then the receiver closes.
            {
                let mut waiting = routed.lock().unwrap();
                gone.store(true, std::sync::atomic::Ordering::SeqCst);
                waiting.clear();
            }
            drop(lines);
            if lagged {
                let _ = events
                    .send(Notification { encoded: json!({"event": "follow_lagged", "durable": false, "reason": "client_lagged"}).to_string(), _bytes: None })
                    .await;
            }
        });
        Ok((
            Arc::new(Self {
                writer: Mutex::new(write),
                pending,
                next: std::sync::atomic::AtomicU64::new(0),
                closed,
                store,
            }),
            Events(receiver),
        ))
    }

    /// Let the daemon go: the write half shuts down, the daemon sees the end
    /// of the stream and closes its side, the reader ends, and whoever is
    /// waiting on the notifications sees them close.
    pub async fn close(&self) {
        let _ = self.writer.lock().await.shutdown().await;
    }

    pub fn store(&self) -> Option<&str> {
        self.store.as_deref()
    }

    pub async fn request(&self, op: &str, mut params: Value) -> Result<Value> {
        let id = self.next.fetch_add(1, std::sync::atomic::Ordering::Relaxed) + 1;
        params["id"] = json!(id);
        params["op"] = json!(op);
        let (sender, receiver) = oneshot::channel();
        {
            let mut pending = self.pending.lock().unwrap();
            if self.closed.load(std::sync::atomic::Ordering::SeqCst) {
                return Err(Error::new("daemon_disconnected"));
            }
            pending.insert(id, sender);
        }
        let _registration = PendingRequest {
            pending: &self.pending,
            id,
        };
        let mut line = serde_json::to_vec(&params)?;
        line.push(b'\n');
        {
            let mut writer = self.writer.lock().await;
            if self.closed.load(std::sync::atomic::Ordering::SeqCst) {
                return Err(Error::new("daemon_disconnected"));
            }
            let mut frame = FrameWrite {
                fd: writer.as_ref().as_raw_fd(),
                closed: &self.closed,
                complete: false,
            };
            writer.write_all(&line).await?;
            frame.complete = true;
        }
        let message = receiver
            .await
            .map_err(|_| Error::new("daemon_disconnected"))?;
        let Some(code) = message.get("error").and_then(Value::as_str) else {
            return Ok(message["result"].clone());
        };
        let mut error = Error::new(code);
        error.detail = message["detail"].as_str().map(str::to_owned);
        if let Value::Object(mut facts) = message {
            for key in ["id", "error", "detail"] {
                facts.remove(key);
            }
            error.facts = (!facts.is_empty()).then(|| Box::new(facts));
        }
        Err(error)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn canceled_waits_release_pending_requests_without_closing_the_session() {
        let (stream, peer) = UnixStream::pair().unwrap();
        let (_, write) = stream.into_split();
        let client = Arc::new(Client {
            writer: Mutex::new(write),
            pending: Arc::default(),
            next: std::sync::atomic::AtomicU64::new(0),
            closed: Arc::new(std::sync::atomic::AtomicBool::new(false)),
            store: None,
        });
        let mut lines = BufReader::new(peer).lines();
        for _ in 0..20 {
            let request = tokio::spawn({
                let client = client.clone();
                async move {
                    client
                        .request("wait", json!({"handles":["turn:Bob:1"]}))
                        .await
                }
            });
            lines.next_line().await.unwrap().unwrap();
            request.abort();
            let _ = request.await;
            assert!(client.pending.lock().unwrap().is_empty());
            assert!(!client.closed.load(std::sync::atomic::Ordering::SeqCst));
        }
    }

    #[tokio::test]
    async fn a_refusal_keeps_the_facts_sent_beside_its_detail() {
        let (stream, peer) = UnixStream::pair().unwrap();
        let (_, write) = stream.into_split();
        let client = Arc::new(Client {
            writer: Mutex::new(write),
            pending: Arc::default(),
            next: std::sync::atomic::AtomicU64::new(0),
            closed: Arc::new(std::sync::atomic::AtomicBool::new(false)),
            store: None,
        });
        let mut lines = BufReader::new(peer).lines();
        let request = tokio::spawn({
            let client = client.clone();
            async move { client.request("submit", json!({"bot":"Bob"})).await }
        });
        let sent: Value = serde_json::from_str(&lines.next_line().await.unwrap().unwrap()).unwrap();
        let id = sent["id"].as_u64().unwrap();
        let reply = client.pending.lock().unwrap().remove(&id).unwrap();
        reply
            .send(
                json!({"id":id,"error":"bot_busy","detail":"turn 3 is running",
                "running_turn":3,"fork_point":42}),
            )
            .unwrap();
        let error = request.await.unwrap().unwrap_err();
        assert_eq!(
            (error.code.as_str(), error.detail.as_deref()),
            ("bot_busy", Some("turn 3 is running"))
        );
        assert_eq!(
            error.facts.map(|facts| Value::Object(*facts)),
            Some(json!({"running_turn":3,"fork_point":42}))
        );
    }

    #[tokio::test]
    async fn canceling_a_partial_frame_disconnects_instead_of_corrupting_the_next_request() {
        use tokio::io::AsyncReadExt;
        let (stream, mut peer) = UnixStream::pair().unwrap();
        let (_, write) = stream.into_split();
        let client = Arc::new(Client {
            writer: Mutex::new(write),
            pending: Arc::default(),
            next: std::sync::atomic::AtomicU64::new(0),
            closed: Arc::new(std::sync::atomic::AtomicBool::new(false)),
            store: None,
        });
        let request = tokio::spawn({
            let client = client.clone();
            async move {
                client
                    .request("submit", json!({"prompt":"x".repeat(4*1024*1024)}))
                    .await
            }
        });
        peer.read_u8().await.unwrap(); // The request exceeds the socket buffer and is still writing.
        request.abort();
        let _ = request.await;
        let result =
            tokio::time::timeout(Duration::from_secs(1), client.request("bots", json!({}))).await;
        assert_eq!(
            result
                .expect("a partial frame must close the session")
                .unwrap_err()
                .code,
            "daemon_disconnected"
        );
        assert!(client.pending.lock().unwrap().is_empty());
    }
}
