//! Sonny, a terminal client for one bot, beside the app: its conversation goes
//! into the terminal's own scrollback as it happens, and a few rows at the
//! bottom hold what is still moving: the reply being written, what the bot
//! is doing, and the composer. Prototype.
//!
//! The look follows Codex's TUI (openai/codex, codex-rs/tui): finished
//! history is written above an inline viewport rather than kept in an
//! alternate screen, replies stream a line at a time, tool calls are one
//! `• Ran …` line with a short `└` look at the result, and a status row
//! says how long the turn has run and how to stop it.
mod calls;
mod diff;
mod project;
mod rex;
mod shell;
mod sound;
mod status;
mod transcript;

use agent_client::{Client, Events};
use ratatui::{
    Terminal, TerminalOptions, Viewport,
    backend::CrosstermBackend,
    crossterm::{
        event::{
            self, DisableBracketedPaste, EnableBracketedPaste, Event, KeyCode, KeyEventKind,
            KeyModifiers,
        },
        execute,
        terminal::{disable_raw_mode, enable_raw_mode},
    },
    layout::{Constraint, Layout},
    style::{Color, Modifier, Style, Stylize},
    text::{Line, Span},
    widgets::{Paragraph, Widget},
};
use serde_json::{Value, json};
use std::{
    collections::HashMap,
    io::Write,
    path::PathBuf,
    sync::Arc,
    time::{Duration, Instant},
};
use transcript::Markdown;
use unicode_width::UnicodeWidthStr;

const DEFAULT_TOOLS: [&str; 6] = ["shell", "read", "write", "edit", "wait", "history"];
/// Rows kept at the bottom: the reply's unfinished line, the status, a gap,
/// the composer and the footer.
const VIEWPORT: u16 = 5;
/// How much history is shown on attach.
const HISTORY_ITEMS: u64 = 200;

type Tui = Terminal<CrosstermBackend<std::io::Stdout>>;

struct Options {
    socket: PathBuf,
    bot: Option<String>,
    new: bool,
    model: Option<String>,
    workspace: Option<String>,
    /// Lay the project's tasks out in Rex (on by default when rex is found).
    rex: bool,
    /// The Rex server the project's session lives on, when not this one.
    rex_host: Option<String>,
    /// Ring when work ends or needs the person.
    sound: bool,
}

fn usage() -> String {
    "usage: sonny [--store PATH] [--socket PATH] [--model PROVIDER/MODEL] [--no-rex] [--rex-host HOST] [--no-sound]\n\
     \x20      [--bot NAME [--new [--workspace DIR]]]\n\
     With no --bot, the folder's coordinator, made with --model the first time; its tasks\n\
     open in the project's Rex session. Type /status for the project's agents."
        .into()
}

/// `sonny show TASK [--split SIDE [--beside BOT]] [--rex-host HOST]`: put a
/// task's window where the person asked, from the lead's shell. A task gets
/// a window of its own unless told otherwise; `--split` puts it beside the
/// lead's block, or `--beside` another bot's.
fn show() -> Result<(), String> {
    let usage =
        "usage: sonny show TASK [--split right|left|above|below [--beside BOT]] [--rex-host HOST]";
    let mut args = std::env::args().skip(2);
    let bot = args.next().filter(|b| !b.starts_with('-')).ok_or(usage)?;
    let caller = std::env::var("AGENT_BOT").ok();
    let (mut side, mut beside, mut host) = (None, caller.clone(), None);
    while let Some(arg) = args.next() {
        let mut value = || args.next().ok_or(usage);
        match arg.as_str() {
            "--split" => side = Some(value()?),
            "--beside" => beside = Some(value()?),
            "--window" => side = None,
            "--rex-host" => host = Some(value()?),
            _ => return Err(usage.into()),
        }
    }
    let socket = std::env::var_os("AGENT_SOCKET")
        .map(PathBuf::from)
        .ok_or("run this from an agent's shell (AGENT_SOCKET is not set)")?;
    let socket = std::path::absolute(socket).map_err(|e| e.to_string())?;
    // The project is what the names start with: `calc` for `calc.lead`.
    let project = caller
        .as_deref()
        .unwrap_or(&bot)
        .split('.')
        .next()
        .unwrap_or(&bot)
        .to_owned();
    let bin = rex::find().ok_or("rex not found")?;
    let mut rex = rex::Rex::open(bin, host.as_deref(), &project)?;
    let me = std::env::current_exe().map_err(|e| e.to_string())?;
    let command = [
        me.to_string_lossy().into_owned(),
        "--socket".into(),
        socket.to_string_lossy().into_owned(),
        "--no-sound".into(),
        "--bot".into(),
        bot.clone(),
    ];
    let short = bot.strip_prefix(&format!("{project}.")).unwrap_or(&bot);
    let placement = match side {
        Some(side) => rex::Placement::Split {
            side,
            beside: beside.ok_or("--split needs --beside BOT outside an agent")?,
        },
        None => rex::Placement::Window,
    };
    rex.place(&bot, short, &command, &placement)?;
    // The lead's Sonny opens a window for a task whose turn starts, maybe
    // while this placed it: what it opened then goes.
    std::thread::sleep(Duration::from_millis(1500));
    rex.close_others(&bot);
    println!("{}", json!({"bot": bot, "session": project}));
    Ok(())
}

fn parse() -> Result<Options, String> {
    let mut args = std::env::args().skip(1);
    let (mut store, mut socket, mut bot, mut new, mut model, mut workspace) =
        (None, None, None, false, None, None);
    let (mut rex, mut rex_host, mut sound) = (true, None, true);
    while let Some(arg) = args.next() {
        let mut value = || args.next().ok_or_else(usage);
        match arg.as_str() {
            "--store" => store = Some(PathBuf::from(value()?)),
            "--socket" => socket = Some(PathBuf::from(value()?)),
            "--bot" => bot = Some(value()?),
            "--new" => new = true,
            "--model" => model = Some(value()?),
            "--workspace" => workspace = Some(value()?),
            "--no-rex" => rex = false,
            "--rex-host" => rex_host = Some(value()?),
            "--no-sound" => sound = false,
            _ => return Err(usage()),
        }
    }
    let store = match store.or_else(|| std::env::var_os("AGENT_STORE").map(PathBuf::from)) {
        Some(store) => store,
        None => PathBuf::from(std::env::var_os("HOME").ok_or("set --store or HOME")?)
            .join(".agent/state.sqlite"),
    };
    let socket = match socket.or_else(|| std::env::var_os("AGENT_SOCKET").map(PathBuf::from)) {
        Some(socket) => socket,
        None => agent_client::socket::default_socket(&store).map_err(|e| e.to_string())?,
    };
    Ok(Options {
        socket,
        bot,
        new,
        model,
        workspace,
        rex,
        rex_host,
        sound,
    })
}

#[tokio::main(flavor = "current_thread")]
async fn main() -> std::process::ExitCode {
    match run().await {
        Ok(()) => std::process::ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("sonny: {error}");
            std::process::ExitCode::FAILURE
        }
    }
}

async fn run() -> Result<(), String> {
    if std::env::args().nth(1).as_deref() == Some("show") {
        return show();
    }
    let options = parse()?;
    let (client, mut events) = Client::connect(&options.socket)
        .await
        .map_err(|e| format!("{e}; start the daemon with `agent start`"))?;
    let (name, project) = match &options.bot {
        Some(bot) => {
            if options.new {
                create(&client, &options, bot).await?;
            }
            (bot.clone(), None)
        }
        None => {
            if options.new {
                return Err(usage());
            }
            let dir = std::env::current_dir().map_err(|e| e.to_string())?;
            // Before the lead is made, so its instructions list the skill.
            if options.rex
                && let Some(bin) = rex::find()
            {
                rex::install_skill(&bin);
            }
            let lead =
                project::open(&client, &dir, &options.socket, options.model.as_deref()).await?;
            (lead.bot, Some(lead.project))
        }
    };
    let bot = client
        .request("resume", json!({"bot": name}))
        .await
        .map_err(|e| e.to_string())?;
    // Everything after now arrives live; history is read from the bot's
    // items instead of replayed events, so the replay is skipped.
    client
        .request("follow", json!({"bot": name, "after": 0}))
        .await
        .map_err(|e| e.to_string())?;
    caught_up(&mut events).await;

    enable_raw_mode().map_err(|e| e.to_string())?;
    let _ = execute!(std::io::stdout(), EnableBracketedPaste);
    let mut terminal = Terminal::with_options(
        CrosstermBackend::new(std::io::stdout()),
        TerminalOptions {
            viewport: Viewport::Inline(VIEWPORT),
        },
    )
    .map_err(|e| e.to_string())?;
    let mut app = App::new(client, &name, &bot);
    app.sounds = sound::Sounds::new(options.sound);
    // A bot keeps the model it was made with; --model runs this window's
    // turns on another.
    if let Some(model) = options.model.as_ref().filter(|m| **m != app.model) {
        app.model = model.clone();
        app.turn_model = Some(model.clone());
    }
    if let Some(project) = &project
        && options.rex
    {
        app.fleet = Fleet::start(&options, project, &name).await;
        if let Some(fleet) = &app.fleet {
            app.notice = fleet.note.clone();
        }
        // Skills are listed in a bot's instructions when it is made.
        if bot["instructions"]
            .as_str()
            .is_some_and(|text| !text.contains("rex-windows"))
        {
            app.notice = Some("older lead: ask it to read the rex-windows skill".into());
        }
    }
    app.project = project;
    let result = app.main(&mut terminal, &mut events).await;
    let _ = terminal.clear();
    let _ = execute!(std::io::stdout(), DisableBracketedPaste);
    let _ = disable_raw_mode();
    status_report("clear", &name);
    result
}

/// A new bot in the folder, with the instructions the app would give it.
async fn create(client: &Client, options: &Options, bot: &str) -> Result<(), String> {
    let model = options
        .model
        .clone()
        .or_else(|| std::env::var("AGENT_MODEL").ok())
        .ok_or("--new needs --model PROVIDER/MODEL")?;
    let workspace = match &options.workspace {
        Some(dir) => std::path::absolute(dir).map_err(|e| e.to_string())?,
        None => std::env::current_dir().map_err(|e| e.to_string())?,
    };
    let memory = agent_client::policy::memory_indexes(&workspace).map_err(|e| e.to_string())?;
    let instructions = agent_client::policy::instructions(&workspace, None, &memory)
        .map_err(|e| e.to_string())?
        .text;
    client
        .request(
            "create",
            json!({"bot": bot, "workspace": workspace, "model": model,
                "instructions": instructions, "tools": DEFAULT_TOOLS,
                "request_id": unique("tui-create")}),
        )
        .await
        .map_err(|e| e.to_string())?;
    Ok(())
}

fn unique(prefix: &str) -> String {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default();
    format!("{prefix}-{}-{}", now.as_millis(), std::process::id())
}

/// Program status (OSC 7501), so a terminal or multiplexer that reads it
/// (Ghostty, Rex) can mark this pane working, blocked or done.
fn status_report(state: &str, bot: &str) {
    use base64::Engine;
    let mut body = format!("state={state}");
    if state == "blocked" {
        body.push_str(":kind=permission");
    }
    if state != "clear" {
        let title = base64::engine::general_purpose::STANDARD.encode(bot);
        body.push_str(&format!(":app=agent:title={title}"));
    }
    let mut stdout = std::io::stdout();
    let _ = write!(stdout, "\x1b]7501;{body}\x1b\\");
    // The terminal title, which Rex shows on the block: the bot's name in
    // its project, and what it is doing.
    let name = bot.split_once('.').map_or(bot, |(_, name)| name);
    let doing = match state {
        "blocked" => "needs you",
        "error" => "failed",
        "clear" => "",
        other => other,
    };
    let title = if doing.is_empty() {
        String::new()
    } else {
        format!("{name} · {doing}")
    };
    let _ = write!(stdout, "\x1b]2;{title}\x07");
    let _ = stdout.flush();
}

/// Read past a follow's replay as it arrives, so a long one cannot fill
/// the client's queue and cut the stream before it goes live.
async fn caught_up(events: &mut Events) {
    let replay = async {
        while let Some(event) = events.recv().await {
            if event["event"] == "follow_live" {
                return;
            }
        }
    };
    let _ = tokio::time::timeout(Duration::from_secs(30), replay).await;
}

/// The reply being written: lines are committed as they end, and the line
/// still open stays in the viewport.
struct Live {
    markdown: Markdown,
    partial: String,
    lines: usize,
}

struct Running {
    turn: i64,
    started: Instant,
    /// What the status row says the bot is doing.
    phase: String,
}

struct App {
    client: Arc<Client>,
    bot: String,
    /// The project whose lead this is, when opened as the folder's coordinator.
    project: Option<String>,
    workspace: String,
    model: String,
    effort: Option<String>,
    tokens_used: u64,
    input: String,
    /// Lines waiting to be written into scrollback.
    pending: Vec<Line<'static>>,
    any_history: bool,
    width: usize,
    live: Option<Live>,
    /// Whether the reply now ending arrived as deltas.
    streamed: bool,
    running: Option<Running>,
    usage: (u64, u64, u64),
    calls: HashMap<String, (String, Value)>,
    /// Calls waiting on the person, as `approvals` lists them.
    approvals: Vec<Value>,
    following: bool,
    notice: Option<String>,
    quit: bool,
    started: Instant,
    fleet: Option<Fleet>,
    /// Layout notes waiting for the lead's running calls to be shown.
    held: Vec<Line<'static>>,
    /// The newest history item drawn on attach.
    seen_node: i64,
    /// The model this window's turns run on, when not the bot's own.
    turn_model: Option<String>,
    sounds: sound::Sounds,
    /// The reply text the current model call has streamed.
    said: String,
    /// What a retried call will stream again, not to be shown twice.
    replay: Option<String>,
}

/// The lead's tasks, laid out in the project's Rex session: a window for
/// each task while it works, opened as its turn starts. It follows every
/// bot on its own connection, so the lead's own view is not disturbed.
struct Fleet {
    rex: rex::Rex,
    _client: Arc<Client>,
    events: Events,
    live: bool,
    project: String,
    lead: String,
    /// The lead and every bot it, or one of them, made.
    lineage: std::collections::HashSet<String>,
    /// How a task's window runs Sonny on it, without the bot's name.
    command: Vec<String>,
    /// Said once in the status row when Rex is set up.
    note: Option<String>,
    /// What each bot is doing, as its window says.
    doing: HashMap<String, &'static str>,
    /// The name each window was given.
    named: HashMap<String, String>,
    /// What to ring for, taken by the app after each event.
    alerts: Vec<(sound::Alert, String)>,
}

impl Fleet {
    async fn start(options: &Options, project: &str, lead: &str) -> Option<Self> {
        let bin = rex::find()?;
        let rex = rex::Rex::open(bin.clone(), options.rex_host.as_deref(), project).ok()?;
        let (client, mut events) = Client::connect(&options.socket).await.ok()?;
        client
            .request("follow", json!({"bot": "*", "after": 0}))
            .await
            .ok()?;
        caught_up(&mut events).await;
        let me = std::env::current_exe().ok()?;
        // Rex starts a block without this process's environment.
        // The lead's Sonny rings for the whole project; a task's is quiet.
        let command = vec![
            me.to_string_lossy().into_owned(),
            "--socket".into(),
            options.socket.to_string_lossy().into_owned(),
            "--no-sound".into(),
        ];
        let mut note = format!("tasks open in Rex session {project}");
        if rex::install_skill(&bin).is_some() {
            note.push_str(" · rex-windows skill updated");
        }
        let mut fleet = Self {
            rex,
            _client: client.clone(),
            events,
            live: true,
            project: project.to_owned(),
            lead: lead.to_owned(),
            lineage: [lead.to_owned()].into(),
            command,
            note: Some(note),
            doing: HashMap::new(),
            named: HashMap::new(),
            alerts: Vec::new(),
        };
        fleet.rex.adopt_own(lead);
        fleet.doing(lead, "idle");
        // Tasks named in the project's way, and the ones already working.
        if let Ok(page) = client
            .request(
                "bots",
                json!({"name": format!("{project}.*"), "limit": 256}),
            )
            .await
        {
            for bot in page["bots"].as_array().into_iter().flatten() {
                let Some(name) = bot["name"].as_str() else {
                    continue;
                };
                fleet.lineage.insert(name.to_owned());
                let running = !bot["running_turn"].is_null();
                if name != lead {
                    fleet
                        .doing
                        .insert(name.to_owned(), if running { "working" } else { "idle" });
                }
                if name != lead && running {
                    let _ = fleet.show(name);
                }
            }
        }
        fleet.name_windows();
        Some(fleet)
    }

    fn show(&mut self, bot: &str) -> Result<bool, String> {
        let mut command = self.command.clone();
        command.extend(["--bot".into(), bot.into()]);
        let name = format!("{} · working", self.short(bot));
        let opened = self.rex.show(bot, &name, &command)?;
        self.doing(bot, "working");
        Ok(opened)
    }

    /// A bot's name in its project's session: `lead`, or the task's name.
    fn short<'a>(&self, bot: &'a str) -> &'a str {
        if bot == self.lead {
            return "lead";
        }
        bot.strip_prefix(&format!("{}.", self.project))
            .unwrap_or(bot)
    }

    /// Note what the bot is doing, and name the windows after it.
    fn doing(&mut self, bot: &str, doing: &'static str) {
        if self.doing.get(bot) != Some(&doing) {
            self.doing.insert(bot.to_owned(), doing);
            self.name_windows();
            let short = self.short(bot).to_owned();
            let alert = match doing {
                "done" if bot == self.lead => {
                    Some((sound::Alert::LeadDone, format!("{short} is done")))
                }
                "done" => Some((sound::Alert::TaskDone, format!("{short} is done"))),
                "failed" => Some((sound::Alert::Failed, format!("{short} failed"))),
                "needs you" => Some((sound::Alert::NeedsYou, format!("{short} needs you"))),
                _ => None,
            };
            self.alerts.extend(alert);
        }
    }

    /// Each window holding the project's bots is named after the lead, or
    /// its first bot, and what that bot is doing, with a count of the others
    /// that are working or need the person: `lead · done · 2 working`.
    fn name_windows(&mut self) {
        for (window, labels) in self.rex.windows() {
            let bots: Vec<&String> = labels
                .iter()
                .filter(|l| self.doing.contains_key(*l))
                .collect();
            let Some(first) = bots
                .iter()
                .find(|b| **b == &self.lead)
                .or_else(|| bots.first())
                .copied()
            else {
                continue;
            };
            let mut name = format!("{} · {}", self.short(first), self.doing[first]);
            for state in ["needs you", "working"] {
                let others = bots
                    .iter()
                    .filter(|b| **b != first && self.doing.get(**b) == Some(&state))
                    .count();
                if others > 0 {
                    name.push_str(&format!(" · {others} {state}"));
                }
            }
            if self.named.get(&window) != Some(&name) {
                self.rex.rename_window(&window, &name);
                self.named.insert(window, name);
            }
        }
    }

    /// What the event did to the layout, as a line for the lead's view.
    fn event(&mut self, event: &Value) -> Option<String> {
        let kind = event["event"].as_str().unwrap_or("");
        if !self.live {
            self.live = kind == "follow_live";
            return None;
        }
        let bot = event["bot"].as_str()?;
        let data = &event["data"];
        if self.lineage.contains(bot) {
            match kind {
                "accepted" if bot == self.lead => self.doing(bot, "working"),
                "approval_requested" => {
                    let manual = data["calls"].as_array().into_iter().flatten().any(|call| {
                        call["gates"]
                            .as_array()
                            .is_some_and(|gates| gates.iter().any(|g| g == "manual"))
                    });
                    if manual {
                        self.doing(bot, "needs you");
                    }
                }
                "tool_completed" | "turn_resumed" if self.doing.get(bot) == Some(&"needs you") => {
                    self.doing(bot, "working")
                }
                "turn_finished" => self.doing(
                    bot,
                    match data["status"].as_str() {
                        Some("completed" | "steered") => "done",
                        Some("interrupted") => "stopped",
                        _ => "failed",
                    },
                ),
                _ => {}
            }
        }
        match kind {
            "created" | "forked" => {
                let by = event["data"]["created_by"].as_str().unwrap_or("");
                if self.lineage.contains(by) || bot.starts_with(&format!("{}.", self.project)) {
                    self.lineage.insert(bot.to_owned());
                }
                None
            }
            "accepted" if bot != self.lead && self.lineage.contains(bot) => match self.show(bot) {
                Ok(true) => Some(format!("{bot} is in Rex ({})", self.rex.label)),
                Ok(false) => None,
                Err(error) => Some(error),
            },
            "deleted" => {
                self.rex.close(bot);
                self.lineage.remove(bot);
                self.doing.remove(bot);
                None
            }
            _ => None,
        }
    }
}

impl App {
    fn new(client: Arc<Client>, bot: &str, record: &Value) -> Self {
        let model = format!(
            "{}/{}",
            record["provider"].as_str().unwrap_or("?"),
            record["model"].as_str().unwrap_or("?")
        );
        Self {
            client,
            bot: bot.to_owned(),
            project: None,
            workspace: record["workspace"].as_str().unwrap_or("").to_owned(),
            model,
            effort: record["effort"].as_str().map(str::to_owned),
            tokens_used: record["tokens_used"].as_u64().unwrap_or(0),
            input: String::new(),
            pending: Vec::new(),
            any_history: false,
            width: 80,
            live: None,
            streamed: false,
            running: record["running_turn"].as_i64().map(|turn| Running {
                turn,
                started: Instant::now(),
                phase: "Working".into(),
            }),
            usage: (0, 0, 0),
            calls: HashMap::new(),
            approvals: Vec::new(),
            following: true,
            notice: None,
            quit: false,
            started: Instant::now(),
            fleet: None,
            held: Vec::new(),
            seen_node: 0,
            turn_model: None,
            sounds: sound::Sounds::new(false),
            said: String::new(),
            replay: None,
        }
    }

    async fn main(&mut self, terminal: &mut Tui, events: &mut Events) -> Result<(), String> {
        self.width = terminal.size().map(|s| s.width as usize).unwrap_or(80);
        self.history().await;
        // A turn already running: its clock, and any call it waits on.
        if let Some(turn) = self.running.as_ref().map(|r| r.turn) {
            status_report("working", &self.bot);
            if let Ok(page) = self
                .client
                .request(
                    "turns",
                    json!({"bot": self.bot, "newest": true, "limit": 1}),
                )
                .await
                && let Some(started) = page["turns"][0]["started_ms"].as_u64()
            {
                let now = std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .unwrap_or_default()
                    .as_millis() as u64;
                let ran = Duration::from_millis(now.saturating_sub(started));
                if let Some(running) = &mut self.running {
                    running.started = Instant::now().checked_sub(ran).unwrap_or(running.started);
                }
            }
            self.approvals_for(Some(turn)).await;
        } else {
            status_report("idle", &self.bot);
        }
        let (keys_tx, mut keys) = tokio::sync::mpsc::unbounded_channel();
        // Poll, not a bare read: a blocked read holds crossterm's reader
        // lock, and the cursor query the inline viewport makes on a resize
        // then times out (Rex resizes a block as it lays it out).
        std::thread::spawn(move || {
            while !keys_tx.is_closed() {
                match event::poll(Duration::from_millis(50)) {
                    Ok(false) => {}
                    Ok(true) => match event::read() {
                        Ok(event) => {
                            if keys_tx.send(event).is_err() {
                                break;
                            }
                        }
                        Err(_) => break,
                    },
                    Err(_) => break,
                }
            }
        });
        let mut tick = tokio::time::interval(Duration::from_millis(100));
        loop {
            self.draw(terminal).map_err(|e| e.to_string())?;
            if self.quit {
                return Ok(());
            }
            tokio::select! {
                event = events.recv() => match event {
                    Some(event) => self.daemon(event).await,
                    None => {
                        self.notice = Some("the daemon hung up".into());
                        self.running = None;
                    }
                },
                Some(key) = keys.recv() => self.key(key).await,
                Some(event) = async {
                    match &mut self.fleet {
                        Some(fleet) => fleet.events.recv().await,
                        None => std::future::pending().await,
                    }
                } => {
                    let said = self.fleet.as_mut().and_then(|fleet| fleet.event(&event));
                    let alerts = self
                        .fleet
                        .as_mut()
                        .map(|fleet| std::mem::take(&mut fleet.alerts))
                        .unwrap_or_default();
                    for (alert, what) in alerts {
                        self.sounds.alert(alert, &what);
                    }
                    if let Some(said) = said {
                        // After the call that started the task, not above it.
                        self.held.push(Line::from(format!("  ↳ {said}")).dim());
                        if self.calls.is_empty() {
                            let held = std::mem::take(&mut self.held);
                            self.commit(held);
                        }
                    }
                }
                _ = tick.tick() => {}
            }
        }
    }

    /// Commit a finished cell, a blank line apart from the one before.
    fn commit(&mut self, lines: Vec<Line<'static>>) {
        if lines.is_empty() {
            return;
        }
        if self.any_history {
            self.pending.push(Line::default());
        }
        self.any_history = true;
        self.pending.extend(lines);
    }

    async fn items(&self, nodes: &[i64]) -> Vec<Value> {
        match self
            .client
            .request("history_items", json!({"bot": self.bot, "nodes": nodes}))
            .await
        {
            Ok(read) => read["items"]
                .as_array()
                .cloned()
                .unwrap_or_default()
                .into_iter()
                .map(|entry| entry["item"].clone())
                .collect(),
            Err(_) => Vec::new(),
        }
    }

    /// The bot's latest items, written as the transcript would have shown them.
    async fn history(&mut self) {
        let Ok(page) = self
            .client
            .request(
                "history_nodes",
                json!({"bot": self.bot, "limit": HISTORY_ITEMS}),
            )
            .await
        else {
            return;
        };
        let mut nodes: Vec<i64> = page["nodes"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(|n| n["node"].as_i64())
            .collect();
        nodes.reverse();
        self.seen_node = nodes.last().copied().unwrap_or(0);
        let mut calls: HashMap<String, (String, Value)> = HashMap::new();
        for chunk in nodes.chunks(100) {
            for item in self.items(chunk).await {
                match (item["type"].as_str(), item["role"].as_str()) {
                    (_, Some("user")) => {
                        let lines = transcript::prompt(&text(&item), None, self.width);
                        self.commit(lines);
                    }
                    (_, Some("assistant")) => {
                        let lines = transcript::reply(&text(&item), self.width);
                        self.commit(lines);
                    }
                    (Some("function_call"), _) => {
                        let arguments = item["arguments"]
                            .as_str()
                            .and_then(|a| serde_json::from_str(a).ok())
                            .unwrap_or(Value::Null);
                        let name = item["name"].as_str().unwrap_or("tool").to_owned();
                        calls.insert(
                            item["call_id"].as_str().unwrap_or("").to_owned(),
                            (name, arguments),
                        );
                    }
                    (Some("function_call_output"), _) => {
                        let id = item["call_id"].as_str().unwrap_or("");
                        if let Some((name, arguments)) = calls.remove(id) {
                            let (output, failed) = output(&item);
                            let lines = transcript::tool(
                                &name,
                                &arguments,
                                Some(&output),
                                failed,
                                &self.workspace,
                                self.width,
                            );
                            self.commit(lines);
                        }
                    }
                    _ => {}
                }
            }
        }
        if self.any_history {
            self.pending.push(Line::default());
            self.pending.push(
                Line::from(format!("─ attached to {} ", self.bot))
                    .dim()
                    .patch_style(Style::new()),
            );
            self.any_history = true;
        }
    }

    fn end_live(&mut self) {
        if let Some(mut live) = self.live.take() {
            let rest = std::mem::take(&mut live.partial);
            if !rest.is_empty() {
                self.live_line(&mut live, &rest);
            }
        }
    }

    fn live_line(&mut self, live: &mut Live, line: &str) {
        if let Some(styled) = live.markdown.line(line) {
            let lead = if live.lines == 0 { "• " } else { "  " };
            let rest = live.markdown.rest();
            let rows = transcript::wrap(styled, self.width, Span::raw(lead), rest);
            live.lines += rows.len();
            self.pending.extend(rows);
            self.any_history = true;
        }
    }

    async fn daemon(&mut self, event: Value) {
        let kind = event["event"].as_str().unwrap_or("");
        if !self.following {
            if kind == "follow_live" {
                self.following = true;
            }
            return;
        }
        if event["bot"] != self.bot.as_str() {
            return;
        }
        let data = &event["data"];
        let turn = event["turn"].as_i64();
        // Followed before history was read, so what history showed can
        // arrive again here.
        if matches!(kind, "accepted" | "steered" | "message" | "tool_completed")
            && data["node"]
                .as_i64()
                .is_some_and(|node| node <= self.seen_node)
        {
            return;
        }
        match kind {
            "accepted" => {
                self.end_live();
                self.said.clear();
                self.replay = None;
                self.usage = (0, 0, 0);
                self.running = Some(Running {
                    turn: turn.unwrap_or(0),
                    started: Instant::now(),
                    phase: "Working".into(),
                });
                status_report("working", &self.bot);
                if let Some(node) = data["node"].as_i64()
                    && let Some(item) = self.items(&[node]).await.first()
                {
                    let lines = transcript::prompt(&text(item), None, self.width);
                    self.commit(lines);
                }
            }
            "steered" => {
                if let Some(node) = data["node"].as_i64()
                    && let Some(item) = self.items(&[node]).await.first()
                {
                    self.end_live();
                    let lines = transcript::prompt(&text(item), None, self.width);
                    self.commit(lines);
                }
            }
            "queued" => self.notice = Some(format!("queued behind turn {}", data["behind"])),
            "text_delta" => {
                let mut delta = event["text"].as_str().unwrap_or("");
                // A retried call streams its reply again from the start:
                // what was already shown is not shown twice.
                if let Some(replay) = &mut self.replay {
                    if let Some(left) = replay.strip_prefix(delta) {
                        self.said.push_str(delta);
                        *replay = left.to_owned();
                        if replay.is_empty() {
                            self.replay = None;
                        }
                        return;
                    }
                    match delta.strip_prefix(replay.as_str()) {
                        Some(rest) => delta = rest,
                        None => {
                            self.notice = Some("the model retried; its reply may repeat".into())
                        }
                    }
                    self.replay = None;
                }
                self.said.push_str(delta);
                self.streamed = true;
                if let Some(running) = &mut self.running {
                    running.phase = "Working".into();
                }
                let mut live = match self.live.take() {
                    Some(live) => live,
                    None => {
                        // The gap above the reply goes out with its first
                        // delta, so the open line sits where it will stay.
                        if self.any_history {
                            self.pending.push(Line::default());
                        }
                        Live {
                            markdown: Markdown::default(),
                            partial: String::new(),
                            lines: 0,
                        }
                    }
                };
                live.partial.push_str(delta);
                while let Some(at) = live.partial.find('\n') {
                    let line: String = live.partial.drain(..=at).collect();
                    self.live_line(&mut live, line.trim_end_matches('\n'));
                }
                self.live = Some(live);
            }
            "thinking_delta" => {
                if let Some(running) = &mut self.running {
                    running.phase = "Thinking".into();
                }
            }
            "message" => {
                let item = match data["node"].as_i64() {
                    Some(node) => self.items(&[node]).await.into_iter().next(),
                    None => None,
                }
                .filter(|item| item["role"] == "assistant");
                let said = std::mem::take(&mut self.said);
                self.replay = None;
                if self.streamed {
                    self.streamed = false;
                    // The lines already ended stay; the rest is drawn from
                    // the message as kept, not as streamed.
                    if let Some(item) = &item
                        && let Some(live) = &mut self.live
                    {
                        let ended = &said[..said.rfind('\n').map_or(0, |at| at + 1)];
                        if let Some(rest) = text(item).strip_prefix(ended) {
                            live.partial = rest.to_owned();
                        }
                    }
                    if let Some(mut live) = self.live.take() {
                        let rest = std::mem::take(&mut live.partial);
                        let rest = rest.strip_suffix('\n').unwrap_or(&rest);
                        if !rest.is_empty() {
                            for line in rest.split('\n') {
                                self.live_line(&mut live, line);
                            }
                        }
                    }
                } else if let Some(item) = &item {
                    let lines = transcript::reply(&text(item), self.width);
                    self.commit(lines);
                }
            }
            "retry" => {
                if !self.said.is_empty() {
                    self.replay = Some(std::mem::take(&mut self.said));
                }
                if let Some(running) = &mut self.running {
                    running.phase = "Retrying".into();
                }
            }
            "tool_started" => {
                self.said.clear();
                self.end_live();
                let name = data["name"].as_str().unwrap_or("tool").to_owned();
                let arguments = data["arguments"]
                    .as_str()
                    .and_then(|a| serde_json::from_str(a).ok())
                    .unwrap_or(Value::Null);
                if let Some(running) = &mut self.running {
                    running.phase = match (name.as_str(), arguments["command"].as_str()) {
                        ("shell", Some(command)) => format!("Running {}", first_line(command)),
                        _ => format!("Calling {name}"),
                    };
                }
                self.calls.insert(
                    data["call_id"].as_str().unwrap_or("").to_owned(),
                    (name, arguments),
                );
            }
            "tool_completed" => {
                let id = data["call_id"].as_str().unwrap_or("");
                let Some((name, arguments)) = self.calls.remove(id) else {
                    return;
                };
                let mut shown = (String::new(), data["failed"] == true);
                if let Some(node) = data["node"].as_i64()
                    && let Some(item) = self.items(&[node]).await.first()
                {
                    shown = output(item);
                    shown.1 |= data["failed"] == true;
                }
                if data["denied"] == true {
                    shown = ("denied".into(), true);
                }
                let lines = transcript::tool(
                    &name,
                    &arguments,
                    Some(&shown.0),
                    shown.1,
                    &self.workspace,
                    self.width,
                );
                self.commit(lines);
                if self.calls.is_empty() && !self.held.is_empty() {
                    let held = std::mem::take(&mut self.held);
                    self.pending.extend(held);
                }
                self.approvals.retain(|call| call["call_id"] != id);
                if let Some(running) = &mut self.running {
                    running.phase = "Working".into();
                }
            }
            "approval_requested" => self.approvals_for(turn).await,
            "turn_waiting" => {
                if let Some(running) = &mut self.running
                    && data["approval"] != true
                {
                    running.phase = "Waiting".into();
                }
            }
            "turn_paced" => {
                if let Some(running) = &mut self.running {
                    running.phase = "Rate limited".into();
                }
            }
            "turn_resumed" => {
                if let Some(running) = &mut self.running {
                    running.phase = "Working".into();
                }
            }
            "usage" => {
                self.usage.0 += data["input_tokens"].as_u64().unwrap_or(0);
                self.usage.1 += data["output_tokens"].as_u64().unwrap_or(0);
                self.usage.2 += data["cached_input_tokens"].as_u64().unwrap_or(0);
                self.tokens_used += data["input_tokens"].as_u64().unwrap_or(0)
                    + data["output_tokens"].as_u64().unwrap_or(0);
            }
            "turn_finished" => {
                if self.running.as_ref().is_some_and(|r| Some(r.turn) != turn) {
                    return;
                }
                self.end_live();
                let status = data["status"].as_str().unwrap_or("unknown");
                let detail = data["error"].as_str().unwrap_or("");
                let seconds = self.running.take().map(|r| r.started.elapsed().as_secs());
                let line = transcript::finished(status, detail, seconds, self.usage, self.width);
                self.commit(vec![line]);
                self.approvals.clear();
                self.notice = None;
                status_report(
                    match status {
                        "completed" | "steered" => "done",
                        "interrupted" => "idle",
                        _ => "error",
                    },
                    &self.bot,
                );
                if self.fleet.is_none() {
                    match status {
                        "completed" | "steered" => {
                            let what = format!("{} is done", self.bot);
                            self.sounds.alert(sound::Alert::LeadDone, &what)
                        }
                        "interrupted" => {}
                        _ => {
                            let what = format!("{} failed", self.bot);
                            self.sounds.alert(sound::Alert::Failed, &what)
                        }
                    }
                }
            }
            "deleted" => {
                self.notice = Some(format!("{} was deleted", self.bot));
                self.running = None;
            }
            "follow_lagged" => self.notice = Some("fell behind the daemon; reattach".into()),
            _ => {}
        }
    }

    /// The project's agents on one card, from the daemon and Rex alone.
    async fn status(&mut self) {
        let (project, lead) = match &self.project {
            Some(project) => (project.clone(), format!("{project}.lead")),
            None => (
                self.bot.split('.').next().unwrap_or(&self.bot).to_owned(),
                self.bot.clone(),
            ),
        };
        let pattern = if self.bot.contains('.') || self.project.is_some() {
            format!("{project}.*")
        } else {
            self.bot.clone()
        };
        let Ok(page) = self
            .client
            .request("bots", json!({"name": pattern, "limit": 256}))
            .await
        else {
            self.notice = Some("could not list the agents".into());
            return;
        };
        let places = self
            .fleet
            .as_ref()
            .map(|fleet| fleet.rex.places())
            .unwrap_or_default();
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_millis() as u64;
        let mut rows = Vec::new();
        for bot in page["bots"].as_array().into_iter().flatten() {
            let Some(name) = bot["name"].as_str() else {
                continue;
            };
            let turn = self
                .client
                .request("turns", json!({"bot": name, "newest": true, "limit": 1}))
                .await
                .ok()
                .and_then(|page| page["turns"].get(0).cloned())
                .unwrap_or(Value::Null);
            let running = !bot["running_turn"].is_null();
            let state = match (running, turn["status"].as_str()) {
                (true, Some("waiting")) if turn["waiting_on"]["approval"] == true => {
                    "needs you".into()
                }
                (true, Some("waiting")) => {
                    let on: Vec<String> = turn["waiting_on"]["handles"]
                        .as_array()
                        .into_iter()
                        .flatten()
                        .filter_map(Value::as_str)
                        .map(calls::handle_bot)
                        .map(|b| short(&b, &project, &lead))
                        .collect();
                    format!("waiting on {}", on.join(", "))
                }
                (true, Some("paced")) => "rate limited".into(),
                (true, Some("queued")) => "queued".into(),
                (true, _) => "working".into(),
                (false, Some("completed" | "steered")) => "done".into(),
                (false, Some("interrupted")) => "stopped".into(),
                (false, Some(other)) => other.to_owned(),
                (false, None) => "idle".into(),
            };
            let at = if running {
                turn["started_ms"].as_u64()
            } else {
                turn["finished_ms"].as_u64()
            };
            let place = match places.get(name) {
                Some((tab, Some((side, other)))) => {
                    format!("tab {tab}, {side} {}", short(other, &project, &lead))
                }
                Some((tab, None)) => format!("tab {tab}"),
                None => String::new(),
            };
            // This window's turns run on its own model, when it has one.
            let model = match &self.turn_model {
                Some(model) if name == self.bot => model.clone(),
                _ => format!(
                    "{}/{}",
                    bot["provider"].as_str().unwrap_or("?"),
                    bot["model"].as_str().unwrap_or("?")
                ),
            };
            rows.push(status::Row {
                name: short(name, &project, &lead),
                state,
                seconds: at.map(|at| now.saturating_sub(at) / 1000),
                model,
                latest: self.latest(name).await,
                place,
            });
        }
        // The lead first, then what needs the person, what works, the rest.
        let rank = |row: &status::Row| match row.state.as_str() {
            _ if row.name == "lead" => 0,
            "needs you" => 1,
            "working" => 2,
            s if s.starts_with("waiting") => 3,
            _ => 4,
        };
        rows.sort_by_key(|row| (rank(row), row.seconds.unwrap_or(u64::MAX)));
        let lines = status::card(&project, &rows, self.width);
        self.commit(lines);
    }

    /// An agent's latest step, as a line.
    async fn latest(&self, bot: &str) -> String {
        let Ok(page) = self
            .client
            .request("history_nodes", json!({"bot": bot, "limit": 3}))
            .await
        else {
            return String::new();
        };
        let nodes: Vec<i64> = page["nodes"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(|n| n["node"].as_i64())
            .collect();
        let Ok(read) = self
            .client
            .request("history_items", json!({"bot": bot, "nodes": nodes}))
            .await
        else {
            return String::new();
        };
        // Newest first, as the nodes were asked for.
        let items: Vec<Value> = read["items"]
            .as_array()
            .into_iter()
            .flatten()
            .map(|entry| entry["item"].clone())
            .collect();
        for item in items {
            match (item["type"].as_str(), item["role"].as_str()) {
                (_, Some("assistant")) => return first_line(&text(&item)),
                (Some("function_call"), _) => {
                    let arguments: Value = item["arguments"]
                        .as_str()
                        .and_then(|a| serde_json::from_str(a).ok())
                        .unwrap_or(Value::Null);
                    let name = item["name"].as_str().unwrap_or("tool");
                    return match arguments["command"].as_str() {
                        Some(command) => match calls::said(command, "", false) {
                            Some(said) => format!("{} {}", said.verb, said.what).trim().to_owned(),
                            None => format!("$ {}", first_line(command)),
                        },
                        None => format!("{name} {}", arguments["path"].as_str().unwrap_or("")),
                    };
                }
                (_, Some("user")) => return format!("asked: {}", first_line(&text(&item))),
                _ => {}
            }
        }
        String::new()
    }

    /// The turn's calls a person must answer; a program answers the rest.
    async fn approvals_for(&mut self, turn: Option<i64>) {
        let Ok(page) = self
            .client
            .request("approvals", json!({"bot": self.bot, "limit": 64}))
            .await
        else {
            return;
        };
        self.approvals = page["approvals"]
            .as_array()
            .into_iter()
            .flatten()
            .filter(|call| {
                call["turn"].as_i64() == turn
                    && call["gates"]
                        .as_array()
                        .is_some_and(|gates| gates.iter().any(|g| g == "manual"))
            })
            .cloned()
            .collect();
        if !self.approvals.is_empty() {
            status_report("blocked", &self.bot);
            // With a fleet, the lead's Sonny rings for every agent.
            if self.fleet.is_none() {
                let what = format!("{} needs you", self.bot);
                self.sounds.alert(sound::Alert::NeedsYou, &what);
            }
        }
    }

    async fn answer(&mut self, decision: &str) {
        let call = self.approvals.remove(0);
        let answered = self
            .client
            .request(
                "answer",
                json!({"bot": self.bot, "turn": call["turn"], "call_id": call["call_id"],
                    "request": call["request"], "tag": "manual", "decision": decision, "by": "tui"}),
            )
            .await;
        if let Err(error) = answered {
            self.notice = Some(error.to_string());
        }
        if self.approvals.is_empty() {
            status_report("working", &self.bot);
        }
    }

    async fn key(&mut self, event: Event) {
        match event {
            Event::Paste(text) => self.input.push_str(&text),
            Event::Resize(width, _) => self.width = width as usize,
            Event::Key(key) if key.kind != KeyEventKind::Release => {
                let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
                match key.code {
                    KeyCode::Char('c') if ctrl => {
                        if self.input.is_empty() {
                            self.quit = true;
                        } else {
                            self.input.clear();
                        }
                    }
                    KeyCode::Char('d') if ctrl && self.input.is_empty() => self.quit = true,
                    KeyCode::Char(c @ ('y' | 'n'))
                        if !self.approvals.is_empty() && self.input.is_empty() =>
                    {
                        self.answer(if c == 'y' { "allow" } else { "deny" }).await;
                    }
                    KeyCode::Char(c) if !ctrl => self.input.push(c),
                    KeyCode::Backspace => {
                        self.input.pop();
                    }
                    KeyCode::Enter if key.modifiers.contains(KeyModifiers::SHIFT) => {
                        self.input.push('\n')
                    }
                    KeyCode::Enter => self.submit().await,
                    KeyCode::Esc => {
                        if let Some(running) = &self.running {
                            let turn = running.turn;
                            let _ = self
                                .client
                                .request("interrupt", json!({"bot": self.bot, "turn": turn}))
                                .await;
                        }
                    }
                    _ => {}
                }
            }
            _ => {}
        }
    }

    async fn submit(&mut self) {
        let prompt = self.input.trim().to_owned();
        if prompt.is_empty() {
            return;
        }
        if matches!(prompt.as_str(), "/status" | "/agents" | "/ps") {
            self.input.clear();
            self.status().await;
            return;
        }
        // A message to a busy bot joins its running turn, on that turn's model.
        let delivery = self.running.is_some().then_some("steer");
        let model = delivery
            .is_none()
            .then(|| self.turn_model.clone())
            .flatten();
        let submitted = self
            .client
            .request(
                "submit",
                json!({"bot": self.bot, "request_id": unique("tui"), "prompt": prompt,
                    "delivery": delivery, "model": model}),
            )
            .await;
        match submitted {
            Ok(_) => {
                self.input.clear();
                self.notice = None;
            }
            Err(error) => self.notice = Some(error.to_string()),
        }
    }

    fn draw(&mut self, terminal: &mut Tui) -> std::io::Result<()> {
        // The pane may have changed size since these lines were wrapped:
        // the viewport takes the new size first, and a line still too wide
        // is cut into rows rather than losing its end.
        terminal.autoresize()?;
        let width = terminal.size()?.width as usize;
        self.width = width;
        if !self.pending.is_empty() {
            let lines: Vec<Line<'static>> = std::mem::take(&mut self.pending)
                .into_iter()
                .flat_map(|line| {
                    if line.width() <= width {
                        vec![line]
                    } else {
                        transcript::wrap(line, width, Span::raw(""), Span::raw("    "))
                    }
                })
                .collect();
            terminal.insert_before(lines.len() as u16, |buf| {
                Paragraph::new(lines).render(buf.area, buf);
            })?;
        }
        terminal.draw(|frame| {
            let [live, status, _, composer, footer] =
                Layout::vertical([Constraint::Length(1); 5]).areas(frame.area());
            let width = live.width as usize;
            if let Some(open) = &self.live
                && !open.partial.is_empty()
            {
                let mut markdown = open.markdown.clone();
                if let Some(line) = markdown.line(&open.partial) {
                    let lead = if open.lines == 0 { "• " } else { "  " };
                    // The open line's end, when it is already wider than a row.
                    let rows = transcript::wrap(line, width, Span::raw(lead), markdown.rest());
                    frame.render_widget(
                        Paragraph::new(rows.last().cloned().unwrap_or_default()),
                        live,
                    );
                }
            }
            frame.render_widget(Paragraph::new(self.status_line()), status);
            let (text, cursor) = if self.input.is_empty() {
                let hint = if self.running.is_some() {
                    "Steer the running turn".to_owned()
                } else {
                    format!("Ask {} to do anything", self.bot)
                };
                (
                    Line::from(vec![Span::raw("› ").bold(), Span::raw(hint).dim()]),
                    2,
                )
            } else {
                let shown = self.input.rsplit('\n').next().unwrap_or("");
                let room = width.saturating_sub(3);
                let mut start = 0;
                while shown[start..].width() > room {
                    start += shown[start..].chars().next().map_or(1, char::len_utf8);
                }
                let line = Line::from(vec![
                    Span::raw("› ").bold(),
                    Span::raw(shown[start..].to_owned()),
                ]);
                (line, 2 + shown[start..].width() as u16)
            };
            frame.render_widget(Paragraph::new(text), composer);
            frame.set_cursor_position((composer.x + cursor, composer.y));
            frame.render_widget(Paragraph::new(self.footer_line(width)), footer);
        })?;
        Ok(())
    }

    fn status_line(&self) -> Line<'static> {
        if let Some(call) = self.approvals.first() {
            let name = call["name"].as_str().unwrap_or("tool");
            let what = call["arguments"]["command"]
                .as_str()
                .or_else(|| call["arguments"]["path"].as_str())
                .map(first_line)
                .unwrap_or_default();
            // The keys first, so a narrow pane still shows how to answer.
            return Line::from(vec![
                Span::raw("⏸ ").fg(Color::Yellow),
                Span::raw(format!("Allow {name}? ")).bold(),
                Span::raw("y").bold(),
                Span::raw(" allow · ").dim(),
                Span::raw("n").bold(),
                Span::raw(" deny  ").dim(),
                Span::raw(what),
            ]);
        }
        if let Some(running) = &self.running {
            let mut spans = vec![Span::raw("• ").dim()];
            spans.extend(shimmer(&running.phase, self.started.elapsed()));
            spans.push(
                Span::raw(format!(
                    " ({} • esc to interrupt)",
                    transcript::elapsed(running.started.elapsed().as_secs())
                ))
                .dim(),
            );
            return Line::from(spans);
        }
        match &self.notice {
            Some(notice) => Line::from(Span::raw(notice.clone()).fg(Color::Yellow)),
            None => Line::default(),
        }
    }

    fn footer_line(&self, width: usize) -> Line<'static> {
        let home = std::env::var("HOME").unwrap_or_default();
        let folder = match self.workspace.strip_prefix(&home) {
            Some(rest) if !home.is_empty() => format!("~{rest}"),
            _ => self.workspace.clone(),
        };
        let mut left = format!("  {}", self.model);
        if let Some(effort) = &self.effort {
            left.push_str(&format!(" {effort}"));
        }
        let right = format!(
            "{} · {} tokens  ",
            self.bot,
            transcript::tokens(self.tokens_used)
        );
        // A long folder keeps its end, the part that tells folders apart.
        let room = width.saturating_sub(left.width() + right.width() + 4);
        let mut folder = folder.as_str();
        let cut = folder.width() > room;
        while folder.width() > room.saturating_sub(1) && !folder.is_empty() {
            folder = &folder[folder.chars().next().map_or(1, char::len_utf8)..];
        }
        left.push_str(&format!(" · {}{folder}", if cut { "…" } else { "" }));
        let fill = width.saturating_sub(left.width() + right.width());
        Line::from(format!("{left}{}{right}", " ".repeat(fill))).dim()
    }
}

/// Codex's sweep: a brighter band moving across the phase every two seconds.
fn shimmer(text: &str, since: Duration) -> Vec<Span<'static>> {
    let chars: Vec<char> = text.chars().collect();
    let period = chars.len() as f32 + 20.0;
    let at = (since.as_secs_f32() % 2.0) / 2.0 * period;
    chars
        .into_iter()
        .enumerate()
        .map(|(i, c)| {
            let distance = (i as f32 + 10.0 - at).abs();
            let style = if distance < 2.0 {
                Style::new().add_modifier(Modifier::BOLD)
            } else if distance < 4.0 {
                Style::new()
            } else {
                Style::new().add_modifier(Modifier::DIM)
            };
            Span::styled(c.to_string(), style)
        })
        .collect()
}

/// A bot's name in its project: `lead`, or what follows the project's dot.
fn short(bot: &str, project: &str, lead: &str) -> String {
    if bot == lead {
        return "lead".into();
    }
    bot.strip_prefix(&format!("{project}."))
        .unwrap_or(bot)
        .to_owned()
}

fn first_line(text: &str) -> String {
    let line = text.lines().next().unwrap_or("");
    let mut cut: String = line.chars().take(60).collect();
    if cut.len() < line.len() || text.lines().nth(1).is_some() {
        cut.push('…');
    }
    cut
}

/// A message item's text.
fn text(item: &Value) -> String {
    match &item["content"] {
        Value::String(text) => text.clone(),
        Value::Array(parts) => parts
            .iter()
            .filter_map(|part| part["text"].as_str())
            .collect::<Vec<_>>()
            .join(""),
        _ => String::new(),
    }
}

/// A tool result's text, and whether it failed. A shell result is JSON
/// with its streams and exit status.
fn output(item: &Value) -> (String, bool) {
    let raw = item["output"]
        .as_str()
        .map(str::to_owned)
        .unwrap_or_else(|| text(item));
    match serde_json::from_str::<Value>(&raw) {
        Ok(shell) if shell.get("exit_code").is_some() => {
            let mut shown = shell["stdout"].as_str().unwrap_or("").to_owned();
            shown.push_str(shell["stderr"].as_str().unwrap_or(""));
            (shown, shell["success"] == false)
        }
        _ => (raw, false),
    }
}
