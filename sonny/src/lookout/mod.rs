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
pub(crate) mod goals;
pub(crate) mod sources;
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
/// A note from sources names at most this many events; the rest wait for
/// the next.
const NOTE_ITEMS: usize = 16;
/// How many events from sources wait for a subscriber; past them the
/// oldest go untold.
const HEARD: usize = 64;

/// What to read for the note: a message someone other than the subscriber
/// wrote in a bot it watches, or the reply a turn of one ended with.
#[derive(Clone, Debug)]
struct Seen {
    task: String,
    turn: i64,
    /// Who wrote it: `the person`, another bot, or an origin; for a reply,
    /// the task.
    from: String,
    /// The history node holding the words, none while queued; for a reply,
    /// the turn's checkpoint.
    node: Option<i64>,
    /// The reply a turn ended with, not a message.
    reply: bool,
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
    if told.items.iter().any(|line| line.text.contains("\n  > ")) {
        lines.push("Lines marked > are a task's own reply, not the person's words.".to_owned());
    }
    lines.push(match steer {
        true => "This cuts into your turn because the person wrote in a task: act on their \
            words where they change what you or a task is doing, then carry on with your turn."
            .to_owned(),
        false => "Each ended turn comes with what the task replied; read a turn with one \
            wait call (timeout_ms 0) only when you need more of it, and act where something \
            changes the work. Tell the person, briefly, what they would want to \
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
/// turn's record; or the reply the turn ended with.
async fn event_text(client: &Client, seen: &Seen) -> String {
    if seen.reply {
        return reply_text(client, seen).await;
    }
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

/// How far back from a turn's checkpoint its reply is looked for.
const REPLY_NODES: usize = 30;

/// The last thing the task said in the turn, read back from where the turn
/// left its history, not from wherever it has moved on to.
async fn reply_text(client: &Client, seen: &Seen) -> String {
    let Ok(page) = client
        .request(
            "history_nodes",
            json!({"bot": seen.task, "limit": REPLY_NODES, "oldest_first": false, "from": seen.node}),
        )
        .await
    else {
        return String::new();
    };
    let nodes: Vec<i64> = (page["nodes"].as_array().into_iter())
        .flatten()
        .filter(|n| n["turn"].as_i64() == Some(seen.turn))
        .filter_map(|n| n["node"].as_i64())
        .collect();
    let Ok(read) = client
        .request("history_items", json!({"bot": seen.task, "nodes": nodes}))
        .await
    else {
        return String::new();
    };
    // Anthropic tasks keep calls as blocks inside a message.
    (read["items"].as_array().into_iter())
        .flatten()
        .flat_map(|entry| crate::responses(&entry["item"]))
        .find(|item| item["role"] == "assistant" && !crate::text(item).trim().is_empty())
        .map(|item| crate::text(&item))
        .unwrap_or_default()
}

/// `event`, a `margin` call's start whose arguments were cut before the
/// note's id, with the call's arguments whole, as the bot's history keeps
/// them; any other event as it is.
async fn whole_margin_call(client: &Client, mut event: Value) -> Value {
    let data = &event["data"];
    if event["event"] != "tool_started"
        || data["name"] != crate::margin::TOOL
        || data["arguments_truncated"] != true
        || crate::margin::called(data).is_some()
    {
        return event;
    }
    let (Some(bot), Some(call)) = (event["bot"].as_str(), data["call_id"].as_str()) else {
        return event;
    };
    let (bot, call) = (bot.to_owned(), call.to_owned());
    let Ok(page) = client
        .request("history_nodes", json!({"bot": bot, "limit": REPLY_NODES}))
        .await
    else {
        return event;
    };
    let nodes: Vec<i64> = (page["nodes"].as_array().into_iter().flatten())
        .filter_map(|n| n["node"].as_i64())
        .collect();
    let Ok(read) = client
        .request("history_items", json!({"bot": bot, "nodes": nodes}))
        .await
    else {
        return event;
    };
    // Anthropic tasks keep calls as blocks inside a message.
    let arguments = (read["items"].as_array().into_iter().flatten())
        .flat_map(|entry| crate::responses(&entry["item"]))
        .find(|item| item["type"] == "function_call" && item["call_id"] == call.as_str())
        .and_then(|item| item["arguments"].as_str().map(str::to_owned));
    if let Some(arguments) = arguments {
        event["data"]["arguments"] = json!(arguments);
        event["data"]["arguments_truncated"] = json!(false);
    }
    event
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
    let (false, Some(brief)) = (seen.reply, seen.brief) else {
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
pub(crate) fn identity() -> String {
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
    // The daemon's own companion takes the place of one a window started
    // before Sonny kept it as one, of this build too.
    if theirs == identity() && std::env::var_os("AGENT_COMPANION").is_none() {
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
    // released when the process exits. Tried twice, since `running` holds
    // it for an instant.
    let take = || unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } == 0;
    let taken = take() || {
        std::thread::sleep(Duration::from_millis(50));
        take()
    };
    if taken {
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
    let (sent, mut from_sources) = tokio::sync::mpsc::channel(256);
    let mut service = Service {
        runner: sources::Runner::new(beside(&settings.store, ".sources"), sent),
        store: settings.store.clone(),
        records: HashMap::new(),
        working: HashMap::new(),
        subscribers: HashMap::new(),
        socket: settings.socket.clone(),
        lookouts: HashMap::new(),
        log: PathBuf::from(log),
        seated: 0,
        live: false,
        cursor: None,
        saved: None,
        goals: goals::Goals::new(&settings.store, &settings.socket),
        resume: marks(&settings.store).0,
        replay: marks(&settings.store).1,
        in_flight: HashMap::new(),
        reacting: HashMap::new(),
        taken: HashMap::new(),
        watchers: HashMap::new(),
        started: true,
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
            sources::stop();
            goals::stop();
            std::process::exit(0);
        });
    }
    service.runner.reload();
    let mut gone: Option<Instant> = None;
    loop {
        if let Ok((client, events)) = Client::connect(&service.socket).await {
            gone = None;
            service
                .watch(client, events, &served, &mut from_sources)
                .await;
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
    /// The sources the person set up, running.
    runner: sources::Runner,
    /// Every bot as the daemon lists it, kept up from the stream: who made
    /// whom, for subscriptions to tasks of a bot other than the lead.
    records: HashMap<String, Value>,
    /// The folder each turn under way works in, as it started: a bot may
    /// move between turns.
    working: HashMap<(String, i64), PathBuf>,
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
    saved: Option<(i64, i64)>,
    /// The agents' goals, whose turns it holds until their checks say.
    goals: goals::Goals,
    /// Where the last lookout left off: events after it are news.
    resume: Option<i64>,
    /// Where the last lookout's stream starts again: before the oldest turn
    /// still under way then, so what each said before it ends is known.
    replay: Option<i64>,
    /// Turns of bots a lookout hears that began and have not ended, by bot
    /// and turn: the event each began at. The stream starts again before
    /// the oldest, as a consumer commits its offset only up to its oldest
    /// record still in work: who asked, what the subscriber waits on and a
    /// task's resolving are replayed rather than lost.
    in_flight: HashMap<(String, i64), i64>,
    /// Lead `react` calls naming another agent, by bot, turn and call,
    /// until they end: the agent named, the emoji, whether it comes off,
    /// and the newest message that agent had taken when the call began.
    reacting: HashMap<Call, (String, String, bool, Option<i64>)>,
    /// The newest message each bot took, by the stream: a reaction is on
    /// what was there when it was made, not what came after.
    taken: HashMap<String, i64>,
    /// Which lookouts hear each bot's events, by bot: its own, and those
    /// that watch it.
    watchers: HashMap<String, Vec<String>>,
    /// Not connected yet since it started: its stream starts again where
    /// the last lookout's would.
    started: bool,
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

/// A tool call: its bot, turn and id.
type Call = (String, i64, String);

/// Where a lookout's answers from the daemon go: its subscriber, its own
/// number, and the answer.
type Woken = tokio::sync::mpsc::Sender<(String, u64, Answer)>;

/// The person's newest message in `bot`: one no agent or client sent,
/// with words of theirs, not only tool results, and no reaction of theirs.
/// Read newest first, as far back as a long turn of tool calls reaches;
/// an error when the daemon did not answer.
async fn newest_said(client: &Client, bot: &str, upto: Option<i64>) -> Result<Option<i64>, ()> {
    const READ: usize = 2000;
    let (mut from, mut read) = (Value::Null, 0);
    while read < READ {
        let page = client
            .request(
                "history_nodes",
                json!({"bot": bot, "from": from, "limit": 200}),
            )
            .await
            .map_err(drop)?;
        let nodes: Vec<i64> = (page["nodes"].as_array().into_iter().flatten())
            .filter_map(|n| n["node"].as_i64())
            .collect();
        read += nodes.len();
        let paged = nodes.is_empty();
        // Newest first: what came after the bound is passed over.
        let after = |node: &i64| upto.is_some_and(|upto| *node > upto);
        let nodes: Vec<i64> = nodes.iter().copied().skip_while(after).collect();
        let mut next = 0;
        while next < nodes.len() {
            let wanted = &nodes[next..(next + 100).min(nodes.len())];
            let items = client
                .request("history_items", json!({"bot": bot, "nodes": wanted}))
                .await
                .map_err(drop)?;
            // Fewer than asked when they are large; at least one.
            let items = items["items"].as_array().cloned().unwrap_or_default();
            if items.is_empty() {
                return Ok(None);
            }
            next += items.len();
            if let Some(node) = items.iter().find_map(said_by_person) {
                return Ok(Some(node));
            }
        }
        from = page["next_from"].clone();
        if from.is_null() || paged {
            return Ok(None);
        }
    }
    Ok(None)
}

/// The node of a history entry the person wrote: words of theirs, as an
/// Anthropic tool result is a user message too.
pub(crate) fn said_by_person(entry: &Value) -> Option<i64> {
    let item = &entry["item"];
    let words = crate::responses(item)
        .into_iter()
        .any(|said| said["role"] == "user" && !crate::text(&said).trim().is_empty());
    (entry["from"].is_null()
        && entry["origin"].is_null()
        && words
        && !crate::reactions::is_reaction(&crate::text(item)))
    .then(|| entry["node"].as_i64())
    .flatten()
}

/// Whether the bot listed under a name is another than the one seated.
fn replaced(seated: &Value, listed: &Value) -> bool {
    let id = |record: &Value| record["bot_id"].as_i64();
    id(seated).is_some() && id(listed).is_some() && id(seated) != id(listed)
}

/// A subscriber's task updates, fresh: the memory its own indexes name,
/// and the other subscribers, which are no one's tasks. The bots it
/// watches are seated once its subscriptions say which (`roots`).
fn updates_for(
    subscriber: &str,
    workspace: &Path,
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
        from_sources: &mut tokio::sync::mpsc::Receiver<sources::Event>,
    ) {
        let bots = all_bots(&client).await;
        self.records = (bots.iter())
            .filter_map(|b| Some((b["name"].as_str()?.to_owned(), b.clone())))
            .collect();
        self.subscribers = subs::all(&self.store).into_iter().collect();
        for bot in &bots {
            self.seat(&client, bot).await;
        }
        // Once a lookout: a connection dropped and made again changes no
        // bot's tools.
        if self.started {
            tokio::spawn(refresh_tools(client.clone(), bots.clone()));
        }
        self.resubscribe(true);
        let listed: HashSet<String> = (bots.iter())
            .filter_map(|b| b["name"].as_str().map(str::to_owned))
            .collect();
        for lookout in self.lookouts.values_mut() {
            // The replay's turns were news then; the live ones are now.
            lookout.updates.live = false;
            for bot in &bots {
                if (bot["name"].as_str()).is_some_and(|name| lookout.hears(name)) {
                    lookout.updates.seat(bot);
                }
            }
            lookout.updates.seated(&listed);
            lookout.lineage.insert(lookout.lead.clone());
        }
        let mut live = false;
        self.live = false;
        // News from before the oldest not told yet, as a restart would: a
        // message whose read the dropped connection cut short is read
        // again. What came before was told, or seen live by this lookout,
        // so the stream starts after it, or before the oldest turn still
        // under way, whose events are history: never the whole log again.
        // Only a lookout with no cursor yet reads it all, once.
        self.resume = self.floor().or(self.resume);
        // A turn the listing shows ended, or never running, ended unseen.
        let running: HashMap<&str, i64> = (bots.iter())
            .filter_map(|bot| Some((bot["name"].as_str()?, bot["running_turn"].as_i64()?)))
            .collect();
        self.in_flight
            .retain(|(bot, turn), _| running.get(bot.as_str()).is_some_and(|at| turn >= at));
        let replay = match self.started {
            true => self.replay,
            false => self.replay_floor(),
        };
        self.started = false;
        let after = [self.resume, replay]
            .into_iter()
            .flatten()
            .min()
            .unwrap_or(0);
        if let Err(error) = (client.request("follow", json!({"bot": "*", "after": after}))).await {
            println!(
                "{}",
                json!({"event": "follow_refused", "error": error.code, "after": after})
            );
            return;
        }
        // Goal checks a dropped connection left under way answer the old
        // channel, never this one: none is under way here yet.
        let (checked, mut settled) = tokio::sync::mpsc::channel(64);
        for lookout in self.lookouts.values_mut() {
            // Reads a dropped connection left under way answer the old
            // channel, never this one.
            lookout.reading = 0;
            lookout.updates.replies_dropped();
        }
        self.goals.reconnected();
        let (done, mut results) = tokio::sync::mpsc::channel(64);
        // What the daemon answered a note or a status check: beside the
        // stream, so no call to it ever holds the events up.
        let (woken, mut woke) = tokio::sync::mpsc::channel(64);
        for lookout in self.lookouts.values_mut() {
            // Notes a dropped connection left under way answer the old
            // channel: what they carried is news again.
            lookout.reconnected();
        }
        // Often enough for a note whose news has settled; subscription
        // files less often.
        let mut tick = tokio::time::interval(SETTLE / 2);
        let mut reread: Option<Instant> = None;
        // Past the replay: a turn that ends now left its folder as it is.
        let mut streaming = false;
        loop {
            // The lookouts whose news may have changed: those an event
            // reached, or all of them.
            let mut touched: Option<Vec<String>> = None;
            tokio::select! {
                event = events.recv() => {
                    let Some(event) = event else {
                        println!("{}", json!({"event": "disconnected", "cursor": self.cursor}));
                        return;
                    };
                    // Let go of by the daemon, or by this client, for falling
                    // behind: it connects again from its cursor.
                    if event["event"] == "follow_lagged" {
                        println!("{}", json!({"event": "lagged", "reason": event["reason"]}));
                    }
                    let event = whole_margin_call(&client, event).await;
                    served.event(&event);
                    streaming |= event["event"] == "follow_live";
                    self.snapshot(&client, &event, streaming);
                    // A `subscriptions` call this lookout answered counts
                    // from the next event on, not from the next tick.
                    if served.subscribed() {
                        reread = Some(Instant::now());
                        self.reread(&client).await;
                    }
                    let cursor = event["cursor"].as_i64();
                    // A call is read from the replay too, so one that ends
                    // after the lookout last watched still lands.
                    let news =
                        live || cursor.is_some_and(|at| self.resume.is_some_and(|seen| at > seen));
                    let held = self.picked(&client, &event, news).await;
                    // What happened since the lookout last watched live, as
                    // a task that ended while it was down, is news now.
                    if !live && cursor.is_some_and(|at| self.resume.is_some_and(|seen| at > seen)) {
                        live = true;
                        self.go_live();
                    }
                    if live && !held && cursor > self.cursor {
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
                        self.in_flight.retain(|(bot, _), _| bot != name);
                        self.records.remove(name);
                        if self.lookouts.remove(name).is_some() {
                            self.index();
                        }
                    }
                    // A turn whose goal's check is to run is news only once
                    // it says how the goal stands.
                    let bot_id = (event["bot"].as_str())
                        .and_then(|bot| self.records.get(bot))
                        .and_then(|record| record["bot_id"].as_i64());
                    if let Some((bot, turn)) = self.goals.event(&event, bot_id, live, &client, &checked) {
                        for lookout in self.lookouts.values_mut() {
                            lookout.updates.goal_turn(&bot, turn);
                        }
                    }
                    // Each event goes to the lookouts that hear its bot,
                    // as an informer hands a change to the handlers that
                    // asked for its object; a bot made or deleted, to all.
                    let hearing = self.hearing(&event);
                    if let (Some(bot), Some(turn)) = (event["bot"].as_str(), event["turn"].as_i64()) {
                        match event["event"].as_str() {
                            Some("accepted" | "queued") if !hearing.is_empty() => {
                                if let Some(at) = cursor {
                                    self.in_flight.entry((bot.to_owned(), turn)).or_insert(at);
                                }
                            }
                            Some("turn_finished") => {
                                self.in_flight.remove(&(bot.to_owned(), turn));
                            }
                            _ => {}
                        }
                    }
                    for name in &hearing {
                        let Some(lookout) = self.lookouts.get_mut(name) else { continue };
                        lookout.event(&event);
                        // The oldest event whose news is not told yet: a
                        // restart takes it up again from there.
                        if live && let Some(at) = cursor
                            && lookout.untold()
                        {
                            lookout.from.get_or_insert(at);
                        }
                    }
                    if let (Some("created" | "forked" | "deleted"), Some(bot)) =
                        (event["event"].as_str(), event["bot"].as_str())
                    {
                        self.index_bot(bot);
                    }
                    // The replay tells who belongs to each subscriber and
                    // each task's current brief; what it holds was news then.
                    if !live {
                        for name in &hearing {
                            let Some(lookout) = self.lookouts.get_mut(name) else { continue };
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
                    touched = Some(hearing);
                }
                Some(goals::Settled { bot, turn, outcome }) = settled.recv() => {
                    self.goals.settled(&bot, turn);
                    for lookout in self.lookouts.values_mut() {
                        lookout.updates.goal_settled(&bot, turn, outcome.as_deref());
                    }
                }
                // What sources say is news once the stream is live; until
                // then it waits in the channel.
                Some(event) = from_sources.recv(), if self.live => {
                    for lookout in self.lookouts.values_mut() {
                        lookout.heard_from(&event);
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
                    touched = Some(vec![lead]);
                }
                Some((lead, id, answer)) = woke.recv() => {
                    if let Some(lookout) = self.lookouts.get_mut(&lead)
                        && lookout.id == id
                    {
                        lookout.answered(answer);
                    }
                    touched = Some(vec![lead]);
                }
                _ = tick.tick() => {
                    let now = Instant::now();
                    self.save_cursor();
                    // What `subscriptions` calls wrote since: at once for
                    // one this lookout answered, else now and then.
                    if served.subscribed() || reread.is_none_or(|at| now >= at + REREAD) {
                        reread = Some(now);
                        self.goals.reread();
                        self.runner.reload();
                        self.reread(&client).await;
                    }
                    for lookout in self.lookouts.values_mut() {
                        lookout.updates.expire(now);
                    }
                }
            }
            let names = touched.unwrap_or_else(|| self.lookouts.keys().cloned().collect());
            for name in names {
                let Some(lookout) = self.lookouts.get_mut(&name) else {
                    continue;
                };
                // Words are read beside the stream, so a slow read never
                // stalls it.
                for seen in std::mem::take(&mut lookout.due) {
                    let (client, done) = (client.clone(), done.clone());
                    let (lead, id) = (lookout.lead.clone(), lookout.id);
                    lookout.reading += 1;
                    if seen.reply {
                        lookout.updates.reading_reply(&seen.task, seen.turn);
                    }
                    tokio::spawn(async move {
                        let seen = read_event(&client, seen).await;
                        let _ = done.send((lead, id, seen)).await;
                    });
                }
                lookout.wake(&client, &woken);
                lookout.wake_sources(&client, &woken);
            }
        }
    }

    /// Keep a task's worktree as a completed turn left it, for a task
    /// forked from that turn (snapshot.rs); drop a deleted task's. Each
    /// turn's start and end is noted however the stream stands, so a
    /// snapshot knows of work in its checkout after its turn; snapshots
    /// are taken only once `streaming`, so a replay never takes one of
    /// files that changed since, but a deleted bot's are dropped either
    /// way.
    fn snapshot(&mut self, client: &Arc<Client>, event: &Value, streaming: bool) {
        let (Some(bot), Some(kind)) = (event["bot"].as_str(), event["event"].as_str()) else {
            return;
        };
        let data = &event["data"];
        let turn = event["turn"].as_i64();
        let record = self.records.get(bot);
        let (folder, ended) = match (kind, turn) {
            ("accepted", Some(turn)) => {
                if let Some(workspace) = data["workspace"].as_str() {
                    let workspace = PathBuf::from(workspace);
                    crate::snapshot::worked(&workspace);
                    self.working.insert((bot.to_owned(), turn), workspace);
                }
                return;
            }
            ("turn_finished", Some(turn)) => {
                // A turn begun before the lookout watched worked where its
                // bot is.
                let folder = (self.working.remove(&(bot.to_owned(), turn)))
                    .or_else(|| record?["workspace"].as_str().map(PathBuf::from));
                let Some(folder) = folder else { return };
                let ended = crate::snapshot::worked(&folder);
                (folder, ended)
            }
            ("deleted", _) => {
                self.working.retain(|(name, _), _| name != bot);
                let Some(folder) = record.and_then(|r| r["workspace"].as_str()) else {
                    return;
                };
                (PathBuf::from(folder), 0)
            }
            _ => return,
        };
        let Some(bot_id) = record.and_then(|r| r["bot_id"].as_i64()) else {
            return;
        };
        // Only a task's own worktree: what forks from, and no one else's.
        // A deletion replayed is still dropped, as no snapshot comes after.
        if !(streaming || kind == "deleted") || !crate::task::in_worktree(&folder) {
            return;
        }
        let refs = crate::snapshot::refs(&self.store);
        match kind {
            "turn_finished" if data["status"] == "completed" => {
                let (Some(turn), Some(checkpoint)) = (turn, data["checkpoint"].as_i64()) else {
                    return;
                };
                let done = crate::snapshot::Completed {
                    refs,
                    indexes: crate::snapshot::indexes(&self.store),
                    workspace: folder,
                    bot_id,
                    turn,
                    checkpoint,
                    ended,
                };
                tokio::spawn(crate::snapshot::after_turn(client.clone(), done));
            }
            // A task's own folder may be gone, and `remove_task` drops its
            // snapshots once it is.
            "deleted" => {
                tokio::spawn(crate::snapshot::deleted(refs, folder, bot_id));
            }
            _ => {}
        }
    }

    /// The lookouts an event reaches: those that hear its bot, or every
    /// one for a bot made or deleted, which may change what each watches,
    /// and for what concerns no bot.
    fn hearing(&self, event: &Value) -> Vec<String> {
        match (event["event"].as_str(), event["bot"].as_str()) {
            (Some("created" | "forked" | "deleted"), _) | (_, None) => {
                self.lookouts.keys().cloned().collect()
            }
            (_, Some(bot)) => self.watchers.get(bot).cloned().unwrap_or_default(),
        }
    }

    /// Which lookouts hear each bot: its own, and those that watch it.
    fn index(&mut self) {
        let mut watchers: HashMap<String, Vec<String>> = HashMap::new();
        for (name, lookout) in &self.lookouts {
            watchers.entry(name.clone()).or_default().push(name.clone());
            for bot in &lookout.updates.watched {
                watchers.entry(bot.clone()).or_default().push(name.clone());
            }
        }
        self.watchers = watchers;
    }

    /// Which lookouts hear one bot, after it was made or deleted.
    fn index_bot(&mut self, bot: &str) {
        let hear: Vec<String> = (self.lookouts.iter())
            .filter(|(_, lookout)| lookout.hears(bot))
            .map(|(name, _)| name.clone())
            .collect();
        match hear.is_empty() {
            true => self.watchers.remove(bot),
            false => self.watchers.insert(bot.to_owned(), hear),
        };
    }

    /// The subscription files as they are now: a bot that subscribed is
    /// watched, and each lookout watches what its file says.
    async fn reread(&mut self, client: &Client) {
        let read: HashMap<String, Vec<subs::Sub>> = subs::all(&self.store).into_iter().collect();
        if read == self.subscribers {
            return;
        }
        self.subscribers = read;
        let records: Vec<Value> = (self.subscribers.keys())
            .filter_map(|name| self.records.get(name).cloned())
            .collect();
        for record in records {
            self.seat(client, &record).await;
        }
        self.resubscribe(false);
    }

    /// A lead's `react` call naming one of its project's agents: once it
    /// succeeds as news, its reaction kept under the person's newest message
    /// there, for that agent's window to draw. Whether it waits on the
    /// daemon still, so the event is read again after a restart.
    async fn picked(&mut self, client: &Client, event: &Value, news: bool) -> bool {
        let Some((by, bot, id, emoji, remove, upto)) = self.reacted(event, news) else {
            return false;
        };
        let store = std::fs::canonicalize(&self.store).unwrap_or_else(|_| self.store.clone());
        let key = crate::reactions::key(&store, id);
        match newest_said(client, &bot, upto).await {
            Ok(node) => {
                if let Some(node) = node {
                    let _ = crate::reactions::Picked::apply(&key, node, &by, &emoji, remove);
                }
                false
            }
            Err(()) => true,
        }
    }

    /// The project a bot is in: the longest start of its name whose lead
    /// the daemon lists.
    fn project<'a>(&self, bot: &'a str) -> Option<&'a str> {
        (bot.match_indices('.').rev())
            .map(|(at, _)| &bot[..at])
            .find(|start| self.records.contains_key(&format!("{start}.lead")))
    }

    /// A targeted `react` call that ended well as news: who called, the
    /// agent named and its id, the emoji, whether it comes off, and the
    /// newest message that agent had taken when the call began.
    fn reacted(
        &mut self,
        event: &Value,
        news: bool,
    ) -> Option<(String, String, i64, String, bool, Option<i64>)> {
        let data = &event["data"];
        let by = event["bot"].as_str()?;
        if let (Some("accepted" | "steered"), Some(node)) =
            (event["event"].as_str(), data["node"].as_i64())
        {
            self.taken.insert(by.to_owned(), node);
            return None;
        }
        let call = (
            by.to_owned(),
            event["turn"].as_i64()?,
            data["call_id"].as_str()?.to_owned(),
        );
        match event["event"].as_str() {
            Some("tool_started") if data["name"] == crate::reactions::TOOL => {
                let arguments = (data["arguments"].as_str())
                    .and_then(|a| serde_json::from_str::<Value>(a).ok())?;
                let bot = crate::reactions::target(&arguments)?.to_owned();
                let (emoji, remove) = crate::reactions::call(&arguments)?;
                let upto = self.taken.get(&bot).copied();
                self.reacting.insert(call, (bot, emoji, remove, upto));
                None
            }
            // The daemon refuses a call that does not fit, or that the bot
            // may not make.
            Some("tool_completed") => {
                let (bot, emoji, remove, upto) = self.reacting.remove(&call)?;
                if !news || data["failed"] == true || data["denied"] == true {
                    return None;
                }
                let id = (self.records.get(&bot))
                    .filter(|_| {
                        bot != by
                            && self
                                .project(&bot)
                                .is_some_and(|p| self.project(by) == Some(p))
                    })
                    .and_then(|record| record["bot_id"].as_i64())?;
                Some((by.to_owned(), bot, id, emoji, remove, upto))
            }
            _ => None,
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
        let updates = updates_for(lead, &workspace, &self.subscribers, self.live);
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
            if let Some(lookout) = self.lookouts.get_mut(lead) {
                lookout.waiting(&turn);
            }
        }
        if fresh {
            self.index();
        }
    }

    /// Each lookout's subscriptions from the files last read: a bot is
    /// watched only while its file lasts. What each watches is worked out
    /// again only where its subscriptions changed, or for `all` once the
    /// bots are listed afresh.
    fn resubscribe(&mut self, all: bool) {
        let subscribers = &self.subscribers;
        self.lookouts
            .retain(|name, _| subscribers.contains_key(name));
        for (name, lookout) in self.lookouts.iter_mut() {
            lookout.updates.subscribed(others(name, subscribers));
            let subs = subscribers.get(name).cloned().unwrap_or_default();
            if all || subs != lookout.subs {
                lookout.subs = subs;
                lookout.roots(&self.records);
                lookout.rehear();
            }
        }
        self.index();
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

    /// Up to just before the oldest news a lookout has not told, or the
    /// oldest turn whose goal's check runs, which a restart or a reconnect
    /// takes up again: news only in memory, or a read or check a dropped
    /// connection cut short, is told or run again, not lost.
    fn floor(&mut self) -> Option<i64> {
        for lookout in self.lookouts.values_mut() {
            if !lookout.untold() {
                lookout.from = None;
            }
        }
        let floor = (self.lookouts.values().filter_map(|l| l.from))
            .chain(self.goals.floor())
            .min();
        match (self.cursor, floor) {
            (Some(at), Some(floor)) => Some(at.min(floor - 1)),
            (at, _) => at,
        }
    }

    /// The newest event seen live, kept beside the store for the next
    /// lookout, written whole and moved in.
    fn save_cursor(&mut self) {
        let Some(cursor) = self.floor() else {
            return;
        };
        let replay = self.replay_floor().map_or(cursor, |at| at.min(cursor));
        if self.saved == Some((cursor, replay)) {
            return;
        }
        let path = cursor_path(&self.store);
        let partial = path.with_extension(format!("cursor.{}.partial", std::process::id()));
        if std::fs::write(&partial, format!("{cursor} {replay}")).is_ok()
            && std::fs::rename(&partial, &path).is_ok()
        {
            self.saved = Some((cursor, replay));
        }
    }

    /// Just before the oldest turn under way that a lookout hears.
    fn replay_floor(&self) -> Option<i64> {
        self.in_flight.values().min().map(|at| at - 1)
    }
}

/// What the last lookout kept: where its news ended, and where its stream
/// starts again, the same when it kept one.
fn marks(store: &Path) -> (Option<i64>, Option<i64>) {
    let text = std::fs::read_to_string(cursor_path(store)).unwrap_or_default();
    let mut marks = text.split_whitespace().map(|mark| mark.parse().ok());
    let news = marks.next().flatten();
    (news, marks.next().flatten().or(news))
}

/// Where the newest event a lookout saw live is kept: beside the store.
fn cursor_path(store: &Path) -> PathBuf {
    let mut path = store.as_os_str().to_owned();
    path.push(".lookout.cursor");
    PathBuf::from(path)
}

/// The most of the log `status` reads, from its end.
const STATUS_BYTES: u64 = 1 << 20;

/// Whether a lookout watches `store`'s daemon: one holds its lock. The pid
/// the file names is no answer, since after a restart another process may
/// have it. The lock is tried shared and let go at once; a lookout
/// starting meanwhile tries again (`lock`).
pub fn running(store: &std::path::Path) -> bool {
    use std::os::fd::AsRawFd;
    let Ok(file) = std::fs::File::open(beside(store, ".lookout.lock")) else {
        return false;
    };
    // SAFETY: flock on a descriptor `file` owns; closing it lets go.
    let taken = unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_SH | libc::LOCK_NB) } == 0;
    !taken && std::io::Error::last_os_error().raw_os_error() == Some(libc::EWOULDBLOCK)
}

/// One line on the lookout for a subscriber, as `/status` shows it:
/// whether one runs and watches it, and today's notes.
pub fn status(store: &std::path::Path, lead: &str) -> String {
    if !running(store) {
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

/// Bring every agent Sonny declared on up to this build's tools, as
/// opening its window would, so one no one opens after an upgrade, such
/// as a task out of sight, gets a tool added since. A busy one is tried
/// again each minute, for an hour.
async fn refresh_tools(client: Arc<Client>, bots: Vec<Value>) {
    let mut left = bots;
    for _ in 0..60 {
        let mut busy = Vec::new();
        for bot in left {
            let Ok(mut record) = client
                .request(
                    "inspect",
                    json!({"bot": bot["name"], "bot_id": bot["bot_id"]}),
                )
                .await
            else {
                continue;
            };
            record["name"] = bot["name"].clone();
            record["bot_id"] = bot["bot_id"].clone();
            let Some(fields) = crate::declared::refresh(&record) else {
                continue;
            };
            match client.request("configure", Value::Object(fields)).await {
                Ok(_) => println!(
                    "{}",
                    json!({"event": "tools_refreshed", "bot": bot["name"]})
                ),
                Err(error) if error.code == "bot_busy" => busy.push(bot),
                Err(error) => println!(
                    "{}",
                    json!({"event": "tools_refresh_failed", "bot": bot["name"], "error": error.code})
                ),
            }
        }
        if busy.is_empty() {
            return;
        }
        left = busy;
        tokio::time::sleep(Duration::from_secs(60)).await;
    }
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
    /// The other leads of the lead's folder and every bot they or theirs
    /// made: theirs, which the lead's tasks subscription never names.
    theirs: HashSet<String>,
    /// Steers waiting to be absorbed, by task and steer turn: who sent it,
    /// or `None` for the subscriber.
    steers: HashMap<(String, i64), Option<String>>,
    /// Queued turns whose words were read when they were queued.
    queued: HashSet<(String, i64)>,
    /// Turns a lookout's note started in a lead a subscription names: that
    /// lead's own work with its tasks, never news here.
    noted: HashSet<(String, i64)>,
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
    /// What sources said that the subscriber is to hear, oldest first.
    heard: Vec<sources::Heard>,
    /// When the first of those came.
    heard_since: Option<Instant>,
    /// A note on its way: what it tells, its key and how it goes, until
    /// the daemon answers.
    sending: Option<(updates::Told, Vec<String>, &'static str)>,
    /// What sources said, on its way in a note, until the daemon answers.
    sourcing: Option<(Vec<sources::Heard>, Vec<String>, &'static str)>,
    /// The daemon is asked whether the subscriber rests: how many of its
    /// own events had come when it was asked. An answer after more came
    /// is older than what they said, and is let go.
    asking: Option<u64>,
    /// How many of the subscriber's own events it heard.
    own: u64,
    log: Option<PathBuf>,
}

/// What the daemon answered a lookout, beside the stream.
enum Answer {
    /// The subscriber as `bots` lists it now, if it does.
    Listed(Option<Value>),
    /// A note was submitted, or refused with this code.
    Noted(Result<(), String>),
    /// A note from sources was submitted, or refused with this code.
    Sourced(Result<(), String>),
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
            theirs: HashSet::new(),
            steers: HashMap::new(),
            queued: HashSet::new(),
            noted: HashSet::new(),
            briefs: HashMap::new(),
            due: Vec::new(),
            reading: 0,
            urgent: false,
            refused: None,
            since: None,
            from: None,
            heard: Vec::new(),
            heard_since: None,
            sending: None,
            sourcing: None,
            asking: None,
            own: 0,
            log: None,
        }
    }

    /// The bots whose news the subscriber hears: those its subscriptions
    /// watch, among `names`, every bot listed. Run when the subscriptions
    /// change; a bot made or deleted since is checked alone (`check`).
    fn sync<'a>(&mut self, names: impl IntoIterator<Item = &'a String>) {
        let watched = (names.into_iter())
            .filter(|bot| self.watches(bot))
            .cloned()
            .collect();
        self.updates.watched = watched;
        // Another subscriber is a task here only where one names it as it is.
        self.updates.named = (self.subs.iter())
            .flat_map(|sub| sub.bots.iter())
            .filter(|glob| !glob.contains('*'))
            .cloned()
            .collect();
    }

    /// Whether one bot is watched now. A bot made or deleted changes no
    /// other bot's standing, so this is all it takes.
    fn check(&mut self, bot: &str) {
        match self.watches(bot) {
            true => self.updates.watched.insert(bot.to_owned()),
            false => self.updates.watched.remove(bot),
        };
    }

    /// Lineage is kept up during the replay too, so tasks made earlier count.
    fn lineage_of(&mut self, event: &Value) {
        let (Some(kind), Some(bot)) = (event["event"].as_str(), event["bot"].as_str()) else {
            return;
        };
        if matches!(kind, "created" | "forked" | "deleted") {
            self.lineage_change(kind, bot, event);
            self.check(bot);
        }
    }

    /// Whether the subscriber keeps a bot's news and doings: its own, and
    /// those of the bots it watches.
    fn hears(&self, bot: &str) -> bool {
        bot == self.lead || self.updates.watched.contains(bot)
    }

    /// The bots it watches that it has not seated yet, as the daemon lists
    /// them: seated once, so what their events said since stands.
    fn seat_watched(&mut self, records: &HashMap<String, Value>) {
        let unseated: Vec<&String> = (self.updates.watched.iter())
            .filter(|bot| !self.updates.has(bot))
            .collect();
        let records: Vec<&Value> = unseated
            .iter()
            .filter_map(|bot| records.get(*bot))
            .collect();
        for record in records {
            self.updates.seat(record);
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
                if self.sibling(bot) || self.sibling(by) || self.theirs.contains(by) {
                    self.theirs.insert(bot.to_owned());
                }
                if self.globs(bot) {
                    self.globbed.insert(bot.to_owned());
                }
            }
            "deleted" if bot != self.lead => {
                self.lineage.remove(bot);
                self.globbed.remove(bot);
                self.theirs.remove(bot);
                // A root keeps its place under its name, so what it makes
                // once made again is still watched.
                for (root, made) in self.others.iter_mut() {
                    if root != bot {
                        made.remove(bot);
                    }
                }
                self.briefs.remove(bot);
                self.noted.retain(|(task, _)| task != bot);
            }
            _ => {}
        }
    }

    /// Who wrote a turn, as the note names them; `None` for the
    /// subscriber, whose words are no news to it.
    fn author(&self, data: &Value) -> Option<String> {
        // A lookout's own note to a lead another subscriber watches, or a
        // goal's next turn, whose end alone is news.
        if (data["request_id"].as_str())
            .is_some_and(|id| id.starts_with("lookout-") || id.starts_with(goals::REQUEST))
        {
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
        // Only what it watches, or watched until this event, is kept: a
        // task made is watched from its `created`, one deleted until its
        // `deleted`.
        let heard = event["bot"].as_str().is_none_or(|bot| self.hears(bot));
        if event["bot"] == self.lead.as_str() {
            self.own += 1;
        }
        self.lineage_of(event);
        if heard || event["bot"].as_str().is_some_and(|bot| self.hears(bot)) {
            self.updates.event(event, Instant::now());
        }
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
        if matches!(kind, "accepted" | "queued")
            && (data["request_id"].as_str()).is_some_and(|id| id.starts_with("lookout-"))
        {
            self.noted.insert(key.clone());
        }
        // How a turn its own lookout's note started ends is that lead's own
        // work with its tasks.
        if kind == "turn_finished" && self.noted.remove(&key) {
            return;
        }
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
            // What it replied goes with how the turn ended.
            "turn_finished" => {
                self.due.push(Seen {
                    task: bot.to_owned(),
                    turn,
                    from: bot.to_owned(),
                    node: data["checkpoint"].as_i64(),
                    reply: true,
                    brief: None,
                    text: String::new(),
                });
            }
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
            reply: false,
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
        // Another lead's tasks stay theirs once that lead is gone: their
        // maker is still named as one.
        self.theirs = (records.iter())
            .filter(|(name, record)| {
                self.sibling(name)
                    || record["created_by"]
                        .as_str()
                        .is_some_and(|by| self.sibling(by))
            })
            .map(|(name, _)| name.clone())
            .collect();
        grow(&mut self.theirs, records);
        self.globbed = (records.iter())
            .filter(|(name, record)| self.globs(name) && record["expire_ms"].is_null())
            .map(|(name, _)| name.clone())
            .collect();
        self.sync(records.keys());
        self.seat_watched(records);
    }

    /// Whether `bot` is another lead of this lead's folder.
    fn sibling(&self, bot: &str) -> bool {
        bot != self.lead
            && crate::project::lead_project(bot).is_some()
            && crate::project::lead_project(bot) == crate::project::lead_project(&self.lead)
    }

    /// Whether a subscription's `--bot` glob names this bot. The tasks
    /// subscription Sonny gives a lead never names another lead's: one the
    /// lead asked for may.
    fn globs(&self, bot: &str) -> bool {
        let theirs = self.theirs.contains(bot);
        bot != self.lead
            && (self.subs.iter()).any(|sub| {
                !(theirs && sub.name == "tasks")
                    && sub.bots.iter().any(|glob| subs::matches(glob, bot))
            })
    }

    /// A message read: news for the next note. The person's own words cut
    /// into the subscriber's running turn, as their words to it would.
    fn heard(&mut self, seen: &Seen) {
        if seen.reply {
            return self.updates.replied(&seen.task, seen.turn, &seen.text);
        }
        let words = (seen.from == PERSON).then_some(seen.text.as_str());
        let news = (self.updates).said(&seen.task, seen.turn, &seen.from, words);
        if news && words.is_some() && self.updates.busy() {
            self.urgent = true;
        }
    }

    /// Whether anything it heard is not told yet: messages to read or
    /// being read, a turn held on a trigger or its goal's check, news, or
    /// a note the daemon has not answered.
    fn untold(&self) -> bool {
        !self.due.is_empty()
            || self.reading > 0
            || self.updates.goal_pending()
            || self.updates.holding()
            || self.updates.news()
            || self.sending.is_some()
    }

    /// Tell the subscriber what happened: at once, cutting into its turn,
    /// when the person wrote in a task; else as soon as it rests. The note
    /// goes beside the stream, one at a time: what the daemon answers
    /// comes back to `answered`.
    fn wake(&mut self, client: &Arc<Client>, woken: &Woken) {
        let now = Instant::now();
        self.since = match self.updates.news() {
            true => Some(self.since.unwrap_or(now)),
            false => None,
        };
        if let Some(since) = self.since
            && self.updates.doubt(now, since)
        {
            self.ask(client, woken, now);
        }
        if self.sending.is_some() {
            return;
        }
        let Some(steer) = self.wake_due(now) else {
            return;
        };
        let told = self.updates.take();
        // All there is waits on replies being read.
        if told.items.is_empty() && told.memory.is_none() {
            self.updates.unsent(told);
            return;
        }
        let prompt = note(&told, steer);
        let key: Vec<String> = told.items.iter().map(|line| line.key.clone()).collect();
        let request_id = format!("lookout-{}", digest(&key.join(",")));
        let delivery = if steer { "steer" } else { "queue" };
        let asked = json!({"bot": self.lead, "bot_id": self.updates.lead_id(), "prompt": prompt,
            "request_id": request_id, "delivery": delivery});
        self.sending = Some((told, key, delivery));
        self.send(client, woken, asked, Answer::Noted);
    }

    /// Ask the daemon whether the subscriber rests: a missed event would
    /// otherwise hold its news until its next turn.
    fn ask(&mut self, client: &Arc<Client>, woken: &Woken, now: Instant) {
        if self.asking.is_some() {
            return;
        }
        self.asking = Some(self.own);
        self.updates.asking(now);
        let (client, woken) = (client.clone(), woken.clone());
        let (lead, id) = (self.lead.clone(), self.id);
        tokio::spawn(async move {
            let listed = (client
                .request("bots", json!({"name": lead, "limit": 1}))
                .await)
                .ok()
                .and_then(|page| page["bots"].get(0).cloned());
            let _ = woken.send((lead, id, Answer::Listed(listed))).await;
        });
    }

    /// Submit a note beside the stream; its answer comes back as `answer`.
    fn send(
        &self,
        client: &Arc<Client>,
        woken: &Woken,
        asked: Value,
        answer: fn(Result<(), String>) -> Answer,
    ) {
        let (client, woken) = (client.clone(), woken.clone());
        let (lead, id) = (self.lead.clone(), self.id);
        tokio::spawn(async move {
            let submitted = client.request("submit", asked).await;
            let _ = woken
                .send((lead, id, answer(submitted.map(drop).map_err(|e| e.code))))
                .await;
        });
    }

    /// What the daemon answered this lookout.
    fn answered(&mut self, answer: Answer) {
        let now = Instant::now();
        match answer {
            Answer::Listed(listed) => {
                let asked = self.asking.take();
                if asked == Some(self.own)
                    && let Some(record) = listed
                    && let Some(was) = self.updates.rests(now, &record)
                {
                    self.write(&json!({"at_ms": epoch_ms(), "lead": self.lead,
                        "rests": format!("the daemon lists it idle; its events said {was}")}));
                }
            }
            Answer::Noted(submitted) => {
                let Some((told, key, delivery)) = self.sending.take() else {
                    return;
                };
                match submitted {
                    Ok(()) => {
                        self.refused = None;
                        self.since = None;
                        // The person's words a bounded note left out still cut in.
                        self.urgent &= self.updates.quoting();
                        self.updates.sent(told);
                        self.write(&json!({"at_ms": epoch_ms(), "lead": self.lead, "woke": key,
                            "delivery": delivery}));
                    }
                    // The same news twice is one note.
                    Err(code) if code == "idempotency_conflict" => {
                        self.urgent &= self.updates.quoting();
                        self.updates.sent(told);
                    }
                    Err(code) => {
                        self.updates.unsent(told);
                        self.write(
                            &json!({"at_ms": epoch_ms(), "lead": self.lead, "wake_failed": code}),
                        );
                        self.refused = Some(now);
                    }
                }
            }
            Answer::Sourced(submitted) => {
                let Some((sent, key, delivery)) = self.sourcing.take() else {
                    return;
                };
                match submitted {
                    Ok(()) => {
                        self.refused = None;
                        self.write(&json!({"at_ms": epoch_ms(), "lead": self.lead, "woke": key,
                            "delivery": delivery, "sources": true}));
                    }
                    // The same news twice is one note.
                    Err(code) if code == "idempotency_conflict" => {}
                    Err(code) => {
                        let mut back = sent;
                        back.append(&mut self.heard);
                        self.heard = back;
                        self.heard_since.get_or_insert(now);
                        self.write(&json!({"at_ms": epoch_ms(), "lead": self.lead,
                            "wake_failed": code, "sources": true}));
                        self.refused = Some(now);
                        return;
                    }
                }
                self.heard_since = (!self.heard.is_empty()).then_some(now);
            }
        }
    }

    /// The connection dropped: answers under way are lost with it, so what
    /// their notes carried is news again, as when one is refused.
    fn reconnected(&mut self) {
        self.asking = None;
        if let Some((told, ..)) = self.sending.take() {
            self.updates.unsent(told);
        }
        if let Some((mut sent, ..)) = self.sourcing.take() {
            sent.append(&mut self.heard);
            self.heard = sent;
            self.heard_since.get_or_insert(Instant::now());
        }
    }

    /// The subscriber's own newest turn, as `turns` lists it: what it
    /// waits on is no news to it.
    fn waiting(&mut self, turn: &Value) {
        if turn["status"] == "waiting" {
            let handles = turn["waiting_on"]["handles"].clone();
            self.event(
                &json!({"bot": self.lead, "turn": turn["turn"], "event": "turn_waiting",
                "data": {"handles": handles, "any": turn["waiting_on"]["any"]}}),
            );
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
    /// The subscriptions that hear a source now.
    fn hearing(&self, source: &str) -> Vec<String> {
        (self.subs.iter())
            .filter(|sub| sub.sources.iter().any(|name| name == source))
            .map(|sub| sub.name.clone())
            .collect()
    }

    /// Events still waiting, told through the subscriptions that hear
    /// them now: a source no longer heard is dropped.
    fn rehear(&mut self) {
        let mut heard = std::mem::take(&mut self.heard);
        heard.retain_mut(|heard| {
            heard.subs = self.hearing(&heard.event.source);
            !heard.subs.is_empty()
        });
        self.heard = heard;
        if self.heard.is_empty() {
            self.heard_since = None;
        }
    }

    /// A source's event, held for the next note when a subscription
    /// hears that source.
    fn heard_from(&mut self, event: &sources::Event) {
        let subs = self.hearing(&event.source);
        if subs.is_empty() {
            return;
        }
        if self.heard.len() >= HEARD {
            let dropped = self.heard.remove(0);
            self.write(
                &json!({"at_ms": epoch_ms(), "lead": self.lead, "source": dropped.event.source,
                "skipped": "too many events waiting for the subscriber"}),
            );
        }
        self.heard_since.get_or_insert_with(Instant::now);
        self.heard.push(sources::Heard {
            event: event.clone(),
            subs,
        });
    }

    /// Tell the subscriber what its sources said: at once for an event its
    /// source marked urgent, else once it rests and the news has settled.
    /// The note goes with the lookout's origin, never as the person's words,
    /// beside the stream as `wake`'s does.
    fn wake_sources(&mut self, client: &Arc<Client>, woken: &Woken) {
        let now = Instant::now();
        let Some(since) = self.heard_since else {
            return;
        };
        // A missed event would hold the note until the subscriber's next
        // turn: once news has waited on it a while, ask the daemon.
        if self.updates.waited(now, since) {
            self.ask(client, woken, now);
        }
        if self.sourcing.is_some() {
            return;
        }
        let steer = self.heard.iter().any(|heard| heard.event.urgent);
        if self.refused(now) || !(steer || (now >= since + SETTLE && self.updates.idle())) {
            return;
        }
        // A bounded note sends what is urgent first, and always at least one.
        self.heard.sort_by_key(|heard| !heard.event.urgent);
        let mut bytes = 0;
        let take = (self.heard.iter().take(NOTE_ITEMS))
            .take_while(|heard| {
                bytes += sources::line(heard).len() + 1;
                bytes <= sources::NOTE_BYTES
            })
            .count()
            .max(1);
        let sent: Vec<sources::Heard> = self.heard.drain(..take).collect();
        let prompt = sources::note(&sent, self.heard.len());
        let key: Vec<String> = (sent.iter())
            .map(|heard| {
                let event = &heard.event;
                let id = event.id.as_deref().unwrap_or_default();
                format!("{}/{id}/{}/{}", event.source, event.at_ms, event.seq)
            })
            .collect();
        let request_id = format!("lookout-{}", digest(&key.join(",")));
        let delivery = if steer { "steer" } else { "queue" };
        let asked = json!({"bot": self.lead, "bot_id": self.updates.lead_id(), "prompt": prompt,
            "request_id": request_id, "delivery": delivery, "origin": sources::ORIGIN});
        self.heard_since = None;
        self.sourcing = Some((sent, key, delivery));
        self.send(client, woken, asked, Answer::Sourced);
    }

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

    #[tokio::test]
    async fn a_long_margin_call_is_read_whole_for_its_note_s_id() {
        // The id after a long text, as a model may write it.
        let whole = format!(r#"{{"markdown": "{}", "id": "results"}}"#, "x".repeat(3000));
        let kept = whole.clone();
        let client = crate::tests::daemon_answering(move |asked| match asked["op"].as_str() {
            Some("history_nodes") => json!({"nodes": [{"node": 5}]}),
            _ => json!({"items": [{"node": 5, "item": {"type": "function_call",
                "call_id": "c9", "name": crate::margin::TOOL, "arguments": kept}}]}),
        })
        .await;
        let started = json!({"event": "tool_started", "bot": "calc.quick", "data": {
            "name": crate::margin::TOOL, "call_id": "c9",
            "arguments": whole[..2048], "arguments_truncated": true}});
        assert_eq!(crate::margin::called(&started["data"]), None);
        let event = whole_margin_call(&client, started).await;
        assert_eq!(
            crate::margin::called(&event["data"]),
            Some(("results".into(), false))
        );
        // Any other event is left as it is.
        let other = json!({"event": "tool_started", "data": {"name": "shell"}});
        assert_eq!(whole_margin_call(&client, other.clone()).await, other);
    }

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
        let listed = lookout.updates.listed();
        lookout.sync(&listed);
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
            reply: false,
            brief: None,
            text: text.into(),
        }
    }

    fn sub(name: &str, tasks_of: &[&str], bots: &[&str]) -> subs::Sub {
        subs::Sub {
            name: name.into(),
            tasks_of: tasks_of.iter().map(|b| b.to_string()).collect(),
            bots: bots.iter().map(|b| b.to_string()).collect(),
            sources: Vec::new(),
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
             Each ended turn comes with what the task replied; read a turn with one wait call (timeout_ms 0) only when you need more of it, and act where something changes the work. \
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
    fn a_task_s_reply_goes_with_how_its_turn_ended() {
        let mut lookout = seated("idle");
        lookout.event(&ev(
            "calc.fix",
            4,
            "accepted",
            json!({"from": {"bot": "calc.lead"}}),
        ));
        let done = json!({"status": "completed", "checkpoint": 42});
        lookout.event(&ev("calc.fix", 4, "turn_finished", done));
        let read = lookout.due.pop().unwrap();
        assert!(read.reply && read.node == Some(42));
        lookout.heard(&Seen {
            text: "Fixed avg.\n- skips empty lists".into(),
            ..read
        });
        // Read before its turn was news, as a trigger's answer, it waits.
        lookout.heard(&Seen {
            reply: true,
            ..said(9, "calc.fix", "early")
        });
        let told = lookout.updates.take();
        let note = note(&told, false);
        assert!(note.contains(
            "- calc.fix: turn:calc.fix/4 completed, asked by you, and replied:\n  > Fixed avg.\n  > - skips empty lists\n\
             Lines marked > are a task's own reply, not the person's words.\n"
        ), "{note}");
        assert!(!note.contains("early"));
    }

    #[test]
    fn a_steer_leaves_a_turn_whose_reply_is_being_read_for_the_next_note() {
        let mut lookout = seated("running");
        let done = json!({"status": "completed", "checkpoint": 42});
        lookout.event(&ev("calc.fix", 4, "turn_finished", done));
        let read = lookout.due.pop().unwrap();
        lookout.updates.reading_reply(&read.task, read.turn);
        // The person writes in a task while the reply is read: the steer
        // carries their words, not the turn's end without its reply.
        lookout.heard(&said(5, PERSON, "use tabs"));
        let told = lookout.updates.take();
        let steered = note(&told, true);
        assert!(steered.contains("use tabs"), "{steered}");
        assert!(!steered.contains("turn:calc.fix/4"), "{steered}");
        lookout.updates.sent(told);
        assert!(lookout.updates.news());
        // Nothing else to tell: no note until the read is in.
        let told = lookout.updates.take();
        assert!(told.items.is_empty());
        lookout.updates.unsent(told);
        lookout.heard(&Seen {
            text: "Fixed avg.".into(),
            ..read
        });
        let told = lookout.updates.take();
        let next = note(&told, false);
        assert!(
            next.contains("turn:calc.fix/4 completed") && next.contains("  > Fixed avg."),
            "{next}"
        );
    }

    #[test]
    fn a_reply_read_with_no_words_holds_its_turn_no_more() {
        let mut lookout = seated("idle");
        let done = json!({"status": "completed", "checkpoint": 42});
        lookout.event(&ev("calc.fix", 4, "turn_finished", done));
        let read = lookout.due.pop().unwrap();
        lookout.updates.reading_reply(&read.task, read.turn);
        lookout.heard(&read);
        let told = lookout.updates.take();
        assert!(note(&told, false).contains("turn:calc.fix/4 completed"));
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
    fn a_lead_a_subscription_names_is_news_as_a_task_is_but_not_its_lookout_s_turns() {
        let mut lookout = lookout();
        lookout.event(&json!({"bot": "calc.lead", "event": "created", "data": {"bot_id": 1, "status": "idle"}}));
        lookout.event(&json!({"bot": "web.lead", "event": "created", "data": {"bot_id": 7, "status": "idle"}}));
        lookout.event(&json!({"event": "follow_live"}));
        // Another subscriber, so no task of this lead's until the person
        // has it watched by name.
        lookout.updates.subscribed(["web.lead".to_owned()].into());
        let mut sub = subs::Sub::lead("calc.lead", "calc");
        sub.name = "web".into();
        sub.tasks_of.clear();
        sub.bots = vec!["web.lead".into()];
        assert!(!lookout.watches("web.lead"));
        lookout.subs.push(sub);
        lookout.roots(&HashMap::from([(
            "web.lead".to_owned(),
            json!({"name": "web.lead", "bot_id": 7}),
        )]));
        assert!(lookout.watches("web.lead"));
        // Its own lookout's note, and the person's question.
        let note = json!({"request_id": "lookout-0123456789abcdef"});
        lookout.event(&ev("web.lead", 2, "accepted", note));
        lookout.event(&ev(
            "web.lead",
            2,
            "turn_finished",
            json!({"status": "completed"}),
        ));
        lookout.event(&ev("web.lead", 3, "accepted", json!({"node": 40})));
        lookout.event(&ev(
            "web.lead",
            3,
            "turn_finished",
            json!({"status": "completed"}),
        ));
        let told = lookout.updates.take();
        let texts: Vec<&str> = told.items.iter().map(|l| l.text.as_str()).collect();
        assert!(texts.iter().all(|t| !t.contains("web.lead/2")), "{texts:?}");
        assert!(texts.iter().any(|t| t.contains("web.lead/3")), "{texts:?}");
        // Its words to read, and its reply.
        let read: Vec<(i64, bool)> = lookout.due.iter().map(|s| (s.turn, s.reply)).collect();
        assert_eq!(read, [(3, false), (3, true)]);
        assert!(lookout.noted.is_empty());
        // What it makes is its own, never this lead's.
        lookout.event(&json!({"bot": "web.fix", "event": "created",
            "data": {"bot_id": 8, "status": "idle", "created_by": "web.lead", "created_by_id": 7}}));
        assert!(!lookout.watches("web.fix"));
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
            runner: sources::Runner::new(store.join("sources"), tokio::sync::mpsc::channel(1).0),
            store,
            records: (records.iter())
                .map(|r| (r["name"].as_str().unwrap().to_owned(), r.clone()))
                .collect(),
            working: HashMap::new(),
            subscribers: HashMap::new(),
            socket: PathBuf::new(),
            lookouts: HashMap::new(),
            log: PathBuf::new(),
            seated: 0,
            live: true,
            cursor: None,
            saved: None,
            resume: None,
            replay: None,
            in_flight: HashMap::new(),
            reacting: HashMap::new(),
            taken: HashMap::new(),
            watchers: HashMap::new(),
            started: false,
            goals: goals::Goals::new(Path::new("/nonexistent"), Path::new("")),
        }
    }

    #[test]
    fn a_lead_s_reaction_in_a_task_lands_once_its_own_call_ends_well_as_news() {
        let record = |name: &str, id: i64, by: Option<&str>| json!({"name": name, "bot_id": id, "created_by": by});
        let mut service = service(
            PathBuf::new(),
            &[
                record("calc.lead", 1, None),
                record("calc.speed", 2, Some("calc.lead")),
                record("ws.lead", 3, None),
                record("ws.fix", 4, Some("ws.lead")),
                record("ws.web.lead", 5, None),
                record("ws.web.fix", 6, Some("ws.web.lead")),
            ],
        );
        let started = |bot: &str, arguments: Value| {
            json!({"event": "tool_started", "bot": bot, "turn": 3, "data": {"call_id": "c1",
                "name": crate::reactions::TOOL, "arguments": arguments.to_string()}})
        };
        let ended = |bot: &str, failed: bool| {
            json!({"event": "tool_completed", "bot": bot, "turn": 3,
                "data": {"call_id": "c1", "failed": failed}})
        };
        let eyes = json!({"emoji": "👀", "bot": "calc.speed"});
        let took = |node: i64| json!({"event": "accepted", "bot": "calc.speed", "turn": 2, "data": {"node": node}});
        service.reacted(&took(9), true);
        // Another bot's call of the same id leaves the lead's waiting.
        assert!(
            service
                .reacted(&started("calc.lead", eyes.clone()), true)
                .is_none()
        );
        // What the person wrote there after the lead reacted is not what it
        // reacted to.
        service.reacted(&took(12), true);
        assert!(service.reacted(&ended("calc.speed", false), true).is_none());
        assert_eq!(
            service.reacted(&ended("calc.lead", false), true),
            Some((
                "calc.lead".into(),
                "calc.speed".into(),
                2,
                "👀".into(),
                false,
                Some(9)
            ))
        );
        // A refused call, or one that ended before the lookout last watched.
        service.reacted(&started("calc.lead", eyes.clone()), true);
        assert!(service.reacted(&ended("calc.lead", true), true).is_none());
        service.reacted(&started("calc.lead", eyes), false);
        assert!(service.reacted(&ended("calc.lead", false), false).is_none());
        // Not on itself, nor outside its project.
        service.reacted(
            &started("calc.lead", json!({"emoji": "👀", "bot": "calc.lead"})),
            true,
        );
        assert!(service.reacted(&ended("calc.lead", false), true).is_none());
        // A project inside another is a project of its own.
        let across = |by: &str, bot: &str, service: &mut Service| {
            service.reacted(&started(by, json!({"emoji": "👀", "bot": bot})), true);
            service
                .reacted(&ended(by, false), true)
                .map(|landed| landed.2)
        };
        assert_eq!(across("ws.web.lead", "ws.fix", &mut service), None);
        assert_eq!(across("ws.lead", "ws.web.fix", &mut service), None);
        assert_eq!(across("ws.web.lead", "ws.web.fix", &mut service), Some(6));
        assert_eq!(across("ws.lead", "ws.fix", &mut service), Some(4));
        assert!(service.reacting.is_empty());
    }

    #[test]
    fn a_lead_s_reaction_lands_on_words_the_person_wrote() {
        let said = |item: Value, extra: Value| {
            let mut entry = json!({"node": 7, "item": item});
            entry
                .as_object_mut()
                .unwrap()
                .extend(extra.as_object().unwrap().clone());
            said_by_person(&entry)
        };
        let words =
            json!({"role": "user", "content": [{"type": "input_text", "text": "add color"}]});
        assert_eq!(said(words.clone(), json!({})), Some(7));
        // An Anthropic tool result is a user message with no words of theirs.
        let result = json!({"role": "user", "content": [
            {"type": "tool_result", "tool_use_id": "t1", "content": "ok"}]});
        assert_eq!(said(result, json!({})), None);
        // Another agent's prompt, or a client's, is not the person's.
        assert_eq!(
            said(words.clone(), json!({"from": {"bot": "calc.lead"}})),
            None
        );
        assert_eq!(said(words, json!({"origin": "lookout"})), None);
        assert_eq!(
            said(
                json!({"role": "assistant", "content": [{"type": "output_text", "text": "hi"}]}),
                json!({})
            ),
            None
        );
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
        service.resubscribe(true);
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
        service.resubscribe(true);
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
        service.resubscribe(true);
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
    fn each_event_reaches_only_the_lookouts_that_hear_its_bot() {
        let record = |name: &str, id: i64, by: Option<&str>| {
            json!({"name": name, "bot_id": id, "workspace": "/", "status": "idle",
                "created_by": by, "created_by_id": by.map(|_| 1)})
        };
        let mut service = service(
            PathBuf::new(),
            &[
                record("calc.lead", 1, None),
                record("calc.fix", 2, Some("calc.lead")),
                record("shop.lead", 3, None),
                record("shop.cart", 4, Some("shop.lead")),
            ],
        );
        for (lead, project) in [("calc.lead", "calc"), ("shop.lead", "shop")] {
            let tasks = vec![subs::Sub::lead(lead, project)];
            service.subscribers.insert(lead.into(), tasks);
        }
        for record in service.records.values().cloned().collect::<Vec<_>>() {
            service.place(&record);
        }
        service.index();
        let delta = |bot: &str| json!({"event": "text_delta", "bot": bot, "turn": 1, "data": {}});
        assert_eq!(service.hearing(&delta("calc.fix")), ["calc.lead"]);
        assert_eq!(service.hearing(&delta("shop.lead")), ["shop.lead"]);
        assert!(service.hearing(&delta("stray")).is_empty());
        // Each keeps only the bots it watches, and its own.
        let calc = &service.lookouts["calc.lead"].updates;
        assert!(calc.has("calc.lead") && calc.has("calc.fix") && !calc.has("shop.cart"));
        // A bot made reaches every lookout, and each checks that bot alone.
        let made = json!({"event": "created", "bot": "calc.docs",
            "data": {"bot_id": 5, "created_by": "calc.lead", "created_by_id": 1}});
        let mut all = service.hearing(&made);
        all.sort();
        assert_eq!(all, ["calc.lead", "shop.lead"]);
        let tell = |service: &mut Service, event: &Value| {
            for name in service.hearing(event) {
                service.lookouts.get_mut(&name).unwrap().event(event);
            }
            service.index_bot(event["bot"].as_str().unwrap());
        };
        tell(&mut service, &made);
        assert_eq!(service.hearing(&delta("calc.docs")), ["calc.lead"]);
        assert!(service.lookouts["calc.lead"].updates.has("calc.docs"));
        assert!(!service.lookouts["shop.lead"].updates.has("calc.docs"));
        // Deleted, it is heard by no one, and seated nowhere.
        tell(
            &mut service,
            &json!({"event": "deleted", "bot": "calc.docs", "data": {}}),
        );
        assert!(service.hearing(&delta("calc.docs")).is_empty());
        assert!(!service.lookouts["calc.lead"].updates.has("calc.docs"));
    }

    #[tokio::test]
    async fn a_note_goes_beside_the_stream_and_one_lost_with_the_connection_is_news_again() {
        let (client, mut got) = submits().await;
        let mut lookout = seated("idle");
        let ended = |lookout: &mut Lookout, turn: i64| {
            let from = json!({"from": {"bot": "calc.lead"}});
            lookout.event(&ev("calc.fix", turn, "accepted", from));
            let finished = json!({"status": "completed"});
            lookout.event(&ev("calc.fix", turn, "turn_finished", finished));
            lookout.since = Some(Instant::now() - SETTLE);
        };
        ended(&mut lookout, 1);
        let (woken, _woke) = tokio::sync::mpsc::channel(4);
        lookout.wake(&client, &woken);
        assert!(lookout.sending.is_some() && !lookout.updates.news());
        // Until the daemon answers, its news holds the cursor back.
        assert!(lookout.untold());
        // One note at a time: news meanwhile waits for the daemon's answer.
        ended(&mut lookout, 2);
        lookout.wake(&client, &woken);
        let prompt = got.recv().await.unwrap()["prompt"].clone();
        assert!(
            prompt.as_str().unwrap().contains("turn:calc.fix/1"),
            "{prompt}"
        );
        tokio::time::sleep(Duration::from_millis(50)).await;
        assert!(got.try_recv().is_err());
        // The connection dropped before the answer came: what the note
        // carried is news again, with what came since.
        lookout.reconnected();
        assert!(lookout.sending.is_none());
        let told: Vec<String> = lookout
            .updates
            .take()
            .items
            .into_iter()
            .map(|l| l.text)
            .collect();
        assert!(
            told.iter().any(|line| line.contains("turn:calc.fix/1")),
            "{told:?}"
        );
        assert!(
            told.iter().any(|line| line.contains("turn:calc.fix/2")),
            "{told:?}"
        );
    }

    #[test]
    fn the_stream_starts_again_before_the_oldest_turn_still_under_way() {
        let dir = std::env::temp_dir().join(format!("lookout-marks-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let store = dir.join("state.sqlite");
        let mut service = service(store.clone(), &[]);
        service.cursor = Some(90);
        service.in_flight.insert(("calc.fix".into(), 7), 40);
        service.in_flight.insert(("calc.docs".into(), 9), 60);
        service.save_cursor();
        assert_eq!(marks(&store), (Some(90), Some(39)));
        // Nothing under way: it starts again where its news ended.
        service.in_flight.clear();
        service.save_cursor();
        assert_eq!(marks(&store), (Some(90), Some(90)));
        std::fs::write(cursor_path(&store), "12").unwrap();
        assert_eq!(marks(&store), (Some(12), Some(12)));
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn a_status_answer_older_than_the_subscribers_own_events_is_let_go() {
        let mut lookout = seated("running");
        let idle =
            json!({"name": "calc.lead", "bot_id": 1, "running_turn": null, "status": "idle"});
        lookout.asking = Some(lookout.own);
        // Its next turn starts before the answer comes back.
        lookout.event(&ev("calc.lead", 9, "accepted", json!({})));
        lookout.answered(Answer::Listed(Some(idle.clone())));
        assert!(lookout.updates.busy() && lookout.asking.is_none());
        // Nothing of its own since the question: the answer stands.
        lookout.asking = Some(lookout.own);
        lookout.answered(Answer::Listed(Some(idle)));
        assert!(lookout.updates.idle());
    }

    #[test]
    fn a_lookout_runs_while_one_holds_its_lock_whatever_pid_it_names() {
        let dir = std::env::temp_dir().join(format!("lookout-running-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let store = dir.join("state.sqlite");
        assert!(!running(&store));
        // A lookout gone before a restart, its pid now another process's.
        std::fs::write(dir.join("state.sqlite.lookout.lock"), "1").unwrap();
        assert!(!running(&store));
        let held = lock(&store).unwrap().unwrap();
        assert!(running(&store));
        // Asking leaves it held.
        assert!(lock(&store).unwrap().is_none());
        drop(held);
        // A process another test starts at that instant may share the lock
        // until it runs its command, which closes it.
        let let_go = (0..100).any(|_| {
            let gone = !running(&store);
            if !gone {
                std::thread::sleep(Duration::from_millis(10));
            }
            gone
        });
        assert!(let_go);
        let _ = std::fs::remove_dir_all(&dir);
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

    #[test]
    fn another_lead_and_its_tasks_are_heard_only_when_asked_for() {
        let mut lookout = lookout();
        lookout.subs = vec![subs::Sub::lead("shop.lead", "shop")];
        lookout.lead = "shop.lead".into();
        lookout.lineage_of(&json!({"event": "created", "bot": "shop.lead.billing", "data": {}}));
        lookout.lineage_of(&json!({"event": "created", "bot": "shop.invoices",
            "data": {"created_by": "shop.lead.billing"}}));
        lookout.lineage_of(&json!({"event": "created", "bot": "shop.stray", "data": {}}));
        // The project's names, as the first lead hears them: not the other
        // lead's, nor its task's.
        assert!(lookout.watches("shop.stray"));
        assert!(!lookout.watches("shop.lead.billing"));
        assert!(!lookout.watches("shop.invoices"));
        // A subscription that names it is heard.
        lookout
            .subs
            .push(sub("billing", &[], &["shop.lead.billing"]));
        lookout.roots(&HashMap::from([
            ("shop.lead.billing".to_owned(), json!({})),
            (
                "shop.invoices".to_owned(),
                json!({"created_by": "shop.lead.billing"}),
            ),
            ("shop.stray".to_owned(), json!({})),
        ]));
        assert!(lookout.watches("shop.lead.billing"));
        assert!(!lookout.watches("shop.invoices"));
    }

    #[test]
    fn a_gone_leads_tasks_stay_off_the_first_lead() {
        let mut lookout = lookout();
        lookout.subs = vec![subs::Sub::lead("shop.lead", "shop")];
        lookout.lead = "shop.lead".into();
        // shop.lead.billing was deleted; its task and the task's own task
        // live on.
        lookout.roots(&HashMap::from([
            (
                "shop.invoices".to_owned(),
                json!({"created_by": "shop.lead.billing"}),
            ),
            (
                "shop.invoices-pdf".to_owned(),
                json!({"created_by": "shop.invoices"}),
            ),
            ("shop.stray".to_owned(), json!({})),
        ]));
        assert!(lookout.watches("shop.stray"));
        assert!(!lookout.watches("shop.invoices"));
        assert!(!lookout.watches("shop.invoices-pdf"));
        // One the gone lead's name makes later is theirs too.
        lookout.lineage_of(&json!({"event": "created", "bot": "shop.receipts",
            "data": {"created_by": "shop.lead.billing"}}));
        assert!(!lookout.watches("shop.receipts"));
    }

    /// The lead as `bots` lists it mid-turn.
    fn busy(lookout: &mut Lookout) {
        (lookout.updates).seat(&json!({"name": "calc.lead", "bot_id": 1, "status": "running"}));
    }

    fn slack(text: &str, urgent: bool) -> sources::Event {
        sources::Event {
            source: "slack".into(),
            id: None,
            text: text.into(),
            from: Some("ana".into()),
            url: None,
            urgent,
            at_ms: 1,
            seq: 0,
        }
    }

    fn hears(name: &str) -> subs::Sub {
        let mut sub = sub(name, &[], &[]);
        sub.sources = vec!["slack".into()];
        sub
    }

    #[test]
    fn a_source_is_told_to_the_subscriptions_that_hear_it() {
        let mut lookout = lookout();
        // A source no subscription names is no one's news.
        lookout.heard_from(&slack("hi", false));
        assert!(lookout.heard.is_empty() && lookout.heard_since.is_none());
        // Every event is told, with each subscription it came through.
        lookout.subs.push(hears("chat"));
        lookout.subs.push(hears("eng"));
        lookout.heard_from(&slack("lunch?", false));
        assert_eq!(lookout.heard.len(), 1);
        assert_eq!(lookout.heard[0].subs, ["chat", "eng"]);
        assert!(lookout.heard_since.is_some());
        // A subscriber that never rests keeps only the newest.
        for n in 0..HEARD + 5 {
            lookout.heard_from(&slack(&n.to_string(), false));
        }
        assert_eq!(lookout.heard.len(), HEARD);
        assert_eq!(lookout.heard[HEARD - 1].event.text, (HEARD + 4).to_string());
        // Events still waiting follow the subscriptions as they change.
        lookout.subs.retain(|sub| sub.name == "eng");
        lookout.rehear();
        assert_eq!(lookout.heard.len(), HEARD);
        assert!(lookout.heard.iter().all(|heard| heard.subs == ["eng"]));
        lookout.subs.clear();
        lookout.rehear();
        assert!(lookout.heard.is_empty() && lookout.heard_since.is_none());
    }

    /// A daemon that greets, answers each request with `{}` and sends each
    /// `submit` it was asked for.
    async fn submits() -> (Arc<Client>, tokio::sync::mpsc::UnboundedReceiver<Value>) {
        use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
        static STARTED: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
        let dir = std::env::temp_dir().join(format!(
            "lookout-daemon-{}-{}",
            std::process::id(),
            STARTED.fetch_add(1, std::sync::atomic::Ordering::SeqCst)
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let socket = dir.join("agent.sock");
        let listener = tokio::net::UnixListener::bind(&socket).unwrap();
        let (sent, got) = tokio::sync::mpsc::unbounded_channel();
        tokio::spawn(async move {
            let (stream, _) = listener.accept().await.unwrap();
            let (read, mut write) = stream.into_split();
            let ready = json!({"event": "ready", "protocol": agent_client::PROTOCOL});
            write
                .write_all(format!("{ready}\n").as_bytes())
                .await
                .unwrap();
            let mut lines = BufReader::new(read).lines();
            while let Ok(Some(line)) = lines.next_line().await {
                let asked: Value = serde_json::from_str(&line).unwrap();
                if asked["op"] == "submit" {
                    let _ = sent.send(asked.clone());
                }
                let answer = json!({"id": asked["id"], "result": {}});
                write
                    .write_all(format!("{answer}\n").as_bytes())
                    .await
                    .unwrap();
            }
        });
        (Client::connect(&socket).await.unwrap().0, got)
    }

    /// `wake_sources`, and what the daemon answered it, if it sent.
    async fn wake_sources(lookout: &mut Lookout, client: &Arc<Client>) {
        let (woken, mut woke) = tokio::sync::mpsc::channel(4);
        lookout.wake_sources(client, &woken);
        drop(woken);
        while let Some((_, _, answer)) = woke.recv().await {
            lookout.answered(answer);
        }
    }

    #[tokio::test]
    async fn sources_are_told_with_an_origin_once_the_subscriber_rests() {
        let (client, mut got) = submits().await;
        let mut lookout = lookout();
        lookout.updates.live = true;
        lookout.subs.push(hears("chat"));
        busy(&mut lookout);
        lookout.heard_from(&slack("lunch?", false));
        lookout.heard_since = Some(Instant::now() - SETTLE);
        // Busy: it waits.
        wake_sources(&mut lookout, &client).await;
        assert_eq!(lookout.heard.len(), 1);
        // At rest: one note, with the lookout's origin.
        (lookout.updates).seat(&json!({"name": "calc.lead", "bot_id": 1, "status": "idle"}));
        wake_sources(&mut lookout, &client).await;
        let submit = got.recv().await.unwrap();
        assert_eq!(submit["origin"], sources::ORIGIN);
        assert_eq!(submit["delivery"], "queue");
        assert!(
            submit["request_id"]
                .as_str()
                .unwrap()
                .starts_with("lookout-")
        );
        let prompt = submit["prompt"].as_str().unwrap();
        assert!(prompt.starts_with(sources::FROM_SOURCES), "{prompt}");
        assert!(
            prompt.contains(r#"- slack, from "ana" (for your chat): "lunch?""#),
            "{prompt}"
        );
        assert!(lookout.heard.is_empty() && lookout.heard_since.is_none());
        // What its source marked urgent steers a busy subscriber at once.
        busy(&mut lookout);
        lookout.heard_from(&slack("prod is down", true));
        wake_sources(&mut lookout, &client).await;
        let submit = got.recv().await.unwrap();
        assert_eq!(submit["delivery"], "steer");
        let prompt = submit["prompt"].as_str().unwrap();
        assert!(
            prompt.contains(r#"(for your chat, urgent): "prod is down""#),
            "{prompt}"
        );
        assert!(prompt.contains("Handle what cannot wait"), "{prompt}");
        // A note stays under its bytes however the text escapes; the rest
        // waits for the next.
        let escaped = "\u{1}".repeat(sources::TEXT_CHARS);
        for _ in 0..NOTE_ITEMS {
            lookout.heard_from(&slack(&escaped, true));
        }
        wake_sources(&mut lookout, &client).await;
        let prompt = got.recv().await.unwrap()["prompt"]
            .as_str()
            .unwrap()
            .to_owned();
        assert!(
            prompt.len() <= sources::NOTE_BYTES + 1024,
            "{}",
            prompt.len()
        );
        let told = prompt.matches("- slack").count();
        assert!(
            told > 0 && lookout.heard.len() == NOTE_ITEMS - told,
            "{told}"
        );
    }
}
