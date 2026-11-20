//! Where a window's project agents stand, kept current by the daemon's
//! events instead of listed again every few seconds. A window follows its
//! project from now for the events that move an agent's state alone (a
//! turn starting, waiting, resuming, held or ending, a call held for
//! approval, an agent made or deleted), and once the follow is live takes
//! one snapshot, every agent its project's pattern names, so it never
//! receives every tool event of every agent to keep a count. Live first,
//! then the listing: an event the daemon keeps for no replay (an agent
//! `deleted`) still reaches a follow that is live, so nothing that moves
//! while the listing is read falls between the two. What moved meanwhile
//! is read again by name. Codex's app server pushing a thread's status
//! instead of clients listing threads again is the same shape.
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
#[derive(Clone, Debug, Default, PartialEq, serde::Serialize, serde::Deserialize)]
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
    /// Whether it is closed (its `closed_ms` set): never kept.
    #[serde(rename = "closed_ms", deserialize_with = "is_set")]
    pub closed: bool,
}

/// Whether a row's field is set, whatever it holds.
fn is_set<'de, D: serde::Deserializer<'de>>(field: D) -> Result<bool, D::Error> {
    use serde::Deserialize;
    Ok(Option::<serde::de::IgnoredAny>::deserialize(field)?.is_some())
}

/// A page of the `bots` listing, read for the fields a window keeps of
/// each row, the rest of each passed over unbuilt.
#[derive(serde::Deserialize)]
struct Page {
    #[serde(default)]
    bots: Vec<Agent>,
    next_after: Option<String>,
}

/// How a turn ended, as a `bots` row's `last` says it.
#[derive(Clone, Debug, Default, PartialEq, Hash, serde::Serialize, serde::Deserialize)]
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

/// Whether a window keeps `agent`, a `bots` row: not once it is closed,
/// which a window's project leaves out as Codex's thread list leaves
/// archived threads out, until a message opens it again.
fn kept(agent: &Agent) -> bool {
    !agent.closed
}

/// Every agent `pattern` names but those closed, every page.
pub async fn list(client: &Client, pattern: &str) -> agent_client::Result<Vec<Agent>> {
    let mut listed = Vec::new();
    let mut after: Option<String> = None;
    loop {
        let mut ask = json!({"name": pattern, "limit": 256, "open": true});
        if let Some(after) = &after {
            ask["after"] = json!(after);
        }
        let page: Page = client.request_as("bots", ask).await?;
        listed.extend(page.bots.into_iter().filter(kept));
        match page.next_after {
            Some(next) if after.as_ref() != Some(&next) => after = Some(next),
            _ => return Ok(listed),
        }
    }
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
        let page: Page = client.request_as("bots", ask).await?;
        rows.extend(page.bots.into_iter().filter(kept));
        match page.next_after {
            Some(next) if after.as_ref() != Some(&next) => after = Some(next),
            _ => return Ok(rows),
        }
    }
}

/// What the follow heard since the window last took it.
#[derive(Default)]
pub struct Changes {
    /// A fresh snapshot, every agent the pattern names: taken at the start,
    /// once the follow is live.
    pub snapshot: Option<Vec<Agent>>,
    /// The agents an event moved since.
    pub moved: BTreeSet<String>,
}

/// What the follow keeps for the window, and whether a snapshot is still
/// to come: what moved meanwhile waits for it, so the window reads no
/// move before the listing it lands in.
#[derive(Default)]
struct Kept {
    changes: Changes,
    listing: bool,
}

/// Whether `pattern` names bots by a GLOB pattern, not one bot by name.
fn is_glob(pattern: &str) -> bool {
    pattern.contains(['*', '?', '['])
}

/// A window's follow of its project, on the window's own connection.
pub struct Follower {
    pub pattern: String,
    client: Arc<Client>,
    kept: Arc<Mutex<Kept>>,
    woken: Arc<Notify>,
    /// Told when the follow reaches the present (`follow_live`).
    live: Arc<Notify>,
    task: tokio::task::JoinHandle<()>,
}

impl Follower {
    /// Follow the agents `pattern` names on `client`, the window's
    /// connection, waking `woken` when something arrives that the window
    /// has not taken.
    pub fn start(client: Arc<Client>, pattern: String, woken: Arc<Notify>) -> Self {
        let kept = Arc::new(Mutex::new(Kept {
            listing: true,
            ..Kept::default()
        }));
        let live = Arc::new(Notify::new());
        let task = tokio::spawn(snapshot(
            client.clone(),
            pattern.clone(),
            (kept.clone(), woken.clone()),
            Some(live.clone()),
        ));
        Self {
            pattern,
            client,
            kept,
            woken,
            live,
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
            // The follow is live: the snapshot is read from here.
            Some("follow_live") if is_glob(pattern) => self.live.notify_one(),
            // A gap retention made. One while the snapshot is still to
            // come is no matter: it is read once the follow is live, and is
            // the truth. One after it: a fresh snapshot, not a catch up
            // event by event.
            Some("pruned") => {
                let mut kept = self.kept.lock().unwrap();
                if !kept.listing {
                    kept.listing = true;
                    drop(kept);
                    self.task.abort();
                    self.task = tokio::spawn(snapshot(
                        self.client.clone(),
                        self.pattern.clone(),
                        (self.kept.clone(), self.woken.clone()),
                        None,
                    ));
                }
            }
            Some(kind) if KINDS.contains(&kind) => {
                if let Some(bot) = event["bot"].as_str() {
                    let first = {
                        let mut kept = self.kept.lock().unwrap();
                        let changes = &mut kept.changes;
                        let first = changes.moved.is_empty() && changes.snapshot.is_none();
                        changes.moved.insert(bot.to_owned());
                        first && !kept.listing
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

    /// What arrived since the last take: nothing while the snapshot is
    /// still to come.
    pub fn take(&self) -> Changes {
        let mut kept = self.kept.lock().unwrap();
        match kept.listing {
            true => Changes::default(),
            false => std::mem::take(&mut kept.changes),
        }
    }

    /// As though an event had moved `bot`.
    #[cfg(test)]
    pub fn moved(&self, bot: &str) {
        let mut kept = self.kept.lock().unwrap();
        kept.changes.moved.insert(bot.to_owned());
    }

    /// Whether the snapshot is in and not yet taken.
    #[cfg(test)]
    pub fn snapshot_waits(&self) -> bool {
        let kept = self.kept.lock().unwrap();
        !kept.listing && kept.changes.snapshot.is_some()
    }
}

/// A connection of its own following the agents `pattern` names, with the
/// task that takes its events to the follow; `woken` is told when
/// something arrives, and when the connection goes.
pub async fn follow_apart(
    socket: &std::path::Path,
    pattern: &str,
    woken: &Arc<Notify>,
) -> Option<(
    Arc<Client>,
    Arc<Mutex<Follower>>,
    tokio::task::JoinHandle<()>,
)> {
    let (client, mut events) = Client::connect(socket).await.ok()?;
    let follower = Arc::new(Mutex::new(Follower::start(
        client.clone(),
        pattern.to_owned(),
        woken.clone(),
    )));
    let (heard, woken) = (follower.clone(), woken.clone());
    let drain = tokio::spawn(async move {
        while let Some(event) = events.recv().await {
            if let Ok(mut follower) = heard.lock() {
                follower.heard(&event);
            }
        }
        woken.notify_one();
    });
    Some((client, follower, drain))
}

impl Drop for Follower {
    fn drop(&mut self) {
        self.task.abort();
    }
}

/// The follow from now, once it is `live`, then the snapshot; tried
/// again, a little later each time, until both are in. What moved
/// meanwhile stays to be read again. A follow already live (`live` none)
/// is not asked again, nor is a pattern that is one agent's name: the
/// window's own follow brings its events, and a second follow of the
/// same name on the connection would take its place.
async fn snapshot(
    client: Arc<Client>,
    pattern: String,
    (kept, woken): (Arc<Mutex<Kept>>, Arc<Notify>),
    mut live: Option<Arc<Notify>>,
) {
    if !is_glob(&pattern) {
        live = None;
    }
    let mut pause = Duration::from_millis(250);
    loop {
        if let Some(told) = &live {
            let asked = json!({"bot": pattern, "since_ms": epoch_ms(), "kinds": KINDS});
            if client.request("follow", asked).await.is_ok() {
                told.notified().await;
                live = None;
            }
        }
        if live.is_none()
            && let Ok(listed) = list(&client, &pattern).await
        {
            let mut held = kept.lock().unwrap();
            held.changes.snapshot = Some(listed);
            held.listing = false;
            drop(held);
            woken.notify_one();
            return;
        }
        tokio::time::sleep(pause).await;
        pause = (pause * 2).min(Duration::from_secs(30));
    }
}

/// Now, in Unix ms, as the daemon writes an event's time.
fn epoch_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |since| since.as_millis() as i64)
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

    /// The follow `follows` was asked for, once it is.
    async fn nth_follow(follows: &Mutex<Vec<Value>>, nth: usize) -> Value {
        loop {
            if let Some(asked) = follows.lock().unwrap().get(nth) {
                return asked.clone();
            }
            tokio::task::yield_now().await;
        }
    }

    #[tokio::test]
    async fn a_follow_on_the_windows_connection_names_what_it_moved_and_snapshots_again_after_a_gap()
     {
        let (client, follows, listings) = listing_daemon().await;
        let woken = Arc::new(Notify::new());
        let mut follower = Follower::start(client, "shop.*".into(), woken.clone());
        // The follow from now first, and no listing until it is live.
        let asked = nth_follow(&follows, 0).await;
        let since = asked["since_ms"].as_i64().unwrap();
        assert!(since > 0 && since <= epoch_ms());
        assert_eq!(
            asked,
            json!({"id": asked["id"], "op": "follow", "bot": "shop.*", "since_ms": since,
                "kinds": KINDS})
        );
        tokio::time::sleep(Duration::from_millis(20)).await;
        assert!(listings.lock().unwrap().is_empty());
        assert!(follower.heard(&json!({"event": "follow_live", "bot": "shop.*"})));
        let first = next_snapshot(&follower, &woken).await;
        assert_eq!(first, vec![agent("shop.0", 1)]);
        let snapshot = listings.lock().unwrap()[0].clone();
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
        assert_eq!(
            follower.take().moved,
            BTreeSet::from(["shop.fix".to_owned()])
        );
        // A gap: a fresh snapshot, not a catch up event by event, from the
        // follow that is live already.
        assert!(follower.heard(&json!({"event": "pruned", "bot": "shop.*", "before": 9})));
        let again = next_snapshot(&follower, &woken).await;
        assert_eq!(again, vec![agent("shop.1", 1)]);
        tokio::time::sleep(Duration::from_millis(20)).await;
        assert_eq!(follows.lock().unwrap().len(), 1);
    }

    #[tokio::test]
    async fn an_agent_deleted_while_the_snapshot_is_read_is_read_again_after_it() {
        let (client, follows, _) = listing_daemon().await;
        let woken = Arc::new(Notify::new());
        let mut follower = Follower::start(client, "shop.*".into(), woken.clone());
        nth_follow(&follows, 0).await;
        // A gap in the follow's short replay is no matter: the listing, read
        // once it is live, is the truth.
        assert!(follower.heard(&json!({"event": "pruned", "bot": "shop.*", "before": 9})));
        // Deleted before the listing is in: the daemon keeps no `deleted`
        // to replay, so only a follow that is live hears it. It waits for
        // the snapshot, not read ahead of it.
        let deleted = json!({"event": "deleted", "bot": "shop.0", "following": "shop.*"});
        assert!(follower.heard(&deleted));
        assert!(follower.take().moved.is_empty());
        assert!(follower.heard(&json!({"event": "follow_live", "bot": "shop.*"})));
        loop {
            let changes = follower.take();
            if let Some(snapshot) = changes.snapshot {
                assert_eq!(snapshot, vec![agent("shop.0", 1)]);
                // Read again by name with the snapshot: gone, it leaves.
                assert_eq!(changes.moved, BTreeSet::from(["shop.0".to_owned()]));
                break;
            }
            assert!(changes.moved.is_empty());
            woken.notified().await;
        }
        assert_eq!(follows.lock().unwrap().len(), 1);
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
        let row = |row: Value| serde_json::from_value::<Agent>(row).unwrap();
        let closed = row(json!({"name": "shop.fix", "head": 3, "closed_ms": 1700000000000_i64}));
        assert!(!kept(&closed));
        let open = row(json!({"name": "shop.fix", "head": 4, "closed_ms": null, "unread": 2}));
        assert!(kept(&open));
        assert_eq!(open, agent("shop.fix", 4));
    }
}
