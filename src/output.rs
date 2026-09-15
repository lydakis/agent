//! Bounded JSONL output for one client session. A stdout session drains on a
//! dedicated thread; a socket session drains on a task. Producers either wait
//! for capacity (`send`) or fail immediately (`try_send`) so a slow subscriber
//! never blocks another bot's turn.
use crate::{Error, Result};
use serde_json::Value;
use std::io::Write;
use std::sync::Arc;
use tokio::{
    io::{AsyncWrite, AsyncWriteExt},
    sync::{OwnedSemaphorePermit, Semaphore, mpsc, oneshot, watch},
};

const QUEUE_BYTES: usize = 2 * 1024 * 1024;
pub const MAX_EVENT: usize = 1024 * 1024;
type Packet = (Vec<u8>, OwnedSemaphorePermit, Option<oneshot::Sender<()>>);

#[derive(Clone)]
pub struct Output {
    sender: mpsc::Sender<Packet>,
    budget: Arc<Semaphore>,
    closed: watch::Sender<bool>,
}

impl Output {
    fn channel() -> (Self, mpsc::Receiver<Packet>) {
        let (sender, receiver) = mpsc::channel::<Packet>(64);
        let budget = Arc::new(Semaphore::new(QUEUE_BYTES));
        let (closed, _) = watch::channel(false);
        (
            Self {
                sender,
                budget,
                closed,
            },
            receiver,
        )
    }

    pub fn stdout() -> (Self, std::thread::JoinHandle<Result<()>>) {
        let (output, mut receiver) = Self::channel();
        let worker = std::thread::spawn(move || {
            let stdout = std::io::stdout();
            let mut writer = stdout.lock();
            while let Some((bytes, _permit, flushed)) = receiver.blocking_recv() {
                writer.write_all(&bytes)?;
                writer.flush()?;
                if let Some(flushed) = flushed {
                    let _ = flushed.send(());
                }
            }
            Ok(())
        });
        (output, worker)
    }

    /// Drain to an async writer; the session ends when writing fails.
    pub fn writer<W: AsyncWrite + Unpin + Send + 'static>(mut writer: W) -> Self {
        let (output, mut receiver) = Self::channel();
        let mut closed = output.closed.subscribe();
        tokio::spawn(async move {
            tokio::select! {
                biased;
                _ = async {
                    if closed.wait_for(|closed| *closed).await.is_err() {
                        // Dropping the last producer must still drain its
                        // accepted packets. Only explicit close aborts I/O.
                        std::future::pending::<()>().await;
                    }
                } => {}
                _ = async {
                    while let Some((bytes, _permit, flushed)) = receiver.recv().await {
                        if writer.write_all(&bytes).await.is_err() {
                            break;
                        }
                        if let Some(flushed) = flushed { let _ = flushed.send(()); }
                    }
                } => {}
            }
        });
        output
    }

    /// Close a socket even if its writer is blocked. EOF is the reliable
    /// overload signal; it does not depend on space in the event queue.
    pub fn close(&self) {
        self.budget.close();
        self.closed.send_replace(true);
    }

    /// Observe closure without retaining a sender to the output worker.
    /// The stdio owner must exit when closed, since a blocking stdout write
    /// cannot be cancelled by the socket writer's async close path.
    pub fn subscribe_closed(&self) -> watch::Receiver<bool> {
        self.closed.subscribe()
    }

    /// Wait until accepted packets have been written, before ending the runtime.
    pub async fn drain(&self) -> Result<()> {
        let (done, drained) = oneshot::channel();
        let permit = self
            .budget
            .clone()
            .acquire_many_owned(0)
            .await
            .map_err(|_| Error::new("output_closed"))?;
        self.sender
            .send((Vec::new(), permit, Some(done)))
            .await
            .map_err(|_| Error::new("output_closed"))?;
        drained.await.map_err(|_| Error::new("output_closed"))
    }

    fn encode(event: &Value) -> Result<Vec<u8>> {
        let mut bytes = serde_json::to_vec(event)?;
        bytes.push(b'\n');
        if bytes.len() > MAX_EVENT {
            return crate::fail("event_size_limit");
        }
        Ok(bytes)
    }

    fn response(id: Value, result: Result<Value>) -> Value {
        match result {
            Ok(result) => serde_json::json!({"id":id.clone(),"result":result}),
            Err(error) => {
                serde_json::json!({"id":id.clone(),"error":error.code,"detail":error.detail})
            }
        }
    }

    pub fn try_respond(&self, id: Value, result: Result<Value>) -> Result<()> {
        match self.try_send(Self::response(id.clone(), result)) {
            Err(error) if error.code == "event_size_limit" => {
                self.try_send(serde_json::json!({"id":id,"error":"response_size_limit"}))
            }
            result => result,
        }
    }

    pub async fn respond(&self, id: Value, result: Result<Value>) -> Result<()> {
        match self.send(Self::response(id.clone(), result)).await {
            Err(error) if error.code == "event_size_limit" => {
                self.send(serde_json::json!({"id":id,"error":"response_size_limit"}))
                    .await
            }
            result => result,
        }
    }

    pub async fn send(&self, event: Value) -> Result<()> {
        let bytes = Self::encode(&event)?;
        let permit = self
            .budget
            .clone()
            .acquire_many_owned(bytes.len() as u32)
            .await
            .map_err(|_| Error::new("output_closed"))?;
        self.sender
            .send((bytes, permit, None))
            .await
            .map_err(|_| Error::new("output_closed"))
    }

    /// Never waits. `output_lagged` means the consumer has not kept up.
    pub fn try_send(&self, event: Value) -> Result<()> {
        let bytes = Self::encode(&event)?;
        let permit = self
            .budget
            .clone()
            .try_acquire_many_owned(bytes.len() as u32)
            .map_err(|_| Error::new("output_lagged"))?;
        self.sender
            .try_send((bytes, permit, None))
            .map_err(|error| match error {
                mpsc::error::TrySendError::Full(_) => Error::new("output_lagged"),
                mpsc::error::TrySendError::Closed(_) => Error::new("output_closed"),
            })
    }
}

/// Count encoded bytes without allocating another copy of a replay event.
pub fn encoded_len(value: &Value) -> Result<usize> {
    struct Counter(usize);
    impl Write for Counter {
        fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
            self.0 += bytes.len();
            Ok(bytes.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }
    let mut counter = Counter(0);
    serde_json::to_writer(&mut counter, value)?;
    Ok(counter.0)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[tokio::test]
    async fn closing_a_stalled_socket_writer_releases_the_connection() {
        let (writer, mut reader) = tokio::io::duplex(16);
        let output = Output::writer(writer);
        output
            .send(serde_json::json!({"text":"x".repeat(1000)}))
            .await
            .unwrap();
        tokio::task::yield_now().await;
        output.close();
        let mut bytes = Vec::new();
        use tokio::io::AsyncReadExt;
        tokio::time::timeout(
            std::time::Duration::from_secs(1),
            reader.read_to_end(&mut bytes),
        )
        .await
        .unwrap()
        .unwrap();
        assert!(output.send(serde_json::json!({})).await.is_err());
    }
    #[tokio::test]
    async fn oversized_response_is_correlated_and_does_not_close_output() {
        let (output, mut receiver) = Output::channel();
        output
            .respond(
                serde_json::json!(7),
                Ok(serde_json::json!("x".repeat(MAX_EVENT))),
            )
            .await
            .unwrap();
        let (bytes, permit, _) = receiver.recv().await.unwrap();
        assert_eq!(
            serde_json::from_slice::<Value>(&bytes).unwrap(),
            serde_json::json!({"id":7,"error":"response_size_limit"})
        );
        drop(permit);
        output
            .respond(serde_json::json!(8), Ok(serde_json::json!("still here")))
            .await
            .unwrap();
        let (bytes, _, _) = receiver.recv().await.unwrap();
        assert_eq!(
            serde_json::from_slice::<Value>(&bytes).unwrap()["result"],
            "still here"
        );
    }
    #[tokio::test]
    async fn slow_consumers_hold_byte_budget_until_packets_are_released() {
        let (output, mut receiver) = Output::channel();
        let event = serde_json::json!({"text":"x".repeat(750_000)});
        output.send(event.clone()).await.unwrap();
        output.send(event.clone()).await.unwrap();
        assert_eq!(
            output.try_send(event.clone()).unwrap_err().code,
            "output_lagged"
        );
        let blocked = output.send(event);
        tokio::pin!(blocked);
        assert!(
            tokio::time::timeout(std::time::Duration::from_millis(10), &mut blocked)
                .await
                .is_err()
        );
        let packet = receiver.recv().await.unwrap();
        assert!(
            tokio::time::timeout(std::time::Duration::from_millis(10), &mut blocked)
                .await
                .is_err()
        );
        drop(packet);
        tokio::time::timeout(std::time::Duration::from_secs(1), blocked)
            .await
            .unwrap()
            .unwrap();
    }
}
