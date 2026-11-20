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
//! The follow shares the window's connection: the daemon names the pattern
//! on each event it brings (`following`), so its events never mix with the
//! window's own agent's, and a window holds one connection, not two. A
//! change names the agents it moved; the window reads only their rows
//! (`bots` by `names`). A window that loses the daemon (the daemon drops a
//! connection that lags) connects again and takes a fresh snapshot rather
//! than catch up event by event: the snapshot is the truth. An idle project
//! costs no reads at all.
use agent_client::Client;
use serde_json::{Value, json};
use std::collections::BTreeSet;
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::sync::Notify;

/// The events that move where an agent stands, as a `bots` row says it:
/// its status, running turn, what it waits on and how its last turn
/// ended; an approval held for the person; an agent made, closed or gone.
pub const KINDS: [&str; 18] = [
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
    "lifted_ended",
    "step_decided",
    "closed",
    "deleted",
];

/// An agent as a window keeps it: the fields of a `bots` row a count
/// reads, typed, so a window that keeps every agent of a large fleet keeps
/// a little of each.
#[derive(Clone, Debug, Default, PartialEq, serde::Deserialize)]
#[serde(default)]
pub struct Agent {
    pub name: String,
    pub bot_id: Option<i64>,
    pub head: Option<i64>,
    pub running_turn: Option<i64>,
    pub status: Option<String>,
    pub waiting_on: Value,
    /// How its newest turn that ran ended.
    pub last: Option<Ended>,
    pub started_ms: Option<u64>,
    /// When a side chat ends: a side chat is not a project's agent.
    pub expire_ms: Option<i64>,
    pub created_by: Option<String>,
    pub created_by_id: Option<i64>,
    pub model: Option<String>,
    pub provider: Option<String>,
    pub workspace: Option<String>,
    /// The profile it was made in, which the daemon keeps on it.
    pub profile: Option<String>,
}

/// How a turn ended, as a `bots` row's `last` says it.
#[derive(Clone, Debug, Default, PartialEq, Hash, serde::Deserialize)]
#[serde(default)]
pub struct Ended {
    pub turn: i64,
    pub status: String,
    pub started_ms: Option<u64>,
    pub finished_ms: Option<u64>,
    /// The code it failed or was interrupted with.
    pub error: Option<String>,
}

impl Agent {
    /// The agent that made it, or empty.
    pub fn maker(&self) -> &str {
        self.created_by.as_deref().unwrap_or("")
    }
}

/// `row`, a `bots` row, as a window keeps it; none for a row that is not
/// one, or for an agent closed, which a window's project leaves out as
/// Codex's thread list leaves archived threads out, until a message
/// opens it again.
pub fn kept(row: Value) -> Option<Agent> {
    if !row["closed_ms"].is_null() {
        return None;
    }
    serde_json::from_value(row).ok()
}

/// Every agent `pattern` names but those closed, every page, with the
/// events' cursor read before the first page: following from it misses no
/// change.
pub async fn list(client: &Client, pattern: &str) -> agent_client::Result<(Vec<Agent>, i64)> {
    let mut listed = Vec::new();
    let mut after: Option<String> = None;
    let mut cursor = None;
    loop {
        let mut ask = json!({"name": pattern, "limit": 256, "open": true});
        if let Some(after) = &after {
            ask["after"] = json!(after);
        }
        let page = client.request("bots", ask).await?;
        cursor = cursor.or(page["cursor"].as_i64());
        listed.extend(
            (page["bots"].as_array().into_iter().flatten())
                .cloned()
                .filter_map(kept),
        );
        match page["next_after"].as_str() {
            Some(next) if after.as_deref() != Some(next) => after = Some(next.to_owned()),
            _ => break,
        }
    }
    Ok((listed, cursor.unwrap_or(0)))
}

/// The rows of `names`, read by name, 256 a request: the agents that
/// moved. One that is gone, or closed, is not among them.
pub async fn rows(client: &Client, names: &[String]) -> agent_client::Result<Vec<Agent>> {
    let mut rows = Vec::new();
    for chunk in names.chunks(256) {
        rows.extend(every(client, json!({"names": chunk, "limit": 256})).await?);
    }
    Ok(rows)
}

/// Every page of the `bots` listing `ask` names, each row [`kept`]: a page
/// ends at its row count or its size, whichever comes first.
pub async fn every(client: &Client, ask: Value) -> agent_client::Result<Vec<Agent>> {
    let mut rows = Vec::new();
    let mut after: Option<String> = None;
    loop {
        let mut ask = ask.clone();
        if let Some(after) = &after {
            ask["after"] = json!(after);
        }
        let page = client.request("bots", ask).await?;
        rows.extend(
            (page["bots"].as_array().into_iter().flatten())
                .cloned()
                .filter_map(kept),
        );
        match page["next_after"].as_str() {
            Some(next) if after.as_deref() != Some(next) => after = Some(next.to_owned()),
            _ => return Ok(rows),
        }
    }
}

/// What the follow heard since the window last took it.
#[derive(Default)]
pub struct Changes {
    /// A fresh snapshot, every agent the pattern names: taken at the start
    /// and after the follow said `pruned`.
    pub snapshot: Option<Vec<Agent>>,
    /// The agents an event moved since.
    pub moved: BTreeSet<String>,
}

/// Whether `pattern` names bots by a GLOB pattern, not one bot by name.
fn is_glob(pattern: &str) -> bool {
    pattern.contains(['*', '?', '['])
}

/// A window's follow of its project, on the window's own connection.
pub struct Follower {
    pub pattern: String,
    client: Arc<Client>,
    changes: Arc<Mutex<Changes>>,
    woken: Arc<Notify>,
    task: tokio::task::JoinHandle<()>,
}

impl Follower {
    /// Follow the agents `pattern` names on `client`, the window's
    /// connection, waking `woken` when something arrives that the window
    /// has not taken.
    pub fn start(client: Arc<Client>, pattern: String, woken: Arc<Notify>) -> Self {
        let changes = Arc::new(Mutex::new(Changes::default()));
        let task = tokio::spawn(snapshot(
            client.clone(),
            pattern.clone(),
            changes.clone(),
            woken.clone(),
        ));
        Self {
            pattern,
            client,
            changes,
            woken,
            task,
        }
    }

    /// Whether it follows on `client`: a window that connected again
    /// starts its follow again there.
    pub fn on(&self, client: &Arc<Client>) -> bool {
        Arc::ptr_eq(&self.client, client)
    }

    /// An event of the window's connection, heard. Whether the follow
    /// brought it, so it is not the window's own agent's to show: the
    /// pattern's events name it as `following`, and its notices as their
    /// `bot`. A pattern that is the window's own agent's name is heard
    /// through the window's own follow, whose events stay the window's.
    pub fn heard(&mut self, event: &Value) -> bool {
        let pattern = self.pattern.as_str();
        let brought = event["following"] == pattern;
        let named = event["bot"] == pattern;
        if !brought && !named {
            return false;
        }
        match event["event"].as_str() {
            // A gap retention made: the snapshot is the truth.
            Some("pruned") => {
                self.task.abort();
                self.task = tokio::spawn(snapshot(
                    self.client.clone(),
                    self.pattern.clone(),
                    self.changes.clone(),
                    self.woken.clone(),
                ));
            }
            Some(kind) if KINDS.contains(&kind) => {
                if let Some(bot) = event["bot"].as_str() {
                    let first = {
                        let mut changes = self.changes.lock().unwrap();
                        let first = changes.moved.is_empty() && changes.snapshot.is_none();
                        changes.moved.insert(bot.to_owned());
                        first
                    };
                    if first {
                        self.woken.notify_one();
                    }
                }
            }
            _ => {}
        }
        brought || (named && is_glob(pattern))
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

/// The snapshot, then the follow from its cursor; tried again, a little
/// later each time, until both are in. What moved meanwhile stays to be
/// read again. A pattern that is one agent's name is not followed again:
/// the window's own follow brings its events, and a second follow of the
/// same name on the connection would take its place.
async fn snapshot(
    client: Arc<Client>,
    pattern: String,
    changes: Arc<Mutex<Changes>>,
    woken: Arc<Notify>,
) {
    let mut pause = Duration::from_millis(250);
    loop {
        if let Ok((listed, cursor)) = list(&client, &pattern).await {
            changes.lock().unwrap().snapshot = Some(listed);
            woken.notify_one();
            if !is_glob(&pattern) {
                return;
            }
            let asked = json!({"bot": pattern, "after": cursor, "kinds": KINDS});
            if client.request("follow", asked).await.is_ok() {
                return;
            }
        }
        tokio::time::sleep(pause).await;
        pause = (pause * 2).min(Duration::from_secs(30));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};

    /// A daemon that lists any pattern's one agent, named for how many
    /// times it was listed, with the cursor it was listed at, and keeps
    /// every follow asked of it.
    /// A daemon that lists one agent a snapshot under the pattern's
    /// prefix, and keeps the `follow` asks and the `bots` asks it was sent.
    async fn listing_daemon() -> (Arc<Client>, Arc<Mutex<Vec<Value>>>, Arc<Mutex<Vec<Value>>>) {
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
        let follows = Arc::new(Mutex::new(Vec::new()));
        let asked_follows = follows.clone();
        let listings = Arc::new(Mutex::new(Vec::new()));
        let asked_listings = listings.clone();
        tokio::spawn(async move {
            let (stream, _) = listener.accept().await.unwrap();
            let (read, mut write) = stream.into_split();
            let ready = json!({"event": "ready", "protocol": agent_client::PROTOCOL,
                "providers": {}});
            write
                .write_all(format!("{ready}\n").as_bytes())
                .await
                .unwrap();
            let mut lines = BufReader::new(read).lines();
            let mut listed = 0;
            while let Ok(Some(line)) = lines.next_line().await {
                let asked: Value = serde_json::from_str(&line).unwrap();
                let id = asked["id"].clone();
                let said = match asked["op"].as_str() {
                    Some("bots") => {
                        asked_listings.lock().unwrap().push(asked.clone());
                        let prefix = asked["name"].as_str().unwrap().trim_end_matches('*');
                        let row = json!({"name": format!("{prefix}{listed}"), "head": 1});
                        listed += 1;
                        json!({"id": id, "result":
                            {"bots": [row], "cursor": 39 + listed, "next_after": null}})
                    }
                    Some("follow") => {
                        asked_follows.lock().unwrap().push(asked.clone());
                        json!({"id": id, "result": {"following": asked["bot"]}})
                    }
                    _ => json!({"id": id, "result": {}}),
                };
                write
                    .write_all(format!("{said}\n").as_bytes())
                    .await
                    .unwrap();
            }
        });
        let (client, _events) = Client::connect(&socket).await.unwrap();
        (client, follows, listings)
    }

    /// An agent `name` at history `head`, as a window keeps it.
    fn agent(name: &str, head: i64) -> Agent {
        Agent {
            name: name.into(),
            head: Some(head),
            ..Default::default()
        }
    }

    /// The next snapshot `follower` takes in.
    async fn next_snapshot(follower: &Follower, woken: &Notify) -> Vec<Agent> {
        loop {
            if let Some(snapshot) = follower.take().snapshot {
                return snapshot;
            }
            woken.notified().await;
        }
    }

    #[tokio::test]
    async fn a_follow_on_the_windows_connection_names_what_it_moved_and_snapshots_again_after_a_gap()
     {
        let (client, follows, listings) = listing_daemon().await;
        let woken = Arc::new(Notify::new());
        let mut follower = Follower::start(client, "shop.*".into(), woken.clone());
        // The snapshot, then the follow from the cursor it was listed at.
        let first = next_snapshot(&follower, &woken).await;
        assert_eq!(first, vec![agent("shop.0", 1)]);
        let snapshot = listings.lock().unwrap()[0].clone();
        while follows.lock().unwrap().is_empty() {
            tokio::task::yield_now().await;
        }
        let asked = follows.lock().unwrap()[0].clone();
        assert_eq!(
            asked,
            json!({"id": asked["id"], "op": "follow", "bot": "shop.*", "after": 40, "kinds": KINDS})
        );
        // The snapshot leaves closed agents out, as the follow hears them
        // close.
        assert_eq!(
            snapshot,
            json!({"id": snapshot["id"], "op": "bots", "name": "shop.*", "limit": 256, "open": true})
        );
        assert!(KINDS.contains(&"closed"));
        // What the follow brought is its own; the window's agent's events
        // are the window's, even of a kind the follow takes.
        let brought = json!({"event": "turn_finished", "bot": "shop.fix", "following": "shop.*"});
        assert!(follower.heard(&brought));
        let own = json!({"event": "turn_finished", "bot": "shop.lead"});
        assert!(!follower.heard(&own));
        let quiet = json!({"event": "text_delta", "bot": "shop.fix", "following": "shop.*"});
        assert!(follower.heard(&quiet));
        assert!(follower.heard(&json!({"event": "follow_live", "bot": "shop.*"})));
        assert_eq!(
            follower.take().moved,
            BTreeSet::from(["shop.fix".to_owned()])
        );
        // A gap: a fresh snapshot, not a catch up event by event, and the
        // follow again from its cursor.
        assert!(follower.heard(&json!({"event": "pruned", "bot": "shop.*", "before": 9})));
        let again = next_snapshot(&follower, &woken).await;
        assert_eq!(again, vec![agent("shop.1", 1)]);
        while follows.lock().unwrap().len() < 2 {
            tokio::task::yield_now().await;
        }
        assert_eq!(follows.lock().unwrap()[1]["after"], 41);
    }

    #[tokio::test]
    async fn a_project_of_the_windows_agent_alone_is_heard_through_its_own_follow() {
        let (client, follows, _) = listing_daemon().await;
        let woken = Arc::new(Notify::new());
        let mut follower = Follower::start(client, "solo".into(), woken.clone());
        assert_eq!(
            next_snapshot(&follower, &woken).await,
            vec![agent("solo0", 1)]
        );
        // Its events are the window's to show, and move it here too.
        assert!(!follower.heard(&json!({"event": "turn_finished", "bot": "solo"})));
        assert_eq!(follower.take().moved, BTreeSet::from(["solo".to_owned()]));
        // No second follow of its name, which would take the window's place.
        tokio::time::sleep(Duration::from_millis(20)).await;
        assert!(follows.lock().unwrap().is_empty());
    }

    #[test]
    fn a_closed_agent_is_left_out_and_shows_again_once_open() {
        let closed = json!({"name": "shop.fix", "head": 3, "closed_ms": 1700000000000_i64});
        assert_eq!(kept(closed), None);
        let open = json!({"name": "shop.fix", "head": 4});
        assert_eq!(kept(open), Some(agent("shop.fix", 4)));
    }
}
