//! `sonny lookout`: what happens in the bots each subscriber watches,
//! told to the subscriber. It knows no leads: Sonny subscribes each lead to
//! its tasks (`subs::Sub::lead`), and any bot can subscribe to more. A
//! subscriber hears every message someone else writes in the bots it
//! watches and how each of their turns ends, unjudged (see `updates`): it
//! decides itself what matters. The lookout decides only when it hears:
//! the person's own words in a task cut into the subscriber's running turn,
//! as the person's words to it would; everything else waits until it
//! rests. One lookout watches a daemon, for every subscriber on it, started
//! by the first Sonny to open; the daemon is unchanged: this follows every
//! bot and submits to the subscribers like any client. It also answers its
//! agents' calls of the tools Sonny answers (`served`).
//!
//! A subscriber hears it all in one note, which starts "Task updates: ".
pub(crate) mod subs;
mod updates;

use agent_client::Client;
use serde_json::{Value, json};
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

/// How long after a note is refused the next may go, even a steer.
const WAKE_GAP: Duration = Duration::from_secs(120);
/// How long news settles before its note goes, so turns that end together
/// go in one note.
const SETTLE: Duration = Duration::from_secs(2);
/// How long a note waits for messages still being read.
const SETTLE_MAX: Duration = Duration::from_secs(15);
/// How often subscription files are read again.
const REREAD: Duration = Duration::from_secs(15);
/// How long a task waits on the person's approval before it is news.
const APPROVAL_AFTER: Duration = Duration::from_secs(120);
/// How long a running turn may print nothing before it is news.
const QUIET_AFTER: Duration = Duration::from_secs(30 * 60);
/// How long the daemon may be unreachable before the lookout exits: a
/// client starts another with the next daemon.
const GONE_AFTER: Duration = Duration::from_secs(30);
/// How much of a queued message is read, in characters.
const EVENT_CHARS: usize = 12_000;
/// How much of the person's words a note quotes: the lead reads a task's
/// turns by handle, but the message itself only here.
const QUOTE_CHARS: usize = 2000;

/// A message someone other than the subscriber wrote in a bot it watches,
/// to read for the note.
#[derive(Clone, Debug)]
struct Seen {
    task: String,
    turn: i64,
    /// Who wrote it: `the person`, another bot, or an origin.
    from: String,
    /// The history node holding the words: none while queued.
    node: Option<i64>,
    /// The node of the subscriber's latest message to the task, unless the
    /// person wrote to it since.
    brief: Option<i64>,
    text: String,
}

/// Long text kept at both ends, where a message's ask usually is.
fn cut(text: &str, max: usize) -> String {
    let count = text.chars().count();
    if count <= max {
        return text.to_owned();
    }
    let half = max / 2;
    let head: String = text.chars().take(half).collect();
    let tail: String = text.chars().skip(count - half).collect();
    format!(
        "{head}\n[... {} characters left out ...]\n{tail}",
        count - 2 * half
    )
}

/// How the note to the lead starts, which clients show as news, not as the
/// person's words.
pub const TASK_UPDATES: &str = "Task updates: ";

/// What the subscriber is told: what happened in the bots it watches, by
/// handles its wait tool reads, the person's words quoted. `steer` when it
/// cuts into a running turn, for the person's words.
fn note(told: &updates::Told, steer: bool) -> String {
    let mut lines = vec![format!(
        "{TASK_UPDATES}what happened in your tasks since you last heard; the wait tool reads each turn by its handle."
    )];
    let mut quoted = false;
    for (n, line) in told.items.iter().enumerate() {
        if line.theirs && (n == 0 || !told.items[n - 1].theirs) {
            lines.push(
                "The person asked for these in the task themselves, so they are theirs:".to_owned(),
            );
        }
        quoted |= line.said && line.text.contains("the person wrote: ");
        lines.push(line.text.clone());
    }
    if told.more > 0 {
        lines.push(format!("- {} more tasks in the next note", told.more));
    }
    if quoted {
        lines.push("A quoted message is the person's own words.".to_owned());
    }
    lines.push(match steer {
        true => "This cuts into your turn because the person wrote in a task: act on their \
            words where they change what you or a task is doing, then carry on with your turn."
            .to_owned(),
        false => "Read the turns you need with one wait call (timeout_ms 0), and act where \
            something changes the work. Tell the person, briefly, what they would want to \
            hear: the outcome of work you started for them, once, when it ends, not again \
            when they write in that task; what only they can decide; and where their latest \
            words contradict what they asked before. If nothing, end your turn with an empty \
            reply."
            .to_owned(),
    });
    let mut text = lines.join("\n");
    if let Some(memory) = &told.memory {
        text.push_str("\n\n");
        text.push_str(memory);
    }
    text
}

/// FNV-1a, 64-bit, as the app keys its wakes: the same note twice is one submit.
fn digest(text: &str) -> String {
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    for byte in text.bytes() {
        hash = (hash ^ u64::from(byte)).wrapping_mul(0x0100_0000_01b3);
    }
    format!("{hash:016x}")
}

/// Who wrote a message with no sender and no origin.
const PERSON: &str = "the person";

/// The words written: at their history node, or, still queued, in their
/// turn's record.
async fn event_text(client: &Client, seen: &Seen) -> String {
    match seen.node {
        Some(node) => node_text(client, &seen.task, node).await,
        None => client
            .request(
                "prompts",
                json!({"bot": seen.task, "turn": seen.turn, "bytes": EVENT_CHARS * 4}),
            )
            .await
            .ok()
            .and_then(|read| Some(read["prompts"][0]["text"].as_str()?.to_owned()))
            .unwrap_or_default(),
    }
}

/// The words one history node holds.
async fn node_text(client: &Client, bot: &str, node: i64) -> String {
    client
        .request("history_items", json!({"bot": bot, "nodes": [node]}))
        .await
        .ok()
        .map(|read| crate::text(&read["items"][0]["item"]))
        .unwrap_or_default()
}

/// Whether words with no sender are the person's: not when they repeat
/// the subscriber's own brief, which a client sent without saying so.
async fn unattributed(client: &Client, seen: &Seen, text: &str) -> bool {
    // Only a brief the subscriber is known to have sent: the person's own
    // brief is theirs to say again.
    let Some(brief) = seen.brief else {
        return false;
    };
    seen.from == PERSON
        && !text.trim().is_empty()
        && text.trim() == node_text(client, &seen.task, brief).await.trim()
}

/// The message as read for the note: its words, and who wrote them.
async fn read_event(client: &Client, seen: Seen) -> Seen {
    let text = event_text(client, &seen).await;
    let from = match unattributed(client, &seen, &text).await {
        true => "someone".to_owned(),
        false => seen.from.clone(),
    };
    Seen { text, from, ..seen }
}

const USAGE: &str = "usage: sonny lookout [--store PATH] [--socket PATH]";

/// `sonny lookout`: one lookout for the store's daemon.
pub fn run(args: &[String]) -> Result<(), String> {
    let (mut store, mut socket) = (None, None);
    let mut args = args.iter();
    while let Some(arg) = args.next() {
        let mut value = || args.next().cloned().ok_or(USAGE);
        match arg.as_str() {
            "--store" => store = Some(PathBuf::from(value()?)),
            "--socket" => socket = Some(PathBuf::from(value()?)),
            _ => return Err(USAGE.into()),
        }
    }
    let (store, socket) = crate::daemon_paths(store, socket)?;
    main(Settings { store, socket })
}

pub struct Settings {
    pub store: PathBuf,
    pub socket: PathBuf,
}

/// Watch the store's daemon for every subscriber on it, until the daemon
/// is gone for good. One lookout a store: another that starts while this
/// one runs says so and exits. What it does is logged beside the store, in
/// `STORE.lookout.jsonl`.
pub fn main(settings: Settings) -> Result<(), String> {
    let held = match lock(&settings.store)? {
        Some(held) => Some(held),
        None => replace(&settings.store)?,
    };
    let Some(_held) = held else {
        println!("{}", json!({"event": "already_watching"}));
        return Ok(());
    };
    // Which Sonny this is, for the next one to start to tell.
    let _ = std::fs::write(beside(&settings.store, ".lookout.sonny"), identity());
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .map_err(|e| e.to_string())?
        .block_on(serve(settings))
}

/// `store`'s path with `suffix` after it, for the files beside it.
fn beside(store: &Path, suffix: &str) -> PathBuf {
    let mut path = store.as_os_str().to_owned();
    path.push(suffix);
    PathBuf::from(path)
}

/// This Sonny, told apart from another build of it: its version and when
/// its binary was written.
fn identity() -> String {
    let written = std::env::current_exe()
        .and_then(std::fs::metadata)
        .and_then(|meta| meta.modified())
        .ok()
        .and_then(|at| at.duration_since(std::time::UNIX_EPOCH).ok())
        .map_or(0, |at| at.as_nanos());
    format!("{} {written}", env!("CARGO_PKG_VERSION"))
}

/// The lock, taken from a lookout another Sonny started, as one an upgrade
/// left running, so the tools this Sonny gives its agents are answered:
/// that lookout is stopped, and the lock awaited for a few seconds. `None`
/// when the lookout holding it is this Sonny's own.
fn replace(store: &Path) -> Result<Option<std::fs::File>, String> {
    let theirs = std::fs::read_to_string(beside(store, ".lookout.sonny")).unwrap_or_default();
    if theirs == identity() {
        return Ok(None);
    }
    let pid: Option<i32> = std::fs::read_to_string(beside(store, ".lookout.lock"))
        .ok()
        .and_then(|pid| pid.trim().parse().ok());
    let Some(pid) = pid.filter(|pid| *pid > 0 && *pid != std::process::id() as i32) else {
        return Ok(None);
    };
    // SAFETY: a signal to the process the lock file names, which asks it
    // to exit.
    unsafe { libc::kill(pid, libc::SIGTERM) };
    let until = Instant::now() + Duration::from_secs(5);
    while Instant::now() < until {
        if let Some(held) = lock(store)? {
            return Ok(Some(held));
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    Ok(None)
}

/// The store's lookout lock, held while this process lives: `None` when
/// another lookout holds it.
fn lock(store: &std::path::Path) -> Result<Option<std::fs::File>, String> {
    use std::os::fd::AsRawFd;
    let mut path = store.as_os_str().to_owned();
    path.push(".lookout.lock");
    let file = std::fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .write(true)
        .open(&path)
        .map_err(|e| format!("{}: {e}", std::path::Path::new(&path).display()))?;
    // SAFETY: flock on a descriptor `file` owns and keeps open; the lock is
    // released when the process exits.
    if unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } == 0 {
        // Its process, which `/status` asks after.
        use std::io::Write;
        let _ = file.set_len(0);
        let _ = write!(&file, "{}", std::process::id());
        return Ok(Some(file));
    }
    let error = std::io::Error::last_os_error();
    match error.raw_os_error() == Some(libc::EWOULDBLOCK) {
        true => Ok(None),
        false => Err(format!(
            "{}: {error}",
            std::path::Path::new(&path).display()
        )),
    }
}

async fn serve(settings: Settings) -> Result<(), String> {
    let mut log = settings.store.as_os_str().to_owned();
    log.push(".lookout.jsonl");
    let mut service = Service {
        store: settings.store.clone(),
        records: HashMap::new(),
        subscribers: HashMap::new(),
        socket: settings.socket,
        lookouts: HashMap::new(),
        log: PathBuf::from(log),
        seated: 0,
        live: false,
        cursor: None,
        saved: None,
        resume: std::fs::read_to_string(cursor_path(&settings.store))
            .ok()
            .and_then(|text| text.trim().parse().ok()),
    };
    println!(
        "{}",
        json!({"event": "watching", "pid": std::process::id()})
    );
    let served = crate::served::Server::start(service.store.clone(), service.socket.clone());
    // Asked to give way, as by another build's lookout: what it still
    // serves stops first, since that one serves it again.
    if let Ok(mut term) = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
    {
        tokio::spawn(async move {
            term.recv().await;
            crate::served::stop();
            std::process::exit(0);
        });
    }
    let mut gone: Option<Instant> = None;
    loop {
        if let Ok((client, events)) = Client::connect(&service.socket).await {
            gone = None;
            service.watch(client, events, &served).await;
        }
        // A daemon restarting is back within moments; one shut down is not,
        // and the client that starts the next daemon starts its lookout.
        if gone.get_or_insert_with(Instant::now).elapsed() >= GONE_AFTER {
            println!("{}", json!({"event": "daemon_gone"}));
            return Ok(());
        }
        tokio::time::sleep(Duration::from_secs(2)).await;
    }
}

/// Every subscriber's lookout on one daemon.
struct Service {
    store: PathBuf,
    /// Every bot as the daemon lists it, kept up from the stream: who made
    /// whom, for subscriptions to tasks of a bot other than the lead.
    records: HashMap<String, Value>,
    /// Subscriptions by subscriber, as their files held them at the last
    /// read.
    subscribers: HashMap<String, Vec<subs::Sub>>,
    socket: PathBuf,
    /// By subscriber name.
    lookouts: HashMap<String, Lookout>,
    log: PathBuf,
    /// Lookouts made so far: each one's own number, so words read after
    /// its subscriber was deleted reach no later subscriber of the name.
    seated: u64,
    /// The stream's replay is over: what happens now is news.
    live: bool,
    /// The newest event seen live, and the newest saved beside the store:
    /// a lookout started again tells what came after it.
    cursor: Option<i64>,
    saved: Option<i64>,
    /// Where the last lookout left off: events after it are news.
    resume: Option<i64>,
}

/// Every bot that one of `made` made, and what those made, as the daemon
/// lists them; side chats are no one's tasks.
fn grow(made: &mut HashSet<String>, records: &HashMap<String, Value>) {
    loop {
        let before = made.len();
        for (name, record) in records {
            if (record["created_by"].as_str()).is_some_and(|by| made.contains(by))
                && record["expire_ms"].is_null()
            {
                made.insert(name.clone());
            }
        }
        if made.len() == before {
            return;
        }
    }
}

/// Whether the bot listed under a name is another than the one seated.
fn replaced(seated: &Value, listed: &Value) -> bool {
    let id = |record: &Value| record["bot_id"].as_i64();
    id(seated).is_some() && id(listed).is_some() && id(seated) != id(listed)
}

/// A subscriber's task updates, fresh: every bot listed seated, the memory
/// its own indexes name, and the other subscribers, which are no one's
/// tasks.
fn updates_for(
    subscriber: &str,
    workspace: &Path,
    records: &HashMap<String, Value>,
    subscribers: &HashMap<String, Vec<subs::Sub>>,
    live: bool,
) -> updates::Updates {
    // What changed in the person's and the project's memory rides along
    // with the task updates.
    let indexes = agent_client::policy::memory_indexes(workspace).unwrap_or_default();
    let memory = (indexes.iter())
        .filter_map(|index| index.parent().map(Path::to_owned))
        .collect();
    let mut updates = updates::Updates::new(subscriber, memory);
    for record in records.values() {
        updates.seat(record);
    }
    updates.subscribed(others(subscriber, subscribers));
    updates.live = live;
    updates
}

/// The subscribers other than this one.
fn others(subscriber: &str, subscribers: &HashMap<String, Vec<subs::Sub>>) -> HashSet<String> {
    (subscribers.keys())
        .filter(|name| *name != subscriber)
        .cloned()
        .collect()
}

impl Service {
    async fn watch(
        &mut self,
        client: Arc<Client>,
        mut events: agent_client::Events,
        served: &crate::served::Server,
    ) {
        let bots = all_bots(&client).await;
        self.records = (bots.iter())
            .filter_map(|b| Some((b["name"].as_str()?.to_owned(), b.clone())))
            .collect();
        self.subscribers = subs::all(&self.store).into_iter().collect();
        for bot in &bots {
            self.seat(&client, bot).await;
        }
        self.resubscribe();
        if client
            .request("follow", json!({"bot": "*", "after": 0}))
            .await
            .is_err()
        {
            return;
        }
        let listed: HashSet<String> = (bots.iter())
            .filter_map(|b| b["name"].as_str().map(str::to_owned))
            .collect();
        for lookout in self.lookouts.values_mut() {
            // The replay's turns were news then; the live ones are now.
            lookout.updates.live = false;
            for bot in &bots {
                lookout.updates.seat(bot);
            }
            lookout.updates.seated(&listed);
            lookout.lineage.insert(lookout.lead.clone());
        }
        let mut live = false;
        self.live = false;
        // From before the oldest news not told yet, as a restart would:
        // a message whose read the dropped connection cut short is read
        // again.
        self.resume = self.floor().or(self.resume);
        for lookout in self.lookouts.values_mut() {
            // Reads a dropped connection left under way answer the old
            // channel, never this one.
            lookout.reading = 0;
        }
        let (done, mut results) = tokio::sync::mpsc::channel(64);
        // Often enough for a note whose news has settled; subscription
        // files less often.
        let mut tick = tokio::time::interval(SETTLE / 2);
        let mut reread: Option<Instant> = None;
        loop {
            tokio::select! {
                event = events.recv() => {
                    let Some(event) = event else { return };
                    served.event(&event);
                    let cursor = event["cursor"].as_i64();
                    // What happened since the lookout last watched live, as
                    // a task that ended while it was down, is news now.
                    if !live && cursor.is_some_and(|at| self.resume.is_some_and(|seen| at > seen)) {
                        live = true;
                        self.go_live();
                    }
                    if live && cursor > self.cursor {
                        self.cursor = cursor;
                    }
                    if let (Some("created" | "forked"), Some(name)) =
                        (event["event"].as_str(), event["bot"].as_str())
                    {
                        let mut record = event["data"].clone();
                        record["name"] = json!(name);
                        self.records.insert(name.to_owned(), record.clone());
                        self.seat(&client, &record).await;
                    }
                    if event["event"] == "deleted"
                        && let Some(name) = event["bot"].as_str()
                    {
                        self.records.remove(name);
                        self.lookouts.remove(name);
                    }
                    for lookout in self.lookouts.values_mut() {
                        lookout.event(&event);
                        // The oldest event whose news is not told yet: a
                        // restart takes it up again from there.
                        if live && let Some(at) = cursor
                            && lookout.untold()
                        {
                            lookout.from.get_or_insert(at);
                        }
                    }
                    // The replay tells who belongs to each subscriber and
                    // each task's current brief; what it holds was news then.
                    if !live {
                        for lookout in self.lookouts.values_mut() {
                            // Nothing in it was read, so a turn still
                            // queued is read once it starts.
                            lookout.due.clear();
                            lookout.queued.clear();
                        }
                        if event["event"] == "follow_live" {
                            live = true;
                            self.go_live();
                        }
                        continue;
                    }
                }
                Some((lead, id, seen)) = results.recv() => {
                    // From a lookout since deleted.
                    if let Some(lookout) = self.lookouts.get_mut(&lead)
                        && lookout.id == id
                    {
                        lookout.reading = lookout.reading.saturating_sub(1);
                        lookout.heard(&seen);
                    }
                }
                _ = tick.tick() => {
                    let now = Instant::now();
                    self.save_cursor();
                    // What `subscriptions` calls wrote since: at once for
                    // one this lookout answered, else now and then.
                    if served.subscribed() || reread.is_none_or(|at| now >= at + REREAD) {
                        reread = Some(now);
                        let read: HashMap<String, Vec<subs::Sub>> = subs::all(&self.store).into_iter().collect();
                        if read != self.subscribers {
                            self.subscribers = read;
                            let records: Vec<Value> = (self.subscribers.keys())
                                .filter_map(|name| self.records.get(name).cloned())
                                .collect();
                            for record in records {
                                self.seat(&client, &record).await;
                            }
                            self.resubscribe();
                        }
                    }
                    for lookout in self.lookouts.values_mut() {
                        lookout.updates.expire(now);
                    }
                }
            }
            for lookout in self.lookouts.values_mut() {
                // Words are read beside the stream, so a slow read never
                // stalls it.
                for seen in std::mem::take(&mut lookout.due) {
                    let (client, done) = (client.clone(), done.clone());
                    let (lead, id) = (lookout.lead.clone(), lookout.id);
                    lookout.reading += 1;
                    tokio::spawn(async move {
                        let seen = read_event(&client, seen).await;
                        let _ = done.send((lead, id, seen)).await;
                    });
                }
                lookout.wake(&client).await;
            }
        }
    }

    /// The subscriber's lookout, made once while its file lasts: whether it
    /// has one.
    fn place(&mut self, record: &Value) -> bool {
        let Some(lead) = record["name"].as_str() else {
            return false;
        };
        // A bot is watched for what it subscribed to, while its file lasts.
        let Some(file) = self.subscribers.get(lead).cloned() else {
            return false;
        };
        // Another bot took the name while no one watched: what was held
        // for the last one is no one's.
        if (self.lookouts.get(lead)).is_some_and(|had| replaced(&had.record, record)) {
            self.lookouts.remove(lead);
        }
        if self.lookouts.contains_key(lead) {
            return true;
        }
        let workspace = PathBuf::from(record["workspace"].as_str().unwrap_or("/"));
        let updates = updates_for(
            lead,
            &workspace,
            &self.records,
            &self.subscribers,
            self.live,
        );
        let mut lookout = Lookout::new(lead);
        self.seated += 1;
        lookout.id = self.seated;
        lookout.log = Some(self.log.clone());
        lookout.record = record.clone();
        lookout.updates = updates;
        lookout.updates.seat(record);
        // What it subscribed to before it was made, or made again.
        lookout.subs = file;
        lookout.roots(&self.records);
        self.lookouts.insert(lead.to_owned(), lookout);
        // Said once a subscriber is watched, so `/status` shows it from
        // the start.
        self.write(&json!({"at_ms": epoch_ms(), "lead": lead, "seated": true}));
        true
    }

    /// A subscriber as `bots` lists it or `created` says it: its lookout,
    /// made once.
    async fn seat(&mut self, client: &Client, record: &Value) {
        let Some(lead) = record["name"].as_str() else {
            return;
        };
        let fresh = !self.lookouts.contains_key(lead);
        if !self.place(record) {
            return;
        }
        // Subscribed while live and mid-turn: what its turn waits on is no
        // news, though the events that said so came before it had a lookout.
        if fresh && self.live {
            let newest = json!({"bot": lead, "newest": true, "limit": 1});
            let turn = (client.request("turns", newest).await.ok())
                .and_then(|page| page["turns"].get(0).cloned())
                .unwrap_or_default();
            if turn["status"] == "waiting"
                && let Some(lookout) = self.lookouts.get_mut(lead)
            {
                let handles = turn["waiting_on"]["handles"].clone();
                lookout.event(
                    &json!({"bot": lead, "turn": turn["turn"], "event": "turn_waiting",
                    "data": {"handles": handles, "any": turn["waiting_on"]["any"]}}),
                );
            }
        }
    }

    /// Each lookout's subscriptions from the files last read: a bot is
    /// watched only while its file lasts.
    fn resubscribe(&mut self) {
        let subscribers = &self.subscribers;
        self.lookouts
            .retain(|name, _| subscribers.contains_key(name));
        for (name, lookout) in self.lookouts.iter_mut() {
            lookout.updates.subscribed(others(name, subscribers));
            lookout.subs = subscribers.get(name).cloned().unwrap_or_default();
            lookout.roots(&self.records);
        }
    }

    fn write(&self, line: &Value) {
        append(&self.log, line);
    }

    /// The replay is over, or has reached what no lookout watched live:
    /// news from here on.
    fn go_live(&mut self) {
        self.live = true;
        for lookout in self.lookouts.values_mut() {
            lookout.updates.live = true;
        }
    }

    /// Up to just before the oldest news a lookout has not told, which a
    /// restart or a reconnect takes up again: news only in memory, or a
    /// read a dropped connection cut short, is told again, not lost.
    fn floor(&mut self) -> Option<i64> {
        for lookout in self.lookouts.values_mut() {
            if !lookout.untold() {
                lookout.from = None;
            }
        }
        let floor = self.lookouts.values().filter_map(|l| l.from).min();
        match (self.cursor, floor) {
            (Some(at), Some(floor)) => Some(at.min(floor - 1)),
            (at, _) => at,
        }
    }

    /// The newest event seen live, kept beside the store for the next
    /// lookout, written whole and moved in.
    fn save_cursor(&mut self) {
        let Some(cursor) = self.floor().filter(|at| Some(*at) != self.saved) else {
            return;
        };
        let path = cursor_path(&self.store);
        let partial = path.with_extension(format!("cursor.{}.partial", std::process::id()));
        if std::fs::write(&partial, cursor.to_string()).is_ok()
            && std::fs::rename(&partial, &path).is_ok()
        {
            self.saved = Some(cursor);
        }
    }
}

/// Where the newest event a lookout saw live is kept: beside the store.
fn cursor_path(store: &Path) -> PathBuf {
    let mut path = store.as_os_str().to_owned();
    path.push(".lookout.cursor");
    PathBuf::from(path)
}

/// The most of the log `status` reads, from its end.
const STATUS_BYTES: u64 = 1 << 20;

/// One line on the lookout for a subscriber, as `/status` shows it:
/// whether one runs and watches it, and today's notes.
pub fn status(store: &std::path::Path, lead: &str) -> String {
    let pid: Option<i32> = std::fs::read_to_string(beside(store, ".lookout.lock"))
        .ok()
        .and_then(|pid| pid.trim().parse().ok());
    // SAFETY: signal 0 sends nothing; it only asks whether the process lives.
    if !pid.is_some_and(|pid| pid > 0 && unsafe { libc::kill(pid, 0) } == 0) {
        return "Lookout · not running".to_owned();
    }
    let (mut seated, mut notes) = (false, 0);
    for line in logged(store, lead) {
        seated |= line["seated"] == true;
        if today(&line) {
            notes += usize::from(line.get("woke").is_some());
        }
    }
    match seated {
        false => "Lookout · not watching this lead yet".to_owned(),
        true => format!(
            "Lookout · watching · {notes} note{} in the last day",
            if notes == 1 { "" } else { "s" }
        ),
    }
}

/// The end of the log, the lines about one subscriber.
fn logged(store: &std::path::Path, lead: &str) -> Vec<Value> {
    use std::io::{Read, Seek};
    let mut text = String::new();
    if let Ok(mut file) = std::fs::File::open(beside(store, ".lookout.jsonl")) {
        let length = file.metadata().map(|m| m.len()).unwrap_or(0);
        let _ = file.seek(std::io::SeekFrom::Start(
            length.saturating_sub(STATUS_BYTES),
        ));
        let _ = file.read_to_string(&mut text);
    }
    (text.lines())
        .filter_map(|l| serde_json::from_str::<Value>(l).ok())
        .filter(|line| line["lead"] == lead)
        .collect()
}

fn today(line: &Value) -> bool {
    let day = epoch_ms().saturating_sub(24 * 60 * 60 * 1000);
    (line["at_ms"].as_u64()).is_some_and(|at| u128::from(at) >= day)
}

/// What Sonny subscribes a lead to, its tasks (`subs::Sub::lead`), added
/// once: one by that name the file holds, the person's edit, say, is kept.
/// Whether it was added.
pub fn subscribe_lead(store: &Path, lead: &str, project: &str) -> Result<bool, String> {
    subs::ensure(store, lead, subs::Sub::lead(lead, project))
}

/// Every bot the daemon lists, page by page.
async fn all_bots(client: &Client) -> Vec<Value> {
    let mut bots = Vec::new();
    let mut after = Value::Null;
    loop {
        let Ok(page) = client
            .request("bots", json!({"limit": 256, "after": after}))
            .await
        else {
            return bots;
        };
        bots.extend(page["bots"].as_array().cloned().unwrap_or_default());
        match &page["next_after"] {
            Value::Null => return bots,
            next => after = next.clone(),
        }
    }
}

fn append(path: &std::path::Path, line: &Value) {
    use std::io::Write;
    if let Ok(mut file) = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
    {
        let _ = writeln!(file, "{line}");
    }
}

struct Lookout {
    /// This lookout's own number in the service.
    id: u64,
    record: Value,
    /// The news the subscriber hears.
    updates: updates::Updates,
    lead: String,
    /// The subscriber's own tasks: every bot it or one of them made.
    lineage: HashSet<String>,
    /// What the lookout watches for it.
    subs: Vec<subs::Sub>,
    /// For each other bot a subscription names the makers of: it, and every
    /// bot it or one of those made.
    others: HashMap<String, HashSet<String>>,
    /// Bots a subscription's `--bot` glob names, as listed and made since.
    globbed: HashSet<String>,
    /// Steers waiting to be absorbed, by task and steer turn: who sent it,
    /// or `None` for the subscriber.
    steers: HashMap<(String, i64), Option<String>>,
    /// Queued turns whose words were read when they were queued.
    queued: HashSet<(String, i64)>,
    /// The node of the subscriber's latest message to each task, until the
    /// person writes to it.
    briefs: HashMap<String, i64>,
    /// Messages to read.
    due: Vec<Seen>,
    /// Messages being read.
    reading: usize,
    /// The person wrote in a task: the note cuts into the subscriber's
    /// running turn.
    urgent: bool,
    /// When the last note was refused: even a steer waits the gap after.
    refused: Option<Instant>,
    /// When the news the next note carries first came.
    since: Option<Instant>,
    /// The cursor of the oldest event whose news is not told yet.
    from: Option<i64>,
    log: Option<PathBuf>,
}

impl Lookout {
    fn new(lead: &str) -> Self {
        Lookout {
            id: 0,
            record: Value::Null,
            updates: updates::Updates::new(lead, Vec::new()),
            lead: lead.to_owned(),
            lineage: [lead.to_owned()].into(),
            subs: Vec::new(),
            others: HashMap::new(),
            globbed: HashSet::new(),
            steers: HashMap::new(),
            queued: HashSet::new(),
            briefs: HashMap::new(),
            due: Vec::new(),
            reading: 0,
            urgent: false,
            refused: None,
            since: None,
            from: None,
            log: None,
        }
    }

    /// The bots whose news the subscriber hears: those its subscriptions
    /// watch, among the bots listed.
    fn sync(&mut self) {
        let watched = (self.updates.listed().into_iter())
            .filter(|bot| self.watches(bot))
            .collect();
        self.updates.watched = watched;
    }

    /// Lineage is kept up during the replay too, so tasks made earlier count.
    fn lineage_of(&mut self, event: &Value) {
        let (Some(kind), Some(bot)) = (event["event"].as_str(), event["bot"].as_str()) else {
            return;
        };
        if matches!(kind, "created" | "forked" | "deleted") {
            self.lineage_change(kind, bot, event);
            self.sync();
        }
    }

    fn lineage_change(&mut self, kind: &str, bot: &str, event: &Value) {
        match kind {
            // A side chat is no one's task.
            "created" | "forked" if event["data"]["expire_ms"].is_null() => {
                let by = event["data"]["created_by"].as_str().unwrap_or("");
                if self.lineage.contains(by) {
                    self.lineage.insert(bot.to_owned());
                }
                for made in self.others.values_mut() {
                    if made.contains(by) {
                        made.insert(bot.to_owned());
                    }
                }
                if self.globs(bot) {
                    self.globbed.insert(bot.to_owned());
                }
            }
            "deleted" if bot != self.lead => {
                self.lineage.remove(bot);
                self.globbed.remove(bot);
                // A root keeps its place under its name, so what it makes
                // once made again is still watched.
                for (root, made) in self.others.iter_mut() {
                    if root != bot {
                        made.remove(bot);
                    }
                }
                self.briefs.remove(bot);
            }
            _ => {}
        }
    }

    /// Who wrote a turn, as the note names them; `None` for the
    /// subscriber, whose words are no news to it.
    fn author(&self, data: &Value) -> Option<String> {
        // A lookout's own note to a lead another subscriber watches.
        if (data["request_id"].as_str()).is_some_and(|id| id.starts_with("lookout-")) {
            return None;
        }
        match (data["from"]["bot"].as_str(), data["origin"].as_str()) {
            (Some(bot), _) if bot == self.lead => None,
            (Some(bot), _) => Some(bot.to_owned()),
            (None, Some(origin)) => Some(origin.to_owned()),
            (None, None) => Some(PERSON.to_owned()),
        }
    }

    fn event(&mut self, event: &Value) {
        self.updates.event(event, Instant::now());
        self.lineage_of(event);
        let (Some(kind), Some(bot)) = (event["event"].as_str(), event["bot"].as_str()) else {
            return;
        };
        let data = &event["data"];
        let turn = event["turn"].as_i64().unwrap_or(0);
        if bot == self.lead {
            // What the subscriber reads with its wait tool is no news to it.
            if kind == "tool_started" && data["name"] == "wait" {
                let arguments: Value = data["arguments"]
                    .as_str()
                    .and_then(|a| serde_json::from_str(a).ok())
                    .unwrap_or_default();
                let handles: HashSet<String> = (arguments["handles"].as_array().into_iter())
                    .flatten()
                    .filter_map(|h| h.as_str().map(str::to_owned))
                    .collect();
                self.updates.read(&handles);
            }
            return;
        }
        if !self.watches(bot) {
            return;
        }
        let node = data["node"].as_i64();
        let key = (bot.to_owned(), turn);
        match kind {
            "accepted" => {
                // Words read when they were queued are not read again, but
                // still say who briefed the task.
                let queued = self.queued.remove(&key);
                match self.author(data) {
                    Some(from) => {
                        if !queued {
                            self.due(bot, turn, &from, node);
                        }
                        self.briefed_by(bot, Some(&from), node);
                    }
                    None if data["from"]["bot"] == self.lead.as_str() => {
                        self.briefed_by(bot, None, node);
                    }
                    None => {}
                }
            }
            // A steer joins a running turn; its words are read once absorbed.
            "queued" if data["delivery"] == "steer" => {
                let from = self.author(data);
                if from.is_some() || data["from"]["bot"] == self.lead.as_str() {
                    self.steers.insert(key, from);
                }
            }
            // Queued work waits for a busy task, so it is read now.
            "queued" => {
                if let Some(from) = self.author(data) {
                    self.queued.insert(key);
                    self.due(bot, turn, &from, None);
                }
            }
            "turn_finished" if data["status"] == "steered" => match self.steers.remove(&key) {
                Some(Some(from)) => {
                    self.due(bot, turn, &from, node);
                    self.briefed_by(bot, Some(&from), node);
                }
                Some(None) => self.briefed_by(bot, None, node),
                None => {}
            },
            _ => {}
        }
    }

    /// Someone wrote to the task, `None` for the subscriber: its words are
    /// the task's brief, which words with no sender that repeat it are not
    /// the person's; the person's own words to it end that.
    fn briefed_by(&mut self, task: &str, from: Option<&str>, node: Option<i64>) {
        match (from, node) {
            (Some(PERSON), _) => {
                self.briefs.remove(task);
            }
            (None, Some(node)) => {
                self.briefs.insert(task.to_owned(), node);
            }
            _ => {}
        }
    }

    /// A message to read, with the task's brief as it stands now.
    fn due(&mut self, task: &str, turn: i64, from: &str, node: Option<i64>) {
        self.due.push(Seen {
            task: task.to_owned(),
            turn,
            from: from.to_owned(),
            node,
            brief: self.briefs.get(task).copied(),
            text: String::new(),
        });
    }

    /// Whether a subscription watches this bot: one of the tasks it names
    /// the makers of, or one its names match. The subscriber's own doings
    /// are never news.
    fn sub_watches(&self, sub: &subs::Sub, bot: &str) -> bool {
        if bot == self.lead {
            return false;
        }
        let made = |root: &String| {
            bot != root
                && match *root == self.lead {
                    true => self.lineage.contains(bot),
                    false => self.others.get(root).is_some_and(|set| set.contains(bot)),
                }
        };
        // A glob names only what `globbed` holds: no side chat.
        sub.tasks_of.iter().any(made)
            || (self.globbed.contains(bot) && sub.bots.iter().any(|glob| subs::matches(glob, bot)))
    }

    fn watches(&self, bot: &str) -> bool {
        self.subs.iter().any(|sub| self.sub_watches(sub, bot))
    }

    /// Every bot a subscription names the makers of, other than the lead,
    /// from what the daemon lists: what each made, and what those made.
    fn roots(&mut self, records: &HashMap<String, Value>) {
        // The subscriber's own: what it made before it was seated is listed,
        // not replayed.
        grow(&mut self.lineage, records);
        let roots: HashSet<String> = (self.subs.iter())
            .flat_map(|sub| sub.tasks_of.iter().cloned())
            .filter(|root| *root != self.lead)
            .collect();
        self.others.retain(|root, _| roots.contains(root));
        for root in roots {
            if self.others.contains_key(&root) {
                continue;
            }
            let mut made: HashSet<String> = [root.clone()].into();
            grow(&mut made, records);
            self.others.insert(root, made);
        }
        self.globbed = (records.iter())
            .filter(|(name, record)| self.globs(name) && record["expire_ms"].is_null())
            .map(|(name, _)| name.clone())
            .collect();
        self.sync();
    }

    /// Whether a subscription's `--bot` glob names this bot.
    fn globs(&self, bot: &str) -> bool {
        bot != self.lead
            && (self.subs.iter()).any(|sub| sub.bots.iter().any(|glob| subs::matches(glob, bot)))
    }

    /// A message read: news for the next note. The person's own words cut
    /// into the subscriber's running turn, as their words to it would.
    fn heard(&mut self, seen: &Seen) {
        let words = (seen.from == PERSON).then_some(seen.text.as_str());
        let news = (self.updates).said(&seen.task, seen.turn, &seen.from, words);
        if news && words.is_some() && self.updates.busy() {
            self.urgent = true;
        }
    }

    /// Whether anything it heard is not told yet: messages to read or
    /// being read, or news.
    fn untold(&self) -> bool {
        !self.due.is_empty() || self.reading > 0 || self.updates.news()
    }

    /// Tell the subscriber what happened: at once, cutting into its turn,
    /// when the person wrote in a task; else as soon as it rests.
    async fn wake(&mut self, client: &Client) {
        let now = Instant::now();
        self.since = match self.updates.news() {
            true => Some(self.since.unwrap_or(now)),
            false => None,
        };
        if let Some(since) = self.since
            && self.updates.doubt(now, since)
        {
            let listed = client
                .request("bots", json!({"name": self.lead, "limit": 1}))
                .await;
            if let Some(record) = listed.ok().and_then(|page| page["bots"].get(0).cloned())
                && let Some(was) = self.updates.rests(now, &record)
            {
                self.write(&json!({"at_ms": epoch_ms(), "lead": self.lead,
                    "rests": format!("the daemon lists it idle; its events said {was}")}));
            }
        }
        let Some(steer) = self.wake_due(now) else {
            return;
        };
        let told = self.updates.take();
        let prompt = note(&told, steer);
        let key: Vec<String> = told.items.iter().map(|line| line.key.clone()).collect();
        let request_id = format!("lookout-{}", digest(&key.join(",")));
        let delivery = if steer { "steer" } else { "queue" };
        let submitted = client
            .request(
                "submit",
                json!({"bot": self.lead, "bot_id": self.updates.lead_id(), "prompt": prompt,
                    "request_id": request_id, "delivery": delivery}),
            )
            .await;
        match submitted {
            Ok(_) => {
                self.refused = None;
                self.since = None;
                // The person's words a bounded note left out still cut in.
                self.urgent &= self.updates.quoting();
                self.updates.sent(told);
                self.write(&json!({"at_ms": epoch_ms(), "lead": self.lead, "woke": key,
                    "delivery": delivery}));
            }
            // The same news twice is one note.
            Err(error) if error.code == "idempotency_conflict" => {
                self.urgent &= self.updates.quoting();
                self.updates.sent(told);
            }
            Err(error) => {
                self.updates.unsent(told);
                self.write(
                    &json!({"at_ms": epoch_ms(), "lead": self.lead, "wake_failed": error.code}),
                );
                self.refused = Some(now);
            }
        }
    }

    /// Whether a note is to go now, and whether it steers: at once while
    /// the person's words wait and the subscriber is mid-turn, else as
    /// soon as it rests, once the news has settled and the messages being
    /// read are read.
    fn wake_due(&self, now: Instant) -> Option<bool> {
        if self.refused(now) || !self.updates.news() {
            return None;
        }
        let steer = self.urgent && self.updates.busy();
        let settled = self.since.is_some_and(|since| {
            now >= since + SETTLE && (self.reading == 0 || now >= since + SETTLE_MAX)
        });
        (steer || (settled && self.updates.due())).then_some(steer)
    }

    /// The last note was refused lately: even a steer waits the gap after.
    fn refused(&self, now: Instant) -> bool {
        self.refused
            .is_some_and(|at| now.duration_since(at) < WAKE_GAP)
    }

    fn write(&self, line: &Value) {
        if let Some(path) = &self.log {
            append(path, line);
        }
    }
}

fn epoch_ms() -> u128 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis())
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn lookout() -> Lookout {
        let mut lookout = Lookout::new("calc.lead");
        lookout.subs = vec![subs::Sub::lead("calc.lead", "calc")];
        lookout.lineage.insert("calc.fix".to_owned());
        lookout
    }

    /// A lookout whose lead and its task are seated and live.
    fn seated(status: &str) -> Lookout {
        let mut lookout = lookout();
        (lookout.updates).seat(&json!({"name": "calc.lead", "bot_id": 1, "status": status}));
        (lookout.updates).seat(&json!({"name": "calc.fix", "bot_id": 2, "status": "idle",
            "created_by": "calc.lead", "created_by_id": 1}));
        lookout.sync();
        lookout.updates.live = true;
        lookout
    }

    fn ev(bot: &str, turn: i64, kind: &str, data: Value) -> Value {
        json!({"bot": bot, "turn": turn, "event": kind, "data": data})
    }

    fn said(turn: i64, from: &str, text: &str) -> Seen {
        Seen {
            task: "calc.fix".into(),
            turn,
            from: from.into(),
            node: None,
            brief: None,
            text: text.into(),
        }
    }

    fn sub(name: &str, tasks_of: &[&str], bots: &[&str]) -> subs::Sub {
        subs::Sub {
            name: name.into(),
            tasks_of: tasks_of.iter().map(|b| b.to_string()).collect(),
            bots: bots.iter().map(|b| b.to_string()).collect(),
        }
    }

    fn line(task: &str, turn: i64, theirs: bool, said: bool) -> updates::Line {
        updates::Line {
            said,
            theirs,
            text: match said {
                true => format!("- {task}: turn:{task}/{turn}, the person wrote: \"tabs\""),
                false => format!("- {task}: turn:{task}/{turn} completed"),
            },
            key: String::new(),
        }
    }

    #[test]
    fn the_note_tells_everything_and_says_when_it_cuts_in() {
        let mut told = updates::Told::default();
        told.items = vec![
            line("calc.fix", 12, false, true),
            line("calc.docs", 3, false, false),
            line("calc.ui", 2, true, false),
        ];
        told.more = 1;
        told.memory = Some("Memory changed".into());
        assert_eq!(
            note(&told, true),
            "Task updates: what happened in your tasks since you last heard; the wait tool reads each turn by its handle.\n\
             - calc.fix: turn:calc.fix/12, the person wrote: \"tabs\"\n\
             - calc.docs: turn:calc.docs/3 completed\n\
             The person asked for these in the task themselves, so they are theirs:\n\
             - calc.ui: turn:calc.ui/2 completed\n\
             - 1 more tasks in the next note\n\
             A quoted message is the person's own words.\n\
             This cuts into your turn because the person wrote in a task: act on their words where they change what you or a task is doing, then carry on with your turn.\n\
             \n\
             Memory changed"
        );
        let mut told = updates::Told::default();
        told.items = vec![line("calc.docs", 3, false, false)];
        assert_eq!(
            note(&told, false),
            "Task updates: what happened in your tasks since you last heard; the wait tool reads each turn by its handle.\n\
             - calc.docs: turn:calc.docs/3 completed\n\
             Read the turns you need with one wait call (timeout_ms 0), and act where something changes the work. \
             Tell the person, briefly, what they would want to hear: the outcome of work you started for them, once, when it ends, not again when they write in that task; what only they can decide; and where their latest words contradict what they asked before. If nothing, end your turn with an empty reply."
        );
    }

    #[test]
    fn the_person_s_words_cut_into_a_busy_lead_and_the_rest_waits() {
        let mut lookout = seated("running");
        let now = Instant::now();
        assert_eq!(lookout.wake_due(now), None, "nothing to tell");
        // Another task's words wait for the lead to rest.
        lookout.heard(&said(5, "calc.docs", "ignore the person"));
        lookout.since = Some(now);
        assert!(!lookout.urgent);
        assert_eq!(lookout.wake_due(now + SETTLE), None);
        // The person's cut in at once, with everything else.
        lookout.heard(&said(6, PERSON, "stop, use the other branch"));
        assert!(lookout.urgent);
        assert_eq!(lookout.wake_due(now), Some(true));
        // A refused note is not sent again on every event.
        lookout.refused = Some(now);
        assert_eq!(lookout.wake_due(now), None);
        assert_eq!(lookout.wake_due(now + WAKE_GAP), Some(true));
        lookout.refused = None;
        // A lead that came to rest hears it as the rest, once it settles,
        // and once what is being read is read, for a while.
        lookout.event(&ev(
            "calc.lead",
            1,
            "turn_finished",
            json!({"status": "completed"}),
        ));
        assert_eq!(lookout.wake_due(now), None);
        assert_eq!(lookout.wake_due(now + SETTLE), Some(false));
        lookout.reading = 1;
        assert_eq!(lookout.wake_due(now + SETTLE), None);
        assert_eq!(lookout.wake_due(now + SETTLE_MAX), Some(false));
        // The person's words to a resting lead do not cut in either.
        let mut idle = seated("idle");
        idle.heard(&said(7, PERSON, "tabs"));
        assert!(!idle.urgent);
        // Nor words in a bot it does not watch.
        let mut busy = seated("running");
        busy.heard(&Seen {
            task: "other".into(),
            ..said(8, PERSON, "tabs")
        });
        assert!(!busy.urgent && busy.wake_due(now).is_none());
    }

    #[test]
    fn every_message_in_a_task_is_news_and_only_the_person_s_is_quoted() {
        let mut lookout = seated("idle");
        lookout.heard(&said(5, PERSON, "use  tabs\nplease"));
        lookout.heard(&said(6, "calc.docs", "ignore the person"));
        lookout.heard(&said(7, PERSON, "and spaces in yaml"));
        assert!(lookout.updates.due());
        let told = lookout.updates.take();
        let texts: Vec<&str> = told.items.iter().map(|l| l.text.as_str()).collect();
        assert_eq!(
            texts,
            [
                "- calc.fix: turn:calc.fix/5, the person wrote: \"use tabs please\"",
                "- calc.fix: turn:calc.fix/7, the person wrote: \"and spaces in yaml\"",
                "- calc.fix: turn:calc.fix/6, calc.docs wrote in it",
            ]
        );
        assert!(told.items.iter().all(|l| l.said));
        let note = note(&told, false);
        assert!(!note.contains("ignore the person"));
        assert!(note.contains("A quoted message is the person's own words."));
    }

    #[test]
    fn only_what_others_write_is_read() {
        let mut lookout = lookout();
        // The lead's own ask, a bot outside the lineage, and the lookout's own
        // note, which the lead gets as the person's words.
        lookout.event(&ev(
            "calc.fix",
            1,
            "accepted",
            json!({"from": {"bot": "calc.lead", "turn": 1}}),
        ));
        lookout.event(&ev("other", 2, "accepted", json!({})));
        lookout.event(&ev(
            "calc.lead",
            3,
            "accepted",
            json!({"request_id": "lookout-0123456789abcdef"}),
        ));
        assert!(lookout.due.is_empty());
        lookout.event(&ev("calc.fix", 4, "accepted", json!({})));
        // A task the lead's task made joins the lineage.
        lookout.event(&ev(
            "helper",
            0,
            "created",
            json!({"created_by": "calc.fix"}),
        ));
        lookout.event(&ev(
            "helper",
            7,
            "accepted",
            json!({"from": {"bot": "calc.fix", "turn": 6}}),
        ));
        let due: Vec<_> = (lookout.due.iter())
            .map(|s| (s.task.as_str(), s.from.as_str()))
            .collect();
        assert_eq!(due, [("calc.fix", PERSON), ("helper", "calc.fix")]);
    }

    #[test]
    fn a_steer_is_read_once_absorbed() {
        let mut lookout = lookout();
        lookout.event(&ev("calc.fix", 9, "queued", json!({"delivery": "steer"})));
        assert!(lookout.due.is_empty());
        lookout.event(&ev(
            "calc.fix",
            9,
            "turn_finished",
            json!({"status": "steered", "into": 8, "node": 21}),
        ));
        assert_eq!((lookout.due.len(), lookout.due[0].turn), (1, 9));
        assert_eq!(lookout.due[0].node, Some(21));
    }

    #[test]
    fn queued_words_are_read_when_queued_and_once() {
        let mut lookout = lookout();
        lookout.event(&ev("calc.fix", 5, "queued", json!({"delivery": "queue"})));
        assert_eq!(lookout.due[0].node, None);
        lookout.due.clear();
        lookout.event(&ev("calc.fix", 5, "accepted", json!({"node": 30})));
        assert!(lookout.due.is_empty());
        // A turn started at once is read by its node.
        lookout.event(&ev("calc.fix", 6, "accepted", json!({"node": 31})));
        assert_eq!(lookout.due[0].node, Some(31));
    }

    #[test]
    fn the_lead_s_words_are_the_brief_until_the_person_writes() {
        let mut lookout = lookout();
        let from = json!({"bot": "calc.lead", "turn": 1});
        lookout.event(&ev(
            "calc.fix",
            2,
            "accepted",
            json!({"from": from, "node": 10}),
        ));
        // A new job for the same task, sent as a steer.
        lookout.event(&ev(
            "calc.fix",
            3,
            "queued",
            json!({"delivery": "steer", "from": from}),
        ));
        lookout.event(&ev(
            "calc.fix",
            3,
            "turn_finished",
            json!({"status": "steered", "node": 20}),
        ));
        // Another task's words are not a brief, and words with no sender
        // that repeat it would be the lead's.
        lookout.event(&ev(
            "calc.fix",
            4,
            "accepted",
            json!({"from": {"bot": "calc.docs", "turn": 1}, "node": 21}),
        ));
        lookout.event(&ev("calc.fix", 5, "accepted", json!({"node": 22})));
        let briefs: Vec<_> = lookout.due.iter().map(|s| s.brief).collect();
        assert_eq!(briefs, [Some(20), Some(20)]);
        // The person's own words are theirs, even said again.
        lookout.event(&ev("calc.fix", 6, "accepted", json!({"node": 23})));
        assert_eq!(lookout.due[2].brief, None);
        assert!(lookout.briefs.is_empty());
    }

    #[test]
    fn what_the_lead_reads_with_its_wait_is_no_news() {
        let mut lookout = seated("idle");
        let turn = |lookout: &mut Lookout, n: i64| {
            lookout.event(&ev(
                "calc.fix",
                n,
                "accepted",
                json!({"from": {"bot": "calc.lead"}}),
            ));
            lookout.event(&ev(
                "calc.fix",
                n,
                "turn_finished",
                json!({"status": "completed"}),
            ));
        };
        turn(&mut lookout, 1);
        assert!(lookout.updates.due());
        let wait = json!({"name": "wait", "arguments": json!({"handles": ["turn:calc.fix/1"]}).to_string()});
        lookout.event(&ev("calc.lead", 7, "tool_started", wait));
        assert!(!lookout.updates.news());
        // A turn that ends after the lead read it is news again.
        turn(&mut lookout, 2);
        assert!(lookout.updates.due());
    }

    #[test]
    fn what_a_task_made_and_bots_named_the_project_s_way_are_news_too() {
        let mut lookout = lookout();
        lookout.event(&json!({"bot": "calc.lead", "event": "created", "data": {"bot_id": 1, "status": "idle"}}));
        lookout.event(&json!({"event": "follow_live"}));
        // Made by a task, and made by no one but named like the project's.
        lookout.event(&json!({"bot": "calc.fix.sub", "event": "created",
            "data": {"bot_id": 5, "status": "idle", "created_by": "calc.fix", "created_by_id": 2}}));
        lookout.event(&json!({"bot": "calc.ops", "event": "created", "data": {"bot_id": 6, "status": "idle"}}));
        for bot in ["calc.fix.sub", "calc.ops"] {
            lookout.event(&ev(bot, 2, "accepted", json!({"origin": "tasks"})));
            lookout.event(&ev(bot, 2, "turn_finished", json!({"status": "completed"})));
        }
        let told = lookout.updates.take();
        let texts: Vec<&str> = told.items.iter().map(|l| l.text.as_str()).collect();
        assert_eq!(
            texts,
            [
                "- calc.fix.sub: turn:calc.fix.sub/2 completed, asked by tasks",
                "- calc.ops: turn:calc.ops/2 completed, asked by tasks",
            ]
        );
    }

    #[test]
    fn a_lead_made_again_while_no_one_watched_starts_afresh() {
        let seated = json!({"name": "calc.lead", "bot_id": 4});
        assert!(!replaced(
            &seated,
            &json!({"name": "calc.lead", "bot_id": 4})
        ));
        assert!(replaced(
            &seated,
            &json!({"name": "calc.lead", "bot_id": 9})
        ));
        assert!(!replaced(&json!({"name": "calc.lead"}), &seated));
    }

    #[test]
    fn a_bot_seated_late_watches_what_it_made_already() {
        let mut lookout = Lookout::new("reviewer");
        lookout.subs = vec![sub("mine", &["reviewer"], &[])];
        let records: HashMap<String, Value> = [("r.a", "reviewer"), ("r.b", "r.a")]
            .into_iter()
            .map(|(name, by)| (name.to_owned(), json!({"name": name, "created_by": by})))
            .collect();
        lookout.roots(&records);
        assert!(lookout.watches("r.a") && lookout.watches("r.b"));
    }

    fn service(store: PathBuf, records: &[Value]) -> Service {
        Service {
            store,
            records: (records.iter())
                .map(|r| (r["name"].as_str().unwrap().to_owned(), r.clone()))
                .collect(),
            subscribers: HashMap::new(),
            socket: PathBuf::new(),
            lookouts: HashMap::new(),
            log: PathBuf::new(),
            seated: 0,
            live: true,
            cursor: None,
            saved: None,
            resume: None,
        }
    }

    #[test]
    fn a_bot_is_watched_while_its_file_lasts() {
        let mut service = service(PathBuf::new(), &[]);
        service.lookouts.insert("calc.lead".into(), lookout());
        service
            .lookouts
            .insert("reviewer".into(), Lookout::new("reviewer"));
        let file = sub("calc", &["calc.lead"], &[]);
        service
            .subscribers
            .insert("reviewer".into(), vec![file.clone()]);
        service
            .subscribers
            .insert("calc.lead".into(), vec![sub("ops", &[], &["ops.*"])]);
        service.resubscribe();
        assert_eq!(service.lookouts["reviewer"].subs, [file]);
        let names: Vec<&str> = service.lookouts["calc.lead"]
            .subs
            .iter()
            .map(|s| s.name.as_str())
            .collect();
        assert_eq!(names, ["ops"]);
        assert!(
            service.lookouts["reviewer"]
                .others
                .contains_key("calc.lead")
        );
        // A lead is no different: without its file, nothing is watched for it.
        service.subscribers.clear();
        service.resubscribe();
        assert!(service.lookouts.is_empty());
    }

    #[test]
    fn a_lead_is_watched_only_for_what_its_file_subscribes_it_to() {
        let record = |name: &str, id: i64, by: Option<&str>| {
            json!({"name": name, "bot_id": id, "workspace": "/", "status": "idle",
                "created_by": by, "created_by_id": by.map(|_| 1)})
        };
        let lead = record("calc.lead", 1, None);
        let mut service = service(
            PathBuf::new(),
            &[
                lead.clone(),
                record("calc.fix", 2, Some("calc.lead")),
                record("calc.review", 3, Some("calc.lead")),
                record("calc.ops", 4, Some("calc.lead")),
            ],
        );
        // Named like a lead, it is a bot like any other.
        assert!(!service.place(&lead));
        assert!(service.lookouts.is_empty());
        let tasks = vec![subs::Sub::lead("calc.lead", "calc")];
        service
            .subscribers
            .insert("calc.lead".into(), tasks.clone());
        // A task that subscribes for itself is no one's task in the updates.
        let own = vec![sub("own", &["calc.review"], &[])];
        service.subscribers.insert("calc.review".into(), own);
        assert!(service.place(&lead));
        let turn = |lookout: &mut Lookout, bot: &str, n: i64| {
            lookout.event(&ev(
                bot,
                n,
                "accepted",
                json!({"from": {"bot": "calc.lead"}}),
            ));
            lookout.event(&ev(bot, n, "turn_finished", json!({"status": "completed"})));
        };
        let lookout = service.lookouts.get_mut("calc.lead").unwrap();
        assert!(lookout.watches("calc.fix"));
        turn(lookout, "calc.review", 1);
        assert!(!lookout.updates.due());
        // Made before the lead was seated, its turn is still news.
        turn(lookout, "calc.fix", 1);
        let told = lookout.updates.take();
        assert_eq!(
            (told.items.iter().map(|l| l.text.as_str())).collect::<Vec<_>>(),
            ["- calc.fix: turn:calc.fix/1 completed, asked by you"]
        );
        // A task that subscribes after its news came: no one's task now.
        turn(lookout, "calc.ops", 1);
        assert!(lookout.updates.due());
        service.subscribers.insert("calc.ops".into(), Vec::new());
        service.resubscribe();
        let lookout = service.lookouts.get_mut("calc.lead").unwrap();
        assert!(!lookout.updates.due());
        // A turn the lead waits on is its wait's to read.
        lookout.event(&ev("calc.lead", 1, "accepted", json!({})));
        lookout.event(&ev(
            "calc.lead",
            1,
            "turn_waiting",
            json!({"handles": ["turn:calc.fix/2"]}),
        ));
        turn(lookout, "calc.fix", 2);
        let finished = json!({"status": "completed"});
        lookout.event(&ev("calc.lead", 1, "turn_finished", finished));
        assert!(!lookout.updates.due());
        turn(lookout, "calc.fix", 3);
        assert!(lookout.updates.due());
    }

    #[test]
    fn status_says_whether_it_watches_from_the_start() {
        let dir = std::env::temp_dir().join(format!("lookout-status-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let store = dir.join("state.sqlite");
        assert_eq!(status(&store, "calc.lead"), "Lookout · not running");
        let held = lock(&store).unwrap().unwrap();
        let now = epoch_ms();
        let log = dir.join("state.sqlite.lookout.jsonl");
        append(
            &log,
            &json!({"at_ms": now, "lead": "calc.lead", "seated": true}),
        );
        append(
            &log,
            &json!({"at_ms": now, "lead": "calc.lead", "woke": []}),
        );
        append(
            &log,
            &json!({"at_ms": now - 2 * 24 * 60 * 60 * 1000, "lead": "calc.lead", "woke": []}),
        );
        assert_eq!(
            status(&store, "calc.lead"),
            "Lookout · watching · 1 note in the last day"
        );
        assert_eq!(
            status(&store, "shop.lead"),
            "Lookout · not watching this lead yet"
        );
        drop(held);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn side_chats_are_never_tasks() {
        let mut lookout = lookout();
        lookout.subs = vec![subs::Sub::lead("shop.lead", "shop")];
        lookout.lead = "shop.lead".into();
        lookout.lineage_of(&json!({"event": "forked", "bot": "shop.fix", "data": {}}));
        lookout.lineage_of(&json!({"event": "forked", "bot": "shop.fix.side-1a2b3c",
            "data": {"expire_ms": 900_000}}));
        lookout.lineage_of(&json!({"event": "forked", "bot": "shop.lead.side-4d5e6f",
            "data": {"created_by": "shop.lead", "expire_ms": 900_000}}));
        assert!(lookout.watches("shop.fix"));
        assert!(!lookout.watches("shop.fix.side-1a2b3c"));
        assert!(!lookout.watches("shop.lead.side-4d5e6f"));
    }
}
