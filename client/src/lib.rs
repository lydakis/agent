//! The daemon's socket protocol for human-facing clients: one session,
//! JSONL requests correlated by `id`, notifications forwarded on a channel.
//! Shared by Sonny and the CLI; nothing here depends on the runtime crate.
pub mod approver;
pub mod clock;
pub mod config;
pub mod env_file;
pub mod files;
pub mod hidden;
pub mod judge;
pub mod memory;
pub mod models;
pub mod policy;
pub mod project;
pub mod rules;
pub mod socket;
pub mod test_home;

#[cfg(test)]
test_home!();

use serde_json::{Value, json};
use std::{collections::HashMap, os::fd::AsRawFd, path::Path, sync::Arc, time::Duration};
use tokio::{
    io::{AsyncBufReadExt, AsyncWriteExt, BufReader},
    net::{UnixStream, unix::OwnedWriteHalf},
    sync::{Mutex, OwnedSemaphorePermit, Semaphore, mpsc, oneshot},
};

#[derive(Debug, Clone)]
pub struct Error {
    pub code: String,
    pub detail: Option<String>,
    /// Facts the daemon sent beside `detail`, such as `running_turn` and
    /// `fork_point` on `bot_busy`, so a caller need not parse the detail or
    /// ask again.
    pub facts: Option<Box<serde_json::Map<String, Value>>>,
}

impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match &self.detail {
            Some(detail) => write!(f, "{} ({detail})", self.code),
            None => f.write_str(&self.code),
        }
    }
}
impl Error {
    pub fn new(code: &str) -> Self {
        Self {
            code: code.to_owned(),
            detail: None,
            facts: None,
        }
    }
    pub fn with(code: &str, detail: &str) -> Self {
        Self {
            detail: Some(detail.to_owned()),
            ..Self::new(code)
        }
    }
}
impl From<std::io::Error> for Error {
    fn from(error: std::io::Error) -> Self {
        Self::with("io", &error.to_string())
    }
}
impl From<serde_json::Error> for Error {
    fn from(error: serde_json::Error) -> Self {
        Self::with("invalid_json", &error.to_string())
    }
}
pub type Result<T> = std::result::Result<T, Error>;

/// A new bot's tools when its client names none: the daemon's own, with
/// the file editor its model family is trained on, Codex's `apply_patch`
/// for a Responses model and Claude Code's `edit` for an Anthropic one.
/// `providers` is `ready.providers`; a model whose provider it does not
/// name gets `edit`. Web search comes too where the provider offers it,
/// unless `search` is the person's `web_search = "disabled"`, as Codex
/// turns it on, or `off` (their `web_search_off`) names the model.
pub fn default_tools(
    providers: &Value,
    model: &str,
    search: config::WebSearch,
    off: &[String],
) -> Vec<&'static str> {
    let provider = model
        .split_once('/')
        .map_or(model, |(provider, _)| provider);
    let editor = match providers[provider]["family"].as_str() {
        Some("responses") => "apply_patch",
        _ => "edit",
    };
    let mut tools = vec![
        "shell",
        "write_stdin",
        "read",
        "write",
        editor,
        "wait",
        "history",
        "send_message",
        "list_agents",
        "pause_agents",
        "spawn_agent",
        "close_agent",
    ];
    if search == config::WebSearch::Live {
        // Search is the provider's to offer; the daemon fetches a page
        // for any.
        if providers[provider]["web_search"] == true
            && !off.iter().any(|pattern| config::matches(pattern, model))
        {
            tools.push("web_search");
        }
        tools.push("web_fetch");
    }
    tools
}

/// The socket protocol's version in `ready`. The daemon and its clients ship
/// together, so a client refuses a daemon of any other version.
pub const PROTOCOL: u64 = 87;
static PROGRAM: std::sync::OnceLock<&'static str> = std::sync::OnceLock::new();
/// Name this process's connections as `program` (such as `window` or
/// `reviewer`): each later [`Client::connect`] says it in its first line,
/// and the daemon's `stats` counts its requests under it. The first name
/// set stays.
pub fn name_program(program: &'static str) {
    let _ = PROGRAM.set(program);
}
static LABEL: std::sync::OnceLock<String> = std::sync::OnceLock::new();
/// The id of a connection's `hello`, whose answer no one waits for.
const HELLO: u64 = u64::MAX;
/// The `hello` naming this process's connections, once it has a program:
/// with its label once [`Client::label`] gave one.
fn hello() -> Option<Vec<u8>> {
    let program = PROGRAM.get()?;
    let mut hello = json!({"id": HELLO, "op": "hello", "program": program});
    if let Some(label) = LABEL.get() {
        hello["label"] = json!(label);
    }
    let mut line = hello.to_string().into_bytes();
    line.push(b'\n');
    Some(line)
}
/// How long the daemon holds a turn on a cause a person can fix before it
/// fails with it, for a bot that sets no `hold_ms`: five days, as mail is
/// retried (RFC 5321 suggests four to five) before it is returned.
pub const HOLD_MS: u64 = 5 * 86_400_000;

/// Variables naming the terminal a daemon was started in: a terminal
/// multiplexer's place. A client starting the daemon leaves them out of its
/// tool shells (`--shell-exclude`), as a command that used one would land
/// in that terminal, not the bot's.
pub const TERMINAL_PLACES: [&str; 12] = [
    "REX_SESSION",
    "REX_BLOCK",
    "TMUX",
    "TMUX_PANE",
    "HERDR_ENV",
    "HERDR_WORKSPACE_ID",
    "HERDR_TAB_ID",
    "HERDR_PANE_ID",
    "CMUX_WORKSPACE_ID",
    "CMUX_SURFACE_ID",
    "CMUX_TAB_ID",
    "CMUX_PANEL_ID",
];

/// Folders at the top of a bot's writable folders that stay read-only and,
/// when missing, may not be made, as Codex keeps its own metadata: what git
/// runs (hooks, config), and what agents trust as the project's settings.
/// A client starting the daemon passes them (`--protect`).
pub const PROTECTED: [&str; 3] = [".git", ".agents", ".codex"];

/// Folders kept read-only where they exist (`--protect-existing`): AWS
/// profiles, which can name a program to run for credentials.
pub const PROTECTED_EXISTING: [&str; 1] = [".aws"];

/// System folders a Linux sandbox shows from a copy the person owns
/// (`--own-copy`): its user namespace maps the person alone, so root's
/// files show as nobody's, and ssh refuses each file an `Include` names
/// that way ("Bad owner or permissions"). Debian's ssh_config includes
/// the first, so no sandboxed command could ssh; Codex has the same bug
/// open (openai/codex#9286). Fedora's, RHEL's and Amazon Linux's include
/// the second's `openssh.config` too, a link into `/usr/share`, whose
/// file the copy holds.
pub const OWN_COPIES: [&str; 2] = ["/etc/ssh/ssh_config.d", "/etc/crypto-policies/back-ends"];

/// The person's ssh config, `~/.ssh/config`, which a Linux sandbox shows
/// from a copy with [`SSH_CONFIG_FIRST`] before it (`--own-copy-file`):
/// with `ControlMaster auto` and no master open, ssh would make its socket
/// in `~/.ssh`, read-only to every sandboxed command, and fail, so would
/// every git push and fetch over ssh.
pub fn ssh_config() -> Option<std::path::PathBuf> {
    std::env::var_os("HOME").map(|home| std::path::PathBuf::from(home).join(".ssh/config"))
}

/// What a sandbox's copy of the person's ssh config says before the
/// config. ssh takes the first value it reads for each option, so this
/// one stands for every host: with `ControlMaster auto`, ssh with no
/// master open makes its control socket at `ControlPath`, usually in
/// `~/.ssh`, and exits 255 when it can't (`unix_listener: cannot bind ...
/// Read-only file system`). With `no`, it joins a master the person has
/// open there, and connects on its own when none is.
pub const SSH_CONFIG_FIRST: &str = "\
# The agent sandbox shows this copy of the ssh config: ssh in a sandboxed
# command joins a master the person has open and connects on its own when
# none is, as its control socket would go in a folder it can't write.
ControlMaster no

";

/// The files a daemon this CLI starts shows in every Linux sandbox from a
/// copy with text before it (`--own-copy-file FILE=FIRST`): the person's
/// ssh config, with [`SSH_CONFIG_FIRST`].
pub fn own_copy_files() -> Vec<(std::path::PathBuf, String)> {
    ssh_config()
        .map(|config| (config, SSH_CONFIG_FIRST.to_owned()))
        .into_iter()
        .collect()
}

/// Start `command` in a session of its own, for a service that outlives the
/// terminal it was started from: closing a Herdr pane, or stopping Herdr,
/// ends every process in the pane's session, not only its foreground job.
pub fn own_session(command: &mut std::process::Command) {
    use std::os::unix::process::CommandExt;
    // SAFETY: setsid is async-signal-safe and touches no parent state.
    unsafe {
        command.pre_exec(|| {
            if libc::setsid() < 0 {
                return Err(std::io::Error::last_os_error());
            }
            Ok(())
        });
    }
}

// No await occurs while this lock is held. Synchronous removal makes dropping
// a request release its registration immediately, without a cleanup task.
type Pending = Arc<std::sync::Mutex<HashMap<u64, oneshot::Sender<Value>>>>;
struct PendingRequest<'a> {
    pending: &'a Pending,
    id: u64,
}
impl Drop for PendingRequest<'_> {
    fn drop(&mut self) {
        self.pending.lock().unwrap().remove(&self.id);
    }
}

// Dropping write_all may leave a partial JSONL frame on the socket. Shut down
// both halves before releasing the writer lock, so nobody appends a new frame
// to that prefix and the reader releases other pending requests.
struct FrameWrite<'a> {
    fd: std::os::fd::RawFd,
    closed: &'a std::sync::atomic::AtomicBool,
    complete: bool,
}
impl Drop for FrameWrite<'_> {
    fn drop(&mut self) {
        if !self.complete {
            self.closed.store(true, std::sync::atomic::Ordering::SeqCst);
            // SAFETY: request holds the client and writer lock until after this
            // guard drops, so fd still names this socket. shutdown does not close fd.
            unsafe { libc::shutdown(self.fd, libc::SHUT_RDWR) };
        }
    }
}

/// Notifications queued ahead of the UI before the session is let go.
/// Larger than any replay page, so a normal attach never trips it.
const QUEUE: usize = 4096;
/// The bytes of notifications, as sent, queued ahead of the UI. A caller
/// that keeps notifications past this queue carries the same budget on
/// with [`Events::recv_sized`].
pub const QUEUE_BYTES: usize = 8 * 1024 * 1024;

struct Notification {
    encoded: String,
    _bytes: Option<OwnedSemaphorePermit>,
}

/// Encoded notifications share an 8 MiB budget, released as callers drain them.
/// Keeping the wire representation also bounds allocations for nested JSON,
/// and each notification is built once, when it is taken.
pub struct Events(mpsc::Receiver<Notification>);
impl Events {
    pub async fn recv(&mut self) -> Option<Value> {
        loop {
            if let Some(event) = self.0.recv().await?.decode() {
                return Some(event);
            }
        }
    }
    pub fn try_recv(&mut self) -> std::result::Result<Value, mpsc::error::TryRecvError> {
        loop {
            if let Some(event) = self.0.try_recv()?.decode() {
                return Ok(event);
            }
        }
    }
    /// The next notification with its size as sent, which it held of
    /// [`QUEUE_BYTES`]: a caller that queues it again charges that much
    /// to a budget of its own, so the bound holds past this queue.
    pub async fn recv_sized(&mut self) -> Option<(Value, usize)> {
        loop {
            let notification = self.0.recv().await?;
            let size = notification.encoded.len();
            if let Some(event) = notification.decode() {
                return Some((event, size));
            }
        }
    }
    pub fn try_recv_sized(
        &mut self,
    ) -> std::result::Result<(Value, usize), mpsc::error::TryRecvError> {
        loop {
            let notification = self.0.try_recv()?;
            let size = notification.encoded.len();
            if let Some(event) = notification.decode() {
                return Ok((event, size));
            }
        }
    }
}
impl Notification {
    /// The notification, or nothing for a line the reader routed by its
    /// envelope that is not JSON a value can hold, as the reader passes by
    /// a line it cannot read.
    fn decode(&self) -> Option<Value> {
        serde_json::from_str(&self.encoded).ok()
    }
}

/// What the reader reads of a line to route it: the id of the request it
/// answers, when it is one. The rest is checked as JSON and skipped, not
/// built, so a notification is built once, by whoever takes it. A line is
/// a response when it is an object whose `id` is a whole number a request
/// is made under; any other line is a notification.
struct Envelope(Option<u64>);
impl<'de> serde::Deserialize<'de> for Envelope {
    fn deserialize<D: serde::Deserializer<'de>>(
        deserializer: D,
    ) -> std::result::Result<Self, D::Error> {
        Ok(Self(
            deserializer.deserialize_any(Routing { object: true })?,
        ))
    }
}
/// Reads a line (`object`) or its `id`, building nothing.
struct Routing {
    object: bool,
}
struct Id(Option<u64>);
impl<'de> serde::Deserialize<'de> for Id {
    fn deserialize<D: serde::Deserializer<'de>>(
        deserializer: D,
    ) -> std::result::Result<Self, D::Error> {
        Ok(Self(
            deserializer.deserialize_any(Routing { object: false })?,
        ))
    }
}
/// Whether an object's key is `id`.
struct IsId(bool);
impl<'de> serde::Deserialize<'de> for IsId {
    fn deserialize<D: serde::Deserializer<'de>>(
        deserializer: D,
    ) -> std::result::Result<Self, D::Error> {
        struct Key;
        impl serde::de::Visitor<'_> for Key {
            type Value = IsId;
            fn expecting(&self, formatter: &mut std::fmt::Formatter) -> std::fmt::Result {
                formatter.write_str("a key")
            }
            fn visit_str<E>(self, key: &str) -> std::result::Result<IsId, E> {
                Ok(IsId(key == "id"))
            }
        }
        deserializer.deserialize_str(Key)
    }
}
impl<'de> serde::de::Visitor<'de> for Routing {
    type Value = Option<u64>;
    fn expecting(&self, formatter: &mut std::fmt::Formatter) -> std::fmt::Result {
        formatter.write_str("any JSON value")
    }
    fn visit_u64<E>(self, id: u64) -> std::result::Result<Option<u64>, E> {
        Ok((!self.object).then_some(id))
    }
    fn visit_i64<E>(self, id: i64) -> std::result::Result<Option<u64>, E> {
        Ok(u64::try_from(id).ok().filter(|_| !self.object))
    }
    fn visit_f64<E>(self, _: f64) -> std::result::Result<Option<u64>, E> {
        Ok(None)
    }
    fn visit_bool<E>(self, _: bool) -> std::result::Result<Option<u64>, E> {
        Ok(None)
    }
    fn visit_str<E>(self, _: &str) -> std::result::Result<Option<u64>, E> {
        Ok(None)
    }
    fn visit_unit<E>(self) -> std::result::Result<Option<u64>, E> {
        Ok(None)
    }
    fn visit_seq<A: serde::de::SeqAccess<'de>>(
        self,
        mut seq: A,
    ) -> std::result::Result<Option<u64>, A::Error> {
        while seq.next_element::<serde::de::IgnoredAny>()?.is_some() {}
        Ok(None)
    }
    fn visit_map<A: serde::de::MapAccess<'de>>(
        self,
        mut map: A,
    ) -> std::result::Result<Option<u64>, A::Error> {
        // As a parsed object would, the last `id` counts.
        let mut id = None;
        while let Some(IsId(is_id)) = map.next_key()? {
            if is_id && self.object {
                id = map.next_value::<Id>()?.0;
            } else {
                map.next_value::<serde::de::IgnoredAny>()?;
            }
        }
        Ok(id)
    }
}

pub struct Client {
    writer: Mutex<OwnedWriteHalf>,
    pending: Pending,
    next: std::sync::atomic::AtomicU64,
    /// Set by the reader on its way out. A request made after it can still
    /// be written, but nothing would ever answer; it fails here instead.
    closed: Arc<std::sync::atomic::AtomicBool>,
    /// The store's lineage from `ready`: drawn when the store was made and
    /// kept by a copy, whichever socket its daemon listens on.
    store: Option<String>,
    /// The store's identity from `ready`: its lineage in this very file,
    /// which a replaced or copied file does not keep.
    identity: Option<String>,
    /// `ready.providers`: each binding's family and the efforts it takes.
    providers: Value,
    /// `ready.speeds`: the tiers the person says models serve, by
    /// `PROVIDER/MODEL` pattern, most specific first.
    speeds: Vec<(String, Vec<String>)>,
    paused: Value,
    /// `ready.version`: the daemon's release.
    version: Option<String>,
    /// `ready.limits.depth`: the most generations below the bot that
    /// starts its line a bot may be made at; `None` for no bound.
    max_depth: Option<u64>,
    /// Requests this client keeps in flight at once, when bounded.
    in_flight: std::sync::OnceLock<Semaphore>,
}

/// A JSON list of names, as `ready` gives a model's tiers.
fn names(list: &Value) -> Vec<String> {
    (list.as_array().into_iter().flatten())
        .filter_map(|name| name.as_str().map(str::to_owned))
        .collect()
}

impl Client {
    /// Connect and wait for the daemon's `ready` line. Notifications (lines
    /// without an `id`) go to the returned receiver; a closed receiver means
    /// the daemon hung up.
    ///
    /// The reader never waits for the UI: a response the UI is awaiting must
    /// not sit behind notifications it has not drained, or both deadlock.
    /// The queue is bounded all the same, so a UI slower than the fleet
    /// cannot grow it without limit. When it fills, the reader does what the
    /// daemon does to a lagging follower: it drops the session and tells the
    /// UI `follow_lagged`, and the UI attaches again from its cursor.
    pub async fn connect(socket: &Path) -> Result<(Arc<Self>, Events)> {
        let unavailable = |error: std::io::Error| {
            Error::with(
                "daemon_unavailable",
                &format!("{}: {error}", socket.display()),
            )
        };
        // Only the Linux sandbox broker can require a blocking read.
        let handed = if cfg!(target_os = "linux") && std::env::var_os(socket::HANDED).is_some() {
            let path = socket.to_owned();
            tokio::task::spawn_blocking(move || socket::handed(&path))
                .await
                .map_err(|_| Error::new("daemon_unavailable"))?
                .map_err(unavailable)?
        } else {
            None
        };
        let stream = match handed {
            Some(stream) => {
                stream.set_nonblocking(true).map_err(unavailable)?;
                UnixStream::from_std(stream).map_err(unavailable)?
            }
            None => tokio::time::timeout(Duration::from_secs(5), UnixStream::connect(socket))
                .await
                .map_err(|_| Error::new("daemon_connect_timeout"))?
                .map_err(unavailable)?,
        };
        let (read, mut write) = stream.into_split();
        let mut lines = BufReader::new(read).lines();
        // A daemon opening its store says it starts, every few seconds,
        // until it is ready: each line gives it the wait again.
        let ready = loop {
            let greeting = match tokio::time::timeout(Duration::from_secs(5), lines.next_line())
                .await
                .map_err(|_| Error::new("daemon_ready_timeout"))??
            {
                Some(line) => serde_json::from_str::<Value>(&line)?,
                None => return Err(Error::new("daemon_disconnected")),
            };
            if greeting["event"] != "starting" {
                break greeting;
            }
        };
        if ready["event"] != "ready" {
            // Not a daemon's greeting, so nothing it says about itself counts.
            return Err(Error::with(
                "daemon_greeting_invalid",
                &format!("{} did not greet as a daemon", socket.display()),
            ));
        }
        if ready["protocol"].as_u64() != Some(PROTOCOL) {
            // What the daemon announced, so a client that starts daemons can
            // tell an older one it may replace from a newer one it must not.
            let mut error = Error::with(
                "daemon_protocol_mismatch",
                &format!(
                    "the daemon speaks protocol {}, this client {PROTOCOL}",
                    ready["protocol"]
                ),
            );
            let facts = ["protocol", "pid"]
                .into_iter()
                .filter_map(|key| Some((key.to_owned(), ready.get(key)?.clone())));
            error.facts = Some(Box::new(facts.collect()));
            return Err(error);
        }
        // Named before its first request, unanswered: the reader drops the
        // reply, as no request waits under its id.
        if let Some(line) = hello() {
            write
                .write_all(&line)
                .await
                .map_err(|_| Error::new("daemon_disconnected"))?;
        }
        let store = ready["store"]["lineage"].as_str().map(str::to_owned);
        let identity = ready["store"]["identity"].as_str().map(str::to_owned);
        let providers = ready["providers"].clone();
        let speeds = config::ranked(
            (ready["speeds"].as_object().into_iter().flatten())
                .map(|(pattern, tiers)| (pattern.clone(), names(tiers)))
                .collect(),
        );
        let paused = ready["paused"].clone();
        let version = ready["version"].as_str().map(str::to_owned);
        let max_depth = ready["limits"]["depth"].as_u64();
        let pending: Pending = Arc::default();
        let closed = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let (events, receiver) = mpsc::channel(QUEUE);
        let budget = Arc::new(Semaphore::new(QUEUE_BYTES));
        let routed = pending.clone();
        let gone = closed.clone();
        tokio::spawn(async move {
            let mut lagged = false;
            while let Ok(Some(line)) = lines.next_line().await {
                let Ok(Envelope(id)) = serde_json::from_str::<Envelope>(&line) else {
                    continue;
                };
                match id {
                    Some(id) => {
                        let waiting = routed.lock().unwrap().remove(&id);
                        if let Some(sender) = waiting
                            && let Ok(message) = serde_json::from_str::<Value>(&line)
                        {
                            let _ = sender.send(message);
                        }
                    }
                    None => {
                        let Ok(size) = u32::try_from(line.len()) else {
                            lagged = true;
                            break;
                        };
                        let Ok(bytes) = budget.clone().try_acquire_many_owned(size) else {
                            lagged = true;
                            break;
                        };
                        match events.try_send(Notification {
                            encoded: line,
                            _bytes: Some(bytes),
                        }) {
                            Ok(()) => {}
                            Err(mpsc::error::TrySendError::Closed(_)) => break,
                            Err(mpsc::error::TrySendError::Full(_)) => {
                                lagged = true;
                                break;
                            }
                        }
                    }
                }
            }
            // The socket is gone, or we let go of it: every request still
            // waiting fails with `daemon_disconnected` now, and every later
            // one fails as it is made. The flag is set under the lock a
            // request registers under, so none slips between. A lag is
            // announced after the queue drains, then the receiver closes.
            {
                let mut waiting = routed.lock().unwrap();
                gone.store(true, std::sync::atomic::Ordering::SeqCst);
                waiting.clear();
            }
            drop(lines);
            if lagged {
                let _ = events
                    .send(Notification { encoded: json!({"event": "follow_lagged", "durable": false, "reason": "client_lagged"}).to_string(), _bytes: None })
                    .await;
            }
        });
        Ok((
            Arc::new(Self {
                writer: Mutex::new(write),
                pending,
                next: std::sync::atomic::AtomicU64::new(0),
                closed,
                store,
                identity,
                providers,
                speeds,
                paused,
                version,
                max_depth,
                in_flight: std::sync::OnceLock::new(),
            }),
            Events(receiver),
        ))
    }

    /// Let the daemon go: the write half shuts down, the daemon sees the end
    /// of the stream and closes its side, the reader ends, and whoever is
    /// waiting on the notifications sees them close.
    pub async fn close(&self) {
        let _ = self.writer.lock().await.shutdown().await;
    }

    /// Say which one of its program this process is, such as the agent a
    /// window shows, on this connection now and on each one it makes
    /// later, so the daemon's `stats` tells its connections from others
    /// of the program. The first label given stays. Unanswered, as the
    /// first `hello` is; nothing is said for a process with no program.
    pub async fn label(&self, label: &str) -> Result<()> {
        let _ = LABEL.set(label.to_owned());
        let Some(line) = hello() else { return Ok(()) };
        let mut writer = self.writer.lock().await;
        if self.closed.load(std::sync::atomic::Ordering::SeqCst) {
            return Err(Error::new("daemon_disconnected"));
        }
        let mut frame = FrameWrite {
            fd: writer.as_ref().as_raw_fd(),
            closed: &self.closed,
            complete: false,
        };
        writer.write_all(&line).await?;
        frame.complete = true;
        Ok(())
    }

    /// The lineage of the daemon's store, which a client keeps to know the
    /// same store again, also once its file was replaced.
    pub fn store(&self) -> Option<&str> {
        self.store.as_deref()
    }

    /// The daemon's release, as its `ready` line names it.
    pub fn version(&self) -> Option<&str> {
        self.version.as_deref()
    }

    /// The identity of the daemon's store file.
    pub fn identity(&self) -> Option<&str> {
        self.identity.as_deref()
    }

    /// The daemon's provider bindings as `ready` announced them.
    pub fn providers(&self) -> &Value {
        &self.providers
    }

    /// The tiers faster than standard the model at `path`,
    /// `PROVIDER/MODEL`, serves, as the daemon decides it: the person's
    /// most specific `[speeds]` pattern that matches, else the list its
    /// provider announced. A bot's speed reaches only these; on any other
    /// model its calls run at standard.
    pub fn speeds(&self, path: &str) -> Vec<String> {
        if let Some((_, tiers)) =
            (self.speeds.iter()).find(|(pattern, _)| config::matches(pattern, path))
        {
            return tiers.clone();
        }
        let (provider, model) = path.split_once('/').unwrap_or(("", path));
        names(&self.providers[provider]["speeds"][model])
    }

    /// The most generations below the bot that starts its line the daemon
    /// makes a bot at, as `ready` announced it; `None` for no bound.
    pub fn max_depth(&self) -> Option<u64> {
        self.max_depth
    }

    /// The pause that held the daemon as this client connected, or null;
    /// `paused` and `resumed` events tell of later ones.
    pub fn paused(&self) -> &Value {
        &self.paused
    }

    /// Keep at most `requests` in flight from now on, as an HTTP/2
    /// connection caps its streams, so a client with thousands of requests
    /// to make queues them here and not in the socket. A `wait` holds no
    /// work at the daemon and a lease renewal is control, like a PING, so
    /// neither waits for room: a renewal is written behind at most this
    /// many requests. The first bound set stays.
    pub fn bound(&self, requests: usize) {
        let _ = self.in_flight.set(Semaphore::new(requests));
    }

    pub async fn request(&self, op: &str, mut params: Value) -> Result<Value> {
        if !(params.is_object() || params.is_null()) {
            return Err(Error::with(
                "invalid_request",
                "expected an object of request parameters",
            ));
        }
        let _room = match self.in_flight.get() {
            Some(room) if !matches!(op, "wait" | "renew_approvals") => Some(
                room.acquire()
                    .await
                    .map_err(|_| Error::new("daemon_disconnected"))?,
            ),
            _ => None,
        };
        let id = self.next.fetch_add(1, std::sync::atomic::Ordering::Relaxed) + 1;
        params["id"] = json!(id);
        params["op"] = json!(op);
        let (sender, receiver) = oneshot::channel();
        {
            let mut pending = self.pending.lock().unwrap();
            if self.closed.load(std::sync::atomic::Ordering::SeqCst) {
                return Err(Error::new("daemon_disconnected"));
            }
            pending.insert(id, sender);
        }
        let _registration = PendingRequest {
            pending: &self.pending,
            id,
        };
        let mut line = serde_json::to_vec(&params)?;
        line.push(b'\n');
        {
            let mut writer = self.writer.lock().await;
            if self.closed.load(std::sync::atomic::Ordering::SeqCst) {
                return Err(Error::new("daemon_disconnected"));
            }
            let mut frame = FrameWrite {
                fd: writer.as_ref().as_raw_fd(),
                closed: &self.closed,
                complete: false,
            };
            writer.write_all(&line).await?;
            frame.complete = true;
        }
        let message = receiver
            .await
            .map_err(|_| Error::new("daemon_disconnected"))?;
        let Some(code) = message.get("error").and_then(Value::as_str) else {
            // Taken, not copied: a page of history runs to megabytes.
            return Ok(match message {
                Value::Object(mut reply) => reply.remove("result").unwrap_or(Value::Null),
                _ => Value::Null,
            });
        };
        let mut error = Error::new(code);
        error.detail = message["detail"].as_str().map(str::to_owned);
        if let Value::Object(mut facts) = message {
            for key in ["id", "error", "detail"] {
                facts.remove(key);
            }
            error.facts = (!facts.is_empty()).then(|| Box::new(facts));
        }
        Err(error)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_line_is_routed_by_its_id_as_a_parsed_object_would_be() {
        let routed = |line: &str| serde_json::from_str::<Envelope>(line).ok().map(|e| e.0);
        let parsed = |line: &str| {
            serde_json::from_str::<Value>(line)
                .ok()
                .map(|v| v.get("id").and_then(Value::as_u64))
        };
        for line in [
            r#"{"id":7,"result":{"id":9}}"#,
            r#"{"result":{"id":9},"id":7}"#,
            r#"{"event":"text_delta","bot":"b","data":{"id":3,"text":"x"}}"#,
            r#"{"id":"7","error":"x"}"#,
            r#"{"id":-1}"#,
            r#"{"id":1.5}"#,
            r#"{"id":null}"#,
            r#"{"id":[1],"x":{"y":[true,null]}}"#,
            r#"{"id":1,"id":2}"#,
            r#"{"\u0069d":4}"#,
            r#"[{"id":1}]"#,
            r#"7"#,
            r#""id""#,
            r#"{"id":1"#,
            r#"{"id":1} x"#,
        ] {
            assert_eq!(routed(line), parsed(line), "{line}");
        }
    }

    #[tokio::test]
    async fn a_notification_is_taken_as_the_value_it_was_sent_as() {
        let (sender, receiver) = mpsc::channel(4);
        let mut events = Events(receiver);
        let sent = json!({"event": "text_delta", "bot": "b", "data": {"text": "é\n"}});
        for encoded in ["{\"bad\":\"\\ud800\"}".to_owned(), sent.to_string()] {
            sender
                .send(Notification {
                    encoded,
                    _bytes: None,
                })
                .await
                .unwrap();
        }
        // A line no value holds is passed by, as the reader passes one by.
        assert_eq!(events.recv().await.unwrap(), sent);
    }

    #[test]
    fn a_service_leaves_the_session_it_was_started_from() {
        let mut command = std::process::Command::new("sleep");
        command.arg("5");
        own_session(&mut command);
        let mut child = command.spawn().unwrap();
        let pid = child.id() as libc::pid_t;
        // SAFETY: getsid only reads the session of a process this test owns.
        let (theirs, ours) = unsafe { (libc::getsid(pid), libc::getsid(0)) };
        let _ = child.kill();
        let _ = child.wait();
        assert_eq!(theirs, pid);
        assert_ne!(theirs, ours);
    }

    #[test]
    fn a_new_bot_edits_files_the_way_its_family_is_trained_to() {
        let providers = json!({"chatgpt": {"family": "responses"},
            "anthropic": {"family": "anthropic", "web_search": true}});
        let live = config::WebSearch::Live;
        assert_eq!(
            default_tools(&providers, "chatgpt/gpt-6-sol", live, &[]),
            [
                "shell",
                "write_stdin",
                "read",
                "write",
                "apply_patch",
                "wait",
                "history",
                "send_message",
                "list_agents",
                "pause_agents",
                "spawn_agent",
                "close_agent",
                "web_fetch"
            ]
        );
        // Search comes where the provider offers it, and fetch everywhere,
        // unless turned off.
        assert_eq!(
            default_tools(&providers, "anthropic/claude-opus-5-5", live, &[]),
            [
                "shell",
                "write_stdin",
                "read",
                "write",
                "edit",
                "wait",
                "history",
                "send_message",
                "list_agents",
                "pause_agents",
                "spawn_agent",
                "close_agent",
                "web_search",
                "web_fetch"
            ]
        );
        let off = default_tools(
            &providers,
            "anthropic/claude-opus-5-5",
            config::WebSearch::Disabled,
            &[],
        );
        assert!(!off.contains(&"web_search") && !off.contains(&"web_fetch"));
        // A model the person's web_search_off names keeps fetch alone.
        let named = default_tools(
            &providers,
            "anthropic/claude-opus-5-5",
            live,
            &["Anthropic/claude-*".into()],
        );
        assert!(!named.contains(&"web_search") && named.contains(&"web_fetch"));
        let other = ["anthropic/claude-haiku-*".into()];
        let unnamed = default_tools(&providers, "anthropic/claude-opus-5-5", live, &other);
        assert!(unnamed.contains(&"web_search"));
        assert_eq!(
            default_tools(&providers, "unknown/model", live, &[])[4],
            "edit"
        );
    }

    #[tokio::test]
    async fn a_model_s_speeds_are_the_person_s_then_its_provider_s() {
        let (stream, _peer) = UnixStream::pair().unwrap();
        let (_, write) = stream.into_split();
        let client = Client {
            writer: Mutex::new(write),
            pending: Arc::default(),
            next: std::sync::atomic::AtomicU64::new(0),
            closed: Arc::new(std::sync::atomic::AtomicBool::new(false)),
            store: None,
            identity: None,
            providers: json!({"work": {"speeds": {"us.openai.gpt-6.1-sol": ["ultrafast"]}}}),
            speeds: config::ranked(vec![
                ("work/*".into(), vec!["fast".into()]),
                ("work/*grok*".into(), vec!["ultrafast".into()]),
                ("work/us.openai.gpt-6.1-sol".into(), Vec::new()),
            ]),
            paused: Value::Null,
            version: None,
            max_depth: None,
            in_flight: std::sync::OnceLock::new(),
        };
        assert_eq!(client.speeds("work/us.xai.grok-4.7"), ["ultrafast"]);
        assert!(client.speeds("work/us.openai.gpt-6.1-sol").is_empty());
        assert_eq!(client.speeds("work/other"), ["fast"]);
        // Without a pattern, the provider's own list; else none.
        let client = Client {
            speeds: Vec::new(),
            ..client
        };
        assert_eq!(client.speeds("work/us.openai.gpt-6.1-sol"), ["ultrafast"]);
        assert!(client.speeds("work/us.xai.grok-4.7").is_empty());
        assert!(client.speeds("home/us.openai.gpt-6.1-sol").is_empty());
    }

    #[tokio::test]
    async fn invalid_parameters_and_canceled_waits_leave_the_session_usable() {
        let (stream, peer) = UnixStream::pair().unwrap();
        let (_, write) = stream.into_split();
        let client = Arc::new(Client {
            writer: Mutex::new(write),
            pending: Arc::default(),
            next: std::sync::atomic::AtomicU64::new(0),
            closed: Arc::new(std::sync::atomic::AtomicBool::new(false)),
            store: None,
            identity: None,
            providers: Value::Null,
            speeds: Vec::new(),
            paused: Value::Null,
            version: None,
            max_depth: None,
            in_flight: std::sync::OnceLock::new(),
        });
        let mut lines = BufReader::new(peer).lines();
        assert_eq!(
            client.request("wait", json!([])).await.unwrap_err().code,
            "invalid_request"
        );
        for _ in 0..20 {
            let request = tokio::spawn({
                let client = client.clone();
                async move { client.request("wait", json!({"handles":["Bob:1"]})).await }
            });
            lines.next_line().await.unwrap().unwrap();
            request.abort();
            let _ = request.await;
            assert!(client.pending.lock().unwrap().is_empty());
            assert!(!client.closed.load(std::sync::atomic::Ordering::SeqCst));
        }
    }

    #[tokio::test]
    async fn a_bounded_client_queues_requests_but_not_a_renewal_or_a_wait() {
        let (stream, peer) = UnixStream::pair().unwrap();
        let (_, write) = stream.into_split();
        let client = Arc::new(Client {
            writer: Mutex::new(write),
            pending: Arc::default(),
            next: std::sync::atomic::AtomicU64::new(0),
            closed: Arc::new(std::sync::atomic::AtomicBool::new(false)),
            store: None,
            identity: None,
            providers: Value::Null,
            speeds: Vec::new(),
            paused: Value::Null,
            version: None,
            max_depth: None,
            in_flight: std::sync::OnceLock::new(),
        });
        client.bound(1);
        let mut lines = BufReader::new(peer).lines();
        let ask = |op: &'static str| {
            let client = client.clone();
            tokio::spawn(async move { client.request(op, json!({"bot":"Bob"})).await })
        };
        let op = |line: String| serde_json::from_str::<Value>(&line).unwrap()["op"].clone();
        let first = ask("inspect");
        assert_eq!(op(lines.next_line().await.unwrap().unwrap()), "inspect");
        let second = ask("turns");
        let early = tokio::time::timeout(Duration::from_millis(100), lines.next_line()).await;
        assert!(early.is_err(), "a second request went out past the bound");
        // Control and long polls go out at once.
        let renewal = ask("renew_approvals");
        assert_eq!(
            op(lines.next_line().await.unwrap().unwrap()),
            "renew_approvals"
        );
        let wait = ask("wait");
        assert_eq!(op(lines.next_line().await.unwrap().unwrap()), "wait");
        // Room comes back as a request ends, answered or not.
        first.abort();
        assert_eq!(op(lines.next_line().await.unwrap().unwrap()), "turns");
        for task in [second, renewal, wait] {
            task.abort();
        }
    }

    #[tokio::test]
    async fn a_refusal_keeps_the_facts_sent_beside_its_detail() {
        let (stream, peer) = UnixStream::pair().unwrap();
        let (_, write) = stream.into_split();
        let client = Arc::new(Client {
            writer: Mutex::new(write),
            pending: Arc::default(),
            next: std::sync::atomic::AtomicU64::new(0),
            closed: Arc::new(std::sync::atomic::AtomicBool::new(false)),
            store: None,
            identity: None,
            providers: Value::Null,
            speeds: Vec::new(),
            paused: Value::Null,
            version: None,
            max_depth: None,
            in_flight: std::sync::OnceLock::new(),
        });
        let mut lines = BufReader::new(peer).lines();
        let request = tokio::spawn({
            let client = client.clone();
            async move { client.request("submit", json!({"bot":"Bob"})).await }
        });
        let sent: Value = serde_json::from_str(&lines.next_line().await.unwrap().unwrap()).unwrap();
        let id = sent["id"].as_u64().unwrap();
        let reply = client.pending.lock().unwrap().remove(&id).unwrap();
        reply
            .send(
                json!({"id":id,"error":"bot_busy","detail":"turn 3 is running",
                "running_turn":3,"fork_point":42}),
            )
            .unwrap();
        let error = request.await.unwrap().unwrap_err();
        assert_eq!(
            (error.code.as_str(), error.detail.as_deref()),
            ("bot_busy", Some("turn 3 is running"))
        );
        assert_eq!(
            error.facts.map(|facts| Value::Object(*facts)),
            Some(json!({"running_turn":3,"fork_point":42}))
        );
    }

    #[tokio::test]
    async fn canceling_a_partial_frame_disconnects_instead_of_corrupting_the_next_request() {
        use tokio::io::AsyncReadExt;
        let (stream, mut peer) = UnixStream::pair().unwrap();
        let (_, write) = stream.into_split();
        let client = Arc::new(Client {
            writer: Mutex::new(write),
            pending: Arc::default(),
            next: std::sync::atomic::AtomicU64::new(0),
            closed: Arc::new(std::sync::atomic::AtomicBool::new(false)),
            store: None,
            identity: None,
            providers: Value::Null,
            speeds: Vec::new(),
            paused: Value::Null,
            version: None,
            max_depth: None,
            in_flight: std::sync::OnceLock::new(),
        });
        let request = tokio::spawn({
            let client = client.clone();
            async move {
                client
                    .request("submit", json!({"prompt":"x".repeat(4*1024*1024)}))
                    .await
            }
        });
        peer.read_u8().await.unwrap(); // The request exceeds the socket buffer and is still writing.
        request.abort();
        let _ = request.await;
        let result =
            tokio::time::timeout(Duration::from_secs(1), client.request("bots", json!({}))).await;
        assert_eq!(
            result
                .expect("a partial frame must close the session")
                .unwrap_err()
                .code,
            "daemon_disconnected"
        );
        assert!(client.pending.lock().unwrap().is_empty());
    }
}
