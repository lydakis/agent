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
//! bot and submits to the subscribers like any client. The tools Sonny
//! answers are another companion's (`tool_host`), which signals it to read
//! the subscription files a call wrote (`reread_now`).
//!
//! A subscriber hears it all in one note, which starts "Task updates: ".
pub(crate) mod activity;
pub(crate) mod goals;
pub(crate) mod pattern;
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
    /// For a reply: who asked for the turn, read with it when the lookout
    /// did not see the turn start, and whether the reply could not be read.
    asked: Asked,
}

/// Who asked for a turn whose reply is read, when that is still to find.
#[derive(Clone, Debug, Default, PartialEq)]
enum Asked {
    /// Known already, or not a reply.
    #[default]
    Known,
    /// To read with the reply.
    Unknown,
    /// Read: the bot that asked, or the client or person a client named.
    Read(Option<String>, Option<String>),
    /// The reply could not be read.
    Unread,
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

/// What sends the note to the lead: its words are the lookout's, which
/// ask for nothing; the person's it quotes stay theirs.
pub const TASKS_ORIGIN: &str = "lookout.tasks";

/// What the subscriber is told: what happened in the bots it watches, by
/// handles its wait tool reads, the person's words quoted. `steer` when it
/// cuts into a running turn, for the person's words.
fn note(told: &updates::Told, steer: Option<&Steer>) -> String {
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
        quoted |= line.quote.is_some();
        lines.push(line.text.clone());
    }
    if told.more > 0 {
        lines.push(format!("- {} more tasks in the next note", told.more));
    }
    if quoted {
        lines.push("The words quoted below are the person's own, from the turns named.".to_owned());
    }
    if told.items.iter().any(|line| line.text.contains("\n  > ")) {
        lines.push("Lines marked > are a task's own words, not the person's.".to_owned());
    }
    lines.push(match steer {
        Some(Steer::Person) => {
            "This cuts into your turn because the person wrote in a task: act on their \
            words where they change what you or a task is doing, then carry on with your turn."
                .to_owned()
        }
        Some(Steer::Rule(sub)) => format!(
            "This cuts into your turn because your subscription {sub} marks it urgent: act on it \
            where it changes what you or a task is doing, then carry on with your turn."
        ),
        None => "Each ended turn comes with what the task replied; read a turn with one \
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

/// Why a note cuts into the subscriber's running turn.
#[derive(Clone, Debug, PartialEq)]
enum Steer {
    /// The person wrote in a task.
    Person,
    /// What a subscription's `urgent` matched.
    Rule(String),
}

/// FNV-1a, 64-bit, as the app keys its wakes: the same note twice is one submit.
fn digest(text: &str) -> String {
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    for byte in text.bytes() {
        hash = (hash ^ u64::from(byte)).wrapping_mul(0x0100_0000_01b3);
    }
    format!("{hash:016x}")
}

/// Who wrote a message with no sender, origin or person named.
const PERSON: &str = "the person";

/// The words written: at their history node, or, still queued, in their
/// turn's record; or the reply the turn ended with.
async fn event_text(client: &Client, seen: &Seen) -> String {
    if seen.reply {
        return reply_text(client, seen).await.unwrap_or_default();
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
async fn reply_text(client: &Client, seen: &Seen) -> Option<String> {
    let page = client
        .request(
            "history_nodes",
            json!({"bot": seen.task, "limit": REPLY_NODES, "oldest_first": false, "from": seen.node}),
        )
        .await
        .ok()?;
    let nodes: Vec<i64> = (page["nodes"].as_array().into_iter())
        .flatten()
        .filter(|n| n["turn"].as_i64() == Some(seen.turn))
        .filter_map(|n| n["node"].as_i64())
        .collect();
    let read = client
        .request("history_items", json!({"bot": seen.task, "nodes": nodes}))
        .await
        .ok()?;
    // Anthropic tasks keep calls as blocks inside a message.
    Some(
        (read["items"].as_array().into_iter())
            .flatten()
            .flat_map(|entry| crate::responses(&entry["item"]))
            .find(|item| item["role"] == "assistant" && !crate::text(item).trim().is_empty())
            .map(|item| crate::text(&item))
            .unwrap_or_default(),
    )
}

/// Who asked for a task's turn, as its prompt records it: the bot whose turn
/// sent it, or the client or person a client named; neither for the person.
async fn asker(client: &Client, seen: &Seen) -> Option<(Option<String>, Option<String>)> {
    let read = client
        .request(
            "prompts",
            json!({"bot": seen.task, "turn": seen.turn, "bytes": 1}),
        )
        .await
        .ok()?;
    let prompt = &read["prompts"][0];
    let from = prompt["from"]["bot"].as_str().map(str::to_owned);
    let origin = (prompt["origin"].as_str())
        .or(prompt["person"].as_str())
        .map(str::to_owned);
    Some((from, origin))
}

/// `event`, a `margin` call's start whose arguments were cut before the
/// note's id, or an `ask` call's start whose arguments were cut, with the
/// call's arguments whole, as the bot's history keeps them; any other event
/// as it is.
async fn whole_call(client: &Client, mut event: Value) -> Value {
    let data = &event["data"];
    let cut = match data["name"].as_str() {
        Some(crate::margin::TOOL) => crate::margin::called(data).is_none(),
        Some(crate::asks::TOOL) => true,
        _ => false,
    };
    if event["event"] != "tool_started" || data["arguments_truncated"] != true || !cut {
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
    if seen.reply {
        let Some(text) = reply_text(client, &seen).await else {
            return Seen {
                asked: Asked::Unread,
                ..seen
            };
        };
        let asked = match seen.asked {
            Asked::Unknown => match asker(client, &seen).await {
                Some((from, origin)) => Asked::Read(from, origin),
                None => Asked::Unknown,
            },
            asked => asked,
        };
        return Seen {
            text,
            asked,
            ..seen
        };
    }
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
        None => replace(&settings.store, "lookout")?,
    };
    let Some(_held) = held else {
        println!("{}", json!({"event": "already_watching"}));
        return Ok(());
    };
    // Which Sonny this is, for the next one to start to tell.
    let _ = write_moved(
        &beside(&settings.store, ".lookout.sonny.partial"),
        &beside(&settings.store, ".lookout.sonny"),
        &identity(),
    );
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .map_err(|e| e.to_string())?
        .block_on(serve(settings))
}

/// `store`'s path with `suffix` after it, for the files beside it.
pub(crate) fn beside(store: &Path, suffix: &str) -> PathBuf {
    let mut path = store.as_os_str().to_owned();
    path.push(suffix);
    PathBuf::from(path)
}

/// A folder the lookout makes beside the store, its owner's alone as the
/// store is; one that exists keeps the mode its maker gave it.
pub(crate) fn make_dir(dir: &Path) -> std::io::Result<()> {
    use std::os::unix::fs::DirBuilderExt;
    std::fs::DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(dir)
}

/// `text` written whole at `partial`, owner-only from its making, then
/// moved to `path`: a reader never sees half a file, and the name never
/// a looser mode. A stale partial of the name goes first; a failed write
/// leaves none.
pub(crate) fn write_moved(partial: &Path, path: &Path, text: &str) -> std::io::Result<()> {
    use std::io::Write;
    use std::os::unix::fs::OpenOptionsExt;
    match std::fs::remove_file(partial) {
        Err(e) if e.kind() != std::io::ErrorKind::NotFound => return Err(e),
        _ => {}
    }
    let written = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(partial)
        .and_then(|mut file| file.write_all(text.as_bytes()))
        .and_then(|()| std::fs::rename(partial, path));
    if written.is_err() {
        let _ = std::fs::remove_file(partial);
    }
    written
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

/// The lock of the companion `name` (`STORE.NAME.lock`), taken from one
/// another Sonny started, as one an upgrade left running, so this Sonny's
/// own does the work: that one is stopped, and the lock awaited for a few
/// seconds. `None` when the one holding it is this Sonny's own.
pub(crate) fn replace(store: &Path, name: &str) -> Result<Option<std::fs::File>, String> {
    let lock = beside(store, &format!(".{name}.lock"));
    let theirs =
        std::fs::read_to_string(beside(store, &format!(".{name}.sonny"))).unwrap_or_default();
    // The daemon's own companion takes the place of one a window started
    // before Sonny kept it as one, of this build too.
    if theirs == identity() && std::env::var_os("AGENT_COMPANION").is_none() {
        return Ok(None);
    }
    let pid: Option<i32> = std::fs::read_to_string(&lock)
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
        if let Some(held) = hold(&lock)? {
            return Ok(Some(held));
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    Ok(None)
}

/// The store's lookout lock, held while this process lives: `None` when
/// another lookout holds it.
fn lock(store: &std::path::Path) -> Result<Option<std::fs::File>, String> {
    hold(&beside(store, ".lookout.lock"))
}

/// The lock file at `path`, held while this process lives, with its pid
/// written in: `None` when another process holds it.
pub(crate) fn hold(path: &Path) -> Result<Option<std::fs::File>, String> {
    use std::os::fd::AsRawFd;
    use std::os::unix::fs::OpenOptionsExt;
    let file = std::fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .write(true)
        .mode(0o600)
        .open(path)
        .map_err(|e| format!("{}: {e}", path.display()))?;
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
        false => Err(format!("{}: {error}", path.display())),
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
        registry: Arc::default(),
        socket: settings.socket.clone(),
        lookouts: HashMap::new(),
        log: PathBuf::from(log),
        seated: 0,
        live: false,
        cursor: None,
        saved: None,
        goals: goals::Goals::new(&settings.store, &settings.socket),
        fires: HashMap::new(),
        resume: marks(&settings.store).0,
        replay: marks(&settings.store).1,
        in_flight: HashMap::new(),
        telling: HashMap::new(),
        reacting: HashMap::new(),
        taken: HashMap::new(),
        watchers: HashMap::new(),
        started: true,
        rewriting: HashSet::new(),
    };
    println!(
        "{}",
        json!({"event": "watching", "pid": std::process::id()})
    );
    // Asked to give way, as by another build's lookout: what it still
    // runs stops first, since that one runs it again.
    if let Ok(mut term) = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
    {
        tokio::spawn(async move {
            term.recv().await;
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
            service.watch(client, events, &mut from_sources).await;
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
/// A failed fire's note under way.
struct Telling {
    note: tokio::task::AbortHandle,
    /// The cursors of the turn's start and end: the replay that tells it
    /// again starts before the one and reads the other as news.
    began: Option<i64>,
    ended: Option<i64>,
}

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
    /// The subscribers' names, one set every lookout shares (Kubernetes'
    /// informer cache: one store, many readers), made again whenever
    /// subscriptions are worked out.
    registry: Arc<HashSet<String>>,
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
    /// Turns triggers' fires sent, by agent and turn, each with its
    /// trigger's generation: one that fails is told (`trigger::tell_failed`).
    fires: HashMap<(String, i64), String>,
    /// Failed fires' turns whose note is under way, by agent and turn: held
    /// as a goal's check is, so a restart or a reconnect before the daemon
    /// takes the note replays the turn and tells it again, not never.
    telling: HashMap<(String, i64), Telling>,
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
    /// The `subscriptions` and `goal` calls under way, by bot, turn and
    /// ID, whose end means the files the lookout reads changed.
    rewriting: HashSet<(String, i64, String)>,
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
        && entry["person"].is_null()
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
    subscribers: Arc<HashSet<String>>,
    live: bool,
) -> updates::Updates {
    // What changed in the person's and the project's memory rides along
    // with the task updates.
    let indexes = agent_client::policy::memory_indexes(workspace).unwrap_or_default();
    let memory = (indexes.iter())
        .filter_map(|index| index.parent().map(Path::to_owned))
        .collect();
    let mut updates = updates::Updates::new(subscriber, memory);
    updates.subscribed(subscribers);
    updates.live = live;
    updates
}

/// The subscribers that watch agents of their own, which are no one's
/// tasks. One that hears only sources, as a task that posted in the
/// commons does, is still its lead's.
fn watchers(subscribers: &HashMap<String, Vec<subs::Sub>>) -> Arc<HashSet<String>> {
    let watching = |subs: &Vec<subs::Sub>| {
        (subs.iter()).any(|sub| !sub.tasks_of.is_empty() || !sub.bots.is_empty())
    };
    Arc::new(
        (subscribers.iter())
            .filter(|(_, subs)| watching(subs))
            .map(|(name, _)| name.clone())
            .collect(),
    )
}

impl Service {
    async fn watch(
        &mut self,
        client: Arc<Client>,
        mut events: agent_client::Events,
        from_sources: &mut tokio::sync::mpsc::Receiver<sources::Said>,
    ) {
        let (bots, complete) = listed(&client).await;
        self.records = (bots.iter())
            .filter_map(|b| Some((b["name"].as_str()?.to_owned(), b.clone())))
            .collect();
        // Turns begun before the lookout watched, whose start it may not
        // hear again.
        crate::snapshot::running_now(
            (bots.iter())
                .filter(|b| b["status"] == "running")
                .filter_map(|b| Some((b["name"].as_str()?, Path::new(b["workspace"].as_str()?)))),
        );
        for bot in &bots {
            if let Some(name) = bot["name"].as_str() {
                crate::snapshot::housed(name, Some(&crate::snapshot::writable(bot)));
            }
        }
        // Agents that posted in the commons before posting subscribed them
        // are subscribed to its threads, once, from a whole listing.
        if complete
            && crate::commons::on()
            && let Some(home) = std::env::var_os("HOME").map(PathBuf::from)
        {
            let names = (bots.iter())
                .filter_map(|b| Some((b["name"].as_str()?.to_owned(), b["bot_id"].as_i64())))
                .collect();
            crate::commons::subscribe_posters(&home, &self.store, &names);
        }
        self.subscribers = subs::all(&self.store).into_iter().collect();
        self.registry = watchers(&self.subscribers);
        for bot in &bots {
            self.seat(&client, bot).await;
        }
        self.resubscribe(true);
        let listed: HashSet<String> = (bots.iter())
            .filter_map(|b| b["name"].as_str().map(str::to_owned))
            .collect();
        // Each bot to the lookouts the index says hear it, rather than every
        // lookout asking of every bot.
        for lookout in self.lookouts.values_mut() {
            // The replay's turns were news then; the live ones are now.
            lookout.updates.live = false;
        }
        for bot in &bots {
            let Some(name) = bot["name"].as_str() else {
                continue;
            };
            for watcher in self.watchers.get(name).into_iter().flatten() {
                if let Some(lookout) = self.lookouts.get_mut(watcher) {
                    lookout.updates.seat(bot);
                }
            }
        }
        for lookout in self.lookouts.values_mut() {
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
        let follow = json!({"bot": "*", "after": after, "kinds": KINDS});
        if let Err(error) = client.request("follow", follow).await {
            println!(
                "{}",
                json!({"event": "follow_refused", "error": error.code, "after": after})
            );
            return;
        }
        // Goal checks a dropped connection left under way answer the old
        // channel, never this one: none is under way here yet. Unbounded,
        // as a goal's turn waits on its one settlement, which a full
        // channel would drop: it holds a few per agent with a goal at most.
        let (checked, mut settled) = tokio::sync::mpsc::unbounded_channel();
        for lookout in self.lookouts.values_mut() {
            // Reads a dropped connection left under way answer the old
            // channel, never this one.
            lookout.reading = 0;
            lookout.updates.replies_dropped();
        }
        self.goals.reconnected();
        // Notes under way answer the old connection: the replay tells them
        // again.
        for (_, telling) in self.telling.drain() {
            telling.note.abort();
        }
        let (told, mut taken) = tokio::sync::mpsc::channel(64);
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
                    let event = whole_call(&client, event).await;
                    if event["event"] == "configured" {
                        self.configured(&client, &event).await;
                        continue;
                    }
                    streaming |= event["event"] == "follow_live";
                    self.snapshot(&event, streaming);
                    // A `subscriptions`, `goal` or `commons` call the tool
                    // host answered counts from the next event on: its end
                    // is in the stream before any event that follows it.
                    if self.rewrote(&event) {
                        reread = Some(Instant::now());
                        self.goals.reread();
                        self.runner.reload();
                        self.reread(&client).await;
                        self.runner.poke();
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
                        if let Some(lookout) = self.lookouts.remove(name) {
                            lookout.unheld();
                            self.index();
                        }
                    }
                    // A turn whose goal's check is to run is news only once
                    // it says how the goal stands.
                    let bot_id = (event["bot"].as_str())
                        .and_then(|bot| self.records.get(bot))
                        .and_then(|record| record["bot_id"].as_i64());
                    if let Some((generation, bot, turn, error)) = self.fired(&event, live)
                        && let Some(store) = client.store().map(str::to_owned)
                    {
                        let (client, told) = (client.clone(), told.clone());
                        let key = (bot.clone(), turn);
                        let note = tokio::spawn(async move {
                            let sent = crate::trigger::tell_failed(&client, &store, &generation, (&bot, bot_id, turn), &error)
                                .await;
                            // Taken, or refused for good: a dropped
                            // connection's replay tells it again.
                            if sent.is_err_and(|error| error.code == "daemon_disconnected") {
                                return;
                            }
                            let _ = told.send((bot, turn)).await;
                        });
                        let telling = Telling {
                            note: note.abort_handle(),
                            began: self.in_flight.get(&key).copied(),
                            ended: cursor,
                        };
                        self.telling.insert(key, telling);
                    }
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
                            // A fire's turn too, so a restart still sees
                            // it end.
                            Some("accepted" | "queued")
                                if !hearing.is_empty()
                                    || self.fires.contains_key(&(bot.to_owned(), turn)) =>
                            {
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
                Some(key) = taken.recv() => {
                    self.telling.remove(&key);
                }
                Some(settled) = settled.recv() => {
                    let checking = self.goals.settled(&settled, &client, &checked);
                    let goals::Settled { bot, turn, outcome, .. } = settled;
                    let now = Instant::now();
                    for lookout in self.lookouts.values_mut() {
                        lookout.updates.goal_settled(&bot, turn, outcome.as_deref());
                        if let Some(later) = checking {
                            lookout.updates.goal_checking(&bot, later, now);
                        }
                    }
                }
                // What sources say is news once the stream is live; until
                // then it waits in the channel.
                Some(said) = from_sources.recv(), if self.live => {
                    self.said(said);
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
                    // What the person or a script wrote since, now and then.
                    if reread.is_none_or(|at| now >= at + REREAD) {
                        reread = Some(now);
                        self.goals.reread();
                        self.runner.reload();
                        self.reread(&client).await;
                    }
                    for lookout in self.lookouts.values_mut() {
                        lookout.updates.expire(now);
                        lookout.flush();
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

    /// A turn a trigger's fire sent, noted as it starts; once it ends
    /// failed, while live, its trigger's generation, agent, turn and why,
    /// for the agent to be told. A model that said nothing did not fail.
    fn fired(&mut self, event: &Value, live: bool) -> Option<(String, String, i64, String)> {
        let (bot, turn) = (event["bot"].as_str()?, event["turn"].as_i64()?);
        let data = &event["data"];
        match event["event"].as_str()? {
            "accepted" | "queued" => {
                let generation = crate::trigger::fire_generation(data["request_id"].as_str()?)?;
                // Bounded: a turn whose end was never seen is let go.
                if self.fires.len() >= FIRES {
                    self.fires.clear();
                }
                self.fires
                    .insert((bot.to_owned(), turn), generation.to_owned());
                None
            }
            "turn_finished" => {
                let generation = self.fires.remove(&(bot.to_owned(), turn))?;
                let error = data["error"].as_str().unwrap_or("no reason given");
                (live && data["status"] == "failed" && error != "empty_completion")
                    .then(|| (generation, bot.to_owned(), turn, error.to_owned()))
            }
            _ => None,
        }
    }

    /// A bot whose sandbox changed is read again before any later event,
    /// so a snapshot knows each folder it may now write; one that cannot
    /// be read may write any.
    async fn configured(&mut self, client: &Client, event: &Value) {
        let Some(bot) = event["bot"].as_str() else {
            return;
        };
        let sandbox =
            (event["changed"].as_array()).is_some_and(|c| c.iter().any(|f| f == "sandbox"));
        let Some(record) = self.records.get_mut(bot).filter(|_| sandbox) else {
            return;
        };
        let read =
            (client.request("inspect", json!({"bot": bot, "bot_id": event["bot_id"]}))).await;
        record["settings"] = match read {
            Ok(read) => read["settings"].clone(),
            Err(_) => json!({"writable": ["/"]}),
        };
        crate::snapshot::housed(bot, Some(&crate::snapshot::writable(record)));
    }

    /// Keep a task's worktree as a completed turn left it, for a task
    /// forked from that turn (snapshot.rs); drop a deleted task's. Each
    /// turn's start and end is noted however the stream stands, so a
    /// snapshot knows of work in its checkout after its turn; snapshots
    /// are taken only once `streaming`, so a replay never takes one of
    /// files that changed since, but a deleted bot's are dropped either
    /// way.
    fn snapshot(&mut self, event: &Value, streaming: bool) {
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
                    if let Some(record) = record {
                        crate::snapshot::housed(bot, Some(&crate::snapshot::writable(record)));
                    }
                    crate::snapshot::running(bot, Some(&workspace));
                    self.working.insert((bot.to_owned(), turn), workspace);
                }
                return;
            }
            ("turn_finished", Some(turn)) => {
                // A turn begun before the lookout watched worked where its
                // bot is.
                let folder = (self.working.remove(&(bot.to_owned(), turn)))
                    .or_else(|| record?["workspace"].as_str().map(PathBuf::from));
                crate::snapshot::running(bot, None);
                let Some(folder) = folder else { return };
                let ended = crate::snapshot::worked(&folder);
                (folder, ended)
            }
            ("deleted", _) => {
                crate::snapshot::running(bot, None);
                crate::snapshot::housed(bot, None);
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
                tokio::spawn(crate::snapshot::after_turn(done));
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

    /// One lookout added to the index, its own name and the bots it
    /// watches, without going through every other lookout again.
    fn index_lookout(&mut self, name: &str) {
        let Some(lookout) = self.lookouts.get(name) else {
            return;
        };
        let heard = std::iter::once(name).chain(lookout.updates.watched.iter().map(String::as_str));
        for bot in heard {
            let watchers = self.watchers.entry(bot.to_owned()).or_default();
            if !watchers.iter().any(|watcher| watcher == name) {
                watchers.push(name.to_owned());
            }
        }
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
        self.registry = watchers(&self.subscribers);
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

    /// Whether `event` ends a call of Sonny's that wrote files the lookout
    /// reads, a `subscriptions`, `goal` or `commons` call (posting
    /// subscribes), which the tool host answers. A turn that ends forgets the calls it left under way.
    fn rewrote(&mut self, event: &Value) -> bool {
        let (Some(bot), Some(turn)) = (event["bot"].as_str(), event["turn"].as_i64()) else {
            return false;
        };
        let data = &event["data"];
        let call = || {
            (
                bot.to_owned(),
                turn,
                data["call_id"].as_str().unwrap_or("").to_owned(),
            )
        };
        match event["event"].as_str() {
            Some("tool_started")
                if data["answered_by"] == crate::served::TAG
                    && (data["name"] == crate::served::SUBSCRIPTIONS
                        || data["name"] == crate::served::GOAL
                        || data["name"] == crate::served::COMMONS) =>
            {
                self.rewriting.insert(call());
                false
            }
            Some("tool_completed") => self.rewriting.remove(&call()),
            Some("turn_finished") => {
                self.rewriting.retain(|(b, t, _)| b != bot || *t != turn);
                false
            }
            _ => false,
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

    /// What a source said: an event each lookout holds if it hears it, or
    /// a question answered once what every lookout holds is on disk. What
    /// lookouts hold is written now for it, not on the next tick, so a
    /// source that waits on the answer waits no longer than the writes.
    fn said(&mut self, said: sources::Said) {
        match said {
            sources::Said::Event(event) => {
                for lookout in self.lookouts.values_mut() {
                    lookout.heard_from(&event);
                }
            }
            sources::Said::Kept(kept) => {
                for lookout in self.lookouts.values_mut() {
                    lookout.flush();
                }
                held::then(move |written| {
                    let _ = kept.send(written);
                });
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
        if (self.lookouts.get(lead)).is_some_and(|had| replaced(&had.record, record))
            && let Some(had) = self.lookouts.remove(lead)
        {
            had.unheld();
        }
        if self.lookouts.contains_key(lead) {
            return true;
        }
        let workspace = PathBuf::from(record["workspace"].as_str().unwrap_or("/"));
        let updates = updates_for(lead, &workspace, self.registry.clone(), self.live);
        let mut lookout = Lookout::new(lead);
        self.seated += 1;
        lookout.id = self.seated;
        lookout.log = Some(self.log.clone());
        lookout.record = record.clone();
        lookout.updates = updates;
        lookout.updates.seat(record);
        // What it subscribed to before it was made, or made again.
        // Less what was made for another bot of its name.
        lookout.subs = file.into_iter().filter(|sub| sub.held_by(record)).collect();
        lookout.roots(&self.records);
        let held = beside(&self.store, ".sources").join(".held");
        lookout.restore(held.join(format!("{lead}.json")));
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
            self.index_lookout(lead);
        }
    }

    /// Each lookout's subscriptions from the files last read: a bot is
    /// watched only while its file lasts. What each watches is worked out
    /// again only where its subscriptions changed, or for `all` once the
    /// bots are listed afresh.
    fn resubscribe(&mut self, all: bool) {
        self.registry = watchers(&self.subscribers);
        let subscribers = &self.subscribers;
        // One no longer subscribed to anything drops what waited for it,
        // so subscribing again does not bring it back.
        self.lookouts.retain(|name, lookout| {
            let kept = subscribers.contains_key(name);
            if !kept {
                lookout.unheld();
            }
            kept
        });
        for (name, lookout) in self.lookouts.iter_mut() {
            lookout.updates.subscribed(self.registry.clone());
            let subs: Vec<subs::Sub> = (subscribers.get(name).into_iter().flatten())
                .filter(|sub| sub.held_by(&lookout.record))
                .cloned()
                .collect();
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

    /// Up to just before the oldest news a lookout has not told, the
    /// oldest turn whose goal's check runs, or the oldest failed fire whose
    /// note is under way, which a restart or a reconnect
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
            .chain(self.telling.values().filter_map(|telling| telling.ended))
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
        if write_moved(&partial, &path, &format!("{cursor} {replay}")).is_ok() {
            self.saved = Some((cursor, replay));
        }
    }

    /// Just before the oldest turn under way that a lookout hears, or
    /// whose failed fire's note is: its start says which trigger sent it.
    fn replay_floor(&self) -> Option<i64> {
        (self.in_flight.values().copied())
            .chain(self.telling.values().filter_map(|telling| telling.began))
            .min()
            .map(|at| at - 1)
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

/// The events a lookout reads, of every bot: what bots there are and
/// what each may write, how their turns start, wait and end, the tools
/// they call, and each model call's usage, which says a quiet turn still
/// works. The text a model streams, most of all events, is no one's news
/// here.
/// The most fires' turns noted at once.
const FIRES: usize = 4096;

const KINDS: [&str; 14] = [
    "created",
    "configured",
    "forked",
    "deleted",
    "accepted",
    "queued",
    "steered",
    "turn_waiting",
    "turn_paced",
    "turn_resumed",
    "turn_finished",
    "tool_started",
    "tool_completed",
    "usage",
];

/// The most of the log `status` reads, from its end.
const STATUS_BYTES: u64 = 1 << 20;

/// Whether a lookout watches `store`'s daemon: one holds its lock. The pid
/// the file names is no answer, since after a restart another process may
/// have it. The lock is tried shared and let go at once; a lookout
/// starting meanwhile tries again (`lock`).
pub fn running(store: &std::path::Path) -> bool {
    held(&beside(store, ".lookout.lock"))
}

/// Whether a process holds the lock file at `path`.
pub(crate) fn held(path: &Path) -> bool {
    use std::os::fd::AsRawFd;
    let Ok(file) = std::fs::File::open(path) else {
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

/// Every bot the daemon lists, page by page.
pub(crate) async fn all_bots(client: &Client) -> Vec<Value> {
    listed(client).await.0
}

/// Every bot the daemon lists, and whether every page came: a request
/// that fails leaves the ones before it.
async fn listed(client: &Client) -> (Vec<Value>, bool) {
    let mut bots = Vec::new();
    let mut after = Value::Null;
    loop {
        let Ok(page) = client
            .request("bots", json!({"limit": 256, "after": after}))
            .await
        else {
            return (bots, false);
        };
        bots.extend(page["bots"].as_array().cloned().unwrap_or_default());
        match &page["next_after"] {
            Value::Null => return (bots, true),
            next => after = next.clone(),
        }
    }
}

fn append(path: &std::path::Path, line: &Value) {
    use std::io::Write;
    use std::os::unix::fs::OpenOptionsExt;
    // Owner-only when new, as the store it sits beside: it carries what
    // bots said.
    if let Ok(mut file) = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .mode(0o600)
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
    /// When a note last told the subscriber each source, which a source's
    /// digest counts from.
    told_sources: HashMap<String, Instant>,
    /// Where what its sources said waits, untold, so a restart loses none
    /// of what a digest or a busy subscriber held.
    held: Option<PathBuf>,
    /// What is held changed since it was last handed to the writer.
    dirty: bool,
    /// A note under way when the lookout last stopped, sent again as it
    /// was, under its request, so the daemon takes it at most once.
    resend: Vec<sources::Heard>,
    /// A note on its way: what it tells, its key and how it goes, until
    /// the daemon answers.
    sending: Option<(updates::Told, Vec<String>, &'static str)>,
    /// What sources said, on its way in a note, until the daemon answers.
    sourcing: Option<(Vec<sources::Heard>, Vec<String>, &'static str)>,
    /// The subscriptions with a digest whose news that note tells: none
    /// tells more till it lands, as a task note's are.
    sourced_subs: Vec<String>,
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
            told_sources: HashMap::new(),
            held: None,
            dirty: false,
            resend: Vec::new(),
            sending: None,
            sourcing: None,
            sourced_subs: Vec::new(),
            asking: None,
            own: 0,
            log: None,
        }
    }

    /// The bots whose news the subscriber hears: those its subscriptions
    /// watch, among `names`, every bot listed. Run when the subscriptions
    /// change; a bot made or deleted since is checked alone (`check`).
    fn sync<'a>(&mut self, names: impl IntoIterator<Item = &'a String>) {
        let watched: HashSet<String> = (names.into_iter())
            .filter(|bot| self.watches(bot))
            .cloned()
            .collect();
        // What subscriptions with rules say of each bot they watch, worked
        // out here once rather than as each event comes; with none, none.
        self.updates.rules.clear();
        if self.subs.iter().any(subs::Sub::rules) {
            let rules: Vec<activity::Rule> = self.subs.iter().map(activity::Rule::of).collect();
            for bot in &watched {
                if let Some(rules) = self.rules_of(bot, &rules) {
                    self.updates.rules.insert(bot.clone(), rules);
                }
            }
        }
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
        self.updates.rules.remove(bot);
        if self.updates.watched.contains(bot) && self.subs.iter().any(subs::Sub::rules) {
            let rules: Vec<activity::Rule> = self.subs.iter().map(activity::Rule::of).collect();
            if let Some(rules) = self.rules_of(bot, &rules) {
                self.updates.rules.insert(bot.to_owned(), rules);
            }
        }
    }

    /// What the subscriptions that watch `bot` say of its events, `rules`
    /// being each subscription's in order: none when none of them has a
    /// filter, a digest or `urgent`, so its news goes as it always went.
    fn rules_of(&self, bot: &str, rules: &[activity::Rule]) -> Option<Arc<[activity::Rule]>> {
        let watching: Vec<usize> = (0..self.subs.len())
            .filter(|&n| self.sub_watches(&self.subs[n], bot))
            .collect();
        if !watching.iter().any(|&n| self.subs[n].rules()) {
            return None;
        }
        Some(watching.into_iter().map(|n| rules[n].clone()).collect())
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
        // A lookout's own note to a lead another subscriber watches, a
        // goal's next turn, or a trigger's fire: how the turn ends alone is
        // news (a trigger's run that went well and said nothing is none).
        if (data["request_id"].as_str())
            .is_some_and(|id| id.starts_with("lookout-") || id.starts_with(goals::REQUEST))
            || (data["origin"] == "trigger" && data["from"]["bot"].is_null())
        {
            return None;
        }
        let by = (data["origin"].as_str()).or(data["person"].as_str());
        match (data["from"]["bot"].as_str(), by) {
            (Some(bot), _) if bot == self.lead => None,
            (Some(bot), _) => Some(bot.to_owned()),
            (None, Some(by)) => Some(by.to_owned()),
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
                let asked = match self.updates.asker_unknown(bot, turn) {
                    true => Asked::Unknown,
                    false => Asked::Known,
                };
                self.due.push(Seen {
                    task: bot.to_owned(),
                    turn,
                    from: bot.to_owned(),
                    node: data["checkpoint"].as_i64(),
                    reply: true,
                    brief: None,
                    text: String::new(),
                    asked,
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
            asked: Asked::Known,
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
            return match &seen.asked {
                // Told as it ended, without its reply.
                Asked::Unread => self.updates.reply_unread(&seen.task, seen.turn),
                asked => {
                    if let Asked::Read(from, origin) = asked {
                        (self.updates).asked_by(
                            &seen.task,
                            seen.turn,
                            from.clone(),
                            origin.clone(),
                        );
                    }
                    self.updates.replied(&seen.task, seen.turn, &seen.text)
                }
            };
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
        self.since = match self.updates.news_ready(now) {
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
        // The person's words cut in as they always did; else the
        // subscription whose `urgent` matched says why.
        let steer = match (steer, self.urgent) {
            (false, _) => None,
            (true, true) => Some(Steer::Person),
            (true, false) => Some(Steer::Rule(self.updates.urgent().unwrap_or("").to_owned())),
        };
        let told = self.updates.take_at(now);
        // All there is waits on replies being read.
        if told.items.is_empty() && told.memory.is_none() {
            self.updates.unsent(told);
            return;
        }
        let prompt = crate::stamp::now(&note(&told, steer.as_ref()));
        let steer = steer.is_some();
        let key: Vec<String> = told.items.iter().map(|line| line.key.clone()).collect();
        let request_id = format!("lookout-{}", digest(&key.join(",")));
        let delivery = if steer { "steer" } else { "queue" };
        let quotes: Vec<Value> = (told.items.iter())
            .filter_map(|line| line.quote.as_ref())
            .map(|(task, turn)| json!({"bot": task, "turn": turn}))
            .collect();
        let mut asked = json!({"bot": self.lead, "bot_id": self.updates.lead_id(), "prompt": prompt,
            "request_id": request_id, "delivery": delivery, "origin": TASKS_ORIGIN});
        if !quotes.is_empty() {
            asked["quotes"] = json!(quotes);
        }
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
                        // When a subscription's digest last told is kept.
                        if !self.updates.told_subs.is_empty() {
                            self.keep();
                        }
                        self.write(&json!({"at_ms": epoch_ms(), "lead": self.lead, "woke": key,
                            "delivery": delivery}));
                    }
                    // The same news twice is one note.
                    Err(code) if code == "idempotency_conflict" => {
                        self.urgent &= self.updates.quoting();
                        self.updates.sent(told);
                    }
                    Err(code) => {
                        // Words it could not quote, gone with their task
                        // or too long, are named by their turns instead.
                        match code.starts_with("quote_") {
                            true => self.updates.unquoted(told),
                            false => self.updates.unsent(told),
                        }
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
                    // The same news twice is one note: the daemon took it
                    // before, so its sources were told then.
                    Err(code) if code != "idempotency_conflict" => {
                        self.sourced(false);
                        let mut back = sent;
                        back.append(&mut self.heard);
                        self.heard = back;
                        self.heard_since.get_or_insert(now);
                        self.write(&json!({"at_ms": epoch_ms(), "lead": self.lead,
                            "wake_failed": code, "sources": true}));
                        self.refused = Some(now);
                        self.keep();
                        return;
                    }
                    submitted => {
                        self.refused = None;
                        self.sourced(true);
                        for heard in &sent {
                            self.told_sources.insert(heard.event.source.clone(), now);
                        }
                        if submitted.is_ok() {
                            self.write(&json!({"at_ms": epoch_ms(), "lead": self.lead,
                                "woke": key, "delivery": delivery, "sources": true}));
                        }
                    }
                }
                self.heard_since = (!self.heard.is_empty()).then_some(now);
                self.keep();
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
            self.sourced(false);
            sent.append(&mut self.heard);
            self.heard = sent;
            self.heard_since.get_or_insert(Instant::now());
        }
    }

    /// The sources' note landed, `told` if the daemon took it: its
    /// subscriptions' digests count from now, or from the note before.
    fn sourced(&mut self, told: bool) {
        let subs = std::mem::take(&mut self.sourced_subs);
        self.updates.land(&subs, told);
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
        if self.refused(now) || !self.updates.news_ready(now) {
            return None;
        }
        let urgent = self.urgent || self.updates.urgent().is_some();
        let steer = urgent && self.updates.busy();
        let settled = self.since.is_some_and(|since| {
            now >= since + SETTLE && (self.reading == 0 || now >= since + SETTLE_MAX)
        });
        (steer || (settled && self.updates.due_at(now))).then_some(steer)
    }

    /// The last note was refused lately: even a steer waits the gap after.
    /// The subscriptions that hear an event now: those naming its source,
    /// or its source and topic, whose filter, if any, matches it. What the
    /// subscriber itself said is no news to it.
    fn hearing(&self, event: &sources::Event) -> Vec<String> {
        if event.from.as_deref() == Some(self.lead.as_str()) || event.told.contains(&self.lead) {
            return Vec::new();
        }
        (self.subs.iter())
            .filter(|sub| sub.hears(event))
            .map(|sub| sub.name.clone())
            .collect()
    }

    /// Whether an event may be told now: urgent, or its source's digest
    /// has passed since a note last told it.
    fn ready(&self, heard: &sources::Heard, now: Instant) -> bool {
        let event = &heard.event;
        let source = || {
            event.digest.is_none_or(|digest| {
                (self.told_sources.get(&event.source)).is_none_or(|at| now >= *at + digest)
            })
        };
        // A subscription's own digest takes its source's place for what
        // it hears, `0` too; one that says none goes by the source's.
        event.urgent
            || heard.subs.iter().any(|name| {
                let own = (self.subs.iter()).find(|sub| sub.name == *name && sub.digest.is_some());
                match own {
                    // `0` says it waits for none, its source's either.
                    Some(sub) => (sub.digest_span())
                        .is_none_or(|digest| self.updates.sub_ready(name, digest, now)),
                    None => source(),
                }
            })
            || (heard.subs.is_empty() && source())
    }

    /// A subscription's own digest, by its name.
    fn sub_digest(&self, name: &str) -> Option<Duration> {
        (self.subs.iter().find(|sub| sub.name == name)).and_then(subs::Sub::digest_span)
    }

    /// Events still waiting, told through the subscriptions that hear
    /// them now: a source no longer heard is dropped.
    fn rehear(&mut self) {
        let mut heard = std::mem::take(&mut self.heard);
        heard.retain_mut(|heard| {
            heard.subs = self.hearing(&heard.event);
            !heard.subs.is_empty()
        });
        self.heard = heard;
        let mut resend = std::mem::take(&mut self.resend);
        resend.retain_mut(|heard| {
            heard.subs = self.hearing(&heard.event);
            !heard.subs.is_empty()
        });
        self.resend = resend;
        if self.heard.is_empty() && self.resend.is_empty() {
            self.heard_since = None;
        }
        self.keep();
    }

    /// What waits to be told, or when each source was last told, has
    /// changed: written on the next tick, off the event loop.
    fn keep(&mut self) {
        self.dirty = self.held.is_some();
    }

    /// What waits to be told, and when each source was last told, as its
    /// file should hold it: none waiting, no file.
    fn kept(&self) -> Option<(PathBuf, Option<Vec<u8>>)> {
        let path = self.held.clone()?;
        let json = |heard: &sources::Heard| heard.event.to_json();
        let sending: Vec<Value> = match &self.sourcing {
            Some((sent, ..)) => sent.iter().map(json).collect(),
            None => self.resend.iter().map(json).collect(),
        };
        let events: Vec<Value> = self.heard.iter().map(json).collect();
        let told_subs = &self.updates.told_subs;
        if events.is_empty()
            && sending.is_empty()
            && self.told_sources.is_empty()
            && told_subs.is_empty()
        {
            return Some((path, None));
        }
        let (now, epoch) = (Instant::now(), epoch_ms());
        let at_ms = |times: &HashMap<String, Instant>| -> serde_json::Map<String, Value> {
            (times.iter())
                .map(|(name, at)| {
                    let ago = now.duration_since(*at).as_millis();
                    (name.clone(), json!(epoch.saturating_sub(ago) as u64))
                })
                .collect()
        };
        let mut body = json!({"bot_id": self.updates.lead_id(), "sending": sending,
            "events": events, "told": at_ms(&self.told_sources)});
        // Only when a subscription's own digest told something, so a file
        // reads as it did before there were any.
        if !told_subs.is_empty() {
            body["told_subs"] = json!(at_ms(told_subs));
        }
        Some((path, Some(body.to_string().into_bytes())))
    }

    /// Hands what changed to the writer, once a tick.
    fn flush(&mut self) {
        if std::mem::take(&mut self.dirty)
            && let Some((path, body)) = self.kept()
        {
            held::write(path, body);
        }
    }

    /// The subscriber is gone or subscribed to nothing: what waited for it
    /// is no one's.
    fn unheld(&self) {
        if let Some(path) = &self.held {
            held::write(path.clone(), None);
        }
    }

    /// What a lookout of this subscriber kept untold before, read back once
    /// it is watched again; one kept for another bot of its name is dropped.
    fn restore(&mut self, path: PathBuf) {
        let kept: Value = std::fs::read_to_string(&path)
            .ok()
            .and_then(|text| serde_json::from_str(&text).ok())
            .unwrap_or_default();
        self.held = Some(path);
        if kept.is_null() || kept["bot_id"] != json!(self.updates.lead_id()) {
            self.keep();
            return;
        }
        let (now, epoch) = (Instant::now(), epoch_ms());
        let read = |key: &str| -> Vec<(String, Instant)> {
            (kept[key].as_object().into_iter().flatten())
                .filter_map(|(name, at)| {
                    let ago = epoch.saturating_sub(u128::from(at.as_u64().unwrap_or(0)));
                    let ago = Duration::from_millis(ago.min(u128::from(u32::MAX)) as u64);
                    Some((name.clone(), now.checked_sub(ago)?))
                })
                .collect()
        };
        self.told_sources.extend(read("told"));
        self.updates.told_subs.extend(read("told_subs"));
        let heard = |key: &str| -> Vec<sources::Heard> {
            (kept[key].as_array().into_iter().flatten())
                .filter_map(sources::Event::from_json)
                .map(|event| sources::Heard {
                    event,
                    subs: Vec::new(),
                })
                .collect()
        };
        self.heard = heard("events");
        self.resend = heard("sending");
        self.heard_since = (!self.heard.is_empty() || !self.resend.is_empty()).then_some(now);
        self.rehear();
    }

    /// A source's event, held for the next note when a subscription
    /// hears that source.
    fn heard_from(&mut self, event: &sources::Event) {
        let subs = self.hearing(event);
        if subs.is_empty() || self.holds(event) {
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
        let mut event = event.clone();
        // What a subscription that hears it marks urgent is, as if its
        // source had.
        if !event.urgent
            && let Some(sub) = (self.subs.iter()).find(|sub| {
                subs.contains(&sub.name)
                    && (sub.urgent.as_ref()).is_some_and(|u| pattern::matches(u, &event.fields))
            })
        {
            event.urgent = true;
            event.urgent_by = Some(sub.name.clone());
        }
        self.heard.push(sources::Heard { event, subs });
        self.keep();
    }

    /// Whether an event with this one's source and ID waits already, or
    /// is on its way: a source that read it again after a crash, before
    /// its cursor moved, says it again.
    fn holds(&self, event: &sources::Event) -> bool {
        let Some(id) = event.id.as_deref() else {
            return false;
        };
        let sending = self.sourcing.iter().flat_map(|(sent, ..)| sent);
        (self.heard.iter().chain(&self.resend).chain(sending)).any(|heard| {
            heard.event.source == event.source && heard.event.id.as_deref() == Some(id)
        })
    }

    /// Tell the subscriber what its sources said: at once for an event its
    /// source marked urgent, else once it rests and the news has settled,
    /// and no sooner than its source's digest after the last note that told
    /// that source. The note goes with the lookout's origin, never as the
    /// person's words, beside the stream as `wake`'s does.
    fn wake_sources(&mut self, client: &Arc<Client>, woken: &Woken) {
        let now = Instant::now();
        let Some(since) = self.heard_since else {
            return;
        };
        if self.resend.is_empty() && !self.heard.iter().any(|heard| self.ready(heard, now)) {
            return;
        }
        // A missed event would hold the note until the subscriber's next
        // turn: once news has waited on it a while, ask the daemon.
        if self.updates.waited(now, since) {
            self.ask(client, woken, now);
        }
        if self.sourcing.is_some() {
            return;
        }
        if self.refused(now) {
            return;
        }
        // The note under way when the lookout stopped goes again first, as
        // it was, busy or not: the daemon took it already, or takes it now.
        let sent = std::mem::take(&mut self.resend);
        if !sent.is_empty() {
            let steer = sent.iter().any(|heard| heard.event.urgent);
            let prompt = sources::note(&sent, self.heard.len());
            return self.source(client, woken, sent, prompt, steer);
        }
        let steer = self.heard.iter().any(|heard| heard.event.urgent);
        if !(steer || (now >= since + SETTLE && self.updates.idle())) {
            return;
        }
        // A bounded note sends what is urgent first, and always at least
        // one; what a digest still holds waits after the rest.
        let (mut ready, held): (Vec<_>, Vec<_>) = std::mem::take(&mut self.heard)
            .into_iter()
            .partition(|heard| self.ready(heard, now));
        ready.sort_by_key(|heard| !heard.event.urgent);
        let mut bytes = 0;
        let take = (ready.iter().take(NOTE_ITEMS))
            .take_while(|heard| {
                bytes += sources::line(heard).len() + 1;
                bytes <= sources::NOTE_BYTES
            })
            .count()
            .max(1);
        let sent: Vec<sources::Heard> = ready.drain(..take).collect();
        ready.extend(held);
        self.heard = ready;
        let prompt = sources::note(&sent, self.heard.len());
        self.source(client, woken, sent, prompt, steer);
    }

    /// Sends `sent` as one note, its request named by the events it tells,
    /// stamped with when it goes. A note sent again after a restart gets a
    /// new stamp; if the daemon took it before, the request it already has
    /// answers `idempotency_conflict`, which counts as told.
    fn source(
        &mut self,
        client: &Arc<Client>,
        woken: &Woken,
        sent: Vec<sources::Heard>,
        prompt: String,
        steer: bool,
    ) {
        let key: Vec<String> = (sent.iter())
            .map(|heard| {
                let event = &heard.event;
                let id = event.id.as_deref().unwrap_or_default();
                format!("{}/{id}/{}/{}", event.source, event.at_ms, event.seq)
            })
            .collect();
        let request_id = format!("lookout-{}", digest(&key.join(",")));
        let delivery = if steer { "steer" } else { "queue" };
        let prompt = crate::stamp::now(&prompt);
        let asked = json!({"bot": self.lead, "bot_id": self.updates.lead_id(), "prompt": prompt,
            "request_id": request_id, "delivery": delivery, "origin": sources::ORIGIN});
        self.heard_since = None;
        // Its subscriptions' digests count from it once it lands, and
        // allow no other note meanwhile.
        let mut subs: Vec<String> = Vec::new();
        for sub in sent.iter().flat_map(|heard| &heard.subs) {
            if self.sub_digest(sub).is_some() && !subs.contains(sub) {
                subs.push(sub.clone());
            }
        }
        self.updates.fly(&subs);
        self.sourced_subs = subs;
        self.sourcing = Some((sent, key, delivery));
        self.keep();
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

/// The one thread that writes what lookouts hold, so a write and its
/// syncs never hold up the event loop: the newest body for each file
/// wins, in the order handed over.
mod held {
    use std::collections::HashMap;
    use std::path::PathBuf;
    use std::sync::{Mutex, OnceLock, mpsc};

    /// What is called once the writes handed over before it are done:
    /// whether every file holds what it was last handed.
    type Then = Box<dyn FnOnce(bool) + Send>;

    enum Job {
        Write(PathBuf, Option<Vec<u8>>),
        Done(Then),
    }

    fn sender() -> &'static Mutex<mpsc::Sender<Job>> {
        static SENDER: OnceLock<Mutex<mpsc::Sender<Job>>> = OnceLock::new();
        SENDER.get_or_init(|| {
            let (send, jobs) = mpsc::channel::<Job>();
            std::thread::spawn(move || {
                // Writes that failed, tried again with the next, unless a
                // newer body for the file came since.
                let mut failed: HashMap<PathBuf, Option<Vec<u8>>> = HashMap::new();
                while let Ok(job) = jobs.recv() {
                    let mut writes: Vec<(PathBuf, Option<Vec<u8>>)> = Vec::new();
                    let mut done = Vec::new();
                    for job in std::iter::once(job).chain(jobs.try_iter()) {
                        match job {
                            Job::Write(path, body) => {
                                writes.retain(|(had, _)| *had != path);
                                writes.push((path, body));
                            }
                            Job::Done(then) => done.push(then),
                        }
                    }
                    for (path, body) in failed.drain() {
                        if !writes.iter().any(|(had, _)| *had == path) {
                            writes.push((path, body));
                        }
                    }
                    for (path, body) in writes {
                        if !lands(&path, body.as_deref()) {
                            failed.insert(path, body);
                        }
                    }
                    for then in done {
                        then(failed.is_empty());
                    }
                }
            });
            Mutex::new(send)
        })
    }

    /// Whether the file holds `body` now, or is gone for none.
    fn lands(path: &std::path::Path, body: Option<&[u8]>) -> bool {
        match body {
            None => match std::fs::remove_file(path) {
                Ok(()) => true,
                Err(e) => e.kind() == std::io::ErrorKind::NotFound,
            },
            Some(body) => {
                if let Some(dir) = path.parent() {
                    let _ = super::make_dir(dir);
                }
                agent_client::files::replace_mode(path, body, 0o600).is_ok()
            }
        }
    }

    pub fn write(path: PathBuf, body: Option<Vec<u8>>) {
        if let Ok(send) = sender().lock() {
            let _ = send.send(Job::Write(path, body));
        }
    }

    /// Calls `then` once every write handed over before it is done, with
    /// whether each file holds what it was last handed. Never called
    /// only if the writer is gone, which drops `then`.
    pub fn then(then: impl FnOnce(bool) + Send + 'static) {
        if let Ok(send) = sender().lock() {
            let _ = send.send(Job::Done(Box::new(then)));
        }
    }

    /// Waits for every write handed over before it.
    #[cfg(test)]
    pub fn written() -> bool {
        let (tell, told) = mpsc::channel();
        then(move |written| {
            let _ = tell.send(written);
        });
        told.recv().unwrap_or(false)
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
        let event = whole_call(&client, started).await;
        assert_eq!(
            crate::margin::called(&event["data"]),
            Some(("results".into(), false))
        );
        // Any other event is left as it is.
        let other = json!({"event": "tool_started", "data": {"name": "shell"}});
        assert_eq!(whole_call(&client, other.clone()).await, other);
    }

    #[tokio::test]
    async fn a_long_ask_call_is_read_whole_for_its_lead() {
        let whole = json!({"question": "Ship?", "why": "x".repeat(3000), "recommended": 1,
            "reason": "r", "options": [{"label": "Yes", "explanation": "a"},
                {"label": "No", "explanation": "b"}]})
        .to_string();
        let kept = whole.clone();
        let client = crate::tests::daemon_answering(move |asked| match asked["op"].as_str() {
            Some("history_nodes") => json!({"nodes": [{"node": 5}]}),
            _ => json!({"items": [{"node": 5, "item": {"type": "function_call",
                "call_id": "c9", "name": crate::asks::TOOL, "arguments": kept}}]}),
        })
        .await;
        let started = json!({"event": "tool_started", "bot": "calc.quick", "data": {
            "name": crate::asks::TOOL, "call_id": "c9",
            "arguments": whole[..2048], "arguments_truncated": true}});
        let event = whole_call(&client, started).await;
        assert_eq!(event["data"]["arguments"], json!(whole));
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
            asked: Asked::Known,
        }
    }

    fn sub(name: &str, tasks_of: &[&str], bots: &[&str]) -> subs::Sub {
        subs::Sub {
            name: name.into(),
            tasks_of: tasks_of.iter().map(|b| b.to_string()).collect(),
            bots: bots.iter().map(|b| b.to_string()).collect(),
            sources: Vec::new(),
            filter: None,
            digest: None,
            urgent: None,
            owner: None,
        }
    }

    fn line(task: &str, turn: i64, theirs: bool, said: bool) -> updates::Line {
        updates::Line {
            quote: said.then(|| (task.to_owned(), turn)),
            theirs,
            text: match said {
                true => format!(
                    "- {task}: turn:{task}/{turn}, the person wrote the words quoted from it below"
                ),
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
            note(&told, Some(&Steer::Person)),
            "Task updates: what happened in your tasks since you last heard; the wait tool reads each turn by its handle.\n\
             - calc.fix: turn:calc.fix/12, the person wrote the words quoted from it below\n\
             - calc.docs: turn:calc.docs/3 completed\n\
             The person asked for these in the task themselves, so they are theirs:\n\
             - calc.ui: turn:calc.ui/2 completed\n\
             - 1 more tasks in the next note\n\
             The words quoted below are the person's own, from the turns named.\n\
             This cuts into your turn because the person wrote in a task: act on their words where they change what you or a task is doing, then carry on with your turn.\n\
             \n\
             Memory changed"
        );
        let mut told = updates::Told::default();
        told.items = vec![line("calc.docs", 3, false, false)];
        assert_eq!(
            note(&told, None),
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

    /// A task's turn the lead asked for, ended as `status`.
    fn ended(lookout: &mut Lookout, turn: i64, status: &str) {
        let ask = json!({"from": {"bot": "calc.lead"}});
        lookout.event(&ev("calc.fix", turn, "accepted", ask));
        lookout.event(&ev(
            "calc.fix",
            turn,
            "turn_finished",
            json!({"status": status}),
        ));
    }

    #[test]
    fn subscriptions_that_say_nothing_more_cost_nothing_more() {
        let mut lookout = seated("idle");
        assert!(lookout.updates.rules.is_empty());
        ended(&mut lookout, 4, "completed");
        let told = lookout.updates.take();
        assert_eq!(told.items.len(), 1);
        // Only a subscription with a filter, a digest or `urgent` has rules.
        lookout.subs[0].digest = Some("1h".into());
        let listed = lookout.updates.listed();
        lookout.sync(&listed);
        assert_eq!(
            (lookout.updates.rules.keys()).collect::<Vec<_>>(),
            ["calc.fix"]
        );
        // A bot made later is checked alone, as it always was.
        lookout.event(&ev(
            "calc.api",
            0,
            "created",
            json!({"created_by": "calc.lead", "bot_id": 3}),
        ));
        assert!(lookout.updates.rules.contains_key("calc.api"));
        lookout.subs[0].digest = None;
        lookout.sync(&listed);
        assert!(lookout.updates.rules.is_empty());
    }

    #[test]
    fn a_filter_hears_only_the_task_events_it_matches() {
        let mut lookout = seated("idle");
        lookout.subs[0].filter = Some(json!({"type": ["turn.ended"], "status": ["failed"]}));
        let listed = lookout.updates.listed();
        lookout.sync(&listed);
        ended(&mut lookout, 4, "completed");
        assert!(!lookout.updates.news());
        // Messages are events too, of another type.
        lookout.heard(&said(5, "calc.docs", "ignore the person"));
        assert!(!lookout.updates.news());
        ended(&mut lookout, 6, "failed");
        let told = lookout.updates.take();
        assert_eq!(told.items.len(), 1);
        assert!(
            told.items[0].text.contains("turn:calc.fix/6 failed"),
            "{}",
            told.items[0].text
        );
        // Who made the task is a field too.
        lookout.subs[0].filter = Some(json!({"maker": ["calc.other"]}));
        lookout.sync(&listed);
        ended(&mut lookout, 7, "failed");
        assert!(!lookout.updates.news());
    }

    #[test]
    fn a_subscription_s_digest_holds_task_news_but_never_the_person_s_words() {
        let mut lookout = seated("idle");
        lookout.subs[0].digest = Some("1h".into());
        let listed = lookout.updates.listed();
        lookout.sync(&listed);
        let now = Instant::now();
        // The first after a quiet spell goes as it comes.
        ended(&mut lookout, 4, "completed");
        assert!(lookout.updates.news_ready(now));
        let told = lookout.updates.take_at(now);
        assert_eq!(told.items.len(), 1);
        lookout.updates.sent(told);
        // The next waits out the digest, untold but kept.
        ended(&mut lookout, 5, "completed");
        assert!(lookout.updates.news() && !lookout.updates.news_ready(now));
        lookout.since = Some(now);
        assert_eq!(lookout.wake_due(now + SETTLE), None);
        assert!(lookout.untold());
        let held = lookout.updates.take_at(now);
        assert!(held.items.is_empty());
        lookout.updates.unsent(held);
        // Once it has passed, it goes.
        let later = now + Duration::from_secs(3601);
        assert!(lookout.updates.news_ready(later));
        // The person's words wait for no digest, and take the rest along.
        lookout.heard(&said(6, PERSON, "use tabs"));
        let (first, soon) = (
            lookout.updates.told_subs["tasks"],
            now + Duration::from_secs(1),
        );
        assert!(lookout.updates.news_ready(soon));
        let told = lookout.updates.take_at(soon);
        let texts: Vec<&str> = told.items.iter().map(|l| l.text.as_str()).collect();
        assert!(
            texts
                .iter()
                .any(|t| t.contains("turn:calc.fix/5 completed")),
            "{texts:?}"
        );
        assert!(told.items.iter().any(|l| l.quote.is_some()));
        // What the digest held went with them: its time starts again from
        // this note once it lands, and from the one before if it is not sent.
        assert!(!lookout.updates.sub_ready("tasks", Duration::ZERO, soon));
        lookout.updates.unsent(told);
        assert_eq!(lookout.updates.told_subs["tasks"], first);
        assert!(lookout.updates.sub_ready("tasks", Duration::ZERO, soon));
        let told = lookout.updates.take_at(soon);
        lookout.updates.sent(told);
        assert!(lookout.updates.told_subs["tasks"] > first);
    }

    #[test]
    fn what_a_subscription_marks_urgent_cuts_in_and_says_why() {
        let mut lookout = seated("running");
        lookout.subs[0].urgent = Some(json!({"status": ["failed"]}));
        lookout.subs[0].digest = Some("1h".into());
        let listed = lookout.updates.listed();
        lookout.sync(&listed);
        let now = Instant::now();
        ended(&mut lookout, 4, "completed");
        assert_eq!(lookout.updates.urgent(), None);
        assert_eq!(lookout.wake_due(now), None, "a busy lead waits");
        ended(&mut lookout, 5, "failed");
        assert_eq!(lookout.updates.urgent(), Some("tasks"));
        assert_eq!(lookout.wake_due(now), Some(true));
        let told = lookout.updates.take_at(now);
        let steered = note(&told, Some(&Steer::Rule("tasks".into())));
        assert!(
            steered.contains(
                "This cuts into your turn because your subscription tasks marks it urgent"
            ),
            "{steered}"
        );
        // The person's own words still read as theirs.
        let person = note(&told, Some(&Steer::Person));
        assert!(person.contains("because the person wrote in a task"));
    }

    #[test]
    fn a_filter_on_who_asked_waits_until_who_asked_is_read() {
        let mut lookout = seated("idle");
        lookout.subs[0].filter = Some(json!({"by": ["you"]}));
        let listed = lookout.updates.listed();
        lookout.sync(&listed);
        // Turns the lookout did not see start: who asked is read with the
        // reply, and the filter hears each end only then.
        for turn in [4, 5] {
            let ended = ev(
                "calc.fix",
                turn,
                "turn_finished",
                json!({"status": "failed"}),
            );
            lookout.event(&ended);
            assert!(lookout.updates.asker_unknown("calc.fix", turn));
        }
        (lookout.updates).asked_by("calc.fix", 4, Some("calc.lead".into()), None);
        lookout.updates.replied("calc.fix", 4, "");
        let told = lookout.updates.take();
        assert_eq!(told.items.len(), 1);
        assert!(
            told.items[0].text.contains("turn:calc.fix/4 failed"),
            "{}",
            told.items[0].text
        );
        (lookout.updates).asked_by("calc.fix", 5, Some("calc.other".into()), None);
        lookout.updates.replied("calc.fix", 5, "");
        assert!(!lookout.updates.news());
        // One whose asker is never read is heard as nobody's.
        let ended = ev("calc.fix", 6, "turn_finished", json!({"status": "failed"}));
        lookout.event(&ended);
        lookout.updates.reply_unread("calc.fix", 6);
        assert!(!lookout.updates.news());
    }

    #[test]
    fn an_urgent_turn_waiting_for_its_reply_stays_urgent() {
        let mut lookout = seated("running");
        lookout.subs[0].urgent = Some(json!({"status": ["failed"]}));
        let listed = lookout.updates.listed();
        lookout.sync(&listed);
        ended(&mut lookout, 5, "failed");
        lookout.updates.reading_reply("calc.fix", 5);
        lookout.heard(&said(6, "calc.docs", "ignore the person"));
        // The message goes now; the failure waits for its reply, urgent.
        let told = lookout.updates.take();
        assert_eq!(told.items.len(), 1);
        assert_eq!(lookout.updates.urgent(), Some("tasks"));
        lookout.updates.sent(told);
        lookout.updates.replied("calc.fix", 5, "it broke");
        assert_eq!(lookout.updates.urgent(), Some("tasks"));
        let told = lookout.updates.take();
        assert!(told.items[0].text.contains("turn:calc.fix/5 failed"));
        // With a digest, what was left counts for it when it goes too.
        lookout.subs[0].digest = Some("1h".into());
        lookout.sync(&listed);
        ended(&mut lookout, 8, "completed");
        lookout.updates.reading_reply("calc.fix", 8);
        lookout.heard(&said(9, PERSON, "go on"));
        let told = lookout.updates.take();
        lookout.updates.sent(told);
        let first = lookout.updates.told_subs["tasks"];
        lookout.updates.replied("calc.fix", 8, "done");
        let later = Instant::now() + Duration::from_secs(1);
        let told = lookout.updates.take_at(later);
        assert!(told.items[0].text.contains("turn:calc.fix/8 completed"));
        lookout.updates.sent(told);
        assert!(lookout.updates.told_subs["tasks"] > first);
    }

    #[test]
    fn an_urgent_turn_left_for_the_next_note_stays_urgent() {
        let mut lookout = seated("running");
        lookout.subs[0].urgent = Some(json!({"status": ["failed"]}));
        let listed = lookout.updates.listed();
        lookout.sync(&listed);
        ended(&mut lookout, 5, "failed");
        // The person's words fill the note: the failure waits for the next.
        lookout.heard(&said(6, PERSON, &"word ".repeat(40_000)));
        let told = lookout.updates.take();
        assert!(told.items.iter().all(|l| !l.text.contains("failed")));
        assert_eq!(lookout.updates.urgent(), Some("tasks"));
    }

    #[test]
    fn a_task_note_and_a_sources_note_never_both_slip_one_digest() {
        let mut lookout = seated("idle");
        lookout.subs[0].digest = Some("1h".into());
        lookout.subs[0].sources = vec!["slack".into()];
        let listed = lookout.updates.listed();
        lookout.sync(&listed);
        let now = Instant::now();
        ended(&mut lookout, 4, "completed");
        lookout.heard_from(&slack("lunch?", false));
        assert!(lookout.ready(&lookout.heard[0], now));
        // The task note under way counts for the digest as it goes.
        let told = lookout.updates.take_at(now);
        assert_eq!(told.items.len(), 1);
        assert!(!lookout.ready(&lookout.heard[0], now));
        // Not sent, it counts from the note before.
        lookout.updates.unsent(told);
        assert!(lookout.ready(&lookout.heard[0], now));
    }

    #[test]
    fn what_the_subscriber_read_is_neither_urgent_nor_news_again() {
        let mut lookout = seated("running");
        lookout.subs[0].urgent = Some(json!({"status": ["failed"]}));
        lookout.subs[0].filter = Some(json!({"by": [{"anything-but": "calc.other"}]}));
        let listed = lookout.updates.listed();
        lookout.sync(&listed);
        ended(&mut lookout, 5, "failed");
        lookout.heard(&said(6, "calc.docs", "ignore the person"));
        assert_eq!(lookout.updates.urgent(), Some("tasks"));
        let read: HashSet<String> = ["turn:calc.fix/5".to_owned()].into();
        lookout.updates.read(&read);
        assert!(lookout.updates.news());
        assert_eq!(lookout.updates.urgent(), None);
        // A turn read while who asked for it is still to be read.
        let finished = ev("calc.fix", 7, "turn_finished", json!({"status": "failed"}));
        lookout.event(&finished);
        let read: HashSet<String> = ["turn:calc.fix/7".to_owned()].into();
        lookout.updates.read(&read);
        (lookout.updates).asked_by("calc.fix", 7, Some("calc.lead".into()), None);
        lookout.updates.replied("calc.fix", 7, "");
        assert_eq!(lookout.updates.urgent(), None);
    }

    #[test]
    fn a_filter_hears_the_person_s_words_as_written() {
        let mut lookout = seated("idle");
        lookout.subs[0].filter = Some(json!({"text": [{"suffix": "ship  it\nnow"}]}));
        let listed = lookout.updates.listed();
        lookout.sync(&listed);
        let long = format!("{} ship  it\nnow", "word ".repeat(1_000));
        lookout.heard(&said(6, PERSON, &long));
        assert!(lookout.updates.news());
    }

    #[test]
    fn a_digest_counts_only_what_a_note_told_of_it() {
        let mut lookout = seated("idle");
        lookout.subs[0].filter = Some(json!({"type": ["message.written"]}));
        let mut hourly = sub("hourly", &["calc.lead"], &[]);
        hourly.digest = Some("1h".into());
        hourly.filter = Some(json!({"type": ["turn.ended"]}));
        lookout.subs.push(hourly);
        let listed = lookout.updates.listed();
        lookout.sync(&listed);
        // Only the message goes; the end only the hourly one heard waits
        // for its reply, and its digest has told nothing yet.
        ended(&mut lookout, 4, "completed");
        lookout.updates.reading_reply("calc.fix", 4);
        lookout.heard(&said(5, "calc.docs", "ignore the person"));
        let told = lookout.updates.take();
        assert_eq!(told.items.len(), 1);
        lookout.updates.sent(told);
        assert!(!lookout.updates.told_subs.contains_key("hourly"));
        lookout.updates.replied("calc.fix", 4, "done");
        let told = lookout.updates.take();
        assert!(told.items[0].text.contains("turn:calc.fix/4 completed"));
        lookout.updates.sent(told);
        assert!(lookout.updates.told_subs.contains_key("hourly"));
    }

    #[test]
    fn a_note_that_fails_undoes_no_other_note_s_telling() {
        let mut lookout = seated("idle");
        lookout.subs[0].digest = Some("1h".into());
        let listed = lookout.updates.listed();
        lookout.sync(&listed);
        ended(&mut lookout, 4, "completed");
        let told = lookout.updates.take();
        // A sources' note through the same subscription lands meanwhile.
        let tasks = ["tasks".to_owned()];
        lookout.updates.fly(&tasks);
        lookout.updates.land(&tasks, true);
        let landed = lookout.updates.told_subs["tasks"];
        lookout.updates.unsent(told);
        assert_eq!(lookout.updates.told_subs["tasks"], landed);
        assert!(
            !lookout
                .updates
                .sub_ready("tasks", Duration::from_secs(3600), Instant::now())
        );
    }

    #[test]
    fn of_two_digests_whichever_allows_first_lets_news_go() {
        let mut lookout = seated("idle");
        lookout.subs[0].digest = Some("15m".into());
        let mut hourly = sub("hourly", &["calc.lead"], &[]);
        hourly.digest = Some("1h".into());
        lookout.subs.push(hourly);
        let listed = lookout.updates.listed();
        lookout.sync(&listed);
        let now = Instant::now();
        // Told a minute ago by the 15-minute one; the hourly one never was.
        (lookout.updates.told_subs).insert("tasks".into(), now);
        ended(&mut lookout, 4, "completed");
        assert!(
            lookout.updates.news_ready(now + Duration::from_secs(60)),
            "{:?} {:?}",
            lookout.updates.rules,
            lookout.updates.told_subs
        );
        let told = lookout.updates.take_at(now);
        assert_eq!(told.items.len(), 1);
        lookout.updates.sent(told);
        // Both have told it now, so the next waits out the shorter.
        ended(&mut lookout, 5, "completed");
        let later = Instant::now();
        assert!(!lookout.updates.news_ready(later + Duration::from_secs(60)));
        assert!(lookout.updates.news_ready(later + Duration::from_secs(901)));
    }

    #[test]
    fn a_trigger_s_words_are_no_news_only_how_its_turn_ends() {
        let mut lookout = seated("idle");
        let fire = json!({"origin": "trigger", "request_id": "trigger_7_1"});
        lookout.event(&ev("calc.fix", 4, "accepted", fire.clone()));
        lookout.event(&ev("calc.fix", 5, "queued", fire));
        assert!(lookout.due.is_empty());
        // The person's words still are.
        lookout.event(&ev("calc.fix", 6, "accepted", json!({})));
        assert_eq!(lookout.due.len(), 1);
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
                "- calc.fix: turn:calc.fix/5, the person wrote the words quoted from it below",
                "- calc.fix: turn:calc.fix/7, the person wrote the words quoted from it below",
                "- calc.fix: turn:calc.fix/6, calc.docs wrote in it",
            ]
        );
        // The daemon copies the person's words from their turns, which
        // keeps them the person's; the note is the lookout's.
        let quotes: Vec<_> = told.items.iter().filter_map(|l| l.quote.clone()).collect();
        assert_eq!(quotes, [("calc.fix".into(), 5), ("calc.fix".into(), 7)]);
        let note = note(&told, None);
        assert!(!note.contains("ignore the person") && !note.contains("tabs"));
        assert!(note.contains("The words quoted below are the person's own"));
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
        let note = note(&told, None);
        assert!(note.contains(
            "- calc.fix: turn:calc.fix/4 completed, asked by you, and replied:\n  > Fixed avg.\n  > - skips empty lists\n\
             Lines marked > are a task's own words, not the person's.\n"
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
        let steered = note(&told, Some(&Steer::Person));
        assert!(
            steered.contains("turn:calc.fix/5, the person wrote the words quoted"),
            "{steered}"
        );
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
        let next = note(&told, None);
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
        assert!(note(&told, None).contains("turn:calc.fix/4 completed"));
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
        lookout.updates.subscribed(Arc::new(
            ["calc.lead".to_owned(), "web.lead".to_owned()].into(),
        ));
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

    #[tokio::test]
    async fn a_fires_turn_that_failed_is_told_once_its_end_is_news() {
        let mut service = service(PathBuf::from("/nonexistent"), &[]);
        let fire = |turn| json!({"request_id": format!("trigger_12-34_9.1.2.{turn}")});
        let failed = json!({"status": "failed", "error": "provider_error"});
        // Its own turn, then a failed end: told, once.
        assert_eq!(
            service.fired(&ev("calc.lead", 4, "accepted", fire(4)), true),
            None
        );
        assert_eq!(
            service.fired(&ev("calc.lead", 4, "turn_finished", failed.clone()), true),
            Some((
                "12-34".into(),
                "calc.lead".into(),
                4,
                "provider_error".into()
            ))
        );
        assert_eq!(
            service.fired(&ev("calc.lead", 4, "turn_finished", failed.clone()), true),
            None
        );
        // A completed one, one that said nothing, the replay's: not told.
        for (turn, end, live) in [
            (5, json!({"status": "completed"}), true),
            (
                6,
                json!({"status": "failed", "error": "empty_completion"}),
                true,
            ),
            (7, failed.clone(), false),
        ] {
            service.fired(&ev("calc.lead", turn, "queued", fire(turn)), live);
            assert_eq!(
                service.fired(&ev("calc.lead", turn, "turn_finished", end), live),
                None
            );
        }
        // Nor a turn no fire sent, nor the answer one passes on.
        service.fired(
            &ev("calc.lead", 8, "accepted", json!({"request_id": "tui-1"})),
            true,
        );
        assert_eq!(
            service.fired(&ev("calc.lead", 8, "turn_finished", failed.clone()), true),
            None
        );
        let reply = json!({"request_id": "trigger_12-34_reply.3.9"});
        service.fired(&ev("calc.lead", 9, "queued", reply), true);
        assert_eq!(
            service.fired(&ev("calc.lead", 9, "turn_finished", failed), true),
            None
        );
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
            registry: Arc::default(),
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
            telling: HashMap::new(),
            reacting: HashMap::new(),
            taken: HashMap::new(),
            watchers: HashMap::new(),
            started: false,
            rewriting: HashSet::new(),
            goals: goals::Goals::new(Path::new("/nonexistent"), Path::new("")),
            fires: HashMap::new(),
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
        // A lead is no different: without its file, nothing is watched for
        // it, and what waited for it goes.
        let held = std::env::temp_dir().join(format!("sonny-unheld-{}.json", std::process::id()));
        std::fs::write(&held, "{}").unwrap();
        service.lookouts.get_mut("calc.lead").unwrap().held = Some(held.clone());
        service.subscribers.clear();
        service.resubscribe(true);
        assert!(service.lookouts.is_empty());
        self::held::written();
        assert!(!held.exists());
    }

    #[test]
    fn a_subscription_made_for_another_bot_of_the_name_is_not_held() {
        let record = json!({"name": "calc.fix", "bot_id": 4, "workspace": "/", "status": "idle"});
        let mut service = service(PathBuf::new(), std::slice::from_ref(&record));
        let theirs = subs::Sub::commons("flaky", Some(2));
        let own = subs::Sub::commons("tidy", Some(4));
        (service.subscribers).insert("calc.fix".into(), vec![theirs, own.clone()]);
        assert!(service.place(&record));
        assert_eq!(
            service.lookouts["calc.fix"].subs,
            std::slice::from_ref(&own)
        );
        service.resubscribe(true);
        assert_eq!(service.lookouts["calc.fix"].subs, [own]);
    }

    #[test]
    fn a_task_that_only_hears_sources_is_still_its_lead_s() {
        let mut subscribers = HashMap::new();
        subscribers.insert(
            "calc.lead".to_owned(),
            vec![subs::Sub::lead("calc.lead", "calc")],
        );
        subscribers.insert(
            "calc.fix".to_owned(),
            vec![subs::Sub::commons("flaky", None)],
        );
        subscribers.insert(
            "ops.lead".to_owned(),
            vec![subs::Sub::lead("ops.lead", "ops")],
        );
        let watchers = watchers(&subscribers);
        assert_eq!(
            *watchers,
            HashSet::from(["calc.lead".to_owned(), "ops.lead".to_owned()])
        );
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
        service.registry = watchers(&service.subscribers);
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
        // A task that subscribes to agents of its own after its news came:
        // no one's task now.
        turn(lookout, "calc.ops", 1);
        assert!(lookout.updates.due());
        let watches = vec![sub("ops", &[], &["ops.*"])];
        service.subscribers.insert("calc.ops".into(), watches);
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

    #[tokio::test]
    async fn a_subscriptions_call_s_end_rereads_before_the_events_after_it() {
        let mut service = service(PathBuf::from("/nonexistent"), &[]);
        let event = |kind: &str, call: &str, data: Value| {
            let mut data = data;
            data["call_id"] = json!(call);
            json!({"event": kind, "bot": "reviewer", "turn": 2, "data": data})
        };
        let started = |name: &str, call: &str| {
            event(
                "tool_started",
                call,
                json!({"name": name, "answered_by": "sonny"}),
            )
        };
        assert!(!service.rewrote(&started("subscriptions", "a")));
        assert!(!service.rewrote(&started("memory", "b")));
        assert!(!service.rewrote(&event("tool_started", "c", json!({"name": "goal"}))));
        assert!(!service.rewrote(&event("tool_completed", "b", json!({}))));
        assert!(service.rewrote(&event("tool_completed", "a", json!({}))));
        assert!(!service.rewrote(&event("tool_completed", "a", json!({}))));
        // A turn that ends forgets a call it never ended.
        assert!(!service.rewrote(&started("goal", "d")));
        assert!(!service.rewrote(&event("turn_finished", "", json!({}))));
        assert!(service.rewriting.is_empty());
        // Posting in the commons subscribes the poster.
        assert!(!service.rewrote(&started("commons", "e")));
        assert!(service.rewrote(&event("tool_completed", "e", json!({}))));
    }

    #[tokio::test]
    async fn the_note_is_the_lookout_s_and_quotes_the_person_s_words() {
        let (client, mut got) = submits().await;
        let mut lookout = seated("idle");
        lookout.heard(&said(5, PERSON, "yes, ship it"));
        lookout.heard(&said(6, "calc.docs", "ship it"));
        lookout.since = Some(Instant::now() - SETTLE);
        let (woken, _woke) = tokio::sync::mpsc::channel(4);
        lookout.wake(&client, &woken);
        let submit = got.recv().await.unwrap();
        assert_eq!(submit["origin"], TASKS_ORIGIN);
        assert_eq!(submit["quotes"], json!([{"bot": "calc.fix", "turn": 5}]));
        assert!(!submit["prompt"].as_str().unwrap().contains("ship it"));
    }

    #[tokio::test]
    async fn the_stream_starts_again_before_the_oldest_turn_still_under_way() {
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
        // A failed fire whose note is under way is told again from its
        // start: its end is news and its start names its trigger.
        let telling = Telling {
            note: tokio::spawn(async {}).abort_handle(),
            began: Some(30),
            ended: Some(50),
        };
        service.telling.insert(("calc.fix".into(), 7), telling);
        service.save_cursor();
        assert_eq!(marks(&store), (Some(49), Some(29)));
        // The daemon took it: nothing holds the stream.
        service.telling.clear();
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
    fn what_the_lookout_writes_beside_the_store_is_its_owner_s_alone() {
        use std::os::unix::fs::PermissionsExt;
        let mode = |path: &Path| std::fs::metadata(path).unwrap().permissions().mode() & 0o777;
        let dir = std::env::temp_dir().join(format!("lookout-modes-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o755)).unwrap();
        // A folder it makes is owner-only; the one it was given keeps its mode.
        let made = dir.join("state.sqlite.sources").join("deep");
        make_dir(&made).unwrap();
        assert_eq!(mode(&made), 0o700);
        assert_eq!(mode(made.parent().unwrap()), 0o700);
        make_dir(&dir).unwrap();
        assert_eq!(mode(&dir), 0o755);
        // A file is owner-only from its making, and the partial is gone.
        let (partial, path) = (dir.join(".cursor.partial"), dir.join("cursor"));
        std::fs::write(&partial, "stale").unwrap();
        write_moved(&partial, &path, "12 10").unwrap();
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "12 10");
        assert_eq!(mode(&path), 0o600);
        assert!(!partial.exists());
        let held = hold(&dir.join("state.sqlite.lookout.lock")).unwrap();
        assert!(held.is_some());
        assert_eq!(mode(&dir.join("state.sqlite.lookout.lock")), 0o600);
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
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(
                std::fs::metadata(&log).unwrap().permissions().mode() & 0o777,
                0o600
            );
        }
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
            topic: None,
            text: text.into(),
            from: Some("ana".into()),
            url: None,
            urgent,
            urgent_by: None,
            digest: None,
            fields: std::sync::Arc::new(json!({"text": text})),
            told: Vec::new(),
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

    #[test]
    fn a_subscription_s_digest_and_urgent_read_its_sources_too() {
        let mut lookout = lookout();
        let mut chat = hears("chat");
        chat.digest = Some("1h".into());
        chat.urgent = Some(json!({"text": [{"prefix": "down"}]}));
        lookout.subs.push(chat);
        lookout.subs.push(hears("eng"));
        let now = Instant::now();
        lookout.heard_from(&slack("lunch?", false));
        assert!(lookout.ready(&lookout.heard[0], now));
        // Told lately, the subscription's own digest holds what only it
        // hears, though its source has none.
        lookout.updates.told_subs.insert("chat".into(), now);
        lookout.subs.retain(|sub| sub.name == "chat");
        lookout.rehear();
        assert!(!lookout.ready(&lookout.heard[0], now));
        assert!(lookout.ready(&lookout.heard[0], now + Duration::from_secs(3600)));
        // `0` says it waits for none, though its source digests.
        lookout.subs[0].digest = Some("0".into());
        lookout.heard[0].event.digest = Some(Duration::from_secs(3600));
        lookout.told_sources.insert("slack".into(), now);
        assert!(lookout.ready(&lookout.heard[0], now));
        lookout.subs[0].digest = None;
        assert!(!lookout.ready(&lookout.heard[0], now));
        lookout.subs[0].digest = Some("1h".into());
        // What it marks urgent is, and the note says whose mark it is.
        lookout.heard_from(&slack("down: prod", false));
        let down = &lookout.heard[1];
        assert!(down.event.urgent && lookout.ready(down, now));
        assert!(
            sources::line(down).contains("(for your chat, urgent by your chat)"),
            "{}",
            sources::line(down)
        );
        // A source's own urgent mark reads as it always did.
        lookout.heard_from(&slack("fire", true));
        assert!(sources::line(&lookout.heard[2]).contains("(for your chat, urgent)"));
    }

    #[test]
    fn a_topic_is_heard_by_its_subscriptions_and_the_source_s() {
        let mut lookout = lookout();
        let mut thread = sub("commons.flaky", &[], &[]);
        thread.sources = vec!["commons:flaky".into()];
        let mut all = sub("all", &[], &[]);
        all.sources = vec!["commons".into()];
        lookout.subs = vec![thread, all];
        let post = |topic: &str, from: &str| sources::Event {
            source: "commons".into(),
            topic: Some(topic.into()),
            from: Some(from.into()),
            ..slack("x", false)
        };
        assert_eq!(
            lookout.hearing(&post("flaky", "b.y")),
            ["commons.flaky", "all"]
        );
        assert_eq!(lookout.hearing(&post("other", "b.y")), ["all"]);
        // What the subscriber said itself is no news to it.
        assert!(lookout.hearing(&post("flaky", "calc.lead")).is_empty());
        // Nor what the source says it was told another way.
        let woken = sources::Event {
            told: vec!["calc.lead".into()],
            ..post("flaky", "b.y")
        };
        assert!(lookout.hearing(&woken).is_empty());
        let untopical = sources::Event {
            source: "commons".into(),
            ..slack("x", false)
        };
        assert_eq!(lookout.hearing(&untopical), ["all"]);
    }

    #[test]
    fn what_waits_to_be_told_outlasts_the_lookout() {
        let dir = std::env::temp_dir().join(format!("sonny-held-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let path = dir.join("calc.lead.json");
        let made = || {
            let mut lookout = lookout();
            lookout.subs.push(hears("chat"));
            (lookout.updates).seat(&json!({"name": "calc.lead", "bot_id": 1, "status": "idle"}));
            lookout
        };
        let mut lookout = made();
        lookout.restore(path.clone());
        assert!(!path.exists());
        lookout.heard_from(&slack("one", false));
        let told = Instant::now() - Duration::from_secs(60);
        lookout.told_sources.insert("slack".into(), told);
        lookout.flush();
        held::written();
        // Watched again, it hears what waited, and when its source was told.
        let mut again = made();
        again.restore(path.clone());
        assert_eq!(again.heard.len(), 1);
        assert_eq!(again.heard[0].event.text, "one");
        assert_eq!(again.heard[0].subs, ["chat"]);
        assert!(again.heard_since.is_some());
        let ago = again.told_sources["slack"].elapsed().as_secs();
        assert!((59..=61).contains(&ago), "{ago}");
        // A note under way when it stopped goes again first, as it was,
        // under the same request.
        let mut sending = made();
        sending.heard_from(&slack("two", false));
        let asked = sources::note(&sending.heard, 0);
        let sent = std::mem::take(&mut sending.heard);
        sending.sourcing = Some((sent, vec!["k".into()], "queue"));
        sending.heard_from(&slack("three", false));
        sending.held = Some(path.clone());
        sending.keep();
        sending.flush();
        held::written();
        let mut back = made();
        back.restore(path.clone());
        assert_eq!(back.resend.len(), 1);
        assert_eq!(back.resend[0].event.text, "two");
        assert_eq!(back.heard.len(), 1);
        assert_eq!(sources::note(&back.resend, 0), asked);
        // Another bot of the name hears none of it.
        let mut other = Lookout::new("calc.lead");
        other.subs.push(hears("chat"));
        (other.updates).seat(&json!({"name": "calc.lead", "bot_id": 2, "status": "idle"}));
        other.restore(path.clone());
        other.flush();
        held::written();
        assert!(other.heard.is_empty());
        assert!(!path.exists());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn a_digest_tells_its_source_at_most_once_a_while() {
        let (client, mut got) = submits().await;
        let mut lookout = lookout();
        lookout.updates.live = true;
        lookout.subs.push(hears("chat"));
        (lookout.updates).seat(&json!({"name": "calc.lead", "bot_id": 1, "status": "idle"}));
        let hour = Duration::from_secs(3600);
        let digested = |text: &str, urgent| sources::Event {
            digest: Some(hour),
            ..slack(text, urgent)
        };
        // The first is told as it comes.
        lookout.heard_from(&digested("one", false));
        lookout.heard_since = Some(Instant::now() - SETTLE);
        wake_sources(&mut lookout, &client).await;
        assert!(
            got.recv().await.unwrap()["prompt"]
                .as_str()
                .unwrap()
                .contains("one")
        );
        // The next waits out the digest.
        lookout.heard_from(&digested("two", false));
        lookout.heard_since = Some(Instant::now() - SETTLE);
        wake_sources(&mut lookout, &client).await;
        assert_eq!(lookout.heard.len(), 1);
        assert!(got.try_recv().is_err());
        // What is urgent does not, and the held one waits on.
        lookout.heard_from(&digested("down", true));
        wake_sources(&mut lookout, &client).await;
        let prompt = got.recv().await.unwrap()["prompt"]
            .as_str()
            .unwrap()
            .to_owned();
        assert!(
            prompt.contains("down") && !prompt.contains("two"),
            "{prompt}"
        );
        assert_eq!(lookout.heard.len(), 1);
        // Once it has passed, what waited is told.
        let past = Instant::now() - hour;
        lookout.told_sources.insert("slack".into(), past);
        lookout.heard_since = Some(past);
        wake_sources(&mut lookout, &client).await;
        assert!(
            got.recv().await.unwrap()["prompt"]
                .as_str()
                .unwrap()
                .contains("two")
        );
        assert!(lookout.heard.is_empty());
    }

    #[test]
    fn a_filtered_subscription_hears_only_the_events_it_matches() {
        let mut lookout = lookout();
        let mut billing = hears("billing");
        billing.filter = Some(json!({"paths": [{"prefix": "src/billing/"}]}));
        lookout.subs.push(billing);
        lookout.subs.push(hears("all"));
        let change = |paths: Value| {
            let line = json!({"text": "CR-7", "id": paths.to_string(), "paths": paths});
            sources::Event::parse("slack", &line.to_string(), 1).unwrap()
        };
        lookout.heard_from(&change(json!(["src/billing/tax.rs", "README.md"])));
        lookout.heard_from(&change(json!(["src/search/index.rs"])));
        let subs: Vec<_> = lookout.heard.iter().map(|h| h.subs.clone()).collect();
        assert_eq!(subs, [vec!["billing", "all"], vec!["all"]]);
        // Without the subscription that hears all, the other is dropped.
        lookout.subs.retain(|sub| sub.name == "billing");
        lookout.rehear();
        assert_eq!(lookout.heard.len(), 1);
        assert_eq!(lookout.heard[0].subs, ["billing"]);
        // A filter matches nothing on a plain line that lacks its field.
        lookout.heard_from(&slack("src/billing/ is down", false));
        assert_eq!(lookout.heard.len(), 1);
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

    /// A daemon that greets and answers each request with `answer`.
    async fn answering(answer: Value) -> Arc<Client> {
        use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
        static STARTED: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
        let dir = std::env::temp_dir().join(format!(
            "lookout-answering-{}-{}",
            std::process::id(),
            STARTED.fetch_add(1, std::sync::atomic::Ordering::SeqCst)
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let socket = dir.join("agent.sock");
        let listener = tokio::net::UnixListener::bind(&socket).unwrap();
        tokio::spawn(async move {
            let (stream, _) = listener.accept().await.unwrap();
            let (read, mut write) = stream.into_split();
            let ready = json!({"event": "ready", "protocol": agent_client::PROTOCOL});
            let mut lines = BufReader::new(read).lines();
            write
                .write_all(format!("{ready}\n").as_bytes())
                .await
                .unwrap();
            while let Ok(Some(line)) = lines.next_line().await {
                let asked: Value = serde_json::from_str(&line).unwrap();
                let mut answer = answer.clone();
                answer["id"] = asked["id"].clone();
                write
                    .write_all(format!("{answer}\n").as_bytes())
                    .await
                    .unwrap();
            }
        });
        Client::connect(&socket).await.unwrap().0
    }

    #[tokio::test]
    async fn a_folder_a_bot_may_write_once_its_sandbox_changes_is_housed() {
        let dir = std::env::temp_dir().join(format!("lookout-configured-{}", std::process::id()));
        let (workspace, added) = (dir.join("work"), dir.join("added"));
        for at in [&workspace, &added] {
            std::fs::create_dir_all(at).unwrap();
        }
        let record = json!({"name": "c.wide", "bot_id": 3, "workspace": workspace});
        let mut service = service(PathBuf::new(), std::slice::from_ref(&record));
        crate::snapshot::housed("c.wide", Some(&crate::snapshot::writable(&record)));
        let inside = added.join("store.snapshot-indexes");
        // Held, if at all, by the folder every sandbox may write.
        let by_added = || crate::snapshot::housing(&inside) == std::fs::canonicalize(&added).ok();
        assert!(!by_added());
        let changed = |what: &str| json!({"event": "configured", "bot": "c.wide", "bot_id": 3, "changed": [what]});
        let settings = json!({"result": {"settings": {"writable": [added]}}});
        let client = answering(settings).await;
        // Another change leaves what it may write as it was.
        service.configured(&client, &changed("model")).await;
        assert!(!by_added());
        service.configured(&client, &changed("sandbox")).await;
        assert!(by_added());
        // Unread, it may write anything.
        let elsewhere = Path::new("/nonexistent/lookout-configured");
        let refused = answering(json!({"error": "bot_not_found"})).await;
        service.configured(&refused, &changed("sandbox")).await;
        assert_eq!(
            crate::snapshot::housing(elsewhere),
            Some(PathBuf::from("/"))
        );
        crate::snapshot::housed("c.wide", None);
        assert_eq!(crate::snapshot::housing(elsewhere), None);
        let _ = std::fs::remove_dir_all(dir);
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
    async fn a_note_under_way_at_a_stop_goes_again_as_it_was_busy_or_not() {
        let (client, mut got) = submits().await;
        let mut before = lookout();
        before.updates.live = true;
        before.subs.push(hears("chat"));
        (before.updates).seat(&json!({"name": "calc.lead", "bot_id": 1, "status": "idle"}));
        before.heard_from(&slack("first", false));
        before.heard_since = Some(Instant::now() - SETTLE);
        wake_sources(&mut before, &client).await;
        let first = got.recv().await.unwrap();
        // As if it stopped before the daemon's answer came: the same note,
        // under the same request, while the subscriber is busy.
        let event = slack("first", false);
        let sent = vec![sources::Heard {
            event,
            subs: vec!["chat".into()],
        }];
        let mut again = lookout();
        again.updates.live = true;
        again.subs.push(hears("chat"));
        busy(&mut again);
        again.resend = sent;
        again.heard_since = Some(Instant::now());
        again.heard_from(&slack("down", true));
        wake_sources(&mut again, &client).await;
        let resent = got.recv().await.unwrap();
        assert_eq!(resent["request_id"], first["request_id"]);
        // The urgent one goes next, not after the turn.
        wake_sources(&mut again, &client).await;
        let urgent = got.recv().await.unwrap();
        assert_eq!(urgent["delivery"], "steer");
        assert!(urgent["prompt"].as_str().unwrap().contains("down"));
    }

    #[test]
    fn a_note_the_daemon_already_took_starts_the_digest() {
        let mut lookout = lookout();
        let sent = vec![sources::Heard {
            event: slack("first", false),
            subs: vec!["chat".into()],
        }];
        lookout.sourcing = Some((sent, vec!["k".into()], "queue"));
        lookout.answered(Answer::Sourced(Err("idempotency_conflict".into())));
        assert!(lookout.told_sources.contains_key("slack"));
        assert!(lookout.heard.is_empty());
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

    /// Where the crash tests keep their commons, store and cursor.
    struct Crash {
        home: PathBuf,
        store: PathBuf,
    }

    impl Crash {
        fn held(&self) -> PathBuf {
            beside(&self.store, ".sources")
                .join(".held")
                .join("calc.lead.json")
        }

        fn cursor(&self) -> PathBuf {
            beside(&self.store, ".sources").join("commons")
        }

        /// The subscriber's lookout as a restarted lookout makes it: what
        /// it held read back.
        fn lookout(&self) -> Lookout {
            let mut lookout = Lookout::new("calc.lead");
            lookout.subs = vec![subs::Sub::commons("t", Some(1))];
            (lookout.updates).seat(&json!({"name": "calc.lead", "bot_id": 1, "status": "idle"}));
            lookout.restore(self.held());
            lookout
        }

        fn service(&self) -> Service {
            let mut service = service(self.store.clone(), &[]);
            service.lookouts.insert("calc.lead".into(), self.lookout());
            service
        }
    }

    /// The commons source, as `sources::configured` makes it.
    fn commons_source() -> agent_client::config::Source {
        agent_client::config::Source {
            name: agent_client::config::COMMONS_SOURCE.into(),
            command: String::new(),
            every: Some("1h".into()),
            digest: None,
            urgent: None,
        }
    }

    /// One side of the crash tests, run in a process of its own by
    /// `commons_news_outlives_a_crash_at_every_step`: a lookout whose
    /// commons source reads two new replies, killed at `SONNY_CRASH_STEP`
    /// with no chance to clean up. Run on its own, it does nothing.
    #[tokio::test]
    async fn commons_crash_child() {
        let (Some(step), Some(store)) = (
            std::env::var("SONNY_CRASH_STEP").ok(),
            std::env::var_os("SONNY_CRASH_STORE"),
        ) else {
            return;
        };
        let home = PathBuf::from(std::env::var_os("HOME").unwrap());
        let crash = Crash {
            home,
            store: PathBuf::from(store),
        };
        let mut service = crash.service();
        let (sent, mut got) = tokio::sync::mpsc::channel(16);
        let mut runner = sources::Runner::new(beside(&crash.store, ".sources"), sent);
        runner.set(vec![commons_source()]);
        while let Some(said) = got.recv().await {
            match (said, step.as_str()) {
                // Read and on its way to the lookout, held by no one yet.
                (said @ sources::Said::Event(_), "heard") => {
                    service.said(said);
                    std::process::abort();
                }
                (said @ sources::Said::Event(_), _) => service.said(said),
                // Every event in the lookout's memory, none on disk.
                (sources::Said::Kept(_), "kept") => std::process::abort(),
                // On disk, the cursor not moved yet.
                (sources::Said::Kept(_), "written") => {
                    let (kept, answer) = tokio::sync::oneshot::channel();
                    service.said(sources::Said::Kept(kept));
                    assert!(answer.await.unwrap());
                    std::process::abort();
                }
                // On disk, and the cursor moved past it.
                (sources::Said::Kept(kept), _) => {
                    service.said(sources::Said::Kept(kept));
                    let read = crash.cursor().join("read.json");
                    let moved = || {
                        let read: Value = std::fs::read_to_string(&read)
                            .ok()
                            .and_then(|text| serde_json::from_str(&text).ok())
                            .unwrap_or_default();
                        read["t"]["posts"] == 3
                    };
                    let until = Instant::now() + Duration::from_secs(10);
                    while !moved() && Instant::now() < until {
                        tokio::time::sleep(Duration::from_millis(10)).await;
                    }
                    assert!(moved());
                    std::process::abort();
                }
            }
        }
        panic!("the commons source stopped");
    }

    #[test]
    fn commons_news_outlives_a_crash_at_every_step() {
        // (step, whether the subscriber's file holds the replies after the
        // crash, whether the commons tells them again)
        let steps = [
            ("heard", false, true),
            ("kept", false, true),
            ("written", true, true),
            ("marked", true, false),
        ];
        for (step, kept, again) in steps {
            let root =
                std::env::temp_dir().join(format!("sonny-crash-{step}-{}", std::process::id()));
            let _ = std::fs::remove_dir_all(&root);
            let crash = Crash {
                home: root.join("home"),
                store: root.join("state.sqlite"),
            };
            let post = |by: &str, arguments: Value| {
                crate::commons::call(&crash.home, by, None, None, &arguments).unwrap()
            };
            post(
                "calc.lead",
                json!({"post": {"name": "t", "title": "T", "text": "x"}}),
            );
            // The first look tells none of the past.
            let first = crate::commons::news(&crash.home, &crash.cursor());
            assert!(first.lines.is_empty());
            assert!(first.mark_read());
            post("b.y", json!({"reply": {"thread": "t", "text": "one"}}));
            post("c.z", json!({"reply": {"thread": "t", "text": "two"}}));
            let child = std::process::Command::new(std::env::current_exe().unwrap())
                .args([
                    "lookout::tests::commons_crash_child",
                    "--exact",
                    "--nocapture",
                ])
                .env("HOME", &crash.home)
                .env("SONNY_CRASH_STEP", step)
                .env("SONNY_CRASH_STORE", &crash.store)
                .output()
                .unwrap();
            assert!(
                !child.status.success(),
                "{step}: the child ran to the end: {}",
                String::from_utf8_lossy(&child.stdout)
            );
            assert!(
                std::os::unix::process::ExitStatusExt::signal(&child.status) == Some(libc::SIGABRT),
                "{step}: {:?} {}",
                child.status,
                String::from_utf8_lossy(&child.stderr)
            );
            // Restarted: what the subscriber held, then what the commons
            // reads again, which it holds once.
            let mut lookout = crash.lookout();
            assert_eq!(lookout.heard.len(), if kept { 2 } else { 0 }, "{step}");
            let news = crate::commons::news(&crash.home, &crash.cursor());
            assert_eq!(news.lines.len(), if again { 2 } else { 0 }, "{step}");
            for line in &news.lines {
                lookout.heard_from(&sources::Event::parse("commons", line, epoch_ms()).unwrap());
            }
            let mut ids: Vec<_> = (lookout.heard.iter())
                .map(|heard| heard.event.id.clone().unwrap())
                .collect();
            ids.sort();
            assert_eq!(ids, ["t/1", "t/2"], "{step}");
            let _ = std::fs::remove_dir_all(&root);
        }
    }

    #[tokio::test]
    async fn a_source_waits_for_what_lookouts_hold_to_be_written() {
        let root = std::env::temp_dir().join(format!("sonny-kept-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        let crash = Crash {
            home: root.join("home"),
            store: root.join("state.sqlite"),
        };
        let mut service = crash.service();
        let event = |text: &str| {
            sources::Event::parse(
                "commons",
                &json!({"id": text, "topic": "t", "text": text}).to_string(),
                1,
            )
            .unwrap()
        };
        service.said(sources::Said::Event(Box::new(event("t/1"))));
        // Asked, it writes what it holds at once and answers once written.
        let (kept, answer) = tokio::sync::oneshot::channel();
        service.said(sources::Said::Kept(kept));
        assert!(answer.await.unwrap());
        let written: Value =
            serde_json::from_str(&std::fs::read_to_string(crash.held()).unwrap()).unwrap();
        assert_eq!(written["events"][0]["id"], "t/1");
        // Said again, as after a crash before the cursor moved, it is held once.
        service.said(sources::Said::Event(Box::new(event("t/1"))));
        assert_eq!(service.lookouts["calc.lead"].heard.len(), 1);
        // A write that fails says so, and is tried again with the next.
        std::fs::remove_file(crash.held()).unwrap();
        std::fs::create_dir(crash.held()).unwrap();
        service.said(sources::Said::Event(Box::new(event("t/2"))));
        let (kept, answer) = tokio::sync::oneshot::channel();
        service.said(sources::Said::Kept(kept));
        assert!(!answer.await.unwrap());
        std::fs::remove_dir(crash.held()).unwrap();
        let (kept, answer) = tokio::sync::oneshot::channel();
        service.said(sources::Said::Kept(kept));
        assert!(answer.await.unwrap());
        let written: Value =
            serde_json::from_str(&std::fs::read_to_string(crash.held()).unwrap()).unwrap();
        assert_eq!(written["events"].as_array().unwrap().len(), 2);
        let _ = std::fs::remove_dir_all(&root);
    }
}
