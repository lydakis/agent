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
mod approvals;
mod cache;
mod calls;
mod diff;
mod errand;
mod html;
mod image;
mod js;
mod math;
mod mermaid;
mod models;
mod project;
mod rex;
mod settings;
mod shell;
mod skills;
mod sound;
mod status;
mod table;
mod term;
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
        terminal::{
            Clear, ClearType, EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode,
            enable_raw_mode,
        },
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

#[derive(Clone)]
struct Options {
    store: PathBuf,
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
     \x20      [--copies worktree|sandfs] [--bot NAME [--new [--workspace DIR]]]\n\
     With no --bot, the folder's coordinator, made with --model the first time; its tasks\n\
     open in the project's Rex session. Type /status for the project's agents.\n\
     --copies (or /copies): how its tasks get their own copy, kept for every folder: git\n\
     worktrees (the default), or sandfs copies, which take uncommitted changes along and\n\
     work outside git too."
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

/// A program on PATH.
fn which(program: &str) -> Option<PathBuf> {
    let path = std::env::var_os("PATH")?;
    std::env::split_paths(&path)
        .map(|dir| dir.join(program))
        .find(|candidate| candidate.is_file())
}

/// Agent's own binary: the one the daemon gave this shell, else the one
/// installed with Sonny (Homebrew's `libexec/agent`, beside `bin/sonny`),
/// which speaks Sonny's protocol, else the app's. Not `agent` on PATH,
/// which may be another program.
fn agent_bin() -> Option<PathBuf> {
    let beside = std::env::current_exe()
        .and_then(std::fs::canonicalize)
        .ok()
        .and_then(|me| Some(me.parent()?.parent()?.join("libexec/agent")));
    std::env::var_os("AGENT_BIN")
        .map(PathBuf::from)
        .into_iter()
        .chain(beside)
        .chain([PathBuf::from("/Applications/Agent.app/Contents/MacOS/agent")])
        .find(|bin| bin.is_file())
}

/// Agent's binary to run, with the provider keys the app keeps in
/// ~/.agent/env over this shell's own, as the app starts it; an empty
/// value unsets one.
fn agent_command(bin: &std::path::Path) -> Result<std::process::Command, String> {
    let mut command = std::process::Command::new(bin);
    if let Some(file) = agent_client::env_file::path() {
        for (key, value) in agent_client::env_file::read(&file)? {
            match value.is_empty() {
                true => command.env_remove(key),
                false => command.env(key, value),
            };
        }
    }
    Ok(command)
}

/// An `auto` gate needs an approver serving it: when none does, one is
/// started the way the CLI starts it, detached, logging beside the store,
/// judged by `model` unless a judge is set.
async fn serve_auto(client: &Client, options: &Options, model: &str) -> Result<(), String> {
    let stats = client
        .request("stats", json!({}))
        .await
        .map_err(|e| e.to_string())?;
    if stats["approvers"]
        .as_array()
        .is_some_and(|tags| tags.iter().any(|tag| tag == "auto"))
    {
        return Ok(());
    }
    let bin = agent_bin().ok_or("Agent's binary was not found to judge calls; set AGENT_BIN")?;
    let mut log = options.store.clone().into_os_string();
    log.push(".approver.log");
    let log = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&log)
        .map_err(|e| e.to_string())?;
    let mut command = agent_command(&bin)?;
    {
        use std::os::unix::process::CommandExt;
        command.process_group(0);
    }
    command
        .env("AGENT_MODEL", model)
        .arg("approver")
        .arg("--store")
        .arg(&options.store)
        .arg("--socket")
        .arg(&options.socket)
        .stdin(std::process::Stdio::null())
        .stdout(log.try_clone().map_err(|e| e.to_string())?)
        .stderr(log)
        .spawn()
        .map_err(|e| format!("{}: {e}", bin.display()))?;
    Ok(())
}

/// The daemon, started the way the CLI starts it when it is not running.
async fn connect(options: &Options) -> Result<(Arc<Client>, agent_client::Events), String> {
    if let Ok(connected) = Client::connect(&options.socket).await {
        return Ok(connected);
    }
    let bin = agent_bin().ok_or(
        "the daemon is not running and Agent's binary was not found; install Sonny with Homebrew, open Agent.app, or set AGENT_BIN",
    )?;
    let mut command = agent_command(&bin)?;
    let out = command
        .arg("start")
        .arg("--store")
        .arg(&options.store)
        .arg("--socket")
        .arg(&options.socket)
        .stdin(std::process::Stdio::null())
        .output()
        .map_err(|e| format!("{}: {e}", bin.display()))?;
    if !out.status.success() {
        let stderr = String::from_utf8_lossy(&out.stderr);
        let reason = stderr
            .lines()
            .rev()
            .find_map(|line| serde_json::from_str::<Value>(line).ok())
            .and_then(|failure| Some(failure["detail"].as_str()?.to_owned()))
            .unwrap_or_else(|| stderr.trim().to_owned());
        return Err(format!("could not start the daemon: {reason}"));
    }
    Client::connect(&options.socket)
        .await
        .map_err(|e| e.to_string())
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
            "--copies" => {
                let copies = value()?;
                if !agent_client::project::COPIES.contains(&copies.as_str()) {
                    return Err(usage());
                }
                let mut settings = settings::Settings::load();
                settings.copies = copies;
                settings.save()?;
            }
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
        store,
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

// Two threads, so the daemon's events are read while a picture is drawn:
// a reader that stops is dropped by the daemon as lagging.
#[tokio::main(flavor = "multi_thread", worker_threads = 2)]
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
    if std::env::args().nth(1).as_deref() == Some("--version") {
        println!("sonny {}", env!("CARGO_PKG_VERSION"));
        return Ok(());
    }
    let options = parse()?;
    // Fonts are read before the first picture needs them.
    std::thread::spawn(image::preload);
    let (client, mut events) = connect(&options).await?;
    // A lead made just now starts in the first approval mode.
    let mut fresh = false;
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
            // Before the lead is made, so its instructions list the skills.
            skills::install_errand();
            if options.rex
                && let Some(bin) = rex::find()
            {
                rex::install_skill(&bin);
            }
            let lead =
                project::open(&client, &dir, &options.socket, options.model.as_deref()).await?;
            fresh = lead.new;
            (lead.bot, Some(lead.project))
        }
    };
    let bot = client
        .request("inspect", json!({"bot": name}))
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
    term::detect();
    let _ = execute!(std::io::stdout(), EnableBracketedPaste);
    let mut terminal = Terminal::with_options(
        CrosstermBackend::new(std::io::stdout()),
        TerminalOptions {
            viewport: Viewport::Inline(VIEWPORT),
        },
    )
    .map_err(|e| e.to_string())?;
    let mut app = App::new(client, &name, &bot, options.clone());
    app.sounds = sound::Sounds::new(options.sound);
    app.sounds.chime = settings::Settings::load().chime;
    // A bot keeps the model it was made with; --model runs this window's
    // turns on another.
    if let Some(model) = options.model.as_ref().filter(|m| **m != app.model) {
        app.model = model.clone();
        app.turn_model = Some(model.clone());
    }
    project::remember(&app.model);
    // Tasks are laid out in Rex when the person is in it (or names the
    // Rex server to use); elsewhere they run out of sight, in /status.
    if let Some(project) = &project
        && options.rex
        && (std::env::var_os("REX_SESSION").is_some() || options.rex_host.is_some())
    {
        app.fleet = Fleet::start(&options, project, &name).await;
        if let Some(fleet) = &app.fleet {
            app.notice = fleet.note.clone();
        }
    }
    // The lead told what it is told today: skills and memory added since it
    // was made, and Rex while it is read inside Rex.
    if project.is_some()
        && let Some(dir) = bot["workspace"].as_str()
    {
        let rex = app.fleet.as_ref().map(|fleet| fleet.rex.label.clone());
        match project::instructions(std::path::Path::new(dir), rex.as_deref()) {
            Ok(text) if bot["instructions"].as_str() != Some(text.as_str()) => {
                app.instruct = Some(text)
            }
            Ok(_) => {}
            Err(error) => app.notice = Some(format!("the lead's instructions: {error}")),
        }
    }
    app.project = project;
    if fresh {
        app.gating = Some(approvals::MODES[0].0);
    } else if app.mode == Some("auto")
        && let Err(error) = serve_auto(&app.client, &app.options, &app.model).await
    {
        app.notice = Some(error);
    }
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
    let mut asked = json!({"bot": bot, "workspace": workspace, "model": model,
        "instructions": instructions, "tools": DEFAULT_TOOLS,
        "request_id": unique("tui-create")});
    let tools = DEFAULT_TOOLS.map(String::from);
    if let Value::Object(fields) = &mut asked {
        fields.extend(approvals::gate(approvals::MODES[0].0, &tools));
    }
    client
        .request("create", asked)
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

/// The slash commands, as the menu lists them: name and what it does.
/// `/agents` and `/ps` also open `/status`, unlisted.
const COMMANDS: [(&str, &str); 8] = [
    ("/model", "the model and effort"),
    ("/approvals", "whether calls wait for a verdict"),
    ("/status", "this project's agents"),
    ("/sound", "the chime when work ends, on or off"),
    ("/copies", "tasks in git worktrees or sandfs copies"),
    ("/copy", "the last reply, as Markdown"),
    ("/open", "the last HTML page, in your browser"),
    ("/quit", "the agents keep working"),
];

/// `/status` on its own screen, its timers ticking and its rows read
/// again every two seconds, until Esc.
struct Watch {
    screen: Tui,
    project: String,
    rows: Vec<status::Row>,
    /// This project's active Errand jobs.
    jobs: Vec<status::Row>,
    /// When the rows were read: their times count on from here.
    read: Instant,
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
    /// The slash command picked in the menu.
    pick: usize,
    watch: Option<Watch>,
    /// The live area is drawn afresh: the main screen is back from `/status`.
    redraw: bool,
    sounds: sound::Sounds,
    /// The reply text the current model call has streamed.
    said: String,
    /// What a retried call will stream again, not to be shown twice.
    replay: Option<String>,
    /// How to reach the daemon again when the connection drops.
    options: Options,
    /// The lead's connection dropped.
    down: bool,
    /// The background switched: the conversation is drawn again once idle.
    repaint: bool,
    /// The lead's instructions as composed now, sent once no turn runs.
    instruct: Option<String>,
    /// When to connect again, while it or the fleet's is down.
    retry: Option<Instant>,
    /// `/model`, open over the composer.
    picker: Option<models::Picker>,
    /// The model listing `/model` waits on.
    listing: Option<tokio::sync::oneshot::Receiver<Result<Value, String>>>,
    /// The model and effort picked, set once no turn runs.
    switch: Option<(String, Option<String>)>,
    /// The bot's tools, which an approval mode gates.
    tools: Vec<String>,
    /// Its approval mode, when it is one of Sonny's.
    mode: Option<&'static str>,
    /// `/approvals`, open over the composer: the mode picked.
    choosing: Option<usize>,
    /// The mode picked, set once no turn runs.
    gating: Option<&'static str>,
    /// The newest reply's text, for `/copy`.
    last_reply: String,
}

/// The lead's tasks, laid out in the project's Rex session: a window for
/// each task while it works, opened as its turn starts. It follows every
/// bot on its own connection, so the lead's own view is not disturbed.
struct Fleet {
    rex: rex::Rex,
    _client: Arc<Client>,
    events: Events,
    socket: PathBuf,
    /// The connection dropped; it is made again with the lead's.
    down: bool,
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
    /// Detached Errand runs the project's bots started, by call: the
    /// command, until the call's output names the job.
    errands: HashMap<String, String>,
    /// Finished calls whose output names a job to follow in a window:
    /// the bot, the output's node, and the command.
    jobs: Vec<(String, i64, String)>,
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
            "--store".into(),
            options.store.to_string_lossy().into_owned(),
            "--socket".into(),
            options.socket.to_string_lossy().into_owned(),
            "--no-sound".into(),
        ];
        // Said only when the tasks go somewhere this pane is not.
        let note = (!rex.here).then(|| format!("tasks open in Rex session {project}"));
        rex::install_skill(&bin);
        let mut fleet = Self {
            rex,
            _client: client.clone(),
            events,
            socket: options.socket.clone(),
            down: false,
            live: true,
            project: project.to_owned(),
            lead: lead.to_owned(),
            lineage: [lead.to_owned()].into(),
            command,
            note,
            doing: HashMap::new(),
            named: HashMap::new(),
            alerts: Vec::new(),
            errands: HashMap::new(),
            jobs: Vec::new(),
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

    /// Note a detached Errand run as it starts, and its job once the call
    /// finishes, to follow it in a window of its own.
    fn errand(&mut self, bot: &str, kind: &str, data: &Value) {
        let call = data["call_id"].as_str().unwrap_or("").to_owned();
        match kind {
            "tool_started" if data["name"] == "shell" => {
                let command = data["arguments"]
                    .as_str()
                    .and_then(|a| serde_json::from_str::<Value>(a).ok())
                    .and_then(|a| a["command"].as_str().and_then(calls::errand_detached));
                if let Some(command) = command {
                    self.errands.insert(call, command);
                }
            }
            "tool_completed" => {
                if let Some(command) = self.errands.remove(&call)
                    && let Some(node) = data["node"].as_i64()
                    && data["failed"] != true
                {
                    self.jobs.push((bot.to_owned(), node, command));
                }
            }
            _ => {}
        }
    }

    /// A window for each Errand job started since: `errand attach` on its
    /// handle, named after its command.
    async fn follow_jobs(&mut self) {
        let Some(errand) = which("errand") else {
            self.jobs.clear();
            return;
        };
        for (bot, node, command) in std::mem::take(&mut self.jobs) {
            let Ok(read) = self
                ._client
                .request("history_items", json!({"bot": bot, "nodes": [node]}))
                .await
            else {
                continue;
            };
            let (printed, _) = output(&read["items"][0]["item"]);
            let Some(handle) = printed
                .lines()
                .map(str::trim)
                .find(|line| line.contains('/') && !line.contains(' '))
            else {
                continue;
            };
            let label = format!("errand:{handle}");
            let name = format!("errand · {}", first_line(&command));
            let attach = vec![
                errand.to_string_lossy().into_owned(),
                "attach".into(),
                handle.to_owned(),
            ];
            let _ = self
                .rex
                .place(&label, &name, &attach, &rex::Placement::Window);
        }
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

    /// Follow every bot again on a new connection; what changed while
    /// away shows as the next events come.
    async fn reattach(&mut self) -> bool {
        let Ok((client, mut events)) = Client::connect(&self.socket).await else {
            return false;
        };
        if client
            .request("follow", json!({"bot": "*", "after": 0}))
            .await
            .is_err()
        {
            return false;
        }
        caught_up(&mut events).await;
        self._client = client;
        self.events = events;
        self.down = false;
        true
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
            self.errand(bot, kind, data);
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
                Ok(true) if !self.rex.here => Some(format!("{bot} is in Rex ({})", self.rex.label)),
                Ok(_) => None,
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
    fn new(client: Arc<Client>, bot: &str, record: &Value, options: Options) -> Self {
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
            pick: 0,
            watch: None,
            redraw: false,
            sounds: sound::Sounds::new(false),
            said: String::new(),
            replay: None,
            options,
            down: false,
            repaint: false,
            instruct: None,
            retry: None,
            picker: None,
            listing: None,
            switch: None,
            tools: record["tools"]
                .as_array()
                .into_iter()
                .flatten()
                .filter_map(Value::as_str)
                .map(str::to_owned)
                .collect(),
            mode: approvals::mode(record),
            choosing: None,
            gating: None,
            last_reply: String::new(),
        }
    }

    async fn main(&mut self, terminal: &mut Tui, events: &mut Events) -> Result<(), String> {
        term::laid_out().await;
        let _ = terminal.autoresize();
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
        self.configure().await;
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
                event = events.recv(), if !self.down => match event {
                    Some(event) => self.daemon(event).await,
                    None => self.dropped(),
                },
                Some(key) = keys.recv() => self.key(key).await,
                event = async {
                    match &mut self.fleet {
                        Some(fleet) if !fleet.down => fleet.events.recv().await,
                        _ => std::future::pending().await,
                    }
                } => {
                    let Some(event) = event else {
                        if let Some(fleet) = &mut self.fleet {
                            fleet.down = true;
                        }
                        self.retry.get_or_insert_with(Instant::now);
                        continue;
                    };
                    let said = self.fleet.as_mut().and_then(|fleet| fleet.event(&event));
                    if let Some(fleet) = &mut self.fleet
                        && !fleet.jobs.is_empty()
                    {
                        fleet.follow_jobs().await;
                    }
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
            if self
                .watch
                .as_ref()
                .is_some_and(|w| w.read.elapsed() > Duration::from_secs(2))
            {
                self.reread().await;
            }
            if self.retry.is_some_and(|at| at <= Instant::now()) {
                self.reattach(events).await;
            }
            self.listed();
            self.repaint |= term::switched();
            if self.repaint && self.running.is_none() && self.watch.is_none() {
                self.repaint = false;
                self.repaint(terminal).await?;
            }
        }
    }

    /// The lead's instructions as composed on attach, sent once no turn
    /// runs; the next turn reads them.
    async fn configure(&mut self) {
        if self.running.is_some()
            || self.instruct.is_none() && self.switch.is_none() && self.gating.is_none()
        {
            return;
        }
        let (text, switch, gating) = (self.instruct.take(), self.switch.take(), self.gating.take());
        let mut asked = json!({"bot": self.bot});
        if let Some(text) = &text {
            asked["instructions"] = json!(text);
        }
        if let Some((model, effort)) = &switch {
            asked["model"] = json!(model);
            asked["effort"] = json!(effort);
        }
        if let (Some(mode), Value::Object(fields)) = (gating, &mut asked) {
            fields.extend(approvals::gate(mode, &self.tools));
        }
        match self.client.request("configure", asked).await {
            Ok(_) => {
                if let Some((model, effort)) = switch {
                    project::remember(&model);
                    self.model = model;
                    self.effort = effort;
                    // The bot's own model now, for this window's turns too.
                    self.turn_model = None;
                }
                if let Some(mode) = gating {
                    self.mode = Some(mode);
                    if mode == "auto"
                        && let Err(error) =
                            serve_auto(&self.client, &self.options, &self.model).await
                    {
                        self.notice = Some(error);
                    }
                }
            }
            Err(error) if error.code == "bot_busy" => {
                self.instruct = text;
                self.switch = switch;
                self.gating = gating;
            }
            Err(error) => self.notice = Some(error.to_string()),
        }
    }

    /// What the footer and pickers show of the bot, from its record.
    /// A changed model replaces the one this window's turns run on.
    fn refresh(&mut self, record: &Value, model: bool) {
        if model
            && let (Some(provider), Some(model)) =
                (record["provider"].as_str(), record["model"].as_str())
        {
            self.model = format!("{provider}/{model}");
            self.turn_model = None;
            self.effort = record["effort"].as_str().map(str::to_owned);
        }
        if let Some(workspace) = record["workspace"].as_str() {
            self.workspace = workspace.to_owned();
        }
        self.mode = approvals::mode(record);
    }

    /// `/model`'s listing, once it arrives: the picker opens on it.
    fn listed(&mut self) {
        let Some(listing) = &mut self.listing else {
            return;
        };
        let answer = match listing.try_recv() {
            Ok(answer) => answer,
            Err(tokio::sync::oneshot::error::TryRecvError::Empty) => return,
            Err(_) => Err("the listing was dropped".into()),
        };
        self.listing = None;
        match answer {
            Ok(listing) => {
                self.notice = None;
                let (model, effort) = match &self.switch {
                    Some((model, effort)) => (model.as_str(), effort.as_deref()),
                    None => (self.model.as_str(), self.effort.as_deref()),
                };
                self.picker = Some(models::Picker::new(
                    &listing,
                    self.client.providers(),
                    model,
                    effort,
                ));
            }
            Err(error) => self.notice = Some(error),
        }
    }

    /// The conversation drawn again in the background's new colours, once
    /// no turn is running: the screen and scrollback cleared, and the
    /// bot's items read again as on attach.
    async fn repaint(&mut self, terminal: &mut Tui) -> Result<(), String> {
        execute!(
            std::io::stdout(),
            Clear(ClearType::Purge),
            Clear(ClearType::All),
            ratatui::crossterm::cursor::MoveTo(0, 0)
        )
        .map_err(|e| e.to_string())?;
        *terminal = Terminal::with_options(
            CrosstermBackend::new(std::io::stdout()),
            TerminalOptions {
                viewport: Viewport::Inline(VIEWPORT),
            },
        )
        .map_err(|e| e.to_string())?;
        self.pending.clear();
        self.held.clear();
        self.live = None;
        self.any_history = false;
        self.seen_node = 0;
        self.history().await;
        Ok(())
    }

    /// The connection is gone: the daemon drops a reader that falls
    /// behind, or it stopped. Sonny connects again; a reply cut off
    /// mid-stream is drawn whole from the bot's items once back.
    fn dropped(&mut self) {
        self.live = None;
        self.streamed = false;
        self.said.clear();
        self.replay = None;
        self.notice = Some("reconnecting to the daemon".into());
        self.down = true;
        self.retry.get_or_insert_with(Instant::now);
    }

    /// Connect again, starting the daemon if it stopped, and catch up on
    /// what happened meanwhile: the bot's new items, and a turn that ended
    /// or began. Tried again every two seconds until it works.
    async fn reattach(&mut self, events: &mut Events) {
        let lead = async {
            let (client, mut fresh) = connect(&self.options).await?;
            let record = client
                .request("inspect", json!({"bot": self.bot}))
                .await
                .map_err(|e| e.to_string())?;
            client
                .request("follow", json!({"bot": self.bot, "after": 0}))
                .await
                .map_err(|e| e.to_string())?;
            caught_up(&mut fresh).await;
            Ok::<_, String>((client, fresh, record))
        };
        if self.down {
            let Ok((client, fresh, record)) = lead.await else {
                self.retry = Some(Instant::now() + Duration::from_secs(2));
                return;
            };
            self.client = client;
            *events = fresh;
            self.down = false;
            self.notice = None;
            self.tokens_used = record["tokens_used"].as_u64().unwrap_or(self.tokens_used);
            self.history_after(self.seen_node).await;
            let now = record["running_turn"].as_i64();
            // A turn that ended while away ends here as its event would have.
            if let Some(turn) = self.running.as_ref().map(|r| r.turn)
                && now != Some(turn)
            {
                let page = self
                    .client
                    .request(
                        "turns",
                        json!({"bot": self.bot, "newest": true, "limit": 8}),
                    )
                    .await
                    .unwrap_or_default();
                let row = page["turns"]
                    .as_array()
                    .into_iter()
                    .flatten()
                    .find(|row| row["turn"].as_i64() == Some(turn))
                    .cloned()
                    .unwrap_or_default();
                let status = row["status"].as_str().unwrap_or("unknown");
                let ended = json!({"event": "turn_finished", "bot": self.bot,
                    "turn": turn, "data": {"status": status}});
                self.daemon(ended).await;
            }
            if self.running.is_none()
                && let Some(turn) = now
            {
                self.running = Some(Running {
                    turn,
                    started: Instant::now(),
                    phase: "Working".into(),
                });
                status_report("working", &self.bot);
            }
            self.approvals_for(self.running.as_ref().map(|r| r.turn))
                .await;
        }
        if let Some(fleet) = &mut self.fleet
            && fleet.down
            && !fleet.reattach().await
        {
            self.retry = Some(Instant::now() + Duration::from_secs(2));
            return;
        }
        self.retry = None;
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
        self.history_after(0).await;
    }

    /// The bot's items after `after`, newest [`HISTORY_ITEMS`] at most,
    /// drawn as they would have been live.
    async fn history_after(&mut self, after: i64) {
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
            .filter(|node| *node > after)
            .collect();
        nodes.reverse();
        self.seen_node = nodes.last().copied().unwrap_or(self.seen_node);
        let mut calls: HashMap<String, (String, Value)> = HashMap::new();
        for chunk in nodes.chunks(100) {
            for item in self.items(chunk).await {
                match (item["type"].as_str(), item["role"].as_str()) {
                    (_, Some("user")) => {
                        let lines = transcript::prompt(&text(&item), None, self.width);
                        self.commit(lines);
                    }
                    (_, Some("assistant")) => {
                        self.last_reply = text(&item);
                        let lines = transcript::reply(&self.last_reply, self.width);
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
    }

    /// The reply's last lines, and a table it ended on, drawn.
    fn end_live(&mut self) {
        if let Some(mut live) = self.live.take() {
            let rest = std::mem::take(&mut live.partial);
            let rest = rest.strip_suffix('\n').unwrap_or(&rest);
            if !rest.is_empty() {
                for line in rest.split('\n') {
                    self.live_line(&mut live, line);
                }
            }
            let held = live.markdown.finish(self.width);
            self.live_rows(&mut live, held);
        }
    }

    fn live_line(&mut self, live: &mut Live, line: &str) {
        let rows = live.markdown.feed(line, self.width, live.lines == 0);
        self.live_rows(live, rows);
    }

    fn live_rows(&mut self, live: &mut Live, rows: Vec<Line<'static>>) {
        if !rows.is_empty() {
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
        // Each node is drawn once, should its event come again.
        if matches!(kind, "accepted" | "steered" | "message" | "tool_completed")
            && let Some(node) = data["node"].as_i64()
        {
            self.seen_node = self.seen_node.max(node);
        }
        match kind {
            // Changed by this or another client: the footer and the
            // pickers read the bot again.
            "configured" => {
                if let Ok(record) = self
                    .client
                    .request("inspect", json!({"bot": self.bot}))
                    .await
                {
                    let model = event["changed"]
                        .as_array()
                        .is_some_and(|c| c.iter().any(|f| f == "model"));
                    self.refresh(&record, model);
                }
            }
            "accepted" => {
                self.end_live();
                self.said.clear();
                self.replay = None;
                self.running = Some(Running {
                    turn: turn.unwrap_or(0),
                    started: Instant::now(),
                    phase: "Working".into(),
                });
                // What this turn runs on and in: the bot may have moved, or
                // a client chosen another model for it.
                if let Some(workspace) = data["workspace"].as_str() {
                    self.workspace = workspace.to_owned();
                }
                if let Some(model) = data["model"].as_str() {
                    self.model = model.to_owned();
                }
                self.effort = data["effort"].as_str().map(str::to_owned);
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
                if let Some(item) = &item {
                    self.last_reply = text(item);
                }
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
                    self.end_live();
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
                let line = transcript::finished(status, detail, seconds, self.width);
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
                self.configure().await;
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
            _ => {}
        }
    }

    /// The project's agents on one card, from the daemon and Rex alone.
    /// Open `/status`: its own screen, live until Esc.
    async fn status(&mut self) {
        let (project, rows) = match self.status_rows().await {
            Ok(read) => read,
            Err(error) => {
                self.notice = Some(error);
                return;
            }
        };
        let jobs = errand::jobs(self.folder_name()).await;
        let _ = execute!(std::io::stdout(), EnterAlternateScreen);
        match Terminal::new(CrosstermBackend::new(std::io::stdout())) {
            Ok(screen) => {
                self.watch = Some(Watch {
                    screen,
                    project,
                    rows,
                    jobs,
                    read: Instant::now(),
                })
            }
            Err(error) => {
                let _ = execute!(std::io::stdout(), LeaveAlternateScreen);
                self.notice = Some(error.to_string());
            }
        }
    }

    async fn reread(&mut self) {
        let jobs = errand::jobs(self.folder_name()).await;
        if let Ok((_, rows)) = self.status_rows().await
            && let Some(watch) = &mut self.watch
        {
            watch.rows = rows;
            watch.jobs = jobs;
            watch.read = Instant::now();
        }
    }

    /// The lead's folder's own name, which Errand names its jobs' project by.
    fn folder_name(&self) -> String {
        std::path::Path::new(&self.workspace)
            .file_name()
            .map(|name| name.to_string_lossy().into_owned())
            .unwrap_or_default()
    }

    /// The project's agents, as `/status` shows them.
    async fn status_rows(&self) -> Result<(String, Vec<status::Row>), String> {
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
            return Err("could not list the agents".into());
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
        Ok((project, rows))
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
                (_, Some("assistant")) => return status::plain_line(&text(&item)),
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

    /// The slash commands the input could be, while it is one word that
    /// starts with `/`.
    fn menu(&self) -> Vec<(&'static str, &'static str)> {
        if !self.input.starts_with('/') || self.input.contains(char::is_whitespace) {
            return Vec::new();
        }
        COMMANDS
            .into_iter()
            .filter(|(name, _)| name.starts_with(self.input.as_str()))
            .collect()
    }

    async fn key(&mut self, event: Event) {
        if self.watch.is_some() {
            if let Event::Key(key) = event
                && key.kind != KeyEventKind::Release
                && (matches!(key.code, KeyCode::Esc | KeyCode::Char('q') | KeyCode::Enter)
                    || key.code == KeyCode::Char('c')
                        && key.modifiers.contains(KeyModifiers::CONTROL))
            {
                self.watch = None;
                let _ = execute!(std::io::stdout(), LeaveAlternateScreen);
                self.redraw = true;
            }
            return;
        }
        if let Some(pick) = &mut self.choosing {
            if let Event::Key(key) = event
                && key.kind != KeyEventKind::Release
            {
                let count = approvals::MODES.len();
                match key.code {
                    KeyCode::Up => *pick = (*pick + count - 1) % count,
                    KeyCode::Down => *pick = (*pick + 1) % count,
                    KeyCode::Enter => {
                        let mode = approvals::MODES[*pick].0;
                        self.choosing = None;
                        if self.mode != Some(mode) {
                            self.gating = Some(mode);
                            self.configure().await;
                            if self.gating.is_some() {
                                self.notice = Some("changes when this turn ends".into());
                            }
                        }
                    }
                    KeyCode::Esc => self.choosing = None,
                    KeyCode::Char('c') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                        self.choosing = None
                    }
                    _ => {}
                }
            }
            return;
        }
        if let Some(picker) = &mut self.picker {
            match event {
                Event::Key(key) if key.kind != KeyEventKind::Release => match key.code {
                    KeyCode::Up => picker.up(),
                    KeyCode::Down => picker.down(),
                    KeyCode::Left => picker.less(),
                    KeyCode::Right => picker.more(),
                    KeyCode::Backspace => {
                        let mut filter = picker.filter.clone();
                        filter.pop();
                        picker.narrow(filter);
                    }
                    KeyCode::Char('c') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                        self.picker = None
                    }
                    KeyCode::Char(c) => picker.narrow(format!("{}{c}", picker.filter)),
                    KeyCode::Esc => self.picker = None,
                    KeyCode::Enter => {
                        if let Some(chosen) = picker.chosen() {
                            self.picker = None;
                            self.switch = Some(chosen);
                            self.configure().await;
                            if self.switch.is_some() {
                                self.notice = Some("switches when this turn ends".into());
                            }
                        }
                    }
                    _ => {}
                },
                Event::Paste(text) => picker.narrow(format!("{}{text}", picker.filter)),
                Event::Resize(width, _) => self.width = width as usize,
                _ => {}
            }
            return;
        }
        let menu = self.menu();
        if let Event::Key(key) = &event
            && key.kind != KeyEventKind::Release
            && !menu.is_empty()
        {
            let picked = menu[self.pick.min(menu.len() - 1)].0;
            match key.code {
                KeyCode::Up => {
                    self.pick = (self.pick + menu.len() - 1) % menu.len();
                    return;
                }
                KeyCode::Down => {
                    self.pick = (self.pick + 1) % menu.len();
                    return;
                }
                KeyCode::Tab => {
                    self.input = picked.to_owned();
                    self.pick = 0;
                    return;
                }
                KeyCode::Enter if !key.modifiers.contains(KeyModifiers::SHIFT) => {
                    self.input = picked.to_owned();
                    self.pick = 0;
                    self.submit().await;
                    return;
                }
                KeyCode::Esc => {
                    self.input.clear();
                    self.pick = 0;
                    return;
                }
                _ => self.pick = 0,
            }
        }
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
        if prompt == "/open" {
            self.input.clear();
            let page = transcript::PAGE.lock().ok().and_then(|page| page.clone());
            self.notice = Some(match page {
                Some(page) => {
                    let opener = if cfg!(target_os = "macos") {
                        "open"
                    } else {
                        "xdg-open"
                    };
                    match std::process::Command::new(opener)
                        .arg(&page)
                        .stdin(std::process::Stdio::null())
                        .stdout(std::process::Stdio::null())
                        .stderr(std::process::Stdio::null())
                        .spawn()
                    {
                        Ok(_) => "opened in your browser".to_owned(),
                        Err(error) => format!("{opener}: {error}"),
                    }
                }
                None => "no HTML page in this conversation yet".into(),
            });
            return;
        }
        if prompt == "/model" {
            self.input.clear();
            let (send, receive) = tokio::sync::oneshot::channel();
            let client = self.client.clone();
            tokio::spawn(async move {
                let listing = client.request("provider_models", json!({})).await;
                let _ = send.send(listing.map_err(|error| error.to_string()));
            });
            self.listing = Some(receive);
            self.notice = Some("listing models…".into());
            return;
        }
        if prompt == "/approvals" {
            self.input.clear();
            let current = self.gating.or(self.mode);
            self.choosing = Some(
                approvals::MODES
                    .iter()
                    .position(|(mode, _)| Some(*mode) == current)
                    .unwrap_or(0),
            );
            return;
        }
        if prompt == "/copy" {
            self.input.clear();
            self.notice = Some(if self.last_reply.is_empty() {
                "no reply yet".into()
            } else {
                term::copy(&self.last_reply);
                "copied".into()
            });
            return;
        }
        if prompt == "/sound" {
            self.input.clear();
            let mut settings = settings::Settings::load();
            settings.chime = !self.sounds.chime;
            self.notice = Some(match settings.save() {
                Ok(()) => {
                    self.sounds.chime = settings.chime;
                    if settings.chime {
                        "chime on"
                    } else {
                        "chime off"
                    }
                    .to_owned()
                }
                Err(error) => error,
            });
            return;
        }
        if prompt == "/copies" {
            self.input.clear();
            let mut settings = settings::Settings::load();
            settings.copies = match settings.copies.as_str() {
                "sandfs" => "worktree",
                _ => "sandfs",
            }
            .into();
            if let Err(error) = settings.save() {
                self.notice = Some(error);
                return;
            }
            self.notice = Some(match settings.copies.as_str() {
                "sandfs" => "tasks get sandfs copies".into(),
                _ => "tasks get git worktrees".into(),
            });
            // The lead is told before its next turn, as on attach.
            if self.project.is_some() {
                let rex = self.fleet.as_ref().map(|fleet| fleet.rex.label.clone());
                let dir = std::path::Path::new(&self.workspace);
                match project::instructions(dir, rex.as_deref()) {
                    Ok(text) => self.instruct = Some(text),
                    Err(error) => {
                        self.notice = Some(format!("the lead's instructions: {error}"))
                    }
                }
            }
            return;
        }
        if prompt == "/quit" {
            self.input.clear();
            self.quit = true;
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
        if let Some(watch) = &mut self.watch {
            // History waits for the main screen.
            return draw_watch(watch);
        }
        if std::mem::take(&mut self.redraw) {
            terminal.clear()?;
        }
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
            let menu = self.menu();
            if !menu.is_empty() {
                // Up to three, around the pick, in the rows above the input.
                let pick = self.pick.min(menu.len() - 1);
                let first = pick.saturating_sub(2).min(menu.len().saturating_sub(3));
                let wide = menu.iter().map(|(name, _)| name.len()).max().unwrap_or(0);
                let rows: Vec<Line> = menu
                    .iter()
                    .enumerate()
                    .skip(first)
                    .take(3)
                    .map(|(i, (name, about))| {
                        let name = format!("{name:<wide$}");
                        if i == pick {
                            Line::from(vec![
                                Span::raw("  "),
                                Span::raw(format!(" {name}  {about} ")).reversed(),
                            ])
                        } else {
                            Line::from(vec![
                                Span::raw("   "),
                                Span::raw(name),
                                Span::raw(format!("  {about}")).dim(),
                            ])
                        }
                    })
                    .collect();
                let [_, _, _, composer_row, _] =
                    Layout::vertical([Constraint::Length(1); 5]).areas(frame.area());
                let area = ratatui::layout::Rect {
                    y: composer_row.y.saturating_sub(rows.len() as u16),
                    height: rows.len() as u16,
                    ..composer_row
                };
                frame.render_widget(ratatui::widgets::Clear, area);
                frame.render_widget(Paragraph::new(rows), area);
            }
            if let Some(pick) = self.choosing {
                let rows: Vec<Line> = approvals::MODES
                    .iter()
                    .enumerate()
                    .map(|(i, (mode, about))| {
                        let mark = if Some(*mode) == self.mode {
                            "• "
                        } else {
                            "  "
                        };
                        let name = format!("{mode:<4}");
                        if i == pick {
                            Line::from(vec![
                                Span::raw(format!("  {mark}")),
                                Span::raw(format!(" {name}  {about} ")).reversed(),
                            ])
                        } else {
                            Line::from(vec![
                                Span::raw(format!("  {mark} {name}")),
                                Span::raw(format!("  {about}")).dim(),
                            ])
                        }
                    })
                    .collect();
                let area = ratatui::layout::Rect {
                    y: composer.y.saturating_sub(3),
                    height: 3,
                    ..composer
                };
                frame.render_widget(ratatui::widgets::Clear, area);
                frame.render_widget(Paragraph::new(rows), area);
                let line = Line::from(vec![
                    Span::raw("› ").bold(),
                    Span::raw("approvals for this agent and those it makes").dim(),
                ]);
                frame.render_widget(Paragraph::new(line), composer);
                let keys = Line::from("  ↑↓ mode · enter to choose · esc").dim();
                frame.render_widget(Paragraph::new(keys), footer);
                return;
            }
            if let Some(picker) = &self.picker {
                let shown = picker.shown();
                let at = picker.at();
                let first = at.saturating_sub(2).min(shown.len().saturating_sub(3));
                let mut rows: Vec<Line> = shown
                    .iter()
                    .enumerate()
                    .skip(first)
                    .take(3)
                    .map(|(i, model)| {
                        let mark = if *model == self.model { "• " } else { "  " };
                        if i == at {
                            Line::from(vec![
                                Span::raw(format!("  {mark}")),
                                Span::raw(format!(" {model} ")).reversed(),
                            ])
                        } else {
                            Line::from(format!("  {mark} {model}"))
                        }
                    })
                    .collect();
                // Why the list is short, in the rows it leaves.
                for refused in picker.refused.iter().take(3 - rows.len()) {
                    rows.push(Line::from(format!("     {refused}")).dim());
                }
                let area = ratatui::layout::Rect {
                    y: composer.y.saturating_sub(3),
                    height: 3,
                    ..composer
                };
                frame.render_widget(ratatui::widgets::Clear, area);
                frame.render_widget(Paragraph::new(rows), area);
                let effort = picker.effort().unwrap_or("default");
                let line = if picker.filter.is_empty() {
                    Line::from(vec![
                        Span::raw("› ").bold(),
                        Span::raw("type to narrow").dim(),
                    ])
                } else {
                    Line::from(vec![
                        Span::raw("› ").bold(),
                        Span::raw(picker.filter.clone()),
                    ])
                };
                frame.render_widget(Paragraph::new(line), composer);
                frame.set_cursor_position((
                    composer.x + 2 + picker.filter.width() as u16,
                    composer.y,
                ));
                let keys = Line::from(vec![
                    Span::raw("  effort ").dim(),
                    Span::raw(format!("‹ {effort} ›")).bold(),
                    Span::raw("  ↑↓ model · ←→ effort · enter to switch · esc").dim(),
                ]);
                frame.render_widget(Paragraph::new(keys), footer);
                return;
            }
            let (text, cursor) = if self.input.is_empty() {
                let hint = if self.running.is_some() {
                    "Steer the running turn".to_owned()
                } else {
                    "Ask anything".to_owned()
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

/// `/status` on its own screen: the card, its times counted on since it was
/// read, and how to leave.
fn draw_watch(watch: &mut Watch) -> std::io::Result<()> {
    let since = watch.read.elapsed().as_secs();
    let rows: Vec<status::Row> = watch
        .rows
        .iter()
        .cloned()
        .map(|mut row| {
            row.seconds = row.seconds.map(|s| s + since);
            row
        })
        .collect();
    let jobs: Vec<status::Row> = watch
        .jobs
        .iter()
        .cloned()
        .map(|mut job| {
            job.seconds = job.seconds.map(|s| s + since);
            job
        })
        .collect();
    let project = watch.project.clone();
    watch.screen.autoresize()?;
    watch.screen.draw(|frame| {
        let area = frame.area();
        let width = area.width.saturating_sub(2) as usize;
        let mut lines = vec![Line::default()];
        lines.extend(
            status::card(&project, &rows, &jobs, width)
                .into_iter()
                .map(|line| {
                    let mut spans = vec![Span::raw(" ")];
                    spans.extend(line.spans);
                    Line::from(spans).style(line.style)
                }),
        );
        let footer = Line::from("  esc to close").dim();
        let body = ratatui::layout::Rect {
            height: area.height.saturating_sub(1),
            ..area
        };
        let foot = ratatui::layout::Rect {
            y: area.y + area.height.saturating_sub(1),
            height: 1,
            ..area
        };
        frame.render_widget(Paragraph::new(lines), body);
        frame.render_widget(Paragraph::new(footer), foot);
    })?;
    Ok(())
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
