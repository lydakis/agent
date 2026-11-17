//! `sonny lookout`: one plain pipe from sources to subscribers. Every
//! source says what happened in the one envelope, `sources::Event`:
//! script sources the person set up, the commons, and the agent source,
//! what happens in the tasks a subscriber watches (`activity`). The
//! lookout judges none of it: it matches each subscription's filter,
//! drops what it holds already, and delivers each event to its
//! subscriber at once, as a steer, or at the subscription's digest. Each
//! note says each event under its header, `[source · topic · from · time]`; the
//! person's words in a task go as the daemon quotes them, never as the
//! lookout says them.
//!
//! It knows no leads: Sonny subscribes each lead to its tasks
//! (`subs::Sub::lead`), and any bot can subscribe to more. One lookout
//! watches a daemon, for every subscriber on it, started by the first
//! Sonny to open; the daemon is unchanged: this follows every bot and
//! submits to the subscribers like any client. The tools Sonny answers
//! are another companion's (`tool_host`), which signals it to read the
//! subscription files a call wrote (`reread_now`).
pub(crate) mod activity;
pub(crate) mod memory;
pub(crate) mod pattern;
pub(crate) mod sources;
pub(crate) mod subs;

use agent_client::Client;
use serde_json::{Value, json};
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

/// How long after a note is refused the next may go, even a steer.
const WAKE_GAP: Duration = Duration::from_secs(120);
/// How often the lookout looks at what waits: a note goes as soon as the
/// last one landed, and what came meanwhile goes in the next.
pub(crate) const TICK: Duration = Duration::from_secs(1);
/// Each subscriber's notes are one group, as Prometheus Alertmanager
/// groups alerts, with `group_wait` 0 and this `group_interval`: an
/// event goes at once when no note went for this long, and what comes
/// within it after a note goes together once it passes, so many agents
/// at once are a note every so often, not one each. A steer waits for
/// none of it.
pub(crate) const GROUP_INTERVAL: Duration = Duration::from_secs(2);
/// How often a lookout with events waiting looks whether its group
/// interval ended.
const NUDGE: Duration = Duration::from_millis(200);
/// How often subscription files are read again.
pub(crate) const REREAD: Duration = Duration::from_secs(15);
/// How long the daemon may be unreachable before the lookout exits: a
/// client starts another with the next daemon.
const GONE_AFTER: Duration = Duration::from_secs(30);
/// How many events wait for a subscriber unless `sonny.lookout_waiting`
/// says: kept on disk with what it is told, past them newer ones are not
/// kept, and its next note says so.
const WAITING: usize = 1024;

/// The origin every note of the lookout's goes with: the daemon says
/// "sent by lookout", never the person, and Sonny folds them by it.
pub const ORIGIN: &str = "lookout";

/// What `sonny.lookout_waiting` says now, 0 for no limit; and, read with
/// it, the most notes a margin holds. Settings that do not read keep the
/// defaults.
fn waiting() -> usize {
    let Ok(sonny) = crate::kept::config().map(|config| config.sonny) else {
        crate::margin::set_limits(
            agent_client::config::MARGIN_NOTES,
            agent_client::config::MARGIN_NOTE_LINES,
        );
        return WAITING;
    };
    crate::margin::set_limits(sonny.margin_notes, sonny.margin_note_lines);
    usize::try_from(sonny.lookout_waiting).unwrap_or(usize::MAX)
}

/// What `sonny.lookout_trailer` says now, read as each note goes: what
/// ends every note, the same for every subscriber and every source.
fn trailer() -> String {
    match crate::kept::config() {
        Ok(config) => config.sonny.lookout_trailer,
        Err(_) => agent_client::config::LOOKOUT_TRAILER.to_owned(),
    }
}

/// `note` with `trailer` under it, as a `[lookout]` line: none when empty.
fn trailed(note: String, trailer: &str) -> String {
    match trailer.trim() {
        "" => note,
        trailer => format!("{note}\n[lookout]\n{trailer}"),
    }
}

/// Events from sources a subscriber's full lookout did not keep: how many,
/// from where and when, for its next note to say.
#[derive(Clone, Debug, Default, PartialEq)]
struct Unkept {
    count: u64,
    sources: std::collections::BTreeSet<String>,
    first_ms: u64,
    last_ms: u64,
}

impl Unkept {
    fn add(&mut self, source: &str, at_ms: u64) {
        if self.count == 0 {
            self.first_ms = at_ms;
        }
        self.count += 1;
        self.sources.insert(source.to_owned());
        self.first_ms = self.first_ms.min(at_ms);
        self.last_ms = self.last_ms.max(at_ms);
    }

    fn merge(&mut self, other: Unkept) {
        if other.count == 0 {
            return;
        }
        if self.count == 0 {
            *self = other;
            return;
        }
        self.count += other.count;
        self.sources.extend(other.sources);
        self.first_ms = self.first_ms.min(other.first_ms);
        self.last_ms = self.last_ms.max(other.last_ms);
    }

    fn to_json(&self) -> Value {
        json!({"count": self.count, "sources": self.sources, "first_ms": self.first_ms,
            "last_ms": self.last_ms})
    }

    fn from_json(kept: &Value) -> Unkept {
        Unkept {
            count: kept["count"].as_u64().unwrap_or(0),
            sources: (kept["sources"].as_array().into_iter().flatten())
                .filter_map(|name| Some(name.as_str()?.to_owned()))
                .collect(),
            first_ms: kept["first_ms"].as_u64().unwrap_or(0),
            last_ms: kept["last_ms"].as_u64().unwrap_or(0),
        }
    }

    /// The note's line for them, `now_ms` being when it goes; none when
    /// every event was kept.
    fn line(&self, waiting: usize, now_ms: u64) -> Option<String> {
        if self.count == 0 {
            return None;
        }
        let names: Vec<&str> = self.sources.iter().map(String::as_str).collect();
        let from = match names.as_slice() {
            [one] => one.to_string(),
            [rest @ .., last] => format!("{} and {last}", rest.join(", ")),
            [] => "your sources".to_owned(),
        };
        let ago = |at_ms: u64| match now_ms.saturating_sub(at_ms) / 60_000 {
            0 => "less than a minute ago".to_owned(),
            1 => "1 minute ago".to_owned(),
            minutes => format!("{minutes} minutes ago"),
        };
        let (first, last) = (ago(self.first_ms), ago(self.last_ms));
        let when = match first == last {
            true => first,
            false => format!("between {first} and {last}"),
        };
        let (count, them) = match self.count {
            1 => ("1 more event".to_owned(), "it was"),
            n => (format!("{n} more events"), "they were"),
        };
        Some(format!(
            "{count} from {from} came {when}, and {them} not kept: {waiting} were waiting for you already"
        ))
    }
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

/// An agent whose message a turn carries, by name and id when known.
type Sender = (String, Option<i64>);
/// A sender a failed turn's message goes back to, and the turn.
struct Sent {
    from: String,
    id: Option<i64>,
    bot: String,
    turn: i64,
    status: String,
    error: String,
    /// Who asked for the turn, as its start named them: the sender's
    /// filters hear its end with the asker the end's event carries.
    asker: Option<Asked>,
    /// Whether a turn took the message in: one that ended in line is no
    /// event of the tasks source, so only its bounce tells its sender.
    taken: bool,
}

/// Who a message that started a turn came from: a bot, the origin it
/// went with, and whether a lookout note sent it, whose turn's end is no
/// event.
#[derive(Clone)]
struct Asked {
    from: Option<String>,
    origin: Option<String>,
    note: bool,
}

/// A bounce: the agent and turn that did not carry a message out, and who
/// sent it.
type Bounce = (String, i64, String);

/// The origin of a note telling an agent that a turn its message started
/// or joined ended without being carried out.
pub const BOUNCE_ORIGIN: &str = "lookout.bounce";

/// FNV-1a, 64-bit: the same note twice is one submit.
fn digest(text: &str) -> String {
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    for byte in text.bytes() {
        hash = (hash ^ u64::from(byte)).wrapping_mul(0x0100_0000_01b3);
    }
    format!("{hash:016x}")
}

/// Tell `to` that `bot`'s turn its message started or joined ended
/// without being carried out (`error`): a steer, read in its next model
/// call or waking it when idle, as a trigger's failure is told. Its
/// request id names the turn and who is told, so it is told once.
async fn tell_bounced(
    client: &Client,
    (to, to_id): (&str, Option<i64>),
    (bot, turn): (&str, i64),
    error: &str,
) -> agent_client::Result<()> {
    let prompt = bounce_note(bot, turn, error);
    // Bounded whatever the names' lengths, as request ids are.
    let request_id = format!("bounce.{turn}.{}", digest(&format!("{bot}\n{turn}\n{to}")));
    let mut submit = json!({"bot": to, "request_id": request_id,
        "prompt": prompt, "delivery": "steer", "origin": BOUNCE_ORIGIN});
    if let Some(id) = to_id {
        submit["bot_id"] = json!(id);
    }
    client.request("submit", submit).await.map(drop)
}

/// The words of a bounce: whose turn, and how it ended.
fn bounce_note(bot: &str, turn: i64, error: &str) -> String {
    format!("[your message to {bot} was not carried out: turn {turn} ended {error}]")
}

/// How far back in a task's history a turn's reply, or a call, is looked
/// for.
const REPLY_NODES: usize = 30;

/// The last thing the task said in its turn `turn`, read back from where
/// the turn left its history, its checkpoint `node`, not from wherever it
/// has moved on to.
async fn reply_text(
    client: &Client,
    task: &str,
    turn: i64,
    node: Option<i64>,
    completed: bool,
) -> Option<String> {
    // Anthropic tasks keep calls as blocks inside a message.
    let said = |read: &Value| {
        (read["items"].as_array().into_iter())
            .flatten()
            .flat_map(|entry| crate::responses(&entry["item"]))
            .find(|item| item["role"] == "assistant" && !crate::text(item).trim().is_empty())
            .map(|item| crate::text(&item))
    };
    // A turn that completed ended on its reply, its checkpoint: one read,
    // where a busy fleet's every read waits its turn.
    if completed
        && let Some(node) = node
        && let Ok(read) = (client)
            .request("history_items", json!({"bot": task, "nodes": [node]}))
            .await
        && let Some(text) = said(&read)
    {
        return Some(text);
    }
    let page = client
        .request(
            "history_nodes",
            json!({"bot": task, "limit": REPLY_NODES, "oldest_first": false, "from": node}),
        )
        .await
        .ok()?;
    let nodes: Vec<i64> = (page["nodes"].as_array().into_iter())
        .flatten()
        .filter(|n| n["turn"].as_i64() == Some(turn))
        .filter_map(|n| n["node"].as_i64())
        .collect();
    let read = client
        .request("history_items", json!({"bot": task, "nodes": nodes}))
        .await
        .ok()?;
    Some(said(&read).unwrap_or_default())
}

/// Who asked for a task's turn, as its prompt records it: the bot whose
/// turn sent it, and the client or person a client named.
async fn asker(client: &Client, task: &str, turn: i64) -> Option<(Option<String>, Option<String>)> {
    let read = client
        .request("prompts", json!({"bot": task, "turn": turn, "bytes": 1}))
        .await
        .ok()?;
    let prompt = &read["prompts"][0];
    let from = prompt["from"]["bot"].as_str().map(str::to_owned);
    let origin = (prompt["origin"].as_str())
        .or(prompt["person"].as_str())
        .map(str::to_owned);
    Some((from, origin))
}

/// A call's tool and arguments, as `bot`'s history keeps them: at `node`,
/// or among its latest nodes.
async fn find_call(
    client: &Client,
    bot: &str,
    call: &str,
    node: Option<i64>,
) -> Option<(Option<String>, String)> {
    let nodes: Vec<i64> = match node {
        Some(node) => vec![node],
        None => {
            let page = client
                .request("history_nodes", json!({"bot": bot, "limit": REPLY_NODES}))
                .await
                .ok()?;
            (page["nodes"].as_array().into_iter().flatten())
                .filter_map(|n| n["node"].as_i64())
                .collect()
        }
    };
    let read = client
        .request("history_items", json!({"bot": bot, "nodes": nodes}))
        .await
        .ok()?;
    // Anthropic tasks keep calls as blocks inside a message.
    let item = (read["items"].as_array().into_iter().flatten())
        .flat_map(|entry| crate::responses(&entry["item"]))
        .find(|item| item["type"] == "function_call" && item["call_id"] == call)?;
    let arguments = item["arguments"].as_str()?.to_owned();
    Some((item["name"].as_str().map(str::to_owned), arguments))
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
    if let Some((_, arguments)) = find_call(client, &bot, &call, None).await {
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

/// What a pending event of the agent source needs read: the words written,
/// the reply a turn ended with and who asked for it, or the call waiting.
async fn read(client: &Client, pending: &activity::Pending) -> activity::Read {
    use activity::{Kind, Read};
    let (task, turn) = (pending.task.as_str(), pending.turn);
    match &pending.kind {
        Kind::Words { from, node, brief } => {
            let text = match node {
                Some(node) => node_text(client, task, *node).await,
                None => client
                    .request(
                        "prompts",
                        json!({"bot": task, "turn": turn, "bytes": sources::TEXT_BYTES}),
                    )
                    .await
                    .ok()
                    .and_then(|read| Some(read["prompts"][0]["text"].as_str()?.to_owned()))
                    .unwrap_or_default(),
            };
            // Words with no sender that repeat the subscriber's own brief
            // are a client's sending of it, not the person's.
            let someone = match (from == activity::PERSON, brief) {
                (true, Some(brief)) if !text.trim().is_empty() => {
                    text.trim() == node_text(client, task, *brief).await.trim()
                }
                _ => false,
            };
            Read::Words { text, someone }
        }
        Kind::Reply {
            node,
            completed,
            asked_by,
            ..
        } => {
            let text =
                (reply_text(client, task, turn, *node, *completed).await).unwrap_or_default();
            let asker = match asked_by {
                Some(_) => None,
                None => asker(client, task, turn).await,
            };
            Read::Reply { text, asker }
        }
        Kind::Call { call, name, node } => {
            let found = find_call(client, task, call, *node).await;
            let name = (found.as_ref().and_then(|(name, _)| name.clone())).or_else(|| name.clone());
            let arguments = found.as_ref().map(|(_, arguments)| arguments.as_str());
            let text = activity::call_text(name.as_deref(), arguments);
            Read::Call { name, text }
        }
    }
}

/// What a pending event is built from when its read never came: no words,
/// no reply, who asked unknown.
fn unread(pending: &activity::Pending) -> activity::Read {
    use activity::{Kind, Read};
    match &pending.kind {
        Kind::Words { .. } => Read::Words {
            text: String::new(),
            someone: false,
        },
        Kind::Reply { .. } => Read::Reply {
            text: String::new(),
            asker: None,
        },
        Kind::Call { name, .. } => Read::Call {
            name: name.clone(),
            text: activity::call_text(name.as_deref(), None),
        },
    }
}

/// How long reading words may take before the event goes without them, as
/// Codex bounds a tool call (60 s unless configured): a read the daemon
/// never answers holds no task's end (#388).
const READ_DEADLINE: Duration = Duration::from_secs(60);

/// `pending` read as `read` reads it, or by `READ_DEADLINE` without it.
async fn read_in_time(client: &Client, pending: &activity::Pending) -> activity::Read {
    within(READ_DEADLINE, read(client, pending), unread(pending)).await
}

/// What `read` gives within `deadline`, else `late`.
async fn within<T>(deadline: Duration, read: impl Future<Output = T>, late: T) -> T {
    tokio::time::timeout(deadline, read).await.unwrap_or(late)
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
    let written = crate::named::me()
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
        waiting: waiting(),
        records: HashMap::new(),
        made: Made::default(),
        subscribers: HashMap::new(),
        files: subs::Read::default(),
        read_at: HashMap::new(),
        socket: settings.socket.clone(),
        lookouts: HashMap::new(),
        log: PathBuf::from(log),
        seated: 0,
        live: false,
        replay_over: false,
        cursor: None,
        saved: None,
        fires: HashMap::new(),
        senders: HashMap::new(),
        askers: HashMap::new(),
        resume: marks(&settings.store).0,
        replayed: None,
        replay: marks(&settings.store).1,
        kept: told_marks(&settings.store),
        in_flight: HashMap::new(),
        telling: HashMap::new(),
        bouncing: HashMap::new(),
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

/// The connections words are read on, beside the one the stream and the
/// notes go on. The daemon takes one read at a time from a session, in
/// order, as a database serves one connection's queries, so a lookout
/// reading every reply of a busy fleet on one connection would read them
/// one by one, its notes queued behind; it keeps a pool, as a database
/// client does, one connection for each reader the daemon keeps by
/// default. They last as long as the stream's connection: when it drops,
/// the next takes new ones.
struct Readers {
    pool: Vec<(Arc<Client>, agent_client::Events)>,
    /// The stream's connection, when no other could be had.
    main: Arc<Client>,
    next: std::sync::atomic::AtomicUsize,
}

impl Readers {
    async fn connect(socket: &Path, main: Arc<Client>) -> Self {
        let count = std::thread::available_parallelism().map_or(1, |n| n.get());
        let mut pool = Vec::with_capacity(count);
        for _ in 0..count {
            match Client::connect(socket).await {
                Ok(connected) => pool.push(connected),
                Err(_) => break,
            }
        }
        Self {
            pool,
            main,
            next: Default::default(),
        }
    }

    /// The connection for the next read, in turn.
    fn next(&self) -> Arc<Client> {
        if self.pool.is_empty() {
            return self.main.clone();
        }
        let at = (self.next).fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        self.pool[at % self.pool.len()].0.clone()
    }
}

/// A failed fire's note under way.
struct Telling {
    note: tokio::task::AbortHandle,
    /// The cursors of the turn's start and end: the replay that tells it
    /// again starts before the one and reads the other as news.
    began: Option<i64>,
    ended: Option<i64>,
}

/// Every subscriber's lookout on one daemon.
struct Service {
    store: PathBuf,
    /// How many events from sources wait for one subscriber
    /// (`sonny.lookout_waiting`), as the settings said at the last read.
    waiting: usize,
    /// The sources the person set up, running.
    runner: sources::Runner,
    /// Every bot as the daemon lists it, kept up from the stream: who made
    /// whom, for subscriptions to tasks of a bot other than the lead.
    records: HashMap<String, Value>,
    /// Who made whom among `records`.
    made: Made,
    /// Subscriptions by subscriber, as their files held them at the last
    /// read.
    subscribers: HashMap<String, Vec<subs::Sub>>,
    /// The subscribers' files as last read, so a look reads only those
    /// that changed.
    files: subs::Read,
    /// Which revision of each subscriber's file the lookout took and when,
    /// kept beside the store ([`subs::taken`]) for its `follows`
    /// listing to show.
    read_at: HashMap<String, subs::Took>,
    socket: PathBuf,
    /// By subscriber name.
    lookouts: HashMap<String, Lookout>,
    log: PathBuf,
    /// Lookouts made so far: each one's own number, so words read after
    /// its subscriber was deleted reach no later subscriber of the name.
    seated: u64,
    /// The stream's replay is over: what happens now is news.
    live: bool,
    /// The stream reached the present (`follow_live`): every event it held
    /// for a lookout has come.
    replay_over: bool,
    /// The newest event seen live, and the newest saved beside the store:
    /// a lookout started again tells what came after it.
    cursor: Option<i64>,
    /// What was saved there last, its subscribers' own marks too.
    saved: Option<String>,
    /// Each subscriber's own mark, as the last lookout kept it, until its
    /// lookout is seated (`mark_told`).
    kept: HashMap<String, i64>,
    /// Turns triggers' fires sent, by agent and turn, each with its
    /// trigger's generation: one that fails is told (`trigger::tell_failed`).
    fires: HashMap<(String, i64), String>,
    /// The agents whose messages started or joined each turn, by bot and
    /// turn, for whom a turn that fails is a note (`bounced`).
    senders: HashMap<(String, i64), Vec<Sender>>,
    /// Who asked for each turn whose senders wait on its end.
    askers: HashMap<(String, i64), Asked>,
    /// Failed fires' turns whose note is under way, by agent and turn: held
    /// so a restart or a reconnect before the daemon takes the note replays
    /// the turn and tells it again, not never.
    telling: HashMap<(String, i64), Telling>,
    /// Bounces under way, by agent, turn and who is told: held as a failed
    /// fire's note is, so one the daemon has not taken is told again.
    bouncing: HashMap<Bounce, Telling>,
    /// Where the last lookout left off: events after it are news.
    resume: Option<i64>,
    /// How far a first replay got, with no lookout before it to leave off:
    /// one a lag cuts short goes on from there, as a consumer goes on
    /// from its last offset, rather than reading the whole log again.
    replayed: Option<i64>,
    /// Where the last lookout's stream starts again: before the oldest turn
    /// still under way then, so what each said before it ends is known.
    replay: Option<i64>,
    /// Turns of bots a lookout hears that began and have not ended, by bot
    /// and turn: the event each began at. The stream starts again before
    /// the oldest, as a consumer commits its offset only up to its oldest
    /// record still in work: who asked, what the subscriber waits on and a
    /// task's resolving are replayed rather than lost.
    in_flight: HashMap<(String, i64), i64>,
    /// Which lookouts hear each bot's events, by bot: its own, and those
    /// that watch it.
    watchers: HashMap<String, Vec<String>>,
    /// Not connected yet since it started: its stream starts again where
    /// the last lookout's would.
    started: bool,
    /// The `follows` and `commons` calls under way, by bot, turn and
    /// ID, whose end means the files the lookout reads changed.
    rewriting: HashSet<(String, i64, String)>,
}

/// Who made whom among the bots the daemon lists, kept as the listing
/// changes: a lineage grows through what each bot made, as a tree's
/// child lists are walked, rather than through every bot listed again
/// for each lookout and each generation.
#[derive(Debug, Default)]
struct Made {
    /// By maker's name, listed or not: what it made, and whether each is
    /// a side chat, which is no one's task.
    by: HashMap<String, Vec<(String, bool)>>,
    /// The leads listed.
    leads: HashSet<String>,
}

impl Made {
    fn of(records: &HashMap<String, Value>) -> Self {
        let mut made = Made::default();
        for (name, record) in records {
            made.add(name, record);
        }
        made
    }

    fn add(&mut self, name: &str, record: &Value) {
        if let Some(by) = record["created_by"].as_str() {
            let side = !record["expire_ms"].is_null();
            self.by
                .entry(by.to_owned())
                .or_default()
                .push((name.to_owned(), side));
        }
        if crate::project::lead_project(name).is_some() {
            self.leads.insert(name.to_owned());
        }
    }

    fn remove(&mut self, name: &str, record: &Value) {
        if let Some(by) = record["created_by"].as_str()
            && let Some(made) = self.by.get_mut(by)
        {
            made.retain(|(made, _)| made != name);
            if made.is_empty() {
                self.by.remove(by);
            }
        }
        self.leads.remove(name);
    }

    /// Every bot that one of `made` made, and what those made; side chats
    /// are no one's tasks.
    fn grow(&self, made: &mut HashSet<String>) {
        let mut next: Vec<String> = made.iter().cloned().collect();
        while let Some(by) = next.pop() {
            for (name, _) in (self.by.get(&by).into_iter().flatten()).filter(|(_, side)| !side) {
                if made.insert(name.clone()) {
                    next.push(name.clone());
                }
            }
        }
    }
}

/// Where a lookout's answers from the daemon go: its subscriber, its own
/// number, and whether the daemon took its note, or the code it refused
/// it with.
type Woken = tokio::sync::mpsc::Sender<(String, u64, Result<(), String>)>;

/// Whether the bot listed under a name is another than the one seated.
fn replaced(seated: &Value, listed: &Value) -> bool {
    let id = |record: &Value| record["bot_id"].as_i64();
    id(seated).is_some() && id(listed).is_some() && id(seated) != id(listed)
}

/// Where the stream starts again: before the oldest of where the last
/// lookout left off and the oldest turn still under way; with neither, as
/// far as a first replay got, or the start.
fn follow_after(resume: Option<i64>, replay: Option<i64>, replayed: Option<i64>) -> i64 {
    ([resume, replay].into_iter().flatten().min())
        .or(replayed)
        .unwrap_or(0)
}

impl Service {
    async fn watch(
        &mut self,
        client: Arc<Client>,
        mut events: agent_client::Events,
        from_sources: &mut tokio::sync::mpsc::Receiver<sources::Said>,
    ) {
        let readers = Readers::connect(&self.socket, client.clone()).await;
        // Subscriptions written after this are left as they are: a bot
        // made since, which the listing may miss, may have written them.
        let listed_at = std::time::SystemTime::now();
        let (bots, complete) = listed(&client).await;
        self.records = (bots.iter())
            .filter_map(|b| Some((b["name"].as_str()?.to_owned(), b.clone())))
            .collect();
        self.made = Made::of(&self.records);
        // Files renamed before anything rewrites them: a writer reads a
        // file as `Sub` and would drop a key not yet renamed.
        subs::agents_files(&self.store);
        // What Sonny's walk of the repositories inside agents' folders gave
        // them leaves their sandboxes, once, from a whole listing.
        if complete {
            crate::approvals::nested_gone(client.clone(), &bots, &self.store);
        }
        subs::goals_gone(&self.store);
        if let Some(home) = std::env::var_os("HOME").map(PathBuf::from) {
            crate::commons::woken_gone(&home);
            // Old commons posts name their authors' projects, once, from
            // a whole listing, off the loop: each thread is rewritten.
            if complete {
                let records: HashMap<String, Value> = (self.records.iter())
                    .map(|(name, bot)| {
                        let kept = json!({"name": name, "bot_id": bot["bot_id"],
                            "created_by": bot["created_by"], "created_by_id": bot["created_by_id"]});
                        (name.clone(), kept)
                    })
                    .collect();
                tokio::task::spawn_blocking(move || crate::commons::projects(&home, &records));
            }
        }
        crate::git::drop_snapshot_indexes(&self.store);
        let revisions = subs::revisions(&self.store);
        self.subscribers = subs::all(&self.store).into_iter().collect();
        // Bots deleted while it was away: their subscriptions end now, as
        // ones it hears deleted do. Only from a whole listing.
        if complete && self.unheld(listed_at) {
            self.subscribers = subs::all(&self.store).into_iter().collect();
        }
        self.read_at.clear();
        subs::take(&mut self.read_at, &revisions, epoch_ms());
        subs::took(&self.store, &self.read_at);
        for bot in &bots {
            self.seat(bot);
        }
        self.resubscribe(true);
        for lookout in self.lookouts.values_mut() {
            // The replay's turns were news then; the live ones are now.
            lookout.live = false;
            lookout.replay_over = false;
            lookout.lineage.insert(lookout.lead.clone());
        }
        let mut live = false;
        self.live = false;
        self.replay_over = false;
        // News from before the oldest not told yet, as a restart would: a
        // message whose read the dropped connection cut short is read
        // again. What came before was told, or seen live by this lookout,
        // so the stream starts after it, or before the oldest turn still
        // under way, whose events are history: never the whole log again.
        // Only a lookout with no cursor yet reads it all, once: one a lag
        // cuts short goes on from where it got to.
        self.resume = self.floor().or(self.resume);
        self.mark_told();
        self.ended_unseen(&bots);
        // The listing is in `records` now: let it go rather than hold a
        // second copy of every bot for as long as the lookout watches.
        drop(bots);
        let replay = match self.started {
            true => self.replay,
            false => self.replay_floor(),
        };
        self.started = false;
        let after = follow_after(self.resume, replay, self.replayed);
        let follow = json!({"bot": "*", "after": after, "kinds": KINDS});
        if let Err(error) = client.request("follow", follow).await {
            println!(
                "{}",
                json!({"event": "follow_refused", "error": error.code, "after": after})
            );
            return;
        }
        for lookout in self.lookouts.values_mut() {
            // Reads a dropped connection left under way answer the old
            // channel, never this one.
            lookout.reading.clear();
        }
        // Notes under way answer the old connection: the replay tells them
        // again.
        for telling in
            (self.telling.drain().map(|(_, t)| t)).chain(self.bouncing.drain().map(|(_, t)| t))
        {
            telling.note.abort();
        }
        let (told, mut taken) = tokio::sync::mpsc::channel(64);
        let (bounce_told, mut bounce_taken) = tokio::sync::mpsc::channel::<Bounce>(64);
        let (done, mut results) = tokio::sync::mpsc::channel::<(
            String,
            u64,
            Instant,
            activity::Pending,
            activity::Read,
        )>(64);
        // What the daemon answered a note or a status check: beside the
        // stream, so no call to it ever holds the events up.
        let (woken, mut woke) = tokio::sync::mpsc::channel(64);
        for lookout in self.lookouts.values_mut() {
            // Notes a dropped connection left under way answer the old
            // channel: what they carried is news again.
            lookout.reconnected();
        }
        // What waits is looked at often; subscription files less often.
        let mut tick = tokio::time::interval(TICK);
        // What waits out a group interval goes soon after it ends, not a tick
        // later.
        let mut nudge = tokio::time::interval(NUDGE);
        nudge.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        let mut reread: Option<Instant> = None;
        loop {
            // The lookouts whose news may have changed: those an event
            // reached, or all of them.
            let mut touched: Option<Vec<String>> = None;
            let mut overdue: Vec<(String, Vec<subs::Sub>)> = Vec::new();
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
                    // A `follows` or `commons` call the tool
                    // host answered counts from the next event on: its end
                    // is in the stream before any event that follows it.
                    if self.rewrote(&event) {
                        reread = Some(Instant::now());
                        self.runner.reload();
                        self.runner.wanted_by(&self.subscribers);
                        self.reread();
                        self.runner.poke();
                    }
                    let cursor = event["cursor"].as_i64();
                    let news =
                        live || cursor.is_some_and(|at| self.resume.is_some_and(|seen| at > seen));
                    // What happened since the lookout last watched live, as
                    // a task that ended while it was down, is news now.
                    if !live && cursor.is_some_and(|at| self.resume.is_some_and(|seen| at > seen)) {
                        live = true;
                        self.go_live();
                    }
                    if live && cursor > self.cursor {
                        self.cursor = cursor;
                    }
                    if !live && self.resume.is_none() && cursor > self.replayed {
                        self.replayed = cursor;
                    }
                    if let (Some("created" | "forked"), Some(name)) =
                        (event["event"].as_str(), event["bot"].as_str())
                    {
                        let mut record = event["data"].clone();
                        record["name"] = json!(name);
                        if let Some(had) = self.records.insert(name.to_owned(), record.clone()) {
                            self.made.remove(name, &had);
                        }
                        self.made.add(name, &record);
                        self.seat(&record);
                    }
                    if event["event"] == "deleted"
                        && let Some(name) = event["bot"].as_str()
                    {
                        self.in_flight.retain(|(bot, _), _| bot != name);
                        let had = self.records.remove(name);
                        if let Some(had) = &had {
                            self.made.remove(name, had);
                        }
                        if let Some(lookout) = self.lookouts.remove(name) {
                            lookout.unheld();
                            self.index();
                        }
                        // Only as news: what the replay shows from before the
                        // lookout last watched it saw then.
                        if news {
                            let id = had.as_ref().and_then(|had| had["bot_id"].as_i64());
                            // In force at once: a bot made again under the
                            // name is never heard by what ended.
                            if self.ended_with(name, id) {
                                self.reread();
                            }
                        }
                    }
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
                    // The senders of a turn that failed, or of a message
                    // that ended in line, kept to bounce it back once the
                    // lookouts hear its end.
                    let began = self.began(&event);
                    let bounces = self.sent(&event, live);
                    // Each event goes to the lookouts that hear its bot,
                    // as an informer hands a change to the handlers that
                    // asked for its object; a bot made or deleted, to all.
                    let hearing = self.hearing(&event);
                    self.track(&event, !hearing.is_empty(), cursor);
                    // The replay is over: no subscriber's mark is ahead.
                    if event["event"] == "follow_live" {
                        self.replay_over = true;
                        for lookout in self.lookouts.values_mut() {
                            lookout.caught_up();
                        }
                    }
                    self.hand(&event, &hearing, cursor, live);
                    if let (Some("created" | "forked" | "deleted"), Some(bot)) =
                        (event["event"].as_str(), event["bot"].as_str())
                    {
                        self.index_bot(bot);
                    }
                    // A message another agent sent that a failed turn
                    // never carried out goes back to it, as a bounce: to
                    // each sender whose lookout did not just hear it end.
                    for (to, to_id, bot, turn, error) in self.unheard(bounces) {
                        let (client, told) = (client.clone(), bounce_told.clone());
                        let key = (bot.clone(), turn, to.clone());
                        let note = tokio::spawn(async move {
                            let sent = tell_bounced(&client, (&to, to_id), (&bot, turn), &error).await;
                            // Taken, or refused for good: a dropped
                            // connection's replay tells it again.
                            if sent.is_err_and(|error| error.code == "daemon_disconnected") {
                                return;
                            }
                            let _ = told.send((bot, turn, to)).await;
                        });
                        self.bouncing.insert(key, Telling { note: note.abort_handle(), began, ended: cursor });
                    }
                    // The replay tells who belongs to each subscriber and
                    // each task's current brief; what it holds was news then.
                    if !live {
                        for name in &hearing {
                            let Some(lookout) = self.lookouts.get_mut(name) else { continue };
                            // Nothing in it is read again. A first lookout
                            // reads a turn still queued once it starts;
                            // after another, what was queued up to where it
                            // left off was told then, and is not again.
                            lookout.due.clear();
                            if self.resume.is_none() {
                                lookout.activity.queued.clear();
                            }
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
                Some(key) = bounce_taken.recv() => {
                    self.bouncing.remove(&key);
                }
                // What sources say is news once the stream is live; until
                // then it waits in the channel.
                Some(said) = from_sources.recv(), if self.live => {
                    self.said(said);
                }
                Some((lead, id, started, pending, read)) = results.recv() => {
                    // From a lookout since deleted.
                    if let Some(lookout) = self.lookouts.get_mut(&lead)
                        && lookout.id == id
                    {
                        lookout.read(started, &pending, read);
                    }
                    touched = Some(vec![lead]);
                }
                Some((lead, id, answer)) = woke.recv() => {
                    let mut ended = Vec::new();
                    if let Some(lookout) = self.lookouts.get_mut(&lead)
                        && lookout.id == id
                    {
                        lookout.answered(answer);
                        ended = std::mem::take(&mut lookout.ended);
                    }
                    if !ended.is_empty() {
                        for sub in &ended {
                            self.end_sub(String::as_str(&lead), sub, "its runs were told");
                        }
                        self.reread();
                    }
                    touched = Some(vec![lead]);
                }
                _ = nudge.tick() => {
                    touched = Some((self.lookouts.iter())
                        .filter(|(_, lookout)| !lookout.heard.is_empty())
                        .map(|(name, _)| name.clone())
                        .collect());
                }
                _ = tick.tick() => {
                    let now = Instant::now();
                    self.save_cursor();
                    // What the person or a script wrote since, now and then.
                    if reread.is_none_or(|at| now >= at + REREAD) {
                        reread = Some(now);
                        self.runner.reload();
                        self.runner.wanted_by(&self.subscribers);
                        self.waiting = waiting();
                        for lookout in self.lookouts.values_mut() {
                            lookout.waiting = self.waiting;
                        }
                        self.reread();
                    }
                    for lookout in self.lookouts.values_mut() {
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
                for pending in std::mem::take(&mut lookout.due) {
                    let (client, done) = (readers.next(), done.clone());
                    let (lead, id) = (lookout.lead.clone(), lookout.id);
                    let started = Instant::now();
                    lookout
                        .reading
                        .push((started, pending.task.clone(), pending.order));
                    tokio::spawn(async move {
                        let read = read_in_time(&client, &pending).await;
                        let _ = done.send((lead, id, started, pending, read)).await;
                    });
                }
                lookout.wake(&client, &woken);
                let over = std::mem::take(&mut lookout.over);
                if !over.is_empty() {
                    overdue.push((name, over));
                }
            }
            // Subscriptions whose `until` came between rereads end before
            // any note tells what they heard.
            if !overdue.is_empty() {
                for (lead, over) in std::mem::take(&mut overdue) {
                    for sub in &over {
                        self.end_sub(&lead, sub, "its until came");
                    }
                }
                self.reread();
            }
        }
    }

    /// A turn a trigger's fire sent, noted as it starts; once it ends
    /// failed, while live, its trigger's generation, agent, turn and why,
    /// for the agent to be told. A model that said nothing did not fail.
    fn fired(&mut self, event: &Value, live: bool) -> Option<(String, String, i64, String)> {
        let (bot, turn) = (event["bot"].as_str()?, number(event)?);
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
            "turn_finished" | "message_ended" => {
                let generation = self.fires.remove(&(bot.to_owned(), turn))?;
                let error = data["error"].as_str().unwrap_or("no reason given");
                (live && data["status"] == "failed" && error != "empty_completion")
                    .then(|| (generation, bot.to_owned(), turn, error.to_owned()))
            }
            _ => None,
        }
    }

    /// Lets go of the turns under way that the listing shows ended, or
    /// never running, unseen: but for one carrying another agent's
    /// message, which may still wait in line, ready or held, and whose end
    /// is to bounce. Its finish, if it came while the lookout was away, is
    /// in the replay from its start.
    fn ended_unseen(&mut self, bots: &[Value]) {
        let running: HashMap<&str, i64> = (bots.iter())
            .filter_map(|bot| Some((bot["name"].as_str()?, bot["running_turn"].as_i64()?)))
            .collect();
        let listed: HashSet<&str> = bots.iter().filter_map(|bot| bot["name"].as_str()).collect();
        let senders = &self.senders;
        self.in_flight
            .retain(|key @ (bot, turn), _| match running.get(bot.as_str()) {
                Some(at) => turn >= at,
                None => listed.contains(bot.as_str()) && senders.contains_key(key),
            });
    }

    /// Where the turn or message an event ends began, while it is still
    /// tracked: a message ending in line has no turn, only its number.
    fn began(&self, event: &Value) -> Option<i64> {
        (event["bot"].as_str())
            .zip(number(event))
            .and_then(|(bot, n)| self.in_flight.get(&(bot.to_owned(), n)).copied())
    }

    /// Note where each turn or message whose end is still owed began, so
    /// a restart reads again from there: one a lookout hears, a fire's,
    /// and one carrying another agent's message, so a restart still sees
    /// it end and can bounce it. A message taken into a running turn ends
    /// in line there: its own number has no end of its own to wait for.
    fn track(&mut self, event: &Value, heard: bool, cursor: Option<i64>) {
        let (Some(bot), Some(turn)) = (event["bot"].as_str(), number(event)) else {
            return;
        };
        let key = (bot.to_owned(), turn);
        match event["event"].as_str() {
            Some("steered") => {
                if let Some(message) = event["message"].as_i64() {
                    self.in_flight.remove(&(bot.to_owned(), message));
                }
                if self.senders.contains_key(&key)
                    && let Some(at) = cursor
                {
                    self.in_flight.entry(key).or_insert(at);
                }
            }
            Some("accepted" | "queued")
                if heard || self.fires.contains_key(&key) || self.senders.contains_key(&key) =>
            {
                if let Some(at) = cursor {
                    self.in_flight.entry(key).or_insert(at);
                }
            }
            Some("turn_finished" | "message_ended") => {
                self.in_flight.remove(&key);
            }
            _ => {}
        }
    }

    /// Who to tell that a turn their message started or joined ended
    /// failed or interrupted: each agent whose message it carried, other
    /// than the bot itself and any whose lookout hears the bot already
    /// (a lead hears its task's turn end). Told only of what happens
    /// live; a turn whose start was never seen tells no one.
    fn sent(&mut self, event: &Value, live: bool) -> Vec<Sent> {
        let (Some(bot), Some(turn)) = (event["bot"].as_str(), number(event)) else {
            return Vec::new();
        };
        let data = &event["data"];
        match event["event"].as_str() {
            // A steer's event comes on the turn it joins: its message
            // reached that turn, so what waited on the message's own end
            // goes with it, and the turn's end now tells its sender.
            Some(kind @ ("accepted" | "queued" | "steered")) => {
                if kind == "steered"
                    && let Some(message) = event["message"].as_i64()
                {
                    self.senders.remove(&(bot.to_owned(), message));
                    self.askers.remove(&(bot.to_owned(), message));
                }
                // Bounded: turns whose end was never seen are let go.
                if self.senders.len() >= FIRES || self.askers.len() >= FIRES {
                    self.senders.clear();
                    self.askers.clear();
                }
                // The message that starts a turn names who asked for it.
                if kind != "steered" {
                    let text = |value: &Value| value.as_str().map(str::to_owned);
                    let note =
                        (data["request_id"].as_str()).is_some_and(|id| id.starts_with("lookout-"));
                    let origin = text(&data["origin"]).or(text(&data["person"]));
                    let from = text(&data["from"]["bot"]);
                    let asked = Asked { from, origin, note };
                    self.askers.insert((bot.to_owned(), turn), asked);
                }
                let Some(from) = data["from"]["bot"].as_str().filter(|from| *from != bot) else {
                    return Vec::new();
                };
                let senders = self.senders.entry((bot.to_owned(), turn)).or_default();
                if !senders.iter().any(|(known, _)| known == from) {
                    senders.push((from.to_owned(), data["from"]["bot_id"].as_i64()));
                }
                Vec::new()
            }
            Some("turn_finished" | "message_ended") => {
                let asker = self.askers.remove(&(bot.to_owned(), turn));
                let Some(senders) = self.senders.remove(&(bot.to_owned(), turn)) else {
                    return Vec::new();
                };
                let status = data["status"].as_str().unwrap_or("");
                let error = data["error"].as_str().unwrap_or(status);
                if !live
                    || !matches!(status, "failed" | "interrupted" | "withdrawn")
                    || error == "empty_completion"
                {
                    return Vec::new();
                }
                (senders.into_iter())
                    .map(|(from, id)| Sent {
                        from,
                        id,
                        bot: bot.to_owned(),
                        turn,
                        status: status.to_owned(),
                        error: error.to_owned(),
                        asker: asker.clone(),
                        taken: event["event"] == "turn_finished",
                    })
                    .collect()
            }
            _ => Vec::new(),
        }
    }

    /// The senders to tell, of those `sent` named, once the lookouts have
    /// the turn's end: each but those whose lookout hears it as news, as
    /// a subscription whose filters let this end through tells it. One
    /// that hears only what went well bounces.
    fn unheard(&self, sent: Vec<Sent>) -> Vec<(String, Option<i64>, String, i64, String)> {
        let hears = |sent: &Sent| {
            (self.watchers.get(&sent.bot)).is_some_and(|names| names.contains(&sent.from))
                && (self.lookouts.get(&sent.from)).is_none_or(|lookout| lookout.hears_end(sent))
        };
        (sent.into_iter())
            .filter(|sent| !hears(sent))
            .map(|sent| (sent.from, sent.id, sent.bot, sent.turn, sent.error))
            .collect()
    }

    /// `sent` and `unheard` together, as the event loop runs them around
    /// the lookouts hearing the event.
    #[cfg(test)]
    fn bounced(
        &mut self,
        event: &Value,
        live: bool,
    ) -> Vec<(String, Option<i64>, String, i64, String)> {
        let sent = self.sent(event, live);
        self.unheard(sent)
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

    /// Which lookouts hear each bot: those that watch it.
    fn index(&mut self) {
        let mut watchers: HashMap<String, Vec<String>> = HashMap::new();
        for (name, lookout) in &self.lookouts {
            for bot in &lookout.watched {
                watchers.entry(bot.clone()).or_default().push(name.clone());
            }
        }
        self.watchers = watchers;
    }

    /// One lookout added to the index, the bots it watches, without going
    /// through every other lookout again.
    fn index_lookout(&mut self, name: &str) {
        let Some(lookout) = self.lookouts.get(name) else {
            return;
        };
        for bot in &lookout.watched {
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
    fn reread(&mut self) {
        let revisions = subs::revisions(&self.store);
        let mut read: HashMap<String, Vec<subs::Sub>> =
            self.files.all(&self.store).into_iter().collect();
        // A subscription whose `until` came ends, as an automation does.
        let now_s = (epoch_ms() / 1000) as i64;
        for (subscriber, subs) in read.iter_mut() {
            for sub in subs.iter().filter(|sub| sub.over(now_s)) {
                self.end_sub(subscriber, sub, "its until came");
            }
            subs.retain(|sub| !sub.over(now_s));
        }
        read.retain(|_, subs| !subs.is_empty());
        // Every revision read is kept, one that changes nothing and a
        // file that went included, so the listing knows it was taken.
        let now = epoch_ms();
        if subs::take(&mut self.read_at, &revisions, now) {
            subs::took(&self.store, &self.read_at);
        }
        if read == self.subscribers {
            return;
        }
        // Each file whose subscriptions changed, said once it is read, so a
        // rewritten filter is known to be live before an event it hears comes.
        let changed = (read.iter())
            .filter(|(name, subs)| self.subscribers.get(*name).is_some_and(|was| was != *subs));
        for (name, _) in changed {
            self.write(&json!({"at_ms": now, "lead": name, "reloaded": true}));
        }
        self.subscribers = read;
        self.runner.wanted_by(&self.subscribers);
        let records: Vec<Value> = (self.subscribers.keys())
            .filter_map(|name| self.records.get(name).cloned())
            .collect();
        for record in records {
            self.seat(&record);
        }
        self.resubscribe(false);
    }

    /// End `subscriber`'s subscription `name`, as it says `why`, in its
    /// file and the log; the next read of the files stops it.
    fn end_sub(&self, subscriber: &str, sub: &subs::Sub, why: &str) {
        let removed = subs::end(&self.store, subscriber, sub);
        self.write(
            &json!({"at_ms": epoch_ms(), "lead": subscriber, "ended": sub.name, "why": why,
            "removed": removed.as_ref().is_ok_and(|r| *r), "error": removed.err()}),
        );
    }

    /// Subscribers no bot now listed holds, as after deletes the lookout
    /// was away for: what they held ends. Whether any did.
    fn unheld(&self, listed: std::time::SystemTime) -> bool {
        let mut any = false;
        for name in self.subscribers.keys() {
            let live = (self.records.get(name)).and_then(|bot| bot["bot_id"].as_i64());
            let ended = subs::unheld(&self.store, name, live, listed);
            if ended.as_ref().is_ok_and(|n| *n > 0) || ended.is_err() {
                any = true;
                self.write(&json!({"at_ms": epoch_ms(), "lead": name, "ended": "all",
                    "why": "its subscriber was deleted", "error": ended.err()}));
            }
        }
        any
    }

    /// A bot was deleted: the subscriptions it held end with it, and so
    /// does any that watched only bots by their names, every one of them
    /// gone now, and no source or maker's tasks.
    /// Whether any ended.
    fn ended_with(&self, gone: &str, id: Option<i64>) -> bool {
        let mut any = false;
        if self.subscribers.contains_key(gone) {
            let forgot = subs::forget(&self.store, gone, id);
            if forgot.as_ref().is_ok_and(|n| *n > 0) || forgot.is_err() {
                any = true;
                self.write(&json!({"at_ms": epoch_ms(), "lead": gone, "ended": "all",
                    "why": "its subscriber was deleted", "error": forgot.err()}));
            }
        }
        for (subscriber, subs) in &self.subscribers {
            for sub in subs {
                // A maker's tasks outlive it: one that watches them stays.
                let names: Vec<&String> = sub.agents.iter().collect();
                let watched_gone = names.iter().any(|name| *name == gone)
                    && sub.sources.is_empty()
                    && sub.tasks_of.is_empty()
                    && names
                        .iter()
                        .all(|name| !name.contains('*') && !self.records.contains_key(*name));
                if watched_gone && subscriber != gone {
                    self.end_sub(subscriber, sub, "every bot it watched was deleted");
                    any = true;
                }
            }
        }
        any
    }

    /// Whether `event` ends a call of Sonny's that wrote files the lookout
    /// reads, a `follows` or `commons` call (posting
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
                    && (data["name"] == crate::served::FOLLOWS
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
                held::then(self.store.clone(), move |written| {
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
        let mut lookout = Lookout::new(lead);
        lookout.waiting = self.waiting;
        self.seated += 1;
        lookout.id = self.seated;
        lookout.log = Some(self.log.clone());
        lookout.record = record.clone();
        lookout.lead_id = record["bot_id"].as_i64();
        lookout.live = self.live;
        lookout.replay_over = self.replay_over;
        // What it subscribed to before it was made, or made again.
        // Less what was made for another bot of its name.
        lookout.subs = file.into_iter().filter(|sub| sub.held_by(record)).collect();
        lookout.roots(&self.records, &self.made);
        lookout.restore(held_file(&self.store, lead));
        lookout.counts = subs::counts(&self.store, lead);
        lookout.counts_file = Some(subs::counts_file(&self.store, lead));
        lookout.counting();
        self.lookouts.insert(lead.to_owned(), lookout);
        // Said once a subscriber is watched, so `/status` shows it from
        // the start.
        self.write(&json!({"at_ms": epoch_ms(), "lead": lead, "seated": true}));
        true
    }

    /// A subscriber as `bots` lists it or `created` says it: its lookout,
    /// made once.
    fn seat(&mut self, record: &Value) {
        let Some(lead) = record["name"].as_str() else {
            return;
        };
        let fresh = !self.lookouts.contains_key(lead);
        if !self.place(record) {
            return;
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
            let subs: Vec<subs::Sub> = (subscribers.get(name).into_iter().flatten())
                .filter(|sub| sub.held_by(&lookout.record))
                .cloned()
                .collect();
            if all || subs != lookout.subs {
                lookout.resubscribed(subs);
                lookout.roots(&self.records, &self.made);
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
            // One whose own mark is ahead goes live once the stream passes it.
            lookout.live = lookout.told_to.is_none();
        }
    }

    /// An event to each lookout that hears it. What comes at or before a
    /// subscriber's own mark was told to it already: history to it, as the
    /// replay is to all, though news to another whose mark is further back.
    fn hand(&mut self, event: &Value, hearing: &[String], cursor: Option<i64>, live: bool) {
        // Who made the bot, which its events name.
        let maker = (event["bot"].as_str())
            .and_then(|bot| self.records.get(bot))
            .and_then(|record| record["created_by"].as_str());
        for name in hearing {
            let Some(lookout) = self.lookouts.get_mut(name) else {
                continue;
            };
            if lookout.told_before(cursor, live) {
                lookout.event(event, maker);
                // Nothing in it is read again.
                lookout.due.clear();
                continue;
            }
            lookout.event(event, maker);
            // The oldest event whose news is not told yet: a restart takes
            // it up again from there.
            if live
                && let Some(at) = cursor
                && lookout.untold()
            {
                lookout.from.get_or_insert(at);
            }
        }
    }

    /// Each subscriber's own mark, from what its lookout holds now or what
    /// the last lookout kept: up to it, its news was told. The stream
    /// starts again before the oldest news any of them has not told, and
    /// one whose news waits holds no other back: each hears as news only
    /// what came after its own mark. A mark no further than where the
    /// stream's news starts (`resume`) says nothing more. After `floor`,
    /// which lets go of what was told since.
    fn mark_told(&mut self) {
        let kept = std::mem::take(&mut self.kept);
        let resume = self.resume;
        for (name, lookout) in &mut self.lookouts {
            lookout.told_to = (lookout.mark(self.cursor).max(kept.get(name).copied()))
                .filter(|mark| resume.is_none_or(|resume| *mark > resume));
        }
    }

    /// Up to just before the oldest news a lookout has not told, the
    /// or the oldest failed fire whose
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
            .chain(
                (self.telling.values().chain(self.bouncing.values()))
                    .filter_map(|telling| telling.ended),
            )
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
        // Each subscriber's own mark, where it is past where the stream
        // starts again: in steady state, none.
        let mut told: Vec<(&String, i64)> = (self.lookouts.iter())
            .filter_map(|(name, lookout)| Some((name, lookout.mark(self.cursor)?)))
            .filter(|(_, mark)| *mark > cursor)
            .collect();
        told.sort_unstable();
        let mut text = format!("{cursor} {replay}\n");
        for (name, mark) in told {
            text.push_str(&format!("{mark} {name}\n"));
        }
        if self.saved.as_ref() == Some(&text) {
            return;
        }
        // What the lookouts hold goes to disk first, the runs a note spent
        // with it: the stream never starts again past news whose spending
        // a crash lost.
        for lookout in self.lookouts.values_mut() {
            lookout.flush();
        }
        let path = cursor_path(&self.store);
        let partial = path.with_extension(format!("cursor.{}.partial", std::process::id()));
        self.saved = Some(text.clone());
        held::then(self.store.clone(), move |written| {
            if written {
                let _ = write_moved(&partial, &path, &text);
            }
        });
    }

    /// Just before the oldest turn under way that a lookout hears, or
    /// whose failed fire's note is: its start says which trigger sent it.
    fn replay_floor(&self) -> Option<i64> {
        (self.in_flight.values().copied())
            .chain(
                (self.telling.values().chain(self.bouncing.values()))
                    .filter_map(|telling| telling.began),
            )
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

/// Each subscriber's own mark the last lookout kept, after its first
/// line: up to it, that subscriber's news was told.
fn told_marks(store: &Path) -> HashMap<String, i64> {
    let text = std::fs::read_to_string(cursor_path(store)).unwrap_or_default();
    (text.lines().skip(1))
        .filter_map(|line| {
            let (mark, name) = line.split_once(' ')?;
            Some((name.to_owned(), mark.parse().ok()?))
        })
        .collect()
}

/// Where the newest event a lookout saw live is kept: beside the store.
fn cursor_path(store: &Path) -> PathBuf {
    let mut path = store.as_os_str().to_owned();
    path.push(".lookout.cursor");
    PathBuf::from(path)
}

/// The most fires' turns noted at once.
const FIRES: usize = 4096;

/// The number an event names: its turn's, or for a message in line that
/// no turn has taken, the message's, which the turn it opens shares.
pub(crate) fn number(event: &Value) -> Option<i64> {
    event["turn"].as_i64().or_else(|| event["message"].as_i64())
}

/// The events a lookout reads, of every bot: what bots there are, how
/// their messages go in line and are taken in or end there, how their
/// turns end, the tools they call, and the calls that wait on the person.
/// The text a model streams, most of all events, is no one's news here.
const KINDS: [&str; 12] = [
    "created",
    "forked",
    "deleted",
    "accepted",
    "queued",
    "steered",
    "message_ended",
    "turn_finished",
    "tool_started",
    "tool_completed",
    "approval_requested",
    "approval_asked",
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

/// Where the lookout keeps what waits to be told to `subscriber`, and
/// what each of its subscriptions that lasts so many notes has told.
pub fn held_file(store: &Path, subscriber: &str) -> PathBuf {
    (beside(store, ".sources").join(".held")).join(format!("{subscriber}.json"))
}

/// How many notes each subscription of `subscriber` that lasts so many
/// has told, by its [`subs::Sub::counter`], as its lookout last kept them.
pub fn runs_told(store: &Path, subscriber: &str) -> HashMap<String, usize> {
    let kept: Value = (std::fs::read_to_string(held_file(store, subscriber)).ok())
        .and_then(|text| serde_json::from_str(&text).ok())
        .unwrap_or_default();
    (kept["runs_told"].as_object().into_iter().flatten())
        .map(|(sub, told)| (sub.clone(), told.as_array().map_or(0, Vec::len)))
        .collect()
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

/// One line on the lookout for a subscriber, as `/background` shows it:
/// whether one runs and watches it, and today's updates.
pub fn status(store: &std::path::Path, lead: &str) -> String {
    if !running(store) {
        return "Lookout · not running".to_owned();
    }
    let (mut seated, mut updates, mut unkept, mut waiting) = (false, 0, 0, 0);
    for line in logged(store, lead) {
        seated |= line["seated"] == true;
        // What an update left waiting, as of the latest.
        if line.get("woke").is_some() {
            waiting = line["waiting"].as_u64().unwrap_or(0);
        }
        if today(&line) {
            updates += usize::from(line.get("woke").is_some());
            unkept += usize::from(line.get("skipped").is_some() && line.get("source").is_some());
        }
    }
    if !seated {
        return "Lookout · not watching this lead yet".to_owned();
    }
    let mut said = format!(
        "Lookout · watching · {updates} update{} in the last day",
        if updates == 1 { "" } else { "s" }
    );
    if waiting > 0 {
        said.push_str(&format!(" · {waiting} waiting"));
    }
    if unkept > 0 {
        said.push_str(&format!(" · {unkept} not kept"));
    }
    said
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
    lead: String,
    /// The subscriber's ID, as the daemon lists it.
    lead_id: Option<i64>,
    /// What happens now is news to it: the replay is over, and the
    /// stream past its own mark.
    live: bool,
    /// The stream reached the present: the reads of what its task events
    /// kept on disk follow have begun, so those may go.
    replay_over: bool,
    /// The agent source, for the tasks it watches.
    activity: activity::Activity,
    /// The subscriber's own tasks: every bot it or one of them made.
    lineage: HashSet<String>,
    /// What the lookout watches for it.
    subs: Vec<subs::Sub>,
    /// For each other bot a subscription names the makers of: it, and every
    /// bot it or one of those made.
    others: HashMap<String, HashSet<String>>,
    /// Bots a subscription's `agents` glob names, as listed and made since.
    globbed: HashSet<String>,
    /// The other leads of the lead's folder and every bot they or theirs
    /// made: theirs, which the lead's tasks subscription never names.
    theirs: HashSet<String>,
    /// The bots it watches.
    watched: HashSet<String>,
    /// Events of the agent source to read before they are built.
    due: Vec<activity::Pending>,
    /// Reads under way: when each started, its task and its place in the
    /// stream, which holds back the task's later events till it is done.
    reading: Vec<(Instant, String, i64)>,
    /// The longest a read took since the last note, which the log says
    /// with the note.
    slowest_read: Duration,
    /// When the last note was refused: even a steer waits the gap after.
    refused: Option<Instant>,
    /// The cursor of the oldest event whose news is not told yet.
    from: Option<i64>,
    /// Up to where its news was told, by the last lookout or before the
    /// connection dropped, while the stream is still behind it: what
    /// comes at or before it is history to this subscriber.
    told_to: Option<i64>,
    /// What the subscriber is to hear, oldest first.
    heard: Vec<sources::Heard>,
    /// How many of those wait at most (`sonny.lookout_waiting`), 0 for no
    /// limit.
    waiting: usize,
    /// What came past them, not kept, for the next note to say.
    unkept: Unkept,
    /// What the note on its way says was not kept, until the daemon takes it.
    unkept_sent: Unkept,
    /// When a note last told the subscriber each source, which a source's
    /// digest counts from.
    told_sources: HashMap<String, Instant>,
    /// When a note last told what each subscription with a digest of its
    /// own heard, which its digest counts from.
    told_subs: HashMap<String, Instant>,
    /// The notes, by request, that told what each subscription that lasts
    /// so many (`runs`) heard, under `Sub::counter`, kept with what waits:
    /// a note sent again after a restart is one note.
    runs_told: HashMap<String, Vec<String>>,
    /// The subscriptions that last so many whose news the note on its way
    /// tells, by `Sub::counter` as they were when it went: one replaced
    /// meanwhile is not charged for it.
    charged: Vec<String>,
    /// Subscriptions whose notes ran out, for the service to end, as they
    /// were read: one put in its place since stays.
    ended: Vec<subs::Sub>,
    /// Subscriptions whose `until` came before the files were read again,
    /// for the service to end.
    over: Vec<subs::Sub>,
    /// What each subscription told lately, for the listing, written to
    /// `counts_file` once a tick when it changed.
    counts: subs::Counts,
    counts_file: Option<PathBuf>,
    counts_dirty: bool,
    /// Where what waits is kept, untold, so a restart loses none of what
    /// a digest or a note on its way held.
    held: Option<PathBuf>,
    /// What is held changed since it was last handed to the writer.
    dirty: bool,
    /// A note under way when the lookout last stopped, sent again as it
    /// was, under its request, so the daemon takes it at most once.
    resend: Vec<sources::Heard>,
    /// The note on its way, and its key, until the daemon answers.
    sending: Option<(Vec<sources::Heard>, Vec<String>)>,
    /// When the last note went, in epoch ms: the next waits
    /// `GROUP_INTERVAL` after.
    sent_ms: u128,
    /// The subscriptions with a digest of their own whose news that note
    /// tells: none tells more till it lands.
    sourced_subs: Vec<String>,
    log: Option<PathBuf>,
}

impl Lookout {
    fn new(lead: &str) -> Self {
        Lookout {
            id: 0,
            record: Value::Null,
            lead: lead.to_owned(),
            lead_id: None,
            live: false,
            replay_over: false,
            activity: activity::Activity::new(lead),
            lineage: [lead.to_owned()].into(),
            subs: Vec::new(),
            others: HashMap::new(),
            globbed: HashSet::new(),
            theirs: HashSet::new(),
            watched: HashSet::new(),
            due: Vec::new(),
            reading: Vec::new(),
            slowest_read: Duration::ZERO,
            refused: None,
            from: None,
            told_to: None,
            heard: Vec::new(),
            waiting: WAITING,
            unkept: Unkept::default(),
            unkept_sent: Unkept::default(),
            told_sources: HashMap::new(),
            told_subs: HashMap::new(),
            runs_told: HashMap::new(),
            charged: Vec::new(),
            ended: Vec::new(),
            over: Vec::new(),
            counts: subs::Counts::default(),
            counts_file: None,
            counts_dirty: false,
            held: None,
            dirty: false,
            resend: Vec::new(),
            sending: None,
            sent_ms: 0,
            sourced_subs: Vec::new(),
            log: None,
        }
    }

    /// The bots it watches: those its subscriptions watch, among `names`,
    /// every bot listed. Run when the subscriptions change; a bot made or
    /// deleted since is checked alone (`check`).
    fn sync<'a>(&mut self, names: impl IntoIterator<Item = &'a String>) {
        self.watched = (names.into_iter())
            .filter(|bot| self.watches(bot))
            .cloned()
            .collect();
    }

    /// Whether one bot is watched now. A bot made or deleted changes no
    /// other bot's standing, so this is all it takes.
    fn check(&mut self, bot: &str) {
        match self.watches(bot) {
            true => self.watched.insert(bot.to_owned()),
            false => self.watched.remove(bot),
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

    /// Whether the subscriber hears a bot's events: it watches it.
    fn hears(&self, bot: &str) -> bool {
        self.watched.contains(bot)
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
                self.activity.gone(bot);
            }
            _ => {}
        }
    }

    /// Whether a subscription watches this bot: one of the tasks it names
    /// the makers of, or one its names match. The subscriber's own doings
    /// are never news.
    fn sub_watches(&self, sub: &subs::Sub, bot: &str) -> bool {
        // A workflow's workers wake no one: their run tells its caller
        // once, at its end.
        if bot == self.lead || crate::workflow::is_worker(bot) {
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
            || (self.globbed.contains(bot)
                && sub.agents.iter().any(|glob| subs::matches(glob, bot)))
    }

    fn watches(&self, bot: &str) -> bool {
        self.subs.iter().any(|sub| self.sub_watches(sub, bot))
    }

    /// Every bot a subscription names the makers of, other than the lead,
    /// from what the daemon lists: what each made, and what those made.
    fn roots(&mut self, records: &HashMap<String, Value>, made: &Made) {
        // The subscriber's own: what it made before it was seated is listed,
        // not replayed.
        made.grow(&mut self.lineage);
        let roots: HashSet<String> = (self.subs.iter())
            .flat_map(|sub| sub.tasks_of.iter().cloned())
            .filter(|root| *root != self.lead)
            .collect();
        self.others.retain(|root, _| roots.contains(root));
        for root in roots {
            if self.others.contains_key(&root) {
                continue;
            }
            let mut lineage: HashSet<String> = [root.clone()].into();
            made.grow(&mut lineage);
            self.others.insert(root, lineage);
        }
        // Another lead's tasks stay theirs once that lead is gone: their
        // maker is still named as one.
        let siblings = (made.leads.iter())
            .chain(made.by.keys())
            .filter(|name| self.sibling(name));
        let mut theirs = HashSet::new();
        for sibling in siblings {
            if records.contains_key(sibling) {
                theirs.insert(sibling.clone());
            }
            theirs
                .extend((made.by.get(sibling).into_iter().flatten()).map(|(name, _)| name.clone()));
        }
        made.grow(&mut theirs);
        self.theirs = theirs;
        // Only an `agents` glob names bots beyond the lineages.
        self.globbed = match self.subs.iter().any(|sub| !sub.agents.is_empty()) {
            true => (records.iter())
                .filter(|(name, record)| self.globs(name) && record["expire_ms"].is_null())
                .map(|(name, _)| name.clone())
                .collect(),
            false => HashSet::new(),
        };
        // What it watches is among those, as listed.
        let named: Vec<String> = (self.lineage.iter())
            .chain(self.others.values().flatten())
            .chain(&self.globbed)
            .filter(|name| records.contains_key(*name))
            .cloned()
            .collect();
        self.sync(&named);
    }

    /// Whether `bot` is another lead of this lead's folder.
    fn sibling(&self, bot: &str) -> bool {
        bot != self.lead
            && crate::project::lead_project(bot).is_some()
            && crate::project::lead_project(bot) == crate::project::lead_project(&self.lead)
    }

    /// Whether a subscription's `agents` glob names this bot. The tasks
    /// subscription Sonny gives a lead never names another lead's: one the
    /// lead asked for may.
    fn globs(&self, bot: &str) -> bool {
        let theirs = self.theirs.contains(bot);
        bot != self.lead
            && (self.subs.iter()).any(|sub| {
                !(theirs && sub.name == "tasks")
                    && sub.agents.iter().any(|glob| subs::matches(glob, bot))
            })
    }

    /// Up to where its news was told, with the stream at `cursor`: just
    /// before its oldest news not told yet, or where its mark still is.
    /// Run after `Service::floor`, which lets go of a `from` told since.
    fn mark(&self, cursor: Option<i64>) -> Option<i64> {
        let own = match (cursor, self.from) {
            (Some(at), Some(from)) => Some(at.min(from - 1)),
            (at, _) => at,
        };
        own.max(self.told_to)
    }

    /// Whether an event at `at` comes at or before its mark: told already.
    /// The first past it makes what it hears news again, once the stream
    /// is `live`. One with no cursor is never replayed.
    fn told_before(&mut self, at: Option<i64>, live: bool) -> bool {
        let (Some(mark), Some(at)) = (self.told_to, at) else {
            return false;
        };
        if at <= mark {
            return true;
        }
        self.told_to = None;
        self.live = live;
        false
    }

    /// The replay is over: nothing it hears now was told before.
    fn caught_up(&mut self) {
        self.replay_over = true;
        if self.told_to.take().is_some() {
            self.live = true;
        }
    }

    /// Notes counted only for subscriptions it has that last so many,
    /// each under when it was added, so one removed and added again, even
    /// in one call, starts its count afresh; and counts only for those it
    /// has, as they are, for this bot.
    fn counting(&mut self) {
        let subs = &self.subs;
        self.runs_told.retain(|counter, _| {
            subs.iter()
                .any(|sub| sub.runs.is_some() && &sub.counter() == counter)
        });
        let id = self.lead_id;
        let kept: Vec<(&str, String)> = (subs.iter())
            .map(|sub| (sub.name.as_str(), sub.identity(id)))
            .collect();
        if (self.counts).keep(|name, of| kept.iter().any(|(n, o)| *n == name && o == of)) {
            self.counts_dirty = true;
        }
    }

    /// The identity its counts keep for its subscription named `name`.
    fn of(&self, name: &str) -> Option<String> {
        (self.subs.iter().find(|sub| sub.name == name)).map(|sub| sub.identity(self.lead_id))
    }

    /// Whether a subscription's `until` came since the files were last
    /// read: it goes to `over`, to end before any note is sent.
    fn overdue(&mut self) -> bool {
        let now = (epoch_ms() / 1000) as i64;
        for sub in self.subs.iter().filter(|sub| sub.over(now)) {
            if !self.over.contains(sub) {
                self.over.push(sub.clone());
            }
        }
        !self.over.is_empty()
    }

    /// A note that told what these subscriptions heard landed: each it
    /// still has counts what it told, for the listing, once a note, by its
    /// request: sent again, it counts no more.
    fn count(&mut self, key: &[String], heard: &[(String, u64)], steered: &[&str]) {
        let now = epoch_ms() as u64;
        let request = format!("lookout-{}", digest(&key.join(",")));
        for (name, n) in heard {
            let Some(of) = self.of(name) else {
                continue;
            };
            let steered = steered.contains(&name.as_str());
            self.counts.told(name, &of, &request, *n, steered, now);
            self.counts_dirty = true;
        }
    }

    /// What a note telling these subscriptions' news charges: each that
    /// lasts so many, by its `Sub::counter`.
    fn charges<'a>(&self, names: impl Iterator<Item = &'a String>) -> Vec<String> {
        let mut charges: Vec<String> = Vec::new();
        for name in names {
            let Some(sub) = self
                .subs
                .iter()
                .find(|sub| &sub.name == name && sub.runs.is_some())
            else {
                continue;
            };
            let counter = sub.counter();
            if !charges.contains(&counter) {
                charges.push(counter);
            }
        }
        charges
    }

    /// One of the daemon's events, made by `maker` when it names a bot:
    /// the agent source's events it makes, once the stream is news to
    /// this subscriber.
    fn event(&mut self, event: &Value, maker: Option<&str>) {
        if event["event"] == "follow_live" {
            self.live = true;
            return;
        }
        self.lineage_of(event);
        let Some(bot) = event["bot"].as_str() else {
            return;
        };
        // Only what it watches is kept: a task made is watched from its
        // `created`, one deleted until its `deleted`.
        if event["event"] == "deleted" {
            self.activity.gone(bot);
            return;
        }
        if !self.watched.contains(bot) {
            return;
        }
        let order = event["cursor"].as_i64().unwrap_or(0);
        let made = self.activity.event(event, maker, order);
        if !self.live {
            return;
        }
        for out in made {
            match out {
                activity::Out::Now(event) => self.hear(*event, order),
                activity::Out::Read(pending) => self.due.push(pending),
            }
        }
    }

    /// The subscriptions that hear an event now: for the agent source,
    /// those that watch its task, the event's topic; for any other, those
    /// naming its source, or its source and topic. Of those, the ones
    /// whose filter, if any, matches it. What the subscriber itself said
    /// is no news to it.
    fn hearing(&self, event: &sources::Event) -> Vec<String> {
        if event.from.as_deref() == Some(self.lead.as_str()) {
            return Vec::new();
        }
        let task = match event.source == activity::SOURCE && !event.lookout {
            true => Some(event.topic.as_deref().unwrap_or_default()),
            false => None,
        };
        let filtered = |sub: &subs::Sub| {
            (sub.filter.as_ref()).is_none_or(|filter| pattern::matches(filter, &event.fields))
        };
        (self.subs.iter())
            .filter(|sub| match task {
                Some(task) => self.sub_watches(sub, task) && filtered(sub),
                None => sub.hears(event),
            })
            .map(|sub| sub.name.clone())
            .collect()
    }

    /// Whether the end of a turn the subscriber sent a message into
    /// reaches it, as its subscriptions' filters hear that end, asked for
    /// by whom its start named. A turn a lookout note started ends with
    /// no event, and one whose start was not seen names no asker.
    fn hears_end(&self, sent: &Sent) -> bool {
        let bot = sent.bot.as_str();
        if !sent.taken
            || !self.watched.contains(bot)
            || sent.asker.as_ref().is_some_and(|asked| asked.note)
        {
            return false;
        }
        let mut fields = json!({"type": "turn.ended", "bot": bot, "from": bot, "turn": sent.turn,
            "status": sent.status, "done": false});
        if let Some(asked) = &sent.asker {
            let by = activity::by(&self.lead, asked.from.as_deref(), asked.origin.as_deref());
            fields["asked_by"] = json!(by);
        }
        (self.subs.iter()).any(|sub| {
            self.sub_watches(sub, bot)
                && (sub.filter.as_ref()).is_none_or(|filter| pattern::matches(filter, &fields))
        })
    }

    /// A read that started at `started` is done: the event it builds, if
    /// any, waits to be told.
    fn read(&mut self, started: Instant, pending: &activity::Pending, read: activity::Read) {
        self.slowest_read = self.slowest_read.max(started.elapsed());
        if let Some(at) = (self.reading.iter()).position(|(at, task, order)| {
            *at == started && *task == pending.task && *order == pending.order
        }) {
            self.reading.remove(at);
        }
        if let Some(event) = activity::built(&self.lead, pending, read) {
            self.hear(event, pending.order);
        }
    }

    /// A source's event, held for the next note when a subscription
    /// hears that source.
    fn heard_from(&mut self, event: &sources::Event) {
        self.hear(event.clone(), 0);
    }

    /// An event held for the next note when a subscription hears it, at
    /// `order` in the stream for the agent source's: after what its task
    /// said before.
    fn hear(&mut self, mut event: sources::Event, order: i64) {
        let subs = self.hearing(&event);
        if subs.is_empty() || self.holds(&event) {
            return;
        }
        // What a subscription that hears it marks as a steer is one, as if
        // its source had.
        if !event.steer
            && let Some(sub) = (self.subs.iter()).find(|sub| {
                subs.contains(&sub.name)
                    && (sub.steer.as_ref()).is_some_and(|u| pattern::matches(u, &event.fields))
            })
        {
            event.steer = true;
            event.steer_by = Some(sub.name.clone());
        }
        // Full: a newer event is not kept, and the next note says so. One
        // that steers takes the place of the newest that does not.
        if self.waiting > 0 && self.heard.len() >= self.waiting {
            let newest = (self.heard.iter()).rposition(|heard| !heard.event.steer);
            let dropped = match (event.steer, newest) {
                (true, Some(at)) => self.heard.remove(at).event,
                _ => event.clone(),
            };
            let at_ms = epoch_ms();
            self.unkept.add(&dropped.source, at_ms as u64);
            self.write(
                &json!({"at_ms": at_ms, "lead": self.lead, "source": dropped.source,
                "id": dropped.id, "skipped": "more waiting than sonny.lookout_waiting"}),
            );
            self.keep();
            if !(event.steer && newest.is_some()) {
                return;
            }
        }
        // A task's events in the order they happened, though their reads
        // ended in another.
        let at = match event.source == activity::SOURCE && order > 0 {
            true => (self.heard.iter()).position(|heard| {
                heard.order > order
                    && heard.event.source == event.source
                    && heard.event.topic == event.topic
            }),
            false => None,
        };
        let heard = sources::Heard { event, subs, order };
        match at {
            Some(at) => self.heard.insert(at, heard),
            None => self.heard.push(heard),
        }
        self.keep();
    }

    /// Whether anything it heard is neither told nor kept on disk yet:
    /// events to read or being read.
    fn untold(&self) -> bool {
        !self.due.is_empty() || !self.reading.is_empty()
    }

    /// Whether an event waits for a read of its task that came before it.
    fn blocked(&self, heard: &sources::Heard) -> bool {
        // Until the stream reaches the present, an earlier event's read
        // may not have begun: one kept on disk waits for it.
        heard.order > 0
            && heard.event.source == activity::SOURCE
            && (!self.replay_over
                || (self.reading.iter()).any(|(_, task, order)| {
                    *order < heard.order && heard.event.topic.as_deref() == Some(task.as_str())
                }))
    }

    /// Whether an event may be told now: it steers, or its digest has
    /// passed since a note last told it.
    fn ready(&self, heard: &sources::Heard, now: Instant) -> bool {
        let event = &heard.event;
        let source = || {
            event.digest.is_none_or(|digest| {
                (self.told_sources.get(&event.source)).is_none_or(|at| now >= *at + digest)
            })
        };
        // A subscription's own digest takes its source's place for what
        // it hears, `0` too; one that says none goes by the source's.
        event.steer
            || heard.subs.iter().any(|name| {
                let own = (self.subs.iter()).find(|sub| sub.name == *name && sub.digest.is_some());
                match own {
                    // `0` says it waits for none, its source's either.
                    Some(sub) => (sub.digest_span()).is_none_or(|digest| {
                        (self.told_subs.get(name)).is_none_or(|at| now >= *at + digest)
                    }),
                    None => source(),
                }
            })
            || (heard.subs.is_empty() && source())
    }

    /// Tell the subscriber what waits, as a steer: it joins the running
    /// turn at its next round, or starts one. A note goes as soon as the
    /// last one landed, with all that may go then; what a digest holds
    /// waits for it. The note goes beside the stream, one at a time: what
    /// the daemon answers comes back to `answered`.
    fn wake(&mut self, client: &Arc<Client>, woken: &Woken) {
        let now = Instant::now();
        if self.sending.is_some() || self.refused(now) {
            return;
        }
        if self.resend.is_empty() && self.heard.is_empty() {
            return;
        }
        if self.overdue() {
            return;
        }
        // The note under way when the lookout stopped goes again first, as
        // it was: the daemon took it already, or takes it now.
        let sent = std::mem::take(&mut self.resend);
        if !sent.is_empty() {
            let unkept = self.unkept_sent.line(self.waiting, epoch_ms() as u64);
            let prompt = sources::note(&sent, &self.heard, unkept.as_deref());
            return self.send_note(client, woken, sent, prompt);
        }
        // What is ready goes at once, unless a note went less than
        // `GROUP_INTERVAL` ago: then it gathers till then, or till
        // something steers.
        let grouping = epoch_ms() < self.sent_ms + GROUP_INTERVAL.as_millis();
        let goes = |heard: &sources::Heard| self.ready(heard, now) && !self.blocked(heard);
        let any = match grouping {
            true => self
                .heard
                .iter()
                .any(|heard| heard.event.steer && goes(heard)),
            false => self.heard.iter().any(goes),
        };
        if !any {
            return;
        }
        // What waits keeps its place: a task's events stay in the order
        // they happened, however their reads finished.
        let (ready, held): (Vec<_>, Vec<_>) = std::mem::take(&mut self.heard)
            .into_iter()
            .enumerate()
            .partition(|(_, heard)| self.ready(heard, now) && !self.blocked(heard));
        // A bounded note, always at least one event: the rest wait for the
        // next.
        let chosen = bounded(&ready);
        let (sent, rest): (Vec<_>, Vec<_>) =
            (ready.into_iter().enumerate()).partition(|(at, _)| chosen[*at]);
        let sent: Vec<sources::Heard> = sent.into_iter().map(|(_, (_, heard))| heard).collect();
        let mut rest: Vec<_> = rest.into_iter().map(|(_, waiting)| waiting).collect();
        rest.extend(held);
        rest.sort_unstable_by_key(|(place, _)| *place);
        self.heard = rest.into_iter().map(|(_, heard)| heard).collect();
        // What did not fit is told with it, and again if it is refused.
        self.unkept_sent = std::mem::take(&mut self.unkept);
        let unkept = self.unkept_sent.line(self.waiting, epoch_ms() as u64);
        let prompt = sources::note(&sent, &self.heard, unkept.as_deref());
        self.send_note(client, woken, sent, prompt);
    }

    /// Sends `sent` as one note, its request named by the events it tells.
    /// A note sent again after a restart, if the daemon took it before,
    /// answers `idempotency_conflict`, which counts as told.
    fn send_note(
        &mut self,
        client: &Arc<Client>,
        woken: &Woken,
        sent: Vec<sources::Heard>,
        prompt: String,
    ) {
        let prompt = trailed(prompt, &trailer());
        let key = note_key(&sent);
        let request_id = format!("lookout-{}", digest(&key.join(",")));
        let mut asked = json!({"bot": self.lead, "bot_id": self.lead_id, "prompt": prompt,
            "request_id": request_id, "delivery": "steer", "origin": ORIGIN});
        let quotes: Vec<Value> = (sent.iter())
            .filter_map(|heard| heard.event.quote.as_ref())
            .map(|(bot, message)| json!({"bot": bot, "message": message}))
            .collect();
        if !quotes.is_empty() {
            asked["quotes"] = json!(quotes);
        }
        // Its subscriptions' digests count from it once it lands, and
        // allow no other note meanwhile.
        let mut subs: Vec<String> = Vec::new();
        for sub in sent.iter().flat_map(|heard| &heard.subs) {
            if self.sub_digest(sub).is_some() && !subs.contains(sub) {
                subs.push(sub.clone());
            }
        }
        self.sourced_subs = subs;
        self.charged = self.charges(sent.iter().flat_map(|heard| &heard.subs));
        self.sending = Some((sent, key));
        self.sent_ms = epoch_ms();
        self.keep();
        let (client, woken) = (client.clone(), woken.clone());
        let (lead, id) = (self.lead.clone(), self.id);
        tokio::spawn(async move {
            let submitted = client.request("submit", asked).await;
            let _ = woken
                .send((lead, id, submitted.map(drop).map_err(|e| e.code)))
                .await;
        });
    }

    /// What the daemon answered the note on its way.
    fn answered(&mut self, submitted: Result<(), String>) {
        let now = Instant::now();
        let Some((sent, key)) = self.sending.take() else {
            return;
        };
        match submitted {
            // Words it could not quote, gone with their task or too long,
            // are named by their turns instead, and the note goes again.
            Err(code) if code.starts_with("quote_") => {
                // Nothing reached the subscriber: it goes again at once.
                self.sent_ms = 0;
                self.sourced(false);
                let mut back: Vec<sources::Heard> = sent;
                for heard in &mut back {
                    unquote(&mut heard.event);
                }
                back.append(&mut self.heard);
                self.heard = back;
                self.unkept.merge(std::mem::take(&mut self.unkept_sent));
                self.write(&json!({"at_ms": epoch_ms(), "lead": self.lead, "unquoted": code}));
            }
            // The same news twice is one note: the daemon took it before,
            // so it was told then.
            Err(code) if code != "idempotency_conflict" => {
                self.sourced(false);
                let mut back = sent;
                back.append(&mut self.heard);
                self.heard = back;
                self.unkept.merge(std::mem::take(&mut self.unkept_sent));
                self.write(&json!({"at_ms": epoch_ms(), "lead": self.lead, "wake_failed": code}));
                self.refused = Some(now);
            }
            submitted => {
                self.refused = None;
                self.sourced(true);
                // Each subscription counts the events it heard, and a steer
                // for it: one it set off, or one the source marked, which
                // steers for every subscription.
                let mut heard: Vec<(String, u64)> = Vec::new();
                let mut steered: Vec<&str> = Vec::new();
                for one in &sent {
                    for sub in &one.subs {
                        match heard.iter_mut().find(|(had, _)| had == sub) {
                            Some((_, n)) => *n += 1,
                            None => heard.push((sub.clone(), 1)),
                        }
                        let marked = one.event.steer && one.event.steer_by.is_none();
                        let own = (self.subs.iter().find(|had| had.name == *sub))
                            .and_then(|had| had.steer.as_ref())
                            .is_some_and(|steer| pattern::matches(steer, &one.event.fields));
                        if (marked || own) && !steered.contains(&sub.as_str()) {
                            steered.push(sub);
                        }
                    }
                }
                self.count(&key, &heard, &steered);
                self.spend(&key);
                self.unkept_sent = Unkept::default();
                for heard in &sent {
                    self.told_sources.insert(heard.event.source.clone(), now);
                }
                told_commons(&crate::commons::who(&self.lead, self.lead_id), &sent);
                if submitted.is_ok() {
                    let request_id = format!("lookout-{}", digest(&key.join(",")));
                    self.write(&json!({"at_ms": epoch_ms(), "lead": self.lead, "woke": key,
                        "request_id": request_id, "waiting": self.heard.len(),
                        "read_ms": self.slowest_read.as_millis()}));
                    self.slowest_read = Duration::ZERO;
                }
            }
        }
        self.keep();
    }

    /// Its subscriptions are now `subs`: what only those gone heard goes
    /// with them, and one put in another's place under its name counts
    /// afresh.
    fn resubscribed(&mut self, subs: Vec<subs::Sub>) {
        let gone = gone(&self.subs, &subs);
        // A note under way told what those gone heard: once it lands, it
        // counts for none of them, nor for one put in another's place.
        if let Some((sent, ..)) = &mut self.sending {
            for heard in sent {
                heard.subs.retain(|name| !gone.contains(name));
            }
        }
        self.told_subs.retain(|name, _| !gone.contains(name));
        self.subs = subs;
        self.counting();
    }

    /// The note landed, under `key`: each subscription it charged that
    /// lasts so many notes has one fewer to go, once however often that
    /// note is sent, and one with none left ends, as an automation does
    /// after its `--runs`.
    fn spend(&mut self, key: &[String]) {
        let request = format!("lookout-{}", digest(&key.join(",")));
        let mut spent = false;
        for counter in std::mem::take(&mut self.charged) {
            let Some(sub) = self.subs.iter().find(|sub| sub.counter() == counter) else {
                continue;
            };
            let (Some(runs), sub) = (sub.runs, sub.clone()) else {
                continue;
            };
            let told = self.runs_told.entry(counter.clone()).or_default();
            if told.contains(&request) {
                continue;
            }
            spent = true;
            told.push(request.clone());
            if told.len() as u64 >= runs {
                self.runs_told.remove(&counter);
                if !self.ended.contains(&sub) {
                    self.ended.push(sub);
                }
            }
        }
        if spent {
            self.keep();
        }
    }

    /// The connection dropped: the answer under way is lost with it, so
    /// what its note carried waits again, as when one is refused.
    fn reconnected(&mut self) {
        self.sent_ms = 0;
        if let Some((mut sent, _)) = self.sending.take() {
            self.sourced(false);
            sent.append(&mut self.heard);
            self.heard = sent;
            self.unkept.merge(std::mem::take(&mut self.unkept_sent));
        }
    }

    /// The note landed, `told` if the daemon took it: its subscriptions'
    /// digests count from now, or from the note before.
    fn sourced(&mut self, told: bool) {
        let now = Instant::now();
        for sub in std::mem::take(&mut self.sourced_subs) {
            if told {
                self.told_subs.insert(sub, now);
            }
        }
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
        // A task's event keeps its place in the daemon's stream, so one
        // read again after a restart is still told before it.
        let json = |heard: &sources::Heard| {
            let mut kept = heard.event.to_json();
            if heard.order > 0 {
                kept["order"] = json!(heard.order);
            }
            kept
        };
        let sending: Vec<Value> = match &self.sending {
            Some((sent, ..)) => sent.iter().map(json).collect(),
            None => self.resend.iter().map(json).collect(),
        };
        let events: Vec<Value> = self.heard.iter().map(json).collect();
        let told_subs = &self.told_subs;
        // What the note under way says was not kept stays with it, so the
        // note sent again says the same.
        let (unkept, unkept_sending) = (&self.unkept, &self.unkept_sent);
        if events.is_empty()
            && unkept.count == 0
            && unkept_sending.count == 0
            && sending.is_empty()
            && self.told_sources.is_empty()
            && told_subs.is_empty()
            && self.runs_told.is_empty()
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
        let mut body = json!({"bot_id": self.lead_id, "sending": sending,
            "events": events, "told": at_ms(&self.told_sources)});
        // Only when a subscription's own digest told something, so a file
        // reads as it did before there were any.
        if !told_subs.is_empty() {
            body["told_subs"] = json!(at_ms(told_subs));
        }
        if !self.runs_told.is_empty() {
            body["runs_told"] = json!(self.runs_told);
        }
        if unkept.count > 0 {
            body["unkept"] = unkept.to_json();
        }
        if unkept_sending.count > 0 {
            body["unkept_sending"] = unkept_sending.to_json();
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
        if std::mem::take(&mut self.counts_dirty)
            && let Some(path) = &self.counts_file
        {
            let body = (!self.counts.is_empty()).then(|| self.counts.to_json().to_string());
            held::write(path.clone(), body.map(String::into_bytes));
        }
    }

    /// The subscriber is gone or subscribed to nothing: what waited for it
    /// is no one's.
    fn unheld(&self) {
        if let Some(path) = &self.held {
            held::write(path.clone(), None);
        }
        if let Some(path) = &self.counts_file {
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
        if kept.is_null() || kept["bot_id"] != json!(self.lead_id) {
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
        self.told_subs.extend(read("told_subs"));
        self.runs_told
            .extend(
                (kept["runs_told"].as_object().into_iter().flatten()).map(|(sub, told)| {
                    let told = (told.as_array().into_iter().flatten())
                        .filter_map(|request| request.as_str().map(str::to_owned));
                    (sub.clone(), told.collect())
                }),
            );
        let heard = |key: &str| -> Vec<sources::Heard> {
            (kept[key].as_array().into_iter().flatten())
                .filter_map(|kept| {
                    Some(sources::Heard {
                        event: sources::Event::from_json(kept)?,
                        subs: Vec::new(),
                        order: kept["order"].as_i64().unwrap_or(0),
                    })
                })
                .collect()
        };
        self.heard = heard("events");
        self.resend = heard("sending");
        self.unkept = Unkept::from_json(&kept["unkept"]);
        self.unkept_sent = Unkept::from_json(&kept["unkept_sending"]);
        self.rehear();
    }

    /// Whether an event with this one's source and ID waits already, or
    /// is on its way: a source that read it again after a crash, before
    /// its cursor moved, says it again.
    fn holds(&self, event: &sources::Event) -> bool {
        let Some(id) = event.id.as_deref() else {
            return false;
        };
        let sending = self.sending.iter().flat_map(|(sent, ..)| sent);
        (self.heard.iter().chain(&self.resend).chain(sending)).any(|heard| {
            heard.event.source == event.source && heard.event.id.as_deref() == Some(id)
        })
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

/// What keys a note: each event it tells, by source, ID and when the
/// lookout read it.
fn note_key(sent: &[sources::Heard]) -> Vec<String> {
    (sent.iter())
        .map(|heard| {
            let event = &heard.event;
            match (event.source == activity::SOURCE, event.id.as_deref()) {
                // A task's event is named by its task, turn and kind, as
                // the stream says it again after a restart.
                (true, Some(id)) => format!("{}/{id}", event.source),
                (_, id) => {
                    let id = id.unwrap_or_default();
                    format!("{}/{id}/{}/{}", event.source, event.at_ms, event.seq)
                }
            }
        })
        .collect()
}

/// Which of the ready events, in the order they wait, the next note
/// carries: those that steer first, each with what waits before it in
/// its task, then the rest in order while they fit; never more than a
/// note holds, but always one.
fn bounded(ready: &[(usize, sources::Heard)]) -> Vec<bool> {
    let mut chosen = vec![false; ready.len()];
    let (mut bytes, mut quotes) = (0, 0);
    fn task(heard: &sources::Heard) -> Option<Option<&str>> {
        (heard.event.source == activity::SOURCE).then_some(heard.event.topic.as_deref())
    }
    let mut add = |chosen: &mut [bool], group: &[usize]| {
        let more: usize = group.iter().map(|at| sources::bytes(&ready[*at].1)).sum();
        let quoted: usize = (group.iter())
            .map(|at| usize::from(ready[*at].1.event.quote.is_some()))
            .sum();
        // Only a lone event goes past the bounds, so a note is never empty.
        let alone = group.len() == 1 && !chosen.contains(&true);
        if !alone && (bytes + more > sources::NOTE_BYTES || quotes + quoted > sources::NOTE_QUOTES)
        {
            return false;
        }
        (bytes, quotes) = (bytes + more, quotes + quoted);
        for at in group {
            chosen[*at] = true;
        }
        true
    };
    for (at, (_, heard)) in ready.iter().enumerate() {
        if !heard.event.steer {
            continue;
        }
        let group: Vec<usize> = (0..=at)
            .filter(|before| {
                !chosen[*before]
                    && (*before == at
                        || task(heard).is_some() && task(&ready[*before].1) == task(heard))
            })
            .collect();
        if !add(&mut chosen, &group) {
            // Too much waits before it in its task: what fits goes, in
            // order, and the steer follows in a later note.
            for at in group {
                if !add(&mut chosen, &[at]) {
                    break;
                }
            }
            break;
        }
    }
    for at in 0..ready.len() {
        if !chosen[at] && !add(&mut chosen, &[at]) {
            break;
        }
    }
    chosen
}

/// An event whose quote the daemon refused: it names the person's
/// message instead.
fn unquote(event: &mut sources::Event) {
    if let Some((bot, message)) = event.quote.take() {
        event.text = format!("(their words are in {bot}/{message}, which could not be quoted)");
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
    /// whether every file of its store holds what it was last handed.
    type Then = Box<dyn FnOnce(bool) + Send>;

    enum Job {
        Write(PathBuf, Option<Vec<u8>>),
        /// The store whose files it asks about, by the path each is
        /// named beside, and what to call.
        Done(PathBuf, Then),
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
                            Job::Done(store, then) => done.push((store, then)),
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
                    // Another store's write failing is not this one's: one
                    // process holds one store, but tests hold many at once.
                    for (store, then) in done {
                        let store = store.as_os_str().as_encoded_bytes();
                        then(
                            !failed
                                .keys()
                                .any(|path| path.as_os_str().as_encoded_bytes().starts_with(store)),
                        );
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
    /// whether each file of `store`, named beside it, holds what it was
    /// last handed. Never called only if the writer is gone, which drops
    /// `then`.
    pub fn then(store: PathBuf, then: impl FnOnce(bool) + Send + 'static) {
        if let Ok(send) = sender().lock() {
            let _ = send.send(Job::Done(store, Box::new(then)));
        }
    }

    /// Waits for every write handed over before it.
    #[cfg(test)]
    pub fn written() {
        let (tell, told) = mpsc::channel();
        then(PathBuf::new(), move |_| {
            let _ = tell.send(());
        });
        let _ = told.recv();
    }
}

/// The subscriptions of `had` that `now` no longer holds, by name: one
/// put in another's place under its name ends the one before, so what
/// that heard is not the new one's to tell.
fn gone(had: &[subs::Sub], now: &[subs::Sub]) -> Vec<String> {
    (had.iter())
        .filter(|had| !now.contains(had))
        .map(|had| had.name.clone())
        .collect()
}

fn epoch_ms() -> u128 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis())
        .unwrap_or(0)
}

/// The commons posts a note told `lead` (as `commons::who` names it)
/// count as shown to it, as a read does, so its reply after them is
/// posted at once: each event's id is `THREAD/N`, its Nth post from 0.
fn told_commons(lead: &str, sent: &[sources::Heard]) {
    let mut told: Vec<(String, Vec<usize>)> = Vec::new();
    for heard in sent {
        let event = &heard.event;
        if event.source != agent_client::config::COMMONS_SOURCE {
            continue;
        }
        let Some((thread, n)) = (event.id.as_deref())
            .and_then(|id| id.rsplit_once('/'))
            .and_then(|(thread, n)| Some((thread, n.parse::<usize>().ok()?)))
        else {
            continue;
        };
        match told.iter_mut().find(|(had, _)| had == thread) {
            Some((_, posts)) => posts.push(n),
            None => told.push((thread.to_owned(), vec![n])),
        }
    }
    let Some(home) = (!told.is_empty())
        .then(|| std::env::var_os("HOME"))
        .flatten()
    else {
        return;
    };
    // The commons' lock and its file's write wait on other agents' posts
    // and the disk, so they run beside the lookout's loop, never in it.
    let lead = lead.to_owned();
    let mark = move || {
        for (thread, posts) in told {
            crate::commons::told(Path::new(&home), &lead, &thread, &posts);
        }
    };
    match tokio::runtime::Handle::try_current() {
        Ok(runtime) => drop(runtime.spawn_blocking(mark)),
        Err(_) => mark(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_note_ends_with_the_trailer_the_config_names_and_none_when_empty() {
        assert_eq!(
            trailed("[slack · ana]\nlunch?".into(), "Reply when it is urgent."),
            "[slack · ana]\nlunch?\n[lookout]\nReply when it is urgent."
        );
        assert_eq!(
            trailed("[slack · ana]\nlunch?".into(), ""),
            "[slack · ana]\nlunch?"
        );
        assert_eq!(
            trailed("[slack · ana]\nlunch?".into(), " \n"),
            "[slack · ana]\nlunch?"
        );
    }

    /// `prompt` untimed, without the trailer that ends every note, which
    /// it checks is there.
    fn told(prompt: &str) -> String {
        let untimed = sources::untimed(prompt);
        let trailer = format!("\n[lookout]\n{}", agent_client::config::LOOKOUT_TRAILER);
        match untimed.strip_suffix(&trailer) {
            Some(note) => note.to_owned(),
            None => panic!("no trailer: {untimed}"),
        }
    }

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
    async fn a_completed_turn_s_reply_is_read_in_one_request() {
        let asked = Arc::new(std::sync::Mutex::new(Vec::new()));
        let kept = asked.clone();
        let client = crate::tests::daemon_answering(move |request| {
            kept.lock().unwrap().push(request["op"].as_str().unwrap_or_default().to_owned());
            match (request["op"].as_str(), request["nodes"][0].as_i64()) {
                (Some("history_nodes"), _) => {
                    json!({"nodes": [{"node": 41, "turn": 4}, {"node": 40, "turn": 4}]})
                }
                (_, Some(42)) => json!({"items": [{"node": 42, "item": {"type": "message",
                    "role": "assistant", "content": [{"type": "output_text", "text": "Fixed avg."}]}}]}),
                _ => json!({"items": [{"node": 41, "item": {"type": "message",
                    "role": "assistant", "content": [{"type": "output_text", "text": "Earlier."}]}}]}),
            }
        })
        .await;
        let read = reply_text(&client, "calc.fix", 4, Some(42), true).await;
        assert_eq!(read.as_deref(), Some("Fixed avg."));
        assert_eq!(*asked.lock().unwrap(), ["history_items"]);
        // A turn that failed may end anywhere: its turn's nodes are looked
        // through, as before.
        asked.lock().unwrap().clear();
        let read = reply_text(&client, "calc.fix", 4, Some(41), false).await;
        assert_eq!(read.as_deref(), Some("Earlier."));
        assert_eq!(*asked.lock().unwrap(), ["history_nodes", "history_items"]);
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

    #[test]
    fn a_first_replay_a_lag_cuts_short_goes_on_from_where_it_got_to() {
        // No lookout before it and nothing under way: from the start, once.
        assert_eq!(follow_after(None, None, None), 0);
        assert_eq!(follow_after(None, None, Some(4_096)), 4_096);
        // A turn under way still has its start read again.
        assert_eq!(follow_after(None, Some(1_500), Some(4_096)), 1_500);
        // Where the last lookout left off says, as it always did.
        assert_eq!(follow_after(Some(9_000), None, Some(4_096)), 9_000);
        assert_eq!(follow_after(Some(9_000), Some(8_000), None), 8_000);
    }

    fn lookout() -> Lookout {
        let mut lookout = Lookout::new("calc.lead");
        lookout.subs = vec![subs::Sub::lead("calc.lead", "calc")];
        lookout.lineage.insert("calc.fix".to_owned());
        lookout
    }

    /// A lookout whose task is watched, live.
    fn seated() -> Lookout {
        let mut lookout = lookout();
        lookout.lead_id = Some(1);
        lookout.sync(&["calc.fix".to_owned()]);
        lookout.live = true;
        lookout.replay_over = true;
        lookout
    }

    fn ev(bot: &str, turn: i64, kind: &str, data: Value) -> Value {
        json!({"bot": bot, "turn": turn, "event": kind, "data": data})
    }

    /// `event` at `cursor` in the stream.
    fn at(mut event: Value, cursor: i64) -> Value {
        event["cursor"] = json!(cursor);
        event
    }

    /// Reads what a lookout has due as `read` answers, as the service
    /// would beside the stream.
    fn read_due(lookout: &mut Lookout, read: impl Fn(&activity::Pending) -> activity::Read) {
        for pending in std::mem::take(&mut lookout.due) {
            let started = Instant::now();
            (lookout.reading).push((started, pending.task.clone(), pending.order));
            let answer = read(&pending);
            lookout.read(started, &pending, answer);
        }
    }

    /// What reads answer in tests: `text` for words and replies, and who
    /// asked unknown.
    fn says(text: &str) -> impl Fn(&activity::Pending) -> activity::Read + '_ {
        move |pending| match &pending.kind {
            activity::Kind::Words { .. } => activity::Read::Words {
                text: text.to_owned(),
                someone: false,
            },
            activity::Kind::Reply { .. } => activity::Read::Reply {
                text: text.to_owned(),
                asker: None,
            },
            activity::Kind::Call { name, .. } => activity::Read::Call {
                name: name.clone(),
                text: text.to_owned(),
            },
        }
    }

    /// An event about a message in line, which no turn has taken.
    fn in_line(bot: &str, message: i64, kind: &str, data: Value) -> Value {
        json!({"bot": bot, "turn": null, "message": message, "event": kind, "data": data})
    }

    fn sub(name: &str, tasks_of: &[&str], bots: &[&str]) -> subs::Sub {
        subs::Sub {
            name: name.into(),
            tasks_of: tasks_of.iter().map(|b| b.to_string()).collect(),
            agents: bots.iter().map(|b| b.to_string()).collect(),
            sources: Vec::new(),
            filter: None,
            digest: None,
            steer: None,
            owner: None,
            until: None,
            runs: None,
            made: None,
        }
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
        lookout.roots(&records, &Made::of(&records));
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

    #[tokio::test]
    async fn a_lead_whose_filter_hears_no_failure_still_gets_its_bounce() {
        let mut service = service(PathBuf::from("/nonexistent"), &[]);
        service
            .watchers
            .insert("calc.fix".into(), vec!["calc.lead".into()]);
        let from = json!({"from": {"bot": "calc.lead", "turn": 1, "bot_id": 1}});
        let failed = json!({"status": "failed", "error": "provider_http_400"});
        // Its filter hears the end as the agent source tells it, with who
        // asked: the lead, as `you`.
        for (filter, bounces) in [
            (
                json!({"type": ["turn.ended"], "status": ["completed"]}),
                true,
            ),
            (json!({"type": ["turn.ended"], "status": ["failed"]}), false),
            (json!({"type": ["turn.ended"], "asked_by": ["you"]}), false),
            (
                json!({"type": ["turn.ended"], "asked_by": ["the person"]}),
                true,
            ),
            (json!({"type": ["turn.ended"], "done": [false]}), false),
        ] {
            let mut lookout = seated();
            lookout.subs[0].filter = Some(filter);
            let accepted = ev("calc.fix", 4, "accepted", from.clone());
            let finished = ev("calc.fix", 4, "turn_finished", failed.clone());
            service.lookouts.insert("calc.lead".into(), lookout);
            service.bounced(&accepted, true);
            let told = service.bounced(&finished, true);
            assert_eq!(told.len(), usize::from(bounces));
        }
    }

    #[tokio::test]
    async fn a_reconnect_keeps_the_start_of_a_message_still_waiting_in_line() {
        let mut service = service(PathBuf::from("/nonexistent"), &[]);
        let from = json!({"from": {"bot": "calc.lead", "turn": 1, "bot_id": 1}});
        service.bounced(&ev("calc.fix", 4, "queued", from), true);
        for key in [
            ("calc.fix", 4),
            ("calc.docs", 2),
            ("shop.fix", 3),
            ("gone", 5),
        ] {
            service.in_flight.insert((key.0.into(), key.1), 10);
        }
        // calc.fix runs nothing, its message still in line; calc.docs's
        // turn and shop.fix's earlier one ended unseen; gone was deleted.
        let bots = [
            json!({"name": "calc.fix"}),
            json!({"name": "calc.docs"}),
            json!({"name": "shop.fix", "running_turn": 6}),
        ];
        service.ended_unseen(&bots);
        let kept: HashSet<(String, i64)> = service.in_flight.keys().cloned().collect();
        assert_eq!(kept, HashSet::from([("calc.fix".to_owned(), 4)]));
    }

    #[tokio::test]
    async fn a_message_taken_into_a_running_turn_is_tracked_by_that_turn() {
        let mut service = service(PathBuf::from("/nonexistent"), &[]);
        let from = json!({"from": {"bot": "api.lead", "turn": 1, "bot_id": 7}});
        // Another agent's message waits in line for docs's running turn 3.
        let queued = in_line("docs", 5, "queued", from.clone());
        service.bounced(&queued, true);
        service.track(&queued, false, Some(40));
        assert!(service.in_flight.contains_key(&("docs".into(), 5)));
        // Turn 3 takes it in: nothing waits on message 5 any more, and
        // turn 3's end is what tells its sender.
        let mut steered = json!({"bot": "docs", "turn": 3, "message": 5, "event": "steered",
            "data": {"message": 5, "node": 9}});
        steered["data"]["from"] = from["from"].clone();
        service.bounced(&steered, true);
        service.track(&steered, false, Some(41));
        assert!(!service.in_flight.contains_key(&("docs".into(), 5)));
        assert!(!service.senders.contains_key(&("docs".into(), 5)));
        assert_eq!(service.in_flight.get(&("docs".into(), 3)), Some(&41));
        let failed = json!({"status": "failed", "error": "provider_http_400"});
        let told = service.bounced(&ev("docs", 3, "turn_finished", failed.clone()), true);
        assert_eq!(told.len(), 1);
        service.track(&ev("docs", 3, "turn_finished", failed), false, Some(42));
        assert!(service.in_flight.is_empty() && service.replay_floor().is_none());
    }

    #[tokio::test]
    async fn a_message_that_ends_in_line_keeps_where_it_began_for_its_bounce() {
        let mut service = service(PathBuf::from("/nonexistent"), &[]);
        let from = json!({"from": {"bot": "api.lead", "turn": 1, "bot_id": 7}});
        let queued = in_line("docs", 5, "queued", from.clone());
        service.bounced(&queued, true);
        service.track(&queued, false, Some(40));
        // It fails before any turn takes it: its bounce restarts from where
        // it was queued, not from its end.
        let mut ended = in_line(
            "docs",
            5,
            "message_ended",
            json!({"status": "failed", "error": "provider_http_400"}),
        );
        ended["data"]["from"] = from["from"].clone();
        assert_eq!(service.began(&ended), Some(40));
        assert_eq!(service.bounced(&ended, true).len(), 1);
    }

    #[tokio::test]
    async fn a_message_that_ends_in_line_bounces_though_its_sender_hears_every_end() {
        let mut service = service(PathBuf::from("/nonexistent"), &[]);
        service
            .watchers
            .insert("calc.fix".into(), vec!["calc.lead".into()]);
        service.lookouts.insert("calc.lead".into(), seated());
        let from = json!({"from": {"bot": "calc.lead", "turn": 1, "bot_id": 1}});
        let failed = json!({"status": "failed", "error": "provider_http_400"});
        // A turn that ends is told by the tasks source: no bounce.
        service.bounced(&ev("calc.fix", 4, "accepted", from.clone()), true);
        let told = service.bounced(&ev("calc.fix", 4, "turn_finished", failed.clone()), true);
        assert!(told.is_empty());
        // A message that fails before any turn takes it is no event there,
        // so only its bounce tells its sender.
        let queued = in_line("calc.fix", 5, "queued", from.clone());
        service.bounced(&queued, true);
        let mut ended = in_line("calc.fix", 5, "message_ended", failed);
        ended["data"]["from"] = from["from"].clone();
        assert_eq!(service.bounced(&ended, true).len(), 1);
    }

    #[tokio::test]
    async fn a_message_a_failed_turn_never_carried_out_goes_back_to_its_sender() {
        let mut service = service(PathBuf::from("/nonexistent"), &[]);
        let from = |bot: &str| json!({"from": {"bot": bot, "turn": 1, "bot_id": 7}});
        let failed = json!({"status": "failed", "error": "provider_http_400"});
        // A message from another agent starts the turn; one more joins it.
        assert!(
            service
                .bounced(&ev("docs", 4, "accepted", from("api.lead")), true)
                .is_empty()
        );
        service.bounced(&ev("docs", 4, "steered", from("web.lead")), true);
        service.bounced(&ev("docs", 4, "steered", from("api.lead")), true);
        let told = service.bounced(&ev("docs", 4, "turn_finished", failed.clone()), true);
        let to: Vec<(&str, Option<i64>, i64, &str)> = (told.iter())
            .map(|(to, id, _, turn, error)| (to.as_str(), *id, *turn, error.as_str()))
            .collect();
        assert_eq!(
            to,
            [
                ("api.lead", Some(7), 4, "provider_http_400"),
                ("web.lead", Some(7), 4, "provider_http_400")
            ]
        );
        assert!(
            service
                .bounced(&ev("docs", 4, "turn_finished", failed.clone()), true)
                .is_empty()
        );
        // Not a completed turn, one that said nothing, the replay's, nor a
        // message the bot sent itself.
        for (turn, end, live, sender) in [
            (5, json!({"status": "completed"}), true, "api.lead"),
            (
                6,
                json!({"status": "failed", "error": "empty_completion"}),
                true,
                "api.lead",
            ),
            (7, failed.clone(), false, "api.lead"),
            (8, failed.clone(), true, "docs"),
        ] {
            service.bounced(&ev("docs", turn, "queued", from(sender)), live);
            assert!(
                service
                    .bounced(&ev("docs", turn, "turn_finished", end), live)
                    .is_empty()
            );
        }
        // Nor a lead whose lookout hears the bot: its task's end reaches it.
        service
            .watchers
            .insert("docs".into(), vec!["api.lead".into()]);
        service.bounced(&ev("docs", 9, "accepted", from("api.lead")), true);
        assert!(
            service
                .bounced(&ev("docs", 9, "turn_finished", failed), true)
                .is_empty()
        );
        assert_eq!(
            bounce_note("docs", 4, "provider_http_400"),
            "[your message to docs was not carried out: turn 4 ended provider_http_400]"
        );
    }

    fn service(store: PathBuf, records: &[Value]) -> Service {
        Service {
            runner: sources::Runner::new(store.join("sources"), tokio::sync::mpsc::channel(1).0),
            store,
            waiting: WAITING,
            records: (records.iter())
                .map(|r| (r["name"].as_str().unwrap().to_owned(), r.clone()))
                .collect(),
            made: Made::of(
                &(records.iter())
                    .map(|r| (r["name"].as_str().unwrap().to_owned(), r.clone()))
                    .collect(),
            ),
            subscribers: HashMap::new(),
            files: subs::Read::default(),
            read_at: HashMap::new(),
            socket: PathBuf::new(),
            lookouts: HashMap::new(),
            log: PathBuf::new(),
            seated: 0,
            live: true,
            replay_over: true,
            cursor: None,
            saved: None,
            kept: HashMap::new(),
            resume: None,
            replayed: None,
            replay: None,
            in_flight: HashMap::new(),
            telling: HashMap::new(),
            bouncing: HashMap::new(),
            watchers: HashMap::new(),
            started: false,
            rewriting: HashSet::new(),
            fires: HashMap::new(),
            senders: HashMap::new(),
            askers: HashMap::new(),
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
    fn a_lookout_holds_as_many_from_sources_as_the_settings_say() {
        let record = json!({"name": "calc.fix", "bot_id": 4, "workspace": "/", "status": "idle"});
        let mut service = service(PathBuf::new(), std::slice::from_ref(&record));
        service.waiting = 0;
        let sub = subs::Sub::commons("tidy", Some(4));
        (service.subscribers).insert("calc.fix".into(), vec![sub]);
        assert!(service.place(&record));
        assert_eq!(service.lookouts["calc.fix"].waiting, 0);
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

    #[tokio::test]
    async fn a_task_s_events_are_told_in_the_order_they_happened_though_read_out_of_it() {
        let (client, mut got) = submits().await;
        let mut lookout = seated();
        let from = json!({"from": {"bot": "calc.docs"}, "node": 4});
        lookout.event(&at(ev("calc.fix", 5, "queued", from), 10), None);
        let finished = json!({"status": "completed"});
        lookout.event(&at(ev("calc.fix", 4, "turn_finished", finished), 11), None);
        let mut due = std::mem::take(&mut lookout.due);
        assert_eq!(due.len(), 2);
        let later = due.pop().unwrap();
        let earlier = due.pop().unwrap();
        // Both reads start; the later one ends first.
        let started = Instant::now();
        for pending in [&earlier, &later] {
            (lookout.reading).push((started, pending.task.clone(), pending.order));
        }
        lookout.read(started, &later, says("done")(&later));
        assert!(
            lookout.blocked(&lookout.heard[0]),
            "an earlier read is under way"
        );
        lookout.read(started, &earlier, says("look at this")(&earlier));
        let froms: Vec<&str> = (lookout.heard.iter())
            .map(|heard| heard.event.from.as_deref().unwrap())
            .collect();
        assert_eq!(froms, ["calc.docs", "calc.fix replied, completed"]);
        assert!(!lookout.blocked(&lookout.heard[1]));
        // Waiting out a group interval keeps that order, and so does the
        // note once it goes.
        let (woken, _woke) = tokio::sync::mpsc::channel(4);
        lookout.sent_ms = epoch_ms();
        lookout.wake(&client, &woken);
        let froms: Vec<&str> = (lookout.heard.iter())
            .map(|heard| heard.event.from.as_deref().unwrap())
            .collect();
        assert_eq!(froms, ["calc.docs", "calc.fix replied, completed"]);
        lookout.sent_ms = 0;
        lookout.wake(&client, &woken);
        let prompt = got.recv().await.unwrap()["prompt"].clone();
        assert_eq!(
            told(prompt.as_str().unwrap()),
            "[tasks · calc.fix · calc.docs]\nlook at this\n\
             [tasks · calc.fix · calc.fix replied, completed]\ndone"
        );
    }

    #[test]
    fn a_task_s_event_kept_on_disk_keeps_its_place_for_one_read_again() {
        let dir = std::env::temp_dir().join(format!("sonny-order-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let path = dir.join("calc.lead.json");
        let mut lookout = seated();
        lookout.restore(path.clone());
        let from = json!({"from": {"bot": "calc.docs"}, "node": 4});
        lookout.event(&at(ev("calc.fix", 5, "queued", from), 10), None);
        let finished = json!({"status": "completed"});
        lookout.event(&at(ev("calc.fix", 4, "turn_finished", finished), 11), None);
        let mut due = std::mem::take(&mut lookout.due);
        let later = due.pop().unwrap();
        let earlier = due.pop().unwrap();
        let started = Instant::now();
        for pending in [&earlier, &later] {
            (lookout.reading).push((started, pending.task.clone(), pending.order));
        }
        // The later read ends and is kept; the lookout stops before the
        // earlier one does.
        lookout.read(started, &later, says("done")(&later));
        lookout.flush();
        held::written();
        let mut again = seated();
        again.replay_over = false;
        again.restore(path.clone());
        assert_eq!(again.heard.len(), 1);
        assert_eq!(again.heard[0].order, later.order);
        // While the replay has not reached the earlier one, nothing has
        // begun reading it: the kept event waits all the same.
        assert!(again.blocked(&again.heard[0]));
        again.caught_up();
        assert!(!again.blocked(&again.heard[0]));
        // Read again from the stream, the earlier one holds it back while
        // under way, and is told before it.
        again
            .reading
            .push((started, earlier.task.clone(), earlier.order));
        assert!(again.blocked(&again.heard[0]));
        again.read(started, &earlier, says("look at this")(&earlier));
        let froms: Vec<&str> = (again.heard.iter())
            .map(|heard| heard.event.from.as_deref().unwrap())
            .collect();
        assert_eq!(froms, ["calc.docs", "calc.fix replied, completed"]);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_steer_goes_in_the_next_note_with_what_waits_before_it_in_its_task() {
        let big = "x".repeat(sources::NOTE_BYTES / 2);
        let heard = |event: sources::Event, order| {
            let subs = vec!["chat".to_owned()];
            (0, sources::Heard { event, subs, order })
        };
        let task = |text: &str, steer| sources::Event {
            source: activity::SOURCE.into(),
            topic: Some("calc.fix".into()),
            steer,
            ..slack(text, false)
        };
        let ready = [
            heard(slack(&big, false), 0),
            heard(slack(&big, false), 0),
            heard(task("asked", false), 1),
            heard(slack(&big, false), 0),
            heard(task("the person wrote", true), 2),
        ];
        // The steer and what came before it in its task, then the rest
        // in order while they fit.
        assert_eq!(bounded(&ready), [true, false, true, false, true]);
        // One that fits nothing else still goes.
        assert_eq!(bounded(&ready[..1]), [true]);
        // A steer with more before it in its task than a note holds: the
        // task's events go in order while they fit, the steer later.
        let third = "x".repeat(sources::NOTE_BYTES / 3);
        let much = [
            heard(task(&third, false), 1),
            heard(task(&third, false), 2),
            heard(task(&third, false), 3),
            heard(task("the person wrote", true), 4),
        ];
        assert_eq!(bounded(&much), [true, true, false, false]);
    }

    #[test]
    fn a_subscription_s_filter_reads_the_task_events_fields() {
        let mut lookout = seated();
        lookout.subs[0].filter = Some(json!({"type": ["turn.ended"], "status": ["failed"]}));
        for (turn, status) in [(1, "completed"), (2, "failed")] {
            let finished = json!({"status": status});
            lookout.event(
                &at(ev("calc.fix", turn, "turn_finished", finished), turn),
                None,
            );
        }
        lookout.event(
            &at(ev("calc.fix", 3, "accepted", json!({"node": 2})), 9),
            None,
        );
        read_due(&mut lookout, says("it broke"));
        let told: Vec<&str> = (lookout.heard.iter())
            .map(|heard| heard.event.from.as_deref().unwrap())
            .collect();
        assert_eq!(told, ["calc.fix replied, failed"]);
        // A turn it sent a message into ends for it as the filter hears
        // it, asked for by whom the turn's start named.
        let asked = |from: Option<&str>| Asked {
            from: from.map(str::to_owned),
            origin: None,
            note: false,
        };
        let sent = |bot: &str, status: &str, asker: Option<Asked>| Sent {
            from: "calc.lead".into(),
            id: None,
            bot: bot.into(),
            turn: 7,
            status: status.into(),
            error: status.into(),
            asker,
            taken: true,
        };
        let you = Some(asked(Some("calc.lead")));
        assert!(lookout.hears_end(&sent("calc.fix", "failed", you.clone())));
        assert!(!lookout.hears_end(&sent("calc.fix", "completed", you.clone())));
        assert!(!lookout.hears_end(&sent("calc.other", "failed", you.clone())));
        // Asked for by the person, its end does not pass a filter for the
        // subscriber's own turns: the message bounces.
        lookout.subs[0].filter = Some(json!({"type": ["turn.ended"], "asked_by": ["you"]}));
        assert!(lookout.hears_end(&sent("calc.fix", "failed", you)));
        assert!(!lookout.hears_end(&sent("calc.fix", "failed", Some(asked(None)))));
        // Nor does a turn a lookout note started, which ends with no event.
        let note = Asked {
            note: true,
            ..asked(Some("calc.lead"))
        };
        assert!(!lookout.hears_end(&sent("calc.fix", "failed", Some(note))));
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
            let asked = json!({"from": {"bot": "calc.lead"}});
            lookout.event(&ev(bot, n, "accepted", asked), Some("calc.lead"));
            let ended = json!({"status": "completed"});
            lookout.event(&ev(bot, n, "turn_finished", ended), Some("calc.lead"));
        };
        let lookout = service.lookouts.get_mut("calc.lead").unwrap();
        lookout.live = true;
        assert!(lookout.watches("calc.fix"));
        // Made before the lead was seated, its turn's end is news, with
        // the reply to read; and one that subscribes for itself is still
        // its lead's task: the lookout judges no one.
        turn(lookout, "calc.fix", 1);
        turn(lookout, "calc.review", 1);
        let read: Vec<(&str, i64)> = (lookout.due.iter())
            .map(|pending| (pending.task.as_str(), pending.turn))
            .collect();
        assert_eq!(read, [("calc.fix", 1), ("calc.review", 1)]);
        // Its own turns are never its news.
        lookout.due.clear();
        turn(lookout, "calc.lead", 2);
        assert!(lookout.due.is_empty());
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
        // Its own events are no one's news, its own lookout's either.
        assert!(service.hearing(&delta("shop.lead")).is_empty());
        assert!(service.hearing(&delta("stray")).is_empty());
        // Each keeps only the bots it watches.
        let calc = &service.lookouts["calc.lead"];
        assert!(calc.hears("calc.fix") && !calc.hears("shop.cart") && !calc.hears("calc.lead"));
        // A bot made reaches every lookout, and each checks that bot alone.
        let made = json!({"event": "created", "bot": "calc.docs",
            "data": {"bot_id": 5, "created_by": "calc.lead", "created_by_id": 1}});
        let mut all = service.hearing(&made);
        all.sort();
        assert_eq!(all, ["calc.lead", "shop.lead"]);
        let tell = |service: &mut Service, event: &Value| {
            for name in service.hearing(event) {
                service.lookouts.get_mut(&name).unwrap().event(event, None);
            }
            service.index_bot(event["bot"].as_str().unwrap());
        };
        tell(&mut service, &made);
        assert_eq!(service.hearing(&delta("calc.docs")), ["calc.lead"]);
        assert!(service.lookouts["calc.lead"].hears("calc.docs"));
        assert!(!service.lookouts["shop.lead"].hears("calc.docs"));
        // Deleted, it is heard by no one, and seated nowhere.
        tell(
            &mut service,
            &json!({"event": "deleted", "bot": "calc.docs", "data": {}}),
        );
        assert!(service.hearing(&delta("calc.docs")).is_empty());
        assert!(!service.lookouts["calc.lead"].hears("calc.docs"));
    }

    #[tokio::test]
    async fn every_subscription_steer_counts_though_its_note_joins_no_turn() {
        let (client, _got) = submits().await;
        let mut lookout = seated();
        for bot in ["calc.fix", "calc.docs"] {
            lookout.globbed.insert(bot.to_owned());
        }
        let failed = Some(json!({"status": ["failed"]}));
        let mut fixes = sub("fixes", &[], &["calc.fix"]);
        fixes.steer = failed.clone();
        let mut docs = sub("docs", &[], &["calc.docs"]);
        docs.steer = failed;
        lookout.subs = vec![fixes, docs.clone()];
        lookout.sync(&["calc.fix".to_owned(), "calc.docs".to_owned()]);
        for (n, bot) in ["calc.fix", "calc.docs"].into_iter().enumerate() {
            let ask = json!({"from": {"bot": "calc.lead"}});
            let cursor = 10 * n as i64;
            lookout.event(&at(ev(bot, 5, "accepted", ask), cursor + 1), None);
            let failed = json!({"status": "failed"});
            lookout.event(&at(ev(bot, 5, "turn_finished", failed), cursor + 2), None);
        }
        read_due(&mut lookout, says("it broke"));
        let (woken, _woke) = tokio::sync::mpsc::channel(4);
        lookout.wake(&client, &woken);
        let (told, ..) = lookout.sending.as_ref().unwrap();
        let mut steered: Vec<&str> = (told.iter())
            .filter_map(|heard| heard.event.steer_by.as_deref())
            .collect();
        steered.sort_unstable();
        assert_eq!(steered, ["docs", "fixes"]);
        // One put in another's place while the note is under way counts
        // nothing of it once it lands.
        docs.steer = None;
        let fixes = lookout.subs[0].clone();
        lookout.resubscribed(vec![fixes, docs]);
        lookout.answered(Ok(()));
        let now = epoch_ms() as u64;
        assert_eq!(
            lookout
                .counts
                .of("fixes", &lookout.of("fixes").unwrap_or_default(), now)
                .unwrap()["steers"],
            1
        );
        assert!(
            lookout
                .counts
                .of("docs", &lookout.of("docs").unwrap_or_default(), now)
                .is_none()
        );
    }

    #[tokio::test]
    async fn a_note_goes_beside_the_stream_and_one_lost_with_the_connection_is_news_again() {
        let (client, mut got) = submits().await;
        let mut lookout = seated();
        let ended = |lookout: &mut Lookout, turn: i64| {
            let from = json!({"from": {"bot": "calc.lead"}});
            lookout.event(&at(ev("calc.fix", turn, "accepted", from), 2 * turn), None);
            let finished = json!({"status": "completed"});
            lookout.event(
                &at(
                    ev("calc.fix", turn, "turn_finished", finished),
                    2 * turn + 1,
                ),
                None,
            );
            read_due(lookout, says(&format!("done {turn}")));
        };
        ended(&mut lookout, 1);
        let (woken, _woke) = tokio::sync::mpsc::channel(4);
        lookout.wake(&client, &woken);
        assert!(lookout.sending.is_some() && lookout.heard.is_empty());
        // What it carries is kept on disk with what waits: the cursor may
        // pass it.
        assert!(!lookout.untold());
        // One note at a time: what comes meanwhile waits for the answer.
        ended(&mut lookout, 2);
        lookout.wake(&client, &woken);
        let prompt = got.recv().await.unwrap()["prompt"].clone();
        assert_eq!(
            told(prompt.as_str().unwrap()),
            "[tasks · calc.fix · calc.fix replied, completed]\ndone 1"
        );
        tokio::time::sleep(Duration::from_millis(50)).await;
        assert!(got.try_recv().is_err());
        // The connection dropped before the answer came: what the note
        // carried waits again, before what came since.
        lookout.reconnected();
        assert!(lookout.sending.is_none());
        let ids: Vec<&str> = (lookout.heard.iter())
            .filter_map(|heard| heard.event.id.as_deref())
            .collect();
        assert_eq!(ids, ["calc.fix/1/ended", "calc.fix/2/ended"]);
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
        assert!(!service.rewrote(&started("follows", "a")));
        assert!(!service.rewrote(&started("memory", "b")));
        assert!(!service.rewrote(&event("tool_started", "c", json!({"name": "memory"}))));
        assert!(!service.rewrote(&event("tool_completed", "b", json!({}))));
        assert!(service.rewrote(&event("tool_completed", "a", json!({}))));
        assert!(!service.rewrote(&event("tool_completed", "a", json!({}))));
        // A turn that ends forgets a call it never ended.
        assert!(!service.rewrote(&started("follows", "d")));
        assert!(!service.rewrote(&event("turn_finished", "", json!({}))));
        assert!(service.rewriting.is_empty());
        // Posting in the commons subscribes the poster.
        assert!(!service.rewrote(&started("commons", "e")));
        assert!(service.rewrote(&event("tool_completed", "e", json!({}))));
    }

    #[tokio::test]
    async fn the_note_is_the_lookout_s_and_quotes_the_person_s_words() {
        let (client, mut got) = submits().await;
        let mut lookout = seated();
        lookout.event(
            &at(ev("calc.fix", 5, "accepted", json!({"node": 9})), 1),
            None,
        );
        read_due(&mut lookout, says("yes, ship it"));
        let docs = json!({"from": {"bot": "calc.docs"}, "node": 10});
        lookout.event(&at(ev("calc.fix", 6, "queued", docs), 2), None);
        read_due(&mut lookout, says("ship it"));
        let (woken, _woke) = tokio::sync::mpsc::channel(4);
        lookout.wake(&client, &woken);
        let submit = got.recv().await.unwrap();
        assert_eq!(submit["origin"], ORIGIN);
        assert_eq!(submit["delivery"], "steer");
        assert_eq!(submit["quotes"], json!([{"bot": "calc.fix", "message": 5}]));
        // The person's words only as the daemon quotes them; an agent's as
        // it wrote them.
        assert_eq!(
            told(submit["prompt"].as_str().unwrap()),
            "[tasks · calc.fix · the person]\n\
             (their words are quoted below, from calc.fix/5)\n\
             [tasks · calc.fix · calc.docs]\nship it"
        );
    }

    #[tokio::test]
    async fn words_the_daemon_will_not_quote_go_again_named_by_their_turn() {
        let (client, mut got) = submits().await;
        let mut lookout = seated();
        lookout.event(
            &at(ev("calc.fix", 5, "accepted", json!({"node": 9})), 1),
            None,
        );
        read_due(&mut lookout, says("ship it"));
        let (woken, _woke) = tokio::sync::mpsc::channel(4);
        lookout.wake(&client, &woken);
        got.recv().await.unwrap();
        lookout.answered(Err("quote_too_large".into()));
        assert!(lookout.refused.is_none());
        lookout.wake(&client, &woken);
        let submit = got.recv().await.unwrap();
        assert!(submit.get("quotes").is_none());
        assert_eq!(
            told(submit["prompt"].as_str().unwrap()),
            "[tasks · calc.fix · the person]\n\
             (their words are in calc.fix/5, which could not be quoted)"
        );
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
        held::written();
        assert_eq!(marks(&store), (Some(90), Some(39)));
        // Nothing under way: it starts again where its news ended.
        service.in_flight.clear();
        service.save_cursor();
        held::written();
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
        held::written();
        assert_eq!(marks(&store), (Some(49), Some(29)));
        // The daemon took it: nothing holds the stream.
        service.telling.clear();
        service.save_cursor();
        held::written();
        assert_eq!(marks(&store), (Some(90), Some(90)));
        std::fs::write(cursor_path(&store), "12").unwrap();
        assert_eq!(marks(&store), (Some(12), Some(12)));
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[tokio::test]
    async fn a_restart_tells_each_subscriber_only_what_came_after_its_own_mark() {
        let dir = std::env::temp_dir().join(format!("lookout-told-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let store = dir.join("state.sqlite");
        // Each lead and its task, seated, as a lookout starting holds them.
        let lead = |lead: &str, task: &str| {
            let mut lookout = Lookout::new(lead);
            let project = lead.split('.').next().unwrap();
            lookout.subs = vec![subs::Sub::lead(lead, project)];
            lookout.lineage.insert(task.to_owned());
            lookout.sync(&[task.to_owned()]);
            lookout
        };
        let at = |bot: &str, turn: i64, kind: &str, cursor: i64| {
            at(ev(bot, turn, kind, json!({"status": "completed"})), cursor)
        };
        let hearing = |names: &[&str]| names.iter().map(|n| n.to_string()).collect::<Vec<_>>();
        let pending = |turn: i64| activity::Pending {
            task: "calc.fix".into(),
            maker: None,
            turn,
            order: 50,
            kind: activity::Kind::Words {
                from: activity::PERSON.into(),
                node: None,
                brief: None,
            },
        };
        // calc.lead's news from event 50 waits untold; shop.lead was told
        // everything up to 90.
        let mut first = service(store.clone(), &[]);
        let mut calc = lead("calc.lead", "calc.fix");
        calc.due.push(pending(5));
        calc.from = Some(50);
        first.lookouts.insert("calc.lead".into(), calc);
        first
            .lookouts
            .insert("shop.lead".into(), lead("shop.lead", "shop.fix"));
        first.cursor = Some(90);
        first.save_cursor();
        held::written();
        // The stream starts again before calc.lead's news; shop.lead's own
        // mark is kept beside it.
        assert_eq!(marks(&store), (Some(49), Some(49)));
        assert_eq!(
            told_marks(&store),
            HashMap::from([("shop.lead".to_owned(), 90)])
        );
        // Nothing waits: no mark of its own past where the stream starts.
        first.lookouts.get_mut("calc.lead").unwrap().due.clear();
        first.save_cursor();
        held::written();
        assert_eq!(marks(&store), (Some(90), Some(90)));
        assert!(told_marks(&store).is_empty());
        first
            .lookouts
            .get_mut("calc.lead")
            .unwrap()
            .due
            .push(pending(5));
        first.lookouts.get_mut("calc.lead").unwrap().from = Some(50);
        first.save_cursor();
        held::written();

        // The next lookout, as it starts.
        let mut again = service(store.clone(), &[]);
        again.live = false;
        again.resume = marks(&store).0;
        again.kept = told_marks(&store);
        again
            .lookouts
            .insert("calc.lead".into(), lead("calc.lead", "calc.fix"));
        again
            .lookouts
            .insert("shop.lead".into(), lead("shop.lead", "shop.fix"));
        again.resume = again.floor().or(again.resume);
        again.mark_told();
        assert_eq!(again.resume, Some(49));
        assert_eq!(again.lookouts["calc.lead"].told_to, None);
        assert_eq!(again.lookouts["shop.lead"].told_to, Some(90));
        again.go_live();
        // A task's turn that ended between the two marks is news to the
        // lead that never heard it, and history to the one told it.
        for cursor in [60, 61] {
            let kind = if cursor == 60 {
                "accepted"
            } else {
                "turn_finished"
            };
            again.hand(
                &at("calc.fix", 7, kind, cursor),
                &hearing(&["calc.lead"]),
                Some(cursor),
                true,
            );
            again.hand(
                &at("shop.fix", 3, kind, cursor),
                &hearing(&["shop.lead"]),
                Some(cursor),
                true,
            );
        }
        // The person's words and the turn's reply, to read.
        assert_eq!(again.lookouts["calc.lead"].due.len(), 2);
        assert_eq!(again.lookouts["calc.lead"].from, Some(60));
        let shop = &again.lookouts["shop.lead"];
        assert!(shop.due.is_empty() && !shop.untold() && !shop.live);
        // Its mark kept while the stream is behind it.
        assert_eq!(again.lookouts["shop.lead"].mark(again.cursor), Some(90));
        // Past its mark, what it hears is news again.
        for (cursor, kind) in [(91, "accepted"), (92, "turn_finished")] {
            again.hand(
                &at("shop.fix", 4, kind, cursor),
                &hearing(&["shop.lead"]),
                Some(cursor),
                true,
            );
        }
        let shop = &again.lookouts["shop.lead"];
        assert_eq!(shop.due.len(), 2);
        assert!(shop.told_to.is_none() && shop.live);
        // The replay's end catches up any mark still ahead.
        let calc = again.lookouts.get_mut("calc.lead").unwrap();
        calc.told_to = Some(500);
        calc.live = false;
        calc.caught_up();
        assert!(calc.told_to.is_none() && calc.live);
        std::fs::remove_dir_all(&dir).unwrap();
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
            "Lookout · watching · 1 update in the last day"
        );
        // What a full lookout left waiting, and did not keep, shows too.
        append(
            &log,
            &json!({"at_ms": now, "lead": "calc.lead", "woke": [], "waiting": 40}),
        );
        append(
            &log,
            &json!({"at_ms": now, "lead": "calc.lead", "source": "chat", "skipped": "full"}),
        );
        assert_eq!(
            status(&store, "calc.lead"),
            "Lookout · watching · 2 updates in the last day · 40 waiting · 1 not kept"
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
        let records = HashMap::from([
            ("shop.lead.billing".to_owned(), json!({})),
            (
                "shop.invoices".to_owned(),
                json!({"created_by": "shop.lead.billing"}),
            ),
            ("shop.stray".to_owned(), json!({})),
        ]);
        lookout.roots(&records, &Made::of(&records));
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
        let records = HashMap::from([
            (
                "shop.invoices".to_owned(),
                json!({"created_by": "shop.lead.billing"}),
            ),
            (
                "shop.invoices-pdf".to_owned(),
                json!({"created_by": "shop.invoices"}),
            ),
            ("shop.stray".to_owned(), json!({})),
        ]);
        lookout.roots(&records, &Made::of(&records));
        assert!(lookout.watches("shop.stray"));
        assert!(!lookout.watches("shop.invoices"));
        assert!(!lookout.watches("shop.invoices-pdf"));
        // One the gone lead's name makes later is theirs too.
        lookout.lineage_of(&json!({"event": "created", "bot": "shop.receipts",
            "data": {"created_by": "shop.lead.billing"}}));
        assert!(!lookout.watches("shop.receipts"));
    }

    fn slack(text: &str, urgent: bool) -> sources::Event {
        sources::Event {
            source: "slack".into(),
            id: None,
            topic: None,
            text: text.into(),
            from: Some("ana".into()),
            details: None,
            quote: None,
            steer: urgent,
            steer_by: None,
            digest: None,
            fields: std::sync::Arc::new(json!({"text": text})),
            lookout: false,
            at_ms: 1,
            seq: 0,
        }
    }

    #[test]
    fn the_lookout_s_word_on_a_source_reaches_every_subscription_to_it() {
        let source = agent_client::config::Source {
            name: "slack".into(),
            command: "true".into(),
            every: None,
            digest: None,
            steer: None,
            watch: Vec::new(),
            icon: None,
            wants: None,
        };
        let down = sources::Event::of_lookout(&source, "down", "down".into(), json!({}));
        // A topic of it, and a filter the notice never matches, hear it.
        let mut topical = hears("eng");
        topical.sources = vec!["slack:eng".into()];
        topical.filter = Some(json!({"from": ["boss"]}));
        assert!(topical.hears(&down));
        assert!(!topical.hears(&slack("lunch?", false)));
        // Another source's subscription does not.
        let mut other = hears("ci");
        other.sources = vec!["ci".into()];
        assert!(!other.hears(&down));
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
        assert!(lookout.heard.is_empty());
        // Every event is told, with each subscription it came through.
        lookout.subs.push(hears("chat"));
        lookout.subs.push(hears("eng"));
        lookout.heard_from(&slack("lunch?", false));
        assert_eq!(lookout.heard.len(), 1);
        assert_eq!(lookout.heard[0].subs, ["chat", "eng"]);
        // A subscriber that never rests keeps the oldest it can hold, and
        // counts what it did not keep for its next note.
        lookout.waiting = 8;
        for n in 1..8 + 5 {
            lookout.heard_from(&slack(&n.to_string(), false));
        }
        assert_eq!(lookout.heard.len(), 8);
        assert_eq!(lookout.heard[0].event.text, "lunch?");
        assert_eq!(lookout.heard[7].event.text, "7");
        assert_eq!(lookout.unkept.count, 5);
        assert_eq!(lookout.unkept.sources, ["slack".to_owned()].into());
        // One that steers takes the place of the newest that does not.
        lookout.heard_from(&slack("prod is down", true));
        assert_eq!(lookout.heard.len(), 8);
        assert_eq!(lookout.heard[6].event.text, "6");
        assert_eq!(lookout.heard[7].event.text, "prod is down");
        assert_eq!(lookout.unkept.count, 6);
        // Events still waiting follow the subscriptions as they change.
        lookout.subs.retain(|sub| sub.name == "eng");
        lookout.rehear();
        assert_eq!(lookout.heard.len(), 8);
        assert!(lookout.heard.iter().all(|heard| heard.subs == ["eng"]));
        lookout.subs.clear();
        lookout.rehear();
        assert!(lookout.heard.is_empty());
    }

    #[test]
    fn a_subscription_s_digest_and_urgent_read_its_sources_too() {
        let mut lookout = lookout();
        let mut chat = hears("chat");
        chat.digest = Some("1h".into());
        chat.steer = Some(json!({"text": [{"prefix": "down"}]}));
        lookout.subs.push(chat);
        lookout.subs.push(hears("eng"));
        let now = Instant::now();
        lookout.heard_from(&slack("lunch?", false));
        assert!(lookout.ready(&lookout.heard[0], now));
        // Told lately, the subscription's own digest holds what only it
        // hears, though its source has none.
        lookout.told_subs.insert("chat".into(), now);
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
        // What it marks urgent is, and is counted as its steer.
        lookout.heard_from(&slack("down: prod", false));
        let down = &lookout.heard[1];
        assert!(down.event.steer && lookout.ready(down, now));
        assert_eq!(down.event.steer_by.as_deref(), Some("chat"));
        assert_eq!(
            sources::untimed(&sources::line(down)),
            "[slack · ana]\ndown: prod"
        );
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
            lookout.lead_id = Some(1);
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
        let ago = again.told_sources["slack"].elapsed().as_secs();
        assert!((59..=61).contains(&ago), "{ago}");
        // A note under way when it stopped goes again first, as it was,
        // under the same request.
        let mut sending = made();
        sending.heard_from(&slack("two", false));
        let asked = sources::note(&sending.heard, &[], None);
        let sent = std::mem::take(&mut sending.heard);
        sending.sending = Some((sent, vec!["k".into()]));
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
        assert_eq!(sources::note(&back.resend, &[], None), asked);
        // Another bot of the name hears none of it.
        let mut other = Lookout::new("calc.lead");
        other.subs.push(hears("chat"));
        other.lead_id = Some(2);
        other.restore(path.clone());
        other.flush();
        held::written();
        assert!(other.heard.is_empty());
        assert!(!path.exists());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_subscription_that_lasts_so_many_notes_ends_after_them_across_a_restart() {
        let dir = std::env::temp_dir().join(format!("sonny-runs-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let path = dir.join("calc.lead.json");
        let made = || {
            let mut lookout = lookout();
            let mut twice = hears("twice");
            twice.runs = Some(2);
            twice.made = Some(7);
            lookout.subs.push(twice);
            lookout.subs.push(hears("always"));
            lookout.lead_id = Some(1);
            lookout.restore(path.clone());
            lookout.counting();
            lookout
        };
        let names =
            |names: &[&str]| -> Vec<String> { names.iter().map(|n| n.to_string()).collect() };
        let key = |k: &str| vec![k.to_owned()];
        let mut lookout = made();
        // One that lasts is never charged; one note told is one spent.
        lookout.charged = lookout.charges(names(&["always", "twice"]).iter());
        assert_eq!(lookout.charged, ["twice@7"]);
        lookout.spend(&key("a"));
        assert_eq!(lookout.runs_told["twice@7"].len(), 1);
        assert!(lookout.ended.is_empty());
        lookout.flush();
        held::written();
        // Watched again, it has one note to go. The same note sent again
        // after the restart is that one note, not another.
        let mut again = made();
        assert_eq!(again.runs_told["twice@7"].len(), 1);
        again.charged = again.charges(names(&["twice"]).iter());
        again.spend(&key("a"));
        assert!(again.ended.is_empty());
        again.charged = again.charges(names(&["twice"]).iter());
        again.spend(&key("b"));
        assert_eq!(again.ended[0].name, "twice");
        assert!(again.runs_told.is_empty());
        again.ended.clear();
        // One whose `until` came before the files are read again holds
        // every note back until the service ends it.
        assert!(!again.overdue());
        let mut late = hears("late");
        late.until = Some("2020-01-01 00:00".into());
        again.subs.push(late);
        assert!(again.overdue());
        assert_eq!(again.over[0].name, "late");
        // Replaced while its note is on the way, even in one call, the new
        // one is not charged for it and counts afresh.
        again.charged = again.charges(names(&["twice"]).iter());
        let twice = again
            .subs
            .iter_mut()
            .find(|sub| sub.name == "twice")
            .unwrap();
        twice.made = Some(8);
        again.counting();
        assert!(again.runs_told.is_empty());
        again.spend(&key("c"));
        assert!(again.runs_told.is_empty() && again.ended.is_empty());
        again
            .runs_told
            .insert("twice@8".into(), vec!["lookout-d".into()]);
        again.subs.retain(|sub| sub.name != "twice");
        again.counting();
        assert!(again.runs_told.is_empty());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_deleted_bot_ends_its_subscriptions_and_those_that_watched_only_it() {
        let dir = std::env::temp_dir().join(format!("sonny-ended-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let store = dir.join("state.sqlite");
        let add = |subscriber: &str, owner: i64, sub: Value| {
            subs::change(
                &store,
                subscriber,
                Some(owner),
                Some(&sub),
                None,
                &["ci".into()],
            )
            .unwrap();
        };
        add(
            "shop.fix",
            2,
            json!({"name": "mine", "agents": ["shop.lead"]}),
        );
        add(
            "shop.lead",
            1,
            json!({"name": "fix", "tasks_of": ["shop.fix"]}),
        );
        add(
            "shop.lead",
            1,
            json!({"name": "both", "agents": ["shop.fix", "shop.docs"]}),
        );
        add(
            "shop.lead",
            1,
            json!({"name": "glob", "agents": ["shop.*"]}),
        );
        add(
            "shop.lead",
            1,
            json!({"name": "ci", "agents": ["shop.fix"], "sources": ["ci"]}),
        );
        let lead = json!({"name": "shop.lead", "bot_id": 1});
        let docs = json!({"name": "shop.docs", "bot_id": 3});
        let mut service = service(store.clone(), &[lead, docs]);
        service.subscribers = subs::all(&store).into_iter().collect();
        assert!(service.ended_with("shop.fix", Some(2)));
        assert!(subs::read(&store, "shop.fix").is_empty());
        let names = |store: &Path| -> Vec<String> {
            subs::read(store, "shop.lead")
                .into_iter()
                .map(|s| s.name)
                .collect()
        };
        // One bot of two still there, a glob, a source, or the tasks a
        // maker made, which outlive it, keeps it.
        assert_eq!(names(&store), ["fix", "both", "glob", "ci"]);
        service.records.remove("shop.docs");
        service.subscribers = subs::all(&store).into_iter().collect();
        assert!(service.ended_with("shop.docs", Some(3)));
        assert_eq!(names(&store), ["fix", "glob", "ci"]);
        // Nothing left that it ends: the files are not read again for it.
        service.subscribers = subs::all(&store).into_iter().collect();
        assert!(!service.ended_with("shop.docs", Some(3)));
        // A delete the lookout was away for: a subscriber no longer listed
        // ends whole, and one still listed keeps what its bot holds.
        add(
            "shop.gone",
            4,
            json!({"name": "old", "agents": ["shop.lead"]}),
        );
        service.subscribers = subs::all(&store).into_iter().collect();
        let after = std::time::SystemTime::now() + Duration::from_secs(5);
        assert!(service.unheld(after));
        assert!(subs::read(&store, "shop.gone").is_empty());
        assert_eq!(names(&store), ["fix", "glob", "ci"]);
        service.subscribers = subs::all(&store).into_iter().collect();
        assert!(!service.unheld(after), "nothing more");
        // A file written since the listing is left: a bot made since may
        // have written it.
        add(
            "shop.new",
            5,
            json!({"name": "new", "agents": ["shop.lead"]}),
        );
        service.subscribers = subs::all(&store).into_iter().collect();
        let before = std::time::SystemTime::now() - Duration::from_secs(5);
        assert!(!service.unheld(before));
        assert_eq!(subs::read(&store, "shop.new").len(), 1);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn an_event_goes_at_once_and_what_follows_it_closely_goes_together() {
        let (client, mut got) = submits().await;
        let mut lookout = lookout();
        lookout.live = true;
        lookout.subs.push(hears("chat"));
        let (woken, mut woke) = tokio::sync::mpsc::channel(4);
        let answer = |lookout: &mut Lookout, woke: &mut tokio::sync::mpsc::Receiver<_>| {
            let (_, _, answer) = woke.try_recv().unwrap();
            lookout.answered(answer);
        };
        // None went lately: the first goes as it comes.
        lookout.heard_from(&slack("one", false));
        lookout.wake(&client, &woken);
        assert_eq!(
            told(got.recv().await.unwrap()["prompt"].as_str().unwrap()),
            "[slack · ana]\none"
        );
        tokio::time::sleep(Duration::from_millis(50)).await;
        answer(&mut lookout, &mut woke);
        // What comes within the group interval waits for it to end, together.
        lookout.heard_from(&slack("two", false));
        lookout.heard_from(&slack("three", false));
        lookout.wake(&client, &woken);
        tokio::time::sleep(Duration::from_millis(50)).await;
        assert!(got.try_recv().is_err());
        // A steer waits for none, and takes along what waits.
        lookout.heard_from(&slack("down", true));
        lookout.wake(&client, &woken);
        let prompt = got.recv().await.unwrap()["prompt"].clone();
        assert_eq!(
            told(prompt.as_str().unwrap()),
            "[slack · ana]\ntwo\n[slack · ana]\nthree\n[slack · ana]\ndown"
        );
        tokio::time::sleep(Duration::from_millis(50)).await;
        answer(&mut lookout, &mut woke);
        lookout.heard_from(&slack("four", false));
        lookout.wake(&client, &woken);
        tokio::time::sleep(Duration::from_millis(50)).await;
        assert!(got.try_recv().is_err());
        // Once the group interval has passed it goes.
        lookout.sent_ms -= GROUP_INTERVAL.as_millis();
        lookout.wake(&client, &woken);
        assert_eq!(
            told(got.recv().await.unwrap()["prompt"].as_str().unwrap()),
            "[slack · ana]\nfour"
        );
    }

    #[tokio::test]
    async fn a_digest_tells_its_source_at_most_once_a_while() {
        let (client, mut got) = submits().await;
        let mut lookout = lookout();
        lookout.live = true;
        lookout.subs.push(hears("chat"));
        lookout.lead_id = Some(1);
        let hour = Duration::from_secs(3600);
        let digested = |text: &str, urgent| sources::Event {
            digest: Some(hour),
            ..slack(text, urgent)
        };
        // The first is told as it comes.
        lookout.heard_from(&digested("one", false));
        wake(&mut lookout, &client).await;
        assert!(
            got.recv().await.unwrap()["prompt"]
                .as_str()
                .unwrap()
                .contains("one")
        );
        // The next waits out the digest.
        lookout.heard_from(&digested("two", false));
        wake(&mut lookout, &client).await;
        assert_eq!(lookout.heard.len(), 1);
        assert!(got.try_recv().is_err());
        // What is urgent does not, and the held one waits on.
        lookout.heard_from(&digested("down", true));
        wake(&mut lookout, &client).await;
        let prompt = told(got.recv().await.unwrap()["prompt"].as_str().unwrap());
        assert!(
            prompt.contains("down") && !prompt.contains("two"),
            "{prompt}"
        );
        assert_eq!(lookout.heard.len(), 1);
        // Once it has passed, what waited is told.
        let past = Instant::now() - hour;
        lookout.told_sources.insert("slack".into(), past);
        wake(&mut lookout, &client).await;
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

    /// `wake_sources`, and what the daemon answered it, if it sent.
    async fn wake(lookout: &mut Lookout, client: &Arc<Client>) {
        // As if the group interval since the last note had passed.
        lookout.sent_ms = 0;
        let (woken, mut woke) = tokio::sync::mpsc::channel(4);
        lookout.wake(client, &woken);
        drop(woken);
        while let Some((_, _, answer)) = woke.recv().await {
            lookout.answered(answer);
        }
    }

    #[tokio::test]
    async fn a_note_under_way_at_a_stop_goes_again_first_as_it_was() {
        let (client, mut got) = submits().await;
        let mut before = lookout();
        before.live = true;
        before.subs.push(hears("chat"));
        before.heard_from(&slack("first", false));
        wake(&mut before, &client).await;
        let first = got.recv().await.unwrap();
        // As if it stopped before the daemon's answer came: the same note,
        // under the same request.
        let event = slack("first", false);
        let sent = vec![sources::Heard {
            event,
            subs: vec!["chat".into()],
            order: 0,
        }];
        let mut again = lookout();
        again.live = true;
        again.subs.push(hears("chat"));
        again.resend = sent;
        again.heard_from(&slack("down", true));
        wake(&mut again, &client).await;
        let resent = got.recv().await.unwrap();
        assert_eq!(resent["request_id"], first["request_id"]);
        // What came since goes next.
        wake(&mut again, &client).await;
        let urgent = got.recv().await.unwrap();
        assert_eq!(urgent["delivery"], "steer");
        assert!(urgent["prompt"].as_str().unwrap().contains("down"));
    }

    #[tokio::test]
    async fn what_a_full_lookout_did_not_keep_is_told_until_a_note_lands() {
        let dir = std::env::temp_dir().join(format!("sonny-unkept-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let path = dir.join("calc.lead.json");
        let made = || {
            let mut lookout = lookout();
            lookout.live = true;
            lookout.subs.push(hears("chat"));
            lookout.lead_id = Some(1);
            lookout.waiting = 2;
            lookout
        };
        let mut lookout = made();
        lookout.restore(path.clone());
        for text in ["one", "two", "three", "four"] {
            lookout.heard_from(&slack(text, false));
        }
        assert_eq!(lookout.unkept.count, 2);
        // Kept on disk with what waits, so a restart still tells it.
        lookout.flush();
        held::written();
        let mut lookout = made();
        lookout.restore(path.clone());
        assert_eq!(lookout.heard.len(), 2);
        assert_eq!(lookout.unkept.count, 2);
        // The next note says it, once.
        let (client, mut got) = submits().await;
        wake(&mut lookout, &client).await;
        let prompt = told(got.recv().await.unwrap()["prompt"].as_str().unwrap());
        assert!(prompt.starts_with("[slack · ana]\none\n"), "{prompt}");
        assert!(
            prompt.ends_with(
                "[lookout]\n2 more events from slack came less than a minute ago, and they were \
                 not kept: 2 were waiting for you already"
            ),
            "{prompt}"
        );
        assert_eq!(lookout.unkept.count + lookout.unkept_sent.count, 0);
        // A note refused says it again in the next.
        lookout.heard_from(&slack("five", false));
        lookout.heard_from(&slack("six", false));
        let sent = std::mem::take(&mut lookout.heard);
        lookout.unkept.add("slack", epoch_ms() as u64);
        lookout.unkept_sent = std::mem::take(&mut lookout.unkept);
        lookout.sending = Some((sent, vec!["k".into()]));
        lookout.answered(Err("bot_busy".into()));
        assert_eq!(lookout.unkept.count, 1);
        assert_eq!(lookout.heard.len(), 2);
        // A note under way when the lookout stops says it again when it goes
        // again, even with nothing else waiting.
        let sent = std::mem::take(&mut lookout.heard);
        lookout.unkept_sent = std::mem::take(&mut lookout.unkept);
        lookout.sending = Some((sent, vec!["k".into()]));
        lookout.keep();
        lookout.flush();
        held::written();
        let mut lookout = made();
        lookout.restore(path.clone());
        assert_eq!((lookout.resend.len(), lookout.unkept.count), (2, 0));
        wake(&mut lookout, &client).await;
        let prompt = told(got.recv().await.unwrap()["prompt"].as_str().unwrap());
        assert!(prompt.starts_with("[slack · ana]\nfive\n"), "{prompt}");
        assert!(
            prompt.contains("[lookout]\n1 more event from slack came"),
            "{prompt}"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_note_the_daemon_already_took_starts_the_digest() {
        let mut lookout = lookout();
        let sent = vec![sources::Heard {
            event: slack("first", false),
            subs: vec!["chat".into()],
            order: 0,
        }];
        lookout.sending = Some((sent, vec!["k".into()]));
        lookout.answered(Err("idempotency_conflict".into()));
        assert!(lookout.told_sources.contains_key("slack"));
        assert!(lookout.heard.is_empty());
    }

    #[tokio::test]
    async fn sources_are_told_with_the_lookout_s_origin_as_a_steer() {
        let (client, mut got) = submits().await;
        let mut lookout = lookout();
        lookout.live = true;
        lookout.subs.push(hears("chat"));
        lookout.heard_from(&slack("lunch?", false));
        wake(&mut lookout, &client).await;
        let submit = got.recv().await.unwrap();
        assert_eq!(submit["origin"], ORIGIN);
        assert_eq!(submit["delivery"], "steer");
        assert!(
            submit["request_id"]
                .as_str()
                .unwrap()
                .starts_with("lookout-")
        );
        // The events, each under its header, and the trailer: no other words.
        assert_eq!(
            told(submit["prompt"].as_str().unwrap()),
            "[slack · ana]\nlunch?"
        );
        assert!(lookout.heard.is_empty());
        // A note stays under its bytes however long the text; the rest
        // waits for the next, and the note says so.
        let long = "x".repeat(sources::TEXT_CHARS);
        for _ in 0..12 {
            lookout.heard_from(&slack(&long, true));
        }
        wake(&mut lookout, &client).await;
        let prompt = told(got.recv().await.unwrap()["prompt"].as_str().unwrap());
        assert!(prompt.len() <= sources::NOTE_BYTES, "{}", prompt.len());
        let told = prompt.matches("[slack · ana]").count();
        assert!(told > 0 && lookout.heard.len() == 12 - told, "{told}");
        assert!(
            prompt.contains(&format!(
                "[lookout]\n{} more events wait for the next note, the first heard ",
                12 - told
            )),
            "{prompt}"
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
            lookout.lead_id = Some(1);
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
            steer: None,
            watch: Vec::new(),
            icon: None,
            wants: None,
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
                crate::commons::call(&crash.home, by, None, by, None, None, &arguments).unwrap()
            };
            post(
                "calc.lead",
                json!({"post": {"name": "t", "title": "T", "text": "x"}}),
            );
            // The first look tells none of the past.
            let first = crate::commons::news(&crash.home, &crash.cursor());
            assert!(first.lines.is_empty());
            assert!(first.mark_read());
            // Each had read the thread, so its reply is posted.
            crate::commons::told(&crash.home, "b.y", "t", &[0]);
            post("b.y", json!({"reply": {"thread": "t", "text": "one"}}));
            crate::commons::told(&crash.home, "c.z", "t", &[0, 1]);
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
