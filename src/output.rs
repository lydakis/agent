use crate::{Error, Result};
use serde_json::Value;
use std::io::Write;
use std::sync::Arc;
use tokio::sync::{OwnedSemaphorePermit, Semaphore, mpsc};

const QUEUE_BYTES: usize = 2 * 1024 * 1024;
pub const MAX_EVENT: usize = 1024 * 1024;
type Packet = (Vec<u8>, OwnedSemaphorePermit);

#[derive(Clone)]
pub struct Output {
    sender: mpsc::Sender<Packet>,
    budget: Arc<Semaphore>,
}

impl Output {
    pub async fn respond(&self, id: Value, result: Result<Value>) -> Result<()> {
        let response = match result {
            Ok(result) => serde_json::json!({"id":id.clone(),"result":result}),
            Err(error) => serde_json::json!({"id":id.clone(),"error":error.0}),
        };
        match self.send(response).await {
            Err(error) if error.0 == "event_size_limit" => {
                self.send(serde_json::json!({"id":id,"error":"response_size_limit"}))
                    .await
            }
            result => result,
        }
    }

    pub fn stdout() -> (Self, std::thread::JoinHandle<Result<()>>) {
        let (sender, mut receiver) = mpsc::channel::<Packet>(64);
        let budget = Arc::new(Semaphore::new(QUEUE_BYTES));
        let worker = std::thread::spawn(move || {
            let stdout = std::io::stdout();
            let mut writer = stdout.lock();
            while let Some((bytes, _permit)) = receiver.blocking_recv() {
                writer.write_all(&bytes)?;
                writer.flush()?;
            }
            Ok(())
        });
        (Self { sender, budget }, worker)
    }
    pub async fn send(&self, event: Value) -> Result<()> {
        let mut bytes = serde_json::to_vec(&event)?;
        bytes.push(b'\n');
        if bytes.len() > MAX_EVENT {
            return crate::fail("event_size_limit");
        }
        let permit = self
            .budget
            .clone()
            .acquire_many_owned(bytes.len() as u32)
            .await
            .map_err(|_| Error("output_closed".into()))?;
        self.sender
            .send((bytes, permit))
            .await
            .map_err(|_| Error("output_closed".into()))
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
    async fn oversized_response_is_correlated_and_does_not_close_output() {
        let (sender, mut receiver) = mpsc::channel::<Packet>(64);
        let output = Output {
            sender,
            budget: Arc::new(Semaphore::new(QUEUE_BYTES)),
        };
        output
            .respond(
                serde_json::json!(7),
                Ok(serde_json::json!("x".repeat(MAX_EVENT))),
            )
            .await
            .unwrap();
        let (bytes, permit) = receiver.recv().await.unwrap();
        assert_eq!(
            serde_json::from_slice::<Value>(&bytes).unwrap(),
            serde_json::json!({"id":7,"error":"response_size_limit"})
        );
        drop(permit);
        output
            .respond(serde_json::json!(8), Ok(serde_json::json!("still here")))
            .await
            .unwrap();
        let (bytes, _) = receiver.recv().await.unwrap();
        assert_eq!(
            serde_json::from_slice::<Value>(&bytes).unwrap()["result"],
            "still here"
        );
    }
    #[tokio::test]
    async fn slow_consumers_hold_byte_budget_until_packets_are_released() {
        let (sender, mut receiver) = mpsc::channel::<Packet>(64);
        let output = Output {
            sender,
            budget: Arc::new(Semaphore::new(QUEUE_BYTES)),
        };
        let event = serde_json::json!({"text":"x".repeat(750_000)});
        output.send(event.clone()).await.unwrap();
        output.send(event.clone()).await.unwrap();
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
