//! Client-side state built from the daemon's event stream, and the actions the
//! keys trigger. Everything here is derived from protocol events plus the bot
//! records the daemon returns; no other channel exists.
use crate::client::{Client, Error, Result};
use crate::items::{self, Entry};
use serde_json::{Value, json};
use std::{
    collections::{BTreeMap, HashMap},
    sync::Arc,
    time::{Duration, Instant},
};

const LAZY_ITEMS: usize = 400;
/// Decoded items kept around the end of a transcript read live; bodies
/// beyond it fold back into their nodes and a scroll up loads them again.
const WINDOW: usize = 3 * LAZY_ITEMS;
/// Notifications applied per loop pass before input and drawing get a turn.
pub const DRAIN: usize = 256;
pub const TOAST: Duration = Duration::from_millis(2200);

#[derive(Debug, Default, Clone)]
pub struct Bot {
    pub name: String,
    /// The daemon's store-wide identity: a recreated name is a new bot, and
    /// nothing kept for the old one may leak into it.
    pub id: Option<i64>,
    pub status: String,
    pub running_turn: Option<i64>,
    pub model: String,
    pub workspace: Option<String>,
    /// Inferred from the shell call that ran `agent run --new --bot NAME`
    /// until the daemon records a creator itself.
    pub parent: Option<String>,
    pub waiting_on: Vec<String>,
    pub turn_started: Option<Instant>,
    pub elapsed: Option<Duration>,
}

/// One transcript item. Nodes are fetched lazily for the bot on screen.
#[derive(Debug, Clone)]
pub enum Item {
    User(String),
    Text(String),
    Thought {
        text: String,
        secs: u64,
    },
    Tool {
        call_id: String,
        name: String,
        summary: String,
        args: String,
        background: bool,
        spawns: bool,
        done: bool,
        started: Option<Instant>,
        took: Option<Duration>,
    },
    Output(String),
    Note(String),
    /// A peer this bot created; rendered as a card, opened as a peek.
    Peer(String),
    /// A background command this bot started; the same card shape.
    Proc {
        handle: String,
        cmd: String,
        done: Option<String>,
        open: bool,
    },
    Node {
        node: i64,
        call_id: Option<String>,
    },
}

#[derive(Debug, Default)]
pub struct Transcript {
    pub items: Vec<(Option<i64>, Item)>,
    /// Parallel to `items`: the history node an item was decoded from and
    /// that node's call id, `None` for items built from live events. A
    /// decoded body folds back into its node when it leaves the window.
    origin: Vec<Option<(i64, Option<String>)>>,
    /// How many items are still bare nodes; loads skip a transcript at zero
    /// instead of scanning it on every event.
    pub nodes: usize,
    /// Kept in step with `items`, so the key bar never scans the history.
    pub thoughts: usize,
    pub long_outputs: usize,
    /// Peers whose cards are on this transcript, in card order.
    pub peers: Vec<String>,
    pub text: String,
    pub thinking: String,
    pub thinking_since: Option<Instant>,
    pub streaming_turn: Option<i64>,
}

impl Transcript {
    /// Append an item built from a live event.
    pub fn add(&mut self, turn: Option<i64>, item: Item) {
        self.count(&item, 1);
        self.items.push((turn, item));
        self.origin.push(None);
    }
    /// Replace the bare node at `at` with what it decoded to.
    fn decode(&mut self, at: usize, entries: Vec<(Option<i64>, Item)>) {
        let Item::Node { node, call_id } = &self.items[at].1 else {
            return;
        };
        let from = Some((*node, call_id.clone()));
        let (_, bare) = self.items.remove(at);
        self.origin.remove(at);
        self.count(&bare, -1);
        for item in &entries {
            self.count(&item.1, 1);
        }
        let n = entries.len();
        self.items.splice(at..at, entries);
        self.origin.splice(at..at, std::iter::repeat_n(from, n));
    }
    /// Remove the item at `at`.
    pub fn take(&mut self, at: usize) -> (Option<i64>, Item) {
        let entry = self.items.remove(at);
        self.origin.remove(at);
        self.count(&entry.1, -1);
        entry
    }
    /// Fold decoded bodies older than the newest `keep` items back into their
    /// nodes. Whole nodes only: a run split by the boundary folds entirely,
    /// so a later decode cannot sit next to its own remainder.
    pub fn evict(&mut self, keep: usize) {
        let len = self.items.len();
        if len <= keep {
            return;
        }
        let limit = len - keep;
        let items = std::mem::take(&mut self.items);
        let origin = std::mem::take(&mut self.origin);
        let mut folding: Option<i64> = None;
        for (i, ((turn, item), from)) in items.into_iter().zip(origin).enumerate() {
            match from {
                Some((node, _)) if folding == Some(node) => self.count(&item, -1),
                Some((node, call_id)) if i < limit => {
                    folding = Some(node);
                    self.count(&item, -1);
                    let bare = Item::Node { node, call_id };
                    self.count(&bare, 1);
                    self.items.push((turn, bare));
                    self.origin.push(None);
                }
                _ => {
                    folding = None;
                    self.items.push((turn, item));
                    self.origin.push(from);
                }
            }
        }
    }
    fn count(&mut self, item: &Item, delta: isize) {
        let bump = |n: &mut usize| *n = (*n as isize + delta).max(0) as usize;
        match item {
            Item::Node { .. } => bump(&mut self.nodes),
            Item::Thought { .. } => bump(&mut self.thoughts),
            Item::Output(s) if s.lines().take(3).count() > 2 => bump(&mut self.long_outputs),
            Item::Peer(who) if delta > 0 && !self.peers.contains(who) => {
                self.peers.push(who.clone());
            }
            _ => {}
        }
    }
}

/// A width moving from one value to another over a short time.
#[derive(Debug, Clone)]
pub struct Tween {
    from: u16,
    to: u16,
    start: Instant,
    dur: Duration,
}
impl Tween {
    pub fn at(value: u16) -> Self {
        Self {
            from: value,
            to: value,
            start: Instant::now(),
            dur: Duration::ZERO,
        }
    }
    pub fn go(&mut self, to: u16, dur: Duration) {
        let now = self.value();
        *self = Self {
            from: now,
            to,
            start: Instant::now(),
            dur,
        };
    }
    pub fn value(&self) -> u16 {
        if self.dur.is_zero() {
            return self.to;
        }
        let t = (self.start.elapsed().as_secs_f32() / self.dur.as_secs_f32()).min(1.0);
        let eased = 1.0 - (1.0 - t) * (1.0 - t) * (1.0 - t);
        (self.from as f32 + (self.to as f32 - self.from as f32) * eased).round() as u16
    }
    pub fn active(&self) -> bool {
        !self.dur.is_zero() && self.start.elapsed() < self.dur
    }
    pub fn target(&self) -> u16 {
        self.to
    }
}

#[derive(Debug, Default)]
pub struct Picker {
    pub query: String,
    pub sel: usize,
}

pub struct Ui {
    pub rail: Tween,
    pub peek: Option<String>,
    pub peek_w: Tween,
    pub picker: Option<Picker>,
    pub help: bool,
    pub thoughts: bool,
    pub output: bool,
    pub toast: Option<(String, Instant)>,
    pub scroll: usize,
    pub peek_scroll: usize,
    pub motion: bool,
    /// Mouse captured: the wheel scrolls panes; released: the terminal selects text.
    pub mouse: bool,
    /// A near-background gray for code and tool output, chosen from the
    /// terminal's reported theme; none when the terminal did not answer.
    pub shade: Option<ratatui::style::Color>,
}

pub struct App {
    pub client: Option<Arc<Client>>,
    pub socket: std::path::PathBuf,
    pub bots: BTreeMap<String, Bot>,
    pub transcripts: HashMap<String, Transcript>,
    pub selected: String,
    pub input: String,
    pub cursor: i64,
    pub live: bool,
    pub ui: Ui,
    /// No bot chosen yet: pick the first root once replay has built the tree.
    pub auto_select: bool,
    /// The follower lagged and was dropped; the loop attaches again from the cursor.
    pub reattach: bool,
    /// Replay that arrived while the attach snapshot was still paging; the
    /// loop applies it before reading the live receiver.
    pub backlog: Vec<Value>,
    pub default_model: Option<String>,
    pub default_workspace: String,
    pub instructions: String,
    /// What the instructions were composed from, for the create notice.
    pub instructions_note: String,
    pub tools: Vec<String>,
}

pub const RAIL_W: u16 = 26;
pub const SLIDE: Duration = Duration::from_millis(160);

impl App {
    pub fn new(
        socket: std::path::PathBuf,
        default_model: Option<String>,
        default_workspace: String,
        motion: bool,
    ) -> Self {
        Self {
            client: None,
            socket,
            bots: BTreeMap::new(),
            transcripts: HashMap::new(),
            selected: String::new(),
            input: String::new(),
            cursor: 0,
            live: false,
            ui: Ui {
                rail: Tween::at(0),
                peek: None,
                peek_w: Tween::at(0),
                picker: None,
                help: false,
                thoughts: false,
                output: false,
                toast: None,
                scroll: 0,
                peek_scroll: 0,
                motion,
                mouse: true,
                shade: None,
            },
            auto_select: true,
            reattach: false,
            backlog: Vec::new(),
            default_model,
            default_workspace,
            instructions: String::new(),
            instructions_note: String::new(),
            tools: ["shell", "read", "write", "edit", "wait", "history"]
                .map(String::from)
                .to_vec(),
        }
    }

    /// The shared client policy for this workspace: preamble, AGENTS.md
    /// files, skills. Too much text falls back to the preamble and says so.
    pub fn compose_instructions(&mut self) {
        match agent_client::policy::instructions(std::path::Path::new(&self.default_workspace)) {
            Ok(composed) => {
                self.instructions_note = format!(
                    "preamble + {} AGENTS.md + {} skills",
                    composed.sources.len(),
                    composed.skills.len()
                );
                self.instructions = composed.text;
            }
            Err(error) => {
                self.instructions_note = format!("preamble only: {error}");
                self.instructions = agent_client::policy::PREAMBLE.to_owned();
            }
        }
    }

    pub fn bot(&self) -> Option<&Bot> {
        self.bots.get(&self.selected)
    }
    fn client(&self) -> Result<Arc<Client>> {
        self.client.clone().ok_or(Error::new("detached"))
    }
    pub fn toast(&mut self, text: impl Into<String>) {
        self.ui.toast = Some((text.into(), Instant::now()));
    }
    pub fn animating(&self) -> bool {
        self.ui.rail.active() || self.ui.peek_w.active()
    }
    /// Something is in flight: a redraw tick is worth paying for. A failed
    /// or interrupted bot keeps its glyph but costs nothing while it sits.
    pub fn busy(&self) -> bool {
        self.bots.values().any(|b| is_active(&b.status))
    }
    pub fn slide(&self) -> Duration {
        if self.ui.motion {
            SLIDE
        } else {
            Duration::ZERO
        }
    }

    /// Bots as a tree by creator, depth first: (bot, depth, is last child, ancestors' last flags).
    /// Bots as a tree by creator, depth first: (bot, depth, prefix). The
    /// prefix is the rail's connector text; it is built from the parent's
    /// continuation, so a deep chain costs one string per row and no
    /// recursion.
    pub fn tree<'a>(&'a self) -> Vec<(&'a Bot, usize, String)> {
        type Stack<'a> = Vec<(&'a Bot, usize, bool, String)>;
        // One pass builds the children index; the walk is linear in the fleet.
        let mut children: HashMap<Option<&'a str>, Vec<&'a Bot>> = HashMap::new();
        for b in self.bots.values() {
            let parent = b.parent.as_deref().filter(|p| self.bots.contains_key(*p));
            children.entry(parent).or_default().push(b);
        }
        fn push_kids<'a>(
            children: &HashMap<Option<&'a str>, Vec<&'a Bot>>,
            seen: &std::collections::HashSet<&'a str>,
            stack: &mut Stack<'a>,
            parent: Option<&'a str>,
            depth: usize,
            cont: &str,
        ) {
            let Some(kids) = children.get(&parent) else {
                return;
            };
            let kids: Vec<&'a Bot> = kids
                .iter()
                .copied()
                .filter(|b| !seen.contains(b.name.as_str()))
                .collect();
            let n = kids.len();
            for (i, kid) in kids.into_iter().enumerate().rev() {
                stack.push((kid, depth, i + 1 == n, cont.to_owned()));
            }
        }
        let mut out: Vec<(&'a Bot, usize, String)> = Vec::new();
        let mut seen: std::collections::HashSet<&'a str> = std::collections::HashSet::new();
        // Explicit stack of (bot, depth, is last sibling, parent's continuation).
        let mut stack: Stack<'a> = Vec::new();
        push_kids(&children, &seen, &mut stack, None, 0, "");
        loop {
            while let Some((b, depth, last, cont)) = stack.pop() {
                if !seen.insert(b.name.as_str()) {
                    continue;
                }
                // The continuation stops growing past a few levels: the rail
                // is narrow, and a chain of thousands must not cost a string
                // of thousands per row.
                let (prefix, next) = if depth == 0 {
                    (String::new(), String::new())
                } else if depth > 6 {
                    (
                        format!("{cont}{}", if last { "└ " } else { "├ " }),
                        cont.clone(),
                    )
                } else {
                    (
                        format!("{cont}{}", if last { "└ " } else { "├ " }),
                        format!("{cont}{}", if last { "  " } else { "│ " }),
                    )
                };
                out.push((b, depth, prefix));
                push_kids(
                    &children,
                    &seen,
                    &mut stack,
                    Some(b.name.as_str()),
                    depth + 1,
                    &next,
                );
            }
            // Whatever the walk did not reach still needs a row: a creator cycle
            // left by delete-and-recreate. Each such bot roots its own subtree.
            match self.bots.values().find(|b| !seen.contains(b.name.as_str())) {
                Some(orphan) => stack.push((orphan, 0, true, String::new())),
                None => break,
            }
        }
        out
    }

    // ----- lifecycle -----

    pub async fn attach(&mut self) -> Result<tokio::sync::mpsc::Receiver<Value>> {
        let (client, mut events) = Client::connect(&self.socket).await?;
        self.client = Some(client.clone());
        // Subscribe before taking the snapshot: a deletion is a live-only
        // notice, so anything that happens after the list is seen on the
        // stream, and nothing can fall between the two.
        client
            .request("follow", json!({"bot": "*", "after": self.cursor}))
            .await?;
        // Page the snapshot on a task while this loop drains the replay the
        // subscription is already sending. A large store replays more than
        // the client queue holds; left unread, it would drop the session
        // under the very request that is listing it.
        let pager = {
            let client = client.clone();
            tokio::spawn(async move {
                let mut bots = Vec::new();
                let mut after: Option<String> = None;
                loop {
                    let page = client
                        .request("bots", json!({"after": after, "limit": 256}))
                        .await?;
                    bots.extend(page["bots"].as_array().cloned().unwrap_or_default());
                    match page["next_after"].as_str() {
                        Some(next) => after = Some(next.to_owned()),
                        None => break,
                    }
                }
                Ok::<Vec<Value>, Error>(bots)
            })
        };
        tokio::pin!(pager);
        let mut backlog = Vec::new();
        let records = loop {
            tokio::select! {
                paged = &mut pager => break paged.map_err(|_| Error::new("attach_failed"))??,
                event = events.recv() => match event {
                    Some(event) => backlog.push(event),
                    None => return Err(Error::new("daemon_disconnected")),
                },
            }
        };
        let mut listed: std::collections::HashSet<String> = std::collections::HashSet::new();
        for record in &records {
            self.upsert(record);
            if let Some(name) = record["name"].as_str() {
                listed.insert(name.to_owned());
            }
        }
        self.backlog = backlog;
        // The snapshot is authoritative: a bot deleted while this client had
        // no session is gone from it, and its live-only `deleted` notice
        // cannot be replayed. Anything created after the snapshot arrives as
        // an event on the subscription taken before it.
        let gone: Vec<String> = self
            .bots
            .keys()
            .filter(|n| !listed.contains(*n))
            .cloned()
            .collect();
        for name in gone {
            self.bots.remove(&name);
            self.transcripts.remove(&name);
            if self.ui.peek.as_deref() == Some(name.as_str()) {
                self.ui.peek = None;
            }
        }
        if self.selected.is_empty() || !self.bots.contains_key(&self.selected) {
            self.selected = self.bots.keys().next().cloned().unwrap_or_default();
        }
        self.live = false;
        Ok(events)
    }

    /// What to remember for next time, and how to come back to it.
    pub fn session(&self) -> crate::session::Session {
        crate::session::Session {
            selected: (!self.selected.is_empty()).then(|| self.selected.clone()),
            peek: self.ui.peek.clone(),
            rail: self.ui.rail.target() > 0,
            thoughts: self.ui.thoughts,
            output: self.ui.output,
        }
    }
    pub fn restore(&mut self, saved: &crate::session::Session) {
        if let Some(name) = &saved.selected
            && self.bots.contains_key(name)
        {
            self.selected = name.clone();
            self.auto_select = false;
        }
        if let Some(peek) = &saved.peek
            && self.bots.contains_key(peek)
        {
            self.ui.peek = Some(peek.clone());
            self.ui.peek_w = Tween::at(44);
        }
        if saved.rail {
            self.ui.rail = Tween::at(RAIL_W);
        }
        self.ui.thoughts = saved.thoughts;
        self.ui.output = saved.output;
    }

    fn upsert(&mut self, record: &Value) {
        let Some(name) = record["name"].as_str() else {
            return;
        };
        let id = record["id"].as_i64();
        if let Some(existing) = self.bots.get(name)
            && existing.id.is_some()
            && id.is_some()
            && existing.id != id
        {
            // Same name, different identity: everything known about the old
            // bot belongs to the old bot.
            self.bots.remove(name);
            self.transcripts.remove(name);
        }
        let bot = self.bots.entry(name.to_owned()).or_default();
        bot.name = name.to_owned();
        if id.is_some() {
            bot.id = id;
        }
        let status = record["status"].as_str().unwrap_or("?");
        bot.status = if status == "completed" {
            "idle".into()
        } else {
            status.to_owned()
        };
        bot.running_turn = record["running_turn"].as_i64();
        bot.model = format!(
            "{}/{}",
            record["provider"].as_str().unwrap_or("?"),
            record["model"].as_str().unwrap_or("?")
        );
        bot.workspace = record["workspace"].as_str().map(str::to_owned);
        if let Some(creator) = record["created_by"].as_str() {
            bot.parent = Some(creator.to_owned());
        }
    }

    async fn refresh_bot(&mut self, name: &str) {
        if let Ok(client) = self.client()
            && let Ok(record) = client.request("resume", json!({"bot": name})).await
        {
            self.upsert(&record);
        }
    }

    fn push(&mut self, bot: &str, turn: Option<i64>, item: Item) {
        self.transcripts
            .entry(bot.to_owned())
            .or_default()
            .add(turn, item);
    }

    /// Apply one notification from the daemon.
    pub async fn event(&mut self, event: Value) {
        let kind = event["event"].as_str().unwrap_or("").to_owned();
        let bot = event["bot"].as_str().unwrap_or("").to_owned();
        let turn = event["turn"].as_i64();
        if let Some(cursor) = event["cursor"].as_i64() {
            self.cursor = self.cursor.max(cursor);
        }
        let data = event["data"].clone();
        match kind.as_str() {
            "follow_live" => {
                self.live = true;
                if self.auto_select
                    && let Some((root, ..)) = self.tree().first()
                {
                    let name = root.name.clone();
                    self.selected = name;
                }
                self.auto_select = false;
            }
            "follow_lagged" => {
                self.client = None;
                self.live = false;
                self.reattach = true;
                self.toast("event stream lagged; attaching again");
            }
            "text_delta" => {
                let t = self.transcripts.entry(bot).or_default();
                t.streaming_turn = turn;
                t.text.push_str(event["text"].as_str().unwrap_or(""));
            }
            "thinking_delta" => {
                let t = self.transcripts.entry(bot).or_default();
                t.streaming_turn = turn;
                if t.thinking_since.is_none() {
                    t.thinking_since = Some(Instant::now());
                }
                t.thinking.push_str(event["text"].as_str().unwrap_or(""));
            }
            "created" | "forked" => {
                // The snapshot already holds every bot that existed at attach,
                // creator included; only a bot born after it needs a fetch.
                // Replaying a 10,000-bot store must not cost 10,000 requests.
                if !self.bots.contains_key(&bot) {
                    self.refresh_bot(&bot).await;
                }
                if let Some(creator) = data["created_by"].as_str()
                    && let Some(b) = self.bots.get_mut(&bot)
                {
                    b.parent = Some(creator.to_owned());
                }
                // Lineage comes from the daemon's record or the event, never
                // from guessing at shell text; a bot without one is a root.
                let declared = self.bots.get(&bot).and_then(|b| b.parent.clone());
                if kind == "created"
                    && let Some(parent) = declared
                    && self.bots.contains_key(&parent)
                {
                    if let Some(b) = self.bots.get_mut(&bot) {
                        b.parent = Some(parent.clone());
                    }
                    let pturn = self.bots.get(&parent).and_then(|p| p.running_turn);
                    self.push(&parent, pturn, Item::Peer(bot.clone()));
                }
                if kind == "forked" {
                    let source = data["source"].as_str().unwrap_or("?").to_owned();
                    self.push(&bot, None, Item::Note(format!("forked from {source}")));
                }
            }
            "accepted" => {
                if let Some(b) = self.bots.get_mut(&bot) {
                    b.status = "running".into();
                    b.running_turn = turn;
                    b.waiting_on.clear();
                    // Replayed events carry no clock; only live turns get timed.
                    b.turn_started = self.live.then(Instant::now);
                    b.elapsed = None;
                }
                if let Some(node) = data["node"].as_i64() {
                    self.push(
                        &bot,
                        turn,
                        Item::Node {
                            node,
                            call_id: None,
                        },
                    );
                }
            }
            "queued" => {
                // `ready` waits for a daemon-wide slot with nothing else
                // running on the bot; `queued` sits behind the bot's own turn.
                let state = data["status"].as_str().unwrap_or("queued");
                let behind_own = self
                    .bots
                    .get(&bot)
                    .is_some_and(|b| b.running_turn.is_some() || is_active(&b.status));
                if !behind_own && let Some(b) = self.bots.get_mut(&bot) {
                    b.status = state.to_owned();
                }
                let note = if behind_own {
                    "queued behind the running turn"
                } else {
                    "queued for a slot"
                };
                self.push(&bot, turn, Item::Note(note.into()));
            }
            "message" => {
                let t = self.transcripts.entry(bot.clone()).or_default();
                if t.streaming_turn == turn {
                    if !t.thinking.is_empty() {
                        let secs = t.thinking_since.map(|s| s.elapsed().as_secs()).unwrap_or(0);
                        let text = std::mem::take(&mut t.thinking);
                        t.thinking_since = None;
                        t.add(turn, Item::Thought { text, secs });
                    }
                    t.text.clear();
                }
                if let Some(node) = data["node"].as_i64() {
                    self.push(
                        &bot,
                        turn,
                        Item::Node {
                            node,
                            call_id: None,
                        },
                    );
                }
            }
            "tool_started" => {
                let name = data["name"].as_str().unwrap_or("tool").to_owned();
                let args = data["arguments"].as_str().unwrap_or("").to_owned();
                let summary = items::call_summary(&name, &args);
                let call_id = data["call_id"].as_str().unwrap_or("").to_owned();
                let parsed: Value = serde_json::from_str(&args).unwrap_or(Value::Null);
                let background = name == "shell" && parsed["background"].as_bool() == Some(true);
                let spawns = name == "shell" && parsed["command"].as_str().is_some_and(spawns_peer);
                let started = self.live.then(Instant::now);
                self.push(
                    &bot,
                    turn,
                    Item::Tool {
                        call_id,
                        name,
                        summary,
                        args,
                        background,
                        spawns,
                        done: false,
                        started,
                        took: None,
                    },
                );
            }
            "tool_completed" => {
                let call_id = data["call_id"].as_str().unwrap_or("").to_owned();
                let mut eager = None;
                if let Some(t) = self.transcripts.get_mut(&bot) {
                    for (_, item) in t.items.iter_mut().rev() {
                        if let Item::Tool {
                            call_id: id,
                            started,
                            took,
                            name,
                            background,
                            done,
                            ..
                        } = item
                            && *id == call_id
                        {
                            *took = started.map(|s| s.elapsed());
                            *started = None;
                            *done = true;
                            if *background || name == "wait" {
                                eager = Some(name.clone());
                            }
                            break;
                        }
                    }
                }
                if let Some(node) = data["node"].as_i64() {
                    self.push(
                        &bot,
                        turn,
                        Item::Node {
                            node,
                            call_id: Some(call_id.clone()),
                        },
                    );
                    if let Some(tool) = eager {
                        self.load_wait_or_proc(&bot, node, &tool, &call_id).await;
                    }
                }
            }
            "turn_waiting" => {
                let handles: Vec<String> = data["handles"]
                    .as_array()
                    .map(|h| {
                        h.iter()
                            .filter_map(Value::as_str)
                            .map(String::from)
                            .collect()
                    })
                    .unwrap_or_default();
                if let Some(b) = self.bots.get_mut(&bot) {
                    b.status = "waiting".into();
                    b.waiting_on = handles;
                }
            }
            "turn_paced" => {
                if let Some(b) = self.bots.get_mut(&bot) {
                    b.status = "paced".into();
                }
            }
            "turn_resumed" => {
                if let Some(b) = self.bots.get_mut(&bot) {
                    b.status = "running".into();
                    b.waiting_on.clear();
                }
            }
            "steered" => self.push(
                &bot,
                turn,
                Item::Note("steered into the running turn".into()),
            ),
            "turn_finished" => {
                let status = data["status"].as_str().unwrap_or("?").to_owned();
                let error = data["error"].as_str().map(|e| {
                    format!(
                        "{e}{}",
                        data["detail"]
                            .as_str()
                            .map(|d| format!(": {d}"))
                            .unwrap_or_default()
                    )
                });
                // A steer absorbed into a running turn finishes as its own
                // turn while that turn goes on; only the running turn's own
                // end changes the bot's state.
                if let Some(b) = self.bots.get_mut(&bot)
                    && (b.running_turn.is_none() || b.running_turn == turn)
                {
                    b.running_turn = None;
                    b.waiting_on.clear();
                    b.elapsed = b.turn_started.map(|s| s.elapsed());
                    b.turn_started = None;
                    b.status = if status == "completed" || status == "steered" {
                        "idle".into()
                    } else {
                        status.clone()
                    };
                }
                if let Some(t) = self.transcripts.get_mut(&bot)
                    && t.streaming_turn == turn
                {
                    if !t.text.is_empty() {
                        let text = std::mem::take(&mut t.text);
                        t.add(turn, Item::Text(text));
                    }
                    t.thinking.clear();
                    t.thinking_since = None;
                    t.streaming_turn = None;
                }
                if status != "completed" && status != "steered" {
                    self.push(
                        &bot,
                        turn,
                        Item::Note(match error {
                            Some(e) => format!("{status}: {e}"),
                            None => status,
                        }),
                    );
                }
                // A background command may outlive the turn that started it;
                // only a wait result says how it ended, so its card stays as is.
            }
            "deleted" => {
                self.bots.remove(&bot);
                self.transcripts.remove(&bot);
                if self.selected == bot {
                    self.selected = self.bots.keys().next().cloned().unwrap_or_default();
                }
                if self.ui.peek.as_deref() == Some(bot.as_str()) {
                    self.close_peek();
                }
            }
            "pruned" => {
                // A `follow *` replay reports a retention gap with bot "*":
                // it is a notice about the store, not a transcript.
                let before = event["before"].as_i64().unwrap_or(0);
                if bot == "*" {
                    self.toast(format!(
                        "events before cursor {before} were pruned; older history is gone"
                    ));
                } else {
                    self.push(&bot, None, Item::Note("earlier history pruned".into()));
                }
            }
            _ => {}
        }
    }

    /// A background shell's result names its proc handle; a wait's result
    /// names which handles resolved. Both are read eagerly, they are rare.
    async fn load_wait_or_proc(&mut self, bot: &str, node: i64, tool: &str, call_id: &str) {
        let Ok(client) = self.client() else { return };
        // A failed fetch keeps the node: the next attach loads it lazily.
        let Ok(item) = client
            .request("item", json!({"bot": bot, "node": node}))
            .await
        else {
            return;
        };
        self.apply_wait_or_proc(bot, &item, tool, call_id);
        // The node is spent: its output is what the cards now show, and a
        // later lazy load must not decode it a second time.
        if let Some(t) = self.transcripts.get_mut(bot)
            && let Some(pos) = t
                .items
                .iter()
                .rposition(|(_, i)| matches!(i, Item::Node { node: n, .. } if *n == node))
        {
            t.take(pos);
        }
    }
    /// Decode a background start (a proc handle) or a wait result (which
    /// handles resolved) into the cards; also reached by a retried load.
    fn apply_wait_or_proc(&mut self, bot: &str, item: &Value, tool: &str, call_id: &str) {
        let output = item["output"]
            .as_str()
            .or_else(|| item["content"][0]["content"].as_str())
            .unwrap_or("");
        let Ok(value) = serde_json::from_str::<Value>(output) else {
            return;
        };
        let turn = self.bots.get(bot).and_then(|b| b.running_turn);
        if tool == "shell" {
            if let Some(handle) = value["handle"].as_str() {
                // The card names the call that started the process, not the
                // newest shell call; a decode seen twice adds no second card.
                let Some(t) = self.transcripts.get(bot) else {
                    return;
                };
                if t.items
                    .iter()
                    .rev()
                    .any(|(_, i)| matches!(i, Item::Proc { handle: h, .. } if h == handle))
                {
                    return;
                }
                let cmd = t
                    .items
                    .iter()
                    .rev()
                    .find_map(|(_, i)| match i {
                        Item::Tool {
                            call_id: id,
                            summary,
                            ..
                        } if id == call_id => Some(summary.clone()),
                        _ => None,
                    })
                    .unwrap_or_default();
                self.push(
                    bot,
                    turn,
                    Item::Proc {
                        handle: handle.to_owned(),
                        cmd,
                        done: None,
                        open: false,
                    },
                );
            }
        } else if let Some(results) = value["results"].as_object()
            && let Some(t) = self.transcripts.get_mut(bot)
        {
            for (handle, result) in results {
                if result["pending"].as_bool() == Some(true) {
                    continue;
                }
                for (_, item) in t.items.iter_mut() {
                    if let Item::Proc {
                        handle: h, done, ..
                    } = item
                        && h == handle
                    {
                        let out = result["stdout"]
                            .as_str()
                            .or(result["output"].as_str())
                            .unwrap_or("");
                        let last = out.trim_end().lines().last().unwrap_or("").to_owned();
                        // A process can end without an exit status: a spawn
                        // failure, a timeout, an output limit. Say which.
                        *done = Some(if let Some(error) = result["error"].as_str() {
                            match result["detail"].as_str() {
                                Some(detail) => format!("{error}: {detail}"),
                                None => error.to_owned(),
                            }
                        } else if let Some(code) = result["exit_code"].as_i64()
                            && code != 0
                        {
                            format!("exit {code}")
                        } else if result["success"].as_bool() == Some(false) {
                            "failed".to_owned()
                        } else {
                            last
                        });
                    }
                }
            }
        }
    }

    /// Fetch the items behind unloaded nodes for the bots on screen, in one
    /// pipelined batch per bot, so history is paid for only when looked at.
    pub async fn load_visible(&mut self) {
        let mut names = vec![self.selected.clone()];
        if let Some(p) = &self.ui.peek {
            names.push(p.clone());
        }
        // Cards on screen show their peer's last line; a peer whose final
        // text just became a node would otherwise show a stale tool line.
        for peer in self.peers().into_iter().rev().take(12) {
            if !names.contains(&peer) {
                names.push(peer);
            }
        }
        for name in names {
            self.load(&name).await;
        }
    }
    /// Replay buffered while the attach snapshot paged, applied in order.
    pub async fn drain_backlog(&mut self) {
        for event in std::mem::take(&mut self.backlog) {
            self.event(event).await;
        }
    }
    async fn load(&mut self, name: &str) {
        // One batch of the newest bare nodes: what the screen can show.
        // Scrolling up asks for the next batch, so a long history is
        // materialized only as far as someone reads. A pane at its end
        // keeps only the window: older bodies fold back into their nodes
        // and are not fetched again until someone scrolls up to them.
        let at_end = if name == self.selected {
            self.ui.scroll == 0
        } else if self.ui.peek.as_deref() == Some(name) {
            self.ui.peek_scroll == 0
        } else {
            true
        };
        let floor = if at_end {
            self.transcripts
                .get(name)
                .map_or(0, |t| t.items.len().saturating_sub(WINDOW))
        } else {
            0
        };
        self.load_batch(name, floor).await;
        if at_end && let Some(t) = self.transcripts.get_mut(name) {
            t.evict(WINDOW);
        }
    }
    /// One batch of bare nodes at or after `floor`; `false` when there was
    /// nothing left to fetch.
    async fn load_batch(&mut self, name: &str, floor: usize) -> bool {
        let Ok(client) = self.client() else {
            return false;
        };
        if self.transcripts.get(name).is_none_or(|t| t.nodes == 0) {
            return false;
        }
        let pending: Vec<(usize, i64)> = self
            .transcripts
            .get(name)
            .map(|t| {
                t.items
                    .iter()
                    .enumerate()
                    .skip(floor)
                    .rev()
                    .filter_map(|(i, (_, item))| match item {
                        Item::Node { node, .. } => Some((i, *node)),
                        _ => None,
                    })
                    .take(LAZY_ITEMS)
                    .collect()
            })
            .unwrap_or_default();
        if pending.is_empty() {
            return false;
        }
        let fetched = futures_util::future::join_all(pending.iter().map(|(_, node)| {
            let client = client.clone();
            let name = name.to_owned();
            async move {
                client
                    .request("item", json!({"bot": name, "node": node}))
                    .await
            }
        }))
        .await;
        let Some(t) = self.transcripts.get_mut(name) else {
            return false;
        };
        // `pending` runs from the back already, so earlier indices stay valid.
        let mut progressed = false;
        // Background starts and wait results decoded after the borrow ends:
        // a load retried after a lost session must still build its card.
        let mut deferred: Vec<(String, String, Value)> = Vec::new();
        for ((index, node), result) in pending.into_iter().zip(fetched) {
            let turn = t.items[index].0;
            if let Ok(item) = &result
                && let (
                    _,
                    Item::Node {
                        call_id: Some(id), ..
                    },
                ) = &t.items[index]
                && let Some(tool) = t.items[..index].iter().rev().find_map(|(_, i)| match i {
                    Item::Tool {
                        call_id,
                        name,
                        background,
                        ..
                    } if call_id == id && (*background || name == "wait") => Some(name.clone()),
                    _ => None,
                })
            {
                deferred.push((tool, id.clone(), item.clone()));
            }
            let mut entries = match result {
                Ok(item) => items::entries(&item),
                // A lost session is not the item's fault: the node stays and
                // the next attach fetches it. Anything else is final.
                Err(error)
                    if matches!(
                        error.code.as_str(),
                        "daemon_disconnected" | "io" | "detached"
                    ) =>
                {
                    continue;
                }
                Err(error) => vec![Entry::Note(format!("node {node}: {error}"))],
            };
            progressed = true;
            // A delegate call's result is the handle JSON the peer cards already
            // show, a background start's result is its proc card, and a wait's
            // result is what those cards became. None of it is shown twice.
            if let (_, Item::Node { call_id: Some(id), .. }) = &t.items[index]
                && t.items[..index].iter().rev().any(|(_, i)| matches!(i, Item::Tool { call_id, name, background, spawns, .. } if call_id == id && (*background || *spawns || name == "wait")))
            {
                entries.retain(|e| !matches!(e, Entry::ToolOutput(_)));
            }
            let mut replacement: Vec<(Option<i64>, Item)> = Vec::new();
            for e in entries {
                let item = match e {
                    Entry::User(s) => Item::User(s),
                    Entry::Assistant(s) => Item::Text(s),
                    Entry::Thinking(s) => {
                        // A live thought was already recorded with its duration.
                        if let Some((_, Item::Thought { text, .. })) = t.items[..index].last_mut() {
                            *text = s;
                            continue;
                        }
                        Item::Thought { text: s, secs: 0 }
                    }
                    Entry::ToolOutput(s) => Item::Output(s),
                    Entry::Note(s) => Item::Note(s),
                };
                replacement.push((turn, item));
            }
            t.decode(index, replacement);
        }
        for (tool, call_id, item) in deferred {
            self.apply_wait_or_proc(name, &item, &tool, &call_id);
        }
        progressed
    }

    // ----- actions -----

    /// Scroll the pane under `column` (the peek if it is open there, else the thread).
    pub fn scroll_by(&mut self, delta: i32, column: u16, width: u16) {
        let peek_cols = (width as u32 * self.ui.peek_w.value() as u32 / 100) as u16;
        let target =
            if self.ui.peek.is_some() && peek_cols > 0 && column >= width.saturating_sub(peek_cols)
            {
                &mut self.ui.peek_scroll
            } else {
                &mut self.ui.scroll
            };
        *target = (*target as i64 + i64::from(delta)).max(0) as usize;
    }
    pub fn open_peek(&mut self, name: &str) {
        self.ui.peek = Some(name.to_owned());
        self.ui.peek_scroll = 0;
        let dur = self.slide();
        self.ui.peek_w.go(44, dur);
    }
    pub fn close_peek(&mut self) {
        self.ui.peek = None;
        let dur = self.slide();
        self.ui.peek_w.go(0, dur);
    }
    pub fn toggle_rail(&mut self) {
        let to = if self.ui.rail.target() == 0 {
            RAIL_W
        } else {
            0
        };
        let dur = self.slide();
        self.ui.rail.go(to, dur);
    }
    pub fn select(&mut self, name: &str) {
        if self.bots.contains_key(name) {
            self.selected = name.to_owned();
            self.auto_select = false;
            self.ui.scroll = 0;
            if self.ui.peek.as_deref() == Some(name) {
                self.close_peek();
            }
        }
    }
    /// Peers this bot created, in order, for ^p cycling.
    pub fn peers(&self) -> Vec<String> {
        self.transcripts
            .get(&self.selected)
            .map(|t| {
                t.peers
                    .iter()
                    .filter(|n| self.bots.contains_key(*n))
                    .cloned()
                    .collect()
            })
            .unwrap_or_default()
    }

    pub async fn submit(&mut self, prompt: String) -> Result<()> {
        let name = self.selected.clone();
        if name.is_empty() {
            return Err(Error::new("no_bot"));
        }
        let client = self.client()?;
        let workspace = self
            .bots
            .get(&name)
            .and_then(|b| b.workspace.clone())
            .unwrap_or_else(|| self.default_workspace.clone());
        let busy = self.bots.get(&name).is_some_and(|b| b.status != "idle");
        // The identity on screen, so a name that changed hands in between
        // is refused rather than handed the prompt.
        let bot_id = self.bots.get(&name).and_then(|b| b.id);
        client
            .request(
                "submit",
                json!({"bot": name, "bot_id": bot_id, "request_id": format!("tui-{}", now_ms()), "prompt": prompt,
                "workspace": workspace, "delivery": if busy { "queue" } else { "reject" }}),
            )
            .await?;
        self.ui.scroll = 0;
        Ok(())
    }

    pub async fn create(&mut self, spec: &str) -> Result<()> {
        let mut parts = spec.split_whitespace();
        let name = parts.next().ok_or(Error::new("name_required"))?.to_owned();
        let model = parts
            .next()
            .map(str::to_owned)
            .or_else(|| self.default_model.clone())
            .ok_or(Error::with(
                "model_required",
                "NAME PROVIDER/MODEL, or set AGENT_MODEL",
            ))?;
        let client = self.client()?;
        // Compose now: an AGENTS.md edited since startup reaches this bot.
        self.compose_instructions();
        client
            .request(
                "create",
                json!({"bot": name, "workspace": self.default_workspace, "model": model,
                "instructions": self.instructions, "tools": self.tools}),
            )
            .await?;
        self.refresh_bot(&name).await;
        self.select(&name);
        self.toast(format!("created {name} · {}", self.instructions_note));
        Ok(())
    }

    pub async fn interrupt(&mut self) -> Result<()> {
        let bot = self.bot().cloned().ok_or(Error::new("no_bot"))?;
        let turn = bot.running_turn.ok_or(Error::new("idle"))?;
        self.client()?
            .request("interrupt", json!({"bot": bot.name, "turn": turn}))
            .await?;
        Ok(())
    }
}

/// Turn states with work in flight, as the daemon names them.
pub fn is_active(status: &str) -> bool {
    matches!(status, "running" | "waiting" | "paced" | "queued" | "ready")
}

/// Does this shell command run the agent CLI's detached submission? Only
/// that call's output is the handle JSON a peer card already shows; any
/// other program's `--detach` keeps its output.
pub fn spawns_peer(command: &str) -> bool {
    // Each shell segment on its own: the executable must be the agent CLI,
    // its first argument `run`, and `--detach` among the rest before `--`.
    command.split([';', '|', '&', '\n']).any(|segment| {
        let mut tokens = segment
            .split_whitespace()
            .map(|t| t.trim_matches(|c| c == '"' || c == '\''));
        let Some(exe) = tokens.next() else {
            return false;
        };
        let is_agent = exe == "$AGENT_BIN"
            || exe == "${AGENT_BIN}"
            || exe == "agent"
            || exe.ends_with("/agent");
        if !is_agent || tokens.next() != Some("run") {
            return false;
        }
        tokens.take_while(|t| *t != "--").any(|t| t == "--detach")
    })
}

pub fn now_ms() -> u128 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis())
        .unwrap_or(0)
}

pub fn fmt_secs(d: Duration) -> String {
    let s = d.as_secs();
    if s < 60 {
        format!("{s}s")
    } else {
        format!("{}m{:02}s", s / 60, s % 60)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn app_with(parents: &[(&str, Option<&str>)]) -> App {
        let mut app = App::new(
            std::path::PathBuf::from("/nonexistent.sock"),
            None,
            "/tmp".into(),
            false,
        );
        for (name, parent) in parents {
            app.bots.insert(
                (*name).to_owned(),
                Bot {
                    name: (*name).to_owned(),
                    parent: parent.map(str::to_owned),
                    ..Bot::default()
                },
            );
        }
        app
    }

    #[test]
    fn the_tree_shows_every_bot_even_inside_a_creator_cycle() {
        // A created B, A was deleted, B created a new A: A -> B -> A.
        let app = app_with(&[
            ("A", Some("B")),
            ("B", Some("A")),
            ("solo", None),
            ("kid", Some("solo")),
        ]);
        let rows: Vec<(String, usize)> = app
            .tree()
            .into_iter()
            .map(|(b, d, ..)| (b.name.clone(), d))
            .collect();
        assert_eq!(rows.len(), 4, "nothing hidden: {rows:?}");
        assert!(rows.contains(&("solo".into(), 0)));
        assert!(rows.contains(&("kid".into(), 1)));
        let a = rows.iter().find(|(n, _)| n == "A").unwrap().1;
        let b = rows.iter().find(|(n, _)| n == "B").unwrap().1;
        assert_eq!(a.min(b), 0, "one member of the cycle roots it");
    }

    #[test]
    fn a_spawn_is_the_agent_cli_running_detached_not_a_mention_of_it() {
        for yes in [
            "\"$AGENT_BIN\" run --detach --new --bot Bob -- do it",
            "cd /tmp && $AGENT_BIN run --new --detach --bot Bob -- task",
            "/usr/local/bin/agent run --detach --bot Bob -- go",
        ] {
            assert!(spawns_peer(yes), "{yes}");
        }
        for no in [
            "echo '$AGENT_BIN run --detach' > notes.md; cat notes.md",
            "\"$AGENT_BIN\" run --new --bot Bob -- explain --detach",
            "\"$AGENT_BIN\" wait --detach turn:Bob/1",
            "grep -- --detach docs/CLI.md",
        ] {
            assert!(!spawns_peer(no), "{no}");
        }
    }

    #[test]
    fn bodies_beyond_the_window_fold_back_into_whole_nodes() {
        let mut t = Transcript::default();
        t.add(Some(1), Item::User("hi".into()));
        for node in 1..=4 {
            t.add(
                Some(1),
                Item::Node {
                    node,
                    call_id: None,
                },
            );
        }
        t.add(Some(2), Item::Peer("kid".into()));
        // Node 2 decodes to a thought and a long output; node 3 to text.
        t.decode(
            2,
            vec![
                (
                    Some(1),
                    Item::Thought {
                        text: "t".into(),
                        secs: 0,
                    },
                ),
                (Some(1), Item::Output("a\nb\nc".into())),
            ],
        );
        t.decode(4, vec![(Some(1), Item::Text("three".into()))]);
        assert_eq!((t.nodes, t.thoughts, t.long_outputs), (2, 1, 1));
        assert_eq!(t.peers, vec!["kid".to_owned()]);
        // Keep the newest four items: the boundary falls inside node 2's run.
        t.evict(4);
        let kinds: Vec<String> = t
            .items
            .iter()
            .map(|(_, i)| match i {
                Item::User(_) => "user".into(),
                Item::Node { node, .. } => format!("node{node}"),
                Item::Text(_) => "text".into(),
                Item::Peer(_) => "peer".into(),
                other => format!("{other:?}"),
            })
            .collect();
        assert_eq!(kinds, ["user", "node1", "node2", "text", "node4", "peer"]);
        assert_eq!((t.nodes, t.thoughts, t.long_outputs), (3, 0, 0));
        assert_eq!(t.items.len(), t.origin.len());
        assert!(
            t.origin[3].is_some(),
            "the surviving body still knows its node"
        );
    }

    #[test]
    fn a_recreated_name_is_a_new_bot_with_nothing_of_the_old_one() {
        let mut app = app_with(&[("Bob", Some("Alice"))]);
        app.transcripts.entry("Bob".into()).or_default().text = "old".into();
        app.upsert(&serde_json::json!({"name": "Bob", "id": 7, "status": "idle"}));
        assert_eq!(
            app.transcripts["Bob"].text, "old",
            "same identity keeps its history"
        );
        app.upsert(&serde_json::json!({"name": "Bob", "id": 8, "status": "idle"}));
        assert_eq!(app.bots["Bob"].id, Some(8));
        assert_eq!(app.bots["Bob"].parent, None, "the old lineage is gone");
        assert!(
            !app.transcripts.contains_key("Bob"),
            "the old transcript is gone"
        );
    }
}
