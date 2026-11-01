//! Where a project's work is laid out: a terminal multiplexer the person
//! watches (Rex, Herdr or cmux). Each project has a home there (a Rex
//! session, a Herdr or cmux workspace), with a window per task and a block
//! labelled with each bot's name, so it can be found again by that name; a command a bot asked for is labelled `BOT/NAME` and
//! closes with its owner. `Layout` keeps what Sonny does there; a `Backend`
//! says how, in its multiplexer's CLI.
use std::{
    collections::{HashMap, HashSet},
    io::{BufRead, BufReader, Read, Write},
    os::unix::net::UnixStream,
    path::{Path, PathBuf},
    process::{Child, Command, Output, Stdio},
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, AtomicU64, Ordering},
        mpsc,
    },
    time::{Duration, Instant, SystemTime},
};

/// How long a multiplexer's CLI may take to answer: one whose server has
/// stopped answering is given up on, rather than holding Sonny's screen.
const ANSWER: Duration = Duration::from_secs(10);

/// `command`'s output, as `Command::output` gives it, unless the command
/// takes longer than [`ANSWER`]: then it is stopped, and that is the error.
pub fn output(command: &mut Command) -> std::io::Result<Output> {
    output_within(command, ANSWER)
}

fn output_within(command: &mut Command, limit: Duration) -> std::io::Result<Output> {
    let mut child = command
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()?;
    // Each pipe read to its end on a thread of its own, so a full one
    // never holds the other up.
    fn read(pipe: Option<impl Read + Send + 'static>) -> mpsc::Receiver<Vec<u8>> {
        let (sent, read) = mpsc::channel();
        std::thread::spawn(move || {
            let mut bytes = Vec::new();
            if let Some(mut pipe) = pipe {
                let _ = pipe.read_to_end(&mut bytes);
            }
            let _ = sent.send(bytes);
        });
        read
    }
    let (stdout, stderr) = (read(child.stdout.take()), read(child.stderr.take()));
    let deadline = Instant::now() + limit;
    let read = |pipe: &mpsc::Receiver<Vec<u8>>| {
        pipe.recv_timeout(deadline.saturating_duration_since(Instant::now()))
            .ok()
    };
    let answered = 'answered: {
        let (Some(stdout), Some(stderr)) = (read(&stdout), read(&stderr)) else {
            break 'answered None;
        };
        // Its output closed, the command has all but exited.
        loop {
            match child.try_wait() {
                Ok(Some(status)) => {
                    break 'answered Some(Output {
                        status,
                        stdout,
                        stderr,
                    });
                }
                Ok(None) if Instant::now() < deadline => {
                    std::thread::sleep(Duration::from_millis(1))
                }
                _ => break 'answered None,
            }
        }
    };
    answered.ok_or_else(|| {
        let _ = child.kill();
        let _ = child.wait();
        std::io::Error::new(
            std::io::ErrorKind::TimedOut,
            format!("no answer in {limit:?}"),
        )
    })
}

/// A block as its multiplexer lists it.
#[derive(Clone)]
pub struct Block {
    pub id: String,
    pub label: String,
    pub window: String,
}

/// Where a block may go beside another: on a side, or floating over its
/// window, for a moment.
pub const SPLITS: [&str; 5] = ["right", "left", "above", "below", FLOAT];
pub const FLOAT: &str = "float";

/// Where `show_agent` puts a task.
pub enum Placement {
    Window,
    /// Beside a block (a bot's, by its label), on one side.
    Split {
        side: String,
        beside: String,
    },
}

/// How many of a project's task windows a lead keeps open, as VS Code's
/// `workbench.editor.limit` keeps editors: past `most`, opening one closes
/// the window the person used least recently. A window whose task needs
/// the person or failed, or that holds blocks its task asked for (a dev
/// server it runs), is never closed for room, as VS Code keeps a dirty
/// editor; with no other, the limit is passed. A window the person has
/// not used yet is not one they used least recently: it opened on its
/// own, as a VS Code editor never does, and closing it would take new
/// work out of view before they saw it. It stays, and while only such
/// windows could make room a new task's window does not open, until the
/// person asks for it.
pub struct Room {
    /// Task windows at most, 0 for no limit.
    pub most: usize,
    /// The project whose tasks' windows count: `shop.fix` for `shop`.
    pub project: String,
    /// The lead's own tasks: only their windows count and close, as
    /// another lead of the folder shares the home and keeps its own.
    pub mine: HashSet<String>,
    /// Tasks whose window stays, by name.
    pub keep: HashSet<String>,
    /// When the person last used a task's window there, as its own Sonny
    /// saw it.
    pub used: Used,
    /// Whether a task's window says its task failed or needs the person,
    /// as its own Sonny last reported: it stays, also when the lead started
    /// again since and has not heard so itself.
    pub held: Held,
}

/// When the person last used a task's window, by its task.
pub type Used = Arc<dyn Fn(&str) -> Option<SystemTime> + Send + Sync>;
/// Whether a task's window holds its place, by its task.
pub type Held = Arc<dyn Fn(&str) -> bool + Send + Sync>;

/// Where a lead keeps which of its tasks' windows it closed for room, so
/// that a lead started again still opens one the person asks to see.
#[derive(Clone)]
pub struct Aside {
    pub was: Held,
    pub set: SetAside,
}

/// Mark a task's window as closed for room, or not.
pub type SetAside = Arc<dyn Fn(&str, bool) + Send + Sync>;

/// What [`Layout::show_task`] did.
#[derive(Debug, PartialEq)]
pub enum Shown {
    /// The task had a window already.
    Had,
    /// The task's window was closed for room, and opens again only when
    /// the person asks for it.
    Aside,
    /// A window opened, closing these tasks' for room.
    Opened(Vec<String>),
    /// No window opened: only windows the person has not used yet could
    /// have made room. It opens when the person asks for it.
    NoRoom,
}

/// Where each block sits, by label: its window, counted from 1, and in a
/// split, its side of its neighbour (`right of`, `below`, … a label).
pub type Places = HashMap<String, (usize, Option<(String, String)>)>;

pub trait Backend: Send {
    /// The project's home as the person reads it: `Rex session shop`.
    fn home(&self) -> String;
    /// Whether that home is the one Sonny runs in.
    fn here(&self) -> bool;
    /// Whether that home was made just now, with a shell nobody asked for.
    fn made(&self) -> bool;
    /// The home's blocks, in the multiplexer's order, or why they could
    /// not be read.
    fn blocks(&self) -> Result<Vec<Block>, String>;
    /// What a [`window`](Self::window) opened `block` in, which counts and
    /// closes as one against a lead's [`Room`]: its window, where a window
    /// holds blocks side by side (Rex's windows, Herdr's tabs).
    fn tab(&self, block: &Block) -> String {
        block.window.clone()
    }
    fn close_block(&self, block: &str);
    fn rename_block(&self, block: &str, label: &str);
    fn rename_window(&self, window: &str, name: &str);
    /// A new window named `name` whose one block, labelled `label`, runs
    /// `command`, leaving the person's focus where it is. `keep`: in `cwd`,
    /// and left open once the command exits so it can be read; otherwise
    /// it goes with the command. Returns the block's ID.
    fn window(
        &self,
        label: &str,
        name: &str,
        cwd: &Path,
        command: &[String],
        keep: bool,
    ) -> Result<String, String>;
    /// A new block beside `anchor`, on `side` (`right`, `left`, `above`,
    /// `below`), as `window` makes one; `small` makes it the smaller part.
    #[allow(clippy::too_many_arguments)]
    fn split(
        &self,
        anchor: &str,
        side: &str,
        small: bool,
        label: &str,
        cwd: &Path,
        command: &[String],
        keep: bool,
    ) -> Result<String, String>;
    /// A new block floating over `anchor`'s window, as `split` makes one.
    /// A multiplexer that cannot float one splits it on the right.
    fn float(
        &self,
        anchor: &str,
        label: &str,
        cwd: &Path,
        command: &[String],
        keep: bool,
    ) -> Result<String, String> {
        self.split(anchor, "right", false, label, cwd, command, keep)
    }
    /// Bring the block into view: its window made the one shown.
    fn focus(&self, block: &Block) -> Result<(), String>;
    /// The block Sonny itself runs in, when in this home.
    fn own_block(&self) -> Option<String>;
    fn places(&self) -> Result<Places, String>;
    /// The home, to find again.
    fn place(&self) -> Place;
    /// Where the multiplexer says what changes in the home, when it can.
    fn watch(&self) -> Option<Watch> {
        None
    }
}

/// A multiplexer's news of what changes in a home, one JSON object a line,
/// so that the blocks listed once are listed again only once one changed.
/// The host's events say that something changed, not what: the home is
/// read again whole, as Herdr's and cmux's event docs ask of a client
/// that keeps their state.
pub struct Watch {
    pub from: Stream,
    /// What a line says.
    pub heard: Box<dyn Fn(&serde_json::Value) -> Heard + Send>,
}

pub enum Stream {
    /// A command that prints the news until it is stopped.
    Command(Command),
    /// A socket that sends the news once asked, by the line given.
    Socket(PathBuf, String),
}

#[derive(Debug, PartialEq)]
pub enum Heard {
    /// Something in the home may have changed.
    Changed,
    /// Nothing that moves a block, said by a stream that is up.
    Same,
    /// The stream will say no more.
    Over,
}

/// What a home's watch has heard: whether it is up, and how many times the
/// home changed, which what was read before is checked against.
#[derive(Default)]
struct News {
    up: AtomicBool,
    changes: AtomicU64,
}

impl News {
    fn changes(&self) -> u64 {
        self.changes.load(Ordering::Acquire)
    }

    fn changed(&self) {
        self.changes.fetch_add(1, Ordering::AcqRel);
    }
}

/// What a watch holds open, closed with its layout.
enum Watching {
    Command(Child),
    Socket(UnixStream),
}

impl Watching {
    fn close(self) {
        match self {
            Self::Command(mut child) => {
                let _ = child.kill();
                let _ = child.wait();
            }
            Self::Socket(socket) => {
                let _ = socket.shutdown(std::net::Shutdown::Both);
            }
        }
    }
}

/// The watch's stream, opened on its own thread so that a host slow to
/// take it holds up no work. Once the layout is gone, one opened after
/// is closed at once.
#[derive(Default)]
struct Following {
    gone: bool,
    stream: Option<Watching>,
}

/// A project's home, as one of its Sonny windows found it in the
/// person's terminal: kept so that an agent's `run_in_pane` and
/// `show_agent` calls go there, to that multiplexer, instead of one
/// guessed from where the lookout answering them runs, which is in
/// neither.
#[derive(Clone, Debug, PartialEq)]
pub enum Place {
    Herdr {
        workspace: String,
    },
    Cmux {
        workspace: String,
    },
    Rex {
        session: String,
        host: Option<String>,
    },
}

impl Place {
    /// One file a project of a daemon, under `~/.agent/sonny/places`.
    fn file(socket: &Path, project: &str) -> Option<PathBuf> {
        let socket = std::fs::canonicalize(socket).unwrap_or_else(|_| socket.to_owned());
        let key = crate::project::fingerprint(&format!("{}\n{project}", socket.display()));
        let home = std::env::var_os("HOME").filter(|h| !h.is_empty())?;
        Some(PathBuf::from(home).join(format!(".agent/sonny/places/{key}.json")))
    }

    fn json(&self) -> serde_json::Value {
        match self {
            Self::Herdr { workspace } => {
                serde_json::json!({"multiplexer": "herdr", "workspace": workspace})
            }
            Self::Cmux { workspace } => {
                serde_json::json!({"multiplexer": "cmux", "workspace": workspace})
            }
            Self::Rex { session, host } => {
                serde_json::json!({"multiplexer": "rex", "session": session, "host": host})
            }
        }
    }

    /// A window's place, or none, as `--window-place` carries it.
    pub fn encode(place: &Option<Place>) -> String {
        place
            .as_ref()
            .map_or(serde_json::Value::Null, Place::json)
            .to_string()
    }

    pub fn decode(text: &str) -> Option<Option<Place>> {
        let json: serde_json::Value = serde_json::from_str(text).ok()?;
        match json.is_null() {
            true => Some(None),
            false => Place::of(&json).map(Some),
        }
    }

    fn of(json: &serde_json::Value) -> Option<Self> {
        let text = |key: &str| json[key].as_str().map(str::to_owned);
        match json["multiplexer"].as_str()? {
            "herdr" => Some(Self::Herdr {
                workspace: text("workspace")?,
            }),
            "cmux" => Some(Self::Cmux {
                workspace: text("workspace")?,
            }),
            "rex" => Some(Self::Rex {
                session: text("session")?,
                host: text("host"),
            }),
            _ => None,
        }
    }
}

/// One of a project's Sonny windows, as the person last used it: its
/// process, the agent it shows, when, and where it is, `None` for a
/// terminal in no multiplexer.
#[derive(Clone, Debug, PartialEq)]
pub struct Window {
    pub pid: u32,
    pub bot: String,
    pub used_ms: u64,
    pub place: Option<Place>,
}

/// A project's Sonny windows, newest use first, from the project's file.
fn windows(file: &Path) -> Vec<Window> {
    let json: serde_json::Value = (std::fs::read(file).ok())
        .and_then(|text| serde_json::from_slice(&text).ok())
        .unwrap_or_default();
    let mut windows: Vec<Window> = (json["windows"].as_array().into_iter().flatten())
        .filter_map(|window| {
            Some(Window {
                pid: u32::try_from(window["pid"].as_u64()?).ok()?,
                bot: window["bot"].as_str().unwrap_or_default().to_owned(),
                used_ms: window["used_ms"].as_u64()?,
                place: Place::of(&window["place"]),
            })
        })
        .collect();
    windows.sort_by_key(|window| std::cmp::Reverse(window.used_ms));
    windows
}

/// Whether process `pid` still runs: one this user may not signal runs too.
fn alive(pid: u32) -> bool {
    // 0 is a window closed by the process that goes on as another Sonny,
    // and `kill(0, …)` would ask about this process group instead.
    let Ok(pid) = libc::pid_t::try_from(pid) else {
        return false;
    };
    if pid == 0 {
        return false;
    }
    // SAFETY: signal 0 checks the process without sending it anything,
    // and `kill` reads nothing of this process's memory.
    let signalled = unsafe { libc::kill(pid, 0) } == 0;
    signalled || std::io::Error::last_os_error().raw_os_error() == Some(libc::EPERM)
}

/// The window the person used last: the newest of those still open, else
/// the newest of all, and whether it is still open. As tmux acts on the
/// client used most recently when a command names none, this is where an
/// agent's window opens and its lead's new tabs go: the window being
/// typed in, not one in another terminal that only laid the project out
/// first.
fn latest_in(file: &Path, alive: impl Fn(u32) -> bool) -> Option<(Window, bool)> {
    let windows = windows(file);
    if let Some(open) = windows.iter().find(|window| alive(window.pid)) {
        return Some((open.clone(), true));
    }
    windows.into_iter().next().map(|window| (window, false))
}

pub fn latest(socket: &Path, project: &str) -> Option<(Window, bool)> {
    latest_in(&Place::file(socket, project)?, alive)
}

/// The window an agent acts in: the open one the person used last among
/// those showing it, else among those showing the agent that made it, and
/// so on up `chain` (the agent first, then its makers); else the project's
/// window used last. So an agent spoken to in one terminal acts there
/// while another terminal talks to another of the project's agents.
fn latest_for_in(
    file: &Path,
    chain: &[String],
    alive: impl Fn(u32) -> bool,
) -> Option<(Window, bool)> {
    let windows = windows(file);
    for bot in chain {
        if let Some(open) = (windows.iter()).find(|window| window.bot == *bot && alive(window.pid))
        {
            return Some((open.clone(), true));
        }
    }
    latest_in(file, alive)
}

pub fn latest_for(socket: &Path, project: &str, chain: &[String]) -> Option<(Window, bool)> {
    latest_for_in(&Place::file(socket, project)?, chain, alive)
}

/// Notes that the person used this window of `project`'s just now, at
/// `place`. The windows since closed go, but for the newest, whose place
/// still counts once every window is closed.
fn mark_in(file: &Path, me: Window, alive: impl Fn(u32) -> bool) {
    let mut kept: Vec<Window> = Vec::new();
    let mut closed = false;
    for window in windows(file) {
        if window.pid == me.pid {
            continue;
        }
        if alive(window.pid) {
            kept.push(window);
        } else if !closed {
            closed = true;
            kept.push(window);
        }
    }
    kept.insert(0, me);
    write(file, &kept);
}

/// Notes that window `me` of the project closed, though its process goes
/// on as another agent's Sonny (`exec` keeps the process): its place
/// stays, as a closed window's.
fn retire_in(file: &Path, me: u32) {
    let mut windows = windows(file);
    if !windows.iter().any(|window| window.pid == me) {
        return;
    }
    for window in windows.iter_mut().filter(|window| window.pid == me) {
        window.pid = 0;
    }
    write(file, &windows);
}

fn write(file: &Path, windows: &[Window]) {
    let list: Vec<serde_json::Value> = (windows.iter())
        .map(|window| {
            serde_json::json!({
                "pid": window.pid,
                "bot": window.bot,
                "used_ms": window.used_ms,
                "place": window.place.as_ref().map(Place::json),
            })
        })
        .collect();
    if let Some(dir) = file.parent() {
        let _ = std::fs::create_dir_all(dir);
    }
    // Two windows writing at once can drop the other's line: it is written
    // again the next time the person uses that window.
    let temp = file.with_extension(format!("{}.tmp", std::process::id()));
    let json = serde_json::json!({ "windows": list });
    if std::fs::write(&temp, format!("{json}\n")).is_ok() {
        let _ = std::fs::rename(&temp, file);
    }
}

fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_millis() as u64)
}

/// This Sonny window of a project, noted as the one the person uses each
/// time they do, at most once a second.
pub struct Marker {
    file: PathBuf,
    /// The agent this window shows.
    bot: String,
    place: Option<Place>,
    /// A window of an agent, not a lead, takes the place of the project's
    /// window used last, since it opened in that one's home.
    follows: bool,
    last: Option<Instant>,
}

impl Marker {
    /// A lead's window, laid out at `place` or in no multiplexer.
    pub fn lead(socket: &Path, project: &str, bot: &str, place: Option<Place>) -> Option<Self> {
        Some(Self {
            file: Place::file(socket, project)?,
            bot: bot.to_owned(),
            place,
            follows: false,
            last: None,
        })
    }

    /// The window of one of the project's agents, not its lead: `here`
    /// when it runs in a multiplexer, so in the home its lead's window
    /// laid out there.
    /// `given` is the place of the window this one replaced in the same
    /// terminal (`/switch`), which is this one's too.
    pub fn agent(
        socket: &Path,
        project: &str,
        bot: &str,
        here: bool,
        given: Option<Option<Place>>,
    ) -> Option<Self> {
        Some(Self {
            file: Place::file(socket, project)?,
            bot: bot.to_owned(),
            follows: here && given.is_none(),
            place: given.flatten(),
            last: None,
        })
    }

    /// Where this window is, once known: handed to the Sonny that
    /// replaces it in the same terminal.
    pub fn known(&self) -> Option<Option<Place>> {
        (!self.follows).then(|| self.place.clone())
    }

    /// Typing: noted at most once a second.
    pub fn used(&mut self) {
        if self
            .last
            .is_some_and(|last| last.elapsed() < Duration::from_secs(1))
        {
            return;
        }
        self.used_now();
    }

    /// A click, a paste, Enter or the window coming to the front: noted
    /// at once, as the person may have just come back from another window.
    pub fn used_now(&mut self) {
        self.last = Some(Instant::now());
        // An agent's window Sonny opens is told its home; one opened by
        // hand in a multiplexer was most likely opened in the home of the
        // window used last that is in one, open or since closed. That home
        // is its own from then on, wherever the person goes after.
        if self.follows {
            let windows = windows(&self.file);
            let opened_in = (windows.iter())
                .find(|window| window.place.is_some() && alive(window.pid))
                .or_else(|| windows.iter().find(|window| window.place.is_some()))
                .cloned();
            let Some(window) = opened_in else {
                return;
            };
            self.place = window.place;
            self.follows = false;
        }
        let place = self.place.clone();
        let me = Window {
            pid: std::process::id(),
            bot: self.bot.clone(),
            used_ms: now_ms(),
            place,
        };
        mark_in(&self.file, me, alive);
    }

    /// This window is gone, though its process goes on as another
    /// agent's Sonny, which marks itself.
    pub fn retire(&self) {
        retire_in(&self.file, std::process::id());
    }
}

/// Whether the window `lead` acts in ([`latest_for`]) is in `home`, the
/// home of this lead's window (that window, or an agent's in the same
/// home), or no open window is known: only then does the lead's new work
/// open there.
pub fn mine(socket: &Path, project: &str, lead: &str, home: &Place) -> bool {
    Place::file(socket, project)
        .is_none_or(|file| mine_in(&file, std::process::id(), lead, home, alive))
}

fn mine_in(file: &Path, me: u32, lead: &str, home: &Place, alive: impl Fn(u32) -> bool) -> bool {
    latest_for_in(file, &[lead.to_owned()], alive).is_none_or(|(window, open)| {
        !open || window.pid == me || window.place.as_ref() == Some(home)
    })
}

pub struct Layout {
    backend: Box<dyn Backend>,
    /// Each task's block, by bot name.
    blocks: HashMap<String, String>,
    /// The blocks a home made just now came with, closed once something
    /// is placed in it.
    spare: Vec<String>,
    /// The home's blocks and places as last read, with the changes the
    /// watch had heard of by then.
    listed: Option<(u64, Vec<Block>)>,
    placed: Option<(u64, Places)>,
    news: Arc<News>,
    following: Arc<Mutex<Following>>,
    /// When Sonny last opened or brought each task's window into view.
    used: HashMap<String, SystemTime>,
    /// Tasks whose window was closed for room ([`Room`]), and where they
    /// are kept past this Sonny, when they are.
    aside: HashSet<String>,
    kept: Option<Aside>,
}

impl Drop for Layout {
    fn drop(&mut self) {
        let mut following = self.following.lock().unwrap_or_else(|e| e.into_inner());
        following.gone = true;
        if let Some(stream) = following.stream.take() {
            stream.close();
        }
    }
}

impl Layout {
    /// Where its home is, to be kept for the window that laid it out.
    pub fn kept(&self) -> Place {
        self.backend.place()
    }

    pub fn new(backend: Box<dyn Backend>) -> Self {
        let spare = match backend.made() {
            true => (backend.blocks().unwrap_or_default().into_iter())
                .map(|b| b.id)
                .collect(),
            false => Vec::new(),
        };
        Self {
            backend,
            blocks: HashMap::new(),
            spare,
            listed: None,
            placed: None,
            news: Arc::default(),
            following: Arc::default(),
            used: HashMap::new(),
            aside: HashSet::new(),
            kept: None,
        }
    }

    /// Keep which tasks' windows were closed for room in `kept` too.
    pub fn keep_aside(&mut self, kept: Aside) {
        self.kept = Some(kept);
    }

    fn set_aside(&mut self, bot: &str, aside: bool) {
        let changed = match aside {
            true => self.aside.insert(bot.to_owned()),
            false => self.aside.remove(bot),
        };
        if let Some(kept) = &self.kept
            && (changed || (kept.was)(bot) != aside)
        {
            (kept.set)(bot, aside);
        }
    }

    /// Follow the home's news, when its multiplexer gives it: from then
    /// on, what was read is read again only once the news says the home
    /// changed. The first line heard says the stream is up; until then,
    /// and once it ends, the home is read again for each batch of work.
    pub fn watch(&mut self) {
        let Some(Watch { from, heard }) = self.backend.watch() else {
            return;
        };
        let news = self.news.clone();
        let following = self.following.clone();
        std::thread::spawn(move || {
            let (stream, lines): (_, Box<dyn BufRead + Send>) = match from {
                Stream::Command(mut command) => {
                    let Ok(mut child) = command
                        .stdin(Stdio::null())
                        .stdout(Stdio::piped())
                        .stderr(Stdio::null())
                        .spawn()
                    else {
                        return;
                    };
                    let Some(out) = child.stdout.take() else {
                        return;
                    };
                    (Watching::Command(child), Box::new(BufReader::new(out)))
                }
                Stream::Socket(path, ask) => {
                    let Ok(mut socket) = UnixStream::connect(&path) else {
                        return;
                    };
                    let Ok(kept) = socket.try_clone() else {
                        return;
                    };
                    if writeln!(socket, "{ask}").is_err() {
                        return;
                    }
                    (Watching::Socket(kept), Box::new(BufReader::new(socket)))
                }
            };
            {
                let mut following = following.lock().unwrap_or_else(|e| e.into_inner());
                if following.gone {
                    stream.close();
                    return;
                }
                following.stream = Some(stream);
            }
            for line in lines.lines() {
                let Ok(line) = line else { break };
                let said = serde_json::from_str(&line).map_or(Heard::Changed, |line| heard(&line));
                if said == Heard::Over {
                    break;
                }
                // Up just now: what was read before it may have missed a
                // change, so it is read again.
                if !news.up.swap(true, Ordering::AcqRel) || said == Heard::Changed {
                    news.changed();
                }
            }
            news.up.store(false, Ordering::Release);
            news.changed();
        });
    }

    /// Ready for a batch of work: with no news followed, what was read
    /// before may be out of date, so it is read again when next asked.
    pub fn fresh(&mut self) {
        if !self.news.up.load(Ordering::Acquire) {
            self.listed = None;
            self.placed = None;
        }
    }

    /// The home's blocks, as last read unless the home changed since. A
    /// read that failed is none, and tried again when next asked.
    fn listed(&mut self) -> Vec<Block> {
        let now = self.news.changes();
        if let Some((at, blocks)) = &self.listed
            && *at == now
        {
            return blocks.clone();
        }
        let Ok(blocks) = self.backend.blocks() else {
            return Vec::new();
        };
        self.listed = Some((now, blocks.clone()));
        blocks
    }

    /// What Sonny itself changes is read again after.
    fn forget(&mut self) {
        self.listed = None;
        self.placed = None;
    }

    /// A close or rename is not confirmed by its multiplexer, so the
    /// home is read again after rather than guessed.
    fn close_block(&mut self, block: &str) {
        self.backend.close_block(block);
        self.forget();
    }

    fn rename_block(&mut self, block: &str, label: &str) {
        self.backend.rename_block(block, label);
        self.forget();
    }

    /// Close the shell a home made just now came with, now that it holds
    /// something of its own.
    fn tidy(&mut self) {
        for block in std::mem::take(&mut self.spare) {
            self.close_block(&block);
        }
    }

    pub fn home(&self) -> String {
        self.backend.home()
    }

    pub fn here(&self) -> bool {
        self.backend.here()
    }

    /// A block already labelled with the bot's name.
    fn adopt(&mut self, bot: &str, listed: &[Block]) -> bool {
        let Some(found) = listed.iter().find(|b| b.label == bot) else {
            return false;
        };
        self.blocks.insert(bot.to_owned(), found.id.clone());
        true
    }

    /// A window for the task running `command`, named `name`, unless the
    /// task has a block already. Returns whether one was opened. The
    /// person's focus stays where it is.
    pub fn show(&mut self, bot: &str, name: &str, command: &[String]) -> Result<bool, String> {
        let listed = self.listed();
        if let Some(block) = self.blocks.get(bot)
            && listed.iter().any(|b| &b.id == block)
        {
            return Ok(false);
        }
        if self.adopt(bot, &listed) {
            return Ok(false);
        }
        self.place(bot, name, command, &Placement::Window)?;
        Ok(true)
    }

    /// A window for the task running `command`, named `name`, as
    /// [`show`](Self::show) opens one, within `room`. A task whose window
    /// was closed for room stays without one until `asked`: the person
    /// asked to see it.
    pub fn show_task(
        &mut self,
        bot: &str,
        name: &str,
        command: &[String],
        room: &Room,
        asked: bool,
    ) -> Result<Shown, String> {
        let listed = self.listed();
        if let Some(block) = self.blocks.get(bot)
            && listed.iter().any(|b| &b.id == block)
        {
            return Ok(Shown::Had);
        }
        if self.adopt(bot, &listed) {
            return Ok(Shown::Had);
        }
        if self.aside(bot) && !asked {
            return Ok(Shown::Aside);
        }
        let Some(closed) = self.make_room(bot, &listed, room, asked) else {
            self.set_aside(bot, true);
            return Ok(Shown::NoRoom);
        };
        self.place(bot, name, command, &Placement::Window)?;
        self.set_aside(bot, false);
        Ok(Shown::Opened(closed))
    }

    /// Whether the task's window was closed for room.
    pub fn aside(&self, bot: &str) -> bool {
        self.aside.contains(bot) || self.kept.as_ref().is_some_and(|kept| (kept.was)(bot))
    }

    /// Close the windows the person used least recently, so that one more
    /// for `bot` keeps the project's within `room`. A window counts once,
    /// with every task placed in it, and closes whole. Returns their tasks;
    /// or none, closing nothing, when windows the person has not used yet
    /// stand in the way of the room and they did not ask for this one.
    fn make_room(
        &mut self,
        bot: &str,
        listed: &[Block],
        room: &Room,
        asked: bool,
    ) -> Option<Vec<String>> {
        if room.most == 0 {
            return Some(Vec::new());
        }
        let tasks = format!("{}.", room.project);
        let owners: HashSet<&str> = (listed.iter())
            .filter_map(|b| b.label.split_once('/').map(|(owner, _)| owner))
            .collect();
        let task = |label: &str| {
            label.starts_with(&tasks)
                && !label.contains('/')
                && !crate::project::is_lead(label)
                && room.mine.contains(label)
                && label != bot
        };
        // Each window and its tasks; a task window holds this lead's tasks
        // and the blocks they asked for alone (a task split beside its
        // lead is the lead's).
        let mut windows: Vec<(&str, Vec<&str>)> = Vec::new();
        let mut others: HashSet<&str> = HashSet::new();
        let tabs: Vec<String> = listed.iter().map(|block| self.backend.tab(block)).collect();
        for (block, tab) in listed.iter().zip(&tabs) {
            let owner =
                (block.label.split_once('/')).map_or(block.label.as_str(), |(owner, _)| owner);
            if !task(owner) {
                others.insert(tab);
                continue;
            }
            match windows.iter_mut().find(|(window, _)| *window == tab) {
                Some((_, labels)) => labels.push(owner),
                None => windows.push((tab, vec![owner])),
            }
        }
        windows.retain(|(window, _)| !others.contains(window));
        for (_, labels) in &mut windows {
            labels.sort_unstable();
            labels.dedup();
        }
        let over = (windows.len() + 1).saturating_sub(room.most);
        if over == 0 {
            return Some(Vec::new());
        }
        let (mut spare, mut unseen) = (Vec::<(SystemTime, Vec<&str>)>::new(), false);
        for (_, labels) in windows {
            let holds = |label: &&str| {
                room.keep.contains(*label) || owners.contains(label) || (room.held)(label)
            };
            if labels.iter().any(holds) {
                continue;
            }
            let used = (labels.iter())
                .filter_map(|label| self.used.get(*label).copied().max((room.used)(label)))
                .max();
            match used {
                Some(used) => spare.push((used, labels)),
                None => unseen = true,
            }
        }
        if spare.len() < over && unseen && !asked {
            return None;
        }
        spare.sort_unstable();
        let mut closed: Vec<String> = (spare.into_iter().take(over))
            .flat_map(|(_, labels)| labels)
            .map(str::to_owned)
            .collect();
        closed.sort_unstable();
        closed.dedup();
        for bot in &closed {
            self.close(bot);
            self.set_aside(bot, true);
        }
        Some(closed)
    }

    /// Open the task's block where asked, closing any it had.
    pub fn place(
        &mut self,
        bot: &str,
        name: &str,
        command: &[String],
        placement: &Placement,
    ) -> Result<(), String> {
        let listed = self.listed();
        for old in listed.iter().filter(|b| b.label == bot) {
            self.close_block(&old.id);
        }
        let cwd = Path::new("/");
        let block = match placement {
            Placement::Window => self.backend.window(bot, name, cwd, command, false)?,
            Placement::Split { side, beside } => {
                let anchor = listed
                    .iter()
                    .find(|b| &b.label == beside && b.label != bot)
                    .map(|b| b.id.clone())
                    .ok_or(format!("no block labelled {beside} in {}", self.home()))?;
                match side.as_str() {
                    FLOAT => (self.backend).float(&anchor, bot, cwd, command, false)?,
                    side => (self.backend).split(&anchor, side, false, bot, cwd, command, false)?,
                }
            }
        };
        self.forget();
        self.blocks.insert(bot.to_owned(), block);
        self.tidy();
        Ok(())
    }

    /// Each window and the labels of its blocks, in the multiplexer's order.
    pub fn windows(&mut self) -> Vec<(String, Vec<String>)> {
        let mut windows: Vec<(String, Vec<String>)> = Vec::new();
        for block in self.listed() {
            match windows
                .iter_mut()
                .find(|(window, _)| *window == block.window)
            {
                Some((_, labels)) => labels.push(block.label),
                None => windows.push((block.window, vec![block.label])),
            }
        }
        windows
    }

    /// Where each block sits, as last read unless the home changed since.
    /// A read that failed is nowhere, and tried again when next asked.
    pub fn places(&mut self) -> Places {
        let now = self.news.changes();
        if let Some((at, places)) = &self.placed
            && *at == now
        {
            return places.clone();
        }
        let Ok(places) = self.backend.places() else {
            return Places::new();
        };
        self.placed = Some((now, places.clone()));
        places
    }

    pub fn rename_window(&self, window: &str, name: &str) {
        self.backend.rename_window(window, name);
    }

    /// Bring the bot's block into view where the person watches.
    pub fn focus(&mut self, bot: &str) -> Result<(), String> {
        let known = self.blocks.get(bot).cloned();
        let listed = self.listed();
        let block = (listed.iter())
            .find(|b| Some(&b.id) == known.as_ref())
            .or_else(|| listed.iter().find(|b| b.label == bot))
            .ok_or_else(|| format!("{bot} has no tab in {}", self.home()))?;
        self.backend.focus(block)?;
        self.used.insert(bot.to_owned(), SystemTime::now());
        Ok(())
    }

    /// Take on the block Sonny itself runs in, when in this home, named
    /// after the bot.
    pub fn adopt_own(&mut self, bot: &str) {
        let mine = self.backend.own_block();
        let listed = self.listed();
        let own = listed
            .iter()
            .find(|b| mine.as_ref().is_some_and(|mine| &b.id == mine))
            .or_else(|| listed.iter().find(|b| b.label == bot));
        let Some(own) = own else {
            return;
        };
        let own = own.id.clone();
        if !listed.iter().any(|b| b.id == own && b.label == bot) {
            self.rename_block(&own, bot);
        }
        self.blocks.insert(bot.to_owned(), own);
    }

    /// Close every other block labelled with the bot's name, as one Sonny
    /// opened for the task while this one placed it.
    pub fn close_others(&mut self, bot: &str) {
        let Some(mine) = self.blocks.get(bot).cloned() else {
            return;
        };
        for other in self
            .listed()
            .into_iter()
            .filter(|b| b.label == bot && b.id != mine)
        {
            self.close_block(&other.id);
        }
    }

    /// Run a command an agent asked for in a block labelled `OWNER/NAME`, so
    /// the person can see whose it is and it closes with its owner's
    /// window: on `side` of the owner's block, or in a window of its own
    /// (named `name`) when there is no side or the owner has no block here.
    /// One already labelled so is replaced. The block stays open once the
    /// command exits, to be read. Returns the block's ID.
    pub fn split(
        &mut self,
        owner: &str,
        name: &str,
        cwd: &Path,
        command: &[String],
        side: Option<&str>,
    ) -> Result<String, String> {
        let label = format!("{owner}/{name}");
        let listed = self.listed();
        for old in listed.iter().filter(|b| b.label == label) {
            self.close_block(&old.id);
        }
        let anchor = listed.iter().find(|b| b.label == owner);
        let block = match anchor.zip(side) {
            Some((anchor, FLOAT)) => (self.backend).float(&anchor.id, &label, cwd, command, true),
            Some((anchor, side)) => {
                (self.backend).split(&anchor.id, side, true, &label, cwd, command, true)
            }
            None => self.backend.window(&label, name, cwd, command, true),
        }?;
        self.forget();
        self.tidy();
        Ok(block)
    }

    /// Close a task's block, and every block it asked for, as when it is
    /// done or its bot is deleted.
    pub fn close(&mut self, bot: &str) {
        let known = self.blocks.remove(bot);
        self.used.remove(bot);
        self.set_aside(bot, false);
        let owned = format!("{bot}/");
        for block in self.listed().into_iter().filter(|b| {
            b.label == bot || b.label.starts_with(&owned) || Some(&b.id) == known.as_ref()
        }) {
            self.close_block(&block.id);
        }
    }
}

/// A job for a [`Worker`]'s layout.
type Job = Box<dyn FnOnce(&mut Layout) + Send>;

enum Work {
    Job(Job),
    /// Naming the windows, which one sent after it makes needless.
    Names(Job),
    /// Reading where each block sits again.
    Places,
}

/// A [`Layout`] worked on a thread of its own, so that no multiplexer
/// call holds up Sonny's screen: jobs run in the order they are sent,
/// a naming of the windows that another follows is skipped, and where
/// each block sits is kept to be read at once. It follows the home's
/// news, so what nothing changed is not read again.
pub struct Worker {
    work: mpsc::Sender<Work>,
    home: String,
    here: bool,
    places: Arc<Mutex<Places>>,
}

impl Worker {
    pub fn new(layout: Layout) -> Self {
        let (work, queue) = mpsc::channel::<Work>();
        let places = Arc::new(Mutex::new(Places::new()));
        let kept = places.clone();
        let (home, here) = (layout.home(), layout.here());
        std::thread::spawn(move || {
            let mut layout = layout;
            layout.watch();
            while let Ok(first) = queue.recv() {
                let batch: Vec<Work> = std::iter::once(first).chain(queue.try_iter()).collect();
                layout.fresh();
                let last_names = batch.iter().rposition(|w| matches!(w, Work::Names(_)));
                let read = batch.iter().any(|w| matches!(w, Work::Places));
                for (n, work) in batch.into_iter().enumerate() {
                    match work {
                        Work::Job(job) => job(&mut layout),
                        Work::Names(job) if Some(n) == last_names => job(&mut layout),
                        Work::Names(_) | Work::Places => {}
                    }
                }
                if read {
                    let read = layout.places();
                    if let Ok(mut places) = kept.lock() {
                        *places = read;
                    }
                }
            }
        });
        Self {
            work,
            home,
            here,
            places,
        }
    }

    /// Run `job` on the layout, after those sent before it.
    pub fn send(&self, job: impl FnOnce(&mut Layout) + Send + 'static) {
        let _ = self.work.send(Work::Job(Box::new(job)));
    }

    /// Name the windows with `job`, unless another naming follows it.
    pub fn names(&self, job: impl FnOnce(&mut Layout) + Send + 'static) {
        let _ = self.work.send(Work::Names(Box::new(job)));
    }

    /// Where each block sat when last read, read again in the background.
    pub fn places(&self) -> Places {
        let _ = self.work.send(Work::Places);
        self.places.lock().map(|p| p.clone()).unwrap_or_default()
    }

    pub fn home(&self) -> &str {
        &self.home
    }

    pub fn here(&self) -> bool {
        self.here
    }

    /// Wait until every job sent so far has run.
    #[cfg(test)]
    pub fn settle(&self) {
        let (done, settled) = mpsc::channel();
        self.send(move |_| {
            let _ = done.send(());
        });
        let _ = settled.recv();
    }
}

/// The multiplexers Sonny lays work out in, by the CLI that drives each.
enum Kind {
    Rex(PathBuf),
    Herdr(PathBuf),
    Cmux(PathBuf),
}

/// The multiplexer this terminal is in: Herdr when Sonny runs in a Herdr
/// pane (even one in a cmux tab, as the innermost), cmux when in a cmux
/// tab, Rex when in a Rex block or `host` names a Rex server.
fn current(host: Option<&str>) -> Option<Kind> {
    if host.is_none() && crate::herdr::pane().is_some() {
        return crate::herdr::find().map(Kind::Herdr);
    }
    if host.is_none() && crate::cmux::surface().is_some() {
        return crate::cmux::find().map(Kind::Cmux);
    }
    let rex = std::env::var_os("REX_SESSION").is_some() || host.is_some();
    rex.then(crate::rex::find).flatten().map(Kind::Rex)
}

/// Whether Sonny runs in a multiplexer it lays work out in.
pub fn inside(host: Option<&str>) -> bool {
    current(host).is_some()
}

/// The project's home in the multiplexer Sonny runs in, from the person's
/// terminal; its window's [`Marker`] keeps it as the place its agents'
/// work goes while it is the window the person used last. None when Sonny
/// runs in none; tasks then run out of sight, in /status.
pub fn here(project: &str, host: Option<&str>) -> Option<Layout> {
    let backend: Box<dyn Backend> = match current(host)? {
        Kind::Rex(bin) => Box::new(crate::rex::Rex::open(bin, host, project, true, None).ok()?),
        Kind::Herdr(bin) => {
            let pane = crate::herdr::pane();
            Box::new(crate::herdr::Herdr::open(bin, project, pane.as_deref(), None).ok()?)
        }
        Kind::Cmux(bin) => {
            let surface = crate::cmux::surface();
            Box::new(crate::cmux::Cmux::open(bin, project, surface.as_deref(), None).ok()?)
        }
    };
    Some(Layout::new(backend))
}

/// The project's home, for an agent's call, from the tool host answering it,
/// whose multiplexer variables would name the terminal Sonny started it
/// in: where the project's Sonny window the person used last is, in that
/// multiplexer; when that window is open in a terminal in none, nowhere,
/// since another terminal is not where they look. Without a window, in
/// Herdr or cmux when the project's lead is in a Herdr pane or a cmux tab,
/// else in the multiplexer project `like`'s home is in, made when needed.
/// With none of these, the lead is in no multiplexer, and neither is
/// anything it would see.
pub fn for_agent(
    socket: &Path,
    project: &str,
    chain: &[String],
    like: Option<&str>,
) -> Result<Layout, String> {
    let kept = match latest_for(socket, project, chain) {
        Some((Window { place: None, .. }, true)) => {
            return Err(format!(
                "{project}'s Sonny was used last in a terminal outside Rex, Herdr and cmux, \
                 so nothing opened in one would be seen there"
            ));
        }
        Some((window, _)) => window.place,
        None => None,
    };
    let like = like
        .and_then(|like| latest(socket, like))
        .and_then(|(window, _)| window.place);
    let herdr = crate::herdr::find();
    let cmux = crate::cmux::find();
    let rex = crate::rex::find();
    let backend: Box<dyn Backend> = match (kept, herdr, cmux, rex) {
        (Some(Place::Herdr { workspace }), Some(herdr), _, _) => Box::new(
            crate::herdr::Herdr::open(herdr, project, None, Some(&workspace))?,
        ),
        (Some(Place::Cmux { workspace }), _, Some(cmux), _) => Box::new(crate::cmux::Cmux::open(
            cmux,
            project,
            None,
            Some(&workspace),
        )?),
        (Some(Place::Rex { session, host }), _, _, Some(rex)) => Box::new(crate::rex::Rex::open(
            rex,
            host.as_deref(),
            project,
            false,
            Some(&session),
        )?),
        (_, Some(herdr), _, _)
            if crate::herdr::has_lead(&herdr, project)
                || matches!(like, Some(Place::Herdr { .. })) =>
        {
            Box::new(crate::herdr::Herdr::open(herdr, project, None, None)?)
        }
        (_, _, Some(cmux), _)
            if crate::cmux::has_lead(&cmux, project)
                || matches!(like, Some(Place::Cmux { .. })) =>
        {
            Box::new(crate::cmux::Cmux::open(cmux, project, None, None)?)
        }
        (_, _, _, Some(rex)) if let Some(Place::Rex { host, .. }) = &like => Box::new(
            crate::rex::Rex::open(rex, host.as_deref(), project, false, None)?,
        ),
        // A home no one watches would hide what it runs: the person
        // reads this project's work in its lead's Sonny instead.
        _ => {
            return Err(format!(
                "{project}'s lead is not in Rex, Herdr or cmux, so nothing opened there would be seen"
            ));
        }
    };
    Ok(Layout::new(backend))
}

/// The skills that let any agent run long work where the person can see
/// it, for the multiplexer this terminal is in (the others' taken out, so
/// agents read one), and show the person pictures. Each names this
/// machine's multiplexer CLI.
pub fn install_skill(host: Option<&str>) {
    crate::skills::install("pictures", include_str!("../skills/pictures/SKILL.md"));
    let quoted = |path: &Path| format!("\"{}\"", path.to_string_lossy());
    let skill =
        |name, text: &str, bin: &str, path: PathBuf| (name, text.replace(bin, &quoted(&path)));
    let rex = |bin| {
        let text = include_str!("../skills/rex-windows/SKILL.md");
        skill("rex-windows", text, "REX_BIN", bin)
    };
    let (name, text) = match current(host) {
        Some(Kind::Herdr(bin)) => {
            let text = include_str!("../skills/herdr-panes/SKILL.md");
            skill("herdr-panes", text, "HERDR_BIN", bin)
        }
        Some(Kind::Cmux(bin)) => {
            let text = include_str!("../skills/cmux-tabs/SKILL.md");
            skill("cmux-tabs", text, "CMUX_BIN", bin)
        }
        Some(Kind::Rex(bin)) => rex(bin),
        // Out of every multiplexer, none is installed: nothing opened in
        // one would be seen. One installed stays, for another project's
        // Sonny still in its multiplexer.
        None => return,
    };
    crate::skills::install(name, &text);
    for other in ["rex-windows", "herdr-panes", "cmux-tabs"] {
        if other != name {
            crate::skills::remove(other);
        }
    }
}

/// A layout on a multiplexer in memory holding `blocks`, by label and
/// window, for another module's tests.
#[cfg(test)]
pub fn in_memory(blocks: &[(&str, &str)]) -> Layout {
    let fake = tests::Fake::default();
    for (label, window) in blocks {
        fake.add(label, window);
    }
    Layout::new(Box::new(fake))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::RefCell;

    /// A multiplexer in memory: its blocks, and Sonny's own.
    #[derive(Default)]
    pub(super) struct Fake {
        blocks: RefCell<Vec<Block>>,
        own: Option<String>,
        made: bool,
        /// How many times the blocks were listed.
        lists: Arc<std::sync::atomic::AtomicUsize>,
        /// While set, the blocks cannot be read.
        down: Arc<AtomicBool>,
        news: RefCell<Option<Watch>>,
        focused: RefCell<Vec<String>>,
        /// Each block a tab of its own, as cmux's tabs share a pane.
        pub(super) own_tabs: bool,
    }

    impl Fake {
        pub(super) fn add(&self, label: &str, window: &str) -> String {
            let id = format!("b{}", self.blocks.borrow().len() + 1);
            self.blocks.borrow_mut().push(Block {
                id: id.clone(),
                label: label.into(),
                window: window.into(),
            });
            id
        }
    }

    impl Backend for Fake {
        fn home(&self) -> String {
            "Fake home".into()
        }
        fn tab(&self, block: &Block) -> String {
            match self.own_tabs {
                true => block.id.clone(),
                false => block.window.clone(),
            }
        }
        fn here(&self) -> bool {
            true
        }
        fn made(&self) -> bool {
            self.made
        }
        fn blocks(&self) -> Result<Vec<Block>, String> {
            self.lists.fetch_add(1, Ordering::SeqCst);
            if self.down.load(Ordering::SeqCst) {
                return Err("down".into());
            }
            Ok((self.blocks.borrow().iter())
                .map(|b| Block {
                    id: b.id.clone(),
                    label: b.label.clone(),
                    window: b.window.clone(),
                })
                .collect())
        }
        fn close_block(&self, block: &str) {
            self.blocks.borrow_mut().retain(|b| b.id != block);
        }
        fn rename_block(&self, block: &str, label: &str) {
            for b in self
                .blocks
                .borrow_mut()
                .iter_mut()
                .filter(|b| b.id == block)
            {
                b.label = label.into();
            }
        }
        fn rename_window(&self, _: &str, _: &str) {}
        fn window(
            &self,
            label: &str,
            name: &str,
            _: &Path,
            _: &[String],
            _: bool,
        ) -> Result<String, String> {
            Ok(self.add(label, name))
        }
        fn split(
            &self,
            anchor: &str,
            _: &str,
            _: bool,
            label: &str,
            _: &Path,
            _: &[String],
            _: bool,
        ) -> Result<String, String> {
            let window = (self.blocks.borrow().iter())
                .find(|b| b.id == anchor)
                .map(|b| b.window.clone())
                .ok_or("no anchor")?;
            Ok(self.add(label, &window))
        }
        fn own_block(&self) -> Option<String> {
            self.own.clone()
        }
        fn places(&self) -> Result<Places, String> {
            Ok(Places::new())
        }
        fn place(&self) -> Place {
            Place::Herdr {
                workspace: "w1".into(),
            }
        }
        fn focus(&self, block: &Block) -> Result<(), String> {
            self.focused.borrow_mut().push(block.id.clone());
            Ok(())
        }
        fn watch(&self) -> Option<Watch> {
            self.news.borrow_mut().take()
        }
    }

    #[test]
    fn past_its_room_a_task_window_closes_the_one_used_least_recently() {
        let fake = Fake::default();
        fake.add("shop.lead", "lead");
        // Another lead's task window, never used: not this lead's to count
        // or close.
        fake.add("shop.other", "other");
        let mut layout = Layout::new(Box::new(fake));
        let at = |secs| SystemTime::UNIX_EPOCH + Duration::from_secs(secs);
        // As each window's own Sonny marked it: `b` was used last, `a`
        // before it, `c` never.
        let room = |most, keep: &[&str]| Room {
            most,
            project: "shop".into(),
            // `shop.other` is another lead's of the folder.
            mine: ["a", "b", "c", "d", "e", "f"]
                .map(|t| format!("shop.{t}"))
                .into(),
            keep: keep.iter().map(|k| k.to_string()).collect(),
            used: Arc::new(move |bot: &str| match bot {
                "shop.a" => Some(at(20)),
                "shop.b" => Some(at(30)),
                _ => None,
            }),
            held: Arc::new(|_: &str| false),
        };
        let open = |layout: &mut Layout, bot: &str, room: &Room, asked| {
            layout.show_task(bot, bot, &[], room, asked).unwrap()
        };
        let labels = |layout: &mut Layout| -> Vec<String> {
            let mut labels: Vec<String> = (layout.windows().into_iter())
                .flat_map(|(_, labels)| labels)
                .collect();
            labels.sort();
            labels
        };
        let three = room(3, &[]);
        for bot in ["shop.a", "shop.b", "shop.c"] {
            assert_eq!(open(&mut layout, bot, &three, false), Shown::Opened(vec![]));
        }
        assert_eq!(open(&mut layout, "shop.a", &three, false), Shown::Had);
        // Opening one is no use of it; one the person brought into view
        // counts from then.
        layout.focus("shop.c").unwrap();
        // The lead's own window never counts and never closes.
        assert_eq!(
            open(&mut layout, "shop.d", &three, false),
            Shown::Opened(vec!["shop.a".into()])
        );
        assert_eq!(
            labels(&mut layout),
            ["shop.b", "shop.c", "shop.d", "shop.lead", "shop.other"]
        );
        // Closed for room, it stays closed as its task works on...
        assert_eq!(open(&mut layout, "shop.a", &three, false), Shown::Aside);
        assert!(layout.aside("shop.a"));
        // ...until the person asks for it. What needs them keeps its
        // window, and so does one running a block it asked for.
        layout
            .split("shop.b", "dev", Path::new("/"), &[], Some("right"))
            .unwrap();
        assert_eq!(
            open(&mut layout, "shop.a", &room(3, &["shop.d"]), true),
            Shown::Opened(vec!["shop.c".into()])
        );
        assert!(!layout.aside("shop.a"));
        // With none to close, the room is passed rather than refused.
        let full = room(2, &["shop.a", "shop.d"]);
        assert_eq!(
            open(&mut layout, "shop.e", &full, false),
            Shown::Opened(vec![])
        );
        // A window that says its task failed, as its own Sonny reported,
        // stays though the lead never heard so itself.
        // A limit of 0 is none.
        assert_eq!(
            open(&mut layout, "shop.f", &room(0, &[]), false),
            Shown::Opened(vec![])
        );
        // A task closed as done is no longer set aside.
        layout.close("shop.c");
        assert!(!layout.aside("shop.c"));
        assert_eq!(
            open(&mut layout, "shop.c", &room(0, &[]), false),
            Shown::Opened(vec![])
        );
    }

    #[test]
    fn a_window_counts_once_with_every_task_placed_in_it_and_closes_whole() {
        let fake = Fake::default();
        fake.add("shop.lead", "lead");
        let mut layout = Layout::new(Box::new(fake));
        let mut room = Room {
            most: 2,
            project: "shop".into(),
            mine: ["shop.a", "shop.b", "shop.c", "shop.d"]
                .map(String::from)
                .into(),
            keep: HashSet::new(),
            // `b`'s window was used before `a`'s, `c` placed in it never.
            used: Arc::new(|bot: &str| match bot {
                "shop.a" => Some(SystemTime::UNIX_EPOCH + Duration::from_secs(20)),
                "shop.b" => Some(SystemTime::UNIX_EPOCH + Duration::from_secs(10)),
                _ => None,
            }),
            held: Arc::new(|_: &str| false),
        };
        for bot in ["shop.a", "shop.b"] {
            layout.show_task(bot, bot, &[], &room, false).unwrap();
        }
        // `c` shown beside `b`: two windows still, so nothing closes.
        let beside = Placement::Split {
            side: "right".into(),
            beside: "shop.b".into(),
        };
        layout.place("shop.c", "shop.c", &[], &beside).unwrap();
        // One more is over: the window used longest ago goes, both its tasks.
        assert_eq!(
            layout
                .show_task("shop.d", "shop.d", &[], &room, false)
                .unwrap(),
            Shown::Opened(vec!["shop.b".into(), "shop.c".into()])
        );
        // A window one of whose tasks failed stays whole.
        room.most = 1;
        room.held = Arc::new(|bot: &str| bot == "shop.d");
        assert_eq!(
            layout
                .show_task("shop.b", "shop.b", &[], &room, true)
                .unwrap(),
            Shown::Opened(vec!["shop.a".into()])
        );
    }

    #[test]
    fn tabs_that_share_the_lead_s_pane_each_count_as_a_window() {
        // As cmux opens a task's tab in the lead's pane.
        let fake = Fake {
            own_tabs: true,
            ..Fake::default()
        };
        for label in ["shop.lead", "shop.a", "shop.b"] {
            fake.add(label, "pane");
        }
        let mut layout = Layout::new(Box::new(fake));
        let room = Room {
            most: 2,
            project: "shop".into(),
            mine: ["shop.a", "shop.b", "shop.c"].map(String::from).into(),
            keep: HashSet::new(),
            used: Arc::new(|bot: &str| match bot {
                "shop.b" => Some(SystemTime::now()),
                _ => Some(SystemTime::UNIX_EPOCH),
            }),
            held: Arc::new(|_: &str| false),
        };
        assert_eq!(
            layout
                .show_task("shop.c", "shop.c", &[], &room, false)
                .unwrap(),
            Shown::Opened(vec!["shop.a".into()])
        );
    }

    #[test]
    fn a_lead_started_again_still_opens_a_window_it_closed_for_room() {
        let kept: Arc<Mutex<HashSet<String>>> = Arc::default();
        let aside = || {
            let (was, set) = (kept.clone(), kept.clone());
            Aside {
                was: Arc::new(move |bot: &str| was.lock().unwrap().contains(bot)),
                set: Arc::new(move |bot: &str, aside| {
                    let mut set = set.lock().unwrap();
                    match aside {
                        true => set.insert(bot.to_owned()),
                        false => set.remove(bot),
                    };
                }),
            }
        };
        let room = Room {
            most: 1,
            project: "shop".into(),
            mine: ["shop.a", "shop.b"].map(String::from).into(),
            keep: HashSet::new(),
            used: Arc::new(|_: &str| Some(SystemTime::UNIX_EPOCH)),
            held: Arc::new(|_: &str| false),
        };
        let fake = Fake::default();
        let mut layout = Layout::new(Box::new(fake));
        layout.keep_aside(aside());
        layout
            .show_task("shop.a", "shop.a", &[], &room, false)
            .unwrap();
        layout
            .show_task("shop.b", "shop.b", &[], &room, false)
            .unwrap();
        assert!(kept.lock().unwrap().contains("shop.a"));
        // Started again: it knows `a` was closed for room, not done.
        let mut again = Layout::new(Box::new(Fake::default()));
        again.keep_aside(aside());
        assert!(again.aside("shop.a"));
        assert_eq!(
            again
                .show_task("shop.a", "shop.a", &[], &room, false)
                .unwrap(),
            Shown::Aside
        );
        again
            .show_task("shop.a", "shop.a", &[], &room, true)
            .unwrap();
        assert!(!kept.lock().unwrap().contains("shop.a"));
    }

    #[test]
    fn a_window_that_says_its_task_failed_keeps_its_place() {
        let fake = Fake::default();
        fake.add("shop.lead", "lead");
        let mut layout = Layout::new(Box::new(fake));
        // As the windows' own Sonny reported: `a` failed, which this lead,
        // started again since, has not heard itself.
        let room = Room {
            most: 2,
            project: "shop".into(),
            mine: ["shop.a", "shop.b", "shop.c"].map(String::from).into(),
            keep: HashSet::new(),
            used: Arc::new(|_: &str| Some(SystemTime::UNIX_EPOCH)),
            held: Arc::new(|bot: &str| bot == "shop.a"),
        };
        for bot in ["shop.a", "shop.b"] {
            assert_eq!(
                layout.show_task(bot, bot, &[], &room, false).unwrap(),
                Shown::Opened(vec![])
            );
        }
        assert_eq!(
            layout
                .show_task("shop.c", "shop.c", &[], &room, false)
                .unwrap(),
            Shown::Opened(vec!["shop.b".into()])
        );
    }

    #[test]
    fn a_window_the_person_has_not_used_yet_is_not_closed_for_room() {
        let fake = Fake::default();
        fake.add("shop.lead", "lead");
        let mut layout = Layout::new(Box::new(fake));
        let seen: Arc<Mutex<HashSet<String>>> = Arc::default();
        let marks = seen.clone();
        let room = Room {
            most: 2,
            project: "shop".into(),
            mine: ["a", "b", "c", "d"].map(|t| format!("shop.{t}")).into(),
            keep: HashSet::new(),
            // As each window's own Sonny marks it once the person looks.
            used: Arc::new(move |bot: &str| {
                (marks.lock().unwrap().contains(bot)).then_some(SystemTime::UNIX_EPOCH)
            }),
            held: Arc::new(|_: &str| false),
        };
        // Tasks started at once: the first two fill the room, and the
        // third, with only windows nobody has looked at to close, waits
        // for the person, rather than closing new work seconds after it
        // opened.
        for bot in ["shop.a", "shop.b"] {
            assert_eq!(
                layout.show_task(bot, bot, &[], &room, false).unwrap(),
                Shown::Opened(vec![])
            );
        }
        assert_eq!(
            layout.show_task("shop.c", "c", &[], &room, false).unwrap(),
            Shown::NoRoom
        );
        assert!(layout.aside("shop.c"));
        assert_eq!(layout.windows().len(), 3);
        // Once the person has looked at one, it is theirs to close for
        // the next.
        seen.lock().unwrap().insert("shop.a".into());
        assert_eq!(
            layout.show_task("shop.d", "d", &[], &room, false).unwrap(),
            Shown::Opened(vec!["shop.a".into()])
        );
        // Asked for, the waiting one opens, past the room if it must.
        assert_eq!(
            layout.show_task("shop.c", "c", &[], &room, true).unwrap(),
            Shown::Opened(vec![])
        );
        assert!(!layout.aside("shop.c"));
        assert_eq!(layout.windows().len(), 4);
    }

    #[test]
    fn a_multiplexer_that_does_not_answer_is_given_up_on() {
        let answered =
            super::output(std::process::Command::new("sh").args(["-c", "echo hi; echo oh >&2"]))
                .unwrap();
        assert!(answered.status.success());
        assert_eq!(
            (&answered.stdout[..], &answered.stderr[..]),
            (&b"hi\n"[..], &b"oh\n"[..])
        );
        let started = std::time::Instant::now();
        let mut hung = std::process::Command::new("sleep");
        let error = super::output_within(hung.arg("30"), std::time::Duration::from_millis(200));
        assert_eq!(error.unwrap_err().kind(), std::io::ErrorKind::TimedOut);
        assert!(started.elapsed() < std::time::Duration::from_secs(5));
    }

    /// Wait until `news` has heard of `changes` changes.
    fn hear(news: &News, changes: u64) {
        let started = Instant::now();
        while news.changes() < changes {
            assert!(started.elapsed() < Duration::from_secs(5), "never heard");
            std::thread::sleep(Duration::from_millis(1));
        }
    }

    #[test]
    fn a_task_is_brought_into_view_by_its_block() {
        let fake = Fake::default();
        fake.add("shop.fix", "w1");
        let mut layout = Layout::new(Box::new(fake));
        assert_eq!(layout.focus("shop.fix"), Ok(()));
        let none = layout.focus("shop.gone");
        assert_eq!(none, Err("shop.gone has no tab in Fake home".into()));
    }

    #[test]
    fn with_no_news_the_home_is_read_once_a_batch() {
        let fake = Fake::default();
        let lists = fake.lists.clone();
        fake.add("shop.fix", "w1");
        let mut layout = Layout::new(Box::new(fake));
        layout.windows();
        layout.windows();
        assert_eq!(lists.load(Ordering::SeqCst), 1);
        layout.fresh();
        layout.windows();
        assert_eq!(lists.load(Ordering::SeqCst), 2);
        // What Sonny opens itself, it reads again.
        layout.show("shop.new", "new", &[]).unwrap();
        assert_eq!(lists.load(Ordering::SeqCst), 2);
        let windows = layout.windows();
        assert_eq!((lists.load(Ordering::SeqCst), windows.len()), (3, 2));
    }

    #[test]
    fn with_news_the_home_is_read_again_only_once_it_changed() {
        let dir = std::env::temp_dir().join(format!("sonny-news-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("news.sock");
        let listener = std::os::unix::net::UnixListener::bind(&path).unwrap();
        let fake = Fake::default();
        let lists = fake.lists.clone();
        let down = fake.down.clone();
        fake.add("shop.fix", "w1");
        *fake.news.borrow_mut() = Some(Watch {
            from: Stream::Socket(path, "{\"ask\":1}".into()),
            heard: Box::new(|line| match line["said"].as_str() {
                Some("changed") => Heard::Changed,
                Some("over") => Heard::Over,
                _ => Heard::Same,
            }),
        });
        let mut layout = Layout::new(Box::new(fake));
        let news = layout.news.clone();
        layout.watch();
        let (socket, _) = listener.accept().unwrap();
        let mut asked = String::new();
        BufReader::new(socket.try_clone().unwrap())
            .read_line(&mut asked)
            .unwrap();
        assert_eq!(asked, "{\"ask\":1}\n");
        let say = |said: &str| writeln!(&socket, "{{\"said\":\"{said}\"}}").unwrap();
        // Up: read again once, then not, batch after batch.
        say("started");
        hear(&news, 1);
        layout.windows();
        layout.fresh();
        layout.places();
        layout.windows();
        assert_eq!(lists.load(Ordering::SeqCst), 1);
        // What does not move a block changes nothing.
        say("focus");
        say("changed");
        hear(&news, 2);
        layout.windows();
        assert_eq!(lists.load(Ordering::SeqCst), 2);
        // A read that failed is not kept: it is tried again until it is
        // read.
        say("changed");
        hear(&news, 3);
        down.store(true, Ordering::SeqCst);
        assert!(layout.windows().is_empty());
        layout.windows();
        down.store(false, Ordering::SeqCst);
        assert_eq!(layout.windows().len(), 1);
        layout.windows();
        assert_eq!(lists.load(Ordering::SeqCst), 5);
        // Once the news ends, each batch reads the home again.
        say("over");
        hear(&news, 4);
        assert!(!news.up.load(Ordering::SeqCst));
        layout.windows();
        layout.fresh();
        layout.windows();
        assert_eq!(lists.load(Ordering::SeqCst), 7);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_worker_runs_jobs_in_order_and_skips_a_naming_that_another_follows() {
        let worker = Worker::new(Layout::new(Box::new(Fake::default())));
        assert_eq!((worker.home(), worker.here()), ("Fake home", true));
        let named = Arc::new(Mutex::new(Vec::new()));
        // Held until all are sent, so they arrive as one batch.
        let (go, wait) = mpsc::channel::<()>();
        worker.send(move |_| {
            let _ = wait.recv();
        });
        let sonny = ["sonny".to_owned()];
        worker.send(move |layout| {
            layout.show("shop.fix", "fix", &sonny).unwrap();
        });
        for n in 0..3 {
            let named = named.clone();
            worker.names(move |layout| {
                let windows = layout.windows();
                named.lock().unwrap().push((n, windows.len()));
            });
        }
        go.send(()).unwrap();
        worker.settle();
        // Only the last naming ran, after the task's window was opened.
        assert_eq!(*named.lock().unwrap(), [(2, 1)]);
    }

    #[test]
    fn a_project_keeps_each_place_it_was_laid_out_in() {
        let file = std::env::temp_dir().join(format!("sonny-place-{}.json", std::process::id()));
        let _ = std::fs::remove_file(&file);
        let places = [
            Place::Herdr {
                workspace: "wD".into(),
            },
            Place::Cmux {
                workspace: "8F2C".into(),
            },
            Place::Rex {
                session: "s7".into(),
                host: Some("mini".into()),
            },
        ];
        for (at, place) in places.iter().enumerate() {
            let me = Window {
                pid: 10 + at as u32,
                bot: "shop.lead".into(),
                used_ms: 100 + at as u64,
                place: Some(place.clone()),
            };
            mark_in(&file, me.clone(), |_| true);
            assert_eq!(latest_in(&file, |_| true), Some((me, true)));
        }
        assert_eq!(windows(&file).len(), 3);
        for place in [Some(places[2].clone()), None] {
            assert_eq!(Place::decode(&Place::encode(&place)), Some(place));
        }
        std::fs::write(
            &file,
            "{\"windows\": [{\"pid\": 1, \"used_ms\": 1, \"place\": {\"multiplexer\": \"tmux\"}}]}",
        )
        .unwrap();
        assert_eq!(windows(&file)[0].place, None);
        let _ = std::fs::remove_file(&file);
        let socket = Path::new("/tmp/agent.sock");
        assert_ne!(Place::file(socket, "shop"), Place::file(socket, "shop.web"));
    }

    /// Ghostty's Sonny typed in after Rex's: an agent's window opens in
    /// neither, rather than in the terminal the person is not in.
    #[test]
    fn the_window_used_last_says_where_agents_open() {
        let file = std::env::temp_dir().join(format!("sonny-used-{}.json", std::process::id()));
        let _ = std::fs::remove_file(&file);
        let rex = Window {
            pid: 1,
            bot: "shop.lead".into(),
            used_ms: 100,
            place: Some(Place::Rex {
                session: "s1".into(),
                host: None,
            }),
        };
        let ghostty = Window {
            pid: 2,
            bot: "shop.lead".into(),
            used_ms: 200,
            place: None,
        };
        mark_in(&file, rex.clone(), |_| true);
        mark_in(&file, ghostty.clone(), |_| true);
        assert_eq!(latest_in(&file, |_| true), Some((ghostty.clone(), true)));
        // Rex's typed in again: it is the one.
        let again = Window {
            used_ms: 300,
            ..rex.clone()
        };
        mark_in(&file, again.clone(), |_| true);
        assert_eq!(latest_in(&file, |_| true), Some((again.clone(), true)));
        // Rex's closed: Ghostty's is the newest still open.
        assert_eq!(latest_in(&file, |pid| pid != 1), Some((ghostty, true)));
        // Every window closed: the last used still names its place.
        assert_eq!(latest_in(&file, |_| false), Some((again.clone(), false)));
        // A new window drops every closed one but the newest.
        let third = Window {
            pid: 3,
            bot: "shop.lead".into(),
            used_ms: 400,
            place: None,
        };
        mark_in(&file, third.clone(), |pid| pid == 3);
        assert_eq!(windows(&file), [third, again]);
        let _ = std::fs::remove_file(&file);
    }

    /// Ghostty's Sonny talks to the lead and Rex's to another lead of the
    /// project at once: each agent acts in the terminal it is spoken to
    /// in, its tasks in their maker's, and an agent no window shows in the
    /// project's window used last.
    #[test]
    fn each_agent_acts_in_the_terminal_it_is_spoken_to_in() {
        let file = std::env::temp_dir().join(format!("sonny-each-{}.json", std::process::id()));
        let _ = std::fs::remove_file(&file);
        let home = Place::Rex {
            session: "s1".into(),
            host: None,
        };
        let rex = Window {
            pid: 1,
            bot: "shop.lead.web".into(),
            used_ms: 100,
            place: Some(home.clone()),
        };
        let ghostty = Window {
            pid: 2,
            bot: "shop.lead".into(),
            used_ms: 200,
            place: None,
        };
        mark_in(&file, rex.clone(), |_| true);
        mark_in(&file, ghostty.clone(), |_| true);
        let chain = |bots: &[&str]| bots.iter().map(|bot| bot.to_string()).collect::<Vec<_>>();
        let open = |pid| pid == 1 || pid == 2;
        assert_eq!(
            latest_for_in(&file, &chain(&["shop.lead"]), open),
            Some((ghostty.clone(), true))
        );
        assert_eq!(
            latest_for_in(&file, &chain(&["shop.lead.web"]), open),
            Some((rex.clone(), true))
        );
        // A task of the Rex lead's, shown nowhere, acts in its maker's.
        let task = chain(&["shop.fix", "shop.lead.web"]);
        assert_eq!(latest_for_in(&file, &task, open), Some((rex.clone(), true)));
        // An agent no window shows, made by none that is: the project's
        // window used last.
        assert_eq!(
            latest_for_in(&file, &chain(&["shop.cron"]), open),
            Some((ghostty, true))
        );
        // The Rex lead's window opens its tasks' tabs though Ghostty's was
        // used since; the Ghostty lead's work opens none in Rex.
        assert!(mine_in(&file, 1, "shop.lead.web", &home, open));
        let other = Place::Herdr {
            workspace: "w2".into(),
        };
        assert!(!mine_in(&file, 3, "shop.lead", &other, open));
        // The Rex lead's window closed: its tasks go by the project's.
        assert_eq!(
            latest_for_in(&file, &task, |pid| pid == 2).map(|(w, _)| w.pid),
            Some(2)
        );
        let _ = std::fs::remove_file(&file);
    }

    /// A task's window, opened in the lead's home, takes that home as its
    /// own when it starts and keeps it: used, it leaves the lead's new
    /// tasks opening there, and never moves to a home typed in since.
    #[test]
    fn an_agents_window_keeps_the_home_it_opened_in() {
        let file = std::env::temp_dir().join(format!("sonny-agent-{}.json", std::process::id()));
        let _ = std::fs::remove_file(&file);
        let me = std::process::id();
        let gone = {
            let mut child = Command::new("true").spawn().unwrap();
            let pid = child.id();
            child.wait().unwrap();
            pid
        };
        let home = Place::Rex {
            session: "s1".into(),
            host: None,
        };
        let other = Place::Herdr {
            workspace: "w2".into(),
        };
        // The lead's window in Rex, open; this test's process stands in
        // for the task's window.
        let lead = Window {
            pid: gone,
            bot: "shop.lead".into(),
            used_ms: now_ms() - 10,
            place: Some(home.clone()),
        };
        mark_in(&file, lead, |_| true);
        let mut task = Marker {
            file: file.clone(),
            bot: "shop.fix".into(),
            place: None,
            follows: true,
            last: None,
        };
        // Read while the lead's window counts as open.
        let opened_in = windows(&file)[0].place.clone();
        task.place = opened_in;
        task.follows = false;
        task.used();
        assert_eq!(windows(&file)[0].place, Some(home.clone()));
        // The task's window used last, in the lead's home: the lead still
        // opens its tasks there; a lead elsewhere does not.
        assert!(mine_in(&file, gone, "shop.fix", &home, |pid| pid == me));
        assert!(!mine_in(&file, 2, "shop.fix", &other, |pid| pid == me));
        // A window in another home used since: the task keeps its own.
        mark_in(
            &file,
            Window {
                pid: 1,
                bot: "shop.lead".into(),
                used_ms: now_ms() + 1000,
                place: Some(other.clone()),
            },
            |_| true,
        );
        task.last = None;
        task.used();
        let own = windows(&file).into_iter().find(|window| window.pid == me);
        assert_eq!(own.and_then(|window| window.place), Some(home));
        let _ = std::fs::remove_file(&file);
    }

    /// `/switch` to another project keeps the process: the window it was
    /// is closed for its old project, its place kept.
    #[test]
    fn a_window_switched_away_counts_as_closed() {
        let file = std::env::temp_dir().join(format!("sonny-retire-{}.json", std::process::id()));
        let _ = std::fs::remove_file(&file);
        let me = std::process::id();
        let bare = Window {
            pid: me,
            bot: "shop.lead".into(),
            used_ms: 5,
            place: None,
        };
        mark_in(&file, bare, |_| true);
        assert_eq!(latest_in(&file, alive).map(|(_, open)| open), Some(true));
        retire_in(&file, me);
        let (window, open) = latest_in(&file, alive).unwrap();
        assert_eq!((window.pid, window.place, open), (0, None, false));
        let _ = std::fs::remove_file(&file);
    }

    /// `/switch` from a lead's window to one of its tasks: the task's
    /// window is in the same terminal, so it takes the lead's place even
    /// with no open window left to read it from, and hands it on in turn.
    #[test]
    fn a_window_switched_to_takes_the_place_it_replaced() {
        let file = std::env::temp_dir().join(format!("sonny-given-{}.json", std::process::id()));
        let _ = std::fs::remove_file(&file);
        let home = Place::Cmux {
            workspace: "9A".into(),
        };
        let mut task = Marker {
            file: file.clone(),
            bot: "shop.fix".into(),
            follows: false,
            place: Some(home.clone()),
            last: None,
        };
        assert_eq!(task.known(), Some(Some(home.clone())));
        task.used_now();
        assert_eq!(windows(&file)[0].place, Some(home));
        // One still to learn its home hands nothing on.
        task.follows = true;
        assert_eq!(task.known(), None);
        let _ = std::fs::remove_file(&file);
    }

    #[test]
    fn a_process_that_runs_is_alive_and_one_gone_is_not() {
        assert!(alive(std::process::id()));
        let mut child = Command::new("true").spawn().unwrap();
        let pid = child.id();
        child.wait().unwrap();
        assert!(!alive(pid));
    }

    fn labels(layout: &mut Layout) -> Vec<(String, Vec<String>)> {
        layout.windows()
    }

    #[test]
    fn a_task_has_one_block_wherever_it_is_put_and_its_commands_close_with_it() {
        let fake = Fake::default();
        let own = fake.add("zsh", "home");
        let fake = Fake {
            own: Some(own),
            ..fake
        };
        let mut layout = Layout::new(Box::new(fake));
        layout.adopt_own("shop.lead");
        let sonny = ["sonny".to_owned()];
        assert!(layout.show("shop.fix", "fix", &sonny).unwrap());
        // Shown again, it is already there.
        assert!(!layout.show("shop.fix", "fix", &sonny).unwrap());
        let split = Placement::Split {
            side: "right".into(),
            beside: "shop.lead".into(),
        };
        layout.place("shop.fix", "fix", &sonny, &split).unwrap();
        layout
            .split("shop.fix", "tests", Path::new("/w"), &sonny, Some("below"))
            .unwrap();
        layout
            .split("shop.lead", "server", Path::new("/w"), &sonny, None)
            .unwrap();
        assert_eq!(
            labels(&mut layout),
            [
                (
                    "home".into(),
                    vec![
                        "shop.lead".into(),
                        "shop.fix".into(),
                        "shop.fix/tests".into()
                    ]
                ),
                ("server".into(), vec!["shop.lead/server".into()]),
            ]
        );
        layout.close("shop.fix");
        assert_eq!(
            labels(&mut layout),
            [
                ("home".into(), vec!["shop.lead".into()]),
                ("server".into(), vec!["shop.lead/server".into()]),
            ]
        );
    }

    #[test]
    fn a_home_made_for_a_lead_holds_only_its_sonny() {
        let fake = Fake {
            made: true,
            ..Fake::default()
        };
        fake.add("", "shell");
        let mut layout = Layout::new(Box::new(fake));
        let sonny = ["sonny".to_owned()];
        assert!(layout.show("web.lead", "lead", &sonny).unwrap());
        assert_eq!(
            labels(&mut layout),
            [("lead".into(), vec!["web.lead".into()])]
        );
        // The lead's Sonny is there already: left alone.
        assert!(!layout.show("web.lead", "lead", &sonny).unwrap());
    }

    #[test]
    fn a_home_sonny_was_opened_in_keeps_what_was_there() {
        let fake = Fake::default();
        fake.add("editor", "work");
        let own = fake.add("zsh", "work");
        let fake = Fake {
            own: Some(own),
            ..fake
        };
        let mut layout = Layout::new(Box::new(fake));
        layout.adopt_own("shop.lead");
        let sonny = ["sonny".to_owned()];
        layout.show("shop.fix", "fix", &sonny).unwrap();
        layout
            .split(
                "shop.lead",
                "server",
                Path::new("/w"),
                &sonny,
                Some("right"),
            )
            .unwrap();
        layout.close("shop.fix");
        assert_eq!(
            labels(&mut layout),
            [(
                "work".into(),
                vec![
                    "editor".into(),
                    "shop.lead".into(),
                    "shop.lead/server".into()
                ]
            )]
        );
    }
}
