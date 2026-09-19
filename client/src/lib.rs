//! The daemon's socket protocol for human-facing clients: one session,
//! JSONL requests correlated by `id`, notifications forwarded on a channel.
//! Shared by the terminal client and the desktop app; nothing here depends
//! on the runtime crate.
pub mod policy;

use serde_json::{Value, json};
use std::{collections::HashMap, path::Path, sync::Arc, time::Duration};
use tokio::{
    io::{AsyncBufReadExt, AsyncWriteExt, BufReader},
    net::{UnixStream, unix::OwnedWriteHalf},
    sync::{Mutex, mpsc, oneshot},
};

#[derive(Debug, Clone)]
pub struct Error {
    pub code: String,
    pub detail: Option<String>,
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
        }
    }
    pub fn with(code: &str, detail: &str) -> Self {
        Self {
            code: code.to_owned(),
            detail: Some(detail.to_owned()),
        }
    }
}
impl From<std::io::Error> for Error {
    fn from(error: std::io::Error) -> Self {
        Self {
            code: "io".into(),
            detail: Some(error.to_string()),
        }
    }
}
impl From<serde_json::Error> for Error {
    fn from(error: serde_json::Error) -> Self {
        Self {
            code: "invalid_json".into(),
            detail: Some(error.to_string()),
        }
    }
}
pub type Result<T> = std::result::Result<T, Error>;

type Pending = Arc<Mutex<HashMap<u64, oneshot::Sender<Value>>>>;

/// Notifications queued ahead of the UI before the session is let go.
/// Larger than any replay page, so a normal attach never trips it.
const QUEUE: usize = 4096;

pub struct Client {
    writer: Mutex<OwnedWriteHalf>,
    pending: Pending,
    next: std::sync::atomic::AtomicU64,
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
    pub async fn connect(socket: &Path) -> Result<(Arc<Self>, mpsc::Receiver<Value>)> {
        let stream = tokio::time::timeout(Duration::from_secs(5), UnixStream::connect(socket))
            .await
            .map_err(|_| Error::new("daemon_connect_timeout"))?
            .map_err(|error| Error {
                code: "daemon_unavailable".into(),
                detail: Some(format!("{}: {error}", socket.display())),
            })?;
        let (read, write) = stream.into_split();
        let mut lines = BufReader::new(read).lines();
        let ready = match lines.next_line().await? {
            Some(line) => serde_json::from_str::<Value>(&line)?,
            None => return Err(Error::new("daemon_disconnected")),
        };
        if ready["event"] != "ready" {
            return Err(Error::new("daemon_protocol_mismatch"));
        }
        let pending: Pending = Arc::default();
        let (events, receiver) = mpsc::channel(QUEUE);
        let routed = pending.clone();
        tokio::spawn(async move {
            let mut lagged = false;
            while let Ok(Some(line)) = lines.next_line().await {
                let Ok(message) = serde_json::from_str::<Value>(&line) else {
                    continue;
                };
                match message.get("id").and_then(Value::as_u64) {
                    Some(id) => {
                        if let Some(sender) = routed.lock().await.remove(&id) {
                            let _ = sender.send(message);
                        }
                    }
                    None => match events.try_send(message) {
                        Ok(()) => {}
                        Err(mpsc::error::TrySendError::Closed(_)) => break,
                        Err(mpsc::error::TrySendError::Full(_)) => {
                            lagged = true;
                            break;
                        }
                    },
                }
            }
            // The socket is gone, or we let go of it: every request still
            // waiting fails with `daemon_disconnected` now. A lag is
            // announced after the queue drains, then the receiver closes.
            routed.lock().await.clear();
            drop(lines);
            if lagged {
                let _ = events
                    .send(json!({"event": "follow_lagged", "durable": false, "reason": "client_lagged"}))
                    .await;
            }
        });
        Ok((
            Arc::new(Self {
                writer: Mutex::new(write),
                pending,
                next: std::sync::atomic::AtomicU64::new(0),
            }),
            receiver,
        ))
    }

    pub async fn request(&self, op: &str, mut params: Value) -> Result<Value> {
        let id = self.next.fetch_add(1, std::sync::atomic::Ordering::Relaxed) + 1;
        params["id"] = json!(id);
        params["op"] = json!(op);
        let (sender, receiver) = oneshot::channel();
        self.pending.lock().await.insert(id, sender);
        let mut line = serde_json::to_vec(&params)?;
        line.push(b'\n');
        {
            let mut writer = self.writer.lock().await;
            if let Err(error) = writer.write_all(&line).await {
                self.pending.lock().await.remove(&id);
                return Err(error.into());
            }
        }
        let message = receiver
            .await
            .map_err(|_| Error::new("daemon_disconnected"))?;
        match message.get("error").and_then(Value::as_str) {
            Some(code) => Err(Error {
                code: code.to_owned(),
                detail: message["detail"].as_str().map(str::to_owned),
            }),
            None => Ok(message["result"].clone()),
        }
    }
}
