//! What needs the person in the other projects: the footer's `← ? 1`,
//! and where `alt+n` goes once this project has no one. And every
//! question open, with the agents above the one that asked it, so the
//! window of each of them shows it, whatever window started it and
//! whichever were opened, closed or restarted since.
//!
//! One follow for every window of a store: the first window to open takes
//! a lock beside the store and follows; the others hear what it found on
//! a socket beside the store, one line each time it changes, and each
//! leaves out its own project. A window is woken only when what it draws
//! changes: what needs the person outside its project. When that window
//! closes, another takes the lock and follows on. So a hundred windows
//! cost the daemon and the machine what one does.
//!
//! It reads what `/status` reads, the way the sky and `/switch` do, on a
//! connection of its own: the calls held for the person ([`needs::listing`])
//! and each idle agent's question as of its last turn, taken from what any
//! window kept beside the store ([`crate::kept_seen`]) and read only where
//! none did. From then on the daemon's events keep it ([`needs::Needs`]),
//! and nothing is read again on a timer.
//!
//! Every agent is followed only for the events that can start a wait on
//! the person ([`WIDE`]): a call held for a verdict, a call starting (a
//! question is the ask tool's call), an agent closed (done, it needs no
//! one), a fork
//! made (it keeps its source's history, and any question open in it), and
//! an agent made (the event names who made it, so who is above whom is
//! known with nothing read). The
//! rest of an agent's events, its calls ending and its turn's end, are
//! followed only while it waits or runs a turn begun before the follow
//! ([`NARROW`]), from that event on, so nothing is missed
//! between. The daemon has no unfollow: once none of those does any more,
//! a new connection follows every agent on from the last event, with
//! nothing read again.
//!
//! The daemon replays each follow on a task of its own, so while one still
//! replays (every one at the start, and the wide one on a new connection),
//! an agent's call can end before the older approval that held it comes.
//! The events are applied in the order of their cursors ([`Lanes`]), so
//! what needs the person comes out as the daemon's history says.

use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet, VecDeque};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use agent_client::Client;
use serde_json::{Value, json};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::{UnixListener, UnixStream};
use tokio::sync::{Notify, broadcast, watch};

use crate::needs::{self, Seen};
use crate::standing;

/// The kinds of event every agent is followed for: those that can start
/// a wait on the person, a call lifted out of its turn and its end (a
/// step of it held for the person waits past the turn), a fork made (it
/// keeps any question open in its source's history), an agent made (who
/// made it), and an agent's closing or deletion.
pub const WIDE: [&str; 9] = [
    "approval_requested",
    "approval_asked",
    "tool_started",
    "call_lifted",
    "lifted_ended",
    "forked",
    "created",
    "closed",
    "deleted",
];

/// The kinds of event an agent that waits on the person is followed for as
/// well, which [`needs::Needs::event`] reads: its calls ending or decided,
/// which let a held call go or take a question's answer, the person's
/// words, and its turn's end.
pub const NARROW: [&str; 5] = [
    "tool_completed",
    "step_decided",
    "accepted",
    "steered",
    "turn_finished",
];

/// Every project's agents that need the person, followed while the window
/// is open: by this window, or heard from the one that follows. And, while
/// the window asks for them ([`Elsewhere::want_rows`]), every agent's row.
pub struct Elsewhere {
    /// What needs the person in every project.
    pub waiting: watch::Receiver<Shared>,
    /// The window's project, whose own it leaves out.
    pub project: String,
    /// Every agent's row, while the window asks for them and once the
    /// first are in.
    rows: Arc<std::sync::Mutex<Option<Rows>>>,
    want: watch::Sender<Wish>,
    task: tokio::task::JoinHandle<()>,
}

/// Every open agent of the store, by name, as the window that follows
/// keeps it from the daemon's events ([`standing::Follower`] of every
/// agent): one listing and one follow for every window, held only while a
/// window asks for them.
#[derive(Clone, Debug, Default)]
pub struct Rows {
    /// Moves on with each change, so a reader knows whether anything did;
    /// never back, and from a different start in each window that follows,
    /// so rows listed again after a pause or by another window never pass
    /// for ones already read.
    pub version: u64,
    /// Whether every row is in: a listing was told since the follow began.
    pub whole: bool,
    /// Shared with the change that told each and the reads counted from
    /// them: a row is copied once, as it comes.
    pub agents: BTreeMap<String, Arc<standing::Agent>>,
}

/// A change of [`Rows`], as told to the windows: every row (`all`), or
/// the rows set and the agents gone (closed or deleted) since.
#[derive(Clone, Debug, Default, serde::Serialize, serde::Deserialize)]
#[serde(default)]
struct Moved {
    version: u64,
    all: bool,
    set: Vec<Arc<standing::Agent>>,
    gone: Vec<String>,
}

impl Rows {
    fn take(&mut self, moved: &Moved) {
        if moved.all {
            self.agents.clear();
            self.whole = true;
        }
        for name in &moved.gone {
            self.agents.remove(name);
        }
        for agent in &moved.set {
            self.agents.insert(agent.name.clone(), agent.clone());
        }
        self.version = moved.version;
    }

    fn all(&self) -> Moved {
        Moved {
            version: self.version,
            all: true,
            set: self.agents.values().cloned().collect(),
            gone: Vec::new(),
        }
    }
}

/// One line the window that follows tells a window that connected.
#[derive(serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
enum Line {
    Waiting(Shared),
    Rows(Moved),
}

/// One line a window asks the one that follows: every agent's row, or no
/// more of them.
#[derive(serde::Serialize, serde::Deserialize)]
struct Asked {
    rows: bool,
}

/// What the window that follows tells the others: every project's agents
/// that need the person, in the order **Projects** lists their projects,
/// each with the agents above it, and the questions open.
#[derive(Clone, Debug, Default, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct Shared {
    /// Each project's place in **Projects**, by name.
    pub order: HashMap<String, usize>,
    /// Each agent with its project's place and its project.
    pub agents: Vec<(usize, String, String)>,
    /// Each agent of `agents`, with the agents above it ([`above`]).
    pub above: BTreeMap<String, Vec<String>>,
    /// Each agent of `agents` with a question open, and the question
    /// whole, as the window that answers it shows it.
    pub asks: BTreeMap<String, crate::asks::Ask>,
    /// The calls held for the person: each agent's, its turn and the call.
    pub calls: BTreeSet<(String, i64, String)>,
}

/// The agents of other projects that need the person, in the order
/// **Projects** lists their projects.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Waiting {
    /// The place of the window's project in that order.
    pub here: usize,
    /// Each agent, with its project's place.
    pub agents: Vec<(usize, String)>,
}

impl Shared {
    /// The place of `bot`'s project in **Projects**, if it is listed.
    pub fn place(&self, bot: &str) -> Option<usize> {
        self.order.get(project_in(bot, &self.order)).copied()
    }

    /// Whether `bot` needs the person, as this follow has it; `None` for
    /// one it does not follow (`closed`, a side chat, one of no project,
    /// or any before it told anything), which its own reads tell.
    pub fn waits(&self, bot: &str, closed: bool) -> Option<bool> {
        if self.order.is_empty() || closed || !other(bot, &HashSet::new()) {
            return None;
        }
        Some(self.agents.iter().any(|(_, _, waiting)| waiting == bot))
    }

    /// The questions open that show in the window of `window`: those of
    /// the agents it is above, or one of the leads it follows (`follows`)
    /// is, as a question goes up to every agent above the one that asked
    /// it. Its own are its own.
    pub fn below<'a>(
        &'a self,
        window: &'a str,
        follows: &'a [String],
    ) -> impl Iterator<Item = (&'a str, &'a crate::asks::Ask)> {
        (self.asks.iter())
            .filter(move |(bot, _)| self.is_below(bot, window, follows))
            .map(|(bot, ask)| (bot.as_str(), ask))
    }

    /// Whether `bot`, one of `agents`, is below `window`: `window` is
    /// above it, or it is a lead `window` follows, or one is above it.
    pub fn is_below(&self, bot: &str, window: &str, follows: &[String]) -> bool {
        let followed = |over: &str| follows.iter().any(|lead| lead == over);
        bot != window
            && (followed(bot)
                || (self.above.get(bot).into_iter().flatten())
                    .any(|over| over == window || followed(over)))
    }

    /// What needs the person outside `project`.
    pub fn outside(&self, project: &str) -> Waiting {
        Waiting {
            here: self.order.get(project).copied().unwrap_or(0),
            agents: (self.agents.iter())
                .filter(|(_, of, _)| of != project)
                .map(|(place, _, bot)| (*place, bot.clone()))
                .collect(),
        }
    }
}

impl Drop for Elsewhere {
    fn drop(&mut self) {
        self.task.abort();
    }
}

impl Elsewhere {
    /// Follows what needs the person in every project, or hears it from
    /// the window that does, waking the window (`woken`) only when what
    /// needs the person outside `project` changes, what its footer draws,
    /// or the questions open or calls held do, which it may show.
    pub fn start(socket: PathBuf, store: PathBuf, project: String, woken: Arc<Notify>) -> Self {
        let (tell, waiting) = watch::channel(Shared::default());
        let mut seen = waiting.clone();
        let outside = project.clone();
        let rows = Arc::new(std::sync::Mutex::new(None));
        let (want, wanted) = watch::channel(Wish::default());
        let here = Here {
            rows: rows.clone(),
            wanted,
            woken: woken.clone(),
        };
        let task = tokio::spawn(async move {
            let wake = async {
                let mut was = Waiting::default();
                let mut asked = (BTreeMap::new(), BTreeMap::new(), BTreeSet::new());
                while seen.changed().await.is_ok() {
                    let shared = seen.borrow_and_update();
                    let now = shared.outside(&outside);
                    let moved = shared.asks != asked.0
                        || shared.above != asked.1
                        || shared.calls != asked.2;
                    if moved {
                        asked = (
                            shared.asks.clone(),
                            shared.above.clone(),
                            shared.calls.clone(),
                        );
                    }
                    if now != was || moved {
                        was = now;
                        woken.notify_one();
                    }
                }
            };
            tokio::select! {
                () = share(&socket, &store, &tell, here) => {}
                () = wake => {}
            }
        });
        Elsewhere {
            waiting,
            project,
            rows,
            want,
            task,
        }
    }

    /// Ask for every agent's row, or no more of them (what was held is let
    /// go): while asked, the window is woken each time one moves.
    pub fn want_rows(&self, yes: bool) {
        self.want.send_if_modified(|was| {
            if was.yes == yes {
                return false;
            }
            // Each ask its own, so one that follows a let-go before the
            // hub saw it still gets every row again.
            was.ask += u64::from(yes);
            was.yes = yes;
            true
        });
        if !yes && let Ok(mut rows) = self.rows.lock() {
            *rows = None;
        }
    }

    /// Every agent's row, as last told: none until the first are in, or
    /// while not asked for.
    pub fn rows<T>(&self, read: impl FnOnce(Option<&Rows>) -> T) -> T {
        match self.rows.lock() {
            Ok(rows) => read(rows.as_ref()),
            Err(_) => read(None),
        }
    }

    /// Whether it stopped, as when the daemon went: to start again.
    pub fn ended(&self) -> bool {
        self.task.is_finished()
    }

    /// One that tells what `told` sends, for tests.
    #[cfg(test)]
    pub fn told(project: &str) -> (Self, watch::Sender<Shared>) {
        let (tell, waiting) = watch::channel(Shared::default());
        let open = Elsewhere {
            waiting,
            project: project.to_owned(),
            rows: Default::default(),
            want: watch::channel(Wish::default()).0,
            task: tokio::spawn(std::future::pending()),
        };
        (open, tell)
    }
}

/// The lock the window that follows holds, and the socket it tells the
/// others on: beside `store`, or, where that path is too long for a Unix
/// socket, the short one the client finds for a daemon's
/// ([`agent_client::socket::default_socket`]). Named for what is told on
/// it ([`Line`]): a Sonny that tells otherwise follows under another name,
/// so windows of two builds each hear their own.
fn beside(store: &Path) -> (PathBuf, PathBuf) {
    let store = agent_client::socket::canonical_store(store).unwrap_or_else(|_| store.into());
    let at = |end: &str| {
        let mut path = store.as_os_str().to_owned();
        path.push(end);
        PathBuf::from(path)
    };
    let lock = at(".asks.lock");
    let told =
        agent_client::socket::default_socket(&at(".asks")).unwrap_or_else(|_| at(".asks.sock"));
    (lock, told)
}

/// What a window keeps of every agent's row, and whether it asks for them.
/// Whether the window asks for every agent's row, and its ask's number.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
struct Wish {
    yes: bool,
    ask: u64,
}

struct Here {
    rows: Arc<std::sync::Mutex<Option<Rows>>>,
    wanted: watch::Receiver<Wish>,
    woken: Arc<Notify>,
}

impl Here {
    /// A change of every agent's row, kept where the window asks for them.
    fn take(&self, moved: &Moved) {
        if !self.wanted.borrow().yes {
            return;
        }
        if let Ok(mut rows) = self.rows.lock() {
            match (&mut *rows, moved.all) {
                (Some(rows), _) => rows.take(moved),
                (None, true) => {
                    *rows = Some(Rows::default()).map(|mut rows| {
                        rows.take(moved);
                        rows
                    })
                }
                // A change before every row: the first all is to come.
                (None, false) => return,
            }
        }
        self.woken.notify_one();
    }
}

/// Follow, where no window of the store does; else hear the one that does,
/// and follow once it is gone. Ends when the daemon goes.
async fn share(socket: &Path, store: &Path, tell: &watch::Sender<Shared>, here: Here) {
    let (lock, told) = beside(store);
    let mut missed = 0;
    loop {
        if let Ok(Some(held)) = crate::scheduler::locked(&lock) {
            lead(socket, store, &told, tell, &here).await;
            drop(held);
            return;
        }
        match UnixStream::connect(&told).await {
            Ok(stream) => {
                missed = 0;
                hear(stream, tell, &here).await;
            }
            // The window that took the lock is still making its socket.
            Err(_) => {
                missed = (missed + 1).min(7);
                tokio::time::sleep(Duration::from_millis(40 << missed)).await;
            }
        }
    }
}

/// The socket the window that follows tells the others on, gone with it.
struct Told(PathBuf);

impl Drop for Told {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.0);
    }
}

/// Every agent's row as the window that follows keeps it, for the windows
/// that ask: followed only while one does.
struct Book {
    rows: std::sync::Mutex<Rows>,
    moved: broadcast::Sender<Arc<Moved>>,
    /// How many windows ask for them.
    asking: watch::Sender<usize>,
}

impl Book {
    fn new() -> Self {
        // Where this window's versions start: the time, so another window
        // following after it starts elsewhere.
        let start = (std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH))
            .map_or(1, |since| since.as_micros() as u64);
        Book {
            rows: std::sync::Mutex::new(Rows {
                version: start,
                ..Default::default()
            }),
            moved: broadcast::channel(64).0,
            asking: watch::channel(0).0,
        }
    }

    /// A window asks for every agent's row: they are followed from now,
    /// and what is kept already comes first. Let go with the [`Asking`].
    fn ask(&self) -> (Asking<'_>, Option<Moved>, broadcast::Receiver<Arc<Moved>>) {
        self.asking.send_modify(|asking| *asking += 1);
        let moved = self.moved.subscribe();
        let kept = (self.rows.lock().ok())
            .filter(|rows| rows.whole)
            .map(|rows| rows.all());
        (Asking(self), kept, moved)
    }

    /// A change, kept and told.
    fn tell(&self, moved: Moved) {
        let Ok(mut rows) = self.rows.lock() else {
            return;
        };
        let moved = Moved {
            version: rows.version + 1,
            ..moved
        };
        rows.take(&moved);
        let _ = self.moved.send(Arc::new(moved));
    }
}

/// A task ended when dropped.
struct Ended(tokio::task::JoinHandle<()>);

impl Drop for Ended {
    fn drop(&mut self) {
        self.0.abort();
    }
}

/// A window asking for every agent's row, until dropped.
struct Asking<'a>(&'a Book);

impl Drop for Asking<'_> {
    fn drop(&mut self) {
        self.0.asking.send_modify(|asking| *asking -= 1);
    }
}

/// Follow every project, and tell each window that connects what needs
/// the person, at once and each time it changes; and every agent's row to
/// each window that asks, followed while any does.
async fn lead(socket: &Path, store: &Path, told: &Path, tell: &watch::Sender<Shared>, here: &Here) {
    let _ = std::fs::remove_file(told);
    let listener = UnixListener::bind(told).ok();
    let _told = listener.as_ref().map(|_| Told(told.to_owned()));
    let book = Arc::new(Book::new());
    let mut hearing = tokio::task::JoinSet::new();
    let following = follow(socket, store, tell);
    tokio::pin!(following);
    let rows = follow_rows(socket, &book);
    tokio::pin!(rows);
    let mine = rows_here(&book, here);
    tokio::pin!(mine);
    // What needs the person, written out once each time it changes and
    // that line sent to every window, not written out again for each.
    let mut shared = tell.subscribe();
    let (lines, _) = watch::channel(waiting_line(&shared.borrow_and_update()));
    let writing = async {
        while shared.changed().await.is_ok() {
            let line = waiting_line(&shared.borrow_and_update());
            lines.send_replace(line);
        }
    };
    tokio::pin!(writing);
    loop {
        tokio::select! {
            () = &mut following => return,
            () = &mut rows => return,
            () = &mut mine => return,
            () = &mut writing => return,
            Ok((stream, _)) = async { listener.as_ref().unwrap().accept().await }, if listener.is_some() => {
                hearing.spawn(tell_on(stream, lines.subscribe(), book.clone()));
            }
            Some(_) = hearing.join_next(), if !hearing.is_empty() => {}
        }
    }
}

/// Every agent's row, followed on a connection of its own while any window
/// asks for them ([`Book::asking`]), and let go once none does; ends when
/// the daemon goes.
async fn follow_rows(socket: &Path, book: &Book) {
    let mut asking = book.asking.subscribe();
    loop {
        if asking.wait_for(|asking| *asking > 0).await.is_err() {
            return;
        }
        let woken = Arc::new(Notify::new());
        let Some((client, follower, drain)) =
            standing::follow_apart(socket, crate::EVERY_PROJECT, &woken).await
        else {
            return;
        };
        // Ended however this does, dropped with the window's hub included.
        let drain = Ended(drain);
        let followed = async {
            let mut read_at: Option<std::time::Instant> = None;
            loop {
                woken.notified().await;
                // No oftener than `/status` reads its own: what moves
                // meanwhile is read together.
                if let Some(at) = read_at {
                    tokio::time::sleep(crate::STATUS_AGAIN.saturating_sub(at.elapsed())).await;
                }
                let changes = (follower.lock())
                    .map(|follower| follower.take())
                    .unwrap_or_default();
                if let Some(snapshot) = changes.snapshot {
                    book.tell(Moved {
                        all: true,
                        set: snapshot.into_iter().map(Arc::new).collect(),
                        ..Default::default()
                    });
                }
                // Moved since the listing, or since the last of these.
                if !changes.moved.is_empty() {
                    let names: Vec<String> = changes.moved.into_iter().collect();
                    read_at = Some(std::time::Instant::now());
                    let Ok(set) = standing::rows(&client, &names).await else {
                        return;
                    };
                    let gone = (names.into_iter())
                        .filter(|name| !set.iter().any(|row| row.name == *name))
                        .collect();
                    book.tell(Moved {
                        set: set.into_iter().map(Arc::new).collect(),
                        gone,
                        ..Default::default()
                    });
                }
                if drain.0.is_finished() {
                    return;
                }
            }
        };
        tokio::select! {
            () = followed => return,
            _ = asking.wait_for(|asking| *asking == 0) => {}
        }
        // None asks: the follow is let go, and what it kept.
        drop(drain);
        if let Ok(mut rows) = book.rows.lock() {
            rows.agents.clear();
            rows.whole = false;
        }
    }
}

/// The window that follows asking for every agent's row itself.
async fn rows_here(book: &Book, here: &Here) {
    let mut wanted = here.wanted.clone();
    loop {
        let Ok(ask) = wanted.wait_for(|wish| wish.yes).await.map(|wish| wish.ask) else {
            return std::future::pending().await;
        };
        let (asking, kept, mut moved) = book.ask();
        if let Some(kept) = &kept {
            here.take(kept);
        }
        let mut at = kept.map_or(0, |kept| kept.version);
        loop {
            tokio::select! {
                got = moved.recv() => match got {
                    Ok(moved) if moved.version > at => {
                        at = moved.version;
                        here.take(&moved);
                    }
                    Ok(_) => {}
                    // Too far behind: every row again.
                    Err(broadcast::error::RecvError::Lagged(_)) => {
                        let all = book.rows.lock().ok().filter(|rows| rows.whole).map(|rows| rows.all());
                        if let Some(all) = all {
                            at = all.version;
                            here.take(&all);
                        }
                    }
                    Err(broadcast::error::RecvError::Closed) => return,
                },
                _ = wanted.wait_for(|wish| !wish.yes || wish.ask != ask) => break,
            }
        }
        drop(asking);
    }
}

/// A line to a window that connected.
async fn write_line(write: &mut tokio::net::unix::OwnedWriteHalf, line: &Line) -> bool {
    let line = serde_json::to_string(line).unwrap_or_default();
    write
        .write_all(format!("{line}\n").as_bytes())
        .await
        .is_ok()
}

/// What needs the person as the line a window that connected is told.
fn waiting_line(shared: &Shared) -> Arc<str> {
    let line = serde_json::to_string(&Line::Waiting(shared.clone())).unwrap_or_default();
    format!("{line}\n").into()
}

/// What needs the person, to a window that connected: now, and again each
/// time it changes, one line each ([`waiting_line`]); and every agent's
/// row while it asks.
async fn tell_on(stream: UnixStream, mut shared: watch::Receiver<Arc<str>>, book: Arc<Book>) {
    let (read, mut write) = stream.into_split();
    let mut asks = BufReader::new(read).lines();
    let mut rows: Option<(Asking<'_>, broadcast::Receiver<Arc<Moved>>, u64)> = None;
    let mut told = false;
    loop {
        if !told {
            told = true;
            let line = shared.borrow_and_update().clone();
            if write.write_all(line.as_bytes()).await.is_err() {
                return;
            }
        }
        tokio::select! {
            changed = shared.changed() => match changed {
                Ok(()) => told = false,
                Err(_) => return,
            },
            asked = asks.next_line() => {
                // The window closed.
                let Ok(Some(asked)) = asked else { return };
                let Ok(Asked { rows: yes }) = serde_json::from_str(&asked) else { continue };
                match (yes, rows.is_some()) {
                    (true, false) => {
                        let (asking, kept, moved) = book.ask();
                        let at = kept.as_ref().map_or(0, |kept| kept.version);
                        if let Some(kept) = kept
                            && !write_line(&mut write, &Line::Rows(kept)).await
                        {
                            return;
                        }
                        rows = Some((asking, moved, at));
                    }
                    (false, true) => rows = None,
                    _ => {}
                }
            }
            got = async { rows.as_mut().unwrap().1.recv().await }, if rows.is_some() => {
                let Some((_, _, at)) = &mut rows else { continue };
                let line = match got {
                    Ok(moved) if moved.version > *at => (*moved).clone(),
                    Ok(_) => continue,
                    Err(broadcast::error::RecvError::Lagged(_)) => {
                        let all = book.rows.lock().ok().filter(|rows| rows.whole).map(|rows| rows.all());
                        match all {
                            Some(all) => all,
                            None => continue,
                        }
                    }
                    Err(broadcast::error::RecvError::Closed) => return,
                };
                *at = line.version;
                if !write_line(&mut write, &Line::Rows(line)).await {
                    return;
                }
            }
        }
    }
}

/// What the window that follows tells, until it goes; and the window's
/// asks for every agent's row, told to it.
async fn hear(stream: UnixStream, tell: &watch::Sender<Shared>, here: &Here) {
    let (read, mut write) = stream.into_split();
    let mut lines = BufReader::new(read).lines();
    let mut wanted = here.wanted.clone();
    wanted.mark_changed();
    let mut asked = Wish::default();
    loop {
        tokio::select! {
            line = lines.next_line() => {
                let Ok(Some(line)) = line else { return };
                match serde_json::from_str::<Line>(&line) {
                    Ok(Line::Waiting(mut shared)) => {
                        tell.send_if_modified(|was| {
                            let moved = *was != shared;
                            *was = std::mem::take(&mut shared);
                            moved
                        });
                    }
                    Ok(Line::Rows(moved)) => here.take(&moved),
                    Err(_) => {}
                }
            }
            changed = wanted.changed() => {
                if changed.is_err() {
                    return;
                }
                let wish = *wanted.borrow_and_update();
                // Let go and asked again before this was seen: let go here
                // too, so every row comes again.
                let again = (asked.yes && wish.yes && asked.ask != wish.ask).then_some(false);
                for rows in again.into_iter().chain([wish.yes]) {
                    let line = serde_json::to_string(&Asked { rows }).unwrap_or_default();
                    if write.write_all(format!("{line}\n").as_bytes()).await.is_err() {
                        return;
                    }
                }
                asked = wish;
            }
        }
    }
}

/// Whether `bot` is an agent of a project: not one of Sonny's own (a
/// leading dot), nor a side chat, which `/status` leaves out too. A side
/// chat is named as `/side` names one, but for the tasks in
/// `named_as_sides`, listed with such a name though no side chat's record.
pub fn other(bot: &str, named_as_sides: &HashSet<String>) -> bool {
    !bot.starts_with('.')
        && bot.contains('.')
        && (crate::side_source(bot).is_none() || named_as_sides.contains(bot))
}

/// The project `bot` is an agent of: the longest start of its name that
/// names one of `projects`, as a window finds its own, so `shop.web.page`
/// is `shop.web`'s where both projects are; else up to its first dot.
pub fn project_in<'a>(bot: &'a str, projects: &HashMap<String, usize>) -> &'a str {
    (bot.match_indices('.').map(|(at, _)| &bot[..at]).rev())
        .find(|start| projects.contains_key(*start))
        .unwrap_or_else(|| bot.split_once('.').map_or(bot, |(of, _)| of))
}

/// Follow every project, telling what needs the person in `tell`.
async fn follow(socket: &Path, store: &Path, tell: &watch::Sender<Shared>) {
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |since| since.as_millis() as u64);
    // From now: where things stand comes from the reads below, and an
    // event that lands while they run is applied after them.
    // Where the follow of every agent has reached: its kinds are none of
    // [`NARROW`]'s, so each event of a [`WIDE`] kind is its, in order, while
    // an agent's own follow may run ahead of it.
    let mut wide_from = json!({"since_ms": now});
    let Some((mut client, mut events, mut lanes)) = follow_wide(socket, wide_from.clone()).await
    else {
        return;
    };
    let Some(seeded) = seed(&client, store).await else {
        return;
    };
    let Seeded {
        mut needs,
        mut order,
        named_as_sides,
        mut seen_of,
        running,
        mut closed,
        mut made_by,
    } = seeded;
    let theirs = |bot: &str| other(bot, &named_as_sides);
    // The agents followed narrowly, and whether each has waited since:
    // those that wait now, and those whose turn runs, to its end.
    let mut narrow: HashMap<String, bool> = HashMap::new();
    let waiting: Vec<String> = needs.waiting().map(str::to_owned).collect();
    for (bot, waited) in (waiting.into_iter().map(|bot| (bot, true)))
        .chain(running.into_iter().map(|bot| (bot, false)))
    {
        if narrow.contains_key(&bot) {
            continue;
        }
        let asked = json!({"bot": bot, "since_ms": now, "kinds": NARROW});
        if !lanes.follow(&client, asked).await {
            return;
        }
        narrow.insert(bot, waited);
    }
    // The agents closed (those holding a call when the follow started,
    // then those it hears close): a turn still running in one can ask,
    // and needs no one until a message opens it again.
    tell_waiting(&needs, &order, &made_by, tell);
    loop {
        // The next event in the order of the cursors, once every follow
        // that still replays has passed it.
        let Some(event) = lanes.next() else {
            let Some(event) = events.recv().await else {
                return;
            };
            lanes.heard(event);
            continue;
        };
        let cursor = event["cursor"].clone();
        if (event["event"].as_str()).is_some_and(|kind| WIDE.contains(&kind)) {
            wide_from = json!({"after": cursor});
        }
        let Some(bot) = event["bot"].as_str() else {
            continue;
        };
        let bot = bot.to_owned();
        let mut ours = theirs(&bot);
        match event["event"].as_str() {
            // Who made it, as it was made: a name made again is another's.
            Some("created") => {
                let by = event["data"]["created_by"].as_str().unwrap_or_default();
                made_by.made(&bot, by, &order);
                continue;
            }
            Some("closed") => _ = closed.insert(bot.clone()),
            // Gone for good: what it made is its first lead's ([`above`]).
            Some("deleted") => {
                closed.remove(&bot);
                made_by.gone(&bot);
            }
            // A wait from an agent closed: only if a message opened it
            // again since, as `/status` reads it done until then.
            _ if ours && closed.contains(&bot) && waits_from(&event) => {
                match standing::rows(&client, std::slice::from_ref(&bot)).await {
                    Ok(rows) if rows.iter().any(|row| row.name == bot && !row.closed) => {
                        _ = closed.remove(&bot)
                    }
                    _ => ours = false,
                }
            }
            _ => {}
        }
        if !ours {
            continue;
        }
        // A lead the start did not list: its project is placed after the
        // others, so its agents are told as its own.
        if ours
            && crate::project::is_lead(&bot)
            && let Some(project) = crate::project::lead_project(&bot)
            && !order.contains_key(project)
        {
            order.insert(project.to_owned(), order.len());
        }
        // A fork made: any question open in the history it keeps.
        let mut read = false;
        let mut forked = false;
        if ours && event["event"] == "forked" {
            // Who made it, as a made agent's: a name made again is another's.
            let by = event["data"]["created_by"].as_str().unwrap_or_default();
            made_by.made(&bot, by, &order);
            let stirred = needs.stirred(&bot);
            if let Some(seen) = needs::scan(&client, &bot, None).await
                && needs.stirred(&bot) == stirred
            {
                let open = seen.clone().open(closed.contains(&bot), false);
                forked = open.is_some();
                if forked {
                    seen_of.insert(bot.clone(), seen);
                }
                needs.asked(&bot, open);
                read = forked;
            }
        }
        // A call held for the person, or a question asked or kept: its
        // calls and its turn's end from here on, to see it let go,
        // answered or over.
        if !narrow.contains_key(&bot) && (waits_from(&event) || forked) {
            let asked = json!({"bot": bot, "after": cursor, "kinds": NARROW});
            if !lanes.follow(&client, asked).await {
                return;
            }
            narrow.insert(bot.clone(), false);
        }
        // A turn ending that would let go a step of a call lifted before
        // the follow started: what the daemon holds of it, read again.
        let again = ours
            && event["event"] == "turn_finished"
            && (event["turn"].as_i64()).is_some_and(|turn| needs.ends_steps_of_unseen(&bot, turn));
        // Whether it waits changed, or which of its calls are held, or its
        // question: each is told.
        let (held, asked) = (needs.held_by(&bot), needs.version());
        let mut changed = ours && needs.event(&event);
        if again && let Ok(listed) = needs::listing(&client, Some(&bot)).await {
            let was = needs.waits(&bot);
            needs.held(listed, |of| of == bot);
            changed |= was != needs.waits(&bot);
        }
        changed |= ours && (held != needs.held_by(&bot) || asked != needs.version());
        // Closed, it needs no one, as `/status` lists it no more.
        if ours && event["event"] == "closed" {
            changed |= needs.waits(&bot);
            needs.forget(&bot);
            seen_of.remove(&bot);
        }
        // A turn that may have asked or answered, read once it ends.
        for (due, turn) in needs.due() {
            let stirred = needs.stirred(&due);
            if let Some(seen) = needs::scan(&client, &due, turn).await
                && needs.stirred(&due) == stirred
            {
                let open = seen.clone().open(closed.contains(&due), false);
                match open.is_some() {
                    true => seen_of.insert(due.clone(), seen),
                    false => seen_of.remove(&due),
                };
                needs.asked(&due, open);
                read = true;
            }
        }
        if changed || read {
            tell_waiting(&needs, &order, &made_by, tell);
        }
        if let Some(waited) = narrow.get_mut(&bot) {
            *waited |= needs.waits(&bot) || event["event"] == "turn_finished";
        }
        // None followed narrowly waits, runs any more or is to be
        // read at its turn's end: their events are no longer wanted, and
        // the daemon has no unfollow. A new connection follows every agent
        // on from where its follow had reached, with nothing read again,
        // once every event heard is applied.
        if lanes.is_empty() && done_with(&narrow, &needs) {
            drop((client, events));
            let Some(again) = follow_wide(socket, wide_from.clone()).await else {
                return;
            };
            (client, events, lanes) = again;
            narrow.clear();
        }
    }
}

/// Whether none of the agents followed narrowly is wanted any more: each
/// has waited or run to its turn's end, and none waits now, is to be read
/// when its turn ends (the person's words went past its question).
fn done_with(narrow: &HashMap<String, bool>, needs: &needs::Needs) -> bool {
    !narrow.is_empty()
        && (narrow.iter()).all(|(bot, waited)| *waited && !needs.waits(bot) && !needs.unsure(bot))
}

/// A connection of its own following every agent for [`WIDE`], from
/// `from` (`since_ms` or `after`).
async fn follow_wide(
    socket: &std::path::Path,
    from: Value,
) -> Option<(Arc<Client>, agent_client::Events, Lanes)> {
    let (client, events) = Client::connect(socket).await.ok()?;
    let mut asked = json!({"bot": "*.*", "kinds": WIDE});
    if let (Some(asked), Some(from)) = (asked.as_object_mut(), from.as_object()) {
        asked.extend(from.clone());
    }
    let mut lanes = Lanes::default();
    lanes
        .follow(&client, asked)
        .await
        .then_some((client, events, lanes))
}

/// The events of every follow on one connection, given back in the order
/// of their cursors. The daemon replays each follow on a task of its own
/// until it sends `follow_live`, so while any replays, one follow's events
/// can come ahead of another's older ones: an agent's call ending and its
/// turn's end, on its own follow, before the approval that held the call,
/// on the follow of every agent. Each follow replays in order, so an event
/// is given once every follow still replaying has passed its cursor. Once
/// every follow is live, the daemon publishes in order and each event is
/// given as it comes.
#[derive(Default)]
struct Lanes {
    /// Each follow still replaying, by what it follows (an agent's name or
    /// `*.*`), with the newest cursor it gave.
    replaying: HashMap<String, i64>,
    /// The events heard and not yet given, by cursor.
    held: BTreeMap<i64, Vec<Value>>,
    /// Those given as they came: with no cursor, or heard once every
    /// follow was live with nothing held.
    loose: VecDeque<Value>,
}

impl Lanes {
    /// Follow as `asked`, replaying from the cursor the daemon answers.
    async fn follow(&mut self, client: &Client, asked: Value) -> bool {
        let Some(of) = asked["bot"].as_str().map(str::to_owned) else {
            return false;
        };
        let from = asked["after"].as_i64().unwrap_or(0);
        let Ok(answer) = client.request("follow", asked).await else {
            return false;
        };
        self.replaying
            .insert(of, answer["after"].as_i64().unwrap_or(from));
        true
    }

    /// An event as it came: kept to be given in order. A follow's
    /// `follow_live` says it replays no more.
    fn heard(&mut self, event: Value) {
        let of = (event["following"].as_str()).or(event["bot"].as_str());
        if event["event"] == "follow_live" {
            if let Some(of) = of {
                self.replaying.remove(of);
            }
            return;
        }
        // Every follow live and nothing held: as it comes, in the order
        // the daemon published it.
        let cursor = event["cursor"].as_i64();
        let Some(cursor) = cursor.filter(|_| !self.replaying.is_empty() || !self.held.is_empty())
        else {
            self.loose.push_back(event);
            return;
        };
        if let Some(reached) = of.and_then(|of| self.replaying.get_mut(of)) {
            *reached = (*reached).max(cursor);
        }
        self.held.entry(cursor).or_default().push(event);
    }

    /// The next event to apply: the oldest held, once no follow still
    /// replaying can give an older one.
    fn next(&mut self) -> Option<Value> {
        if let Some(event) = self.loose.pop_front() {
            return Some(event);
        }
        let passed = self.replaying.values().copied().min().unwrap_or(i64::MAX);
        let mut oldest = self.held.first_entry()?;
        if *oldest.key() > passed {
            return None;
        }
        let event = oldest.get_mut().remove(0);
        if oldest.get().is_empty() {
            oldest.remove();
        }
        Some(event)
    }

    /// Whether every event heard was given.
    fn is_empty(&self) -> bool {
        self.held.is_empty() && self.loose.is_empty()
    }
}

/// Where things stood when the follow began.
struct Seeded {
    needs: needs::Needs,
    /// Each project's place in **Projects**, by name.
    order: HashMap<String, usize>,
    /// The tasks listed with a side chat's name that are not side chats.
    named_as_sides: HashSet<String>,
    /// What the history of each agent with a question open said, kept to
    /// read again when a lead marks it done.
    seen_of: HashMap<String, Seen>,
    /// The agents whose turn ran, read once it ends.
    running: Vec<String>,
    /// The agents closed that hold a call for the person.
    closed: HashSet<String>,
    /// Who made each agent listed.
    made_by: Makers,
}

/// The calls held for the person, and each idle agent's question as of
/// its last turn, as `/status` reads it: what a window kept, else read
/// now. The listing of every agent goes once this is read.
async fn seed(client: &Client, store: &Path) -> Option<Seeded> {
    let mut needs = needs::Needs::default();
    let held = needs::listing(client, None).await.ok()?;
    let listed = standing::list(client, "*.*").await.ok()?;
    let order = in_order(store, &listed);
    let named_as_sides: HashSet<String> = (listed.iter())
        .filter(|bot| {
            crate::side_source(&bot.name)
                .is_some_and(|source| bot.maker() != source || bot.expire_ms.is_none())
        })
        .map(|bot| bot.name.clone())
        .collect();
    let theirs = |bot: &str| other(bot, &named_as_sides);
    // A call held by an agent closed needs no one: it is done. The listing
    // is in the order of the names.
    let open = |bot: &str| {
        (listed.binary_search_by(|row| row.name.as_str().cmp(bot)))
            .is_ok_and(|at| !listed[at].closed)
    };
    let mut closed: HashSet<String> = (held.iter())
        .filter(|held| theirs(&held.bot) && !open(&held.bot))
        .map(|held| held.bot.clone())
        .collect();
    // Those closed while their turn runs, which can still ask.
    closed.extend(closed_running(client).await?);
    needs.held(held, |bot| theirs(bot) && !closed.contains(bot));
    let mut seen_of: HashMap<String, Seen> = HashMap::new();
    let mut running = Vec::new();
    let identity = client.identity().map(str::to_owned);
    let kept = crate::kept_seen(store, identity.as_deref());
    let mut read: HashMap<String, (i64, Seen)> = HashMap::new();
    for bot in &listed {
        if !theirs(&bot.name) || bot.head.is_none() || bot.closed {
            continue;
        }
        // Its turn runs: what it asked, read once the turn ends.
        if bot.running_turn.is_some() {
            needs.read_at_end(&bot.name);
            running.push(bot.name.clone());
            continue;
        }
        // A fork that has run no turn of its own yet keeps its source's
        // history, and any question open in it: read it whole.
        let Some(turn) = bot.last.as_ref().map(|last| last.turn) else {
            let Some(seen) = needs::scan(client, &bot.name, None).await else {
                continue;
            };
            let open = seen.clone().open(false, false);
            if open.is_some() {
                seen_of.insert(bot.name.clone(), seen);
            }
            needs.asked(&bot.name, open);
            continue;
        };
        let was = kept.get(&bot.name);
        let seen = match was.filter(|(at, _)| *at == turn) {
            Some((_, seen)) => seen.clone(),
            None => {
                let seen = match was {
                    Some((_, was)) => needs::scan_since(client, &bot.name, Some(turn), was).await,
                    None => needs::scan(client, &bot.name, Some(turn)).await,
                };
                let Some(seen) = seen else {
                    continue;
                };
                read.insert(bot.name.clone(), (turn, seen.clone()));
                seen
            }
        };
        let open = seen.clone().open(false, false);
        if open.is_some() {
            seen_of.insert(bot.name.clone(), seen);
        }
        needs.asked(&bot.name, open);
    }
    if !read.is_empty() {
        crate::keep_seen(store, ".elsewhere", identity.as_deref(), &read);
    }
    let mut made_by = Makers::default();
    for bot in &listed {
        made_by.made(&bot.name, bot.maker(), &order);
    }
    Some(Seeded {
        needs,
        order,
        named_as_sides,
        seen_of,
        running,
        closed,
        made_by,
    })
}

/// The agents closed whose turn runs: every running agent's row, closed
/// ones included, as few as the agents at work.
async fn closed_running(client: &Client) -> Option<Vec<String>> {
    let mut closed = Vec::new();
    let mut after: Option<String> = None;
    loop {
        let mut ask = json!({"name": "*.*", "active": true, "limit": 256});
        if let Some(after) = &after {
            ask["after"] = json!(after);
        }
        let page = client.request("bots", ask).await.ok()?;
        closed.extend(
            (page["bots"].as_array().into_iter().flatten())
                .filter(|row| !row["closed_ms"].is_null())
                .filter_map(|row| row["name"].as_str().map(str::to_owned)),
        );
        match page["next_after"].as_str() {
            Some(next) if after.as_deref() != Some(next) => after = Some(next.to_owned()),
            _ => return Some(closed),
        }
    }
}

/// Whether `event` holds a call for the person, or asks a question (the
/// ask tool's call starting): after it, the agent's calls ending say when
/// it no longer waits.
fn waits_from(event: &Value) -> bool {
    match event["event"].as_str() {
        Some("approval_asked") => true,
        Some("approval_requested") => {
            (event["data"]["calls"].as_array().into_iter().flatten()).any(needs::person_call)
        }
        Some("tool_started") => event["data"]["name"] == crate::asks::TOOL,
        _ => false,
    }
}

/// Each project's place in the order **Projects** lists them, by its
/// name, from its lead's listing and title.
fn in_order(store: &std::path::Path, listed: &[standing::Agent]) -> HashMap<String, usize> {
    let mut titles = crate::title::Titles::read(store);
    let projects = (listed.iter())
        .filter(|bot| {
            crate::project::is_lead(&bot.name)
                && bot.maker().is_empty()
                && bot.expire_ms.is_none()
                && !crate::project::is_internal(&bot.name)
        })
        .map(|lead| crate::projects::Project {
            lead: lead.name.clone(),
            name: crate::project::lead_project(&lead.name)
                .unwrap_or("")
                .to_owned(),
            title: (titles.named(&lead.name, lead.bot_id, lead.workspace.as_deref()))
                .unwrap_or_default(),
            folder: lead.workspace.clone().unwrap_or_default(),
            ..Default::default()
        })
        .collect();
    let mut order = HashMap::new();
    for lead in crate::projects::order(projects, 0) {
        if let Some(name) = crate::project::lead_project(&lead) {
            let next = order.len();
            order.entry(name.to_owned()).or_insert(next);
        }
    }
    order
}

/// Every project's agents waiting on the person, in the order
/// **Projects** lists their projects (one made since, after), each with
/// the agents above it, and the questions open, told to the windows when
/// they changed.
fn tell_waiting(
    needs: &needs::Needs,
    order: &HashMap<String, usize>,
    made_by: &Makers,
    tell: &watch::Sender<Shared>,
) {
    let mut agents: Vec<(usize, String, String)> = (needs.waiting())
        .map(|bot| {
            let project = project_in(bot, order);
            let place = order.get(project).copied().unwrap_or(usize::MAX);
            (place, project.to_owned(), bot.to_owned())
        })
        .collect();
    agents.sort();
    let above = (agents.iter())
        .map(|(_, _, bot)| (bot.clone(), above(bot, made_by, order)))
        .collect();
    let asks = (needs.asks())
        .map(|(bot, ask)| (bot.to_owned(), ask.clone()))
        .collect();
    let calls = (needs.calls())
        .map(|(bot, turn, call)| (bot.to_owned(), turn, call.to_owned()))
        .collect();
    let shared = Shared {
        order: order.clone(),
        agents,
        above,
        asks,
        calls,
    };
    if *tell.borrow() == shared {
        return;
    }
    tell.send_replace(shared);
}

/// Who made each agent an agent made, kept small for a store of
/// thousands: each agent by a hash of its name, and each maker's name
/// once, shared by all it made. One no agent made is not kept, nor one its
/// project's first lead made, which [`above`] finds without it.
#[derive(Default)]
struct Makers {
    by: HashMap<u64, Arc<str>>,
    names: HashSet<Arc<str>>,
}

impl Makers {
    fn key(bot: &str) -> u64 {
        use std::hash::{Hash, Hasher};
        let mut hasher = std::hash::DefaultHasher::new();
        bot.hash(&mut hasher);
        hasher.finish()
    }

    /// `bot` was made by `by`, or by no agent where it is empty.
    fn made(&mut self, bot: &str, by: &str, projects: &HashMap<String, usize>) {
        let first_lead = !crate::project::is_lead(bot)
            && by.strip_suffix(".lead") == Some(project_in(bot, projects));
        if by.is_empty() || first_lead {
            self.gone(bot);
            return;
        }
        let by = match self.names.get(by) {
            Some(name) => name.clone(),
            None => {
                let name: Arc<str> = by.into();
                self.names.insert(name.clone());
                name
            }
        };
        self.by.insert(Self::key(bot), by);
    }

    fn gone(&mut self, bot: &str) {
        self.by.remove(&Self::key(bot));
    }

    /// The agent that made `bot`, when one did.
    fn of(&self, bot: &str) -> Option<&str> {
        self.by.get(&Self::key(bot)).map(|by| &**by)
    }
}

/// How many agents above one are walked at most: a chain of makers is
/// short, and this only stops a ring.
const MOST_ABOVE: usize = 64;

/// The agents above `bot`, nearest first: the agent that made it, the one
/// that made that, and so on, through leads, to a lead no agent made. One
/// no agent made that is no lead is its project's first lead's, as
/// `/status` counts it (and so is one whose maker is gone): that lead is
/// above it, and whatever is above that lead.
fn above(bot: &str, made_by: &Makers, projects: &HashMap<String, usize>) -> Vec<String> {
    let mut chain: Vec<String> = Vec::new();
    let mut at = bot.to_owned();
    while chain.len() < MOST_ABOVE {
        let next = match made_by.of(&at) {
            Some(by) => by.to_owned(),
            None if !crate::project::is_lead(&at) => format!("{}.lead", project_in(&at, projects)),
            None => break,
        };
        if next == bot || chain.contains(&next) {
            break;
        }
        chain.push(next.clone());
        at = next;
    }
    chain
}

/// Every agent that needs the person, in the order `alt+n` goes round
/// them from the window's project: `local`, those its window counts, and
/// `elsewhere`'s, each by its project's place in **Projects**, from the
/// window's own around to the one before it; in each project its lead
/// first, then by name. The same round from every agent of every project, so
/// `alt+n` reaches each.
pub fn round<'a>(local: &'a [(usize, String)], elsewhere: &'a Waiting) -> Vec<&'a str> {
    let here = elsewhere.here;
    let mut all: Vec<(bool, usize, bool, &str)> = (local.iter())
        .map(|(place, bot)| (*place, bot.as_str()))
        .chain((elsewhere.agents.iter()).map(|(place, bot)| (*place, bot.as_str())))
        .map(|(place, bot)| (place < here, place, !crate::project::is_lead(bot), bot))
        .collect();
    all.sort();
    let mut seen = HashSet::new();
    (all.into_iter())
        .map(|(_, _, _, bot)| bot)
        .filter(|bot| seen.insert(*bot))
        .collect()
}

/// Where `alt+n` goes from `here`: the next of `waiting` ([`round`]),
/// around the end, and the first from one not in it.
pub fn next<'a>(waiting: &[&'a str], here: &str) -> Option<&'a str> {
    let first = waiting.first()?;
    Some(match waiting.iter().position(|bot| *bot == here) {
        Some(at) => waiting[(at + 1) % waiting.len()],
        None => first,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn other_projects_are_every_project_but_this_one_and_not_sonnys_own() {
        let projects: HashMap<String, usize> =
            [("tally", 0), ("site", 1), ("shop", 2), ("shop.web", 3)]
                .map(|(name, at)| (name.to_owned(), at))
                .into();
        let sides: HashSet<String> = ["site.side-feed".to_owned()].into();
        // Every window's: what its own project holds is left out below.
        let other = |bot| other(bot, &sides);
        assert!(other("site.lead"));
        assert!(other("books.fix"));
        assert!(!other(".sky.lead"));
        // A side chat is left out, as `/status` leaves it.
        assert!(!other("site.lead.side-1a2b"));
        // A task named like one, which its record says it is not.
        assert!(other("site.side-feed"));
        // A project named with a dot is its own, not the one it starts with.
        let waiting = [
            "tally.readme",
            "shop.web.lead",
            "shop.web.page",
            "shop.webby",
            "shop.fix",
        ];
        let shared = Shared {
            order: projects.clone(),
            agents: (waiting.iter())
                .map(|bot| {
                    let project = project_in(bot, &projects);
                    (projects[project], project.to_owned(), (*bot).to_owned())
                })
                .collect(),
            ..Default::default()
        };
        let outside = |project: &str| -> Vec<String> {
            (shared.outside(project).agents.into_iter())
                .map(|(_, bot)| bot)
                .collect()
        };
        assert_eq!(
            outside("shop"),
            ["tally.readme", "shop.web.lead", "shop.web.page"]
        );
        assert_eq!(
            outside("tally"),
            ["shop.web.lead", "shop.web.page", "shop.webby", "shop.fix"]
        );
        assert_eq!(
            outside("shop.web"),
            ["tally.readme", "shop.webby", "shop.fix"]
        );
        assert_eq!(shared.outside("shop.web").here, 3);
        assert_eq!(project_in("shop.web.page", &projects), "shop.web");
        assert_eq!(project_in("new.lead", &projects), "new");
    }

    /// A question goes up the makers to a lead no agent made: through
    /// leads, through one gone, and from one no agent made to its project's
    /// first lead; a ring stops where it closes.
    #[test]
    fn the_agents_above_one_are_its_makers_up_to_the_top() {
        let projects: HashMap<String, usize> = [("shop", 0), ("shop.web", 1), ("top", 2)]
            .map(|(name, at)| (name.to_owned(), at))
            .into();
        let mut made_by = Makers::default();
        for (bot, by) in [
            ("top.lead", ""),
            ("shop.lead", "top.lead"),
            ("shop.fix", "shop.lead"),
            ("shop.fix.tests", "shop.fix"),
            ("shop.own", ""),
            ("shop.orphan", "shop.gone"),
            ("shop.web.page", ""),
            ("shop.ring", "shop.loop"),
            ("shop.loop", "shop.ring"),
        ] {
            made_by.made(bot, by, &projects);
        }
        // Made again by no agent, and one gone for good: its first lead's.
        made_by.made("shop.again", "shop.fix", &projects);
        made_by.made("shop.again", "", &projects);
        made_by.made("shop.deleted", "shop.fix", &projects);
        made_by.gone("shop.deleted");
        // Made by its project's first lead, as most are: found without
        // being kept.
        made_by.made("shop.plain", "shop.lead", &projects);
        made_by.made("shop.web.page", "shop.web.lead", &projects);
        assert_eq!(made_by.by.len(), 5);
        assert_eq!(made_by.names.len(), 5);
        assert_eq!(
            above("shop.plain", &made_by, &projects),
            ["shop.lead", "top.lead"]
        );
        let above = |bot| above(bot, &made_by, &projects);
        assert_eq!(above("shop.again"), ["shop.lead", "top.lead"]);
        assert_eq!(above("shop.deleted"), ["shop.lead", "top.lead"]);
        assert_eq!(
            above("shop.fix.tests"),
            ["shop.fix", "shop.lead", "top.lead"]
        );
        assert_eq!(above("shop.lead"), ["top.lead"]);
        assert!(above("top.lead").is_empty());
        // Made by no agent: its project's first lead's, and what is above it.
        assert_eq!(above("shop.own"), ["shop.lead", "top.lead"]);
        assert_eq!(above("shop.web.page"), ["shop.web.lead"]);
        // Its maker gone, and one made since the follow listed (none).
        assert_eq!(above("shop.orphan"), ["shop.gone", "shop.lead", "top.lead"]);
        assert_eq!(above("shop.new"), ["shop.lead", "top.lead"]);
        assert_eq!(above("shop.ring"), ["shop.loop"]);
    }

    /// A window shows the questions of every agent below its own, and of
    /// every agent below the leads it follows, never its own.
    #[test]
    fn a_window_shows_every_question_below_it() {
        let ask = |question: &str| crate::asks::Ask {
            id: format!("ask:1/{question}"),
            question: question.into(),
            why: String::new(),
            options: Vec::new(),
            recommended: None,
            unanswered: None,
            deadline: None,
            covers: None,
            task: None,
        };
        let chain = |bots: &[&str]| bots.iter().map(|bot| (*bot).to_owned()).collect();
        let shared = Shared {
            above: [
                (
                    "shop.fix.tests".to_owned(),
                    chain(&["shop.fix", "shop.lead", "top.lead"]),
                ),
                ("shop.fix".to_owned(), chain(&["shop.lead", "top.lead"])),
                ("calc.add".to_owned(), chain(&["calc.lead"])),
                ("calc.lead".to_owned(), Vec::new()),
                ("top.lead".to_owned(), Vec::new()),
            ]
            .into(),
            asks: [
                ("shop.fix.tests".to_owned(), ask("Which suite?")),
                ("shop.fix".to_owned(), ask("Which fix?")),
                ("calc.add".to_owned(), ask("Round?")),
                ("calc.lead".to_owned(), ask("Which release?")),
                ("top.lead".to_owned(), ask("Own?")),
            ]
            .into(),
            ..Default::default()
        };
        let below = |window: &str, follows: &[String]| -> Vec<String> {
            (shared.below(window, follows))
                .map(|(bot, _)| bot.to_owned())
                .collect()
        };
        assert_eq!(below("top.lead", &[]), ["shop.fix", "shop.fix.tests"]);
        assert_eq!(below("shop.lead", &[]), ["shop.fix", "shop.fix.tests"]);
        assert_eq!(below("shop.fix", &[]), ["shop.fix.tests"]);
        assert!(below("shop.fix.tests", &[]).is_empty());
        // A lead followed: its own question too, and its tasks'.
        assert_eq!(
            below("top.lead", &["calc.lead".to_owned()]),
            ["calc.add", "calc.lead", "shop.fix", "shop.fix.tests"]
        );
        assert_eq!(below("calc.lead", &[]), ["calc.add"]);
        assert!(shared.is_below("calc.add", "top.lead", &["calc.lead".to_owned()]));
        assert!(!shared.is_below("top.lead", "top.lead", &[]));
    }

    /// The socket the others hear on fits a Unix socket's address, however
    /// long the store's path.
    #[test]
    fn the_shared_socket_fits_beside_a_long_store() {
        let short = std::env::temp_dir().join("s/state.sqlite");
        let (lock, told) = beside(&short);
        assert!(lock.to_string_lossy().ends_with("state.sqlite.asks.lock"));
        assert!(told.to_string_lossy().ends_with("state.sqlite.asks.sock"));
        let long = std::env::temp_dir()
            .join("a".repeat(120))
            .join("state.sqlite");
        let (lock, told) = beside(&long);
        assert!(lock.to_string_lossy().ends_with("state.sqlite.asks.lock"));
        assert!(
            std::os::unix::net::SocketAddr::from_pathname(&told).is_ok(),
            "{told:?}"
        );
    }

    /// Every agent's row is followed only while a window asks for them: a
    /// window that hears the one that follows gets every row, then only
    /// the rows that moved, and once none asks the follow is let go; the
    /// window that follows asks for them the same way.
    #[tokio::test]
    async fn every_agents_row_is_followed_once_for_the_windows_that_ask_and_only_while_they_do() {
        use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
        let dir = std::env::temp_dir().join(format!("hub-rows-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let socket = dir.join("agent.sock");
        let listener = tokio::net::UnixListener::bind(&socket).unwrap();
        // The rows' follows asked, the rows' connections gone, and the
        // events to send the rows' follow.
        let follows = Arc::new(std::sync::Mutex::new(0));
        let ended = Arc::new(std::sync::Mutex::new(0));
        let to_rows: Arc<std::sync::Mutex<Option<tokio::sync::mpsc::UnboundedSender<Value>>>> =
            Default::default();
        let heads = Arc::new(std::sync::Mutex::new(HashMap::from([
            ("site.lead".to_owned(), 1),
            ("tally.lead".to_owned(), 1),
        ])));
        {
            let (follows, ended, to_rows, heads) = (
                follows.clone(),
                ended.clone(),
                to_rows.clone(),
                heads.clone(),
            );
            tokio::spawn(async move {
                loop {
                    let (stream, _) = listener.accept().await.unwrap();
                    let (follows, ended, to_rows, heads) = (
                        follows.clone(),
                        ended.clone(),
                        to_rows.clone(),
                        heads.clone(),
                    );
                    tokio::spawn(async move {
                        let (read, mut write) = stream.into_split();
                        let ready = json!({"event": "ready", "protocol": agent_client::PROTOCOL});
                        write
                            .write_all(format!("{ready}\n").as_bytes())
                            .await
                            .unwrap();
                        let mut lines = BufReader::new(read).lines();
                        let (send, mut events) = tokio::sync::mpsc::unbounded_channel::<Value>();
                        let mut rows = false;
                        let row = |name: &str, heads: &HashMap<String, i64>| {
                            heads
                                .get(name)
                                .map(|head| json!({"name": name, "head": head}))
                        };
                        loop {
                            tokio::select! {
                                line = lines.next_line() => {
                                    let Ok(Some(line)) = line else { break };
                                    let request: Value = serde_json::from_str(&line).unwrap();
                                    let heads = heads.lock().unwrap().clone();
                                    let result = match request["op"].as_str() {
                                        Some("bots") if request["names"].is_array() => {
                                            let named: Vec<Value> = (request["names"].as_array().unwrap().iter())
                                                .filter_map(|name| row(name.as_str().unwrap(), &heads))
                                                .collect();
                                            json!({"bots": named})
                                        }
                                        Some("bots") if request["active"] == true => json!({"bots": []}),
                                        Some("bots") => {
                                            let mut names: Vec<&String> = heads.keys().collect();
                                            names.sort();
                                            let all: Vec<Value> =
                                                names.into_iter().filter_map(|name| row(name, &heads)).collect();
                                            json!({"bots": all, "cursor": 1})
                                        }
                                        Some("approvals") => json!({"approvals": []}),
                                        Some("follow") if request["kinds"] == json!(standing::KINDS) => {
                                            rows = true;
                                            *follows.lock().unwrap() += 1;
                                            *to_rows.lock().unwrap() = Some(send.clone());
                                            json!({"following": "*.*"})
                                        }
                                        _ => json!({}),
                                    };
                                    let out = json!({"id": request["id"], "result": result});
                                    if write.write_all(format!("{out}\n").as_bytes()).await.is_err() {
                                        break;
                                    }
                                    // Nothing to replay: each follow is live at once.
                                    if request["op"] == "follow" {
                                        let live = json!({"event": "follow_live",
                                            "bot": request["bot"], "cursor": 0});
                                        if write.write_all(format!("{live}\n").as_bytes()).await.is_err() {
                                            break;
                                        }
                                    }
                                }
                                Some(event) = events.recv() => {
                                    if write.write_all(format!("{event}\n").as_bytes()).await.is_err() {
                                        break;
                                    }
                                }
                            }
                        }
                        if rows {
                            *ended.lock().unwrap() += 1;
                        }
                    });
                }
            });
        }
        let wait = std::time::Duration::from_secs(5);
        async fn until(what: &str, done: impl Fn() -> bool) {
            let began = std::time::Instant::now();
            while !done() {
                assert!(
                    began.elapsed() < std::time::Duration::from_secs(5),
                    "waiting for {what}"
                );
                tokio::time::sleep(std::time::Duration::from_millis(5)).await;
            }
        }
        let store = dir.join("state.sqlite");
        let woken = Arc::new(Notify::new());
        let first = Elsewhere::start(socket.clone(), store.clone(), "tally".into(), woken.clone());
        let mut waiting = first.waiting.clone();
        tokio::time::timeout(wait, waiting.changed())
            .await
            .unwrap()
            .unwrap();
        let second = Elsewhere::start(socket.clone(), store, "site".into(), woken);
        // No window asks: no follow of every agent's row.
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        assert_eq!(*follows.lock().unwrap(), 0);
        let head = |open: &Elsewhere, name: &str| {
            open.rows(|rows| {
                rows.and_then(|rows| rows.agents.get(name))
                    .and_then(|row| row.head)
            })
        };
        // A window that hears asks: every row, followed once.
        second.want_rows(true);
        until("every row", || head(&second, "site.lead") == Some(1)).await;
        assert_eq!(head(&second, "tally.lead"), Some(1));
        assert_eq!(*follows.lock().unwrap(), 1);
        // One moves: its row alone is read and told.
        heads.lock().unwrap().insert("site.lead".into(), 9);
        let tell = |event: Value| {
            to_rows
                .lock()
                .unwrap()
                .as_ref()
                .unwrap()
                .send(event)
                .unwrap()
        };
        tell(
            json!({"cursor": 5, "bot": "site.lead", "event": "turn_finished",
            "following": "*.*"}),
        );
        until("the moved row", || head(&second, "site.lead") == Some(9)).await;
        // One goes: it is let go.
        heads.lock().unwrap().remove("tally.lead");
        tell(json!({"cursor": 6, "bot": "tally.lead", "event": "deleted",
            "following": "*.*"}));
        until("the row gone", || {
            second.rows(|rows| rows.is_some_and(|rows| !rows.agents.contains_key("tally.lead")))
        })
        .await;
        // The window that follows asks too: what is kept, no second follow.
        first.want_rows(true);
        until("the rows kept", || head(&first, "site.lead") == Some(9)).await;
        assert_eq!(*follows.lock().unwrap(), 1);
        // None asks any more: what each held is let go, and the follow too.
        let read = second.rows(|rows| rows.unwrap().version);
        second.want_rows(false);
        first.want_rows(false);
        assert!(second.rows(|rows| rows.is_none()));
        until("the follow let go", || *ended.lock().unwrap() == 1).await;
        // Asked again: followed again from a new listing.
        second.want_rows(true);
        until("every row again", || head(&second, "site.lead") == Some(9)).await;
        assert_eq!(*follows.lock().unwrap(), 2);
        // Listed anew, never at a version already read.
        assert!(second.rows(|rows| rows.unwrap().version) > read);
        // Let go and asked again at once, before the hub sees either: every
        // row comes again, to a window that hears and to the one that
        // follows.
        first.want_rows(true);
        until("the rows kept again", || {
            head(&first, "site.lead") == Some(9)
        })
        .await;
        for open in [&second, &first] {
            open.want_rows(false);
            open.want_rows(true);
        }
        until("every row after a quick reopen", || {
            head(&second, "site.lead") == Some(9) && head(&first, "site.lead") == Some(9)
        })
        .await;
    }

    /// What the follow says of an agent is the whole word only for an open
    /// agent of a project, once it told anything.
    #[test]
    fn the_follow_answers_only_for_the_open_agents_of_a_project_it_told() {
        assert_eq!(Shared::default().waits("shop.fix", false), None);
        let shared = Shared {
            order: HashMap::from([("shop".to_owned(), 0)]),
            agents: vec![(0, "shop".to_owned(), "shop.fix".to_owned())],
            ..Default::default()
        };
        assert_eq!(shared.waits("shop.fix", false), Some(true));
        assert_eq!(shared.waits("shop.lead", false), Some(false));
        assert_eq!(shared.waits("shop.fix", true), None);
        assert_eq!(shared.waits("calc", false), None);
        assert_eq!(shared.waits(".sky", false), None);
        assert_eq!(shared.waits("shop.lead.side-ab12", false), None);
    }

    /// An approval held in another project is told the window as the
    /// daemon's event comes, with no read on a timer; one in the window's
    /// own project is not. Every agent is followed for the few events of
    /// a turn; the held one's calls only from its approval on, and once it
    /// no longer waits, the connection is made again. One window of the
    /// store follows, the others hear it, and one of them follows on once
    /// it closes.
    #[tokio::test]
    async fn an_approval_in_another_project_is_told_as_its_event_comes_and_not_once_closed() {
        use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
        let dir = std::env::temp_dir().join(format!("elsewhere-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let socket = dir.join("agent.sock");
        let listener = tokio::net::UnixListener::bind(&socket).unwrap();
        // Each request as (connection, request); events sent to the newest.
        let (asked, mut requests) = tokio::sync::mpsc::unbounded_channel::<(usize, Value)>();
        let (go, gone) = tokio::sync::mpsc::unbounded_channel::<Value>();
        let gone = Arc::new(tokio::sync::Mutex::new(gone));
        tokio::spawn(async move {
            let mut at = 0;
            loop {
                let (stream, _) = listener.accept().await.unwrap();
                at += 1;
                let (asked, gone) = (asked.clone(), gone.clone());
                tokio::spawn(async move {
                    let (read, mut write) = stream.into_split();
                    let ready = json!({"event": "ready", "protocol": agent_client::PROTOCOL});
                    write
                        .write_all(format!("{ready}\n").as_bytes())
                        .await
                        .unwrap();
                    let mut lines = BufReader::new(read).lines();
                    let mut gone = gone.lock().await;
                    loop {
                        tokio::select! {
                            line = lines.next_line() => {
                                let Ok(Some(line)) = line else { return };
                                let request: Value = serde_json::from_str(&line).unwrap();
                                let result = match request["op"].as_str() {
                                    // At work, closed: its turn can still ask.
                                    Some("bots") if request["active"] == true => json!({"bots": [
                                        {"name": "site.shut", "bot_id": 3, "running_turn": 2,
                                            "closed_ms": 1}]}),
                                    Some("bots") => json!({"bots": [
                                        {"name": "site.lead", "bot_id": 2},
                                        {"name": "tally.lead", "bot_id": 1}], "cursor": 1}),
                                    // Held by an agent closed: no one's.
                                    Some("approvals") => json!({"approvals": [
                                        {"bot": "site.gone", "turn": 1, "call_id": "c0",
                                            "gates": ["manual"]}]}),
                                    _ => json!({}),
                                };
                                let _ = asked.send((at, request.clone()));
                                let out = json!({"id": request["id"], "result": result});
                                write.write_all(format!("{out}\n").as_bytes()).await.unwrap();
                                // Nothing to replay: each follow is live at once.
                                if request["op"] == "follow" {
                                    let live = json!({"event": "follow_live",
                                        "bot": request["bot"], "cursor": 0});
                                    write.write_all(format!("{live}\n").as_bytes()).await.unwrap();
                                }
                            }
                            Some(mut event) = gone.recv() => {
                                // The follow of every agent's, naming it.
                                if (event["event"].as_str()).is_some_and(|kind| WIDE.contains(&kind)) {
                                    event["following"] = json!("*.*");
                                }
                                write.write_all(format!("{event}\n").as_bytes()).await.unwrap();
                            }
                        }
                    }
                });
            }
        });
        let woken = Arc::new(Notify::new());
        let store = dir.join("state.sqlite");
        let open = Elsewhere::start(socket.clone(), store.clone(), "tally".into(), woken.clone());
        let mut waiting = open.waiting.clone();
        let held = |bot: &str, call: &str, cursor: i64| {
            json!({"cursor": cursor, "bot": bot, "turn": 1, "event": "approval_requested",
                "data": {"calls": [{"call_id": call, "gates": ["manual"], "name": "shell"}]}})
        };
        let started = |bot: &str, call: &str, cursor: i64| {
            json!({"cursor": cursor, "bot": bot, "turn": 1, "event": "tool_started",
                "data": {"call_id": call, "name": "shell"}})
        };
        let wait = std::time::Duration::from_secs(5);
        let op = |request: &Value| request["op"].as_str().unwrap_or("").to_owned();
        // Every agent, for the few events of a turn; the held calls and
        // the agents read once.
        let (_, first) = requests.recv().await.unwrap();
        assert_eq!(
            (op(&first), first["bot"].clone()),
            ("follow".into(), json!("*.*"))
        );
        assert_eq!(first["kinds"], json!(WIDE));
        // Then those at work, closed ones too.
        while requests.recv().await.unwrap().1["active"] != true {}
        // A second window of the store hears the first: it follows nothing.
        let second = Elsewhere::start(socket, store, "site".into(), woken);
        let mut heard = second.waiting.clone();
        // An approval in each project.
        go.send(held("tally.readme", "c1", 7)).unwrap();
        go.send(held("site.fix", "c2", 8)).unwrap();
        let names = |waiting: &Waiting| -> Vec<String> {
            waiting.agents.iter().map(|(_, bot)| bot.clone()).collect()
        };
        for shared in [&mut waiting, &mut heard] {
            tokio::time::timeout(wait, shared.wait_for(|shared| shared.agents.len() == 2))
                .await
                .unwrap()
                .unwrap();
        }
        // Each window leaves out its own project's.
        assert_eq!(names(&waiting.borrow().outside("tally")), ["site.fix"]);
        assert_eq!(names(&heard.borrow().outside("site")), ["tally.readme"]);
        // The held agents' calls, each from its approval on.
        for (bot, after) in [("tally.readme", 7), ("site.fix", 8)] {
            let (at, narrow) = requests.recv().await.unwrap();
            assert_eq!(at, 1);
            assert_eq!(
                narrow,
                json!({"id": narrow["id"], "op": "follow", "bot": bot,
                "after": after, "kinds": NARROW})
            );
        }
        // Allowed, they start: no one waits, and a new connection.
        go.send(started("tally.readme", "c1", 9)).unwrap();
        go.send(started("site.fix", "c2", 10)).unwrap();
        tokio::time::timeout(wait, heard.wait_for(|shared| shared.agents.is_empty()))
            .await
            .unwrap()
            .unwrap();
        let (again, follow) = tokio::time::timeout(wait, requests.recv())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(
            (
                again,
                op(&follow),
                follow["bot"].clone(),
                follow["after"].clone()
            ),
            (2, "follow".into(), json!("*.*"), json!(10))
        );
        // The first window closes: the second follows on, on its own.
        drop(open);
        let (at, follow) = tokio::time::timeout(wait, requests.recv())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(
            (at, op(&follow), follow["bot"].clone()),
            (3, "follow".into(), json!("*.*"))
        );
        go.send(held("tally.readme", "c3", 11)).unwrap();
        tokio::time::timeout(wait, heard.wait_for(|shared| shared.agents.len() == 1))
            .await
            .unwrap()
            .unwrap();
        assert_eq!(names(&heard.borrow().outside("site")), ["tally.readme"]);
        // Another call held by one that waits already, and one let go
        // while another waits on: each told, for the windows above.
        go.send(held("tally.readme", "c3b", 12)).unwrap();
        tokio::time::timeout(wait, heard.wait_for(|shared| shared.calls.len() == 2))
            .await
            .unwrap()
            .unwrap();
        go.send(started("tally.readme", "c3", 13)).unwrap();
        tokio::time::timeout(wait, heard.wait_for(|shared| shared.calls.len() == 1))
            .await
            .unwrap()
            .unwrap();
        assert_eq!(
            heard.borrow().calls,
            [("tally.readme".to_owned(), 1, "c3b".to_owned())].into()
        );
        // Closed while its turn runs, it needs no one, and what that turn
        // asks after needs no one either, till a message opens it again.
        let closed = json!({"cursor": 14, "bot": "tally.readme", "event": "closed",
            "data": {"closed_ms": 1}});
        go.send(closed).unwrap();
        tokio::time::timeout(wait, heard.wait_for(|shared| shared.agents.is_empty()))
            .await
            .unwrap()
            .unwrap();
        // No one waits: a new connection, which the next events go to.
        loop {
            let (at, follow) = tokio::time::timeout(wait, requests.recv())
                .await
                .unwrap()
                .unwrap();
            if at == 4 && op(&follow) == "follow" {
                break;
            }
        }
        go.send(held("tally.readme", "c4", 15)).unwrap();
        go.send(held("site.shut", "c6", 15)).unwrap();
        go.send(held("site.fix", "c5", 16)).unwrap();
        tokio::time::timeout(wait, heard.wait_for(|shared| !shared.agents.is_empty()))
            .await
            .unwrap()
            .unwrap();
        let bots: Vec<String> = (heard.borrow().agents.iter())
            .map(|(_, _, bot)| bot.clone())
            .collect();
        assert_eq!(bots, ["site.fix"]);
        drop(second);
        std::fs::remove_dir_all(dir).unwrap();
    }

    /// Applied in the order they came, an agent's call ending and its
    /// turn's end, on its own follow, run ahead of the older approval
    /// that held the call, still on its way on the follow of every agent
    /// as it replays: the agent would need the person until the follow
    /// starts again (#1222). Applied in the order of their cursors, it
    /// needs no one, and those that wait still do.
    #[tokio::test]
    async fn an_approval_replayed_after_its_own_end_does_not_wait() {
        use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
        let dir = std::env::temp_dir().join(format!("elsewhere-order-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let socket = dir.join("agent.sock");
        let listener = tokio::net::UnixListener::bind(&socket).unwrap();
        let (asked, mut requests) = tokio::sync::mpsc::unbounded_channel::<Value>();
        let wide = |event: Value| {
            let mut event = event;
            event["following"] = json!("*.*");
            event
        };
        let of = |bot: &str, kind: &str, call: &str, cursor: i64| {
            let data = match kind {
                "approval_requested" => {
                    json!({"calls": [{"call_id": call, "gates": ["manual"], "name": "shell"}]})
                }
                _ => json!({"call_id": call}),
            };
            json!({"cursor": cursor, "bot": bot, "turn": 1, "event": kind, "data": data})
        };
        let live =
            |of: &str, cursor: i64| json!({"event": "follow_live", "bot": of, "cursor": cursor});
        // Its approval, when the follow of every agent replays.
        let first = vec![
            live("site.wait", 6),
            wide(of("site.fix", "approval_requested", "c1", 7)),
        ];
        // Its own follow, at once to the present; then the follow of every
        // agent goes on with the older approval, and one more.
        let then = vec![
            of("site.fix", "tool_completed", "c1", 8),
            of("site.fix", "tool_completed", "c2", 10),
            of("site.fix", "turn_finished", "", 11),
            live("site.fix", 11),
            wide(of("site.fix", "approval_requested", "c2", 9)),
            wide(of("site.next", "approval_requested", "c3", 12)),
            live("*.*", 12),
        ];
        tokio::spawn(async move {
            let (stream, _) = listener.accept().await.unwrap();
            let (read, mut write) = stream.into_split();
            let ready = json!({"event": "ready", "protocol": agent_client::PROTOCOL});
            write
                .write_all(format!("{ready}\n").as_bytes())
                .await
                .unwrap();
            let mut lines = BufReader::new(read).lines();
            let (mut first, mut then) = (Some(first), Some(then));
            while let Ok(Some(line)) = lines.next_line().await {
                let request: Value = serde_json::from_str(&line).unwrap();
                let result = match request["op"].as_str() {
                    Some("bots") if request["active"] == true => json!({"bots": []}),
                    Some("bots") => json!({"bots": [
                        {"name": "site.fix", "bot_id": 3},
                        {"name": "site.lead", "bot_id": 2},
                        {"name": "site.next", "bot_id": 4},
                        {"name": "site.wait", "bot_id": 5},
                        {"name": "tally.lead", "bot_id": 1}], "cursor": 1}),
                    // Held when the follow starts.
                    Some("approvals") => json!({"approvals": [
                        {"bot": "site.wait", "turn": 1, "call_id": "c9", "gates": ["manual"]}]}),
                    Some("follow") => json!({"following": request["bot"],
                        "after": request["after"].as_i64().unwrap_or(6)}),
                    _ => json!({}),
                };
                let out = json!({"id": request["id"], "result": result});
                write
                    .write_all(format!("{out}\n").as_bytes())
                    .await
                    .unwrap();
                let events = match (request["op"].as_str(), request["bot"].as_str()) {
                    (Some("follow"), Some("site.wait")) => first.take(),
                    (Some("follow"), Some("site.fix")) => then.take(),
                    _ => None,
                };
                for event in events.into_iter().flatten() {
                    write
                        .write_all(format!("{event}\n").as_bytes())
                        .await
                        .unwrap();
                }
                let _ = asked.send(request);
            }
        });
        let store = dir.join("state.sqlite");
        let open = Elsewhere::start(socket, store, "tally".into(), Arc::new(Notify::new()));
        let mut waiting = open.waiting.clone();
        let wait = std::time::Duration::from_secs(5);
        tokio::time::timeout(
            wait,
            waiting.wait_for(|shared| shared.agents.iter().any(|(_, _, bot)| bot == "site.next")),
        )
        .await
        .unwrap()
        .unwrap();
        let bots: Vec<String> = (waiting.borrow().agents.iter())
            .map(|(_, _, bot)| bot.clone())
            .collect();
        assert_eq!(bots, ["site.next", "site.wait"]);
        // Its own follow, from the approval that held it.
        let mut follows = Vec::new();
        while let Ok(request) = requests.try_recv() {
            if request["op"] == "follow" {
                follows.push((request["bot"].clone(), request["after"].clone()));
            }
        }
        assert!(follows.contains(&(json!("site.fix"), json!(7))));
        drop(open);
        std::fs::remove_dir_all(dir).unwrap();
    }

    /// Events from follows that replay at once come out in the order of
    /// their cursors, none before every follow still replaying passed it;
    /// a follow's `follow_live` lets the rest go.
    #[test]
    fn events_come_out_in_cursor_order_while_a_follow_replays() {
        let mut lanes = Lanes::default();
        lanes.replaying.insert("*.*".into(), 6);
        lanes.replaying.insert("site.fix".into(), 7);
        let event = |cursor: i64, following: Option<&str>| {
            let mut event = json!({"cursor": cursor, "bot": "site.fix", "event": "e"});
            if let Some(following) = following {
                event["following"] = json!(following);
            }
            event
        };
        let given = |lanes: &mut Lanes| {
            std::iter::from_fn(|| lanes.next())
                .map(|event| event["cursor"].as_i64().unwrap())
                .collect::<Vec<_>>()
        };
        lanes.heard(event(8, None));
        lanes.heard(event(10, None));
        lanes.heard(json!({"event": "follow_live", "bot": "site.fix", "cursor": 10}));
        // The follow of every agent has passed none of them.
        assert!(given(&mut lanes).is_empty());
        lanes.heard(event(9, Some("*.*")));
        assert_eq!(given(&mut lanes), [8, 9]);
        assert!(!lanes.is_empty());
        lanes.heard(json!({"event": "follow_live", "bot": "*.*", "cursor": 10}));
        assert_eq!(given(&mut lanes), [10]);
        // Every follow live: each as it comes.
        lanes.heard(event(11, Some("*.*")));
        assert_eq!(given(&mut lanes), [11]);
        assert!(lanes.is_empty());
    }

    /// A question the person's words went past is read again at its
    /// turn's end, so its agent's turn's end is followed till then.
    #[test]
    fn an_agent_whose_question_is_unsure_is_followed_to_its_turn_end() {
        let event = |kind: &str, data: Value| json!({"bot": "site.fix", "turn": 1, "event": kind, "data": data});
        let mut needs = needs::Needs::default();
        let asked = json!({"call_id": "c1", "name": crate::asks::TOOL});
        needs.event(&event("tool_started", asked));
        let narrow: HashMap<String, bool> = [("site.fix".to_owned(), true)].into();
        assert!(!done_with(&narrow, &needs));
        // A reaction or words of the person's: answered or not, the
        // history says once the turn ends.
        needs.event(&event("accepted", json!({"text": "ok"})));
        assert!(!needs.waits("site.fix"));
        assert!(!done_with(&narrow, &needs));
        needs.event(&event("turn_finished", json!({})));
        assert_eq!(needs.due().len(), 1);
        needs.asked("site.fix", None);
        assert!(done_with(&narrow, &needs));
    }

    #[test]
    fn alt_n_goes_to_the_next_that_needs_you_around_the_end() {
        let waiting = ["tally.readme", "site.lead", "books.fix"];
        assert_eq!(next(&waiting, "tally.lead"), Some("tally.readme"));
        assert_eq!(next(&waiting, "tally.readme"), Some("site.lead"));
        assert_eq!(next(&waiting, "books.fix"), Some("tally.readme"));
        assert_eq!(next(&[], "tally.lead"), None);
    }

    /// Agents waiting in three projects, A, B and C as **Projects** lists
    /// them, two in B, and A's lead follows C's, so A's window counts
    /// `c.fix` as its own: from each agent's window, `alt+n` goes to the
    /// next, and so to all, whatever order a window's own are in.
    #[test]
    fn alt_n_reaches_every_project_from_any() {
        let places = [("a.lead", 0), ("b.ask", 1), ("b.lead", 1), ("c.fix", 2)];
        let own =
            |here: usize, bot: &str, place: usize| place == here || (here == 0 && bot == "c.fix");
        let from = |at: &str| {
            let here = places.iter().find(|(bot, _)| *bot == at).unwrap().1;
            let elsewhere = Waiting {
                here,
                agents: (places.iter())
                    .filter(|(bot, place)| !own(here, bot, *place))
                    .map(|(bot, place)| (*place, (*bot).to_owned()))
                    .collect(),
            };
            let local: Vec<(usize, String)> = (places.iter())
                .filter(|(bot, place)| own(here, bot, *place))
                .map(|(bot, place)| (*place, (*bot).to_owned()))
                .collect();
            let round: Vec<&str> = round(&local, &elsewhere);
            next(&round, at).map(str::to_owned)
        };
        let mut at = "a.lead".to_owned();
        let mut went = Vec::new();
        for _ in 0..places.len() {
            at = from(&at).unwrap();
            went.push(at.clone());
        }
        assert_eq!(went, ["b.lead", "b.ask", "c.fix", "a.lead"]);
    }
}
