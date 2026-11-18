//! Where a window's project agents stand, kept current by the daemon's
//! events instead of listed again every few seconds. A window takes one
//! snapshot, every agent its project's pattern names with the events'
//! cursor the listing was read at, and follows the project from that
//! cursor for the events that move an agent's state alone (a turn starting,
//! waiting, resuming, held or ending, a call held for approval, an agent
//! made or deleted), so it never receives every tool event of every agent
//! to keep a count. Kubernetes' list then watch from the list's
//! resourceVersion, and Codex's app server pushing a thread's status
//! instead of clients listing threads again, are the same shape.
//!
//! The follow has a connection of its own, so its events never mix with the
//! window's own agent's. A change names the agents it moved; the window
//! reads only their rows (`bots` by `names`). A window that falls behind,
//! or loses the daemon, takes a fresh snapshot rather than catch up event
//! by event: the daemon drops a follower that lags, and the snapshot is
//! the truth. An idle project costs no reads at all.
use agent_client::{Client, Events};
use serde_json::{Value, json};
use std::collections::BTreeSet;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::sync::Notify;

/// The events that move where an agent stands, as a `bots` row says it:
/// its status, running turn, what it waits on and how its last turn
/// ended; an approval held for the person; an agent made or gone.
pub const KINDS: [&str; 15] = [
    "created",
    "forked",
    "queued",
    "accepted",
    "steered",
    "held",
    "message_ended",
    "turn_waiting",
    "turn_resumed",
    "turn_paced",
    "turn_finished",
    "approval_requested",
    "approval_asked",
    "call_lifted",
    "deleted",
];

/// Every agent `pattern` names, every page, with the events' cursor read
/// before the first page: following from it misses no change.
pub async fn list(client: &Client, pattern: &str) -> agent_client::Result<(Vec<Value>, i64)> {
    let mut listed = Vec::new();
    let mut after: Option<String> = None;
    let mut cursor = None;
    loop {
        let mut ask = json!({"name": pattern, "limit": 256});
        if let Some(after) = &after {
            ask["after"] = json!(after);
        }
        let page = client.request("bots", ask).await?;
        cursor = cursor.or(page["cursor"].as_i64());
        listed.extend(page["bots"].as_array().cloned().unwrap_or_default());
        match page["next_after"].as_str() {
            Some(next) if after.as_deref() != Some(next) => after = Some(next.to_owned()),
            _ => break,
        }
    }
    Ok((listed, cursor.unwrap_or(0)))
}

/// The rows of `names`, read by name, 256 a request: the agents that
/// moved. One that is gone is not among them.
pub async fn rows(client: &Client, names: &[String]) -> agent_client::Result<Vec<Value>> {
    let mut rows = Vec::new();
    for chunk in names.chunks(256) {
        let page = client
            .request("bots", json!({"names": chunk, "limit": 256}))
            .await?;
        rows.extend(page["bots"].as_array().cloned().unwrap_or_default());
    }
    Ok(rows)
}

/// What the follow heard since the window last took it.
#[derive(Default)]
pub struct Changes {
    /// A fresh snapshot, every agent the pattern names: taken at the start
    /// and after the follow was lost or fell behind.
    pub snapshot: Option<Vec<Value>>,
    /// The agents an event moved since.
    pub moved: BTreeSet<String>,
}

/// How a follow reaches the daemon: a connection of its own each time,
/// with its events.
pub type Connect = Arc<dyn Fn() -> Connecting + Send + Sync>;
/// A connection on its way.
pub type Connecting = std::pin::Pin<
    Box<dyn std::future::Future<Output = agent_client::Result<(Arc<Client>, Events)>> + Send>,
>;

/// Connections to the daemon at `socket`.
pub fn connect_to(socket: PathBuf) -> Connect {
    Arc::new(move || {
        let socket = socket.clone();
        Box::pin(async move { Client::connect(&socket).await })
    })
}

/// A window's follow of its project, in a task of its own.
pub struct Follower {
    pub pattern: String,
    changes: Arc<Mutex<Changes>>,
    task: tokio::task::JoinHandle<()>,
}

impl Follower {
    /// Follow the agents `pattern` names through the daemon at `socket`,
    /// waking `woken` when something arrives that the window has not taken.
    pub fn start(connect: Connect, pattern: String, woken: Arc<Notify>) -> Self {
        let changes = Arc::new(Mutex::new(Changes::default()));
        let task = tokio::spawn(run(connect, pattern.clone(), changes.clone(), woken));
        Self {
            pattern,
            changes,
            task,
        }
    }

    /// What arrived since the last take.
    pub fn take(&self) -> Changes {
        std::mem::take(&mut *self.changes.lock().unwrap())
    }

    /// As though an event had moved `bot`.
    #[cfg(test)]
    pub fn moved(&self, bot: &str) {
        self.changes.lock().unwrap().moved.insert(bot.to_owned());
    }

    /// Whether the snapshot is in and not yet taken.
    #[cfg(test)]
    pub fn snapshot_waits(&self) -> bool {
        self.changes.lock().unwrap().snapshot.is_some()
    }
}

impl Drop for Follower {
    fn drop(&mut self) {
        self.task.abort();
    }
}

/// Follow until the window lets go: on a lost connection or a lag, wait a
/// moment and take a fresh snapshot.
async fn run(connect: Connect, pattern: String, changes: Arc<Mutex<Changes>>, woken: Arc<Notify>) {
    let mut pause = Duration::from_millis(250);
    loop {
        if follow(&connect, &pattern, &changes, &woken).await {
            pause = Duration::from_millis(250);
        }
        tokio::time::sleep(pause).await;
        pause = (pause * 2).min(Duration::from_secs(30));
    }
}

/// One connection's follow: the snapshot, then events until it ends.
/// Whether it got as far as the snapshot.
async fn follow(
    connect: &Connect,
    pattern: &str,
    changes: &Mutex<Changes>,
    woken: &Notify,
) -> bool {
    let Ok((client, mut events)) = connect().await else {
        return false;
    };
    let Ok((listed, cursor)) = list(&client, pattern).await else {
        return false;
    };
    {
        let mut changes = changes.lock().unwrap();
        changes.snapshot = Some(listed);
        changes.moved.clear();
    }
    woken.notify_one();
    let asked = json!({"bot": pattern, "after": cursor, "kinds": KINDS});
    if client.request("follow", asked).await.is_err() {
        return true;
    }
    while let Some(event) = events.recv().await {
        match event["event"].as_str() {
            // Behind, or a gap retention made: the snapshot is the truth.
            Some("follow_lagged" | "pruned") => return true,
            Some("follow_live") | None => {}
            Some(_) => {
                let Some(bot) = event["bot"].as_str() else {
                    continue;
                };
                let first = {
                    let mut changes = changes.lock().unwrap();
                    let first = changes.moved.is_empty() && changes.snapshot.is_none();
                    changes.moved.insert(bot.to_owned());
                    first
                };
                if first {
                    woken.notify_one();
                }
            }
        }
    }
    true
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};

    /// A daemon that lists `shop.*` with how many times it was listed, and
    /// on each follow moves `shop.fix` and then drops the follower as lagged.
    async fn lagging_daemon() -> (PathBuf, Arc<std::sync::atomic::AtomicUsize>) {
        let dir = std::env::temp_dir().join(format!(
            "sonny-standing-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let socket = dir.join("agent.sock");
        let listener = tokio::net::UnixListener::bind(&socket).unwrap();
        let listed = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let count = listed.clone();
        tokio::spawn(async move {
            while let Ok((stream, _)) = listener.accept().await {
                let count = count.clone();
                tokio::spawn(async move {
                    let (read, mut write) = stream.into_split();
                    let ready = json!({"event": "ready", "protocol": agent_client::PROTOCOL,
                        "providers": {}});
                    write
                        .write_all(format!("{ready}\n").as_bytes())
                        .await
                        .unwrap();
                    let mut lines = BufReader::new(read).lines();
                    while let Ok(Some(line)) = lines.next_line().await {
                        let asked: Value = serde_json::from_str(&line).unwrap();
                        let id = asked["id"].clone();
                        let mut said = Vec::new();
                        match asked["op"].as_str() {
                            Some("bots") => {
                                assert_eq!(asked["name"], "shop.*");
                                let n = count.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                                let row = json!({"name": format!("shop.{n}"), "head": 1});
                                said.push(json!({"id": id, "result":
                                    {"bots": [row], "cursor": 40 + n, "next_after": null}}));
                            }
                            Some("follow") => {
                                assert_eq!(asked["bot"], "shop.*");
                                assert_eq!(asked["kinds"], json!(KINDS));
                                said.push(json!({"id": id, "result": {"following": "shop.*"}}));
                                said.push(json!({"event": "follow_live"}));
                                said.push(json!({"event": "turn_finished", "bot": "shop.fix"}));
                                said.push(json!({"event": "follow_lagged"}));
                            }
                            _ => said.push(json!({"id": id, "result": {}})),
                        }
                        for line in said {
                            write
                                .write_all(format!("{line}\n").as_bytes())
                                .await
                                .unwrap();
                        }
                    }
                });
            }
        });
        (socket, listed)
    }

    #[tokio::test]
    async fn a_follow_snapshots_names_what_moved_and_snapshots_again_when_behind() {
        let (socket, listed) = lagging_daemon().await;
        let woken = Arc::new(Notify::new());
        let follower = Follower::start(connect_to(socket), "shop.*".into(), woken.clone());
        // The snapshot, then the agent the event moved.
        let mut first = None;
        let mut moved = BTreeSet::new();
        while first.is_none() || moved.is_empty() {
            woken.notified().await;
            let changes = follower.take();
            first = first.or(changes.snapshot);
            moved.extend(changes.moved);
        }
        assert_eq!(first.unwrap(), vec![json!({"name": "shop.0", "head": 1})]);
        assert_eq!(moved, BTreeSet::from(["shop.fix".to_owned()]));
        // Behind: a fresh snapshot, not a catch up event by event.
        loop {
            woken.notified().await;
            if let Some(again) = follower.take().snapshot {
                assert_eq!(again, vec![json!({"name": "shop.1", "head": 1})]);
                break;
            }
        }
        assert!(listed.load(std::sync::atomic::Ordering::SeqCst) >= 2);
    }
}
