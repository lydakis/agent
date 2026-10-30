//! Client sessions: one stdio owner, or any number of Unix-socket connections,
//! each feeding bounded JSONL requests into the service loop.
use super::{Command, Request, hub::Hub};
use agent_runtime::{Error, Result, fail, output::Output};
use std::collections::{HashMap, VecDeque};
use std::io::{BufRead, Read};
use std::sync::{Arc, Mutex};
use tokio::{
    io::{AsyncBufReadExt, AsyncReadExt, BufReader},
    sync::{Notify, Semaphore},
};

// Keep requests inline in the input queue instead of allocating a box for
// every request. Open/Closed messages are infrequent control traffic.
#[allow(clippy::large_enum_variant)]
pub enum Inbound {
    /// A connection, the sandboxed bot whose own socket it came in on, and
    /// the process at its other end, where the system says.
    Open(u64, Output, Option<i64>, Option<u32>),
    Request(u64, Result<Request>),
    Closed(u64),
}
impl Inbound {
    fn session(&self) -> u64 {
        match self {
            Inbound::Open(id, ..) | Inbound::Request(id, _) | Inbound::Closed(id) => *id,
        }
    }
}

/// The service loop's input: each session's messages in its own queue, and
/// the sessions with any taken in turn, one message each, as Redis and NATS
/// serve their clients. A client that pipelines thousands of requests then
/// holds every other session up by one of them, not by all those read
/// before theirs. `capacity` bounds what all sessions have read ahead
/// together; a reader waits for room in the order it asked.
pub fn channel(capacity: usize) -> (Sender, Receiver) {
    let shared = Arc::new(Shared {
        queues: Mutex::new(Queues::default()),
        room: Semaphore::new(capacity),
        arrived: Notify::new(),
    });
    shared.lock().senders = 1;
    (Sender(shared.clone()), Receiver(shared))
}

struct Shared {
    queues: Mutex<Queues>,
    room: Semaphore,
    arrived: Notify,
}
impl Shared {
    fn lock(&self) -> std::sync::MutexGuard<'_, Queues> {
        self.queues.lock().unwrap_or_else(|e| e.into_inner())
    }
}

#[derive(Default)]
struct Queues {
    sessions: HashMap<u64, VecDeque<Inbound>>,
    /// The sessions with a message queued, each once, next first.
    turns: VecDeque<u64>,
    senders: usize,
}

/// The service loop is gone: nothing more is taken.
#[derive(Debug)]
pub struct Gone;

pub struct Sender(Arc<Shared>);
impl Sender {
    /// Queue `message` once there is room.
    pub async fn send(&self, message: Inbound) -> std::result::Result<(), Gone> {
        self.0.room.acquire().await.map_err(|_| Gone)?.forget();
        let mut guard = self.0.lock();
        let queues = &mut *guard;
        let session = message.session();
        let queue = queues.sessions.entry(session).or_default();
        if queue.is_empty() {
            queues.turns.push_back(session);
        }
        queue.push_back(message);
        drop(guard);
        self.0.arrived.notify_one();
        Ok(())
    }
    /// `send` from a thread outside the runtime, which `runtime` serves.
    pub fn blocking_send(
        &self,
        runtime: &tokio::runtime::Handle,
        message: Inbound,
    ) -> std::result::Result<(), Gone> {
        runtime.block_on(self.send(message))
    }
}
impl Clone for Sender {
    fn clone(&self) -> Self {
        self.0.lock().senders += 1;
        Sender(self.0.clone())
    }
}
impl Drop for Sender {
    fn drop(&mut self) {
        self.0.lock().senders -= 1;
        self.0.arrived.notify_one();
    }
}

pub struct Receiver(Arc<Shared>);
impl Receiver {
    /// The next session's oldest message, or `None` once every sender is
    /// gone and nothing is queued. Cancel-safe: a message is taken only
    /// when it is returned.
    pub async fn recv(&mut self) -> Option<Inbound> {
        loop {
            {
                let mut guard = self.0.lock();
                let queues = &mut *guard;
                if let Some(session) = queues.turns.pop_front() {
                    let queue = queues
                        .sessions
                        .get_mut(&session)
                        .expect("a turn has a queue");
                    let message = queue
                        .pop_front()
                        .expect("a queue with a turn has a message");
                    if queue.is_empty() {
                        queues.sessions.remove(&session);
                    } else {
                        queues.turns.push_back(session);
                    }
                    drop(guard);
                    self.0.room.add_permits(1);
                    return Some(message);
                }
                if queues.senders == 0 {
                    return None;
                }
            }
            self.0.arrived.notified().await;
        }
    }
}
impl Drop for Receiver {
    fn drop(&mut self) {
        self.0.room.close();
    }
}

/// Room for a prompt with its pictures: 5 MiB of images, in base64.
pub const LINE_LIMIT: usize = 8 * 1024 * 1024;

/// A request line. A line that is JSON but not a request is refused with
/// what is wrong with it, under its `id` when that is one, so the client
/// knows which request failed and why.
fn parse(line: &[u8]) -> Result<Request> {
    serde_json::from_slice(line).map_err(|error| {
        let Ok(value) = serde_json::from_slice::<serde_json::Value>(line) else {
            return Error::new("invalid_json");
        };
        let refused = Error::with("invalid_request", error.to_string());
        match value.get("id") {
            Some(id) if id.is_u64() || id.as_str().is_some_and(|id| id.len() <= 128) => {
                refused.facts(serde_json::json!({"id": id}))
            }
            _ => refused,
        }
    })
}

pub fn stdio_reader(sender: Sender) {
    let runtime = tokio::runtime::Handle::current();
    std::thread::spawn(move || {
        let stdin = std::io::stdin();
        let mut reader = stdin.lock();
        loop {
            let mut line = Vec::new();
            let result = (&mut reader)
                .take(LINE_LIMIT as u64 + 1)
                .read_until(b'\n', &mut line);
            match result {
                Ok(0) => break,
                Ok(_) if line.len() <= LINE_LIMIT && line.ends_with(b"\n") => {
                    let request = parse(&line);
                    if sender
                        .blocking_send(&runtime, Inbound::Request(0, request))
                        .is_err()
                    {
                        break;
                    }
                }
                _ => {
                    let _ = sender.blocking_send(
                        &runtime,
                        Inbound::Request(0, fail("input_line_limit_or_truncation")),
                    );
                    break;
                }
            }
        }
        let _ = sender.blocking_send(&runtime, Inbound::Closed(0));
    });
}

/// Accept connections on `listener` until the service stops, each a session
/// numbered from `next`, every one on a bot's own socket answered as `caller`.
/// With `hub`, each session's lease renewals are answered as they are read.
pub async fn accept(
    listener: tokio::net::UnixListener,
    sender: Sender,
    next: std::sync::Arc<std::sync::atomic::AtomicU64>,
    caller: Option<i64>,
    hub: Option<Hub>,
) {
    while let Ok((stream, _)) = listener.accept().await {
        let id = next.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let peer = stream
            .peer_cred()
            .ok()
            .and_then(|cred| cred.pid())
            .and_then(|pid| u32::try_from(pid).ok());
        let (read, write) = stream.into_split();
        let output = Output::writer(write);
        let renewals = hub.clone().map(|hub| (hub, output.clone()));
        if sender
            .send(Inbound::Open(id, output, caller, peer))
            .await
            .is_err()
        {
            break;
        }
        tokio::spawn(socket_reader(
            id,
            BufReader::new(read),
            sender.clone(),
            renewals,
        ));
    }
}

/// The answer to `request` when it renews a lease under a valid id.
fn renewal(hub: &Hub, session: u64, request: &Request) -> Option<Result<serde_json::Value>> {
    let Command::RenewApprovals { tag, lease } = &request.command else {
        return None;
    };
    match &request.id {
        serde_json::Value::Number(number) if number.is_u64() => {}
        serde_json::Value::String(text) if text.len() <= 128 => {}
        _ => return None,
    }
    Some(
        hub.renew(tag, session, *lease)
            .map(|lease_ms| serde_json::json!({"tag":tag,"lease":lease,"lease_ms":lease_ms})),
    )
}

pub async fn socket_reader(
    id: u64,
    mut reader: BufReader<tokio::net::unix::OwnedReadHalf>,
    sender: Sender,
    renewals: Option<(Hub, Output)>,
) {
    loop {
        let mut line = Vec::new();
        let result = (&mut reader)
            .take(LINE_LIMIT as u64 + 1)
            .read_until(b'\n', &mut line)
            .await;
        match result {
            Ok(0) => break,
            Ok(_) if line.len() <= LINE_LIMIT && line.ends_with(b"\n") => {
                let request = parse(&line);
                // A renewal touches only the hub, so it is answered here and
                // not behind the session's other requests: a holder with
                // thousands of calls in flight keeps its tag (Kafka's
                // heartbeat thread, Kubernetes leases).
                if let (Some((hub, output)), Ok(request)) = (&renewals, &request)
                    && let Some(answer) = renewal(hub, id, request)
                {
                    if output.try_respond(request.id.clone(), answer).is_err() {
                        output.close();
                        break;
                    }
                    continue;
                }
                if sender.send(Inbound::Request(id, request)).await.is_err() {
                    break;
                }
            }
            _ => {
                let _ = sender
                    .send(Inbound::Request(id, fail("input_line_limit_or_truncation")))
                    .await;
                break;
            }
        }
    }
    let _ = sender.send(Inbound::Closed(id)).await;
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::AsyncWriteExt;

    fn order(messages: &[Inbound]) -> Vec<u64> {
        messages.iter().map(Inbound::session).collect()
    }

    #[tokio::test]
    async fn sessions_take_turns_however_much_one_sent_first() {
        let (sender, mut inbound) = channel(64);
        for _ in 0..3 {
            sender.send(Inbound::Closed(1)).await.unwrap();
        }
        sender.send(Inbound::Closed(2)).await.unwrap();
        sender.send(Inbound::Closed(3)).await.unwrap();
        sender.send(Inbound::Closed(2)).await.unwrap();
        let mut taken = Vec::new();
        for _ in 0..6 {
            taken.push(inbound.recv().await.unwrap());
        }
        assert_eq!(order(&taken), [1, 2, 3, 1, 2, 1]);
    }

    #[tokio::test]
    async fn a_full_queue_holds_a_reader_until_one_is_taken() {
        let (sender, mut inbound) = channel(1);
        sender.send(Inbound::Closed(1)).await.unwrap();
        let waiting = sender.clone();
        let blocked = tokio::spawn(async move { waiting.send(Inbound::Closed(2)).await.is_ok() });
        tokio::task::yield_now().await;
        assert!(!blocked.is_finished());
        assert_eq!(inbound.recv().await.unwrap().session(), 1);
        assert!(blocked.await.unwrap());
        assert_eq!(inbound.recv().await.unwrap().session(), 2);
        // Every sender gone and nothing queued: the loop's input ends.
        drop(sender);
        assert!(inbound.recv().await.is_none());
        // And with the loop gone, a reader is told so.
        let (sender, inbound) = channel(1);
        drop(inbound);
        assert!(sender.send(Inbound::Closed(1)).await.is_err());
    }

    #[tokio::test]
    async fn a_renewal_is_answered_while_the_service_loop_is_busy() {
        let (daemon, client) = tokio::net::UnixStream::pair().unwrap();
        let (read, write) = daemon.into_split();
        let output = Output::writer(write);
        let hub = Hub::default();
        let (lease, _) = hub.serve("auto", 7, output.clone(), 5000).unwrap();
        hub.start("auto", 7, lease);
        // A service loop that takes nothing: a request sent to it waits.
        let (sender, _inbound) = channel(1);
        sender.send(Inbound::Closed(99)).await.unwrap();
        tokio::spawn(socket_reader(
            7,
            BufReader::new(read),
            sender,
            Some((hub, output)),
        ));
        let (client_read, mut client_write) = client.into_split();
        let line = serde_json::json!({"id":1,"op":"renew_approvals","tag":"auto","lease":lease});
        client_write
            .write_all(format!("{line}\n").as_bytes())
            .await
            .unwrap();
        let mut lines = BufReader::new(client_read).lines();
        let answer = tokio::time::timeout(std::time::Duration::from_secs(5), lines.next_line())
            .await
            .expect("the renewal waited for the service loop")
            .unwrap()
            .unwrap();
        let answer: serde_json::Value = serde_json::from_str(&answer).unwrap();
        assert_eq!(answer["id"], 1);
        assert_eq!(answer["result"]["lease"], lease);
        assert_eq!(answer["result"]["lease_ms"], 5000);
    }
}
