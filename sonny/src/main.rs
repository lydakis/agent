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
mod herdr;
mod html;
mod image;
mod js;
mod lookout;
mod math;
mod mermaid;
mod models;
mod mux;
mod project;
mod rex;
mod settings;
mod shell;
mod skills;
mod sound;
mod status;
mod table;
mod task;
mod term;
mod transcript;
mod updates;

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
/// The most rows a long prompt's composer grows to; it shows its end.
const COMPOSER_ROWS: usize = 8;
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
    /// Lay the project's tasks out in Rex or Herdr, whichever Sonny runs in.
    layout: bool,
    /// The Rex server the project's session lives on, when not this one.
    rex_host: Option<String>,
    /// Ring when work ends or needs the person.
    sound: bool,
    /// The decision model that wakes the lead when its tasks need it;
    /// `None` with `--lookout off`.
    lookout: Option<String>,
}

fn usage() -> String {
    "usage: sonny [--store PATH] [--socket PATH] [--model PROVIDER/MODEL] [--no-layout] [--rex-host HOST] [--no-sound]\n\
     \x20      [--lookout openai-decisions/MODEL|typesafe/jev-*|off]\n\
     \x20      [--copies worktree|sandfs] [--bot NAME [--new [--workspace DIR]]]\n\
     With no --bot, the folder's coordinator, made with --model the first time; its tasks\n\
     open beside it in Rex or Herdr. Type /status for the project's agents.\n\
     --copies (or /copies): how its tasks get their own copy, kept for every folder: git\n\
     worktrees (the default), or sandfs copies (experimental, built in), which take\n\
     uncommitted changes along and work outside git too.\n\
     sonny task NAME [--setup] [RUN FLAGS] -- BRIEF, from the lead's shell, makes a task's\n\
     folder that way and starts the task there; sonny task changes NAME [--apply] lists\n\
     (or writes here) what a sandfs copy of a plain folder changed.\n\
     The lookout wakes the lead when its tasks need it, judged by openai-decisions/gpt-6-luna\n\
     unless --lookout names another judge or off."
        .into()
}

/// `sonny show TASK [--split SIDE [--beside BOT]] [--rex-host HOST]`: put a
/// task's window where the person asked, from the lead's shell. A task gets
/// a window of its own unless told otherwise; `--split` puts it beside the
/// lead's block, or `--beside` another bot's.
async fn show() -> Result<(), String> {
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
    let (socket, project, mut layout) =
        agents_layout(caller.as_deref().unwrap_or(&bot), host).await?;
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
        Some(side) => mux::Placement::Split {
            side,
            beside: beside.ok_or("--split needs --beside BOT outside an agent")?,
        },
        None => mux::Placement::Window,
    };
    layout.place(&bot, short, &command, &placement)?;
    // The lead's Sonny opens a window for a task whose turn starts, maybe
    // while this placed it: what it opened then goes.
    std::thread::sleep(Duration::from_millis(1500));
    layout.close_others(&bot);
    println!("{}", json!({"bot": bot, "in": layout.home()}));
    Ok(())
}

/// The daemon's socket, the project `bot` belongs to, and its home in the
/// person's multiplexer, from an agent's shell.
async fn agents_layout(
    bot: &str,
    host: Option<String>,
) -> Result<(PathBuf, String, mux::Layout), String> {
    let socket = std::env::var_os("AGENT_SOCKET")
        .map(PathBuf::from)
        .ok_or("run this from an agent's shell (AGENT_SOCKET is not set)")?;
    let socket = std::path::absolute(socket).map_err(|e| e.to_string())?;
    let client = Client::connect(&socket).await.map_err(|e| e.to_string())?.0;
    let project = project_of(&client, bot).await;
    let layout = mux::for_agent(&project, host.as_deref())?;
    Ok((socket, project, layout))
}

/// `sonny split NAME [--window] [--rex-host HOST] -- COMMAND...`: run a
/// command an agent wants watched (a dev server, a long build) in a Rex
/// block labelled `AGENT/NAME`, below the agent's own block or in a window
/// of its own, and print the block's ID. It closes with the agent's window.
async fn split() -> Result<(), String> {
    let usage = "usage: sonny split NAME [--window] [--rex-host HOST] -- COMMAND...";
    let mut args = std::env::args().skip(2);
    let name = args.next().filter(|n| !n.starts_with('-')).ok_or(usage)?;
    if name.contains('/') {
        return Err("a name without /".into());
    }
    let owner = std::env::var("AGENT_BOT")
        .map_err(|_| "run this from an agent's shell (AGENT_BOT is not set)")?;
    let (mut window, mut host) = (false, None);
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--window" => window = true,
            "--rex-host" => host = Some(args.next().ok_or(usage)?),
            "--" => break,
            _ => return Err(usage.into()),
        }
    }
    let command: Vec<String> = args.collect();
    if command.is_empty() {
        return Err(usage.into());
    }
    let (_, _, mut layout) = agents_layout(&owner, host).await?;
    let cwd = std::env::current_dir().map_err(|e| e.to_string())?;
    let block = layout.split(&owner, &name, &cwd, &command, window)?;
    println!(
        "{}",
        json!({"block": block, "label": format!("{owner}/{name}"), "in": layout.home()})
    );
    Ok(())
}

/// The project a bot's name starts with: the longest start that names a
/// lead, as `shop.web` for `shop.web.fix` when `shop.web.lead` is there;
/// else the name up to its first dot.
async fn project_of(client: &Client, bot: &str) -> String {
    let mut starts: Vec<&str> = bot.match_indices('.').map(|(at, _)| &bot[..at]).collect();
    starts.reverse();
    for start in &starts {
        let lead = format!("{start}.lead");
        let page = client
            .request("bots", json!({"name": lead, "limit": 1}))
            .await
            .unwrap_or_default();
        if page["bots"][0]["name"] == lead.as_str() {
            return (*start).to_owned();
        }
    }
    starts.last().copied().unwrap_or(bot).to_owned()
}

/// How many lines of transcript wait while `/status` holds the screen.
const KEPT_UNDER_STATUS: usize = 5_000;

/// A program on PATH.
fn which(program: &str) -> Option<PathBuf> {
    let path = std::env::var_os("PATH")?;
    std::env::split_paths(&path)
        .map(|dir| dir.join(program))
        .find(|candidate| candidate.is_file())
}

/// Agent's own binary: the one the daemon gave this shell (or Homebrew's
/// wrapper names, by a path upgrades keep), else the one installed beside
/// Sonny (`libexec/agent`), which speaks Sonny's protocol, else the app's. Not `agent` on PATH,
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
        .chain([PathBuf::from(
            "/Applications/Agent.app/Contents/MacOS/agent",
        )])
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
    let (mut layout, mut rex_host, mut sound) = (true, None, true);
    let mut lookout = Some(lookout::DEFAULT.to_owned());
    while let Some(arg) = args.next() {
        let mut value = || args.next().ok_or_else(usage);
        match arg.as_str() {
            "--store" => store = Some(PathBuf::from(value()?)),
            "--socket" => socket = Some(PathBuf::from(value()?)),
            "--bot" => bot = Some(value()?),
            "--new" => new = true,
            "--model" => model = Some(value()?),
            "--workspace" => workspace = Some(value()?),
            "--no-layout" => layout = false,
            "--rex-host" => rex_host = Some(value()?),
            "--no-sound" => sound = false,
            "--lookout" => lookout = Some(value()?).filter(|judge| judge != "off"),
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
        layout,
        rex_host,
        sound,
        lookout,
    })
}

// Two threads, so the daemon's events are read while a picture is drawn:
// a reader that stops is dropped by the daemon as lagging.
fn main() -> std::process::ExitCode {
    // sandfs serves task copies' mounts from this program run again as
    // `sonny sandfs ...`, outside any runtime of Sonny's.
    sandfs::handle_relaunch(&["sandfs"]);
    let args: Vec<String> = std::env::args().skip(1).collect();
    let result = match args.split_first() {
        Some((task, rest)) if task == "task" => task::run(rest),
        _ => tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .enable_all()
            .build()
            .map_err(|e| e.to_string())
            .and_then(|runtime| runtime.block_on(run())),
    };
    match result {
        Ok(()) => std::process::ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("sonny: {error}");
            std::process::ExitCode::FAILURE
        }
    }
}

async fn run() -> Result<(), String> {
    match std::env::args().nth(1).as_deref() {
        Some("show") => return show().await,
        Some("split") => return split().await,
        _ => {}
    }
    if std::env::args().nth(1).as_deref() == Some("--version") {
        println!("sonny {}", env!("CARGO_PKG_VERSION"));
        return Ok(());
    }
    if std::env::args().nth(1).as_deref() == Some("lookout-replay") {
        return lookout::replay().await;
    }
    let options = parse()?;
    // Fonts are read before the first picture needs them.
    std::thread::spawn(image::preload);
    let (client, mut events) = connect(&options).await?;
    // A lead made just now starts in the first approval mode.
    let mut fresh = false;
    // A lead moved elsewhere, and the folder it comes back to.
    let mut moving = None;
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
            if options.layout {
                mux::install_skill(options.rex_host.as_deref());
            }
            let lead =
                project::open(&client, &dir, &options.socket, options.model.as_deref()).await?;
            fresh = lead.new;
            moving = lead.moving;
            (lead.bot, Some(lead.project))
        }
    };
    // Everything after now arrives live; history is read from the bot's
    // items instead of replayed events, so the replay is skipped. The bot
    // is read after that, so a turn that ended during the replay is seen
    // as ended.
    client
        .request("follow", json!({"bot": name, "after": 0}))
        .await
        .map_err(|e| e.to_string())?;
    let context = caught_up(&mut events).await;
    let bot = client
        .request("inspect", json!({"bot": name}))
        .await
        .map_err(|e| e.to_string())?;

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
    app.context = context;
    app.sounds = sound::Sounds::new(options.sound);
    app.sounds.chime = settings::Settings::load().chime;
    // A bot keeps the model it was made with; --model runs this window's
    // turns on another.
    if let Some(model) = options.model.as_ref().filter(|m| **m != app.model) {
        app.model = model.clone();
        app.turn_model = Some(model.clone());
    }
    // The lead hears what its tasks did, as the app tells it.
    if let Some(project) = &project {
        tokio::spawn(updates::run(
            app.options.socket.clone(),
            name.clone(),
            project.clone(),
        ));
    }
    // Tasks are laid out in Rex when the person is in it (or names the
    // Rex server to use); elsewhere they run out of sight, in /status.
    if let Some(project) = &project
        && options.layout
    {
        app.fleet = Fleet::start(&options, project, &name).await;
        if let Some(fleet) = &app.fleet {
            app.notice = fleet.note.clone();
        }
    }
    app.lead = project::lead(&app.options.socket, &bot);
    // Task copies a restart unmounted are mounted again before any task
    // works in its folder.
    if project.is_some()
        && let Some(error) = task::remount().pop()
    {
        app.notice = Some(error);
    }
    // The lead told what its folder tells it today (skills and memory added
    // since it was made), unless its instructions were set some other way.
    if project.is_some()
        && let Some(dir) = moving
            .as_deref()
            .or(bot["workspace"].as_str().map(std::path::Path::new))
    {
        match project::review(dir, &bot, &app.lead, fresh) {
            Ok(project::Instructions::Stale(text)) => app.instruct = Some(text),
            Ok(project::Instructions::Current) => {}
            Ok(project::Instructions::Kept) => {
                app.notice = Some(
                    "the lead's instructions were set outside its folder's files; \
                     /instructions takes the folder's"
                        .into(),
                )
            }
            Err(error) => app.notice = Some(format!("the lead's instructions: {error}")),
        }
    }
    if let (Some(project), Some(judge), Some(dir)) = (
        &project,
        &options.lookout,
        moving
            .clone()
            .or(bot["workspace"].as_str().map(PathBuf::from)),
    ) {
        match lookout::Judge::parse(judge) {
            Ok(judge) => lookout::spawn(
                options.socket.clone(),
                project.clone(),
                name.clone(),
                dir,
                judge,
            ),
            Err(error) => app.notice = Some(error),
        }
    }
    // Said last: it is what the next message does.
    if let Some(dir) = &moving {
        app.notice = Some(format!(
            "{name} works in {}; your next message brings it back here",
            tilde(bot["workspace"].as_str().unwrap_or("another folder"))
        ));
        // The project's folder from now: what /model and /instructions
        // write and read, before the move itself.
        app.workspace = dir.display().to_string();
        app.move_to = Some(app.workspace.clone());
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
        "compaction_instructions": agent_client::policy::DEFAULT_COMPACTION_INSTRUCTIONS,
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

/// The viewport's rows, top to bottom: the reply's open line, the status,
/// a gap, the composer (as many rows as are left) and the footer.
fn areas(area: ratatui::layout::Rect) -> [ratatui::layout::Rect; 5] {
    Layout::vertical([
        Constraint::Length(1),
        Constraint::Length(1),
        Constraint::Length(1),
        Constraint::Min(1),
        Constraint::Length(1),
    ])
    .areas(area)
}

/// The viewport made `height` rows tall, for a composer that grew or
/// shrank. It keeps its top: a taller one pushes history up, and a shorter
/// one leaves blank rows under it that the next history fills.
fn fit(terminal: &mut Tui, height: u16) -> std::io::Result<()> {
    let area = terminal.get_frame().area();
    let height = height.min(terminal.size()?.height);
    if area.height == height {
        return Ok(());
    }
    terminal.clear()?;
    execute!(
        std::io::stdout(),
        ratatui::crossterm::cursor::MoveTo(0, area.y)
    )?;
    *terminal = Terminal::with_options(
        CrosstermBackend::new(std::io::stdout()),
        TerminalOptions {
            viewport: Viewport::Inline(height),
        },
    )?;
    Ok(())
}

/// The title of the reasoning's latest part: models that summarize their
/// reasoning open each part with a bold line, `**Checking the tests**`.
fn heading(thought: &str) -> Option<&str> {
    let parts: Vec<&str> = thought.split("**").collect();
    // Bold runs are the odd parts; the last part is still open.
    (1..parts.len().saturating_sub(1))
        .step_by(2)
        .map(|at| parts[at].trim())
        .rfind(|title| !title.is_empty() && !title.contains('\n'))
}

/// A folder as the person reads it: under their home, from `~`.
fn tilde(path: &str) -> String {
    let home = std::env::var("HOME").unwrap_or_default();
    match path.strip_prefix(&home) {
        Some(rest) if !home.is_empty() => format!("~{rest}"),
        _ => path.to_owned(),
    }
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
/// Skip the replay of a follow up to its live events; returns how big the
/// context was after the last model round it held, when it held one.
async fn caught_up(events: &mut Events) -> Option<u64> {
    let mut context = None;
    let replay = async {
        while let Some(event) = events.recv().await {
            if event["event"] == "follow_live" {
                return;
            }
            if let Some(size) = context_after(&event) {
                context = Some(size);
            }
        }
    };
    let _ = tokio::time::timeout(Duration::from_secs(30), replay).await;
    context
}

/// What the next model round reads after a `usage` event's round: that
/// round's input and what it wrote. A compaction's own round is not the
/// conversation's.
fn context_after(event: &Value) -> Option<u64> {
    let data = &event["data"];
    // After a provider-side fallback the totals add up every billed
    // attempt; the conversation is what the last one read and wrote.
    let last = data["models"].as_array().and_then(|tried| tried.last());
    let call = last.unwrap_or(data);
    (event["event"] == "usage" && data["purpose"].is_null()).then(|| {
        call["input_tokens"].as_u64().unwrap_or(0) + call["output_tokens"].as_u64().unwrap_or(0)
    })
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
const COMMANDS: [(&str, &str); 9] = [
    ("/model", "the model and effort"),
    ("/approvals", "whether calls wait for a verdict"),
    ("/instructions", "the lead's, from its folder again"),
    ("/status", "this project's agents"),
    ("/sound", "the chime when work ends, on or off"),
    (
        "/copies",
        "tasks in git worktrees or sandfs copies (experimental)",
    ),
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
    /// How big the conversation was after its last model round, as the
    /// next round reads it, once known.
    context: Option<u64>,
    input: String,
    /// Lines waiting to be written into scrollback.
    pending: Vec<Line<'static>>,
    /// Lines let go while `/status` held the screen, past what is kept.
    skipped: usize,
    any_history: bool,
    width: usize,
    live: Option<Live>,
    /// Whether the reply now ending arrived as deltas.
    streamed: bool,
    running: Option<Running>,
    calls: HashMap<String, (String, Value)>,
    /// Calls waiting on the person, as `approvals` lists them.
    approvals: Vec<Value>,
    /// The waiting call whose whole command was last written out.
    asked: Option<Value>,
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
    /// The reasoning streamed since the bot last spoke or called a tool,
    /// whose latest heading the status row shows, as Codex does.
    thought: String,
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
    /// The folder the next message moves the bot to: its lead moved away.
    move_to: Option<String>,
    /// The name under which Sonny notes the instructions it wrote.
    lead: String,
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
    layout: mux::Layout,
    client: Arc<Client>,
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
    /// Detached Errand runs the project's bots started, by bot and call
    /// (call ids are only unique within a bot): the command, until the
    /// call's output names the job.
    errands: HashMap<(String, String), String>,
    /// Finished calls whose output names a job to follow in a window:
    /// the bot, the output's node, and the command.
    jobs: Vec<(String, i64, String, String)>,
    /// Tasks whose last turn completed, and that turn: its window closes
    /// once a wait has taken the result.
    ended: HashMap<String, i64>,
    /// Calls running in the project that may wait on tasks, by bot and call.
    waits: std::collections::HashSet<(String, String)>,
    /// Finished calls that may have waited, to read for the turns they
    /// returned: the bot, the output's node, and the call.
    returned: Vec<(String, i64, String)>,
}

impl Fleet {
    async fn start(options: &Options, project: &str, lead: &str) -> Option<Self> {
        let layout = mux::here(project, options.rex_host.as_deref())?;
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
        let note = (!layout.here()).then(|| format!("tasks open in {}", layout.home()));
        mux::install_skill(options.rex_host.as_deref());
        let mut fleet = Self {
            layout,
            client: client.clone(),
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
            ended: HashMap::new(),
            waits: Default::default(),
            returned: Vec::new(),
        };
        fleet.layout.adopt_own(lead);
        fleet.doing(lead, "idle");
        fleet.reconcile().await;
        Some(fleet)
    }

    /// Tasks named in the project's way, and which are working, read
    /// afresh: at start, and after a reconnect whose missed events are not
    /// replayed. One that stopped meanwhile is marked idle, quietly.
    async fn reconcile(&mut self) {
        let Ok(page) = self
            .client
            .request(
                "bots",
                json!({"name": format!("{}.*", self.project), "limit": 256}),
            )
            .await
        else {
            return;
        };
        for bot in page["bots"].as_array().into_iter().flatten() {
            let Some(name) = bot["name"].as_str() else {
                continue;
            };
            self.lineage.insert(name.to_owned());
            let running = !bot["running_turn"].is_null();
            // A turn that ended while away is not known by number, and
            // one that started makes the last ending stale.
            self.ended.remove(name);
            if running && name != self.lead {
                let _ = self.show(name);
            } else if !running
                && self
                    .doing
                    .get(name)
                    .is_none_or(|doing| matches!(*doing, "working" | "needs you"))
            {
                self.doing.insert(name.to_owned(), "idle");
            }
        }
        self.name_windows();
    }

    fn show(&mut self, bot: &str) -> Result<bool, String> {
        let mut command = self.command.clone();
        command.extend(["--bot".into(), bot.into()]);
        let name = format!("{} · working", self.short(bot));
        let opened = self.layout.show(bot, &name, &command)?;
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
                    self.errands.insert((bot.to_owned(), call), command);
                }
            }
            "tool_completed" => {
                if let Some(command) = self.errands.remove(&(bot.to_owned(), call.clone()))
                    && let Some(node) = data["node"].as_i64()
                    && data["failed"] != true
                {
                    self.jobs.push((bot.to_owned(), node, call, command));
                }
            }
            _ => {}
        }
    }

    /// A finished task's window closes once its result is taken: its turn
    /// completed and a wait in the project (the lead's, or a task's on its
    /// own task) returned it. One that failed, stopped or needs the person
    /// stays open, and a task given more work opens again.
    fn taken(&mut self, bot: &str, kind: &str, event: &Value) {
        let data = &event["data"];
        let call = || {
            (
                bot.to_owned(),
                data["call_id"].as_str().unwrap_or("").to_owned(),
            )
        };
        match kind {
            "accepted" => {
                self.ended.remove(bot);
            }
            "turn_finished" => match (data["status"].as_str(), event["turn"].as_i64()) {
                (Some("completed"), Some(turn)) => {
                    self.ended.insert(bot.to_owned(), turn);
                }
                _ => {
                    self.ended.remove(bot);
                }
            },
            // Which turns a wait returned is read from what it printed,
            // once it ends: a wait that timed out, or never ran, returned
            // none. The arguments, cut short or not, only say it may wait.
            "tool_started" => {
                let may = match data["name"].as_str() {
                    Some("wait") => true,
                    Some("shell") => data["arguments"]
                        .as_str()
                        .is_some_and(|a| a.contains("wait")),
                    _ => false,
                };
                if may {
                    self.waits.insert(call());
                }
            }
            "tool_completed" => {
                if self.waits.remove(&call())
                    && let Some(node) = data["node"].as_i64()
                {
                    self.returned.push((bot.to_owned(), node, call().1));
                }
            }
            _ => {}
        }
    }

    /// Close the windows of the tasks whose completed turns the finished
    /// waits returned.
    async fn close_taken(&mut self) {
        for (bot, node, call) in std::mem::take(&mut self.returned) {
            let Ok(read) = self
                .client
                .request("history_items", json!({"bot": bot, "nodes": [node]}))
                .await
            else {
                continue;
            };
            let Some(result) = responses(&read["items"][0]["item"])
                .into_iter()
                .find(|item| item["call_id"] == call.as_str())
            else {
                continue;
            };
            let (printed, _) = output(&result);
            for (task, turn) in calls::returned(&printed) {
                if task != self.lead && self.ended.get(&task) == Some(&turn) {
                    self.ended.remove(&task);
                    self.layout.close(&task);
                    self.doing.remove(&task);
                    self.name_windows();
                }
            }
        }
    }

    /// A window for each Errand job started since: `errand attach` on its
    /// handle, named after its command.
    async fn follow_jobs(&mut self) {
        let Some(errand) = which("errand") else {
            self.jobs.clear();
            return;
        };
        for (bot, node, call, command) in std::mem::take(&mut self.jobs) {
            let Ok(read) = self
                .client
                .request("history_items", json!({"bot": bot, "nodes": [node]}))
                .await
            else {
                continue;
            };
            // An Anthropic node holds every result of its turn's calls.
            let Some(result) = responses(&read["items"][0]["item"])
                .into_iter()
                .find(|item| item["call_id"] == call.as_str())
            else {
                continue;
            };
            let (printed, _) = output(&result);
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
                .layout
                .place(&label, &name, &attach, &mux::Placement::Window);
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
        for (window, labels) in self.layout.windows() {
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
                self.layout.rename_window(&window, &name);
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
        self.client = client;
        self.events = events;
        self.down = false;
        self.reconcile().await;
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
            self.taken(bot, kind, event);
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
                Ok(true) if !self.layout.here() => {
                    Some(format!("{bot} is in {}", self.layout.home()))
                }
                Ok(_) => None,
                Err(error) => Some(error),
            },
            "deleted" => {
                self.layout.close(bot);
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
            context: None,
            input: String::new(),
            pending: Vec::new(),
            skipped: 0,
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
            asked: None,
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
            thought: String::new(),
            replay: None,
            options,
            down: false,
            repaint: false,
            instruct: None,
            move_to: None,
            lead: String::new(),
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
                    if let Some(fleet) = &mut self.fleet
                        && !fleet.returned.is_empty()
                    {
                        fleet.close_taken().await;
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
    /// Whether what was picked is now the bot's: false while it waits
    /// for the turn to end, or when the daemon refused it (the notice says
    /// why).
    async fn configure(&mut self) -> bool {
        if self.instruct.is_none() && self.switch.is_none() && self.gating.is_none() {
            return true;
        }
        if self.running.is_some() {
            return false;
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
                if let Some(text) = &text {
                    project::wrote(&self.lead, text);
                }
                if let Some((model, effort)) = switch {
                    // A lead's project file says what it runs, for the app
                    // and the next lead made there.
                    if self.project.is_some()
                        && let Err(error) = project::keep_model(
                            std::path::Path::new(&self.workspace),
                            &model,
                            effort.as_deref(),
                        )
                    {
                        self.notice = Some(format!("the project file: {error}"));
                    }
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
                return false;
            }
            Err(error) => {
                self.notice = Some(error.to_string());
                return false;
            }
        }
        true
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
            client
                .request("follow", json!({"bot": self.bot, "after": 0}))
                .await
                .map_err(|e| e.to_string())?;
            let context = caught_up(&mut fresh).await;
            // Read after the replay, so a turn that ends meanwhile arrives
            // as an event rather than lost between the two.
            let record = client
                .request("inspect", json!({"bot": self.bot}))
                .await
                .map_err(|e| e.to_string())?;
            Ok::<_, String>((client, fresh, record, context))
        };
        if self.down {
            let Ok((client, fresh, record, context)) = lead.await else {
                self.retry = Some(Instant::now() + Duration::from_secs(2));
                return;
            };
            self.client = client;
            *events = fresh;
            self.down = false;
            self.notice = None;
            self.context = context.or(self.context);
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
            // A daemon that restarted took the approver Sonny started with it.
            if self.mode == Some("auto")
                && let Err(error) = serve_auto(&self.client, &self.options, &self.model).await
            {
                self.notice = Some(error);
            }
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

    /// The items of `nodes`, in order and in the Responses family's shape.
    /// A read stops at a byte limit, so what it left is asked for again.
    async fn items(&self, nodes: &[i64]) -> Vec<Value> {
        let mut items = Vec::new();
        let mut rest = nodes;
        while !rest.is_empty() {
            let Ok(read) = self
                .client
                .request("history_items", json!({"bot": self.bot, "nodes": rest}))
                .await
            else {
                break;
            };
            let entries = read["items"].as_array().cloned().unwrap_or_default();
            if entries.is_empty() {
                break;
            }
            rest = &rest[entries.len().min(rest.len())..];
            items.extend(entries.iter().flat_map(|entry| responses(&entry["item"])));
        }
        items
    }

    /// Call `id`, its name and arguments, read from the bot's newest items.
    async fn call(&self, id: &str) -> Option<(String, Value)> {
        let page = self
            .client
            .request("history_nodes", json!({"bot": self.bot, "limit": 32}))
            .await
            .ok()?;
        let nodes: Vec<i64> = page["nodes"]
            .as_array()?
            .iter()
            .filter_map(|n| n["node"].as_i64())
            .collect();
        let items = self.items(&nodes).await;
        let call = items
            .iter()
            .find(|item| item["type"] == "function_call" && item["call_id"] == id)?;
        Some((
            call["name"].as_str()?.to_owned(),
            serde_json::from_str(call["arguments"].as_str()?).ok()?,
        ))
    }

    /// The bot's latest items, written as the transcript would have shown them.
    async fn history(&mut self) {
        self.history_after(0).await;
    }

    /// The bot's items after `after`, drawn as they would have been live:
    /// the newest [`HISTORY_ITEMS`] on attaching (`after` 0), and every one
    /// since on reconnecting.
    async fn history_after(&mut self, after: i64) {
        // On attach the newest page; on reconnecting, every page back to
        // the last node this window drew, so nothing missed is skipped.
        let mut nodes: Vec<i64> = Vec::new();
        let mut asked = json!({"bot": self.bot, "limit": HISTORY_ITEMS});
        if after > 0 {
            asked["min_node"] = json!(after + 1);
        }
        loop {
            let Ok(page) = self.client.request("history_nodes", asked.clone()).await else {
                return;
            };
            nodes.extend(
                page["nodes"]
                    .as_array()
                    .into_iter()
                    .flatten()
                    .filter_map(|n| n["node"].as_i64())
                    .filter(|node| *node > after),
            );
            if after == 0 || page["next_from"].is_null() {
                break;
            }
            asked["from"] = page["next_from"].clone();
        }
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
                        let reply = text(&item);
                        if !reply.is_empty() {
                            let lines = transcript::reply(&reply, self.width);
                            self.commit(lines);
                            self.last_reply = reply;
                        }
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
                        // A call this window kept before it lost the daemon
                        // may end in what it reads on reconnecting.
                        if let Some((name, arguments)) =
                            calls.remove(id).or_else(|| self.calls.remove(id))
                        {
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
        // A call still running finishes live, after the history.
        self.calls.extend(calls);
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
            let held = live.markdown.end(self.width, live.lines == 0);
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
        // arrive again here. A turn it showed starting still runs: only
        // its prompt is not drawn again.
        let drawn = data["node"]
            .as_i64()
            .is_some_and(|node| node <= self.seen_node);
        if drawn && matches!(kind, "steered" | "message" | "tool_completed") {
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
                let changed = |field: &str| {
                    event["changed"]
                        .as_array()
                        .is_some_and(|c| c.iter().any(|f| f == field))
                };
                // A pick still waiting was made against what has just
                // changed elsewhere: it is dropped, not laid over it.
                let mut dropped = Vec::new();
                if changed("model") && self.switch.take().is_some() {
                    dropped.push("model");
                }
                if changed("gates") && self.gating.take().is_some() {
                    dropped.push("approvals");
                }
                if changed("instructions") && self.instruct.take().is_some() {
                    dropped.push("instructions");
                }
                if !dropped.is_empty() {
                    self.notice = Some(format!(
                        "{} changed elsewhere; your pick was dropped",
                        dropped.join(" and ")
                    ));
                }
                if let Ok(record) = self
                    .client
                    .request("inspect", json!({"bot": self.bot}))
                    .await
                {
                    self.refresh(&record, changed("model"));
                }
            }
            "accepted" => {
                self.end_live();
                self.said.clear();
                self.thought.clear();
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
                if !drawn
                    && let Some(node) = data["node"].as_i64()
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
                self.thought.clear();
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
                self.thought.push_str(event["text"].as_str().unwrap_or(""));
                if let Some(running) = &mut self.running {
                    running.phase = heading(&self.thought).unwrap_or("Thinking").to_owned();
                }
            }
            "message" => {
                let item = match data["node"].as_i64() {
                    Some(node) => self.items(&[node]).await.into_iter().next(),
                    None => None,
                };
                // Reasoning and calls are kept as items too, each with its
                // event: only the bot's words end the reply streaming now,
                // or draw it when nothing streamed.
                if item
                    .as_ref()
                    .is_some_and(|item| item["role"] != "assistant")
                {
                    return;
                }
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
                self.thought.clear();
                self.end_live();
                let name = data["name"].as_str().unwrap_or("tool").to_owned();
                let id = data["call_id"].as_str().unwrap_or("");
                // A long call's event carries a preview; the call is kept whole.
                let arguments = match data["arguments_truncated"] == true {
                    true => self.call(id).await.map(|(_, arguments)| arguments),
                    false => data["arguments"]
                        .as_str()
                        .and_then(|a| serde_json::from_str(a).ok()),
                }
                .unwrap_or(Value::Null);
                if let Some(running) = &mut self.running {
                    running.phase = match (name.as_str(), arguments["command"].as_str()) {
                        ("shell", Some(command)) => format!("Running {}", first_line(command)),
                        _ => format!("Calling {name}"),
                    };
                }
                self.calls.insert(id.to_owned(), (name, arguments));
            }
            "tool_completed" => {
                let id = data["call_id"].as_str().unwrap_or("");
                // A call denied before it started has no start to have
                // kept it: it is read from the bot's items.
                let call = match self.calls.remove(id) {
                    Some(call) => Some(call),
                    None if data["denied"] == true => self.call(id).await,
                    None => None,
                };
                let Some((name, arguments)) = call else {
                    return;
                };
                let mut shown = (String::new(), data["failed"] == true);
                if let Some(node) = data["node"].as_i64()
                    && let Some(item) = self
                        .items(&[node])
                        .await
                        .iter()
                        .find(|i| i["call_id"] == id)
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
                if let Some(size) = context_after(&event) {
                    self.context = Some(size);
                }
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
            .map(|fleet| fleet.layout.places())
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
        // A long command is listed cut: the person answers the whole of it,
        // read from the node that planned it, or is told it could not be.
        for call in &mut self.approvals {
            let whole = call["arguments"].is_object()
                && call["arguments_cut"].as_array().is_none_or(Vec::is_empty)
                && call["arguments_omitted"].as_u64().unwrap_or(0) == 0;
            if whole {
                continue;
            }
            let whole = match self
                .client
                .request(
                    "history_items",
                    json!({"bot": self.bot, "nodes": [call["node"]]}),
                )
                .await
            {
                Ok(read) => agent_client::approver::call_arguments(
                    &read["items"][0]["item"],
                    call["call_id"].as_str().unwrap_or_default(),
                ),
                Err(_) => None,
            };
            match whole {
                Some(arguments) => call["arguments"] = arguments,
                None => call["incomplete"] = json!(true),
            }
        }
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
        // A round with more calls waiting than one listing holds shows
        // the rest once these are answered.
        if self.approvals.is_empty() {
            self.approvals_for(call["turn"].as_i64()).await;
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
                                self.notice = Some("applies when idle".into());
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
                                self.notice = Some("applies when idle".into());
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
            // Terminals send a pasted line break as a carriage return.
            Event::Paste(text) => self
                .input
                .push_str(&text.replace("\r\n", "\n").replace('\r', "\n")),
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
                        // A call whose whole arguments could not be read
                        // can only be denied.
                        if c == 'y' && self.approvals[0]["incomplete"] == true {
                            self.notice =
                                Some("only part of this call could be read; n denies it".into());
                        } else {
                            self.answer(if c == 'y' { "allow" } else { "deny" }).await;
                        }
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
        if prompt == "/instructions" {
            self.input.clear();
            if self.project.is_none() {
                self.notice = Some("only a folder's lead takes its folder's instructions".into());
                return;
            }
            match project::instructions(std::path::Path::new(&self.workspace)) {
                Ok(text) => {
                    self.instruct = Some(text);
                    if self.configure().await {
                        self.notice = Some("the lead has its folder's instructions".into());
                    } else if self.instruct.is_some() {
                        self.notice = Some("applies when idle".into());
                    }
                }
                Err(error) => self.notice = Some(format!("the lead's instructions: {error}")),
            }
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
                "sandfs" => "tasks get sandfs copies (experimental)".into(),
                _ => "tasks get git worktrees".into(),
            });
            // The lead is told now when idle, else once its turn ends,
            // unless its instructions were set some other way.
            if self.project.is_some() {
                let bot = match self
                    .client
                    .request("inspect", json!({"bot": self.bot}))
                    .await
                {
                    Ok(bot) => bot,
                    Err(error) => {
                        self.notice = Some(format!("the lead's instructions: {error}"));
                        return;
                    }
                };
                let dir = std::path::Path::new(&self.workspace);
                match project::review(dir, &bot, &self.lead, false) {
                    Ok(project::Instructions::Stale(text)) => {
                        self.instruct = Some(text);
                        self.configure().await;
                    }
                    // A toggle back before the turn ended: nothing to send.
                    Ok(project::Instructions::Current) => self.instruct = None,
                    Ok(project::Instructions::Kept) => {
                        self.notice = Some(
                            "the lead keeps instructions set outside its folder's files; \
                             /instructions takes the folder's"
                                .into(),
                        )
                    }
                    Err(error) => self.notice = Some(format!("the lead's instructions: {error}")),
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
        // A steer never moves a bot, so a moved lead comes back with the
        // next message that starts a turn.
        let workspace = delivery.is_none().then(|| self.move_to.clone()).flatten();
        let submitted = self
            .client
            .request(
                "submit",
                json!({"bot": self.bot, "request_id": unique("tui"), "prompt": prompt,
                    "delivery": delivery, "model": model, "workspace": workspace}),
            )
            .await;
        match submitted {
            Ok(_) => {
                self.input.clear();
                self.notice = None;
                if workspace.is_some() {
                    self.move_to = None;
                }
            }
            Err(error) => self.notice = Some(error.to_string()),
        }
    }

    /// The call waiting on the person, written out whole above the
    /// status row before `y` can allow it: the row shows only its start.
    fn show_asked(&mut self) {
        let Some(call) = self.approvals.first() else {
            self.asked = None;
            return;
        };
        if self.asked.as_ref() == Some(&call["call_id"]) {
            return;
        }
        self.asked = Some(call["call_id"].clone());
        let name = call["name"].as_str().unwrap_or("tool").to_owned();
        let arguments = &call["arguments"];
        let what = arguments["command"]
            .as_str()
            .or_else(|| arguments["path"].as_str())
            .map(str::to_owned)
            .unwrap_or_else(|| arguments.to_string());
        let mut lines = vec![Line::from(vec![
            Span::raw("⏸ ").fg(Color::Yellow),
            Span::raw(format!("Allow {name}?")).bold(),
        ])];
        lines.extend(what.lines().map(|line| Line::from(format!("  {line}"))));
        if call["incomplete"] == true {
            lines.push(
                Line::from("  … cut; the rest could not be read, so y is off: n denies it").dim(),
            );
        }
        self.commit(lines);
    }

    fn draw(&mut self, terminal: &mut Tui) -> std::io::Result<()> {
        self.show_asked();
        // The pane may have changed size since these lines were wrapped:
        // the viewport takes the new size first, and a line still too wide
        // is cut into rows rather than losing its end.
        if let Some(watch) = &mut self.watch {
            // History waits for the main screen, its newest lines kept.
            if let Some(over) = self.pending.len().checked_sub(KEPT_UNDER_STATUS)
                && over > 0
            {
                self.pending.drain(..over);
                self.skipped += over;
            }
            return draw_watch(watch);
        }
        if self.skipped > 0 {
            let note = format!(
                "… {} earlier lines went by while /status was open",
                std::mem::take(&mut self.skipped)
            );
            self.pending.insert(0, Line::from(note).dim());
        }
        if std::mem::take(&mut self.redraw) {
            terminal.clear()?;
        }
        terminal.autoresize()?;
        let width = terminal.size()?.width as usize;
        self.width = width;
        let mut composer_rows = self.composer(width);
        // A short pane keeps the prompt's end, where typing goes on.
        let room = (terminal.size()?.height as usize).saturating_sub(VIEWPORT as usize - 1);
        let over = composer_rows.len().saturating_sub(room.max(1));
        composer_rows.drain(..over);
        fit(terminal, VIEWPORT - 1 + composer_rows.len() as u16)?;
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
            // A row count fits a u16 only so far.
            for rows in lines.chunks(4096) {
                terminal.insert_before(rows.len() as u16, |buf| {
                    Paragraph::new(rows.to_vec()).render(buf.area, buf);
                })?;
            }
        }
        terminal.draw(|frame| {
            let [live, status, _, composer, footer] = areas(frame.area());
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
                let [_, _, _, composer_row, _] = areas(frame.area());
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
            // The prompt's end, where typing goes on.
            let cursor = match composer_rows.last() {
                Some(row) if !self.input.is_empty() => row.width(),
                _ => 2,
            };
            let last = composer.y + composer.height.saturating_sub(1);
            frame.render_widget(Paragraph::new(composer_rows), composer);
            frame.set_cursor_position((
                composer.x + (cursor as u16).min(composer.width.saturating_sub(1)),
                last,
            ));
            frame.render_widget(Paragraph::new(self.footer_line(width)), footer);
        })?;
        Ok(())
    }

    /// The composer's rows: the prompt wrapped to the width, the way it
    /// will read once sent, and only its end when it is long; or the hint.
    /// A list or picker open over the composer keeps it to one row.
    fn composer(&self, width: usize) -> Vec<Line<'static>> {
        if self.input.is_empty() {
            let hint = match self.running {
                Some(_) => "Steer the running turn",
                None => "Ask anything",
            };
            return vec![Line::from(vec![
                Span::raw("› ").bold(),
                Span::raw(hint).dim(),
            ])];
        }
        let mut rows = Vec::new();
        for (at, line) in self.input.split('\n').enumerate() {
            let lead = match at {
                0 => Span::raw("› ").bold(),
                _ => Span::raw("  "),
            };
            rows.extend(transcript::wrap(
                Line::from(line.to_owned()),
                width.saturating_sub(1),
                lead,
                Span::raw("  "),
            ));
        }
        if self.choosing.is_some() || self.picker.is_some() {
            rows.drain(..rows.len() - 1);
        }
        let over = rows.len().saturating_sub(COMPOSER_ROWS);
        rows.drain(..over);
        rows
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
        let folder = tilde(&self.workspace);
        let mut left = format!("  {}", self.model);
        if let Some(effort) = &self.effort {
            left.push_str(&format!(" {effort}"));
        }
        // What waits for the turn to end, so a pick is not taken as lost.
        if let Some((model, effort)) = &self.switch {
            left.push_str(&format!(" → {model}"));
            if let Some(effort) = effort {
                left.push_str(&format!(" {effort}"));
            }
            left.push_str(" when idle");
        }
        if let Some(mode) = self.gating {
            left.push_str(&format!(" · {mode} approvals when idle"));
        }
        // The context, not what the bot ever spent: how full the next
        // round is.
        let right = match self.context {
            Some(size) => format!("{} · {} context  ", self.bot, transcript::tokens(size)),
            None => format!("{}  ", self.bot),
        };
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
    let failed = item["failed"] == true;
    match serde_json::from_str::<Value>(&raw) {
        Ok(shell) if shell.get("exit_code").is_some() => {
            let mut shown = shell["stdout"].as_str().unwrap_or("").to_owned();
            shown.push_str(shell["stderr"].as_str().unwrap_or(""));
            (shown, failed || shell["success"] == false)
        }
        _ => (raw, failed),
    }
}

/// An item as the Responses family shapes it, which the transcript reads.
/// An Anthropic message's `tool_use` blocks become function calls after
/// its text, and a user message's `tool_result` blocks their outputs.
fn responses(item: &Value) -> Vec<Value> {
    let Some(parts) = item["content"].as_array() else {
        return Vec::from_iter((!item.is_null()).then(|| item.clone()));
    };
    let tool = |part: &&Value| matches!(part["type"].as_str(), Some("tool_use" | "tool_result"));
    if !parts.iter().any(|part| tool(&part)) {
        return vec![item.clone()];
    }
    let said: Vec<&Value> = parts.iter().filter(|part| part["type"] == "text").collect();
    let mut items = Vec::new();
    if item["role"] == "assistant" || !said.is_empty() {
        items.push(json!({"role": item["role"], "content": said}));
    }
    for part in parts.iter().filter(tool) {
        items.push(match part["type"].as_str() {
            Some("tool_use") => json!({"type": "function_call", "call_id": part["id"],
                "name": part["name"], "arguments": part["input"].to_string()}),
            _ => json!({"type": "function_call_output", "call_id": part["tool_use_id"],
                "output": match &part["content"] {
                    Value::String(output) => output.clone(),
                    content => text(&json!({"content": content})),
                },
                "failed": part["is_error"] == true}),
        });
    }
    items
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A daemon that greets and answers each `history_items` read from
    /// `items`, by node.
    async fn daemon_with(items: Value) -> Arc<Client> {
        use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
        let dir = std::env::temp_dir().join(format!(
            "sonny-daemon-{}-{}",
            std::process::id(),
            unique("t")
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let socket = dir.join("agent.sock");
        let listener = tokio::net::UnixListener::bind(&socket).unwrap();
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
                let read: Vec<Value> = asked["nodes"]
                    .as_array()
                    .into_iter()
                    .flatten()
                    .map(|node| json!({"node": node, "item": items[node.to_string()]}))
                    .collect();
                let answer = json!({"id": asked["id"], "result": {"items": read}});
                write
                    .write_all(format!("{answer}\n").as_bytes())
                    .await
                    .unwrap();
            }
        });
        Client::connect(&socket).await.unwrap().0
    }

    #[tokio::test]
    async fn a_reply_that_follows_its_reasoning_is_drawn_once() {
        let client = daemon_with(json!({
            "1": {"type": "reasoning", "summary": [{"type": "summary_text", "text": "**Reading**"}]},
            "2": {"type": "message", "role": "assistant",
                  "content": [{"type": "output_text", "text": "First line.\nSecond line."}]}
        }))
        .await;
        let options = Options {
            store: PathBuf::new(),
            socket: PathBuf::new(),
            bot: None,
            new: false,
            model: None,
            workspace: None,
            layout: false,
            rex_host: None,
            sound: false,
            lookout: None,
        };
        let mut app = App::new(client, "b", &json!({"name": "b"}), options);
        let event =
            |event: &str, data: Value| json!({"event": event, "bot": "b", "turn": 1, "data": data});
        app.daemon(event("accepted", json!({}))).await;
        for (kind, text) in [
            ("thinking_delta", "**Reading** the"),
            ("thinking_delta", " folder"),
            ("text_delta", "First line.\n"),
            ("text_delta", "Second line."),
        ] {
            let mut delta = event(kind, json!({}));
            delta["text"] = json!(text);
            app.daemon(delta).await;
        }
        // The reasoning is kept, and announced, before the words it led to.
        app.daemon(event("message", json!({"node": 1}))).await;
        app.daemon(event("message", json!({"node": 2}))).await;
        let drawn: Vec<String> = app.pending.iter().map(|line| line.to_string()).collect();
        let said = |text: &str| drawn.iter().filter(|line| line.contains(text)).count();
        assert_eq!(
            (said("First line."), said("Second line.")),
            (1, 1),
            "{drawn:?}"
        );
    }

    #[test]
    fn the_status_row_names_what_the_model_is_thinking_about() {
        assert_eq!(heading(""), None);
        assert_eq!(heading("**Checking the"), None);
        assert_eq!(
            heading("**Checking the tests**\n\nI"),
            Some("Checking the tests")
        );
        let two = "**Checking the tests**\n\nThey pass.\n\n**Reading the README";
        assert_eq!(heading(two), Some("Checking the tests"));
        assert_eq!(heading(&format!("{two}**\n")), Some("Reading the README"));
    }

    #[test]
    fn the_footer_shows_the_context_not_what_was_ever_spent() {
        let usage = |input: u64, output: u64, purpose: Option<&str>| {
            json!({"event": "usage", "bot": "b", "turn": 1,
                "data": {"input_tokens": input, "output_tokens": output,
                    "cached_input_tokens": input / 2, "purpose": purpose}})
        };
        // Three rounds of one turn: each reads the whole conversation again.
        assert_eq!(context_after(&usage(40_000, 900, None)), Some(40_900));
        assert_eq!(context_after(&usage(41_000, 2_000, None)), Some(43_000));
        // A compaction's own round reads the old window to summarize it.
        assert_eq!(
            context_after(&usage(120_000, 3_000, Some("compaction"))),
            None
        );
        assert_eq!(context_after(&json!({"event": "text_delta"})), None);
        // A fallback billed two attempts; the context is the last one's.
        let mut fell_back = usage(20, 10, None);
        fell_back["data"]["models"] = json!([
            {"model": "claude-opus-5-5", "input_tokens": 7, "output_tokens": 3},
            {"model": "claude-opus-4-8", "input_tokens": 13, "output_tokens": 7},
        ]);
        assert_eq!(context_after(&fell_back), Some(20));
    }

    #[test]
    fn anthropic_tool_blocks_read_as_calls_and_outputs() {
        let asked = json!({"role": "assistant", "content": [
            {"type": "thinking", "thinking": "hm", "signature": "s"},
            {"type": "text", "text": "Running it."},
            {"type": "tool_use", "id": "t1", "name": "shell", "input": {"command": "ls"}}]});
        let items = responses(&asked);
        assert_eq!(text(&items[0]), "Running it.");
        assert_eq!(items[1]["type"], "function_call");
        assert_eq!(items[1]["call_id"], "t1");
        assert_eq!(items[1]["arguments"], r#"{"command":"ls"}"#);
        let answered = json!({"role": "user", "content": [
            {"type": "tool_result", "tool_use_id": "t1", "is_error": true,
             "content": [{"type": "text", "text": "no such file"}]}]});
        let items = responses(&answered);
        assert_eq!(items.len(), 1);
        assert_eq!(items[0]["call_id"], "t1");
        assert_eq!(output(&items[0]), ("no such file".into(), true));
        // Responses items, and plain messages, pass through.
        let call = json!({"type": "function_call", "call_id": "c"});
        assert_eq!(responses(&call), [call]);
        assert!(responses(&Value::Null).is_empty());
    }
}
