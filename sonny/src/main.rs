//! Sonny, a terminal client for one bot. It owns the whole screen: the
//! conversation is kept in [`screen::Screen`] above a few rows that hold
//! what is still moving (the reply being written, what the bot is doing,
//! and the composer), and a pane beside it shows what the person opens.
//! Prototype.
//!
//! The look follows Codex's TUI (openai/codex, codex-rs/tui): replies
//! stream a line at a time, tool calls are one `• Ran …` line with a
//! short `└` look at the result, and a status row says how long the turn
//! has run and how to stop it.
mod approvals;
mod asks;
mod attach;
mod background;
mod banner;
mod cache;
mod calls;
mod cmux;
mod declared;
mod diff;
mod embed;
mod exec;
mod herdr;
mod highlight;
mod html;
mod image;
mod js;
mod links;
mod login;
mod lookout;
#[cfg(target_os = "macos")]
mod mac;
mod margin;
mod math;
mod mcp;
mod mermaid;
mod models;
mod mux;
mod pane;
mod place;
mod plan;
mod project;
mod prompt;
mod reactions;
mod resolved;
mod rex;
mod screen;
mod seed;
mod served;
mod settings;
mod setup;
mod shell;
mod skills;
mod sound;
mod started;
mod status;
mod table;
mod task;
mod term;
#[cfg(test)]
mod testing;
mod transcript;
mod tree;
mod trigger;
mod user_shell;

use agent_client::{Client, Events};
use ratatui::{
    Terminal,
    backend::{Backend, CrosstermBackend},
    crossterm::{
        event::{
            self, DisableBracketedPaste, DisableMouseCapture, EnableBracketedPaste,
            EnableMouseCapture, Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers,
        },
        execute,
        terminal::{EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode},
    },
    layout::{Constraint, Layout, Rect},
    style::{Color, Modifier, Style, Stylize},
    text::{Line, Span},
    widgets::Paragraph,
};
use serde_json::{Value, json};
use std::{
    collections::{BTreeMap, HashMap},
    io::Write,
    path::PathBuf,
    sync::Arc,
    time::{Duration, Instant},
};
use transcript::Markdown;
use unicode_width::UnicodeWidthStr;

/// Rows kept at the bottom: the reply's unfinished line, the status, a gap,
/// the composer and the footer.
const VIEWPORT: u16 = 5;
/// The most rows a long prompt's composer grows to; it shows its end.
const COMPOSER_ROWS: usize = 8;
/// How many of the bot's latest replies `/react` offers.
const REACTABLE: usize = 9;
/// How much of a reply's first line a reaction quotes.
const QUOTED: usize = 72;

/// Draws in a row that may fail before the client gives up on its
/// terminal: each failed cursor query waits up to 2 s, so about a minute.
const DRAW_ATTEMPTS: u32 = 30;
/// How much history is shown on attach.
const HISTORY_ITEMS: u64 = 200;
/// The widest the conversation is written, however wide the window: a
/// reply reads as a column, not a band across the screen. Prose reads
/// best at 45 to 90 characters a line, but replies carry code, paths and
/// tables too, which most style guides keep within 100 (rustfmt's own
/// width), so they stay whole at 100 and prose stays near the measure.
const PROSE: usize = 100;
/// How long a side chat's copy stays once idle.
const SIDE_EXPIRE: Duration = Duration::from_secs(15 * 60);
/// This window is a side chat (`--side`).
static SIDE: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);
/// Into the alternate screen with the mouse, which Sonny owns while it
/// runs, or out of it.
fn own_screen(own: bool) {
    let _ = match own {
        true => execute!(std::io::stdout(), EnterAlternateScreen, EnableMouseCapture),
        false => execute!(std::io::stdout(), DisableMouseCapture, LeaveAlternateScreen),
    };
}

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
    /// A side chat, opened by `/side`: the node its copy was taken at,
    /// after which its history is drawn.
    side: Option<i64>,
    /// Drawn in another Sonny's pane: the terminal's tab is that Sonny's,
    /// so this one claims nothing of it.
    embedded: bool,
}

fn usage() -> String {
    "usage: sonny [--store PATH] [--socket PATH] [--model PROVIDER/MODEL] [--no-layout] [--rex-host HOST] [--no-sound]\n\
     \x20      [--bot NAME [--new [--workspace DIR]]]\n\
     With no --bot, the folder's coordinator, made with --model the first time; its tasks\n\
     open beside it in Rex or Herdr, each in a git worktree of its own. Type /status for\n\
     the project's agents.\n\
     Agents start tasks, put windows and panes where the person looks, make projects\n\
     and subscribe to the lookout with tools Sonny gives them, which the lookout answers.\n\
     sonny memory ... and sonny trigger ... are what ~/.agent/memory and ~/.agent/trigger\n\
     run, which Sonny writes at each start.\n\
     sonny lookout [--judge PROVIDER/MODEL|off] watches every lead on the daemon, wakes\n\
     it when its tasks need it and answers its agents' calls of Sonny's tools; Sonny\n\
     starts one. sonny lookout --replay --judge SPEC[,SPEC]... [--limit N] runs its\n\
     checks over what the store kept.\n\
     sonny agent ARGS... runs Sonny's own `agent` with ARGS, whatever `agent` is on PATH."
        .into()
}

/// A `show_agent` call of `caller`'s: put `bot`'s window where the person
/// asked. A task gets a window of its own unless told otherwise; `split`
/// puts it beside the caller's block, or `beside` another bot's. Another
/// project's lead gets its project's own home instead (a Rex session, a
/// Herdr workspace), running that lead's Sonny, unless it is there already.
async fn show(
    caller: &str,
    bot: &str,
    split: Option<String>,
    beside: Option<String>,
) -> Result<Value, String> {
    if let Some(side) = split.as_deref().filter(|side| !mux::SIDES.contains(side)) {
        return Err(format!(
            "{side:?} is not a side: right, left, above or below"
        ));
    }
    let (socket, project) = agents_project(caller).await?;
    let me = std::env::current_exe().map_err(|e| e.to_string())?;
    if split.is_none()
        && let Some(other) = bot.strip_suffix(".lead").filter(|p| *p != project)
    {
        let store = std::env::var_os("AGENT_STORE").map(PathBuf::from);
        let (home, opened) = open_lead(&socket, store.as_deref(), other, &project, &me)?;
        return Ok(json!({"bot": bot, "in": home, "opened": opened}));
    }
    let mut layout = mux::for_agent(&socket, &project, None)?;
    let command = [
        me.to_string_lossy().into_owned(),
        "--socket".into(),
        socket.to_string_lossy().into_owned(),
        "--no-sound".into(),
        "--bot".into(),
        bot.to_owned(),
    ];
    let short = bot.strip_prefix(&format!("{project}.")).unwrap_or(bot);
    let placement = match split {
        Some(side) => mux::Placement::Split {
            side,
            beside: beside.unwrap_or_else(|| caller.to_owned()),
        },
        None => mux::Placement::Window,
    };
    layout.place(bot, short, &command, &placement)?;
    // The lead's Sonny opens a window for a task whose turn starts, maybe
    // while this placed it: what it opened then goes.
    tokio::time::sleep(Duration::from_millis(1500)).await;
    layout.close_others(bot);
    Ok(json!({"bot": bot, "in": layout.home()}))
}

/// Open `project`'s lead in that project's own home, made when needed in
/// the multiplexer `like`'s home is in, running Sonny on it: a session for
/// each lead, and in it a window for each of its tasks. A lead whose Sonny
/// is there already is left as it is. Returns the home, and whether it
/// was opened now.
fn open_lead(
    socket: &std::path::Path,
    store: Option<&std::path::Path>,
    project: &str,
    like: &str,
    me: &std::path::Path,
) -> Result<(String, bool), String> {
    let mut layout = mux::for_agent(socket, project, Some(like))?;
    let bot = format!("{project}.lead");
    let mut command = vec![me.to_string_lossy().into_owned()];
    if let Some(store) = store {
        command.extend(["--store".into(), store.to_string_lossy().into_owned()]);
    }
    command.extend([
        "--socket".into(),
        socket.to_string_lossy().into_owned(),
        "--bot".into(),
        bot.clone(),
    ]);
    let opened = layout.show(&bot, "lead", &command)?;
    Ok((layout.home(), opened))
}

/// The daemon's socket and the project `bot` belongs to, for a call of
/// `bot`'s.
async fn agents_project(bot: &str) -> Result<(PathBuf, String), String> {
    let socket = std::env::var_os("AGENT_SOCKET")
        .map(PathBuf::from)
        .ok_or("AGENT_SOCKET is not set")?;
    let socket = std::path::absolute(socket).map_err(|e| e.to_string())?;
    let client = Client::connect(&socket).await.map_err(|e| e.to_string())?.0;
    let project = project_of(&client, bot).await;
    Ok((socket, project))
}

/// A `run_in_pane` call of `owner`'s: run a command it wants watched (a
/// dev server, a long build) in `cwd`, in a block labelled `OWNER/NAME`,
/// in a tab of its own, or with `split` on that side of the owner's own
/// block, and say the block's ID. It closes with the owner's window.
async fn run_in_pane(
    owner: &str,
    name: &str,
    cwd: &std::path::Path,
    command: &str,
    split: Option<String>,
) -> Result<Value, String> {
    if name.is_empty() || name.contains('/') {
        return Err("a name without /".into());
    }
    if let Some(side) = split.as_deref().filter(|side| !mux::SIDES.contains(side)) {
        return Err(format!(
            "{side:?} is not a side: right, left, above or below"
        ));
    }
    if !cwd.is_absolute() || !cwd.is_dir() {
        return Err(format!("{} is not a folder", cwd.display()));
    }
    let (socket, project) = agents_project(owner).await?;
    let mut layout = mux::for_agent(&socket, &project, None)?;
    let command = ["sh".to_owned(), "-c".to_owned(), command.to_owned()];
    let block = layout.split(owner, name, cwd, &command, split.as_deref())?;
    Ok(json!({"block": block, "label": format!("{owner}/{name}"), "in": layout.home()}))
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

/// Agent's own binary: the one the daemon gave this shell, else the one
/// installed beside Sonny (`libexec/agent`, as the Linux tarball lays it
/// out), else Homebrew's `agent` formula when Sonny is its `sonny`. Not
/// `agent` on PATH, which may be another program.
fn agent_bin() -> Option<PathBuf> {
    let me = std::env::current_exe().and_then(std::fs::canonicalize).ok();
    std::env::var_os("AGENT_BIN")
        .map(PathBuf::from)
        .into_iter()
        .chain(me.iter().flat_map(|me| installed_agent(me)))
        .find(|bin| bin.is_file())
}

/// Where Agent's binary is installed with Sonny at `me`: beside it, and,
/// when Sonny is in Homebrew's Cellar, by the `agent` formula's `opt`
/// path, which outlasts the versions upgrades remove. The daemon hands
/// that path to its agents, so it must outlast the daemon's version.
fn installed_agent(me: &std::path::Path) -> Vec<PathBuf> {
    let Some(keg) = me.parent().and_then(std::path::Path::parent) else {
        return Vec::new();
    };
    let mut found = vec![keg.join("libexec/agent")];
    if let Some(cellar) = keg
        .parent()
        .filter(|formula| formula.ends_with("Cellar/sonny"))
        && let Some(prefix) = cellar.parent().and_then(std::path::Path::parent)
    {
        found.push(prefix.join("opt/agent/bin/agent"));
    }
    found
}

/// `sonny agent ARGS...`: the `agent` Sonny itself starts, run with
/// ARGS and the same keys, in place of this process, so the person reaches
/// it whatever else is called `agent` on their PATH.
fn pass_to_agent(args: &[String]) -> Result<(), String> {
    let bin = agent_bin().ok_or("Agent's binary was not found; set AGENT_BIN")?;
    let mut command = agent_command(&bin)?;
    let error = std::os::unix::process::CommandExt::exec(command.args(args));
    Err(format!("{}: {error}", bin.display()))
}

/// Agent's binary to run, with the provider keys kept in ~/.agent/env
/// over this shell's own; an empty value unsets one.
fn agent_command(bin: &std::path::Path) -> Result<std::process::Command, String> {
    let mut command = std::process::Command::new(bin);
    apply_env_file(&mut command)?;
    Ok(command)
}

/// `~/.agent/env` over `command`'s environment: an empty value unsets.
fn apply_env_file(command: &mut std::process::Command) -> Result<(), String> {
    if let Some(file) = agent_client::env_file::path() {
        for (key, value) in agent_client::env_file::read(&file)? {
            match value.is_empty() {
                true => command.env_remove(key),
                false => command.env(key, value),
            };
        }
    }
    Ok(())
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
    agent_client::own_session(&mut command);
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

/// The lookout, `sonny lookout`, which wakes leads when their tasks need
/// them and answers Sonny's tools: started detached, logging beside the
/// store. One already watching this daemon keeps it, and the new one
/// exits.
fn start_lookout(options: &Options) -> Result<(), String> {
    let me = std::env::current_exe().map_err(|e| e.to_string())?;
    let mut log = options.store.clone().into_os_string();
    log.push(".lookout.log");
    let log = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&log)
        .map_err(|e| e.to_string())?;
    // The judge's settings and keys are in Agent's env file, as for the
    // approver.
    let mut command = agent_command(&me)?;
    agent_client::own_session(&mut command);
    let mut child = command
        .arg("lookout")
        .arg("--store")
        .arg(&options.store)
        .arg("--socket")
        .arg(&options.socket)
        .stdin(std::process::Stdio::null())
        .stdout(log.try_clone().map_err(|e| e.to_string())?)
        .stderr(log)
        .spawn()
        .map_err(|e| format!("{}: {e}", me.display()))?;
    // Reaped when it exits, as it does at once when another one watches.
    std::thread::spawn(move || child.wait());
    Ok(())
}

/// The daemon, started the way the CLI starts it when it is not running.
/// With no provider set anywhere, the person is asked for one first, and a
/// first model list is written once it runs.
async fn connect(options: &Options) -> Result<(Arc<Client>, agent_client::Events), String> {
    if let Ok(connected) = Client::connect(&options.socket).await {
        return Ok(connected);
    }
    match start_daemon(&options.store, &options.socket, false) {
        Err(failed) if failed.code.as_deref() == Some("provider_required") => {
            if !setup::interactive() {
                return Err(format!("{failed}; run sonny in a terminal to choose one"));
            }
            let bin = agent_bin().ok_or("Agent's binary was not found; set AGENT_BIN")?;
            setup::provider(&bin)?;
            start_daemon(&options.store, &options.socket, false).map_err(|e| e.to_string())?;
        }
        started => started.map_err(|e| e.to_string())?,
    }
    let connected = Client::connect(&options.socket)
        .await
        .map_err(|e| e.to_string())?;
    if let Some(path) = agent_client::models::path().filter(|path| !path.exists()) {
        tokio::spawn(first_models(connected.0.clone(), path));
    }
    Ok(connected)
}

/// `~/.agent/models`, the list agents read with `agent models`, written
/// from the providers' listings when there is none yet, as `agent models
/// --discover` writes it. A provider that will not list leaves it for the
/// next start.
async fn first_models(client: Arc<Client>, path: PathBuf) {
    let Ok(listing) = client.request("provider_models", json!({})).await else {
        return;
    };
    if let Ok(text) = agent_client::models::render(&listing, &[]) {
        let _ = agent_client::models::install(&path, &text);
    }
}

/// Why the daemon did not start: the CLI's error code when it named one,
/// and what it said.
pub struct NotStarted {
    pub code: Option<String>,
    reason: String,
}

impl std::fmt::Display for NotStarted {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "could not start the daemon: {}", self.reason)
    }
}

impl From<String> for NotStarted {
    fn from(reason: String) -> Self {
        NotStarted { code: None, reason }
    }
}

/// Start the daemon of `store` on `socket` the way the CLI starts it, and
/// wait for it to be ready. With `login`, it gets the login shell's
/// environment, for a start from launchd, which gives no provider keys.
fn start_daemon(
    store: &std::path::Path,
    socket: &std::path::Path,
    login: bool,
) -> Result<(), NotStarted> {
    let bin = agent_bin().ok_or(
        "the daemon is not running and Agent's binary was not found; install Sonny with Homebrew or its release tarball, or set AGENT_BIN".to_owned(),
    )?;
    let mut command = std::process::Command::new(&bin);
    if let Some(environment) = login.then(login::environment).flatten() {
        command.env_clear().envs(environment);
    }
    apply_env_file(&mut command)?;
    let out = command
        .arg("start")
        .arg("--store")
        .arg(store)
        .arg("--socket")
        .arg(socket)
        .stdin(std::process::Stdio::null())
        .output()
        .map_err(|e| format!("{}: {e}", bin.display()))?;
    if !out.status.success() {
        let stderr = String::from_utf8_lossy(&out.stderr);
        let failure = stderr
            .lines()
            .rev()
            .find_map(|line| serde_json::from_str::<Value>(line).ok());
        let field = |name: &str| Some(failure.as_ref()?[name].as_str()?.to_owned());
        return Err(NotStarted {
            code: field("error"),
            reason: field("detail").unwrap_or_else(|| stderr.trim().to_owned()),
        });
    }
    Ok(())
}

/// The store and socket given, else the environment's, else Agent's own.
fn daemon_paths(
    store: Option<PathBuf>,
    socket: Option<PathBuf>,
) -> Result<(PathBuf, PathBuf), String> {
    let store = match store.or_else(|| std::env::var_os("AGENT_STORE").map(PathBuf::from)) {
        Some(store) => store,
        None => PathBuf::from(std::env::var_os("HOME").ok_or("set --store or HOME")?)
            .join(".agent/state.sqlite"),
    };
    // One store under two names is one lookout's.
    let store = agent_client::socket::canonical_store(&store).map_err(|e| e.to_string())?;
    let socket = match socket.or_else(|| std::env::var_os("AGENT_SOCKET").map(PathBuf::from)) {
        Some(socket) => socket,
        None => agent_client::socket::default_socket(&store).map_err(|e| e.to_string())?,
    };
    Ok((store, socket))
}

fn parse() -> Result<Options, String> {
    let mut args = std::env::args().skip(1);
    let (mut store, mut socket, mut bot, mut new, mut model, mut workspace) =
        (None, None, None, false, None, None);
    let (mut layout, mut rex_host, mut sound) = (true, None, true);
    let (mut side, mut embedded) = (None, false);
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
            // Unlisted: how `/side` opens a side chat.
            "--side" => side = Some(value()?.parse().map_err(|_| usage())?),
            // Unlisted: how a task opens in the pane.
            "--embedded" => embedded = true,
            _ => return Err(usage()),
        }
    }
    let (store, socket) = daemon_paths(store, socket)?;
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
        side,
        embedded,
    })
}

/// What runs this Sonny again on the same bot, from the same folder: `sonny`
/// with its own arguments, less those that made the bot or changed a
/// setting, and with the daemon the environment named.
fn reopen() -> Vec<String> {
    let mut command = vec!["sonny".to_owned()];
    let mut args = std::env::args().skip(1);
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--new" => {}
            "--workspace" => {
                args.next();
            }
            _ => command.push(arg),
        }
    }
    for (flag, var) in [("--store", "AGENT_STORE"), ("--socket", "AGENT_SOCKET")] {
        if !command.iter().any(|a| a == flag)
            && let Some(value) = std::env::var_os(var).filter(|v| !v.is_empty())
        {
            command.extend([flag.to_owned(), value.to_string_lossy().into_owned()]);
        }
    }
    command
}

// Two threads, so the daemon's events are read while a picture is drawn:
// a reader that stops is dropped by the daemon as lagging.
fn main() -> std::process::ExitCode {
    // Every agent Sonny makes, and every agent those make through `sonny
    // agent`, is told to reach the daemon this one way, by a path that
    // outlives an upgrade.
    if let Some(me) = skills::sonny_path() {
        // SAFETY: nothing else runs yet: no thread is started before this.
        unsafe { std::env::set_var("AGENT_CLI", format!("\"{}\" agent", me.display())) };
    }
    let args: Vec<String> = std::env::args().skip(1).collect();
    let result = match args.split_first() {
        // The lookout runs this for each call of a tool Sonny answers.
        Some((tool, rest)) if tool == "tool" => served::answer_here(rest),
        // `~/.agent/memory` runs this, from agents' shells.
        Some((verb, rest)) if verb == "memory" => {
            return std::process::ExitCode::from(agent_client::memory::cli(rest) as u8);
        }
        Some((verb, rest)) if verb == trigger::VERB => {
            return std::process::ExitCode::from(trigger::cli(rest) as u8);
        }
        // launchd runs this for a trigger's fire.
        Some((flag, rest)) if flag == trigger::FIRE_FLAG => {
            return std::process::ExitCode::from(trigger::fire_cli(rest) as u8);
        }
        Some((lookout, rest)) if lookout == "lookout" => lookout::run(rest),
        Some((verb, rest)) if verb == "mcp" => mcp::cli(rest),
        Some((agent, rest)) if agent == "agent" => pass_to_agent(rest),
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
    if std::env::args().nth(1).as_deref() == Some("--version") {
        println!("sonny {}", env!("CARGO_PKG_VERSION"));
        return Ok(());
    }
    let options = parse()?;
    SIDE.store(options.side.is_some(), std::sync::atomic::Ordering::Relaxed);
    // Before any lead is made, so its instructions list the skills.
    skills::install_own();
    trigger::at_start();
    // Fonts are read before the first picture needs them.
    std::thread::spawn(image::preload);
    // So are the syntaxes, before the first code is colored.
    std::thread::spawn(highlight::preload);
    let (client, mut events) = connect(&options).await?;
    // A lead made just now starts in the first approval mode.
    let mut fresh = false;
    // A lead moved elsewhere, and the folder it comes back to.
    let mut moving = None;
    // A lead made below another project's folder, and that project's lead.
    let mut above = None;
    let (name, project) = match &options.bot {
        Some(bot) => {
            if options.new {
                create(&client, &options, bot).await?;
            }
            // Sonny on a project's lead is that lead's Sonny, wherever it
            // was opened, as by `show_agent` in the project's own home.
            (bot.clone(), bot.strip_suffix(".lead").map(str::to_owned))
        }
        None => {
            if options.new {
                return Err(usage());
            }
            let dir = std::env::current_dir().map_err(|e| e.to_string())?;
            // Sonny no longer writes an `errand` skill: one it wrote, unedited, goes.
            skills::remove("errand");
            if options.layout {
                mux::install_skill(options.rex_host.as_deref());
            }
            let lead = project::open(&client, &dir, options.model.as_deref()).await?;
            fresh = lead.new;
            moving = lead.moving;
            above = lead.above;
            (lead.bot, Some(lead.project))
        }
    };
    // A side chat borrows its agent's pane, and Herdr keeps the agent; a
    // task in the pane is in its lead's.
    if options.side.is_none() && !options.embedded {
        herdr::announce(&name, reopen());
        cmux::announce(&name, reopen());
    }
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
    own_screen(true);
    let mut terminal =
        Terminal::new(CrosstermBackend::new(std::io::stdout())).map_err(|e| e.to_string())?;
    // A side chat opens on its window's alternate screen, still drawn on.
    let _ = terminal.clear();
    let mut app = App::new(client, &name, &bot, options.clone());
    app.context = context;
    app.turn_began().await;
    if options.side.is_some() {
        let source = name.rsplit_once(".side-").map_or(name.as_str(), |(s, _)| s);
        app.notice = Some(format!(
            "a copy of {source} that answers without acting; {source} carries on"
        ));
    }
    // The person's agent can react to them; a side chat calls no tool.
    if options.side.is_none() {
        app.declare = declared::declare(&bot, project.is_some());
    }
    app.sounds = sound::Sounds::new(options.sound);
    let settings = settings::Settings::load();
    app.sounds.chime = settings.chime;
    app.verbose = settings.verbose;
    // A bot keeps the model it was made with; --model runs this window's
    // turns on another.
    if let Some(model) = options.model.as_ref().filter(|m| **m != app.model) {
        app.model = model.clone();
        app.turn_model = Some(model.clone());
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
    if let Some(above) = above
        && app.notice.is_none()
    {
        app.notice = Some(format!(
            "a new lead for this folder; {above} leads the folder above: sonny --bot {above}"
        ));
    }
    app.lead = project::lead(&app.options.socket, &bot);
    // The lookout knows no leads: it watches a lead's tasks because Sonny
    // subscribes it to them, before the lookout starts so it reads the file.
    if let Some(project) = &project
        && let Err(error) = lookout::subscribe_lead(&app.options.store, &name, project)
    {
        app.notice = Some(format!("the lookout: {error}"));
    }
    // It answers the tools Sonny gives every agent, a lead or not.
    if let Err(error) = start_lookout(&app.options) {
        app.notice = Some(error);
    }
    // The lead told what its folder tells it today (skills and memory added
    // since it was made), unless its instructions were set some other way.
    // After the lookout's notice: a skill the lead misses matters more.
    if project.is_some()
        && let Some(dir) = moving
            .as_deref()
            .or(bot["workspace"].as_str().map(std::path::Path::new))
    {
        match project::review(dir, &bot, &app.lead, fresh) {
            Ok(project::Instructions::Stale(text)) => app.instruct = Some(text),
            Ok(project::Instructions::Current) => {}
            Ok(project::Instructions::Kept { missing }) => {
                app.notice = Some(project::Instructions::kept_notice(&missing))
            }
            Err(error) => app.notice = Some(format!("the lead's instructions: {error}")),
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
    // A lead made before Sonny confined leads, or whose sandbox is not its
    // mode's in the folder it now works in, takes its mode's again. Only
    // ever confined this way: a lead is unconfined by the person's pick.
    if project.is_some()
        && app.gating.is_none()
        && let Some(mode) = app.mode.filter(|mode| *mode != "full")
        && !approvals::sandboxed_as(
            &bot,
            &approvals::sandbox(mode, true, std::path::Path::new(&app.workspace)),
        )
    {
        app.gating = Some(mode);
    }
    app.project = project;
    // Sonny's welcome heads a lead's conversation, though not a side chat.
    app.welcome = options.side.is_none();
    if fresh {
        app.gating = Some(approvals::MODES[0].0);
    } else if app.mode == Some("auto")
        && let Err(error) = serve_auto(&app.client, &app.options, &app.model).await
    {
        app.notice = Some(error);
    }
    let result = app.main(&mut terminal, &mut events).await;
    let _ = terminal.clear();
    own_screen(false);
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
        .map_or_else(
            || agent_client::config::load().map(|c| c.model),
            |m| Ok(Some(m)),
        )?
        .ok_or(
            "--new needs --model PROVIDER/MODEL, AGENT_MODEL, or model in ~/.agent/config.toml",
        )?;
    let workspace = match &options.workspace {
        Some(dir) => std::path::absolute(dir).map_err(|e| e.to_string())?,
        None => std::env::current_dir().map_err(|e| e.to_string())?,
    };
    let memory = agent_client::policy::memory_indexes(&workspace).map_err(|e| e.to_string())?;
    let composed =
        agent_client::policy::instructions(&workspace, None, &memory).map_err(|e| e.to_string())?;
    let instructions = project::as_task(&workspace, &composed.text)?;
    let declare = json!(declared::tools(false));
    let config = agent_client::config::load()?;
    let tools = agent_client::default_tools(client.providers(), &model, config.web_search);
    let mut asked = json!({"bot": bot, "workspace": workspace, "model": model,
        "instructions": instructions, "tools": tools, "declare": declare,
        "compaction_instructions": agent_client::policy::DEFAULT_COMPACTION_INSTRUCTIONS,
        "request_id": unique("tui-create")});
    // Confined from the start, as the mode it is made with confines it.
    let mut settings = approvals::sandbox(approvals::MODES[0].0, true, &workspace);
    if let Some(ttl) = agent_client::policy::cache_ttl(&model) {
        settings.insert("cache_ttl".into(), json!(ttl));
    }
    if config.hide_senders {
        settings.insert("hide_senders".into(), json!(true));
    }
    asked["settings"] = Value::Object(settings);
    let tools: Vec<String> = tools.into_iter().map(String::from).collect();
    let tools = approvals::gateable(&tools, &declare);
    if let Value::Object(fields) = &mut asked {
        fields.extend(approvals::gate(approvals::MODES[0].0, &tools));
    }
    client
        .request("create", asked)
        .await
        .map_err(|e| e.to_string())?;
    Ok(())
}

/// A link, file or page, opened as the system opens it.
fn open(target: &str) -> Result<(), String> {
    let opener = if cfg!(target_os = "macos") {
        "open"
    } else {
        "xdg-open"
    };
    std::process::Command::new(opener)
        .arg(target)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
        .map(drop)
        .map_err(|error| format!("{opener}: {error}"))
}

/// The viewport's rows, top to bottom: the reply's open line, the status,
/// a gap, the composer (as many rows as are left) and the footer.
/// The live area is `live` rows: the reply's open line, wrapped.
fn areas(area: ratatui::layout::Rect, live: u16) -> [ratatui::layout::Rect; 5] {
    Layout::vertical([
        Constraint::Length(live.max(1)),
        Constraint::Length(1),
        Constraint::Length(1),
        Constraint::Min(1),
        Constraint::Length(1),
    ])
    .areas(area)
}

/// At most `keep` of `rows`, as near the end as still shows `row`; and
/// where `row` is among them.
fn window(mut rows: Vec<Line<'static>>, row: usize, keep: usize) -> (Vec<Line<'static>>, usize) {
    let start = rows.len().saturating_sub(keep).min(row);
    rows.drain(..start);
    rows.truncate(keep);
    (rows, row - start)
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
/// How often the footer reads the workspace's branch again.
const PLACE_EVERY: Duration = Duration::from_secs(3);

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
/// (Ghostty, Rex, cmux) can mark this pane working, blocked or done; and
/// the same to Herdr, which takes it from its own CLI instead.
fn status_report(state: &str, bot: &str) {
    use base64::Engine;
    // A side chat borrows its agent's pane, which keeps saying what the
    // agent itself is doing.
    if SIDE.load(std::sync::atomic::Ordering::Relaxed) {
        return;
    }
    herdr::report(state);
    if state == "clear" {
        cmux::release();
    }
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

/// What the bot is saying: its rows stay in the viewport until what comes
/// next tells whether they were a step on the way, before a call, or its
/// reply, at the turn's end. Rows past what the viewport holds are written
/// as they end, as a reply.
struct Live {
    markdown: Markdown,
    partial: String,
    lines: usize,
    /// The ended rows still in the viewport.
    rows: Vec<Line<'static>>,
    /// A blank row goes above them, apart from the history before.
    gap: bool,
    /// Rows were written already, too many to hold.
    spilled: bool,
    /// Every row has ended: the words wait only for what follows them.
    closed: bool,
    /// The node the words were kept at, once their message came.
    node: Option<i64>,
}

impl Live {
    fn new(gap: bool, workspace: &str) -> Self {
        Live {
            markdown: Markdown::in_folder(std::path::Path::new(workspace)),
            partial: String::new(),
            lines: 0,
            rows: Vec::new(),
            gap,
            spilled: false,
            closed: false,
            node: None,
        }
    }
}

/// What the bot's words turned out to be once something followed them.
#[derive(Clone, Copy, PartialEq)]
enum Words {
    /// A step: a call came next.
    Step,
    /// The turn's reply, or words nothing told apart.
    Reply,
}

/// A call the approver denied: the call, and why.
#[derive(Debug, Clone, PartialEq)]
struct Denial {
    call_id: String,
    name: String,
    arguments: Value,
    reason: String,
}

/// Calls alike, drawn as one: what they did, whom each did it to, and
/// each one's own rows, shown under it while open.
struct Fold {
    verb: String,
    whats: Vec<String>,
    calls: Vec<Vec<Line<'static>>>,
    open: bool,
}

/// The button that opens or shuts the message between agents at `node`.
fn said_button(node: i64) -> screen::Mark {
    screen::Mark::Link(format!("{}said/{node}", screen::BUTTON))
}

/// A message one bot sent another, drawn quiet until a click opens it.
struct Relayed {
    /// What its shut row says, as [`transcript::relayed_head`] has it.
    head: String,
    from: Option<String>,
    /// The whole message while it is open, read again from the history
    /// when it opens, so a long conversation keeps none of them.
    text: Option<String>,
}

/// Ctrl+\, which moves the keys between the conversation and the pane:
/// no terminal or multiplexer takes it, as many take Shift+Tab. Without
/// the keyboard protocol it arrives as Ctrl+4.
fn switches(key: &KeyEvent) -> bool {
    key.modifiers.contains(KeyModifiers::CONTROL) && matches!(key.code, KeyCode::Char('\\' | '4'))
}

/// Whether `key` steps the pane back (Ctrl+[) or forward (Ctrl+]), as a
/// browser's keys do. Without the keyboard protocol Ctrl+[ is Esc, and
/// Ctrl+] arrives as Ctrl+5.
fn steps(key: &KeyEvent) -> Option<bool> {
    if !key.modifiers.contains(KeyModifiers::CONTROL) {
        return None;
    }
    match key.code {
        KeyCode::Char('[') => Some(true),
        KeyCode::Char(']' | '5') => Some(false),
        _ => None,
    }
}

/// The most denials `/approve` offers.
const DENIALS: usize = 8;

/// What the person sends to approve one retry of a denied call: their own
/// words, so the judge reads the consent, as Codex words it.
fn approval(denial: &Denial) -> String {
    format!(
        "I approve one retry of the action auto-review denied: {} {}\n\n\
         Treat this as approval to perform that exact action in the same context in which it \
         was originally requested. Do not assume this also authorizes similar operations with \
         different payloads.",
        denial.name, denial.arguments
    )
}

/// The most rows of a denied call `/approve` writes out for the person to
/// read; a longer one is approved in words.
const RETRY_ROWS: usize = 500;

/// A denied call written out whole before its retry is approved: a
/// command as it would run with every other argument after it, anything
/// else as its arguments.
fn retry_shown(denial: &Denial) -> Vec<Line<'static>> {
    let mut lines = vec![Line::from(
        Span::raw(format!("Approve one retry of {}?", denial.name)).bold(),
    )];
    let what = match (denial.arguments["command"].as_str(), &denial.arguments) {
        (Some(command), Value::Object(fields)) => {
            let mut what = command.to_owned();
            for (key, value) in fields.iter().filter(|(key, _)| *key != "command") {
                what.push_str(&format!("\n{key}: {value}"));
            }
            what
        }
        _ => serde_json::to_string_pretty(&denial.arguments).unwrap_or_default(),
    };
    lines.extend(what.lines().map(|line| Line::from(format!("  {line}"))));
    lines
}

/// What the status row says while the approver reviews calls.
fn reviewing(count: usize) -> String {
    match count {
        1 => "Reviewing approval request".into(),
        n => format!("Reviewing {n} approval requests"),
    }
}

/// Why a call was denied, when its output says it was: the reason given,
/// empty when there was none.
fn denial(item: &Value) -> Option<String> {
    let output: Value = serde_json::from_str(item["output"].as_str()?).ok()?;
    (output["error"] == "approval_denied")
        .then(|| output["detail"].as_str().unwrap_or("").to_owned())
}

struct Running {
    turn: i64,
    started: Instant,
    /// What the status row says the bot is doing.
    phase: String,
    /// The lookout's note started it: it ends quietly unless the bot
    /// says something.
    note: bool,
}

/// Whether the lookout sent the turn: its note, by its request ID.
fn from_lookout(data: &Value) -> bool {
    (data["request_id"].as_str()).is_some_and(|id| id.starts_with("lookout-"))
}

/// The most of the person's refused commands kept for their next
/// message, well under what the daemon takes in one.
const UNSENT_BYTES: usize = 64 * 1024;

/// How the daemon ends a turn whose model round came back with nothing.
const EMPTY: &str = "empty_completion";

/// What sends a command's record: what it printed is not the person's
/// words, so it authorizes nothing the agent then does.
const SHELL: &str = "shell";

/// Keeps a refused command's record for the next message, letting the
/// oldest go past what one message should carry.
fn hold(unsent: &mut Vec<String>, record: String) {
    unsent.push(record);
    while unsent.len() > 1 && unsent.iter().map(String::len).sum::<usize>() > UNSENT_BYTES {
        unsent.remove(0);
    }
}

/// The slash commands, as the menu lists them: name and what it does.
/// `/agents` and `/ps` also open `/status`, and `/triggers` and `/lookout`
/// open `/background`, unlisted.
const COMMANDS: [(&str, &str); 16] = [
    (
        "/side",
        "a side chat with a copy of this agent, which carries on",
    ),
    ("/model", "the model and effort"),
    (
        "/compact",
        "summarize the lead's conversation before its next call",
    ),
    ("/approvals", "whether calls wait for a verdict"),
    (
        "/approve",
        "approve one retry of a recent auto-review denial",
    ),
    ("/instructions", "the lead's, from its folder again"),
    ("/status", "this project's agents"),
    (
        "/margin",
        "the agent's notes beside the conversation, shown or closed",
    ),
    (
        "/background",
        "triggers and lookout subscriptions: fire or remove them",
    ),
    ("/mcp", "the MCP servers agents' scripts call"),
    ("/sound", "the chime when work ends, on or off"),
    (
        "/verbose",
        "every tool call, or only what moves the work on",
    ),
    ("/react", "react to one of the agent's replies"),
    ("/copy", "the last reply, as Markdown"),
    ("/open", "the last HTML page, in your browser"),
    ("/quit", "the agents keep working"),
];

/// What an agent's last few items say, for `/status`.
/// What an idle agent's history says, read once per turn it ended: the
/// question it has open, and whether its last turn resolved it.
#[derive(Clone, Default)]
struct Seen {
    ask: Option<String>,
    itself: bool,
}

/// Whether `items`, one turn of `bot`'s, hold a `resolve_task` call of its
/// own that resolved itself and was not refused.
fn resolved_itself(items: &[Value], bot: &str) -> bool {
    items.iter().any(|call| {
        call["type"] == "function_call"
            && call["name"] == resolved::TOOL
            && resolved::itself(
                &call["arguments"]
                    .as_str()
                    .and_then(|a| serde_json::from_str(a).ok())
                    .unwrap_or(Value::Null),
                bot,
            )
            && items.iter().any(|done| {
                done["type"] == "function_call_output"
                    && done["call_id"] == call["call_id"]
                    && !output(done).1
                    && !reactions::refused(done)
            })
    })
}

#[derive(Default)]
struct Latest {
    /// Its latest step, as a line.
    step: String,
    /// The pull request its latest reply links.
    pr: Option<String>,
    /// The question it asked the person and has no answer to yet.
    ask: Option<String>,
}

/// `/status` in the pane, its timers ticking and its rows read again
/// every two seconds, until Esc.
struct Watch {
    project: String,
    rows: Vec<status::Row>,
    /// When the rows were read: their times count on from here.
    read: Instant,
    /// The lookout's line for the lead.
    lookout: String,
    /// The resolved tasks are listed, not only counted.
    resolved: bool,
    /// `/background` holds the screen instead of the card.
    background: Option<background::View>,
    /// The row of `/background`'s picked item as the pane last drew it,
    /// so a new pick scrolls to it and scrolling otherwise stays put.
    picked: Option<usize>,
}

struct App {
    client: Arc<Client>,
    bot: String,
    /// The project whose lead this is, when opened as the folder's coordinator.
    project: Option<String>,
    workspace: String,
    /// The workspace's branch, as last read, for the footer.
    place: place::Place,
    /// The read under way, of the workspace named, and when the last one
    /// began: the branch can change under a running turn.
    place_job: Option<(String, tokio::task::JoinHandle<place::Place>)>,
    place_read: Option<(String, Instant)>,
    model: String,
    effort: Option<String>,
    /// How big the conversation was after its last model round, as the
    /// next round reads it, once known.
    context: Option<u64>,
    input: prompt::Prompt,
    /// Lines waiting to be written into the conversation, and what marks
    /// the rows of some, by their place among them.
    pending: Vec<Line<'static>>,
    marks: Vec<(usize, screen::Mark)>,
    any_history: bool,
    /// Sonny's welcome heads the transcript once it is drawn back to the
    /// conversation's first item, as Claude Code's does.
    welcome: bool,
    width: usize,
    /// The terminal's rows, which bound how much of the bot's words is held.
    height: usize,
    live: Option<Live>,
    /// Whether the reply now ending arrived as deltas.
    streamed: bool,
    running: Option<Running>,
    calls: HashMap<String, (String, Value)>,
    /// Calls waiting on the person, as `approvals` lists them.
    approvals: Vec<Value>,
    /// The lead's open questions for the person, oldest first: its `ask`
    /// calls not yet answered.
    asks: Vec<asks::Ask>,
    /// Questions this window answered and closed, until their answers
    /// land, so landing does not close another asked the same.
    sent_answers: Vec<String>,
    /// Where the person's reactions to the bot are kept, once its id is
    /// known.
    reactions_key: Option<String>,
    /// Esc set the questions aside: what is typed is a message of its own.
    chatting: bool,
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
    /// What `/status` read of each idle agent's history: its last turn
    /// then, and the question left open.
    asks_seen: HashMap<String, (i64, Seen)>,
    sounds: sound::Sounds,
    /// The reply text the current model call has streamed.
    said: String,
    /// Prompts sent and not in the conversation yet, by their turn: a
    /// steer until the running turn takes it in, a prompt until its turn
    /// starts. Drawn above the status row until they land.
    staged: Vec<(i64, String)>,
    /// Pictures given with the message being written, each behind its
    /// placeholder in the composer: [`attach::placeholder`] of its place
    /// here, from 1.
    /// Pictures attached to the draft, by their placeholders' numbers; one
    /// neither the draft nor Ctrl-Y can bring back is let go.
    pictures: Vec<Option<attach::Picture>>,
    /// The person's reactions to this bot's replies.
    reactions: reactions::Reactions,
    /// The bot's latest replies, by node, with their first lines: what
    /// `/react` offers, newest last.
    replies: Vec<(i64, String)>,
    /// `/react`'s list is open, at this reply from the newest.
    reacting: Option<usize>,
    /// What Ctrl+F looks for on the full screen, while it does.
    finding: Option<String>,
    /// The reply `/react` picked, which the next emoji sent answers.
    react_to: Option<(i64, String)>,
    /// The turn of the last reaction sent, whose attaching says nothing.
    reaction_turn: Option<i64>,
    /// Messages attached to the bot's next call, by their turns, until a
    /// call takes them in: the footer's clip.
    attached: Vec<i64>,
    /// The tools to declare on the bot so it can react, sent once no turn
    /// runs.
    declare: Option<Value>,
    /// The bot's reactions in the running turn so far, drawn when it ends.
    bot_reactions: Vec<String>,
    /// The bot's `react` calls not yet completed, by call id.
    react_calls: HashMap<String, (String, bool)>,
    /// The first line of the person's message the bot answers, and the
    /// rows drawn when it was: a reaction with none drawn since goes right
    /// under it.
    answering: Option<(String, usize, i64)>,
    /// Rows written into the history so far.
    rows_drawn: usize,
    /// The reasoning streamed since the bot last spoke or called a tool,
    /// whose latest heading the status row shows, as Codex does.
    thought: String,
    /// What a retried call will stream again, not to be shown twice.
    replay: Option<String>,
    /// How to reach the daemon again when the connection drops.
    options: Options,
    /// The lead's connection dropped.
    down: bool,
    /// The background switched, or the view did: the conversation is
    /// drawn again once idle.
    repaint: bool,
    /// The oldest node this window drew, which a repaint draws back to.
    first_node: Option<i64>,
    /// The node before it, while the bot has older ones.
    older: Option<i64>,
    /// The full screen was scrolled past its first row: older items are
    /// read, and the view held where it was while all is drawn again.
    want_older: bool,
    hold_view: bool,
    /// The window's width, the pane's included.
    full_width: u16,
    /// The pane beside the conversation on the full screen, with what the
    /// person opened to look at; whether keys go to it; and whether the
    /// mouse button went down in it, so a drag stays in it.
    pane: Option<pane::Pane>,
    pane_keys: bool,
    pane_mouse: bool,
    /// The pane closed last, with its `/status` or `/background`, which
    /// Ctrl+] opens again.
    closed: Option<(pane::Pane, Option<Watch>)>,
    /// The agent's notes beside the conversation; the margin's pane while
    /// something opened is laid over it; a note to scroll the margin to
    /// once it is drawn, one just put there or clicked in the conversation.
    margin: margin::Margin,
    under: Option<pane::Pane>,
    margin_focus: Option<u64>,
    /// Notes drawn in the conversation where they were put, while the
    /// margin is closed or the window has no room for it: by pin, each
    /// note's id and title; whether the margin was beside the
    /// conversation and how often it had changed when they were last
    /// drawn so, and whether a note put since waits to be.
    inline: HashMap<u64, (String, String)>,
    folded: Option<(bool, u64)>,
    refold: bool,
    /// The `plan` calls drawn, by the number each is tagged with, and the
    /// one drawn in full: the newest, where the margin cannot show it.
    plans: HashMap<u64, Vec<String>>,
    plan_count: u64,
    plan_full: Option<u64>,
    /// Where a note was put, to show in the conversation once it is drawn
    /// back that far; and how many more older pages may be read for it.
    jump: Option<(screen::Tag, u8)>,
    /// Buttons pressed whose work waits for the loop: a note the person
    /// took away, to tell the agent; a view to open.
    pressed: Vec<String>,
    /// A program in the pane drew something: the screen is drawn again.
    woken: Arc<tokio::sync::Notify>,
    /// Every call drawn, rather than the clean view of the bot's words and
    /// the calls that move the work on.
    verbose: bool,
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
    /// The tools its client declared, of which a mode gates Sonny's that
    /// can drop work or run outside the sandbox.
    declared: Value,
    /// Its approval mode, when it is one of Sonny's.
    mode: Option<&'static str>,
    /// `/approvals`, open over the composer: the mode picked.
    choosing: Option<usize>,
    /// Calls the approver serving `auto` is reviewing in this turn.
    reviewing: std::collections::HashSet<String>,
    /// Calls it denied while Sonny watched, newest last: what `/approve`
    /// offers to retry once.
    denials: Vec<Denial>,
    /// `/approve`, open over the composer: the denial picked.
    approving: Option<usize>,
    /// The denied call written out whole for the person to read, which a
    /// second Enter then approves.
    approve_shown: Option<String>,
    /// The mode picked, set once no turn runs.
    gating: Option<&'static str>,
    /// The mode picked, still to give the agents the lead made, and when
    /// to try next: one busy now takes it once idle.
    spread: Option<(&'static str, Instant)>,
    /// The newest reply's text, for `/copy`.
    last_reply: String,
    /// A `!command` of the person's, running.
    shell: Option<user_shell::Running>,
    /// What the person's commands printed, to show once the bot's words
    /// allow.
    shown: Vec<Line<'static>>,
    /// The records of those the daemon refused, to go with the next message.
    unsent: Vec<String>,
    /// Those whose output this window showed, from when their record is
    /// sent until it lands.
    ran_here: Vec<user_shell::Ran>,
    /// The conversation's rows.
    screen: screen::Screen,
    /// Calls drawn folded into one, by number; and the one the last call
    /// went into, with the rows drawn by then, which the next call like
    /// it joins while nothing else was drawn.
    folds: BTreeMap<usize, Fold>,
    folding: Option<(usize, usize)>,
    /// Words between agents drawn quiet, by node: what was said, who
    /// said it, and whether a click opened it.
    relayed: HashMap<i64, Relayed>,
    /// The tasks the lead started, by the group they were started in; the
    /// group the last went into, with the rows drawn by then, as a fold
    /// has; and when what they do was last read, or
    /// none for all of them to be read now.
    tasks: Vec<Vec<started::Task>>,
    starting: Option<(usize, usize)>,
    tasks_read: Option<Instant>,
    /// When every task started, ended ones too, was last read.
    tasks_all_read: Option<Instant>,
    /// The width the tasks started were drawn at.
    tasks_width: usize,
    /// How many triggers and subscriptions run for this agent, as the
    /// footer's `↻` says, and when they were last counted.
    behind: (usize, Option<Instant>),
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
    /// Other projects whose leads were seen: their bots are theirs, never
    /// tasks here, and their Sonny opens only when asked for.
    leads: std::collections::HashSet<String>,
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
    /// Tasks whose last turn completed, and that turn: its window closes
    /// once a wait has taken the result.
    ended: HashMap<String, i64>,
    /// Calls running in the project that may wait on tasks, by bot and call.
    waits: std::collections::HashSet<(String, String)>,
    /// Finished calls that may have waited, to read for the turns they
    /// returned: the bot, the output's node, and the call.
    returned: Vec<(String, i64, String)>,
    /// Tasks' turns that resolved them: their window closes once the turn
    /// completes.
    resolving: resolved::Own,
}

/// The projects inside `project` whose leads are among `names`:
/// `shop.web` for `shop.web.lead` in `shop`.
fn inner_projects<'a>(names: impl Iterator<Item = &'a str>, project: &str) -> Vec<String> {
    let lead = format!("{project}.lead");
    names
        .filter(|name| *name != lead)
        .filter_map(|name| name.strip_suffix(".lead"))
        .filter(|inner| inner.starts_with(&format!("{project}.")))
        .map(str::to_owned)
        .collect()
}

/// What a bot made just now is to a lead's Sonny.
#[derive(Debug, PartialEq)]
enum Kin {
    /// One of its tasks: a window here.
    Task,
    /// Another project's lead: a home of its own, opened when asked for.
    Lead(String),
    None,
}

/// What `bot`, made by `by`, is to the lead of `project` whose lineage
/// made the bots in `lineage`, with `leads` the other projects seen: any
/// other project's lead but one this project is inside, by its name alone;
/// else a task when its name is the project's or one of the lineage made
/// it, unless it is one of theirs.
fn kin(
    bot: &str,
    by: &str,
    project: &str,
    lineage: &std::collections::HashSet<String>,
    leads: &std::collections::HashSet<String>,
) -> Kin {
    let lead = format!("{project}.lead");
    let theirs = leads.iter().any(|p| bot.starts_with(&format!("{p}.")));
    // A project this one is inside names all of this one's bots too.
    let outer = |p: &str| project.starts_with(&format!("{p}."));
    match bot.strip_suffix(".lead").filter(|_| bot != lead) {
        Some(other) if outer(other) => Kin::None,
        Some(other) => Kin::Lead(other.to_owned()),
        None if !theirs && (lineage.contains(by) || bot.starts_with(&format!("{project}."))) => {
            Kin::Task
        }
        _ => Kin::None,
    }
}

impl Fleet {
    async fn start(options: &Options, project: &str, lead: &str) -> Option<Self> {
        let layout = mux::here(&options.socket, project, options.rex_host.as_deref())?;
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
            leads: Default::default(),
            command,
            note,
            doing: HashMap::new(),
            named: HashMap::new(),
            alerts: Vec::new(),
            ended: HashMap::new(),
            waits: Default::default(),
            returned: Vec::new(),
            resolving: Default::default(),
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
        let bots = page["bots"].as_array().cloned().unwrap_or_default();
        // A project inside this one has a lead, and a home, of its own: its
        // lead opens there and its tasks are its own.
        let names = bots.iter().filter_map(|bot| bot["name"].as_str());
        self.leads.extend(inner_projects(names, &self.project));
        for bot in &bots {
            // Side chats are never tasks, as when they are made.
            let Some(name) = bot["name"].as_str().filter(|_| bot["expire_ms"].is_null()) else {
                continue;
            };
            let running = !bot["running_turn"].is_null();
            if (self.leads.iter()).any(|p| name.starts_with(&format!("{p}."))) {
                continue;
            }
            self.lineage.insert(name.to_owned());
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

    /// A task resolved, by the lead or itself: its window closes, unless
    /// new work has it at work again.
    fn resolved(&mut self, task: &str) {
        if self.lineage.contains(task)
            && !matches!(self.doing.get(task), Some(&"working" | &"needs you"))
        {
            self.ended.remove(task);
            self.layout.close(task);
            self.doing.remove(task);
            self.name_windows();
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

    /// The bot as the person tells it apart from every other project's:
    /// the lead by its project, as `shop lead`, a task by its own name.
    fn named(&self, bot: &str) -> String {
        match bot == self.lead {
            true => format!("{} lead", self.project),
            false => self.short(bot).to_owned(),
        }
    }

    /// Note what the bot is doing, and name the windows after it.
    fn doing(&mut self, bot: &str, doing: &'static str) {
        if self.doing.get(bot) != Some(&doing) {
            self.doing.insert(bot.to_owned(), doing);
            self.name_windows();
            let short = self.short(bot).to_owned();
            let alert = match doing {
                // The lead's view rings for its own ends: only it knows
                // whether a turn the lookout started said anything.
                "done" if bot == self.lead => None,
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
    /// that are working or need the person: `shop lead · done · 2 working`. A
    /// window that also holds the person's own blocks, as when Sonny was
    /// opened beside them, keeps the name they gave it.
    fn name_windows(&mut self) {
        for (window, labels) in self.layout.windows() {
            if !labels.iter().all(|label| self.owns(label)) {
                continue;
            }
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
            let mut name = format!("{} · {}", self.named(first), self.doing[first]);
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

    /// Whether a block is the project's: a bot's, or a command one asked
    /// for (`BOT/NAME`).
    fn owns(&self, label: &str) -> bool {
        let bot = label.split_once('/').map_or(label, |(bot, _)| bot);
        self.doing.contains_key(bot) || self.lineage.contains(bot)
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
            self.taken(bot, kind, event);
            self.resolving.event(event, &self.lead);
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
                // The judge handed a call to the person.
                "approval_asked" => self.doing(bot, "needs you"),
                "tool_completed" | "turn_resumed" if self.doing.get(bot) == Some(&"needs you") => {
                    self.doing(bot, "working")
                }
                "turn_finished" => {
                    self.doing(
                        bot,
                        match data["status"].as_str() {
                            Some("completed" | "steered") => "done",
                            Some("failed") if data["error"] == EMPTY => "done",
                            Some("interrupted") => "stopped",
                            _ => "failed",
                        },
                    );
                    // A task that resolved itself in the turn it finished.
                    if let Some(turn) = event["turn"].as_i64()
                        && self.resolving.ended(bot, turn)
                        && data["status"] == "completed"
                    {
                        self.resolved(bot);
                    }
                }
                _ => {}
            }
        }
        match kind {
            // A copy that goes once idle is a side chat, not a task.
            "created" | "forked" if event["data"]["expire_ms"].is_null() => {
                let by = event["data"]["created_by"].as_str().unwrap_or("");
                match kin(bot, by, &self.project, &self.lineage, &self.leads) {
                    Kin::Task => {
                        self.lineage.insert(bot.to_owned());
                    }
                    Kin::Lead(project) => {
                        self.leads.insert(project);
                    }
                    Kin::None => {}
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
            place: place::Place::default(),
            place_job: None,
            place_read: None,
            model,
            effort: record["effort"].as_str().map(str::to_owned),
            context: None,
            input: prompt::Prompt::default(),
            pending: Vec::new(),
            marks: Vec::new(),
            any_history: false,
            welcome: false,
            width: 80,
            height: 24,
            live: None,
            streamed: false,
            running: record["running_turn"].as_i64().map(|turn| Running {
                turn,
                started: Instant::now(),
                phase: "Working".into(),
                note: false,
            }),
            calls: HashMap::new(),
            approvals: Vec::new(),
            asks: Vec::new(),
            sent_answers: Vec::new(),
            reactions_key: record["bot_id"].as_i64().map(|id| {
                let store =
                    std::fs::canonicalize(&options.store).unwrap_or_else(|_| options.store.clone());
                reactions::key(&store, id)
            }),
            chatting: false,
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
            behind: (0, None),
            asks_seen: HashMap::new(),
            sounds: sound::Sounds::new(false),
            said: String::new(),
            staged: Vec::new(),
            pictures: Vec::new(),
            reactions: reactions::Reactions::default(),
            replies: Vec::new(),
            reacting: None,
            finding: None,
            react_to: None,
            reaction_turn: None,
            attached: Vec::new(),
            declare: None,
            bot_reactions: Vec::new(),
            react_calls: HashMap::new(),
            answering: None,
            rows_drawn: 0,
            thought: String::new(),
            replay: None,
            options,
            down: false,
            repaint: false,
            first_node: None,
            older: None,
            want_older: false,
            hold_view: false,
            full_width: 0,
            pane: None,
            pane_keys: false,
            pane_mouse: false,
            closed: None,
            margin: margin::Margin::default(),
            under: None,
            margin_focus: None,
            inline: HashMap::new(),
            folded: None,
            refold: false,
            plans: HashMap::new(),
            plan_count: 0,
            plan_full: None,
            jump: None,
            pressed: Vec::new(),
            woken: Arc::new(tokio::sync::Notify::new()),
            verbose: false,
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
            declared: record["declared"].clone(),
            mode: approvals::mode(record),
            choosing: None,
            reviewing: std::collections::HashSet::new(),
            denials: Vec::new(),
            approving: None,
            approve_shown: None,
            gating: approvals::refresh(record),
            spread: None,
            last_reply: String::new(),
            shell: None,
            shown: Vec::new(),
            unsent: Vec::new(),
            ran_here: Vec::new(),
            screen: screen::Screen::default(),
            folds: BTreeMap::new(),
            folding: None,
            relayed: HashMap::new(),
            tasks: Vec::new(),
            starting: None,
            tasks_read: None,
            tasks_all_read: None,
            tasks_width: 0,
        }
    }

    async fn main(&mut self, terminal: &mut Tui, events: &mut Events) -> Result<(), String> {
        term::laid_out().await;
        let _ = terminal.autoresize();
        self.width = terminal.size().map(|s| s.width as usize).unwrap_or(80);
        self.reactions = reactions::Reactions::load(self.reactions_key.as_deref());
        self.margin = margin::Margin::load(self.reactions_key.as_deref());
        self.history().await;
        // Known to start the conversation only once its newest page is
        // read, the welcome goes above it.
        let mut welcome = self.welcome();
        if !welcome.is_empty() {
            if self.any_history {
                welcome.push(Line::default());
            }
            self.any_history = true;
            // What marks the history's rows moves down with them.
            for (at, _) in &mut self.marks {
                *at += welcome.len();
            }
            self.pending.splice(0..0, welcome);
        }
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
        // Poll, not a bare read, so the reader ends with the client.
        std::thread::spawn(move || {
            while !keys_tx.is_closed() {
                // A read that fails, as one can while the Mac wakes, is
                // tried again rather than leaving the client deaf to keys.
                match event::poll(Duration::from_millis(50)) {
                    Ok(false) => {}
                    Ok(true) => match event::read() {
                        Ok(event) => {
                            if keys_tx.send(event).is_err() {
                                break;
                            }
                        }
                        Err(_) => std::thread::sleep(Duration::from_millis(100)),
                    },
                    Err(_) => std::thread::sleep(Duration::from_millis(100)),
                }
            }
        });
        let mut tick = tokio::time::interval(Duration::from_millis(100));
        // A terminal waking with the Mac can be slow to answer the cursor
        // query a resize makes, and a draw then fails; the next tick draws
        // again, and only a terminal that keeps failing ends the client.
        let mut failed_draws = 0;
        loop {
            match self.draw(terminal) {
                Ok(()) => failed_draws = 0,
                Err(_) if failed_draws < DRAW_ATTEMPTS => failed_draws += 1,
                Err(error) => return Err(error.to_string()),
            }
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
                _ = self.woken.notified() => {}
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
            if self.spread.is_some_and(|(_, at)| at <= Instant::now()) {
                self.spread().await;
            }
            // What the side chat copied goes on the clipboard, as the
            // terminal Sonny runs in would have put it.
            if let Some(program) = self.pane.as_ref().and_then(|p| p.program.as_ref()) {
                for text in program.copied() {
                    term::copy(&text);
                }
            }
            self.side_closed();
            self.shell_ran().await;
            self.listed();
            self.follow_place().await;
            self.count_behind();
            self.read_tasks().await;
            for what in std::mem::take(&mut self.pressed) {
                self.press(&what).await;
            }
            if std::mem::take(&mut self.want_older) {
                self.older().await;
            }
            self.jumped().await;
            self.repaint |= term::switched();
            if self.repaint && self.running.is_none() {
                self.repaint = false;
                self.repaint(terminal).await?;
            }
        }
    }

    /// The person's command, once it ran: the bot is given it at once, as
    /// a message, so it answers it as Claude Code's does: a turn of its
    /// own, or a steer of the running one. What it printed is shown once
    /// the bot's words still coming are not cut into.
    async fn shell_ran(&mut self) {
        if self.shell.as_ref().is_some_and(|s| s.is_finished()) {
            let ran = self.shell.take().unwrap().ran().await;
            self.shown.extend(user_shell::lines(&ran, self.prose()));
            let record = user_shell::record(&ran);
            // Refused, it is kept, to go with the next message.
            match self.send(record.clone(), Some(SHELL), false, &[]).await {
                Ok(()) => self.ran_here.extend(user_shell::read(&record)),
                Err(error) => {
                    self.notice = Some(format!("{error}; it goes with your next message"));
                    hold(&mut self.unsent, record);
                }
            }
        }
        if self.live.is_none() && !self.shown.is_empty() {
            let shown = std::mem::take(&mut self.shown);
            self.commit(shown);
        }
    }

    /// Take the workspace's place once read, and read it again every few
    /// seconds, or at once when the bot moved.
    async fn follow_place(&mut self) {
        if let Some((dir, job)) = &self.place_job
            && job.is_finished()
        {
            let dir = dir.clone();
            let job = self.place_job.take().unwrap().1;
            if let Ok(place) = job.await
                && dir == self.workspace
            {
                self.place = place;
            }
        }
        let due = match &self.place_read {
            Some((dir, at)) => *dir != self.workspace || at.elapsed() > PLACE_EVERY,
            None => true,
        };
        if self.place_job.is_none() && due && !self.workspace.is_empty() {
            let dir = self.workspace.clone();
            if self
                .place_read
                .as_ref()
                .is_some_and(|(read, _)| *read != dir)
            {
                self.place = place::Place::default();
            }
            self.place_read = Some((dir.clone(), Instant::now()));
            let job = tokio::task::spawn_blocking({
                let dir = dir.clone();
                move || place::read(std::path::Path::new(&dir))
            });
            self.place_job = Some((dir, job));
        }
    }

    /// The lead's instructions as composed on attach, sent once no turn
    /// runs; the next turn reads them.
    /// Whether what was picked is now the bot's: false while it waits
    /// for the turn to end, or when the daemon refused it (the notice says
    /// why).
    async fn configure(&mut self) -> bool {
        if self.instruct.is_none()
            && self.switch.is_none()
            && self.gating.is_none()
            && self.declare.is_none()
        {
            return true;
        }
        if self.running.is_some() {
            return false;
        }
        let (text, switch, gating) = (self.instruct.take(), self.switch.take(), self.gating.take());
        let mut declare = self.declare.take();
        // A bot that went without the tools Sonny gates, as its gate was
        // not Sonny's, has them with the mode the person picks.
        if declare.is_none() && gating.is_some() && self.mode.is_none() {
            declare =
                declared::declare(&json!({"declared": self.declared}), self.project.is_some());
        }
        // New tools are gated as the bot's mode gates the rest, so a tool
        // Sonny gates never runs unjudged where the mode judges.
        let gating = gating.or(declare
            .as_ref()
            .and(self.mode)
            .filter(|mode| *mode != "full"));
        let mut asked = json!({"bot": self.bot});
        if let Some(text) = &text {
            asked["instructions"] = json!(text);
        }
        if let Some(declare) = &declare {
            asked["declare"] = declare.clone();
        }
        if let Some((model, effort)) = &switch {
            asked["model"] = json!(model);
            asked["effort"] = json!(effort);
        }
        if let (Some(mode), Value::Object(fields)) = (gating, &mut asked) {
            let declared = declare.as_ref().unwrap_or(&self.declared);
            fields.extend(approvals::gate(
                mode,
                &approvals::gateable(&self.tools, declared),
            ));
            // A lead's sandbox goes with its mode.
            if self.project.is_some() {
                let workspace = std::path::Path::new(&self.workspace);
                fields.extend(approvals::sandbox(mode, true, workspace));
            }
        }
        match self.client.request("configure", asked).await {
            Ok(_) => {
                if let Some(declare) = &declare {
                    self.declared = declare.clone();
                }
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
                self.declare = declare;
                return false;
            }
            Err(error) => {
                self.notice = Some(error.to_string());
                return false;
            }
        }
        true
    }

    /// Give the mode picked with `/approvals` to every agent the lead made,
    /// and those they made: the person's pick overrides the gate each was
    /// made with. One that is busy is tried again a little later.
    async fn spread(&mut self) {
        let Some((mode, _)) = self.spread else {
            return;
        };
        self.spread = Some((mode, Instant::now() + Duration::from_secs(2)));
        let mut bots = Vec::new();
        let mut after = json!("");
        loop {
            let Ok(page) = self
                .client
                .request("bots", json!({"after": after, "limit": 256}))
                .await
            else {
                return;
            };
            bots.extend(page["bots"].as_array().cloned().unwrap_or_default());
            match &page["next_after"] {
                Value::Null => break,
                next => after = next.clone(),
            }
        }
        let mut busy = false;
        for bot in approvals::made_by(&bots, &self.bot) {
            // As `inspect` gives it, with the tools its client declared.
            let Ok(record) = (self.client)
                .request(
                    "inspect",
                    json!({"bot": bot["name"], "bot_id": bot["bot_id"]}),
                )
                .await
            else {
                continue;
            };
            let Some(fields) = approvals::regate(&record, mode) else {
                continue;
            };
            // That bot, not one given its name since it was listed.
            let mut asked = Value::Object(fields);
            asked["bot"] = bot["name"].clone();
            asked["bot_id"] = bot["bot_id"].clone();
            match self.client.request("configure", asked).await {
                Ok(_) => {}
                Err(error) if error.code == "bot_busy" => busy = true,
                Err(error) => {
                    let name = bot["name"].as_str().unwrap_or_default();
                    self.notice = Some(format!("{name}'s approvals: {error}"));
                }
            }
        }
        // The lead's own change still waiting: an agent it makes meanwhile
        // takes its old gate, so this goes on until both are set.
        if !busy && self.gating.is_none() {
            self.spread = None;
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

    /// The conversation drawn again in the background's new colours, or
    /// in the view just picked, once no turn is running: the screen
    /// cleared, and the bot's items read again as on attach.
    async fn repaint(&mut self, terminal: &mut Tui) -> Result<(), String> {
        let back = self.screen.back();
        self.screen.clear();
        if std::mem::take(&mut self.hold_view) {
            self.screen.hold(back);
        }
        terminal.clear().map_err(|e| e.to_string())?;
        self.pending.clear();
        self.marks.clear();
        self.folds.clear();
        self.folding = None;
        self.relayed.clear();
        self.plans.clear();
        self.plan_full = None;
        self.refold = true;
        self.tasks.clear();
        self.starting = None;
        self.held.clear();
        self.live = None;
        self.any_history = false;
        self.seen_node = 0;
        let welcome = self.welcome();
        self.commit(welcome);
        // Back to the first node this window drew, not only the newest page.
        match self.first_node {
            Some(first) => self.history_since(first - 1, true).await,
            None => self.history().await,
        }
        Ok(())
    }

    /// The page of items before the oldest drawn, read by drawing the
    /// conversation again from there once idle.
    async fn older(&mut self) {
        let Some(from) = self.older else {
            return;
        };
        let asked = json!({"bot": self.bot, "from": from, "limit": HISTORY_ITEMS});
        let page = match self.client.request("history_nodes", asked).await {
            Ok(page) => page,
            Err(error) => {
                self.notice = Some(error.to_string());
                return;
            }
        };
        let Some(oldest) = (page["nodes"].as_array())
            .and_then(|nodes| nodes.last())
            .and_then(|node| node["node"].as_i64())
        else {
            self.older = None;
            return;
        };
        self.first_node = Some(oldest);
        self.older = page["next_from"].as_i64();
        self.hold_view = true;
        self.repaint = true;
        if self.running.is_some() {
            self.notice = Some("older messages once this turn ends".into());
        }
    }

    /// The connection is gone: the daemon drops a reader that falls
    /// behind, or it stopped. Sonny connects again; a reply cut off
    /// mid-stream is drawn whole from the bot's items once back.
    fn dropped(&mut self) {
        // Words whose message came are past the replay's start: written
        // now, or lost. Words still streaming come again with their message.
        if let Some(live) = self.live.take()
            && live.closed
        {
            self.write_words(live, Words::Reply);
        }
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
            self.history_after(self.seen_node.max(self.options.side.unwrap_or(0)))
                .await;
            self.unstage_landed().await;
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
                    note: false,
                });
                self.turn_began().await;
                status_report("working", &self.bot);
            }
            self.approvals_for(self.running.as_ref().map(|r| r.turn))
                .await;
            // A daemon that restarted took the approver and the lookout
            // Sonny started with it.
            if let Err(error) = start_lookout(&self.options) {
                self.notice = Some(error);
            }
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

    /// A finished call, quiet, as the view shows it: every call when
    /// verbose, else only those [`transcript::clean`] keeps.
    /// Calls like the one before it, with nothing drawn between, fold
    /// into one: several tasks started at once read as one row.
    fn commit_call(&mut self, id: &str, name: &str, arguments: &Value, output: &str, failed: bool) {
        if name == margin::TOOL {
            self.commit_note(id, arguments, failed);
            return;
        }
        // A decision for the person stands out where it was asked, each
        // option a button that answers with it.
        if name == asks::TOOL
            && !failed
            && let Some(ask) = asks::call(id, arguments)
        {
            let (lines, options) = asks::lines(&ask, self.width);
            let from = self.pending.len() + usize::from(self.any_history);
            self.commit(lines);
            self.marks
                .extend(options.into_iter().enumerate().map(|(at, row)| {
                    let button = format!("{}answer/{at}/{id}", screen::BUTTON);
                    (from + row, screen::Mark::Link(button))
                }));
            return;
        }
        // A plan said in a row of how far it has got: the newest shows
        // in full in the margin, or here when the margin cannot show it.
        if name == plan::TOOL
            && !failed
            && let Some(steps) = plan::steps(arguments)
        {
            let at = self.plan_count;
            self.plan_count += 1;
            let lines = transcript::plan_lines(&steps, false, self.prose());
            let from = self.pending.len() + usize::from(self.any_history);
            let rows = lines.len();
            self.commit(lines);
            let tag = screen::Mark::Tag(screen::Tag::Plan(at));
            self.marks
                .extend((from..from + rows).map(|row| (row, tag.clone())));
            self.plans.insert(at, steps);
            self.refold = true;
            return;
        }
        // A task started stands out, and a click opens it in the pane.
        if name == served::START && !failed {
            self.commit_started(arguments, output);
            return;
        }
        let draw = match self.verbose {
            true => transcript::tool,
            false => transcript::clean,
        };
        let lines = transcript::quiet(draw(
            name,
            arguments,
            Some(output),
            failed,
            &self.workspace,
            self.prose(),
        ));
        if lines.is_empty() {
            return;
        }
        let Some((verb, what)) = transcript::folds(name, arguments, Some(output), failed) else {
            self.commit(lines);
            return;
        };
        if let Some((at, drawn)) = self.folding
            && drawn == self.rows_drawn
            && let Some(fold) = self.folds.get_mut(&at)
            && fold.verb == verb
        {
            fold.whats.push(what.clone());
            fold.calls.push(lines.clone());
            if self.draw_fold(at) {
                return;
            }
            // Gone from the conversation: this call starts a fold of its own.
            self.folds.remove(&at);
        }
        // Folds whose rows the conversation let go keep nothing: the
        // oldest go first, as rows do.
        while let Some(&first) = self.folds.keys().next() {
            let tag = screen::Tag::Group(first as u64);
            let waiting = (self.marks.iter()).any(|(_, mark)| *mark == screen::Mark::Tag(tag));
            if waiting || self.screen.has(tag) {
                break;
            }
            self.folds.remove(&first);
        }
        let at = self.folds.keys().next_back().map_or(0, |at| at + 1);
        self.folds.insert(
            at,
            Fold {
                verb,
                whats: vec![what],
                calls: vec![lines.clone()],
                open: false,
            },
        );
        let from = self.pending.len() + usize::from(self.any_history);
        self.commit(lines);
        let tag = screen::Mark::Tag(screen::Tag::Group(at as u64));
        self.marks
            .extend((from..self.pending.len()).map(|line| (line, tag.clone())));
        self.folding = Some((at, self.rows_drawn));
    }

    /// The task a `start_task` call started, joining the tasks started
    /// right before it.
    fn commit_started(&mut self, arguments: &Value, output: &str) {
        let name = arguments["name"].as_str().unwrap_or("task").to_owned();
        let bot = (output.split(|c: char| c.is_whitespace() || c == '"'))
            .find(|word| word.starts_with("turn:"))
            .map(calls::handle_bot)
            .or_else(|| (self.project.as_ref()).map(|project| format!("{project}.{name}")))
            .unwrap_or_else(|| name.clone());
        let task = started::Task {
            bot,
            name,
            // Only its first words are drawn.
            brief: started::first(arguments["brief"].as_str().unwrap_or("")),
            state: "starting".into(),
        };
        self.tasks_read = None;
        if let Some((at, drawn)) = self.starting
            && drawn == self.rows_drawn
        {
            self.tasks[at].push(task.clone());
            if self.draw_started(at) {
                return;
            }
            // Gone from the conversation: this task starts a group of its own.
            self.tasks[at].pop();
        }
        let at = self.tasks.len();
        self.tasks.push(vec![task]);
        let (lines, marks) = self.started_lines(at);
        let count = lines.len();
        self.commit(lines);
        let from = self.pending.len() - count;
        let tag = screen::Mark::Tag(screen::Tag::Tasks(at as u64));
        self.marks
            .extend((from..from + count).map(|line| (line, tag.clone())));
        self.marks
            .extend(marks.into_iter().map(|(line, mark)| (from + line, mark)));
        self.starting = Some((at, self.rows_drawn));
    }

    /// The rows of the tasks started together as `at`, and the buttons
    /// on them that open each.
    fn started_lines(&self, at: usize) -> (Vec<Line<'static>>, Vec<(usize, screen::Mark)>) {
        let tasks = &self.tasks[at];
        let rows = started::draw(tasks, self.prose());
        let mut marks = Vec::new();
        let lines = (rows.into_iter().enumerate())
            .map(|(line, (row, task))| {
                if let Some(task) = task {
                    let button = format!("{}task/{}", screen::BUTTON, tasks[task].bot);
                    marks.push((line, screen::Mark::Link(button)));
                }
                row
            })
            .collect();
        (lines, marks)
    }

    /// The tasks started together as `at` drawn again where they are;
    /// whether they are still in the conversation.
    fn draw_started(&mut self, at: usize) -> bool {
        let (lines, marks) = self.started_lines(at);
        self.flush();
        (self.screen).replace_tagged(screen::Tag::Tasks(at as u64), lines, marks)
    }

    /// What the tasks started are doing, read again every few seconds:
    /// those still at work, and every one when news of them came.
    async fn read_tasks(&mut self) {
        // Drawn again at the width the conversation has now, as when the
        // pane opened beside it.
        if !self.tasks.is_empty() && self.tasks_width != self.prose() {
            self.tasks_width = self.prose();
            for at in 0..self.tasks.len() {
                self.draw_started(at);
            }
        }
        if self.tasks.is_empty()
            || self
                .tasks_read
                .is_some_and(|at| at.elapsed() < Duration::from_secs(3))
        {
            return;
        }
        // A task that ended can be given more work: every one is read
        // again now and then.
        let all = self.tasks_read.is_none()
            || self
                .tasks_all_read
                .is_none_or(|at| at.elapsed() >= Duration::from_secs(30));
        self.tasks_read = Some(Instant::now());
        if all {
            self.tasks_all_read = self.tasks_read;
        }
        let mut bots: Vec<String> = (self.tasks.iter().flatten())
            .filter(|task| all || !started::settled(&task.state))
            .map(|task| task.bot.clone())
            .collect();
        bots.sort();
        bots.dedup();
        let mut states = HashMap::new();
        for bot in bots {
            let read = self
                .client
                .request("turns", json!({"bot": bot, "newest": true, "limit": 8}))
                .await;
            let state = match read {
                Ok(page) => started::state(&self.task_turn(&page).await),
                Err(error) if error.code == "bot_not_found" => "removed".to_owned(),
                // Not read this time: it stays as it was.
                Err(_) => continue,
            };
            states.insert(bot, state);
        }
        for at in 0..self.tasks.len() {
            let mut changed = false;
            for task in &mut self.tasks[at] {
                if let Some(state) = states.get(&task.bot)
                    && *state != task.state
                {
                    task.state = state.clone();
                    changed = true;
                }
            }
            if changed {
                self.draw_started(at);
            }
        }
    }

    /// The turn that says what a task does, from its newest `turns`: a
    /// message steered into a running turn is a turn of its own, settled
    /// at once, so the turn it joined is read instead. A failed one with
    /// why it failed, which `turns` leaves out, as `wait` has it.
    async fn task_turn(&self, page: &Value) -> Value {
        let turns = page["turns"].as_array().map_or(&[][..], Vec::as_slice);
        let Some(turn) = turns.iter().find(|t| t["status"] != "steered") else {
            return turns.first().cloned().unwrap_or(Value::Null);
        };
        let mut turn = turn.clone();
        if turn["status"] == "failed"
            && let Some(handle) = turn["handle"].as_str().map(str::to_owned)
            && let Ok(answer) = (self.client)
                .request("wait", json!({"handles": [handle], "timeout_ms": 0}))
                .await
        {
            turn["error"] = answer["results"][&handle]["error"].clone();
        }
        turn
    }

    /// The task `bot` opened in the pane: its own Sonny, to watch it and
    /// talk to it.
    fn open_task(&mut self, bot: &str) {
        let name = (self.tasks.iter().flatten())
            .find(|task| task.bot == bot)
            .map_or(bot.to_owned(), |task| task.name.clone());
        let args = ["--no-sound", "--no-layout", "--embedded", "--bot", bot].map(str::to_owned);
        if let Err(error) = self.embed(&name, &args) {
            self.notice = Some(format!("could not open {name}: {error}"));
        }
    }

    /// Sonny run with `args` in the pane, on a terminal of its own: a side
    /// chat, or a task opened to watch.
    fn embed(&mut self, title: &str, args: &[String]) -> Result<(), String> {
        let me = std::env::current_exe().map_err(|e| e.to_string())?;
        let mut command = std::process::Command::new(me);
        command
            .arg("--store")
            .arg(&self.options.store)
            .arg("--socket")
            .arg(&self.options.socket)
            .args(args);
        let mut program = portable_pty::CommandBuilder::new(command.get_program());
        program.args(command.get_args());
        if let Ok(here) = std::env::current_dir() {
            program.cwd(here);
        }
        program.env("TERM", "xterm-256color");
        let height = self.height.saturating_sub(2).max(2) as u16;
        let width = match pane::width(self.full_width, PROSE as u16) {
            0 => 80,
            width => width,
        };
        let woken = self.woken.clone();
        let drawn = move || woken.notify_one();
        let program = embed::Embedded::spawn(program, height, width, term::light(), drawn)?;
        self.open_pane(pane::Pane::program(title, program));
        Ok(())
    }

    /// The calls folded at `at` drawn again where they are, folded or
    /// open; whether they are still in the conversation.
    fn draw_fold(&mut self, at: usize) -> bool {
        let Some(fold) = self.folds.get(&at) else {
            return false;
        };
        let lines = transcript::quiet(transcript::folded(
            &fold.verb,
            &fold.whats,
            &fold.calls,
            fold.open,
            self.prose(),
        ));
        let open = screen::Mark::Link(format!("{}fold/{at}", screen::BUTTON));
        self.flush();
        self.screen
            .replace_tagged(screen::Tag::Group(at as u64), lines, vec![(0, open)])
    }

    /// What a button clicked in the conversation does: `fold/N` opens or
    /// folds the calls folded as N; `said/NODE` opens or shuts the message
    /// between agents at NODE; `task/BOT` opens the task BOT in the pane;
    /// `answer/N/ID` answers the question
    /// call ID asked with its option N, while it is open.
    async fn press_row(&mut self, button: &str) {
        if let Some(at) = button
            .strip_prefix("fold/")
            .and_then(|at| at.parse::<usize>().ok())
            && let Some(fold) = self.folds.get_mut(&at)
        {
            fold.open = !fold.open;
            self.draw_fold(at);
        }
        if let Some(bot) = button.strip_prefix("task/") {
            self.open_task(bot);
        }
        if let Some(node) = button
            .strip_prefix("said/")
            .and_then(|node| node.parse::<i64>().ok())
            && self.relayed.contains_key(&node)
        {
            let text = match self.relayed[&node].text {
                Some(_) => None,
                None => match self.items(&[node]).await.first() {
                    Some(item) => Some(text(item)),
                    None => {
                        self.notice = Some("could not read that message".into());
                        return;
                    }
                },
            };
            if let Some(relayed) = self.relayed.get_mut(&node) {
                relayed.text = text;
            }
            let lines = self.relayed_lines(node);
            self.flush();
            let tag = screen::Tag::Relayed(node);
            self.screen
                .replace_tagged(tag, lines, vec![(0, said_button(node))]);
        }
        if let Some((at, id)) = button
            .strip_prefix("answer/")
            .and_then(|rest| rest.split_once('/'))
            .and_then(|(at, id)| Some((at.parse::<usize>().ok()?, id)))
        {
            let Some(ask) = self.asks.iter().find(|ask| ask.id == id).cloned() else {
                self.notice = Some("that question was answered already".into());
                return;
            };
            let answer = asks::answer(&ask, &(at + 1).to_string());
            match self.send(answer, None, false, &[]).await {
                Ok(()) => {
                    self.notice = None;
                    self.asks.retain(|open| open.id != ask.id);
                    self.sent_answers.push(ask.question);
                    if self.asks.is_empty() {
                        self.chatting = false;
                    }
                }
                Err(error) => self.notice = Some(error),
            }
        }
    }

    /// A `margin` call where it was made: the note's first post as a
    /// marker pinned there, which a click scrolls the margin to; what
    /// changed it later only when verbose, the margin showing that.
    fn commit_note(&mut self, call: &str, arguments: &Value, failed: bool) {
        let Some(id) = arguments["id"].as_str() else {
            return;
        };
        let note = self
            .margin
            .note(id)
            .filter(|note| note.call == call)
            .cloned();
        let what = match (&note, arguments["remove"] == true) {
            (Some(note), _) if !failed => note.title.clone(),
            _ if !self.verbose => return,
            (_, true) => format!("took {id} away"),
            _ => format!("changed {id}"),
        };
        self.commit(vec![note_marker(what, true)]);
        // Drawn out where it was put, when the margin cannot show it.
        self.refold = true;
        if let Some(note) = note {
            let pin = margin::anchor(&note.call);
            let link = format!("{}note/{id}", screen::BUTTON);
            self.mark_last(screen::Mark::Tag(screen::Tag::Note(pin)));
            self.mark_last(screen::Mark::Link(link));
        }
    }

    /// The notes drawn out in the conversation where they were put when
    /// the margin is not `beside` it, each its marker alone where it is:
    /// done again only once that, or a note, changed.
    fn fold_notes(&mut self, beside: bool) {
        let workspace = std::path::PathBuf::from(&self.workspace);
        if !beside {
            self.margin.freshen(&workspace);
        }
        let key = (beside, self.margin.changes());
        if !std::mem::take(&mut self.refold) && self.folded == Some(key) {
            return;
        }
        self.folded = Some(key);
        self.fold_plan(beside);
        let notes: Vec<(u64, String, String)> = (self.margin.notes().iter())
            .map(|note| {
                (
                    margin::anchor(&note.call),
                    note.id.clone(),
                    note.title.clone(),
                )
            })
            .collect();
        // Back to their markers: all of them beside the margin, else those
        // taken away.
        let back: Vec<u64> = (self.inline.keys())
            .filter(|pin| beside || !notes.iter().any(|(at, ..)| at == *pin))
            .copied()
            .collect();
        for pin in back {
            let (id, title) = self.inline.remove(&pin).expect("drawn out");
            let link = screen::Mark::Link(format!("{}note/{id}", screen::BUTTON));
            let line = note_marker(title, true);
            (self.screen).replace_tagged(screen::Tag::Note(pin), vec![line], vec![(0, link)]);
        }
        if beside {
            return;
        }
        let width = self.prose();
        for (pin, id, title) in notes {
            let tag = screen::Tag::Note(pin);
            if !self.screen.has(tag) {
                continue;
            }
            let body = (self.margin.body(&id, width, &workspace, INLINE_ROWS)).unwrap_or_default();
            let link = || screen::Mark::Link(format!("{}note/{id}", screen::BUTTON));
            let mut marks = vec![(0, link())];
            // The rest of it a click away, in the margin.
            if body.len() > INLINE_ROWS {
                marks.push((body.len(), link()));
            }
            let mut lines = vec![note_marker(title.clone(), false)];
            lines.extend(body);
            self.screen.replace_tagged(tag, lines, marks);
            self.inline.insert(pin, (id, title));
        }
    }

    /// The newest plan drawn shown in the margin, and in full where it
    /// was set when the margin is not `beside` the conversation; every
    /// other plan call a row of how far it had got.
    fn fold_plan(&mut self, beside: bool) {
        // Those the screen no longer keeps are forgotten.
        let kept: std::collections::HashSet<u64> = (self.screen.tags())
            .filter_map(|tag| match tag {
                screen::Tag::Plan(at) => Some(at),
                _ => None,
            })
            .collect();
        self.plans.retain(|at, _| kept.contains(at));
        let newest = (self.screen.tags())
            .filter_map(|tag| match tag {
                screen::Tag::Plan(at) => Some(at),
                _ => None,
            })
            .last();
        let steps = newest.and_then(|at| self.plans.get(&at)).cloned();
        self.margin.set_plan(steps.as_deref().unwrap_or_default());
        let full = newest.filter(|_| !beside);
        if full == self.plan_full {
            return;
        }
        let width = self.prose();
        for (at, full) in [(self.plan_full, false), (full, true)] {
            let Some(steps) = at.and_then(|at| self.plans.get(&at)) else {
                continue;
            };
            let lines = transcript::plan_lines(steps, full, width);
            let tag = screen::Tag::Plan(at.expect("found"));
            self.screen.replace_tagged(tag, lines, Vec::new());
        }
        self.plan_full = full;
    }

    /// A `margin` call applied as it starts, so a note shows while the
    /// agent works, and the margin scrolled to it.
    fn note_call(&mut self, call: &str, arguments: &Value, turn: Option<i64>) {
        let after = self.answering.as_ref().map(|(.., node)| *node);
        if let Some(margin::Change::Posted(id) | margin::Change::Updated(id)) =
            self.margin.apply(call, arguments, after, turn)
        {
            self.margin_focus = self.margin.note(&id).map(|note| margin::anchor(&note.call));
        }
    }

    /// Whether the person's message `said` at `node`, in `turn`, took a
    /// note away, from this window or another: the margin shows it, once.
    fn note_closed(&mut self, node: i64, said: &str, turn: Option<i64>) -> bool {
        let Some(id) = margin::closed(said) else {
            return false;
        };
        let key = match turn {
            Some(turn) => format!("t{turn}"),
            None => format!("m{node}"),
        };
        self.margin.remove(id, Some((node, &key)));
        true
    }

    /// Commit a denied call, in either view.
    fn commit_denied(&mut self, name: &str, arguments: &Value, reason: &str) {
        let lines = transcript::denied(name, arguments, reason, &self.workspace, self.width);
        self.commit(lines);
    }

    /// The person's message at `node` landed: drawn as it reads, what
    /// they ran as it was when it ran, its last row tagged, and the
    /// message the bot now answers.
    fn land(&mut self, node: i64, said: &str, from: Option<&str>) {
        // Reactions from here on answer this one.
        self.bot_reactions.clear();
        // Words between agents are one quiet row a click opens.
        if transcript::relays(said, from) {
            let relayed = Relayed {
                head: transcript::relayed_head(said, from),
                from: from.map(str::to_owned),
                text: None,
            };
            self.relayed.insert(node, relayed);
            let lines = self.relayed_lines(node);
            let count = lines.len();
            self.commit(lines);
            let at = self.pending.len() - count;
            let tag = screen::Mark::Tag(screen::Tag::Relayed(node));
            self.marks
                .extend((at..at + count).map(|line| (line, tag.clone())));
            self.marks.push((at, said_button(node)));
        } else {
            let prose = self.prose();
            let lines = landed(said, &mut self.ran_here, prose);
            self.commit(lines);
            self.mark_last(screen::Mark::Tag(screen::Tag::End(node)));
        }
        self.answering = Some((first_words(said), self.rows_drawn, node));
    }

    /// The rows of the message between agents at `node`, open or shut.
    fn relayed_lines(&self, node: i64) -> Vec<Line<'static>> {
        let Some(relayed) = self.relayed.get(&node) else {
            return Vec::new();
        };
        match &relayed.text {
            Some(text) => transcript::relayed(text, relayed.from.as_deref(), true, self.prose()),
            None => transcript::relayed_shut(&relayed.head, self.prose()),
        }
    }

    /// The bot's words at `node`, as replayed: their last row tagged,
    /// and the person's reactions to them right under it.
    fn commit_said(&mut self, node: i64, lines: Vec<Line<'static>>) {
        self.commit(lines);
        self.mark_last(screen::Mark::Tag(screen::Tag::End(node)));
        if let Some(row) = self.reacted_row(node) {
            self.commit_after(vec![row], false);
            self.mark_last(screen::Mark::Tag(screen::Tag::Reactions(node)));
        }
    }

    /// Commit a finished cell, a blank line apart from the one before.
    /// The welcome, when this window has one and draws the conversation
    /// from its first item: none above a page with older ones before it.
    fn welcome(&self) -> Vec<Line<'static>> {
        match self.welcome && self.older.is_none() {
            true => banner::welcome(&tilde(&self.workspace), &self.model, self.width),
            false => Vec::new(),
        }
    }

    fn commit(&mut self, lines: Vec<Line<'static>>) {
        self.commit_after(lines, true);
    }

    /// Commit a finished cell, right under the one before when `gap` is
    /// false, as a reaction goes under its reply.
    fn commit_after(&mut self, lines: Vec<Line<'static>>, gap: bool) {
        if lines.is_empty() {
            return;
        }
        if gap && self.any_history {
            self.pending.push(Line::default());
        }
        self.any_history = true;
        self.rows_drawn += lines.len();
        self.pending.extend(lines);
    }

    /// The lines waiting written into the conversation, with their marks.
    fn flush(&mut self) {
        let marks = std::mem::take(&mut self.marks);
        self.screen
            .push_marked(std::mem::take(&mut self.pending), marks);
    }

    /// `mark` on the last row written so far, waiting or drawn.
    fn mark_last(&mut self, mark: screen::Mark) {
        match self.pending.len() {
            0 => {
                self.screen.mark_last(mark);
            }
            n => self.marks.push((n - 1, mark)),
        }
    }

    /// The row tagged `tag` (`line`, or none) right under the row tagged
    /// `under`; whether that row is in the conversation.
    fn put_under(
        &mut self,
        under: screen::Tag,
        tag: screen::Tag,
        line: Option<Line<'static>>,
    ) -> bool {
        self.flush();
        self.screen.put_under(under, tag, line)
    }

    /// The items of `nodes`, in order and in the Responses family's shape.
    /// A read stops at a byte limit, so what it left is asked for again.
    async fn items(&self, nodes: &[i64]) -> Vec<Value> {
        self.items_at(nodes)
            .await
            .into_iter()
            .map(|(_, item)| item)
            .collect()
    }

    /// [`App::items`], each with the node it is kept at.
    async fn items_at(&self, nodes: &[i64]) -> Vec<(i64, Value)> {
        self.items_of(&self.bot, nodes).await
    }

    /// [`App::items_at`] for any bot.
    async fn items_of(&self, bot: &str, nodes: &[i64]) -> Vec<(i64, Value)> {
        let mut items = Vec::new();
        let mut rest = nodes;
        while !rest.is_empty() {
            let Ok(read) = self
                .client
                .request("history_items", json!({"bot": bot, "nodes": rest}))
                .await
            else {
                break;
            };
            let entries = read["items"].as_array().cloned().unwrap_or_default();
            if entries.is_empty() {
                break;
            }
            rest = &rest[entries.len().min(rest.len())..];
            items.extend(entries.iter().flat_map(|entry| {
                let node = entry["node"].as_i64().unwrap_or(0);
                // Who sent a prompt, when another bot did.
                let from = entry["from"]["bot"].clone();
                responses(&entry["item"]).into_iter().map(move |mut item| {
                    if from.is_string() && item["role"] == "user" {
                        item["from"] = from.clone();
                    }
                    (node, item)
                })
            }));
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

    /// The bot's latest items, written as the transcript would have shown
    /// them. A side chat shows only itself, not the history it was copied
    /// with.
    async fn history(&mut self) {
        self.history_after(self.options.side.unwrap_or(0)).await;
    }

    /// The bot's items after `after`, drawn as they would have been live:
    /// the newest [`HISTORY_ITEMS`] on attaching (`after` 0), and every one
    /// since on reconnecting.
    async fn history_after(&mut self, after: i64) {
        self.history_since(after, after > 0).await;
    }

    /// The bot's items after `after`: every one with `every`, else the
    /// newest page.
    async fn history_since(&mut self, after: i64, every: bool) {
        // On attach the newest page; on reconnecting, every page back to
        // the last node this window drew, so nothing missed is skipped.
        let mut nodes: Vec<i64> = Vec::new();
        // The turn each node was made in, for the tasks the lead resolved.
        let mut turns: HashMap<i64, i64> = HashMap::new();
        let mut asked = json!({"bot": self.bot, "limit": HISTORY_ITEMS});
        if every {
            asked["min_node"] = json!(after + 1);
        }
        loop {
            let Ok(page) = self.client.request("history_nodes", asked.clone()).await else {
                return;
            };
            // On attach, where older items start.
            if !every {
                self.older = page["next_from"].as_i64();
            }
            for n in page["nodes"].as_array().into_iter().flatten() {
                let Some(node) = n["node"].as_i64().filter(|node| *node > after) else {
                    continue;
                };
                nodes.push(node);
                if let Some(turn) = n["turn"].as_i64() {
                    turns.insert(node, turn);
                }
            }
            if !every || page["next_from"].is_null() {
                break;
            }
            asked["from"] = page["next_from"].clone();
        }
        nodes.reverse();
        // Drawn from the start (attaching, or a repaint), the bot's
        // reactions are read again with the rest; after a gap, those it
        // made before the gap are still held.
        // The open questions are those the replay asks and does not answer.
        if self.seen_node == 0 || after < self.seen_node {
            self.bot_reactions.clear();
            self.react_calls.clear();
            self.answering = None;
            self.asks.clear();
            self.sent_answers.clear();
        }
        self.seen_node = nodes.last().copied().unwrap_or(self.seen_node);
        if let Some(&first) = nodes.first() {
            self.first_node = Some(self.first_node.map_or(first, |n| n.min(first)));
        }
        let mut calls: HashMap<String, (String, Value)> = HashMap::new();
        // The bot's words, quiet if a call follows them, as they were live.
        let mut said: Option<(i64, Vec<Line<'static>>)> = None;
        for chunk in nodes.chunks(100) {
            for (node, item) in self.items_at(chunk).await {
                // A reaction leaves the words before it as they were.
                let call = item["type"] == "function_call" && item["name"] != reactions::TOOL;
                if let Some((at, lines)) = said.take_if(|_| call || item["role"].is_string()) {
                    let lines = match call {
                        true => transcript::quiet(lines),
                        false => lines,
                    };
                    self.commit_said(at, lines);
                }
                match (item["type"].as_str(), item["role"].as_str()) {
                    // A reaction is drawn under the reply it answers.
                    (_, Some("user")) if reactions::is_reaction(&text(&item)) => {}
                    // The person took a note away: the margin shows it.
                    (_, Some("user")) if margin::closed(&text(&item)).is_some() => {
                        self.note_closed(node, &text(&item), turns.get(&node).copied());
                    }
                    (_, Some("user")) => {
                        // The bot's reactions to the message before, as they
                        // were drawn live.
                        self.draw_bot_reactions();
                        asks::close(&mut self.asks, &text(&item));
                        self.land(node, &text(&item), item["from"].as_str());
                    }
                    (_, Some("assistant")) => {
                        let reply = text(&item);
                        if !reply.is_empty() {
                            let lines = transcript::reply(
                                &reply,
                                self.prose(),
                                std::path::Path::new(&self.workspace),
                            );
                            said = Some((node, lines));
                            self.saw_reply(node, &reply);
                            self.last_reply = reply;
                        }
                    }
                    // Counted once it is known to have been taken.
                    (Some("function_call"), _) if item["name"] == reactions::TOOL => {
                        if let Some(reaction) = item["arguments"]
                            .as_str()
                            .and_then(|a| serde_json::from_str::<Value>(a).ok())
                            .as_ref()
                            .and_then(reactions::call)
                        {
                            let id = item["call_id"].as_str().unwrap_or("").to_owned();
                            self.react_calls.insert(id, reaction);
                        }
                    }
                    (Some("function_call"), _) => {
                        let arguments = item["arguments"]
                            .as_str()
                            .and_then(|a| serde_json::from_str(a).ok())
                            .unwrap_or(Value::Null);
                        let name = item["name"].as_str().unwrap_or("tool").to_owned();
                        let id = item["call_id"].as_str().unwrap_or("").to_owned();
                        calls.insert(id, (name, arguments));
                    }
                    (Some("function_call_output"), _)
                        if self
                            .react_calls
                            .contains_key(item["call_id"].as_str().unwrap_or("")) =>
                    {
                        let id = item["call_id"].as_str().unwrap_or("");
                        if let Some((emoji, remove)) = self.react_calls.remove(id)
                            && !output(&item).1
                            && !reactions::refused(&item)
                        {
                            reactions::apply(&mut self.bot_reactions, &emoji, remove);
                        }
                    }
                    (Some("function_call_output"), _) => {
                        let id = item["call_id"].as_str().unwrap_or("");
                        // A call this window kept before it lost the daemon
                        // may end in what it reads on reconnecting.
                        if let Some((name, arguments)) =
                            calls.remove(id).or_else(|| self.calls.remove(id))
                        {
                            let (output, mut failed) = output(&item);
                            // A note as the call that ran left it.
                            if name == margin::TOOL
                                && !failed
                                && denial(&item).is_none()
                                && !reactions::refused(&item)
                            {
                                self.note_call(id, &arguments, turns.get(&node).copied());
                            }
                            match denial(&item) {
                                Some(reason) => {
                                    // It never ran: nothing it would have done follows.
                                    failed = true;
                                    self.commit_denied(&name, &arguments, &reason);
                                }
                                None => self.commit_call(id, &name, &arguments, &output, failed),
                            }
                            if name == resolved::TOOL && !failed && !reactions::refused(&item) {
                                self.resolve(&arguments, id, turns.get(&node).copied(), false)
                                    .await;
                            }
                            if name == asks::TOOL
                                && !failed
                                && !reactions::refused(&item)
                                && let Some(ask) = asks::call(id, &arguments)
                                && !self.asks.iter().any(|open| open.id == ask.id)
                            {
                                self.asks.push(ask);
                            }
                        }
                    }
                    _ => {}
                }
            }
        }
        if let Some((at, lines)) = said {
            self.commit_said(at, lines);
        }
        // A running turn's reactions are drawn when it ends.
        if self.running.is_none() {
            self.draw_bot_reactions();
        }
        // A call still running finishes live, after the history.
        self.calls.extend(calls);
    }

    /// Drop the staged prompts that landed while Sonny was away, whose
    /// `accepted` or `steered` it missed, and the attached messages a call
    /// took in meanwhile: those whose turn no longer waits.
    async fn unstage_landed(&mut self) {
        let ids = (self.staged.iter().map(|(turn, _)| *turn)).chain(self.attached.iter().copied());
        let (Some(first), Some(last)) = (ids.clone().min(), ids.max()) else {
            return;
        };
        // Every turn from the first staged to the last, a page at a time:
        // other clients may have sent many between them.
        let mut waiting: Vec<i64> = Vec::new();
        let mut after = first - 1;
        loop {
            let asked = json!({"bot": self.bot, "after": after, "before": last + 1, "limit": 256});
            let Ok(page) = self.client.request("turns", asked).await else {
                // Unread, they stay as they are.
                return;
            };
            waiting.extend(
                page["turns"]
                    .as_array()
                    .into_iter()
                    .flatten()
                    .filter(|row| {
                        matches!(
                            row["status"].as_str(),
                            Some("queued" | "ready" | "attached")
                        )
                    })
                    .filter_map(|row| row["turn"].as_i64()),
            );
            match page["next_after"].as_i64() {
                Some(next) => after = next,
                None => break,
            }
        }
        self.staged.retain(|(turn, _)| waiting.contains(turn));
        self.attached.retain(|turn| waiting.contains(turn));
    }

    /// What was sent and has not landed yet, dim, the way it will read
    /// once it does: a blank row above each, as in the history, and one
    /// under the last, so the status row does not touch it.
    fn staged_rows(&self, width: usize) -> Vec<Line<'static>> {
        let mut rows: Vec<Line<'static>> = self
            .staged
            .iter()
            .flat_map(|(_, prompt)| {
                let shown: Vec<Line<'static>> = match user_shell::read(prompt) {
                    Some(ran) => user_shell::header(&ran.command, width, true),
                    None => {
                        // An answer shows as itself, under the question it answers.
                        let (asked, prompt) = match asks::split(prompt) {
                            Some((question, answer)) => {
                                (Some(asks::answering(question, width)), answer)
                            }
                            None => (None, prompt.as_str()),
                        };
                        asked
                            .into_iter()
                            .chain(prompt.split('\n').enumerate().flat_map(|(at, line)| {
                                let lead = if at == 0 { "› " } else { "  " };
                                transcript::wrap(
                                    Line::from(line.to_owned()).dim(),
                                    width,
                                    Span::raw(lead).dim(),
                                    Span::raw("  "),
                                )
                            }))
                            .collect()
                    }
                };
                std::iter::once(Line::default()).chain(shown)
            })
            .collect();
        if !rows.is_empty() {
            rows.push(Line::default());
        }
        rows
    }

    /// The person's command while it runs, and those the agent could not
    /// be given yet.
    fn shell_rows(&self, width: usize) -> Vec<Line<'static>> {
        let mut rows = Vec::new();
        for record in &self.unsent {
            if let Some(ran) = user_shell::read(record) {
                let mut header = user_shell::header(&ran.command, width, true);
                if let Some(last) = header.last_mut() {
                    last.spans
                        .push(Span::raw(" · goes with your next message").dim());
                }
                rows.extend(header);
            }
        }
        if let Some(shell) = &self.shell {
            let mut header = user_shell::header(&shell.command, width, true);
            if let Some(last) = header.last_mut() {
                last.spans.push(
                    Span::raw(format!(
                        " · running {} · esc stops it",
                        transcript::elapsed(shell.started.elapsed().as_secs())
                    ))
                    .dim(),
                );
            }
            rows.extend(header);
        }
        rows
    }

    /// The bot's words not yet written: the rows held, then the open line,
    /// the part after the last newline, as the rows it will take.
    fn open_rows(&self, width: usize) -> Vec<Line<'static>> {
        let Some(open) = &self.live else {
            return Vec::new();
        };
        let mut rows = Vec::new();
        if open.gap && !open.spilled && (!open.rows.is_empty() || !open.partial.is_empty()) {
            rows.push(Line::default());
        }
        // Held rows were wrapped for the width then: a narrower pane cuts
        // them again, as it does the history waiting to be written.
        for row in &open.rows {
            match row.width() <= width {
                true => rows.push(row.clone()),
                false => rows.extend(transcript::wrap(
                    row.clone(),
                    width,
                    Span::raw(""),
                    Span::raw("    "),
                )),
            }
        }
        let mut markdown = open.markdown.clone();
        if !open.partial.is_empty()
            && let Some(line) = markdown.line(&open.partial)
        {
            let lead = if open.lines == 0 { "• " } else { "  " };
            rows.extend(transcript::wrap(
                line,
                width,
                Span::raw(lead),
                markdown.rest(),
            ));
        }
        rows
    }

    /// The words' last lines, and a table they ended on, drawn and held
    /// until what follows says what they were.
    fn close_live(&mut self) {
        if let Some(mut live) = self.live.take() {
            if !live.closed {
                let rest = std::mem::take(&mut live.partial);
                let rest = rest.strip_suffix('\n').unwrap_or(&rest);
                if !rest.is_empty() {
                    for line in rest.split('\n') {
                        self.live_line(&mut live, line);
                    }
                }
                let held = live.markdown.end(self.prose(), live.lines == 0);
                self.live_rows(&mut live, held);
                live.closed = true;
            }
            self.live = Some(live);
        }
    }

    /// The words written into the history as what they turned out to be.
    fn end_live(&mut self, words: Words) {
        self.close_live();
        if let Some(live) = self.live.take() {
            self.write_words(live, words);
        }
    }

    fn write_words(&mut self, live: Live, words: Words) {
        if live.rows.is_empty() {
            return;
        }
        if live.gap && !live.spilled {
            self.pending.push(Line::default());
        }
        self.rows_drawn += live.rows.len();
        self.pending.extend(match words {
            Words::Step => transcript::quiet(live.rows),
            Words::Reply => live.rows,
        });
        self.any_history = true;
        if let Some(node) = live.node {
            self.mark_last(screen::Mark::Tag(screen::Tag::End(node)));
        }
    }

    fn live_line(&mut self, live: &mut Live, line: &str) {
        let rows = live.markdown.feed(line, self.prose(), live.lines == 0);
        self.live_rows(live, rows);
    }

    fn live_rows(&mut self, live: &mut Live, rows: Vec<Line<'static>>) {
        if rows.is_empty() {
            return;
        }
        live.lines += rows.len();
        live.rows.extend(rows);
        // Words longer than half the screen are a reply, written as they
        // come rather than held out of the scrollback.
        if live.spilled || live.rows.len() > (self.height / 2).max(4) {
            if live.gap && !live.spilled {
                self.pending.push(Line::default());
            }
            live.spilled = true;
            self.rows_drawn += live.rows.len();
            self.pending.append(&mut live.rows);
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
            self.first_node.get_or_insert(node);
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
                self.staged.retain(|(staged, _)| Some(*staged) != turn);
                self.end_live(Words::Reply);
                self.said.clear();
                self.thought.clear();
                self.replay = None;
                self.running = Some(Running {
                    turn: turn.unwrap_or(0),
                    started: Instant::now(),
                    phase: "Working".into(),
                    note: from_lookout(data),
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
                self.bot_reactions.clear();
                self.answering = None;
                if !drawn
                    && let Some(node) = data["node"].as_i64()
                    && let Some(item) = self.items(&[node]).await.first()
                    && !self.note_closed(node, &text(item), turn)
                    && !quiet_message(&text(item))
                {
                    self.land(node, &text(item), item["from"].as_str());
                    self.answered(&text(item));
                }
            }
            "steered" => {
                self.staged
                    .retain(|(staged, _)| Some(*staged) != data["steer"].as_i64());
                // A reaction was drawn when it was sent.
                if let Some(node) = data["node"].as_i64()
                    && let Some(item) = self.items(&[node]).await.first()
                    && !self.note_closed(node, &text(item), turn)
                    && !quiet_message(&text(item))
                {
                    self.end_live(Words::Reply);
                    // What the bot reacted with so far answered the
                    // message before this one.
                    self.draw_bot_reactions();
                    self.land(node, &text(item), item["from"].as_str());
                    self.answered(&text(item));
                }
            }
            "queued" => self.notice = Some(format!("queued behind turn {}", data["behind"])),
            // Shown as a clip in the footer until a call takes it in.
            "attached" if turn.is_some() && turn != self.reaction_turn => {
                self.attached.extend(turn);
            }
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
                    Some(live) if !live.closed => live,
                    closed => {
                        // Words that ended with nothing after them.
                        if let Some(closed) = closed {
                            self.write_words(closed, Words::Reply);
                        }
                        Live::new(self.any_history, &self.workspace)
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
                    if let Some(node) = data["node"].as_i64() {
                        self.saw_reply(node, &text(item));
                    }
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
                    self.close_live();
                } else if let Some(item) = &item {
                    self.end_live(Words::Reply);
                    let mut live = Live::new(self.any_history, &self.workspace);
                    let rows = transcript::reply(
                        &text(item),
                        self.prose(),
                        std::path::Path::new(&self.workspace),
                    );
                    self.live_rows(&mut live, rows);
                    live.closed = true;
                    self.live = Some(live);
                }
                // Found again by their node, for the reactions under them:
                // now when written already, else once they are.
                if let Some(node) = data["node"].as_i64()
                    && let Some(live) = &mut self.live
                {
                    match live.spilled {
                        true => self.mark_last(screen::Mark::Tag(screen::Tag::End(node))),
                        false => live.node = Some(node),
                    }
                }
            }
            "compaction_text_delta" | "compaction_thinking_delta" => {
                if let Some(running) = &mut self.running {
                    running.phase = "Compacting".into();
                }
            }
            "compacted" => self.notice = Some("compacted the conversation".into()),
            "compaction_failed" => {
                self.notice = Some(format!(
                    "compaction failed: {}",
                    data["error"]
                        .as_str()
                        .or(event["error"].as_str())
                        .unwrap_or("unknown")
                ))
            }
            "tool_calls_withheld" => {
                self.notice = Some("at the round limit: this reply ends the turn".into())
            }
            "retry" => {
                if !self.said.is_empty() {
                    self.replay = Some(std::mem::take(&mut self.said));
                }
                if let Some(running) = &mut self.running {
                    running.phase = "Retrying".into();
                }
            }
            "tool_started" if data["name"] == reactions::TOOL => {
                // Counted once it completes, and drawn when the turn ends,
                // so one taken off again is never drawn.
                if let Some(reaction) = data["arguments"]
                    .as_str()
                    .and_then(|a| serde_json::from_str::<Value>(a).ok())
                    .as_ref()
                    .and_then(reactions::call)
                {
                    let id = data["call_id"].as_str().unwrap_or("").to_owned();
                    self.react_calls.insert(id, reaction);
                }
            }
            "tool_completed"
                if self
                    .react_calls
                    .contains_key(data["call_id"].as_str().unwrap_or("")) =>
            {
                let id = data["call_id"].as_str().unwrap_or("");
                // The daemon refuses a call that does not fit, or that the
                // bot may not make.
                if let Some((emoji, remove)) = self.react_calls.remove(id)
                    && data["failed"] != true
                    && data["denied"] != true
                {
                    reactions::apply(&mut self.bot_reactions, &emoji, remove);
                    // Under the message as it is made, once it can be.
                    self.place_bot_reactions();
                }
            }
            "tool_started" => {
                self.said.clear();
                self.thought.clear();
                self.end_live(Words::Step);
                let name = data["name"].as_str().unwrap_or("tool").to_owned();
                let id = data["call_id"].as_str().unwrap_or("");
                self.reviewing.remove(id);
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
                if name == margin::TOOL {
                    self.note_call(id, &arguments, turn);
                }
                self.calls.insert(id.to_owned(), (name, arguments));
            }
            "tool_completed" => {
                let id = data["call_id"].as_str().unwrap_or("");
                // A call denied before it started has no start to have
                // kept it: it is read from the bot's items.
                // Done with review, whatever came of it: a call cancelled
                // before it started ends here too.
                self.reviewing.remove(id);
                let call = match self.calls.remove(id) {
                    Some(call) => Some(call),
                    None if data["denied"] == true => self.call(id).await,
                    None => None,
                };
                let Some((name, arguments)) = call else {
                    return;
                };
                self.end_live(Words::Step);
                let mut shown = (String::new(), data["failed"] == true);
                let mut reason = String::new();
                if let Some(node) = data["node"].as_i64()
                    && let Some(item) = self
                        .items(&[node])
                        .await
                        .iter()
                        .find(|i| i["call_id"] == id)
                {
                    shown = output(item);
                    shown.1 |= data["failed"] == true;
                    reason = denial(item).unwrap_or_default();
                }
                if data["denied"] == true {
                    // It never ran: nothing it would have done follows.
                    shown.1 = true;
                    self.commit_denied(&name, &arguments, &reason);
                    // The approver's, not the person's: `/approve` may
                    // retry it once.
                    let by_auto = data["approvals"].as_array().is_some_and(|verdicts| {
                        verdicts
                            .iter()
                            .any(|v| v["tag"] == "auto" && v["allow"] == false)
                    });
                    if by_auto {
                        self.denials.retain(|d| d.call_id != id);
                        self.denials.push(Denial {
                            call_id: id.to_owned(),
                            name: name.clone(),
                            arguments: arguments.clone(),
                            reason,
                        });
                        if self.denials.len() > DENIALS {
                            self.denials.remove(0);
                        }
                    }
                } else {
                    // Started before this window read the conversation, it
                    // ends here; once applied, it counts once.
                    if name == margin::TOOL && !shown.1 {
                        self.note_call(id, &arguments, turn);
                    }
                    self.commit_call(id, &name, &arguments, &shown.0, shown.1);
                }
                if name == asks::TOOL && !shown.1 {
                    self.asked(id, &arguments);
                }
                if name == resolved::TOOL
                    && !shown.1
                    && !reactions::refused(&json!({"output": shown.0}))
                {
                    self.resolve(&arguments, id, turn, true).await;
                }
                if self.calls.is_empty() && !self.held.is_empty() {
                    let held = std::mem::take(&mut self.held);
                    self.rows_drawn += held.len();
                    self.pending.extend(held);
                }
                self.approvals.retain(|call| call["call_id"] != id);
                if let Some(running) = &mut self.running {
                    // The judge may still be reviewing the round's others.
                    running.phase = match self.approvals.is_empty() && !self.reviewing.is_empty() {
                        true => reviewing(self.reviewing.len()),
                        false => "Working".into(),
                    };
                }
            }
            "approval_requested" | "approval_asked" => self.approvals_for(turn).await,
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
                // A staged prompt whose turn ended without starting, or an
                // attached message a call took in.
                self.staged.retain(|(staged, _)| Some(*staged) != turn);
                self.attached.retain(|attached| Some(*attached) != turn);
                if self.running.as_ref().is_some_and(|r| Some(r.turn) != turn) {
                    return;
                }
                // Nothing of an ended turn is still under review.
                self.reviewing.clear();
                let mut status = data["status"].as_str().unwrap_or("unknown");
                let detail = data["error"].as_str().unwrap_or("");
                // A model round that came back empty ended the turn with
                // nothing more to say: no failure for the person to read.
                if status == "failed" && detail == EMPTY {
                    status = "completed";
                }
                let empty = detail == EMPTY;
                let running = self.running.take();
                let note = running.as_ref().is_some_and(|r| r.note);
                let seconds = running.map(|r| r.started.elapsed().as_secs());
                let shown = if empty { "" } else { detail };
                let line = transcript::finished(status, shown, seconds, self.prose());
                self.close_live();
                // A turn that only reacted ends as a reaction under the
                // message, with no line under it.
                // Rows already drawn above, as a long reply spills, count.
                let words =
                    (self.live.as_ref()).is_some_and(|live| live.spilled || !live.rows.is_empty());
                let ended = matches!(status, "completed" | "steered");
                let only_reacted = !words
                    && !self.bot_reactions.is_empty()
                    && self.answering.as_ref().map(|(_, at, _)| *at) == Some(self.rows_drawn);
                if only_reacted {
                    self.draw_bot_reactions();
                }
                // A turn the lookout's note started that ended with nothing
                // to say stays under its task updates: no line, no sound;
                // so does one whose model said nothing at the end.
                let quiet = (note || empty) && !words && ended;
                // A turn that ended well puts its reply under the line, apart
                // from the steps above; one written as it came, or one
                // that failed, keeps the line under it.
                match self.live.take() {
                    _ if (only_reacted || quiet) && ended => {}
                    Some(live) if !live.spilled && !live.rows.is_empty() && ended => {
                        self.commit(vec![line]);
                        self.write_words(Live { gap: true, ..live }, Words::Reply);
                    }
                    live => {
                        if let Some(live) = live {
                            self.write_words(live, Words::Reply);
                        }
                        self.commit(vec![line]);
                    }
                }
                self.draw_bot_reactions();
                self.approvals.clear();
                self.notice = None;
                status_report(
                    match status {
                        _ if quiet => "idle",
                        "completed" | "steered" => "done",
                        "interrupted" => "idle",
                        _ => "error",
                    },
                    &self.bot,
                );
                self.configure().await;
                // Only this view knows whether the turn said anything, so it
                // rings for the end of every turn of its own; the project's
                // other ends ring from the fleet.
                match status {
                    _ if quiet => {}
                    "completed" | "steered" => {
                        let what = format!("{} is done", self.bot);
                        self.sounds.alert(sound::Alert::LeadDone, &what)
                    }
                    "interrupted" => {}
                    _ if self.fleet.is_none() => {
                        let what = format!("{} failed", self.bot);
                        self.sounds.alert(sound::Alert::Failed, &what)
                    }
                    _ => {}
                }
            }
            "deleted" => {
                self.notice = Some(format!("{} was deleted", self.bot));
                self.running = None;
            }
            _ => {}
        }
    }

    /// The store as the daemon names it, which the marks of resolved
    /// tasks are kept beside.
    fn store(&self) -> PathBuf {
        agent_client::socket::canonical_store(&self.options.store)
            .unwrap_or_else(|_| self.options.store.clone())
    }

    /// Note the task a lead's `resolve_task` call `call` in `turn` names
    /// as resolved, up to its newest turn finished by then: `now`, as it
    /// happens, when its window closes too; replayed, by the time the
    /// lead's turn ended, unless the mark it made then is kept.
    async fn resolve(&mut self, arguments: &Value, call: &str, turn: Option<i64>, now: bool) {
        let Some(project) = &self.project else {
            return;
        };
        let (Some(task), Some(turn)) = (resolved::call(arguments, project), turn) else {
            return;
        };
        if !now && resolved::made_by(&self.store(), &task, call) {
            return;
        }
        let clock = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_millis() as i64;
        let at = match now {
            true => clock,
            false => self
                .client
                .request(
                    "turns",
                    json!({"bot": self.bot, "after": turn - 1, "limit": 1}),
                )
                .await
                .ok()
                .and_then(|page| page["turns"][0]["finished_ms"].as_i64())
                .unwrap_or(clock),
        };
        // A task that is gone has nothing to resolve.
        let Ok(page) = self
            .client
            .request("turns", json!({"bot": task, "newest": true, "limit": 16}))
            .await
        else {
            return;
        };
        let upto = resolved::upto(page["turns"].as_array().map_or(&[][..], Vec::as_slice), at);
        if let Err(error) = resolved::keep(&self.store(), &task, upto, call) {
            self.notice = Some(error);
        }
        if now && let Some(fleet) = &mut self.fleet {
            fleet.resolved(&task);
        }
    }

    /// The project's agents on one card, from the daemon and Rex alone,
    /// in the pane beside the conversation, live until Esc.
    async fn status(&mut self) {
        let (project, rows) = match self.status_rows().await {
            Ok(read) => read,
            Err(error) => {
                self.notice = Some(error);
                return;
            }
        };
        self.watch = Some(Watch {
            project,
            rows,
            read: Instant::now(),
            lookout: self.lookout_line(),
            resolved: false,
            background: None,
            picked: None,
        });
        self.open_pane(pane::Pane::card("/status", STATUS_KEYS));
    }

    /// What runs for the project's agents with no one typing:
    /// `/background` in the pane, as `/status` is, until Esc.
    fn background(&mut self) {
        let scope = self.scope();
        let project = scope.project.clone();
        let view = background::View::open(
            scope,
            self.options.store.clone(),
            self.client.store().map(str::to_owned),
            self.bot.clone(),
        );
        self.watch = Some(Watch {
            project,
            rows: Vec::new(),
            read: Instant::now(),
            lookout: String::new(),
            resolved: false,
            background: Some(view),
            picked: None,
        });
        self.open_pane(pane::Pane::card("/background", STATUS_KEYS));
    }

    async fn reread(&mut self) {
        if let Some(watch) = &mut self.watch
            && let Some(view) = &mut watch.background
        {
            view.reload();
            watch.read = Instant::now();
            return;
        }
        let lookout = self.lookout_line();
        if let Ok((_, rows)) = self.status_rows().await
            && let Some(watch) = &mut self.watch
        {
            watch.rows = rows;
            watch.read = Instant::now();
            watch.lookout = lookout;
        }
    }

    /// Count what runs for this agent in the background again, every ten
    /// seconds: files read, no daemon call.
    fn count_behind(&mut self) {
        if self
            .behind
            .1
            .is_some_and(|at| at.elapsed() < Duration::from_secs(10))
            || self.options.side.is_some()
        {
            return;
        }
        let items = background::read(&self.options.store, self.client.store(), |bot| {
            bot == self.bot
        });
        self.behind = (background::count(&items, &self.bot), Some(Instant::now()));
    }

    /// What the lookout does for this window's lead.
    fn lookout_line(&self) -> String {
        match &self.project {
            Some(_) => lookout::status(&self.options.store, &self.bot),
            None => String::new(),
        }
    }

    /// The agents `/status` and `/background` cover: the project's, and
    /// its lead; a lone agent's only.
    fn scope(&self) -> background::Scope {
        let (project, lead) = match &self.project {
            Some(project) => (project.clone(), format!("{project}.lead")),
            None => (
                self.bot.split('.').next().unwrap_or(&self.bot).to_owned(),
                self.bot.clone(),
            ),
        };
        let glob = if self.bot.contains('.') || self.project.is_some() {
            format!("{project}.*")
        } else {
            self.bot.clone()
        };
        background::Scope {
            project,
            lead,
            glob,
        }
    }

    /// The project's agents, as `/status` shows them.
    async fn status_rows(&mut self) -> Result<(String, Vec<status::Row>), String> {
        let background::Scope {
            project,
            lead,
            glob: pattern,
        } = self.scope();
        let behind = background::read(&self.options.store, self.client.store(), |bot| {
            lookout::subs::matches(&pattern, bot)
        });
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
        let store = self.store();
        let mut rows = Vec::new();
        // Side chats are not the project's agents.
        for bot in page["bots"].as_array().into_iter().flatten() {
            let Some(name) = bot["name"].as_str().filter(|_| bot["expire_ms"].is_null()) else {
                continue;
            };
            let turns = self
                .client
                .request("turns", json!({"bot": name, "newest": true, "limit": 8}))
                .await
                .ok()
                .and_then(|page| page["turns"].as_array().cloned())
                .unwrap_or_default();
            let turn = turns.first().cloned().unwrap_or(Value::Null);
            // A message steered into a running turn is recorded as a turn
            // of its own that never ran: the time is the one it joined.
            let ran = turns
                .iter()
                .find(|turn| turn["status"] != "steered")
                .unwrap_or(&Value::Null);
            let running = !bot["running_turn"].is_null();
            let state = match (
                running,
                if running { &turn } else { ran }["status"].as_str(),
            ) {
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
            // A running turn counts on from its start; one that ended
            // shows how long it took.
            let started = ran["started_ms"].as_u64();
            let finished = ran["finished_ms"].as_u64();
            let (at, seconds) = match running {
                true => (started, started.map(|at| now.saturating_sub(at) / 1000)),
                false => (
                    finished,
                    started
                        .zip(finished)
                        .map(|(began, ended)| ended.saturating_sub(began) / 1000),
                ),
            };
            let Latest { mut step, pr, ask } = self.latest(name).await;
            // A question stays open until its answer lands, past work that
            // does not wait on it and messages that answer something else:
            // an idle agent's recent history is read once per turn it
            // ended, as is whether that turn resolved the task. A running
            // turn shows its work, as its last items say; a question still
            // open shows again once it ends.
            let seen = match turn["turn"].as_i64() {
                Some(last) if !running => match self.asks_seen.get(name) {
                    Some((at, seen)) if *at == last => seen.clone(),
                    _ => {
                        let seen = self.scan(name, ran["turn"].as_i64()).await;
                        self.asks_seen.insert(name.to_owned(), (last, seen.clone()));
                        seen
                    }
                },
                _ => Seen { ask, itself: false },
            };
            let ask = seen.ask;
            let itself = seen.itself && name != lead && ran["status"] == "completed";
            let resolved = itself
                || resolved::holds(resolved::read(&store, name), turn["turn"].as_i64(), running);
            if let Some(question) = &ask {
                step = format!("asks: {}", first_line(question));
            }
            let state = match ask {
                Some(_) if !running && !resolved => "needs you".to_owned(),
                _ => state,
            };
            let group = status::group(&state, running, ask.is_some(), pr.is_some(), resolved);
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
                running,
                seconds,
                at,
                model,
                latest: step,
                place,
                pr: pr.unwrap_or_default(),
                background: background::fact(&behind, name),
                group,
            });
        }
        // The lead first, then by group, the latest first in each.
        rows.sort_by_key(|row| {
            (
                row.name != "lead",
                row.group,
                std::cmp::Reverse(row.at.unwrap_or(0)),
            )
        });
        Ok((project, rows))
    }

    /// Count the running turn's time from when the daemon started it, not
    /// from when this window first saw it run.
    async fn turn_began(&mut self) {
        let Some(turn) = self.running.as_ref().map(|r| r.turn) else {
            return;
        };
        let Ok(page) = self
            .client
            .request(
                "turns",
                json!({"bot": self.bot, "after": turn - 1, "limit": 1}),
            )
            .await
        else {
            return;
        };
        let Some(started) = page["turns"]
            .get(0)
            .filter(|row| row["turn"] == turn)
            .and_then(|row| row["started_ms"].as_u64())
        else {
            return;
        };
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_millis() as u64;
        let ago = Duration::from_millis(now.saturating_sub(started));
        if let Some(running) = &mut self.running
            && let Some(began) = Instant::now().checked_sub(ago)
        {
            running.started = began;
        }
    }

    /// What `bot`'s newest 400 nodes say: the oldest question it asked and
    /// has no answer to, and whether its turn `last` resolved it.
    async fn scan(&self, bot: &str, last: Option<i64>) -> Seen {
        let Ok(page) = self
            .client
            .request("history_nodes", json!({"bot": bot, "limit": 400}))
            .await
        else {
            return Seen::default();
        };
        let listed = page["nodes"].as_array().cloned().unwrap_or_default();
        let nodes: Vec<i64> = listed.iter().filter_map(|n| n["node"].as_i64()).collect();
        let in_last: std::collections::HashSet<i64> = listed
            .iter()
            .filter(|n| last.is_some() && n["turn"].as_i64() == last)
            .filter_map(|n| n["node"].as_i64())
            .collect();
        let mut items = Vec::new();
        let mut turn = Vec::new();
        for chunk in nodes.chunks(100) {
            // Newest first, as the nodes were asked for; each node's items
            // newest first too.
            let mut read = self.items_of(bot, chunk).await.into_iter().peekable();
            while let Some((node, item)) = read.next() {
                let mut kept = vec![item];
                while let Some((_, item)) = read.next_if(|(next, _)| *next == node) {
                    kept.push(item);
                }
                if in_last.contains(&node) {
                    turn.extend(kept.iter().cloned());
                }
                items.extend(kept.into_iter().rev());
            }
        }
        Seen {
            ask: asks::open(&items),
            itself: resolved_itself(&turn, bot),
        }
    }

    /// An agent's latest step, as a line; the pull request its latest
    /// reply links; and the question it asked the person, when that is
    /// still open. Its last few items say: asking ends a turn.
    async fn latest(&self, bot: &str) -> Latest {
        let Ok(page) = self
            .client
            .request("history_nodes", json!({"bot": bot, "limit": 4}))
            .await
        else {
            return Latest::default();
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
            return Latest::default();
        };
        // Newest first, as the nodes were asked for.
        let items: Vec<Value> = read["items"]
            .as_array()
            .into_iter()
            .flatten()
            .map(|entry| entry["item"].clone())
            .collect();
        let pr = items
            .iter()
            .find(|item| item["role"] == "assistant")
            .and_then(|item| status::pull_request(&text(item)));
        let flat: Vec<Value> = items
            .iter()
            .flat_map(|item| responses(item).into_iter().rev())
            .collect();
        Latest {
            ask: asks::open(&flat),
            pr,
            step: Self::step(items),
        }
    }

    /// The newest of `items` that reads as a step, as a line.
    fn step(items: Vec<Value>) -> String {
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
                (_, Some("user")) => {
                    let text = text(&item);
                    return match user_shell::read(&text) {
                        Some(ran) => format!("ran: ! {}", first_line(&ran.command)),
                        None => format!("asked: {}", first_line(&text)),
                    };
                }
                _ => {}
            }
        }
        String::new()
    }

    /// The turn's calls a person must answer: every `manual` one, and those
    /// the judge handed over. A program answers the rest.
    async fn approvals_for(&mut self, turn: Option<i64>) {
        let Ok(page) = self
            .client
            .request("approvals", json!({"bot": self.bot, "limit": 64}))
            .await
        else {
            return;
        };
        // What the approver serving `auto` is still reviewing: not yet
        // handed to the person.
        self.reviewing = page["approvals"]
            .as_array()
            .into_iter()
            .flatten()
            .filter(|call| {
                call["turn"].as_i64() == turn
                    && call["asked"].is_null()
                    && call["gates"]
                        .as_array()
                        .is_some_and(|gates| gates.iter().any(|g| g == "auto"))
            })
            .filter_map(|call| call["call_id"].as_str().map(str::to_owned))
            .collect();
        self.approvals = page["approvals"]
            .as_array()
            .into_iter()
            .flatten()
            .filter(|call| {
                call["turn"].as_i64() == turn
                    && (!call["asked"].is_null()
                        || call["gates"]
                            .as_array()
                            .is_some_and(|gates| gates.iter().any(|g| g == "manual")))
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
        if let Some(running) = &mut self.running
            && self.approvals.is_empty()
            && !self.reviewing.is_empty()
        {
            running.phase = reviewing(self.reviewing.len());
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
                    "request": call["request"], "tag": person_gate(&call), "decision": decision,
                    "by": "tui"}),
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
            .filter(|(name, _)| name.starts_with(&*self.input))
            .collect()
    }

    /// What the full screen takes before the rest: the mouse, a copy of
    /// the selection, and the keys that scroll. Whether it took `event`.
    fn full_screen(&mut self, event: &Event) -> bool {
        let key = match event {
            // A paste while finding is more to find.
            // A paste in the pane's program is its own.
            Event::Paste(text)
                if self.pane_keys
                    && let Some(program) = self.pane.as_ref().and_then(|p| p.program.as_ref()) =>
            {
                program.paste(text);
                return true;
            }
            // A paste over a preview or `/status` is for the composer,
            // which comes back into view to show it.
            Event::Paste(_) if self.pane_keys && self.finding.is_none() => {
                self.pane_keys = false;
                return false;
            }
            Event::Paste(text) if let Some(query) = &mut self.finding => {
                query.extend(text.chars().map(|c| if c.is_control() { ' ' } else { c }));
                self.screen.find(query);
                return true;
            }
            Event::Mouse(mouse) => {
                use ratatui::crossterm::event::{MouseButton, MouseEventKind};
                let at = (mouse.column, mouse.row).into();
                let over_pane = self.pane.as_ref().is_some_and(|p| p.area.contains(at));
                let down = matches!(mouse.kind, MouseEventKind::Down(MouseButton::Left));
                if down {
                    self.pane_mouse = over_pane;
                    self.pane_keys = over_pane;
                    // The keys go to the pane: the conversation's find ends.
                    if over_pane && self.finding.take().is_some() {
                        self.screen.end_find();
                    }
                }
                let held = matches!(
                    mouse.kind,
                    MouseEventKind::Drag(MouseButton::Left) | MouseEventKind::Up(MouseButton::Left)
                );
                let in_pane = if held { self.pane_mouse } else { over_pane };
                let action = match &mut self.pane {
                    Some(pane) if in_pane => {
                        // A button on its first row: the margin's glance, or
                        // the arrow back to the margin.
                        if down
                            && pane.on_title(mouse.column, mouse.row)
                            && let Some(button) = pane.head_button(mouse.column)
                        {
                            self.follow(&button);
                            return true;
                        }
                        // ‹ and › go back and forward, as a browser's do.
                        if down && let Some(back) = pane.on_arrow(mouse.column, mouse.row) {
                            self.go(back);
                            return true;
                        }
                        // A click on its name opens the file outside Sonny.
                        if down
                            && pane.on_title(mouse.column, mouse.row)
                            && let Some(url) = pane.url()
                        {
                            if let Err(error) = open(&url) {
                                self.notice = Some(error);
                            }
                            return true;
                        }
                        if let Some(program) = &pane.program {
                            let (x, y) = pane.origin();
                            program.mouse(*mouse, x, y);
                            return true;
                        }
                        pane.screen.mouse(*mouse)
                    }
                    _ => {
                        let up = mouse.kind == MouseEventKind::ScrollUp;
                        if up && self.screen.at_top() && self.older.is_some() {
                            self.want_older = true;
                        }
                        // A press here lets go of what was selected there.
                        if down && let Some(pane) = &mut self.pane {
                            pane.screen.unselect();
                        }
                        self.screen.mouse(*mouse)
                    }
                };
                match action {
                    Some(screen::Action::Copy(text)) => self.copied(&text),
                    // A folder in the pane's tree opens there, in place.
                    Some(screen::Action::Open(link))
                        if in_pane && self.pane.as_mut().is_some_and(|p| p.toggle(&link)) => {}
                    Some(screen::Action::Open(link)) => self.follow(&link),
                    None => {}
                }
                return true;
            }
            Event::Key(key) if key.kind != KeyEventKind::Release => key,
            _ => return false,
        };
        // Ctrl+[ and Ctrl+] step the pane, but for a program that has them.
        let esc = KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE);
        let program = self.pane_keys && self.pane.as_ref().is_some_and(|p| p.program.is_some());
        if self.finding.is_none()
            && !program
            && let Some(back) = steps(key)
            && (self.step_pane(back) || !back)
        {
            return true;
        }
        // Ctrl+[ with nowhere to step is Esc, as it is without the protocol.
        let key = match steps(key) {
            Some(true) if !program => &esc,
            _ => key,
        };
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        let copy =
            key.code == KeyCode::Char('c') && (ctrl || key.modifiers.contains(KeyModifiers::SUPER));
        if self.finding.is_none()
            && let Some(pane) = &mut self.pane
        {
            // A program has every key but Ctrl+\, which comes back here.
            if self.pane_keys
                && let Some(program) = &pane.program
            {
                match switches(key) {
                    true => self.pane_keys = false,
                    false => program.key(*key),
                }
                return true;
            }
            // `/background` takes its own keys: picking, firing, removing.
            if self.pane_keys
                && pane.is_card()
                && let Some(view) = self.watch.as_mut().and_then(|w| w.background.as_mut())
            {
                // A key the pane takes keeps the item a removal was asked
                // for, as any other key does.
                let scrolls = matches!(
                    key.code,
                    KeyCode::Tab
                        | KeyCode::PageUp
                        | KeyCode::PageDown
                        | KeyCode::Home
                        | KeyCode::End
                );
                if scrolls || (copy && pane.screen.selected().is_some()) {
                    view.keep();
                }
                match key.code {
                    _ if switches(key) || key.code == KeyCode::Tab => self.pane_keys = false,
                    _ if copy && let Some(text) = pane.screen.selected() => {
                        pane.screen.unselect();
                        self.copied(&text);
                    }
                    KeyCode::PageUp => pane.screen.page(true),
                    KeyCode::PageDown => pane.screen.page(false),
                    KeyCode::Home => pane.screen.hold(usize::MAX),
                    KeyCode::End => pane.screen.latest(),
                    _ => {
                        if view.key(*key) == background::Key::Close {
                            self.close_pane(true);
                        }
                    }
                }
                return true;
            }
            if self.pane_keys {
                match key.code {
                    // Back to the margin from what is over it; the margin closed.
                    KeyCode::Esc => self.close_pane(true),
                    // `/status` lists the resolved tasks, or only counts them.
                    KeyCode::Char('r')
                        if pane.is_card()
                            && let Some(watch) = &mut self.watch =>
                    {
                        watch.resolved = !watch.resolved
                    }
                    _ if switches(key) || key.code == KeyCode::Tab => self.pane_keys = false,
                    _ if copy && let Some(text) = pane.screen.selected() => {
                        pane.screen.unselect();
                        self.copied(&text);
                    }
                    // Back and forward through what the pane has shown.
                    KeyCode::Left | KeyCode::Backspace if pane.can_go(true) => self.go(true),
                    KeyCode::Right if pane.can_go(false) => self.go(false),
                    KeyCode::Up => pane.screen.scroll(1),
                    KeyCode::Down => pane.screen.scroll(-1),
                    KeyCode::PageUp => pane.screen.page(true),
                    KeyCode::PageDown | KeyCode::Char(' ') => pane.screen.page(false),
                    KeyCode::Home => pane.screen.hold(usize::MAX),
                    KeyCode::End => pane.screen.latest(),
                    // Anything else is for the composer, as it was typed.
                    _ => {
                        self.pane_keys = false;
                        return false;
                    }
                }
                return true;
            }
            // Ctrl+\, or Tab with nothing typed, puts the keys in the pane.
            if switches(key)
                || key.code == KeyCode::Tab
                    && self.input.is_empty()
                    && self.picker.is_none()
                    && self.choosing.is_none()
                    && self.reacting.is_none()
            {
                pane.screen.unselect();
                self.pane_keys = true;
                return true;
            }
        }
        if let Some(query) = &mut self.finding {
            match key.code {
                KeyCode::Esc => self.finding = None,
                _ if copy => self.finding = None,
                KeyCode::Up | KeyCode::Enter => self.screen.find_next(true),
                KeyCode::Char('f' | 'p') if ctrl => self.screen.find_next(true),
                KeyCode::Down => self.screen.find_next(false),
                KeyCode::Char('n') if ctrl => self.screen.find_next(false),
                KeyCode::Backspace => {
                    query.pop();
                    self.screen.find(query);
                }
                KeyCode::Char(c) if !ctrl => {
                    query.push(c);
                    self.screen.find(query);
                }
                KeyCode::PageUp => self.page_up(),
                KeyCode::PageDown => self.screen.page(false),
                _ => {}
            }
            if self.finding.is_none() {
                self.screen.end_find();
            }
            return true;
        }
        match key.code {
            // Over the composer, not over a list open on it.
            KeyCode::Char('f')
                if ctrl
                    && self.picker.is_none()
                    && self.choosing.is_none()
                    && self.reacting.is_none() =>
            {
                self.screen.unselect();
                self.finding = Some(String::new());
                self.screen.find("");
            }
            _ if copy && let Some(text) = self.screen.selected() => {
                self.copied(&text);
                self.screen.unselect();
            }
            KeyCode::Esc if self.screen.unselect() => {}
            KeyCode::PageUp => self.page_up(),
            KeyCode::PageDown => self.screen.page(false),
            // Sending goes back to the newest rows, where its answer comes.
            KeyCode::Enter => {
                self.screen.latest();
                return false;
            }
            _ => return false,
        }
        true
    }

    /// `pane` beside the conversation, with the keys; what was selected
    /// or being found in the conversation let go.
    fn open_pane(&mut self, mut pane: pane::Pane) {
        self.closed = None;
        // Over the margin, kept under it to go back to; what else was
        // open goes.
        if let Some(open) = self.pane.take()
            && open.is_margin()
        {
            self.under = Some(open);
        }
        pane.over = self.under.is_some();
        self.pane = Some(pane);
        self.pane_keys = true;
        self.screen.unselect();
        if self.finding.take().is_some() {
            self.screen.end_find();
        }
    }

    /// What is open beside the conversation closed, as Esc does: what was
    /// laid over the margin goes, back to it, `keep` saying whether the
    /// keys stay there, and is put aside for Ctrl+]; the margin itself
    /// stays closed until asked for.
    fn close_pane(&mut self, keep: bool) {
        match self.pane.take() {
            Some(open) if open.is_margin() => {
                self.margin.close(true);
                self.pane_keys = false;
            }
            // Put aside, with its card, for Ctrl+] to open again.
            Some(open) => {
                self.closed = Some((open, self.watch.take()));
                self.pane = self.under.take();
                self.pane_keys &= keep && self.pane.is_some();
            }
            None => {}
        }
    }

    /// The margin back beside the conversation when nothing else is
    /// there, unless the person closed it.
    fn margin_back(&mut self) {
        if self.margin.closed() {
            self.under = None;
            if let Some(over) = &mut self.pane {
                over.over = false;
            }
        }
        if self.pane.is_none() && !self.margin.closed() {
            self.pane = Some(self.under.take().unwrap_or_else(pane::Pane::margin));
        }
    }

    /// `/margin`: the margin shown, with the keys, or closed.
    fn toggle_margin(&mut self) {
        let shown = !self.margin.closed();
        self.margin.close(shown);
        if shown {
            if self.pane.as_ref().is_some_and(pane::Pane::is_margin) {
                self.pane = None;
                self.pane_keys = false;
            }
            self.margin_back();
            return;
        }
        match &mut self.pane {
            // Under what is open, to go back to.
            Some(over) => {
                over.over = true;
                self.under = Some(pane::Pane::margin());
            }
            None => {
                self.margin_back();
                self.pane_keys = true;
            }
        }
    }

    /// What runs for the agent, for the margin's first row: its tasks at
    /// work, what waits on the person, its background, its context.
    fn glance(&self) -> margin::Glance {
        let doing = |state: &str| {
            (self.fleet.iter().flat_map(|fleet| fleet.doing.iter()))
                .filter(|(bot, doing)| **bot != self.bot && **doing == state)
                .count()
        };
        margin::Glance {
            name: self.bot.clone(),
            running: doing("working"),
            waiting: self.asks.len() + doing("needs you"),
            behind: self.behind.0,
            context: self.context,
        }
    }

    /// A button clicked: what Sonny does for it now, or once the loop
    /// gets to it.
    fn act(&mut self, what: &str) {
        let (verb, id) = what.split_once('/').unwrap_or((what, ""));
        match verb {
            // The margin scrolled to the note, shown if it was not.
            "note" => {
                if self.margin.closed() {
                    self.toggle_margin();
                }
                if !self.pane.as_ref().is_some_and(pane::Pane::is_margin) {
                    self.close_pane(true);
                    self.margin_back();
                }
                // Shown where there is no room for it beside the
                // conversation too: the keys go to it.
                self.pane_keys = true;
                self.margin_focus = self.margin.note(id).map(|note| margin::anchor(&note.call));
            }
            // The conversation back where the note was put.
            "jump" => {
                let Some(note) = self.margin.note(id) else {
                    return;
                };
                // The conversation in view again, where the margin
                // covered it.
                self.pane_keys = false;
                let pin = screen::Tag::Note(margin::anchor(&note.call));
                let after = note.after;
                if self.screen.show_tagged(pin, 2) {
                    return;
                }
                // Drawn back to the message it answered, or a page further
                // at a time.
                if let (Some(after), Some(first)) = (after, self.first_node)
                    && after < first
                {
                    self.first_node = Some(after);
                    self.hold_view = true;
                    self.repaint = true;
                } else if self.older.is_some() {
                    self.want_older = true;
                } else {
                    self.notice = Some("that note's place is no longer in the conversation".into());
                    return;
                }
                self.jump = Some((pin, JUMP_PAGES));
                if self.running.is_some() {
                    self.notice = Some("going back once this turn ends".into());
                }
            }
            "back" => self.close_pane(true),
            _ => self.pressed.push(what.to_owned()),
        }
    }

    /// A button's work that waits on the daemon: a note taken away, the
    /// agent told; `/status` or `/background` opened; a row's button in
    /// the conversation.
    async fn press(&mut self, what: &str) {
        let (verb, id) = what.split_once('/').unwrap_or((what, ""));
        match verb {
            "close" => {
                if self.margin.note(id).is_none() {
                    return;
                }
                // The person's click, said in Sonny's words: nothing the
                // agent wrote, and asking for nothing.
                let told = margin::closed_message(id);
                let sent = self
                    .client
                    .request(
                        "submit",
                        json!({"bot": self.bot, "request_id": unique("tui"), "prompt": told,
                            "delivery": "attach", "origin": "margin"}),
                    )
                    .await;
                match sent {
                    // No clip in the footer: the margin says it.
                    // Taken away once the agent will hear of it; kept to
                    // close again when it could not be told.
                    Ok(reply) => {
                        self.margin.remove(id, None);
                        self.reaction_turn = reply["turn"].as_i64();
                        if let Some(turn) = reply["turn"].as_i64() {
                            self.margin.applied(&format!("t{turn}"));
                        }
                    }
                    Err(error) => self.notice = Some(error.to_string()),
                }
            }
            "status" if self.options.side.is_none() => self.status().await,
            "background" if self.options.side.is_none() => self.background(),
            "status" | "background" => {}
            _ => self.press_row(what).await,
        }
    }

    /// A jump to where a note was put, once the conversation is drawn
    /// back far enough: shown, or the next page older read.
    async fn jumped(&mut self) {
        let Some((pin, pages)) = self.jump else {
            return;
        };
        if self.repaint || self.want_older || self.running.is_some() {
            return;
        }
        if self.screen.show_tagged(pin, 2) {
            self.jump = None;
        } else if pages > 0 && self.older.is_some() {
            self.jump = Some((pin, pages - 1));
            self.older().await;
        } else {
            self.jump = None;
            self.notice = Some("that note's place is no longer in the conversation".into());
        }
    }

    /// A link clicked in the conversation: a file is shown in the pane, as
    /// a preview, and anything else opened as the system opens it.
    fn follow(&mut self, link: &str) {
        if let Some(what) = link.strip_prefix(screen::BUTTON) {
            self.act(what);
            return;
        }
        if link.starts_with("file://") {
            match self.preview(link) {
                // After the file shown, as a browser goes on to a page.
                Ok(pane) => match self.pane.take() {
                    Some(shown) if shown.url().is_some() => self.open_pane(pane.after(shown)),
                    // The margin goes under it, as under anything opened.
                    shown => {
                        self.pane = shown;
                        self.open_pane(pane)
                    }
                },
                Err(error) => self.notice = Some(error),
            }
            return;
        }
        if let Err(error) = open(link) {
            self.notice = Some(error);
        }
    }

    /// The file `link` names, read for the pane at its width.
    fn preview(&self, link: &str) -> Result<pane::Pane, String> {
        let workspace = std::path::Path::new(&self.workspace);
        let width = pane::width(self.full_width, PROSE as u16).max(20) as usize;
        pane::Pane::preview(link, workspace, width)
    }

    /// Ctrl+[ (`back`) or Ctrl+]: the pane back or forward through what
    /// it showed, the keys staying where they are; back from the first
    /// thing it showed closes it, and forward with it closed opens again
    /// the one closed last. Whether there was somewhere to go.
    fn step_pane(&mut self, back: bool) -> bool {
        match &self.pane {
            Some(pane) if pane.can_go(back) => {
                let keys = self.pane_keys;
                self.go(back);
                self.pane_keys = keys;
            }
            // Back from the first thing shown over the margin is the margin;
            // the margin has nothing before it.
            Some(pane) if back && !pane.is_margin() => self.close_pane(true),
            _ if !back
                && self.closed.is_some()
                && self.pane.as_ref().is_none_or(pane::Pane::is_margin) =>
            {
                let (pane, watch) = self.closed.take().expect("a closed pane");
                self.open_pane(pane);
                self.watch = watch;
            }
            _ => return false,
        }
        true
    }

    /// The pane back (`back`) or forward to what it showed, as it was left.
    fn go(&mut self, back: bool) {
        if let Some(pane) = self.pane.take() {
            self.open_pane(pane.go(back));
        }
    }

    /// Where the conversation and the pane go in `area`, the conversation
    /// none when the pane takes the whole of a narrow window, and the
    /// pane none when it is closed or the conversation is shown instead.
    fn split(&self, area: Rect) -> (Rect, Option<Rect>) {
        let Some(open) = &self.pane else {
            return (area, None);
        };
        // The margin where the conversation leaves room for it, or while
        // the keys are in it; out of the way otherwise.
        let margin = pane::margin_width(area.width, PROSE as u16);
        if open.is_margin() && margin.is_none() && !self.pane_keys {
            return (area, None);
        }
        let pane = match margin.filter(|_| open.is_margin()) {
            Some(margin) => margin,
            None => pane::width(area.width, PROSE as u16),
        };
        if pane == area.width {
            return match self.pane_keys {
                true => (Rect { width: 0, ..area }, Some(area)),
                false => (area, None),
            };
        }
        let chat = Rect {
            width: area.width - pane - 1,
            ..area
        };
        let pane = Rect {
            x: area.right() - pane,
            width: pane,
            ..area
        };
        (chat, Some(pane))
    }

    /// A page up the full screen, or the page before its first row.
    fn page_up(&mut self) {
        if self.screen.at_top() && self.older.is_some() {
            self.want_older = true;
        }
        self.screen.page(true);
    }

    /// The width the conversation is written at.
    fn prose(&self) -> usize {
        self.width.min(PROSE)
    }

    /// `text` put on the clipboard, and said so.
    fn copied(&mut self, text: &str) {
        term::copy(text);
        let count = text.chars().count();
        self.notice = Some(match count {
            1 => "copied 1 character".into(),
            _ => format!("copied {count} characters"),
        });
    }

    async fn key(&mut self, mut event: Event) {
        // Keys a shell reads as others: Ctrl-H is the backspace a terminal
        // sends as 0x08, which reads as Ctrl and `h` (with Alt, it deletes
        // a word, as Alt-Backspace does); Ctrl-P and Ctrl-N go up and down.
        if let Event::Key(key) = &mut event
            && key.modifiers.contains(KeyModifiers::CONTROL)
            && let Some(code) = match key.code {
                KeyCode::Char('h') => Some(KeyCode::Backspace),
                KeyCode::Char('p') => Some(KeyCode::Up),
                KeyCode::Char('n') => Some(KeyCode::Down),
                _ => None,
            }
        {
            key.code = code;
            key.modifiers -= KeyModifiers::CONTROL;
        }
        if self.full_screen(&event) {
            for what in std::mem::take(&mut self.pressed) {
                self.press(&what).await;
            }
            return;
        }
        if let Some(pick) = &mut self.reacting {
            if let Event::Key(key) = event
                && key.kind != KeyEventKind::Release
            {
                let count = self.replies.len().max(1);
                match key.code {
                    KeyCode::Up => *pick = (*pick + 1) % count,
                    KeyCode::Down => *pick = (*pick + count - 1) % count,
                    KeyCode::Enter => {
                        let at = self.replies.len().saturating_sub(1 + *pick);
                        self.react_to = self.replies.get(at).cloned();
                        self.reacting = None;
                    }
                    KeyCode::Esc => self.reacting = None,
                    KeyCode::Char('c') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                        self.reacting = None
                    }
                    _ => {}
                }
            }
            return;
        }
        if let Some(pick) = &mut self.approving {
            if let Event::Key(key) = event
                && key.kind != KeyEventKind::Release
            {
                let count = self.denials.len().max(1);
                match key.code {
                    KeyCode::Up => *pick = (*pick + count - 1) % count,
                    KeyCode::Down => *pick = (*pick + 1) % count,
                    KeyCode::Enter => {
                        let at = *pick;
                        // The person approves only what they have read:
                        // the whole call first, then a second Enter.
                        if let Some(denied) = self.denials.get(at)
                            && self.approve_shown.as_deref() != Some(denied.call_id.as_str())
                        {
                            let lines = retry_shown(denied);
                            // Past what the screen keeps, the start would
                            // scroll away unread.
                            if lines.len() > RETRY_ROWS {
                                self.notice = Some(format!(
                                    "{} lines is too long to read here: say in a message what to retry",
                                    lines.len() - 1
                                ));
                                return;
                            }
                            self.approve_shown = Some(denied.call_id.clone());
                            self.commit(lines);
                            self.notice = Some("enter again to approve this exact call".into());
                            return;
                        }
                        self.approving = None;
                        self.approve_shown = None;
                        if at < self.denials.len() {
                            let denied = self.denials.remove(at);
                            self.notice =
                                Some(match self.send(approval(&denied), None, false, &[]).await {
                                    Ok(()) => {
                                        "approved one retry; the retry is still reviewed".into()
                                    }
                                    Err(error) => {
                                        self.denials.insert(at, denied);
                                        error
                                    }
                                });
                        }
                    }
                    KeyCode::Esc => {
                        self.approving = None;
                        self.approve_shown = None;
                    }
                    KeyCode::Char('c') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                        self.approving = None;
                        self.approve_shown = None;
                    }
                    _ => {}
                }
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
                        // The agents the lead made take it too, even when
                        // the lead has it already.
                        self.spread = Some((mode, Instant::now()));
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
                    self.input.set(picked);
                    self.pick = 0;
                    return;
                }
                KeyCode::Enter
                    if !key
                        .modifiers
                        .intersects(KeyModifiers::SHIFT | KeyModifiers::ALT) =>
                {
                    self.input.set(picked);
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
            // Image files dropped on the terminal arrive as their paths.
            Event::Paste(text) => {
                match attach::paths(&text, std::path::Path::new(&self.workspace)) {
                    Some(paths) => {
                        for path in paths {
                            self.attach(attach::load(&path));
                        }
                    }
                    None => self
                        .input
                        .insert(&text.replace("\r\n", "\n").replace('\r', "\n")),
                }
            }
            Event::Resize(width, _) => self.width = width as usize,
            Event::Key(key) if key.kind != KeyEventKind::Release => {
                let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
                // Option on a Mac: terminals send Option-← and → as
                // Alt-b and Alt-f unless set to send them as arrows.
                let alt = key.modifiers.contains(KeyModifiers::ALT);
                match key.code {
                    KeyCode::Char('c') if ctrl => {
                        if self.input.is_empty() {
                            self.quit = true;
                        } else {
                            self.discard();
                        }
                    }
                    KeyCode::Char('d') if ctrl && self.input.is_empty() => self.quit = true,
                    KeyCode::Char('d') if ctrl => self.input.delete(),
                    KeyCode::Char(c @ ('y' | 'n'))
                        if !ctrl && !alt && !self.approvals.is_empty() && self.input.is_empty() =>
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
                    KeyCode::Char('a') if ctrl => self.input.home(),
                    KeyCode::Char('e') if ctrl => self.input.end(),
                    KeyCode::Char('b') if ctrl => self.input.left(),
                    KeyCode::Char('f') if ctrl => self.input.right(),
                    KeyCode::Char('u') if ctrl => self.input.delete_to_start(),
                    KeyCode::Char('k') if ctrl => self.input.delete_to_end(),
                    KeyCode::Char('w') if ctrl => self.input.delete_word(),
                    KeyCode::Char('y') if ctrl => self.input.yank(),
                    // Ctrl-J is the line feed a terminal sends for it.
                    KeyCode::Char('j') if ctrl => self.input.insert("\n"),
                    // A picture on the clipboard, as Codex and Claude Code
                    // take one: terminals paste only text.
                    KeyCode::Char('v') if ctrl => match attach::clipboard() {
                        Ok(Some(picture)) => self.attach(Ok(picture)),
                        Ok(None) => self.notice = Some("no picture on the clipboard".into()),
                        Err(error) => self.notice = Some(error),
                    },
                    KeyCode::Char('b') if alt => self.input.word_left(),
                    KeyCode::Char('f') if alt => self.input.word_right(),
                    KeyCode::Char('d') if alt => self.input.delete_word_forward(),
                    KeyCode::Char(c) if !ctrl => self.input.insert(c.encode_utf8(&mut [0; 4])),
                    KeyCode::Backspace if ctrl || alt => self.input.delete_word(),
                    KeyCode::Backspace => self.input.backspace(),
                    KeyCode::Delete if ctrl || alt => self.input.delete_word_forward(),
                    KeyCode::Delete => self.input.delete(),
                    KeyCode::Left if ctrl || alt => self.input.word_left(),
                    KeyCode::Right if ctrl || alt => self.input.word_right(),
                    KeyCode::Left => self.input.left(),
                    KeyCode::Right => self.input.right(),
                    KeyCode::Home => self.input.home(),
                    KeyCode::End => self.input.end(),
                    KeyCode::Up => {
                        self.input.up(self.prose());
                    }
                    KeyCode::Down => {
                        self.input.down(self.prose());
                    }
                    // Alt-Enter too, for terminals that send Shift-Enter
                    // as a plain Enter.
                    KeyCode::Enter if alt || key.modifiers.contains(KeyModifiers::SHIFT) => {
                        self.input.insert("\n")
                    }
                    KeyCode::Enter => self.submit().await,
                    KeyCode::Esc if self.react_to.is_some() => self.react_to = None,
                    KeyCode::Esc => {
                        if let Some(shell) = &self.shell {
                            shell.stop();
                        } else if let Some(running) = &self.running {
                            let turn = running.turn;
                            let _ = self
                                .client
                                .request("interrupt", json!({"bot": self.bot, "turn": turn}))
                                .await;
                        } else if self.options.side.is_some() {
                            // Back to the agent; the copy goes once idle.
                            if self.input.is_empty() {
                                self.quit = true;
                            } else {
                                self.discard();
                            }
                        } else if !self.asks.is_empty() {
                            self.chatting = !self.chatting;
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
        if let Some(command) = prompt.strip_prefix('!') {
            let command = command.trim();
            if command.is_empty() {
                self.notice = Some("! runs a shell command: type one after it".into());
            } else if self.shell.is_some() {
                self.notice = Some("a command of yours still runs; esc stops it".into());
            } else {
                match user_shell::start(command, &self.workspace) {
                    Ok(running) => {
                        self.shell = Some(running);
                        self.input.clear();
                        self.notice = None;
                    }
                    Err(error) => self.notice = Some(error),
                }
            }
            return;
        }
        if prompt == "/margin" {
            self.input.clear();
            self.toggle_margin();
            return;
        }
        if matches!(prompt.as_str(), "/status" | "/agents" | "/ps") {
            self.input.clear();
            // The side chat already holds the screen /status would take.
            if self.options.side.is_some() {
                self.notice = Some("/status is for the agent itself, not a side chat".into());
            } else {
                self.status().await;
            }
            return;
        }
        if matches!(prompt.as_str(), "/background" | "/triggers" | "/lookout") {
            self.input.clear();
            if self.options.side.is_some() {
                self.notice = Some("/background is for the agent itself, not a side chat".into());
            } else {
                self.background();
            }
            return;
        }
        if prompt == "/mcp" {
            self.input.clear();
            let lines = mcp::card();
            // In the place of `/status` or `/background`, if open.
            self.watch = None;
            let mut pane = pane::Pane::card("/mcp", "esc closes");
            pane.set_card(lines, "esc closes");
            self.open_pane(pane);
            return;
        }
        if prompt == "/open" {
            self.input.clear();
            let page = transcript::PAGE.lock().ok().and_then(|page| page.clone());
            self.notice = Some(match page {
                Some(page) => match open(&page.to_string_lossy()) {
                    Ok(()) => "opened in your browser".to_owned(),
                    Err(error) => error,
                },
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
        if prompt == "/compact" {
            self.input.clear();
            self.notice = Some(
                match self
                    .client
                    .request("compact", json!({"bot": self.bot}))
                    .await
                {
                    Ok(_) if self.running.is_some() => "compacts at its next step".into(),
                    Ok(_) => "compacts before its next reply".into(),
                    Err(error) => format!("compact: {error}"),
                },
            );
            return;
        }
        if prompt == "/react" {
            self.input.clear();
            if self.replies.is_empty() {
                self.notice = Some("no reply to react to yet".into());
            } else {
                self.reacting = Some(0);
            }
            return;
        }
        if prompt == "/approve" {
            self.input.clear();
            match self.denials.len() {
                0 => self.notice = Some("no recent auto-review denials".into()),
                newest => self.approving = Some(newest - 1),
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
        if prompt == "/verbose" {
            self.input.clear();
            let mut settings = settings::Settings::load();
            settings.verbose = !self.verbose;
            self.notice = Some(match settings.save() {
                Ok(()) => {
                    self.verbose = settings.verbose;
                    // What is already drawn is drawn again in the new view.
                    self.repaint = true;
                    match self.verbose {
                        true => "verbose: every tool call",
                        false => "clean: replies, tasks, questions and the plan",
                    }
                    .to_owned()
                }
                Err(error) => error,
            });
            return;
        }
        if prompt == "/quit" {
            self.input.clear();
            self.quit = true;
            return;
        }
        if let Some(question) = prompt
            .strip_prefix("/side")
            .filter(|rest| rest.is_empty() || rest.starts_with(char::is_whitespace))
        {
            let pictures = self.placed(question);
            self.input.clear();
            self.pictures.clear();
            self.side(question.trim().to_owned(), &pictures).await;
            return;
        }
        // What is typed answers the lead's first open question, and goes
        // with it; the question closes once the daemon has the answer.
        let answering = (!self.chatting)
            .then(|| self.asks.first().cloned())
            .flatten();
        let prompt = match &answering {
            Some(ask) => asks::answer(ask, &prompt),
            None => prompt,
        };
        if answering.is_none()
            && let Some(emoji) = transcript::shortcodes(&prompt)
                .or_else(|| transcript::reaction(&prompt).map(str::to_owned))
        {
            self.react(emoji).await;
            return;
        }
        // Words are a message, whatever reply `/react` picked.
        self.react_to = None;
        // What the person ran and the agent could not be given yet goes
        // first, each on its own, and this message joins the turn they start.
        let mut join = false;
        while let Some(record) = self.unsent.first().cloned() {
            if let Err(error) = self.send(record.clone(), Some(SHELL), join, &[]).await {
                self.notice = Some(error);
                return;
            }
            self.ran_here.extend(user_shell::read(&record));
            self.unsent.remove(0);
            join = true;
        }
        let pictures = self.placed(&prompt);
        match self.send(prompt, None, join, &pictures).await {
            Ok(()) => {
                self.input.clear();
                self.notice = None;
                self.pictures.clear();
                if let Some(ask) = answering {
                    self.asks.retain(|open| open.id != ask.id);
                    self.sent_answers.push(ask.question);
                    if self.asks.is_empty() {
                        self.chatting = false;
                    }
                }
            }
            Err(error) => self.notice = Some(error),
        }
    }

    /// Drop what is being written, with the pictures it was given.
    fn discard(&mut self) {
        self.input.clear();
        self.pictures.clear();
    }

    /// The pictures whose placeholders are still in `prompt`, which go
    /// with it.
    fn placed(&self, prompt: &str) -> Vec<attach::Picture> {
        (self.pictures.iter().enumerate())
            .filter(|(at, _)| prompt.contains(&attach::placeholder(at + 1)))
            .filter_map(|(_, picture)| picture.clone())
            .collect()
    }

    /// Send `prompt`: a turn of its own, or a steer of the running one (or
    /// of the one just started, `join`); staged until it lands. `origin`
    /// names what sent it when the person's words did not.
    async fn send(
        &mut self,
        prompt: String,
        origin: Option<&str>,
        join: bool,
        pictures: &[attach::Picture],
    ) -> Result<(), String> {
        // A message to a busy bot joins its running turn, on that turn's model.
        let delivery = (join || self.running.is_some()).then_some("steer");
        let model = delivery
            .is_none()
            .then(|| self.turn_model.clone())
            .flatten();
        // A steer never moves a bot, so a moved lead comes back with the
        // next message that starts a turn.
        let workspace = delivery.is_none().then(|| self.move_to.clone()).flatten();
        let reply = self
            .client
            .request(
                "submit",
                json!({"bot": self.bot, "request_id": unique("tui"), "prompt": prompt,
                    "delivery": delivery, "model": model, "workspace": workspace,
                    "origin": origin, "images": attach::submitted(pictures)}),
            )
            .await
            .map_err(|error| error.to_string())?;
        if let Some(turn) = reply["turn"].as_i64() {
            self.staged.push((turn, prompt));
        }
        if workspace.is_some() {
            self.move_to = None;
        }
        Ok(())
    }

    /// A picture for the message being written, behind a placeholder put
    /// where the cursor is; or why there is none.
    fn attach(&mut self, picture: Result<attach::Picture, String>) {
        for (at, picture) in self.pictures.iter_mut().enumerate() {
            let placeholder = attach::placeholder(at + 1);
            if !self.input.contains(&placeholder) && !self.input.killed().contains(&placeholder) {
                *picture = None;
            }
        }
        // The daemon takes a message's pictures up to a size together.
        let placed: usize = (self.placed(&self.input).iter())
            .map(|placed| placed.data.len())
            .sum();
        match picture {
            Ok(picture) if placed + picture.data.len() > attach::MOST_BYTES => {
                self.notice = Some(format!(
                    "a message's pictures may be {} MB together; this one makes {} MB",
                    attach::MOST_BYTES / 1024 / 1024,
                    (placed + picture.data.len()).div_ceil(1024 * 1024)
                ));
            }
            Ok(picture) => {
                self.pictures.push(Some(picture));
                self.input
                    .insert(&format!("{} ", attach::placeholder(self.pictures.len())));
                self.notice = None;
            }
            Err(error) => self.notice = Some(error),
        }
    }

    /// A reply the bot gave, which `/react` offers until newer ones push
    /// it out.
    fn saw_reply(&mut self, node: i64, reply: &str) {
        if reply.is_empty() || self.replies.iter().any(|(seen, _)| *seen == node) {
            return;
        }
        let first = reply
            .lines()
            .find(|line| !line.trim().is_empty())
            .unwrap_or("");
        self.replies
            .push((node, transcript::cut(first.trim(), QUOTED)));
        if self.replies.len() > REACTABLE {
            self.replies.remove(0);
        }
    }

    /// The question an `ask` call put, open until it is answered.
    fn asked(&mut self, id: &str, arguments: &Value) {
        if let Some(ask) = asks::call(id, arguments)
            && !self.asks.iter().any(|open| open.id == ask.id)
        {
            self.asks.push(ask);
        }
    }

    /// The question a message answered, from this window or another,
    /// closed.
    fn answered(&mut self, message: &str) {
        // An answer sent from here closed its question when it was sent.
        if let Some(question) = asks::answered(message)
            && let Some(at) = self.sent_answers.iter().position(|q| q == question)
        {
            self.sent_answers.remove(at);
            return;
        }
        asks::close(&mut self.asks, message);
        if self.asks.is_empty() {
            self.chatting = false;
        }
    }

    /// The bot's reactions so far to the person's message, right under
    /// it; or, when that message is no longer in the conversation, under
    /// what was drawn last, with a row of their own that says which.
    fn draw_bot_reactions(&mut self) {
        if self.place_bot_reactions() {
            return;
        }
        let on = std::mem::take(&mut self.bot_reactions);
        if on.is_empty() {
            return;
        }
        let emoji = on.join(" ");
        match &self.answering {
            Some((_, at, _)) if *at == self.rows_drawn => {
                self.commit_after(vec![transcript::reacted(&emoji)], false);
            }
            answering => {
                let what = match answering {
                    Some((first, ..)) => format!("↳ reacted {emoji} to “{first}”"),
                    None => format!("↳ reacted {emoji}"),
                };
                let rows = transcript::wrap(
                    Line::from(what),
                    self.prose(),
                    Span::raw("  "),
                    Span::raw("    "),
                );
                self.commit(transcript::quiet(rows));
            }
        }
        // Another reaction now goes under this one's row.
        if let Some((_, at, _)) = &mut self.answering {
            *at = self.rows_drawn;
        }
    }

    /// The bot's reactions so far, none included, as the row under the
    /// person's message they answer; whether that message is in the
    /// conversation to put them under.
    fn place_bot_reactions(&mut self) -> bool {
        let Some((.., node)) = self.answering else {
            return false;
        };
        let tag = screen::Tag::Reactions(node);
        // None now and none drawn: nothing to put or take off.
        if self.bot_reactions.is_empty()
            && !self.screen.has(tag)
            && !(self.marks.iter()).any(|(_, mark)| *mark == screen::Mark::Tag(tag))
        {
            return true;
        }
        let row = (!self.bot_reactions.is_empty())
            .then(|| transcript::reacted(&self.bot_reactions.join(" ")));
        self.put_under(screen::Tag::End(node), tag, row.clone())
            || self.put_under(screen::Tag::Relayed(node), tag, row)
    }

    /// The person's reactions to the reply at `node`, as its row.
    fn reacted_row(&self, node: i64) -> Option<Line<'static>> {
        let on = self.reactions.on(node);
        (!on.is_empty()).then(|| transcript::reacted(&on.join(" ")))
    }

    /// An emoji sent alone reacts to the reply `/react` picked, else to
    /// the bot's last: attached to the bot's next call, so it starts no
    /// turn. The same emoji again takes it off, which the bot is not told.
    async fn react(&mut self, emoji: String) {
        let Some((node, first)) = self
            .react_to
            .take()
            .or_else(|| self.replies.last().cloned())
        else {
            self.notice = Some("no reply to react to yet".into());
            return;
        };
        let last = self.replies.last().map(|(last, _)| *last) == Some(node);
        let added = match self.reactions.toggle(node, &emoji) {
            Ok(added) => added,
            Err(error) => {
                self.notice = Some(error);
                return;
            }
        };
        if added {
            let told = reactions::message(&emoji, (!last).then_some(first.as_str()));
            let sent = self
                .client
                .request(
                    "submit",
                    json!({"bot": self.bot, "request_id": unique("tui"), "prompt": told,
                        "delivery": "attach"}),
                )
                .await;
            match sent {
                Ok(reply) => self.reaction_turn = reply["turn"].as_i64(),
                Err(error) => {
                    let _ = self.reactions.toggle(node, &emoji);
                    self.notice = Some(error.to_string());
                    return;
                }
            }
        }
        self.input.clear();
        self.notice = None;
        // Under the reply, wherever it is in the conversation; else, as
        // when it is no longer there, a row of its own that says which.
        if last && self.running.is_none() {
            self.end_live(Words::Reply);
        }
        let row = self.reacted_row(node);
        if self.put_under(screen::Tag::End(node), screen::Tag::Reactions(node), row) {
            return;
        }
        if added && last && self.running.is_none() {
            self.commit_after(vec![transcript::reacted(&emoji)], false);
            return;
        }
        let what = if added {
            format!("↳ {emoji} on “{first}”")
        } else {
            format!("↳ took back {emoji} on “{first}”")
        };
        let rows = transcript::wrap(
            Line::from(what),
            self.prose(),
            Span::raw("  "),
            Span::raw("    "),
        );
        self.commit(transcript::quiet(rows));
    }

    /// `/side`: a side chat with a copy of this bot, taken where it is now,
    /// mid-turn included, and opened in a Sonny of its own on the alternate
    /// screen. The copy may call no tool, so it answers from what the bot
    /// knows; the daemon deletes it once it has been idle a quarter hour.
    /// This bot carries on, and its lines wait for the main screen.
    async fn side(&mut self, question: String, pictures: &[attach::Picture]) {
        if self.options.side.is_some() {
            self.notice = Some("this is a side chat already".into());
            return;
        }
        let stamp = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_millis();
        let name = format!("{}.side-{:x}", self.bot, stamp % 0x100_0000);
        let forked = self
            .client
            .request(
                "fork",
                json!({"source": self.bot, "bot": name, "allow": [],
                    "expire_ms": SIDE_EXPIRE.as_millis() as u64, "request_id": name}),
            )
            .await;
        let forked = match forked {
            Ok(forked) => forked,
            Err(error) => {
                self.notice = Some(format!("no side chat: {error}"));
                return;
            }
        };
        // On the model this window's turns run on, whose cache is warm.
        if !question.is_empty()
            && let Err(error) = self
                .client
                .request(
                    "submit",
                    json!({"bot": name, "bot_id": forked["bot_id"], "request_id": format!("{name}.1"),
                        "prompt": question, "model": self.turn_model,
                        "images": attach::submitted(pictures)}),
                )
                .await
        {
            self.notice = Some(format!("the side chat did not take the question: {error}"));
        }
        let head = forked["head"].as_i64().unwrap_or(0).to_string();
        let mut args = ["--no-sound", "--no-layout", "--bot", &name, "--side", &head]
            .map(str::to_owned)
            .to_vec();
        // On the model this window's turns run on, whose cache is warm.
        if let Some(model) = &self.turn_model {
            args.extend(["--model".to_owned(), model.clone()]);
        }
        // Beside the conversation, on a terminal of its own.
        if let Err(error) = self.embed("side chat", &args) {
            self.notice = Some(format!("no side chat: {error}"));
        }
    }

    /// The pane gone once the side chat in it has ended.
    fn side_closed(&mut self) {
        if let Some(pane) = &mut self.pane
            && let Some(program) = &mut pane.program
            && program.ended()
        {
            self.close_pane(false);
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
        // What the judge doubted, when it handed the call over.
        if let Some(reason) = call["asked"][0]["reason"].as_str() {
            let why = reason.split(". ").next().unwrap_or(reason);
            lines.push(Line::from(format!("  {why}")).dim());
        }
        self.commit(lines);
    }

    fn draw(&mut self, terminal: &mut Tui) -> std::io::Result<()> {
        self.show_asked();
        self.margin_back();
        // /status beside the conversation, gone once the pane is.
        if self.watch.is_some() && !self.pane.as_ref().is_some_and(|p| p.is_card()) {
            self.watch = None;
        }
        terminal.autoresize()?;
        // The conversation's width, the pane's taken off: the window's
        // while the pane has all of a narrow one, as it gets back after.
        let size = terminal.size()?;
        self.full_width = size.width;
        let (chat, shown) = self.split(Rect::new(0, 0, size.width, size.height));
        // A pane a narrow window hides takes no clicks.
        if shown.is_none()
            && let Some(pane) = &mut self.pane
        {
            pane.area = Rect::default();
        }
        let width = match chat.width {
            0 => size.width,
            chat => chat,
        } as usize;
        self.width = width;
        self.height = terminal.size()?.height as usize;
        let (composer_rows, (cursor_row, cursor_col)) = self.composer(width.min(PROSE));
        // A short pane keeps the rows around the cursor, where typing goes on.
        let room = (terminal.size()?.height as usize).saturating_sub(VIEWPORT as usize - 1);
        let (composer_rows, cursor_row) = window(composer_rows, cursor_row, room.max(1));
        // The reply's open line, wrapped as it will be written: it grows
        // here row by row until its newline writes it into the history.
        let mut open_rows = self.open_rows(width.min(PROSE));
        // Then what was sent and has not landed, kept in view below it.
        open_rows.extend(self.staged_rows(width));
        open_rows.extend(self.shell_rows(width));
        let spare = room.saturating_sub(composer_rows.len()) + 1;
        let hidden = open_rows.len().saturating_sub(spare.max(1));
        open_rows.drain(..hidden);
        let live_rows = open_rows.len().max(1) as u16;
        self.flush();
        let beside =
            !self.margin.closed() && pane::margin_width(size.width, PROSE as u16).is_some();
        self.fold_notes(beside);
        let below = VIEWPORT - 2 + live_rows + composer_rows.len() as u16;
        let workspace = std::path::PathBuf::from(&self.workspace);
        if let Some(at) = shown
            && self.pane.as_ref().is_some_and(pane::Pane::is_margin)
        {
            let live = self.running.as_ref().map(|running| running.turn);
            let width = at.width.saturating_sub(1) as usize;
            let (lines, marks) = self.margin.lines(width, &workspace, live);
            let glance = self.glance().line();
            if let Some(pane) = &mut self.pane {
                pane.set_margin(lines, marks, glance);
                if let Some(pin) = self.margin_focus.take() {
                    pane.screen.show_tagged(screen::Tag::Note(pin), 1);
                }
            }
        }
        if let (Some(watch), Some(pane)) = (&mut self.watch, &mut self.pane)
            && pane.is_card()
        {
            let wide = match chat.width {
                0 => size.width,
                _ => pane::width(size.width, PROSE as u16),
            };
            let width = wide.saturating_sub(2) as usize;
            if let Some(view) = &watch.background {
                // Its card, with what was done or the keys the pick takes
                // kept in view under it.
                let (card, at) = view.card(width);
                let lines: Vec<Line<'static>> = card
                    .into_iter()
                    .map(|line| {
                        let mut spans = vec![Span::raw(" ")];
                        spans.extend(line.spans);
                        Line::from(spans).style(line.style)
                    })
                    .collect();
                pane.set_card(lines, STATUS_KEYS);
                pane.foot = Some(view.footer());
                // A new pick is scrolled to, with the two rows under it.
                if watch.picked != Some(at) {
                    watch.picked = Some(at);
                    pane.screen.keep_in_view(at, 2);
                }
            } else {
                let (lines, any_resolved) = watch_card(watch, width);
                let keys = match any_resolved && watch.resolved {
                    true => "r hides resolved · esc closes",
                    false => STATUS_KEYS,
                };
                pane.set_card(lines, keys);
            }
        }
        // The links drawn, to be written again as hyperlinks.
        let mut linked = Vec::new();
        // Where the pane's program has its cursor.
        let mut pane_cursor = None;
        terminal.draw(|frame| {
            // The conversation above the rows that move, which sit right
            // under it until it fills the screen.
            let (area, pane) = self.split(frame.area());
            if let (Some(at), Some(open)) = (pane, &mut self.pane) {
                // Paths in a file are read from its folder first.
                let base = open.folder().unwrap_or(&workspace).to_owned();
                let file = |word: &str| {
                    links::file_line(&base, word).or_else(|| links::file_line(&workspace, word))
                };
                pane_cursor = open.render(at, frame.buffer_mut(), self.pane_keys, file);
                linked.extend(open.screen.link_cells(frame.buffer_mut()));
                // The line between them, as a split's.
                if area.width > 0 {
                    let line = Rect {
                        x: area.right(),
                        width: 1,
                        ..at
                    };
                    let rule = vec![Line::from("│").dim(); line.height as usize];
                    frame.render_widget(Paragraph::new(rule), line);
                }
            }
            if let Some(cursor) = pane_cursor.filter(|_| self.pane_keys) {
                frame.set_cursor_position(cursor);
            }
            if area.width == 0 {
                return;
            }
            let room = area.height.saturating_sub(below);
            let column = area.width.min(PROSE as u16);
            let rows = self.screen.height(column as usize).min(room as usize);
            let [history, view, _] = Layout::vertical([
                Constraint::Length(rows as u16),
                Constraint::Length(below),
                Constraint::Min(0),
            ])
            .areas(area);
            let file = |word: &str| links::file_line(&workspace, word);
            let history = ratatui::layout::Rect {
                width: column,
                ..history
            };
            self.screen.render(history, frame.buffer_mut(), file);
            linked.extend(self.screen.link_cells(frame.buffer_mut()));
            let [live, status, _, composer, footer] = areas(view, live_rows);
            frame.render_widget(Paragraph::new(open_rows.clone()), live);
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
                let [_, _, _, composer_row, _] = areas(view, live_rows);
                let area = ratatui::layout::Rect {
                    y: composer_row.y.saturating_sub(rows.len() as u16),
                    height: rows.len() as u16,
                    ..composer_row
                };
                frame.render_widget(ratatui::widgets::Clear, area);
                frame.render_widget(Paragraph::new(rows), area);
            }
            if let Some(query) = &self.finding {
                let line = Line::from(vec![Span::raw("find ").dim(), Span::raw(query.clone())]);
                frame.render_widget(Paragraph::new(line), composer);
                frame.set_cursor_position((composer.x + 5 + query.width() as u16, composer.y));
                let place = match self.screen.found() {
                    Some((_, 0)) if !query.is_empty() => "  not found · ".to_owned(),
                    Some((at, count)) if count > 0 => format!("  {at} of {count} · "),
                    _ => "  ".to_owned(),
                };
                let keys = Line::from(vec![
                    Span::raw(place),
                    Span::raw("↑ or enter older · ↓ newer · esc").dim(),
                ]);
                frame.render_widget(Paragraph::new(keys), footer);
                return;
            }
            if let Some(pick) = self.reacting {
                // Newest at the bottom, nearest the composer, as they read.
                let first = pick.saturating_sub(2);
                let mut rows: Vec<Line> = self
                    .replies
                    .iter()
                    .rev()
                    .enumerate()
                    .skip(first)
                    .take(3)
                    .map(|(i, (node, said))| {
                        let on = self.reactions.on(*node).join(" ");
                        let room = width.saturating_sub(8 + on.width());
                        let said = transcript::cut(said, room);
                        let row = if i == pick {
                            Span::raw(format!(" {said} ")).reversed()
                        } else {
                            Span::raw(format!(" {said}"))
                        };
                        Line::from(vec![Span::raw("   "), row, Span::raw(format!("  {on}"))])
                    })
                    .collect();
                rows.reverse();
                let area = ratatui::layout::Rect {
                    y: composer.y.saturating_sub(rows.len() as u16),
                    height: rows.len() as u16,
                    ..composer
                };
                frame.render_widget(ratatui::widgets::Clear, area);
                frame.render_widget(Paragraph::new(rows), area);
                let line = Line::from(vec![
                    Span::raw("› ").bold(),
                    Span::raw("which reply to react to").dim(),
                ]);
                frame.render_widget(Paragraph::new(line), composer);
                let keys = Line::from("  ↑↓ reply · enter, then send an emoji · esc").dim();
                frame.render_widget(Paragraph::new(keys), footer);
                return;
            }
            if let Some(pick) = self.approving {
                // Newest last, as they happened; the reason stays in view
                // before a retry is approved.
                let shown = self.denials.len().min(5);
                let first = pick
                    .saturating_sub(shown - 1)
                    .min(self.denials.len() - shown);
                let room = (composer.width as usize).saturating_sub(6);
                let rows: Vec<Line> = self.denials[first..first + shown]
                    .iter()
                    .enumerate()
                    .map(|(i, denied)| {
                        let what =
                            transcript::action(&denied.name, &denied.arguments, &self.workspace);
                        let why = denied
                            .reason
                            .lines()
                            .find(|line| line.trim_start().starts_with("Reason:"))
                            .or_else(|| denied.reason.lines().next())
                            .unwrap_or("")
                            .trim()
                            .to_owned();
                        let what = transcript::cut(&what, room / 2);
                        let why =
                            transcript::cut(&why, room.saturating_sub(what.chars().count() + 2));
                        if first + i == pick {
                            Line::from(vec![
                                Span::raw("  "),
                                Span::raw(format!(" {what}  {why} ")).reversed(),
                            ])
                        } else {
                            Line::from(vec![
                                Span::raw(format!("   {what}")),
                                Span::raw(format!("  {why}")).dim(),
                            ])
                        }
                    })
                    .collect();
                let area = ratatui::layout::Rect {
                    y: composer.y.saturating_sub(shown as u16),
                    height: shown as u16,
                    ..composer
                };
                frame.render_widget(ratatui::widgets::Clear, area);
                frame.render_widget(Paragraph::new(rows), area);
                let line = Line::from(vec![
                    Span::raw("› ").bold(),
                    Span::raw("auto-review denials: approve one retry").dim(),
                ]);
                frame.render_widget(Paragraph::new(line), composer);
                let keys = Line::from(
                    "  ↑↓ denial · enter to read it whole, again to approve one retry · esc",
                )
                .dim();
                frame.render_widget(Paragraph::new(keys), footer);
                return;
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
            frame.render_widget(Paragraph::new(composer_rows), composer);
            // Keys in the pane: no cursor where they do not go.
            if !self.pane_keys {
                frame.set_cursor_position((
                    composer.x + (cursor_col as u16).min(composer.width.saturating_sub(1)),
                    composer.y + (cursor_row as u16).min(composer.height.saturating_sub(1)),
                ));
            }
            frame.render_widget(Paragraph::new(self.footer_line(width)), footer);
        })?;
        // Over themselves, the cursor kept where the frame left it.
        if !linked.is_empty() {
            execute!(
                terminal.backend_mut(),
                ratatui::crossterm::cursor::SavePosition
            )?;
            terminal
                .backend_mut()
                .draw(linked.iter().map(|(x, y, cell)| (*x, *y, cell)))?;
            execute!(
                terminal.backend_mut(),
                ratatui::crossterm::cursor::RestorePosition
            )?;
        }
        Ok(())
    }

    /// The composer's rows: the prompt wrapped to the width, the way it
    /// will read once sent, and only the rows around the cursor when it
    /// is long; or the hint. Also the cursor's row among those and its
    /// column. A list or picker open over the composer keeps it to the
    /// cursor's row.
    fn composer(&self, width: usize) -> (Vec<Line<'static>>, (usize, usize)) {
        // What is sent answers the lead's first open question.
        let answering = (!self.chatting).then(|| self.asks.first()).flatten();
        if self.input.is_empty() {
            // Named, so a message typed in the wrong pane is caught first.
            let name = match &self.project {
                Some(project) => format!("{project} lead"),
                None => self.bot.clone(),
            };
            let hint = match self.running {
                _ if let Some(ask) = answering => {
                    asks::hint(ask, self.asks.len() - 1, width.saturating_sub(prompt::LEAD))
                }
                _ if self.react_to.is_some() => "An emoji for that reply, or Esc".into(),
                _ if !self.asks.is_empty() => "A message of your own, or Esc to answer".into(),
                Some(_) => format!("Steer {name}"),
                None => format!("Message {name}"),
            };
            let lead = match answering {
                Some(_) => Span::raw("? ").bold().fg(Color::Yellow),
                None => Span::raw("› ").bold(),
            };
            return (
                vec![Line::from(vec![lead, Span::raw(hint).dim()])],
                (0, prompt::LEAD),
            );
        }
        let layout = self.input.layout(width);
        let (row, col) = self.input.cursor(&layout);
        // A shell command is marked as one while it is typed.
        let shell = self.input.starts_with('!');
        let rows: Vec<Line<'static>> = layout
            .iter()
            .enumerate()
            .map(|(at, part)| {
                let lead = match at {
                    0 if shell => Span::raw("› ").bold().fg(Color::Magenta),
                    0 if answering.is_some() => Span::raw("? ").bold().fg(Color::Yellow),
                    0 => Span::raw("› ").bold(),
                    _ => Span::raw("  "),
                };
                Line::from(vec![
                    lead,
                    Span::raw(self.input[part.start..part.end].to_owned()),
                ])
            })
            .collect();
        let keep = match self.choosing.is_some()
            || self.approving.is_some()
            || self.picker.is_some()
            || self.reacting.is_some()
            || self.finding.is_some()
        {
            true => 1,
            false => COMPOSER_ROWS,
        };
        let (rows, row) = window(rows, row, keep);
        (rows, (row, col))
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
            None if self.input.starts_with('!') => Line::from(vec![
                Span::raw("! ").bold().fg(Color::Magenta),
                Span::raw(format!(
                    "runs in {}; the agent is told what it printed",
                    tilde(&self.workspace)
                ))
                .dim(),
            ]),
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
        let right = match (self.options.side, self.context) {
            // Said for as long as the side chat is open.
            (Some(_), _) => format!(
                "side chat with {} · esc goes back  ",
                self.bot
                    .rsplit_once(".side-")
                    .map_or(self.bot.as_str(), |(s, _)| s)
            ),
            (None, Some(size)) => format!("{} · {} context  ", self.bot, transcript::tokens(size)),
            (None, None) => format!("{}  ", self.bot),
        };
        // The margin's glance says these while it is in view; the notes
        // are counted while it is not.
        let glancing = (self.pane.as_ref()).is_some_and(|p| p.is_margin() && p.area.width > 0);
        let right = match (glancing, self.context) {
            (true, Some(_)) if self.options.side.is_none() => format!("{}  ", self.bot),
            _ => right,
        };
        // Something runs for it with no one typing: /background lists it.
        let right = match self.behind.0 {
            0 => right,
            _ if glancing => right,
            n => format!("↻ {n} in background · {right}"),
        };
        let right = match self.margin.notes().len() {
            n if n > 0 && !glancing => format!("✎ {n} · {right}"),
            _ => right,
        };
        // Messages its next call takes in, which need nothing of the person.
        let right = match self.attached.len() {
            0 => right,
            1 => format!("📎 · {right}"),
            n => format!("📎 {n} · {right}"),
        };
        // The folder, and its branch, which is kept whole.
        let branch = match &self.place.branch {
            Some(branch) => format!(" on {branch}"),
            None => String::new(),
        };
        // A long folder keeps its end, the part that tells folders apart.
        let room = width.saturating_sub(left.width() + branch.width() + right.width() + 4);
        let mut folder = folder.as_str();
        let cut = folder.width() > room;
        while folder.width() > room.saturating_sub(1) && !folder.is_empty() {
            folder = &folder[folder.chars().next().map_or(1, char::len_utf8)..];
        }
        left.push_str(&format!(
            " · {}{folder}{branch}",
            if cut { "…" } else { "" }
        ));
        let fill = width.saturating_sub(left.width() + right.width());
        Line::from(format!("{left}{}{right}", " ".repeat(fill))).dim()
    }
}

/// What the pane's title row says the keys do on `/status`.
const STATUS_KEYS: &str = "esc closes";

/// `/status`'s card at `width`, each line after a space, its times
/// running turns counted on from when its rows were read; and whether any task in it
/// is resolved.
fn watch_card(watch: &Watch, width: usize) -> (Vec<Line<'static>>, bool) {
    let since = watch.read.elapsed().as_secs();
    let rows: Vec<status::Row> = watch
        .rows
        .iter()
        .cloned()
        .map(|mut row| {
            if row.running {
                row.seconds = row.seconds.map(|s| s + since);
            }
            row
        })
        .collect();
    let any_resolved = rows.iter().any(|row| row.group == status::Group::Resolved);
    let mut card = status::card(&watch.project, &rows, width, watch.resolved);
    if !watch.lookout.is_empty() {
        card.insert(1, Line::from(watch.lookout.clone()).dim());
    }
    let lines = card
        .into_iter()
        .map(|line| {
            let mut spans = vec![Span::raw(" ")];
            spans.extend(line.spans);
            Line::from(spans).style(line.style)
        })
        .collect();
    (lines, any_resolved)
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
/// The gate a person answers a call on: the first that handed it over,
/// else `manual`.
fn person_gate(call: &Value) -> &str {
    call["asked"][0]["tag"].as_str().unwrap_or("manual")
}

fn short(bot: &str, project: &str, lead: &str) -> String {
    if bot == lead {
        return "lead".into();
    }
    bot.strip_prefix(&format!("{project}."))
        .unwrap_or(bot)
        .to_owned()
}

/// A message as it lands in the conversation. A command the person ran
/// is by name only when `shown` holds it (this window showed its output
/// when it ran, and lets it go), else whole.
/// A message's first line, cut to quote it.
fn first_words(text: &str) -> String {
    let first = text
        .lines()
        .find(|line| !line.trim().is_empty())
        .unwrap_or("");
    transcript::cut(first.trim(), QUOTED)
}

fn landed(text: &str, shown: &mut Vec<user_shell::Ran>, width: usize) -> Vec<Line<'static>> {
    let Some(ran) = user_shell::read(text) else {
        return transcript::prompt(text, None, width);
    };
    match shown.iter().position(|seen| *seen == ran) {
        Some(at) => {
            shown.remove(at);
            user_shell::header(&ran.command, width, true)
        }
        None => user_shell::lines(&ran, width),
    }
}

fn first_line(text: &str) -> String {
    let line = text.lines().next().unwrap_or("");
    let mut cut: String = line.chars().take(60).collect();
    if cut.len() < line.len() || text.lines().nth(1).is_some() {
        cut.push('…');
    }
    cut
}

/// How many pages older a jump to a note's place reads before giving up.
const JUMP_PAGES: u8 = 20;

/// The most rows of a note drawn in the conversation, when the margin
/// cannot show it: a picture and a few lines around it.
const INLINE_ROWS: usize = 40;

/// Where a note was put in the conversation: `title`, said to be in the
/// margin when it is `beside` the conversation, else the note's own head
/// over its rows.
fn note_marker(title: String, beside: bool) -> Line<'static> {
    let mut spans = vec![
        Span::raw("  ✎ ").fg(Color::Yellow).dim(),
        Span::raw(title).dim().underlined(),
    ];
    if beside {
        spans.push(Span::raw(" · in the margin").dim());
    }
    Line::from(spans)
}

/// Whether a message of the person's is drawn elsewhere than as a
/// message: a reaction under its reply, a note taken away in the margin.
fn quiet_message(text: &str) -> bool {
    reactions::is_reaction(text) || margin::closed(text).is_some()
}

/// A message item's text: a prompt's own words, without the line the
/// daemon adds after them naming who sent it.
fn text(item: &Value) -> String {
    match (&item["role"], &item["content"]) {
        (role, Value::Array(parts)) if role == "user" => (parts.iter())
            .find_map(|part| part["text"].as_str())
            .unwrap_or_default()
            .to_owned(),
        (_, content) => joined(content),
    }
}

/// Text, or the text of a list of parts, leaving out the pictures.
fn joined(content: &Value) -> String {
    match content {
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
    // A result with a picture (an image file read) is a list of parts.
    let raw = match &item["output"] {
        Value::Null => text(item),
        output => joined(output),
    };
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
/// its text, and a user message's `tool_result` blocks their outputs. A
/// custom tool's call reads as a function's, its input as `{"input": ...}`
/// as the daemon runs it, and its output as a function's.
fn responses(item: &Value) -> Vec<Value> {
    match item["type"].as_str() {
        Some("custom_tool_call") => {
            return vec![json!({"type": "function_call", "call_id": item["call_id"],
                "name": item["name"], "arguments": json!({"input": item["input"]}).to_string()})];
        }
        Some("custom_tool_call_output") => {
            let mut output = item.clone();
            output["type"] = json!("function_call_output");
            return vec![output];
        }
        _ => {}
    }
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

    #[test]
    fn another_projects_lead_and_its_tasks_are_never_tasks_here() {
        let lineage: std::collections::HashSet<String> = ["ws.lead".into(), "ws.fix".into()].into();
        let mut leads = std::collections::HashSet::new();
        assert_eq!(kin("ws.fix2", "ws.fix", "ws", &lineage, &leads), Kin::Task);
        assert_eq!(kin("ws.side", "", "ws", &lineage, &leads), Kin::Task);
        // Whoever made it, as the new_project tool says no one.
        assert_eq!(
            kin("ws.web.lead", "", "ws", &lineage, &leads),
            Kin::Lead("ws.web".into())
        );
        leads.insert("ws.web".to_owned());
        // That lead's tasks are its own, though named like this project's.
        assert_eq!(
            kin("ws.web.fix", "ws.web.lead", "ws", &lineage, &leads),
            Kin::None
        );
        // After a restart, a project inside this one is known by its lead.
        let names = [
            "ws.lead",
            "ws.fix",
            "ws.web.lead",
            "ws.web.fix",
            "shop.lead",
        ];
        assert_eq!(inner_projects(names.into_iter(), "ws"), ["ws.web"]);
        assert_eq!(
            kin("shop.lead", "ws.lead", "ws", &lineage, &leads),
            Kin::Lead("shop".into())
        );
        assert_eq!(kin("other.task", "", "ws", &lineage, &leads), Kin::None);
        // The lead of a project this one is inside leaves its tasks here.
        let lineage: std::collections::HashSet<String> = ["ws.web.lead".into()].into();
        let mut leads = std::collections::HashSet::new();
        assert_eq!(kin("ws.lead", "", "ws.web", &lineage, &leads), Kin::None);
        leads.insert("ws.api".to_owned());
        assert_eq!(
            kin("ws.web.fix", "ws.web.lead", "ws.web", &lineage, &leads),
            Kin::Task
        );
    }

    #[test]
    fn agent_is_found_beside_sonny_or_by_homebrews_opt_path() {
        assert_eq!(
            installed_agent(std::path::Path::new(
                "/opt/homebrew/Cellar/sonny/0.4.0/bin/sonny"
            )),
            [
                PathBuf::from("/opt/homebrew/Cellar/sonny/0.4.0/libexec/agent"),
                PathBuf::from("/opt/homebrew/opt/agent/bin/agent"),
            ]
        );
        assert_eq!(
            installed_agent(std::path::Path::new("/home/me/sonny-0.4.0/bin/sonny")),
            [PathBuf::from("/home/me/sonny-0.4.0/libexec/agent")]
        );
    }

    /// A daemon that greets and answers each `history_items` read from
    /// `items`, by node.
    async fn daemon_with(items: Value) -> Arc<Client> {
        daemon_answering(move |asked| {
            let read: Vec<Value> = asked["nodes"]
                .as_array()
                .into_iter()
                .flatten()
                .map(|node| json!({"node": node, "item": items[node.to_string()]}))
                .collect();
            json!({"items": read})
        })
        .await
    }

    /// A daemon that greets and gives each request `answer`'s result, or
    /// its refusal when it names an `error`.
    async fn daemon_answering(answer: impl Fn(&Value) -> Value + Send + 'static) -> Arc<Client> {
        use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
        // Tests start stand-ins at once, within the same millisecond.
        static STARTED: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
        let dir = std::env::temp_dir().join(format!(
            "sonny-daemon-{}-{}",
            unique("t"),
            STARTED.fetch_add(1, std::sync::atomic::Ordering::SeqCst)
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
                let mut answer = match answer(&asked) {
                    refused if refused.get("error").is_some() => refused,
                    result => json!({"result": result}),
                };
                answer["id"] = asked["id"].clone();
                write
                    .write_all(format!("{answer}\n").as_bytes())
                    .await
                    .unwrap();
            }
        });
        Client::connect(&socket).await.unwrap().0
    }

    /// Sonny on bot `b`, on `client`.
    fn app(client: Arc<Client>) -> App {
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
            side: None,
            embedded: false,
        };
        App::new(client, "b", &json!({"name": "b"}), options)
    }

    /// What the conversation holds, the lines still waiting written in.
    fn drawn(app: &mut App) -> Vec<String> {
        app.flush();
        app.screen.texts()
    }

    fn turn_event(event: &str, turn: i64, data: Value) -> Value {
        json!({"event": event, "bot": "b", "turn": turn, "data": data})
    }

    #[tokio::test]
    async fn what_is_typed_answers_the_leads_first_question_unless_set_aside() {
        let sent = Arc::new(std::sync::Mutex::new(Vec::new()));
        let seen = sent.clone();
        let client = daemon_answering(move |asked| {
            if asked["op"] == "submit" {
                seen.lock()
                    .unwrap()
                    .push(asked["prompt"].as_str().unwrap().to_owned());
            }
            json!({"turn": 2})
        })
        .await;
        let mut app = app(client);
        let ask = |id: &str, question: &str| {
            asks::call(
                id,
                &json!({"question": question, "options": [
                    {"label": "Keep it", "explanation": "e"},
                    {"label": "Drop it", "explanation": "e"}]}),
            )
            .unwrap()
        };
        app.asks = vec![
            ask("c1", "Keep the old API?"),
            ask("c2", "Tag it?"),
            ask("c3", "Keep the old API?"),
        ];
        app.input.set("2");
        app.submit().await;
        // Its answer landing leaves the other question asked the same open.
        app.answered("You asked: Keep the old API?\n\nDrop it");
        // Esc sets the rest aside: the next message is the person's own.
        app.chatting = true;
        app.input.set("how is it going?");
        app.submit().await;
        assert_eq!(
            *sent.lock().unwrap(),
            [
                "You asked: Keep the old API?\n\nDrop it",
                "how is it going?"
            ]
        );
        assert_eq!(
            app.asks,
            [ask("c2", "Tag it?"), ask("c3", "Keep the old API?")]
        );
    }

    #[tokio::test]
    async fn a_question_asked_stays_open_until_an_answer_lands() {
        let items = json!({
            "1": {"type": "message", "role": "user",
                  "content": [{"type": "input_text", "text": "ship it?"}]},
            "2": {"type": "function_call", "name": "ask", "call_id": "a1",
                  "arguments": json!({"question": "Tag 0.4.1?", "why": "Main is green.",
                      "options": [{"label": "Yes", "explanation": "Brew gets it tonight."},
                          {"label": "No", "explanation": "It waits for #240."}],
                      "recommended": 1, "reason": "Nothing else is due."}).to_string()},
            "3": {"type": "function_call_output", "call_id": "a1", "output": "Asked."},
            "4": {"type": "function_call", "name": "ask", "call_id": "a2",
                  "arguments": "{\"question\":\"Bump?\",\"options\":[1]}"},
            "5": {"type": "function_call_output", "call_id": "a2",
                  "output": "{\"error\":\"invalid_arguments\"}"},
            "6": {"type": "function_call", "name": "ask", "call_id": "a3",
                  "arguments": "{\"question\":\"Write notes?\"}"},
            "7": {"type": "function_call_output", "call_id": "a3", "output": "Asked."},
            "8": {"type": "message", "role": "user",
                  "content": [{"type": "input_text", "text": "You asked: Write notes?\n\nyes"}]},
        });
        let client = daemon_answering(move |asked| match asked["op"].as_str() {
            Some("history_nodes") => {
                json!({"nodes": (1..=9).rev().map(|node| json!({"node": node}))
                .collect::<Vec<_>>(), "next_from": null})
            }
            _ => json!({"items": asked["nodes"].as_array().into_iter().flatten()
                .map(|node| json!({"node": node, "item": items[node.to_string()]}))
                .collect::<Vec<_>>()}),
        })
        .await;
        let mut app = app(client);
        app.history().await;
        // The refused one was never asked, and the answered one is closed.
        assert_eq!(
            app.asks.iter().map(|a| a.id.as_str()).collect::<Vec<_>>(),
            ["a1"]
        );
        // Drawn where it was asked, with what each option means.
        let rows: Vec<String> = app.pending.iter().map(|l| l.to_string()).collect();
        let at = rows
            .iter()
            .position(|r| r == "? Tag 0.4.1?")
            .expect("asked");
        assert_eq!(rows[at + 4], "     Brew gets it tonight.");
        assert_eq!(rows[at + 8], "  Why 1: Nothing else is due.");
        // Its answer landing, sent from here or another window, closes it.
        app.answered("You asked: Tag 0.4.1?\n\nYes");
        assert!(app.asks.is_empty());
        // Live, a question is pinned once the daemon took the call.
        let arguments = json!({"question": "Merge?"}).to_string();
        app.daemon(turn_event("accepted", 4, json!({}))).await;
        app.daemon(turn_event(
            "tool_started",
            4,
            json!({"call_id": "a4", "name": "ask", "arguments": arguments}),
        ))
        .await;
        assert!(app.asks.is_empty());
        app.daemon(turn_event("tool_completed", 4, json!({"call_id": "a4"})))
            .await;
        assert_eq!(app.asks[0].question, "Merge?");
    }

    #[tokio::test]
    async fn an_auto_review_denial_is_drawn_and_approve_sends_one_retry() {
        let sent = Arc::new(std::sync::Mutex::new(Vec::new()));
        let seen = sent.clone();
        let reason = "This action was rejected due to unacceptable risk (risk: high, authorization: low).\n\
            Reason: It force-pushes the default branch.\n\
            The agent must not attempt to achieve the same outcome via workaround.";
        let items = json!({
            "6": {"type": "function_call", "call_id": "c1", "name": "shell",
                "arguments": json!({"command": "git push --force origin main"}).to_string()},
            "7": {"type": "function_call_output", "call_id": "c1",
                "output": json!({"error": "approval_denied", "detail": reason}).to_string()},
        });
        let client = daemon_answering(move |asked| match asked["op"].as_str() {
            Some("submit") => {
                seen.lock().unwrap().push(asked["prompt"].clone());
                json!({"turn": 5})
            }
            Some("history_nodes") => json!({"nodes": [{"node": 7}, {"node": 6}]}),
            Some("history_items") => {
                let read: Vec<Value> = asked["nodes"]
                    .as_array()
                    .into_iter()
                    .flatten()
                    .map(|node| json!({"node": node, "item": items[node.to_string()]}))
                    .collect();
                json!({"items": read})
            }
            Some("approvals") => json!({"approvals": [
                {"turn": 4, "call_id": "c1", "gates": ["auto"], "asked": null},
                {"turn": 4, "call_id": "c2", "gates": ["auto"], "asked": null}]}),
            _ => json!({}),
        })
        .await;
        let mut app = app(client);
        app.width = 80;
        app.daemon(turn_event("accepted", 4, json!({}))).await;
        app.daemon(turn_event("approval_requested", 4, json!({"calls": []})))
            .await;
        // While the approver reviews, the status row says so.
        assert_eq!(
            app.running.as_ref().unwrap().phase,
            "Reviewing 2 approval requests"
        );
        app.daemon(turn_event(
            "tool_completed",
            4,
            json!({"call_id": "c1", "node": 7, "denied": true,
                "approvals": [{"tag": "auto", "by": "approver", "allow": false}]}),
        ))
        .await;
        // The other is still under review.
        assert_eq!(app.reviewing.len(), 1);
        assert_eq!(
            app.running.as_ref().unwrap().phase,
            "Reviewing approval request"
        );
        // A question denied was never asked.
        let question = json!({"question": "Merge?"}).to_string();
        app.daemon(turn_event(
            "tool_started",
            4,
            json!({"call_id": "c2", "name": "ask", "arguments": question}),
        ))
        .await;
        app.daemon(turn_event(
            "tool_completed",
            4,
            json!({"call_id": "c2", "denied": true,
                "approvals": [{"tag": "auto", "by": "approver", "allow": false}]}),
        ))
        .await;
        assert!(app.reviewing.is_empty() && app.asks.is_empty());
        assert_eq!(app.running.as_ref().unwrap().phase, "Working");
        app.denials.retain(|denial| denial.call_id == "c1");
        // One cancelled before it started leaves review too.
        app.reviewing.insert("c3".into());
        app.daemon(turn_event("tool_completed", 4, json!({"call_id": "c3"})))
            .await;
        assert!(app.reviewing.is_empty());
        let rows: Vec<String> = app.pending.iter().map(|l| l.to_string()).collect();
        assert!(
            rows.contains(&"✗ Request denied for git push --force origin main".to_owned()),
            "{rows:?}"
        );
        assert!(
            rows.iter()
                .any(|row| row == "    Reason: It force-pushes the default branch."),
            "{rows:?}"
        );
        // `/approve` offers it; Enter sends the person's approval of that
        // exact call, once.
        app.running = None;
        app.input.set("/approve");
        app.submit().await;
        assert_eq!(app.approving, Some(0));
        let enter = || Event::Key(event::KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
        // The first Enter writes the whole call out; nothing is sent yet.
        app.key(enter()).await;
        assert_eq!(app.approving, Some(0));
        assert!(sent.lock().unwrap().is_empty());
        let rows: Vec<String> = app.pending.iter().map(|l| l.to_string()).collect();
        assert!(
            rows.ends_with(&[
                "Approve one retry of shell?".to_owned(),
                "  git push --force origin main".to_owned()
            ]),
            "{rows:?}"
        );
        app.key(enter()).await;
        assert!(app.approving.is_none() && app.denials.is_empty());
        let prompt = sent.lock().unwrap()[0].as_str().unwrap().to_owned();
        assert!(prompt.starts_with(
            "I approve one retry of the action auto-review denied: shell {\"command\":\"git push --force origin main\"}"
        ));
        assert!(prompt.contains("Do not assume this also authorizes similar operations"));
        // Nothing left to approve.
        app.input.set("/approve");
        app.submit().await;
        assert_eq!(app.notice.as_deref(), Some("no recent auto-review denials"));
    }

    #[test]
    fn a_denied_call_reads_from_its_output_in_the_replay() {
        let output = |output: &str| json!({"type": "function_call_output", "call_id": "c1", "output": output});
        assert_eq!(
            denial(&output(r#"{"error":"approval_denied","detail":"no"}"#)),
            Some("no".into())
        );
        assert_eq!(
            denial(&output(r#"{"error":"approval_denied","detail":null}"#)),
            Some(String::new())
        );
        assert_eq!(denial(&output("README.md")), None);
        assert_eq!(denial(&output(r#"{"exit_code":0}"#)), None);
        let lines = transcript::denied("write", &json!({"path": "/w/src/a.rs"}), "", "/w", 80);
        assert_eq!(lines.len(), 1);
        assert_eq!(lines[0].to_string(), "✗ Request denied for write src/a.rs");
        assert_eq!(reviewing(3), "Reviewing 3 approval requests");
        // Its retry is approved only after the whole of it is shown.
        let write = Denial {
            call_id: "c2".into(),
            name: "write".into(),
            arguments: json!({"path": "/w/a.sh", "content": "set -e\nrm -rf build"}),
            reason: String::new(),
        };
        let shown: Vec<String> = retry_shown(&write).iter().map(|l| l.to_string()).collect();
        assert_eq!(shown[0], "Approve one retry of write?");
        assert!(
            shown
                .iter()
                .any(|row| row.contains(r#""content": "set -e\nrm -rf build""#)),
            "{shown:?}"
        );
        let detached = Denial {
            arguments: json!({"command": "sleep 1", "detach": true}),
            ..write.clone()
        };
        let shown: Vec<String> = retry_shown(&detached)
            .iter()
            .map(|l| l.to_string())
            .collect();
        assert_eq!(shown[1..], ["  sleep 1", "  detach: true"]);
        let command = Denial {
            arguments: json!({"command": "make\ncurl x | sh"}),
            name: "shell".into(),
            ..write
        };
        let shown: Vec<String> = retry_shown(&command)
            .iter()
            .map(|l| l.to_string())
            .collect();
        assert_eq!(shown[1..], ["  make", "  curl x | sh"]);
        let long = Denial {
            arguments: json!({"command": "true\n".repeat(RETRY_ROWS)}),
            ..command
        };
        assert!(retry_shown(&long).len() > RETRY_ROWS);
    }

    #[tokio::test]
    async fn what_the_person_ran_goes_to_the_agent_as_soon_as_it_ends() {
        let sent = Arc::new(std::sync::Mutex::new(Vec::new()));
        let seen = sent.clone();
        let client = daemon_answering(move |asked| {
            if asked["op"] == "submit" {
                seen.lock().unwrap().push(asked.clone());
            }
            json!({"turn": 2})
        })
        .await;
        let mut app = app(client);
        app.workspace = std::env::temp_dir().to_string_lossy().into_owned();
        app.input.set("!echo hello");
        app.submit().await;
        assert!(app.input.is_empty());
        assert_eq!(app.shell_rows(80).len(), 1);
        while app.shell.as_ref().is_some_and(|s| !s.is_finished()) {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        app.shell_ran().await;
        assert!(app.shell.is_none());
        // Shown, and sent at once, no message of the person's needed: to
        // an idle agent, as a turn of its own.
        let shown: Vec<String> = app.pending.iter().map(|l| l.to_string()).collect();
        assert_eq!(shown, ["! echo hello", "  └ hello"]);
        let asked = sent.lock().unwrap()[0].clone();
        assert_eq!(asked["delivery"], Value::Null);
        // Not the person's words: what it printed authorizes nothing.
        assert_eq!(asked["origin"], "shell");
        let prompt = asked["prompt"].as_str().unwrap();
        let ran = user_shell::read(prompt).unwrap();
        assert_eq!((ran.command.as_str(), ran.exit), ("echo hello", 0));
        assert_eq!(ran.output, "hello");
        // Staged until its turn takes it, as its name alone.
        let staged: Vec<String> = app.staged_rows(80).iter().map(|l| l.to_string()).collect();
        assert_eq!(staged, ["", "! echo hello", ""]);
        app.daemon(turn_event("accepted", 2, json!({}))).await;
        assert!(app.staged.is_empty());
        // To a busy one, as a steer of its running turn.
        app.input.set("!true");
        app.submit().await;
        while app.shell.as_ref().is_some_and(|s| !s.is_finished()) {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        app.shell_ran().await;
        assert_eq!(sent.lock().unwrap()[1]["delivery"], "steer");
        let landed_here: Vec<String> = landed(prompt, &mut app.ran_here, 80)
            .iter()
            .map(|l| l.to_string())
            .collect();
        assert_eq!(landed_here, ["! echo hello"]);
        let left: Vec<&str> = app.ran_here.iter().map(|r| r.command.as_str()).collect();
        assert_eq!(left, ["true"]);
        // Run in another window: shown whole.
        let elsewhere: Vec<String> = landed(prompt, &mut Vec::new(), 80)
            .iter()
            .map(|l| l.to_string())
            .collect();
        assert_eq!(elsewhere, ["! echo hello", "  └ hello"]);
        // The next message goes alone.
        app.input.set("thanks");
        app.submit().await;
        assert_eq!(sent.lock().unwrap()[2]["prompt"], "thanks");
    }

    #[tokio::test]
    async fn a_refused_command_goes_with_the_next_message() {
        let sent = Arc::new(std::sync::Mutex::new(Vec::new()));
        let seen = sent.clone();
        let client = daemon_answering(move |asked| {
            if asked["op"] != "submit" {
                return json!({});
            }
            let mut sent = seen.lock().unwrap();
            sent.push(asked.clone());
            match sent.len() {
                1 | 2 => json!({"error": "active_agent_limit", "detail": "every slot is taken"}),
                _ => json!({"turn": 3}),
            }
        })
        .await;
        let mut app = app(client);
        app.workspace = std::env::temp_dir().to_string_lossy().into_owned();
        app.move_to = Some("/moved".into());
        for command in ["!echo hello", "!echo again"] {
            app.input.set(command);
            app.submit().await;
            while app.shell.as_ref().is_some_and(|s| !s.is_finished()) {
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
            app.shell_ran().await;
        }
        // Refused: kept, and said to be.
        assert_eq!(app.unsent.len(), 2);
        assert!(app.ran_here.is_empty());
        assert!(
            app.shell_rows(80)[0]
                .to_string()
                .ends_with("goes with your next message")
        );
        assert!(
            app.notice
                .as_deref()
                .unwrap()
                .contains("active_agent_limit")
        );
        app.input.set("and now?");
        app.submit().await;
        // Each record goes first, on its own, and the message joins the
        // turn the first starts.
        let sent = sent.lock().unwrap().clone();
        let command = |at: usize| user_shell::read(sent[at]["prompt"].as_str().unwrap()).unwrap();
        assert_eq!(command(2).command, "echo hello");
        assert_eq!(command(3).command, "echo again");
        assert_eq!(
            (&sent[2]["origin"], &sent[3]["origin"]),
            (&json!("shell"), &json!("shell"))
        );
        assert_eq!(
            (&sent[2]["delivery"], &sent[3]["delivery"]),
            (&Value::Null, &json!("steer"))
        );
        assert_eq!(sent[4]["prompt"], "and now?");
        assert_eq!(sent[4]["origin"], Value::Null);
        assert_eq!(sent[4]["delivery"], "steer");
        // A turn it starts moves a moved lead, as any message's does.
        assert_eq!(sent[0]["workspace"], "/moved");
        assert_eq!(sent[2]["workspace"], "/moved");
        assert!(app.move_to.is_none());
        assert!(app.unsent.is_empty());
        assert_eq!(app.ran_here.len(), 2);
        assert!(app.shell_rows(80).is_empty());
    }

    #[test]
    fn refused_commands_held_stay_within_a_message() {
        let mut unsent = Vec::new();
        for n in 0..5 {
            hold(&mut unsent, format!("{n}{}", "x".repeat(UNSENT_BYTES / 3)));
        }
        let firsts: Vec<char> = unsent.iter().filter_map(|r| r.chars().next()).collect();
        assert_eq!(firsts, ['3', '4']);
        // One alone over it is still kept: the daemon says if it is too much.
        hold(&mut unsent, "y".repeat(UNSENT_BYTES * 2));
        assert_eq!(unsent.len(), 1);
    }

    #[tokio::test]
    async fn a_sent_prompt_stays_staged_until_its_turn_takes_it() {
        let next = std::sync::atomic::AtomicI64::new(2);
        let client = daemon_answering(move |asked| match asked["op"].as_str() {
            Some("submit") => {
                json!({"turn": next.fetch_add(1, std::sync::atomic::Ordering::SeqCst)})
            }
            _ => json!({"items": [{"node": 10, "item": {"type": "message", "role": "user",
                "content": [{"type": "input_text", "text": "steer one"}]}}]}),
        })
        .await;
        let mut app = app(client);
        let staged = |app: &App| app.staged.iter().map(|(turn, _)| *turn).collect::<Vec<_>>();
        app.daemon(turn_event("accepted", 1, json!({}))).await;
        // Sent while turn 1 runs: two steers, turns 2 and 3.
        for prompt in ["steer one", "steer two"] {
            app.input.set(prompt);
            app.submit().await;
        }
        assert_eq!(staged(&app), [2, 3]);
        assert_eq!(app.staged_rows(80).len(), 5, "each with a blank row above");
        // Turn 1 takes in the first; its own turn ends as steered.
        app.daemon(turn_event("turn_finished", 2, json!({"status": "steered"})))
            .await;
        app.daemon(turn_event("steered", 1, json!({"steer": 2, "node": 10})))
            .await;
        assert_eq!(staged(&app), [3]);
        assert_eq!(app.running.as_ref().map(|r| r.turn), Some(1));
        // Turn 1 ends before the second lands; that one starts its own turn.
        app.daemon(turn_event(
            "turn_finished",
            1,
            json!({"status": "completed"}),
        ))
        .await;
        assert_eq!(staged(&app), [3]);
        app.daemon(turn_event("accepted", 3, json!({}))).await;
        assert!(app.staged.is_empty());
        // A prompt whose turn ends without starting is unstaged too.
        app.input.set("never ran");
        app.submit().await;
        app.daemon(turn_event(
            "turn_finished",
            4,
            json!({"status": "interrupted"}),
        ))
        .await;
        assert!(app.staged.is_empty());
    }

    #[tokio::test]
    async fn on_reconnecting_only_prompts_still_waiting_stay_staged() {
        // Two pages, from the first staged turn on.
        let client =
            daemon_answering(
                |asked| match (asked["op"].as_str(), asked["after"].as_i64()) {
                    (Some("turns"), Some(4)) => json!({"turns": [
                {"turn": 5, "status": "steered"},
                {"turn": 6, "status": "running"},
            ], "next_after": 6}),
                    (Some("turns"), Some(6)) => json!({"turns": [
                {"turn": 7, "status": "queued"},
            ], "next_after": null}),
                    _ => json!({}),
                },
            )
            .await;
        let mut app = app(client);
        app.staged = vec![
            (5, "a".into()),
            (6, "b".into()),
            (7, "c".into()),
            (8, "d".into()),
        ];
        app.unstage_landed().await;
        assert_eq!(app.staged, [(7, "c".to_owned())]);
    }

    #[tokio::test]
    async fn a_reply_that_follows_its_reasoning_is_drawn_once() {
        let client = daemon_with(json!({
            "1": {"type": "reasoning", "summary": [{"type": "summary_text", "text": "**Reading**"}]},
            "2": {"type": "message", "role": "assistant",
                  "content": [{"type": "output_text", "text": "First line.\nSecond line."}]}
        }))
        .await;
        let mut app = app(client);
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
        // Written, or held until the turn ends.
        let drawn: Vec<String> = app
            .pending
            .iter()
            .chain(&app.open_rows(80))
            .map(|line| line.to_string())
            .collect();
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

    #[tokio::test]
    async fn steps_are_quiet_and_the_reply_comes_under_the_turns_line() {
        let client = daemon_answering(|asked| {
            let item = match asked["nodes"][0].as_i64() {
                Some(2) => json!({"type": "message", "role": "assistant",
                    "content": [{"type": "output_text", "text": "I'll look."}]}),
                Some(3) => json!({"type": "function_call_output", "call_id": "c1", "output": "a"}),
                Some(4) => json!({"type": "message", "role": "assistant",
                    "content": [{"type": "output_text", "text": "Done."}]}),
                _ => return json!({}),
            };
            json!({"items": [{"node": asked["nodes"][0], "item": item}]})
        })
        .await;
        let mut app = app(client);
        app.verbose = true;
        let said = |text: &str| json!({"event": "text_delta", "bot": "b", "turn": 1, "text": text});
        app.daemon(turn_event("accepted", 1, json!({}))).await;
        app.daemon(said("I'll look.")).await;
        app.daemon(turn_event("message", 1, json!({"node": 2})))
            .await;
        // Held until what follows says what the words were.
        assert!(app.pending.is_empty());
        app.daemon(turn_event(
            "tool_started",
            1,
            json!({"call_id": "c1", "name": "shell", "arguments": "{\"command\":\"ls\"}"}),
        ))
        .await;
        app.daemon(turn_event(
            "tool_completed",
            1,
            json!({"call_id": "c1", "node": 3}),
        ))
        .await;
        app.daemon(said("Done.")).await;
        app.daemon(turn_event("message", 1, json!({"node": 4})))
            .await;
        let shown: Vec<String> = app.open_rows(40).iter().map(|l| l.to_string()).collect();
        assert_eq!(shown, ["", "• Done."]);
        app.daemon(turn_event(
            "turn_finished",
            1,
            json!({"status": "completed"}),
        ))
        .await;
        let dim = |line: &Line| {
            line.spans.iter().all(|span| {
                line.style
                    .patch(span.style)
                    .add_modifier
                    .contains(Modifier::DIM)
            })
        };
        let rows: Vec<(String, bool)> = app
            .pending
            .iter()
            .map(|line| (line.to_string(), !line.spans.is_empty() && dim(line)))
            .collect();
        assert_eq!(rows[0], ("• I'll look.".into(), true));
        assert_eq!(rows[2], ("• Ran ls".into(), true));
        let line = rows
            .iter()
            .position(|(row, _)| row.starts_with("─ Worked"))
            .unwrap();
        assert_eq!(
            rows[line + 1..],
            [("".into(), false), ("• Done.".into(), false)]
        );
    }

    #[tokio::test]
    async fn a_turn_the_lookout_started_ends_quietly_unless_it_speaks() {
        let client = daemon_answering(|_| {
            json!({"items": [{"node": 4, "item": {"type": "message", "role": "assistant",
                "content": [{"type": "output_text", "text": "Docs are blocked."}]}}]})
        })
        .await;
        let mut app = app(client);
        let lines = |app: &App| -> Vec<String> {
            app.pending.iter().map(|line| line.to_string()).collect()
        };
        let note = json!({"request_id": "lookout-ab12"});
        // Read what came in, nothing to say: no line under it.
        app.daemon(turn_event("accepted", 1, note.clone())).await;
        assert!(app.running.as_ref().is_some_and(|r| r.note));
        app.daemon(turn_event(
            "turn_finished",
            1,
            json!({"status": "completed"}),
        ))
        .await;
        assert!(!lines(&app).iter().any(|row| row.starts_with("─ Worked")));
        // One that says something ends as any other turn.
        app.daemon(turn_event("accepted", 2, note)).await;
        app.daemon(
            json!({"event": "text_delta", "bot": "b", "turn": 2, "text": "Docs are blocked."}),
        )
        .await;
        app.daemon(turn_event("message", 2, json!({"node": 4})))
            .await;
        app.daemon(turn_event(
            "turn_finished",
            2,
            json!({"status": "completed"}),
        ))
        .await;
        let rows = lines(&app);
        assert!(rows.iter().any(|row| row.starts_with("─ Worked")));
        assert_eq!(rows.last().map(String::as_str), Some("• Docs are blocked."));
        // A reply long enough to spill above as it streams still speaks.
        app.pending.clear();
        app.height = 8;
        app.daemon(turn_event(
            "accepted",
            4,
            json!({"request_id": "lookout-cd34"}),
        ))
        .await;
        let long: String = (1..=12).map(|n| format!("line {n}\n")).collect();
        app.daemon(json!({"event": "text_delta", "bot": "b", "turn": 4, "text": long}))
            .await;
        assert!(app.live.as_ref().is_some_and(|live| live.spilled));
        app.daemon(turn_event(
            "turn_finished",
            4,
            json!({"status": "completed"}),
        ))
        .await;
        assert!(lines(&app).iter().any(|row| row.starts_with("─ Worked")));
        // The person's own turn, with nothing said, still ends with its line.
        app.pending.clear();
        app.daemon(turn_event("accepted", 3, json!({"request_id": "r3"})))
            .await;
        app.daemon(turn_event(
            "turn_finished",
            3,
            json!({"status": "completed"}),
        ))
        .await;
        assert!(lines(&app).iter().any(|row| row.starts_with("─ Worked")));
    }

    #[tokio::test]
    async fn an_empty_completion_ends_the_turn_with_no_failure_shown() {
        let client = daemon_answering(|_| {
            json!({"items": [{"node": 4, "item": {"type": "message", "role": "assistant",
                "content": [{"type": "output_text", "text": "Started it."}]}}]})
        })
        .await;
        let mut app = app(client);
        let lines = |app: &App| -> Vec<String> {
            app.pending.iter().map(|line| line.to_string()).collect()
        };
        let empty = json!({"status": "failed", "error": "empty_completion"});
        // Nothing said: nothing drawn, not even the turn's line.
        app.daemon(turn_event("accepted", 1, json!({"request_id": "r1"})))
            .await;
        app.daemon(turn_event("turn_finished", 1, empty.clone()))
            .await;
        assert!(lines(&app).is_empty(), "{:?}", lines(&app));
        assert!(app.running.is_none());
        // Words before it end under the turn's line, as a turn that ended
        // well, with no error on it.
        app.daemon(turn_event("accepted", 2, json!({"request_id": "r2"})))
            .await;
        app.daemon(json!({"event": "text_delta", "bot": "b", "turn": 2, "text": "Started it."}))
            .await;
        app.daemon(turn_event("message", 2, json!({"node": 4})))
            .await;
        app.daemon(turn_event("turn_finished", 2, empty)).await;
        let rows = lines(&app);
        let line = rows.iter().find(|row| row.starts_with("─ ")).unwrap();
        assert!(line.starts_with("─ Worked"), "{line}");
        assert!(!line.contains("empty_completion"));
        // Another failure still says what it was.
        app.pending.clear();
        app.daemon(turn_event("accepted", 3, json!({"request_id": "r3"})))
            .await;
        app.daemon(turn_event(
            "turn_finished",
            3,
            json!({"status": "failed", "error": "provider_error"}),
        ))
        .await;
        assert!(lines(&app).iter().any(|row| row.contains("provider_error")));
    }

    #[tokio::test]
    async fn an_attached_message_is_a_clip_in_the_footer_until_a_call_takes_it() {
        let client = daemon_with(json!([])).await;
        let mut app = app(client);
        app.daemon(turn_event("attached", 5, json!({"status": "attached"})))
            .await;
        assert_eq!(app.notice, None, "nothing for the person to do");
        assert!(app.footer_line(120).to_string().contains("📎 ·"));
        app.daemon(turn_event("attached", 6, json!({"status": "attached"})))
            .await;
        assert!(app.footer_line(120).to_string().contains("📎 2 ·"));
        // Taken in by the running turn's next call.
        for turn in [5, 6] {
            app.daemon(turn_event(
                "turn_finished",
                turn,
                json!({"status": "steered", "into": 4}),
            ))
            .await;
        }
        assert!(!app.footer_line(120).to_string().contains('📎'));
    }

    #[tokio::test]
    async fn an_emoji_reacts_to_a_reply_without_a_turn_and_again_takes_it_off() {
        let sent = Arc::new(std::sync::Mutex::new(Vec::new()));
        let seen = sent.clone();
        let client = daemon_answering(move |asked| {
            if asked["op"] == "submit" {
                seen.lock().unwrap().push(asked.clone());
            }
            json!({"turn": 9})
        })
        .await;
        let mut app = app(client);
        app.turn_model = Some("opus".into());
        app.any_history = true;
        let rows = |app: &mut App| {
            std::mem::take(&mut app.pending)
                .iter()
                .map(|l| l.to_string())
                .collect::<Vec<_>>()
        };
        app.input.set("👍");
        app.submit().await;
        assert_eq!(app.notice.as_deref(), Some("no reply to react to yet"));
        app.saw_reply(4, "Merged it.\nThe notes are updated.");
        app.saw_reply(8, "Tagged 0.4.0.");
        // The last reply: the emoji alone, attached, right under it.
        app.input.set(":+1:");
        app.submit().await;
        let first = sent.lock().unwrap()[0].clone();
        assert_eq!(first["prompt"], "👍");
        assert_eq!(first["delivery"], "attach");
        assert!(first["model"].is_null() && first["workspace"].is_null());
        assert!(app.input.is_empty());
        assert_eq!(rows(&mut app), ["   👍 "], "no blank row above it");
        assert_eq!(app.reactions.on(8), ["👍"]);
        app.daemon(turn_event("attached", 9, json!({}))).await;
        assert_eq!(app.notice, None, "a reaction attaching says nothing");
        // An older reply, picked with /react: quoted, so the bot knows.
        app.input.set("/react");
        app.submit().await;
        assert_eq!(app.reacting, Some(0));
        app.key(Event::Key(KeyCode::Up.into())).await;
        app.key(Event::Key(KeyCode::Enter.into())).await;
        assert_eq!(app.react_to, Some((4, "Merged it.".into())));
        app.input.set("🎉");
        app.submit().await;
        assert_eq!(sent.lock().unwrap()[1]["prompt"], "> Merged it.\n\n🎉");
        assert_eq!(rows(&mut app), ["", "  ↳ 🎉 on “Merged it.”"]);
        // The same emoji again takes it off, telling the bot nothing.
        app.input.set("👍");
        app.submit().await;
        assert_eq!(sent.lock().unwrap().len(), 2);
        assert_eq!(rows(&mut app), ["", "  ↳ took back 👍 on “Tagged 0.4.0.”"]);
        assert!(app.reactions.on(8).is_empty());
        // Words with an emoji are a message, sent as one.
        app.input.set("👍 ship it");
        app.submit().await;
        assert_eq!(sent.lock().unwrap()[2]["prompt"], "👍 ship it");
        assert!(sent.lock().unwrap()[2]["delivery"].is_null());
    }

    #[tokio::test]
    async fn a_reaction_to_an_older_reply_goes_under_that_reply() {
        let client = daemon_answering(|_| json!({"turn": 9})).await;
        let mut app = app(client);
        app.commit_said(
            4,
            transcript::reply("Merged it.", 80, std::path::Path::new("/")),
        );
        app.saw_reply(4, "Merged it.");
        app.land(6, "and tag it", None);
        app.commit_said(
            8,
            transcript::reply("Tagged 0.4.0.", 80, std::path::Path::new("/")),
        );
        app.saw_reply(8, "Tagged 0.4.0.");
        app.react_to = Some((4, "Merged it.".into()));
        app.input.set("🎉");
        app.submit().await;
        let rows = drawn(&mut app);
        assert_eq!(rows[..2], ["• Merged it.", "   🎉 "], "{rows:?}");
        assert_eq!(rows.last().map(String::as_str), Some("• Tagged 0.4.0."));
        // Another joins it on the same row; taking one back leaves the other.
        app.react_to = Some((4, "Merged it.".into()));
        app.input.set("👍");
        app.submit().await;
        assert_eq!(drawn(&mut app)[1], "   🎉 👍 ");
        app.react_to = Some((4, "Merged it.".into()));
        app.input.set("🎉");
        app.submit().await;
        assert_eq!(drawn(&mut app)[1], "   👍 ");
        app.react_to = Some((4, "Merged it.".into()));
        app.input.set("👍");
        app.submit().await;
        assert_eq!(drawn(&mut app)[..2], ["• Merged it.", ""]);
    }

    #[tokio::test]
    async fn the_bots_reaction_goes_under_the_message_once_its_turn_ends() {
        let client = daemon_answering(move |asked| {
            let item = match asked["nodes"][0].as_i64() {
                Some(1) => json!({"type": "message", "role": "user",
                    "content": [{"type": "input_text", "text": "merged, thanks"}]}),
                Some(5) => json!({"type": "message", "role": "user",
                    "content": [{"type": "input_text", "text": "and the notes?"}]}),
                Some(7) => json!({"type": "message", "role": "assistant",
                    "content": [{"type": "output_text", "text": "Updated."}]}),
                _ => return json!({}),
            };
            json!({"items": [{"node": asked["nodes"][0], "item": item}]})
        })
        .await;
        let mut app = app(client);
        let react = |turn: i64, id: &str, arguments: Value| {
            turn_event(
                "tool_started",
                turn,
                json!({"call_id": id, "name": "react", "arguments": arguments.to_string()}),
            )
        };
        // A turn that only reacts: right under the message as it reacts,
        // 👀 taken off again, and no line closes the turn.
        app.daemon(turn_event("accepted", 1, json!({"node": 1})))
            .await;
        for (id, arguments) in [
            ("c1", json!({"emoji": "👀"})),
            ("c2", json!({"emoji": "👍"})),
        ] {
            app.daemon(react(1, id, arguments)).await;
            app.daemon(turn_event("tool_completed", 1, json!({"call_id": id})))
                .await;
        }
        assert_eq!(drawn(&mut app), ["› merged, thanks", "   👀 👍 "]);
        app.daemon(react(1, "c3", json!({"emoji": "👀", "remove": true})))
            .await;
        app.daemon(turn_event("tool_completed", 1, json!({"call_id": "c3"})))
            .await;
        assert_eq!(drawn(&mut app), ["› merged, thanks", "   👍 "]);
        app.daemon(turn_event(
            "turn_finished",
            1,
            json!({"status": "completed"}),
        ))
        .await;
        assert_eq!(drawn(&mut app), ["› merged, thanks", "   👍 "]);
        // One that answers too: under its own message, not the reply.
        app.daemon(turn_event("accepted", 2, json!({"node": 5})))
            .await;
        app.daemon(react(2, "c4", json!({"emoji": "🎉"}))).await;
        app.daemon(turn_event("tool_completed", 2, json!({"call_id": "c4"})))
            .await;
        // One the daemon refused is not drawn.
        app.daemon(react(2, "c5", json!({"emoji": "👀", "remove": "yes"})))
            .await;
        app.daemon(turn_event(
            "tool_completed",
            2,
            json!({"call_id": "c5", "failed": true}),
        ))
        .await;
        app.daemon(turn_event("message", 2, json!({"node": 7})))
            .await;
        app.daemon(turn_event(
            "turn_finished",
            2,
            json!({"status": "completed"}),
        ))
        .await;
        let rows = drawn(&mut app);
        assert_eq!(rows[2..5], ["", "› and the notes?", "   🎉 "]);
        assert!(rows[6].starts_with("─ Worked"), "{rows:?}");
        assert_eq!(rows[7..], ["", "• Updated."]);
        // Once that message is no longer in the conversation, a reaction
        // goes under what was drawn last, saying which message.
        app.seen_node = 0;
        app.daemon(turn_event("accepted", 3, json!({"node": 5})))
            .await;
        app.flush();
        app.screen.clear();
        app.daemon(react(3, "c6", json!({"emoji": "👀"}))).await;
        app.daemon(turn_event("tool_completed", 3, json!({"call_id": "c6"})))
            .await;
        app.daemon(turn_event("message", 3, json!({"node": 7})))
            .await;
        app.daemon(turn_event(
            "turn_finished",
            3,
            json!({"status": "completed"}),
        ))
        .await;
        assert_eq!(
            drawn(&mut app).last().map(String::as_str),
            Some("  ↳ reacted 👀 to “and the notes?”")
        );
    }

    #[tokio::test]
    async fn replayed_bot_reactions_are_drawn_as_they_were_live() {
        let items = json!({
            "1": {"type": "message", "role": "user",
                  "content": [{"type": "input_text", "text": "merged, thanks"}]},
            "2": {"type": "function_call", "name": "react", "call_id": "c1",
                  "arguments": "{\"emoji\":\"👍\"}"},
            "3": {"type": "function_call_output", "call_id": "c1", "output": "Done."},
            "4": {"type": "message", "role": "user",
                  "content": [{"type": "input_text", "text": "and the notes?"}]},
            "5": {"type": "function_call", "name": "react", "call_id": "c2",
                  "arguments": "{\"emoji\":\"👀\",\"remove\":\"yes\"}"},
            "6": {"type": "function_call_output", "call_id": "c2",
                  "output": "{\"error\":\"invalid_arguments\"}"},
            "7": {"type": "function_call", "name": "react", "call_id": "c3",
                  "arguments": "{\"emoji\":\"🎉\"}"},
            "8": {"type": "function_call_output", "call_id": "c3", "output": "Done."},
            "9": {"type": "message", "role": "assistant",
                  "content": [{"type": "output_text", "text": "Updated."}]},
        });
        let client = daemon_answering(move |asked| match asked["op"].as_str() {
            Some("history_nodes") => {
                json!({"nodes": (1..=9).rev().map(|node| json!({"node": node}))
                .collect::<Vec<_>>(), "next_from": null})
            }
            _ => json!({"items": asked["nodes"].as_array().into_iter().flatten()
                .map(|node| json!({"node": node, "item": items[node.to_string()]}))
                .collect::<Vec<_>>()}),
        })
        .await;
        let mut app = app(client);
        app.history().await;
        assert_eq!(
            drawn(&mut app),
            [
                "› merged, thanks",
                "   👍 ",
                "",
                "› and the notes?",
                "   🎉 ",
                "",
                "• Updated.",
            ]
        );
    }

    #[tokio::test]
    async fn the_bot_is_given_react_once_no_turn_runs() {
        let asked = Arc::new(std::sync::Mutex::new(Vec::new()));
        let seen = asked.clone();
        let client = daemon_answering(move |request| {
            seen.lock().unwrap().push(request.clone());
            json!({"bot": "b", "changed": ["declared"]})
        })
        .await;
        let mut app = app(client);
        app.declare = declared::declare(&json!({}), false);
        app.running = Some(Running {
            turn: 1,
            started: Instant::now(),
            phase: "Working".into(),
            note: false,
        });
        assert!(!app.configure().await);
        assert!(asked.lock().unwrap().is_empty());
        app.running = None;
        assert!(app.configure().await);
        let sent = asked.lock().unwrap()[0].clone();
        assert_eq!(sent["op"], "configure");
        assert_eq!(sent["declare"], json!(declared::tools(false)));
        assert!(app.declare.is_none());
    }

    #[tokio::test]
    async fn a_bot_gated_as_sonny_does_not_gets_its_gated_tools_with_a_picked_mode() {
        let asked = Arc::new(std::sync::Mutex::new(Vec::new()));
        let seen = asked.clone();
        let client = daemon_answering(move |request| {
            seen.lock().unwrap().push(request.clone());
            json!({"bot": "b", "changed": ["declared", "gates"]})
        })
        .await;
        let mut app = app(client);
        app.tools = vec!["shell".into()];
        let custom = json!({"gates": [{"tag": "ci", "tools": ["shell"]}]});
        app.mode = approvals::mode(&custom);
        app.declared = declared::declare(&custom, true).unwrap();
        app.gating = Some("ask");
        assert!(app.configure().await);
        let sent = asked.lock().unwrap()[0].clone();
        let names: Vec<&str> = (sent["declare"].as_array().into_iter().flatten())
            .filter_map(|tool| tool["name"].as_str())
            .collect();
        assert!(names.contains(&"run_in_pane"), "{names:?}");
        assert!(
            sent["approve"]
                .as_array()
                .unwrap()
                .contains(&json!("run_in_pane"))
        );
        assert_eq!(sent["approver"], "manual");
    }

    #[tokio::test]
    async fn replayed_reactions_sit_under_their_reply_and_their_messages_are_not_drawn() {
        let items = json!({
            "1": {"type": "message", "role": "assistant",
                  "content": [{"type": "output_text", "text": "Merged it."}]},
            "2": {"type": "message", "role": "user",
                  "content": [{"type": "input_text", "text": "what next?"}]},
            "3": {"type": "message", "role": "user",
                  "content": [{"type": "input_text", "text": "> Merged it.\n\n🎉"}]},
        });
        let client = daemon_answering(move |asked| match asked["op"].as_str() {
            Some("history_nodes") => {
                json!({"nodes": [{"node": 3}, {"node": 2}, {"node": 1}], "next_from": null})
            }
            _ => json!({"items": asked["nodes"].as_array().into_iter().flatten()
                .map(|node| json!({"node": node, "item": items[node.to_string()]}))
                .collect::<Vec<_>>()}),
        })
        .await;
        let mut app = app(client);
        app.reactions.toggle(1, "🎉").unwrap();
        app.history().await;
        let rows: Vec<String> = app.pending.iter().map(|l| l.to_string()).collect();
        assert_eq!(rows, ["• Merged it.", "   🎉 ", "", "› what next?"]);
        assert_eq!(app.replies, [(1, "Merged it.".to_owned())]);
    }

    #[tokio::test]
    async fn the_clean_view_keeps_the_words_and_a_task_started_but_not_a_command() {
        let started = r#"{"bot":"b.fix","handle":"turn:b.fix/1","status":"running","turn":1}"#;
        let client = daemon_answering(move |asked| {
            let item = match asked["nodes"][0].as_i64() {
                Some(2) => json!({"type": "message", "role": "assistant",
                    "content": [{"type": "output_text", "text": "I'll look."}]}),
                Some(3) => json!({"type": "function_call_output", "call_id": "c1", "output": "a"}),
                Some(4) => {
                    json!({"type": "function_call_output", "call_id": "c2", "output": started})
                }
                Some(5) => json!({"type": "message", "role": "assistant",
                    "content": [{"type": "output_text", "text": "Done."}]}),
                _ => return json!({}),
            };
            json!({"items": [{"node": asked["nodes"][0], "item": item}]})
        })
        .await;
        let mut app = app(client);
        let said = |text: &str| json!({"event": "text_delta", "bot": "b", "turn": 1, "text": text});
        let call = |id: &str, command: &str, node: i64| {
            let arguments = json!({"command": command}).to_string();
            [
                turn_event(
                    "tool_started",
                    1,
                    json!({"call_id": id, "name": "shell", "arguments": arguments}),
                ),
                turn_event("tool_completed", 1, json!({"call_id": id, "node": node})),
            ]
        };
        app.daemon(turn_event("accepted", 1, json!({}))).await;
        app.daemon(said("I'll look.")).await;
        app.daemon(turn_event("message", 1, json!({"node": 2})))
            .await;
        for event in call("c1", "ls", 3) {
            app.daemon(event).await;
        }
        let run = r#""$AGENT_BIN" run --detach --new --bot b.fix -- 'Fix the tests.'"#;
        for event in call("c2", run, 4) {
            app.daemon(event).await;
        }
        app.daemon(said("Done.")).await;
        app.daemon(turn_event("message", 1, json!({"node": 5})))
            .await;
        app.daemon(turn_event(
            "turn_finished",
            1,
            json!({"status": "completed"}),
        ))
        .await;
        let rows: Vec<String> = app.pending.iter().map(|line| line.to_string()).collect();
        assert_eq!(
            rows[..5],
            [
                "• I'll look.",
                "",
                "• Started b.fix · turn 1",
                "  └ Fix the tests.",
                ""
            ]
        );
        assert!(rows.iter().all(|row| !row.contains("Ran")), "{rows:?}");
        assert_eq!(rows.last().map(String::as_str), Some("• Done."));
    }

    #[tokio::test]
    async fn words_between_agents_are_one_quiet_row_a_click_opens() {
        // Opened, the message is read again from the history.
        let client = daemon_answering(|asked| match asked["op"].as_str() {
            Some("history_items") => json!({"items": [{"node": 4, "item": {"type": "message",
                "role": "user", "content": "The build is green.\nI pushed the fix to calc.add."}}]}),
            _ => json!({}),
        })
        .await;
        let mut app = app(client);
        app.width = 80;
        app.land(
            4,
            "The build is green.\nI pushed the fix to calc.add.",
            Some("calc.add"),
        );
        let updates = "Task updates: what happened in your tasks since you last heard.\n\
            - calc.neg: turn:calc.neg/3 completed";
        app.land(5, updates, None);
        assert_eq!(
            drawn(&mut app),
            [
                "↳ calc.add: The build is green. ▸",
                "",
                "↳ Task updates · calc.neg completed ▸"
            ]
        );
        // A click on one opens it whole, and again shuts it.
        app.follow("sonny:said/4");
        let button = app.pressed.pop().unwrap();
        app.press(&button).await;
        assert_eq!(
            drawn(&mut app),
            [
                "↳ from calc.add ▾",
                "  The build is green.",
                "  I pushed the fix to calc.add.",
                "",
                "↳ Task updates · calc.neg completed ▸"
            ]
        );
        app.press("said/4").await;
        assert_eq!(drawn(&mut app)[0], "↳ calc.add: The build is green. ▸");
        // The person's own words are drawn as ever.
        app.land(6, "thanks", None);
        assert_eq!(drawn(&mut app).last().unwrap(), "› thanks");
    }

    #[tokio::test]
    async fn agents_messaged_in_a_row_fold_into_one_row_a_click_opens() {
        let client = daemon_answering(|_| json!({})).await;
        let mut app = app(client);
        app.width = 80;
        let start = |app: &mut App, name: &str| {
            let command = format!("\"$AGENT_BIN\" run --bot {name} \"Fix {name}\"");
            app.commit_call("c", "shell", &json!({"command": command}), "", false);
        };
        app.commit(vec![Line::from("› fix the three bugs")]);
        start(&mut app, "calc.add");
        assert_eq!(
            drawn(&mut app),
            [
                "› fix the three bugs",
                "",
                "• Messaged calc.add",
                "  └ Fix calc.add"
            ]
        );
        start(&mut app, "calc.neg");
        start(&mut app, "calc.div");
        assert_eq!(
            drawn(&mut app),
            [
                "› fix the three bugs",
                "",
                "• Messaged calc.add, calc.neg and calc.div ▸"
            ]
        );
        // A click on it opens it, and again folds it.
        app.follow("sonny:fold/0");
        let button = app.pressed.pop().unwrap();
        app.press(&button).await;
        let open = drawn(&mut app);
        assert_eq!(open[2], "• Messaged calc.add, calc.neg and calc.div ▾");
        assert_eq!(
            open[3..],
            [
                "• Messaged calc.add",
                "  └ Fix calc.add",
                "• Messaged calc.neg",
                "  └ Fix calc.neg",
                "• Messaged calc.div",
                "  └ Fix calc.div"
            ]
        );
        app.follow("sonny:fold/0");
        let button = app.pressed.pop().unwrap();
        app.press(&button).await;
        assert_eq!(drawn(&mut app).len(), 3);
        // Something drawn between, or another kind of call, starts anew;
        // a call the clean view hides does not.
        app.commit_call("c", "shell", &json!({"command": "ls"}), "a", false);
        start(&mut app, "calc.mul");
        app.commit_call(
            "c",
            served::REMOVE,
            &json!({"task": "calc.add"}),
            "{}",
            false,
        );
        app.commit(vec![Line::from("• Messaged those.")]);
        start(&mut app, "calc.sub");
        let rows = drawn(&mut app);
        assert_eq!(
            rows[2],
            "• Messaged calc.add, calc.neg, calc.div and calc.mul ▸"
        );
        assert_eq!(rows[4], "• Removed calc.add");
        assert_eq!(rows[8], "• Messaged calc.sub");
    }

    #[tokio::test]
    async fn tasks_started_in_a_row_are_one_roster_a_click_opens_one_from() {
        let reads = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let read = reads.clone();
        let client = daemon_answering(move |asked| {
            let n = read.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            match asked["bot"].as_str() {
                Some("calc.add") => json!({"turns": [{"turn": 1, "status": "completed"}]}),
                // A message steered into a running turn: the task still works.
                Some("calc.neg") if n < 3 => json!({"turns": [
                    {"turn": 2, "status": "steered"}, {"turn": 1, "status": "running"}]}),
                Some("calc.gone") => json!({"error": "bot_not_found"}),
                // The daemon not there for a moment, then there again.
                _ if (3..5).contains(&n) => json!({"error": "daemon_unavailable"}),
                _ => json!({"turns": [{"turn": 1, "status": "running"}]}),
            }
        })
        .await;
        let mut app = app(client);
        app.width = 80;
        let start = |app: &mut App, name: &str| {
            let arguments = json!({"name": name, "brief": format!("Fix {name}")});
            let output = format!("turn:calc.{name}/1");
            app.commit_call("c", served::START, &arguments, &output, false);
        };
        start(&mut app, "add");
        start(&mut app, "neg");
        let rows = drawn(&mut app);
        assert_eq!(rows[0], "◆ Started 2 tasks · click one to open it");
        assert_eq!(rows[1], "  ○ add  starting  Fix add");
        // What each does is read, and drawn where they are.
        app.read_tasks().await;
        let rows = drawn(&mut app);
        assert_eq!(rows[1], "  ✓ add  done     Fix add");
        assert_eq!(rows[2], "  ● neg  working  Fix neg");
        // A task the daemon could not read stays as it was; one it does not
        // have is removed.
        start(&mut app, "gone");
        app.read_tasks().await;
        let rows = drawn(&mut app);
        assert_eq!(rows[2], "  ● neg   working  Fix neg");
        assert_eq!(rows[3], "  ■ gone  removed  Fix gone");
        // A click on a task's row opens that task.
        let links = app.screen.links();
        assert_eq!(links[0], None);
        assert_eq!(links[2].as_deref(), Some("sonny:task/calc.neg"));
    }

    #[tokio::test]
    async fn a_task_that_ended_with_nothing_to_say_is_done() {
        let client = daemon_answering(|asked| match asked["op"].as_str() {
            Some("wait") => json!({"results": {"turn:calc.quiet/1":
                {"status": "failed", "error": "empty_completion"}}}),
            _ => match asked["bot"].as_str() {
                Some("calc.quiet") => json!({"turns": [
                    {"turn": 1, "status": "failed", "handle": "turn:calc.quiet/1"}]}),
                _ => {
                    json!({"turns": [{"turn": 1, "status": "failed", "handle": "turn:calc.bad/1"}]})
                }
            },
        })
        .await;
        let mut app = app(client);
        app.width = 80;
        for name in ["quiet", "bad"] {
            let arguments = json!({"name": name, "brief": format!("Fix {name}")});
            let output = format!("turn:calc.{name}/1");
            app.commit_call("c", served::START, &arguments, &output, false);
        }
        app.read_tasks().await;
        let rows = drawn(&mut app);
        assert_eq!(rows[1], "  ✓ quiet  done    Fix quiet");
        assert_eq!(rows[2], "  ✗ bad    failed  Fix bad");
    }

    #[tokio::test]
    async fn a_click_on_an_option_answers_the_question_with_it() {
        let sent = Arc::new(std::sync::Mutex::new(Vec::new()));
        let seen = sent.clone();
        let client = daemon_answering(move |asked| {
            if asked["op"] == "submit" {
                seen.lock().unwrap().push(asked["prompt"].clone());
            }
            json!({"turn": 9})
        })
        .await;
        let mut app = app(client);
        app.width = 80;
        let arguments = json!({"question": "Merge?", "options": [
            {"label": "Yes", "explanation": "Now."}, {"label": "No", "explanation": "Later."}]})
        .to_string();
        app.daemon(turn_event("accepted", 4, json!({}))).await;
        app.daemon(turn_event(
            "tool_started",
            4,
            json!({"call_id": "a4", "name": "ask", "arguments": arguments}),
        ))
        .await;
        app.daemon(turn_event("tool_completed", 4, json!({"call_id": "a4"})))
            .await;
        assert_eq!(app.asks.len(), 1);
        // The option's row is a button, drawn as the rest of the question.
        let area = Rect::new(0, 0, 80, 12);
        let mut buf = ratatui::buffer::Buffer::empty(area);
        app.flush();
        app.screen.render(area, &mut buf, |_| None);
        let row = (0..12)
            .find(|y| {
                (0..80)
                    .map(|x| buf[(x, *y)].symbol())
                    .collect::<String>()
                    .contains("2  No")
            })
            .unwrap();
        assert!(!buf[(5, row)].modifier.contains(Modifier::UNDERLINED));
        use ratatui::crossterm::event::{MouseButton, MouseEvent, MouseEventKind};
        for kind in [
            MouseEventKind::Down(MouseButton::Left),
            MouseEventKind::Up(MouseButton::Left),
        ] {
            let click = MouseEvent {
                kind,
                column: 5,
                row,
                modifiers: KeyModifiers::NONE,
            };
            app.key(Event::Mouse(click)).await;
        }
        assert_eq!(sent.lock().unwrap()[..], [json!("You asked: Merge?\n\nNo")]);
        assert!(app.asks.is_empty());
        // Once answered, another click sends nothing.
        app.press("answer/0/a4").await;
        assert_eq!(sent.lock().unwrap().len(), 1);
        assert_eq!(
            app.notice.as_deref(),
            Some("that question was answered already")
        );
    }

    #[tokio::test]
    async fn held_words_survive_a_dropped_connection_and_a_narrower_pane() {
        let client = daemon_answering(|_| json!({})).await;
        let mut app = app(client);
        let said = |text: &str| json!({"event": "text_delta", "bot": "b", "turn": 1, "text": text});
        app.daemon(turn_event("accepted", 1, json!({}))).await;
        app.daemon(said("The pay step is the regression at p95."))
            .await;
        app.daemon(turn_event("message", 1, json!({"node": 2})))
            .await;
        assert!(app.pending.is_empty(), "held until what follows");
        // Narrower than when they were wrapped: cut again, nothing lost.
        let narrow: Vec<String> = app.open_rows(20).iter().map(|l| l.to_string()).collect();
        assert!(narrow.iter().all(|row| row.width() <= 20), "{narrow:?}");
        assert_eq!(
            narrow
                .concat()
                .split_whitespace()
                .collect::<Vec<_>>()
                .join(" "),
            "• The pay step is the regression at p95."
        );
        // The message is past the replay's start, so a drop writes it.
        app.dropped();
        assert!(app.live.is_none());
        let written: Vec<String> = app.pending.iter().map(|l| l.to_string()).collect();
        assert_eq!(
            written.last().unwrap(),
            "• The pay step is the regression at p95."
        );
    }

    #[tokio::test]
    async fn the_footer_names_the_branch() {
        let mut app = app(daemon_answering(|_| json!({})).await);
        app.workspace = "/work/.agent/worktrees/fix-cart/packages/shop".into();
        app.model = "opus".into();
        app.place = place::Place {
            branch: Some("agent/fix-cart".into()),
        };
        let wide = app.footer_line(100).to_string();
        assert!(
            wide.contains("opus · /work/.agent/worktrees/fix-cart/packages/shop on agent/fix-cart"),
            "{wide}"
        );
        // A narrow footer cuts the folder from its start, never the branch.
        let narrow = app.footer_line(56).to_string();
        assert!(
            narrow.contains("…") && narrow.contains("packages/shop on agent/fix-cart"),
            "{narrow}"
        );
        assert_eq!(narrow.trim_end().width(), 56 - 2);
    }

    #[tokio::test]
    async fn ctrl_h_deletes_as_backspace_does() {
        let mut app = app(daemon_answering(|_| json!({})).await);
        app.input.set("helo");
        let ctrl_h = event::KeyEvent::new(KeyCode::Char('h'), KeyModifiers::CONTROL);
        app.key(Event::Key(ctrl_h)).await;
        assert_eq!(&*app.input, "hel");
        app.input.set("/mo");
        app.key(Event::Key(ctrl_h)).await;
        assert_eq!(&*app.input, "/m", "with the command menu open too");
    }

    #[tokio::test]
    async fn ctrl_brackets_step_the_pane_back_and_forward_and_close_and_open_it_at_the_ends() {
        let mut app = app(daemon_answering(|_| json!({})).await);
        let dir = std::env::temp_dir().join(unique("sonny-steps"));
        std::fs::create_dir_all(&dir).unwrap();
        let url = |name: &str| {
            let path = dir.join(name);
            std::fs::write(&path, format!("# {name}\n")).unwrap();
            format!("file://{}", path.display())
        };
        let (a, b) = (url("a.md"), url("b.md"));
        app.workspace = dir.to_string_lossy().into_owned();
        app.full_width = 200;
        let ctrl = |c| {
            Event::Key(event::KeyEvent::new(
                KeyCode::Char(c),
                KeyModifiers::CONTROL,
            ))
        };
        let shown = |app: &App| app.pane.as_ref().and_then(|p| p.url());
        app.follow(&a);
        app.follow(&b);
        assert_eq!(shown(&app), Some(b.clone()));
        // Ctrl+\ gives the keys back; stepping leaves them there.
        app.key(ctrl('\\')).await;
        assert!(!app.pane_keys);
        app.key(ctrl('[')).await;
        assert_eq!(shown(&app), Some(a.clone()));
        assert!(!app.pane_keys);
        app.key(ctrl(']')).await;
        assert_eq!(shown(&app), Some(b.clone()));
        // Back past the first page closes the pane; forward opens it again.
        app.key(ctrl('[')).await;
        app.key(ctrl('[')).await;
        assert_eq!(shown(&app), None);
        app.key(ctrl(']')).await;
        assert_eq!(shown(&app), Some(a.clone()));
        app.key(ctrl('5')).await;
        assert_eq!(shown(&app), Some(b.clone()), "Ctrl+] without the protocol");
        // Esc closes too, and Ctrl+] opens what it closed.
        app.key(Event::Key(KeyCode::Esc.into())).await;
        assert_eq!(shown(&app), None, "Esc closes from the pane");
        app.key(ctrl(']')).await;
        assert_eq!(shown(&app), Some(b));
        // With no pane at all, Ctrl+[ is Esc, as without the protocol.
        app.pane = None;
        app.closed = None;
        app.input.set("draft");
        app.key(ctrl('[')).await;
        app.key(ctrl(']')).await;
        assert_eq!(shown(&app), None);
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[tokio::test]
    async fn pictures_go_with_their_placeholders_and_a_discarded_draft_drops_them() {
        let mut app = app(daemon_answering(|_| json!({})).await);
        let picture = |n| attach::Picture {
            media_type: "image/png",
            data: vec![n],
        };
        app.attach(Ok(picture(1)));
        app.key(Event::Key(event::KeyEvent::new(
            KeyCode::Char('c'),
            KeyModifiers::CONTROL,
        )))
        .await;
        assert!(app.input.is_empty() && app.pictures.is_empty());
        app.attach(Ok(picture(1)));
        app.attach(Ok(picture(2)));
        app.input.set("only [Image #2] now");
        assert_eq!(app.placed(&app.input), [picture(2)]);
        assert_eq!(
            attach::submitted(&app.placed(&app.input)),
            Some(json!([{"media_type": "image/png", "data": "Ag=="}]))
        );
        assert_eq!(attach::submitted(&[]), None);
        // Pictures past what a message takes together are not attached.
        app.input.clear();
        let large = || attach::Picture {
            media_type: "image/jpeg",
            data: vec![0; 3 << 20],
        };
        app.attach(Ok(large()));
        app.attach(Ok(large()));
        assert_eq!(app.placed(&app.input).len(), 1);
        assert!(app.notice.as_deref().unwrap().contains("5 MB together"));
        // A picture whose placeholder is gone, and that Ctrl-Y cannot put
        // back, is let go at the next attachment; Ctrl-Y's is kept.
        let ctrl_u = || {
            Event::Key(event::KeyEvent::new(
                KeyCode::Char('u'),
                KeyModifiers::CONTROL,
            ))
        };
        app.attach(Ok(picture(4)));
        app.key(ctrl_u()).await;
        app.attach(Ok(picture(5)));
        let kept: Vec<_> = app.pictures.iter().map(Option::is_some).collect();
        assert_eq!(kept, [false, false, true, true, true]);
        app.key(ctrl_u()).await;
        app.attach(Ok(picture(6)));
        let kept: Vec<_> = app.pictures.iter().map(Option::is_some).collect();
        assert_eq!(kept, [false, false, false, false, true, true]);
    }

    #[tokio::test]
    async fn the_shell_editing_keys_work_in_the_composer() {
        let mut app = app(daemon_answering(|_| json!({})).await);
        let press = |code, modifiers| Event::Key(event::KeyEvent::new(code, modifiers));
        let ctrl = |c| press(KeyCode::Char(c), KeyModifiers::CONTROL);
        let alt = |c| press(KeyCode::Char(c), KeyModifiers::ALT);
        app.input.set("one two three");
        app.key(ctrl('w')).await;
        app.key(ctrl('a')).await;
        app.key(alt('d')).await;
        assert_eq!(&*app.input, " two ");
        app.key(ctrl('y')).await;
        app.key(ctrl('e')).await;
        app.key(ctrl('j')).await;
        app.key(ctrl('y')).await;
        assert_eq!(&*app.input, "one two \none");
        app.key(ctrl('p')).await;
        app.key(press(KeyCode::Delete, KeyModifiers::CONTROL)).await;
        assert_eq!(&*app.input, "one \none", "Ctrl-P went up a row");
        app.key(ctrl('n')).await;
        app.key(press(
            KeyCode::Char('h'),
            KeyModifiers::CONTROL | KeyModifiers::ALT,
        ))
        .await;
        assert_eq!(&*app.input, "one \n");
        app.key(press(KeyCode::Enter, KeyModifiers::ALT)).await;
        assert_eq!(&*app.input, "one \n\n");
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
        // A custom tool's call and output read as a function's.
        let patch = json!({"type": "custom_tool_call", "call_id": "p", "name": "apply_patch",
            "input": "*** Begin Patch\n*** End Patch"});
        let items = responses(&patch);
        assert_eq!(items[0]["type"], "function_call");
        assert_eq!(
            serde_json::from_str::<Value>(items[0]["arguments"].as_str().unwrap()).unwrap(),
            json!({"input": "*** Begin Patch\n*** End Patch"})
        );
        let done = json!({"type": "custom_tool_call_output", "call_id": "p", "output": "Success."});
        assert_eq!(output(&responses(&done)[0]), ("Success.".into(), false));
    }

    #[tokio::test]
    async fn a_note_shows_in_the_margin_as_its_call_starts_and_a_marker_pins_where() {
        let mut app = app(daemon_with(json!([])).await);
        app.any_history = true;
        let call = |id: &str, arguments: Value| json!({"call_id": id, "name": "margin", "arguments": arguments.to_string()});
        let post = json!({"id": "routing", "title": "Routing today", "markdown": "a → b"});
        app.daemon(turn_event("tool_started", 3, call("m1", post)))
            .await;
        // In the margin before the call is done.
        assert_eq!(app.margin.note("routing").unwrap().title, "Routing today");
        app.daemon(turn_event("tool_completed", 3, json!({"call_id": "m1"})))
            .await;
        let rows: Vec<String> = app.pending.iter().map(|l| l.to_string()).collect();
        assert!(
            rows.iter()
                .any(|r| r == "  ✎ Routing today · in the margin"),
            "{rows:?}"
        );
        let pin = screen::Tag::Note(margin::anchor("m1"));
        assert!(app.marks.iter().any(|(_, m)| *m == screen::Mark::Tag(pin)));
        let link = screen::Mark::Link("sonny:note/routing".into());
        assert!(app.marks.iter().any(|(_, m)| *m == link));
        // Changed in place: the margin shows it, the conversation does not.
        let drawn = app.pending.len();
        let update = json!({"id": "routing", "markdown": "a → c"});
        app.daemon(turn_event("tool_started", 3, call("m2", update)))
            .await;
        app.daemon(turn_event("tool_completed", 3, json!({"call_id": "m2"})))
            .await;
        assert_eq!(app.pending.len(), drawn);
        let note = app.margin.note("routing").unwrap();
        assert_eq!(
            (note.call.as_str(), &note.body),
            ("m1", &margin::Body::Markdown("a → c".into()))
        );
    }

    #[tokio::test]
    async fn what_opens_lays_over_the_margin_and_esc_steps_back_to_it() {
        let mut app = app(daemon_with(json!([])).await);
        app.margin_back();
        assert!(app.pane.as_ref().is_some_and(pane::Pane::is_margin));
        app.open_pane(pane::Pane::card("/status", STATUS_KEYS));
        assert!(app.pane.as_ref().is_some_and(|p| p.is_card() && p.over));
        assert!(app.under.is_some());
        // Esc: back to the margin, the keys with it.
        app.close_pane(true);
        assert!(app.pane.as_ref().is_some_and(pane::Pane::is_margin));
        assert!(app.pane_keys);
        // Esc again closes the margin, and it stays closed.
        app.close_pane(true);
        app.margin_back();
        assert!(app.pane.is_none() && app.margin.closed());
        // A card opened now has no margin under it.
        app.open_pane(pane::Pane::card("/status", STATUS_KEYS));
        assert!(!app.pane.as_ref().unwrap().over);
        app.close_pane(true);
        assert!(app.pane.is_none());
        // /margin brings it back, with the keys.
        app.toggle_margin();
        assert!(app.pane.as_ref().is_some_and(pane::Pane::is_margin));
        assert!(app.pane_keys && !app.margin.closed());
    }

    #[tokio::test]
    async fn back_from_what_is_over_the_margin_is_the_margin_and_forward_lays_it_over_again() {
        let mut app = app(daemon_with(json!([])).await);
        app.margin_back();
        app.open_pane(pane::Pane::card("/status", STATUS_KEYS));
        assert!(app.step_pane(true));
        assert!(app.pane.as_ref().is_some_and(pane::Pane::is_margin));
        // The margin has nothing before it, and stays.
        assert!(!app.step_pane(true));
        assert!(!app.margin.closed());
        assert!(app.step_pane(false));
        assert!(app.pane.as_ref().is_some_and(|p| p.is_card() && p.over));
    }

    #[tokio::test]
    async fn a_file_link_opens_over_the_margin_too() {
        let dir = std::env::temp_dir().join(format!("sonny-over-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("a.md"), "# A\n").unwrap();
        let mut app = app(daemon_with(json!([])).await);
        app.margin_back();
        app.follow(&format!("file://{}", dir.join("a.md").display()));
        assert!(app.pane.as_ref().is_some_and(|p| !p.is_margin() && p.over));
        app.close_pane(true);
        assert!(app.pane.as_ref().is_some_and(pane::Pane::is_margin));
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[tokio::test]
    async fn a_note_the_margin_cannot_show_is_drawn_where_it_was_put() {
        let mut app = app(daemon_with(json!([])).await);
        app.any_history = true;
        let call = |id: &str, arguments: Value| json!({"call_id": id, "name": "margin", "arguments": arguments.to_string()});
        let post = json!({"id": "routing", "title": "Routing today", "markdown": "a to b"});
        app.daemon(turn_event("tool_started", 3, call("m1", post)))
            .await;
        app.daemon(turn_event("tool_completed", 3, json!({"call_id": "m1"})))
            .await;
        app.flush();
        app.commit(vec![Line::from("after")]);
        app.flush();
        let texts = |app: &App| -> Vec<String> {
            (app.screen.texts().iter())
                .map(|t| t.trim().to_owned())
                .filter(|t| !t.is_empty())
                .collect()
        };
        // Too narrow for the margin: the note where it was put.
        app.fold_notes(false);
        assert_eq!(texts(&app)[..], ["✎ Routing today", "a to b", "after"]);
        // Changed, it changes there.
        let update = json!({"id": "routing", "markdown": "a to c"});
        app.daemon(turn_event("tool_started", 3, call("m2", update)))
            .await;
        app.fold_notes(false);
        assert_eq!(texts(&app)[..], ["✎ Routing today", "a to c", "after"]);
        // With room for the margin, its marker alone again.
        app.fold_notes(true);
        assert_eq!(
            texts(&app)[..],
            ["✎ Routing today · in the margin", "after"]
        );
        // Closed, out again; taken away, its marker alone.
        app.fold_notes(false);
        assert_eq!(texts(&app).len(), 3);
        app.margin.remove("routing", None);
        app.fold_notes(false);
        assert_eq!(
            texts(&app)[..],
            ["✎ Routing today · in the margin", "after"]
        );
        let link = Some("sonny:note/routing".to_owned());
        assert!(app.screen.links().contains(&link));
    }

    #[tokio::test]
    async fn one_plan_shows_however_often_it_changes() {
        let mut app = app(daemon_with(json!([])).await);
        app.any_history = true;
        let plans = [
            json!({"steps": ["[>] Read", "[ ] Fix", "[ ] Test"]}),
            json!({"steps": ["[x] Read", "[>] Fix", "[ ] Test"]}),
        ];
        for (n, plan) in plans.iter().enumerate() {
            app.commit_call(&format!("p{n}"), "plan", plan, "Plan set.", false);
            app.commit(vec![Line::from(format!("reply {n}"))]);
        }
        app.flush();
        let texts = |app: &App| -> Vec<String> {
            (app.screen.texts().iter())
                .map(|t| t.trim().to_owned())
                .filter(|t| !t.is_empty())
                .collect()
        };
        // Beside the margin: each a row, the newest plan in the margin.
        app.fold_notes(true);
        assert_eq!(
            texts(&app)[..],
            [
                "• Updated the plan · 0/3 · → Read",
                "reply 0",
                "• Updated the plan · 1/3 · → Fix",
                "reply 1"
            ]
        );
        let margin = app.margin.lines(60, std::path::Path::new("/"), None).0;
        let margin: Vec<String> = margin.iter().map(|l| l.to_string()).collect();
        assert!(margin.iter().any(|r| r.contains("Plan  1/3")), "{margin:?}");
        assert!(margin.iter().any(|r| r.contains("→ Fix")));
        assert!(!margin.iter().any(|r| r.contains("→ Read")));
        // Without it: the newest in full where it was set, the rest a row.
        app.fold_notes(false);
        let rows = texts(&app);
        assert_eq!(rows[0], "• Updated the plan · 0/3 · → Read");
        assert_eq!(rows[2], "• Updated the plan");
        assert!(rows[3..6].iter().any(|r| r.contains("→ Fix")), "{rows:?}");
        assert_eq!(rows.last().unwrap(), "reply 1");
        // A newer plan takes the full rows over.
        let done = json!({"steps": ["[x] Read", "[x] Fix", "[x] Test"]});
        app.commit_call("p2", "plan", &done, "Plan set.", false);
        app.flush();
        app.fold_notes(false);
        let rows = texts(&app);
        assert_eq!(rows[2], "• Updated the plan · 1/3 · → Fix");
        assert!(rows.iter().any(|r| r.contains("✔ Test")), "{rows:?}");
        // Gone from the screen, they are forgotten.
        app.screen.clear();
        app.refold = true;
        app.fold_notes(false);
        assert!(app.plans.is_empty());
    }

    #[tokio::test]
    async fn the_margin_shows_by_itself_only_where_the_conversation_keeps_its_width() {
        let mut app = app(daemon_with(json!([])).await);
        app.margin_back();
        let (chat, margin) = app.split(Rect::new(0, 0, 170, 40));
        assert_eq!((chat.width, margin.map(|m| m.width)), (100, Some(69)));
        let (chat, margin) = app.split(Rect::new(0, 0, 130, 40));
        assert_eq!((chat.width, margin), (130, None));
        // Asked for with Tab, it takes its half as anything opened does.
        app.pane_keys = true;
        let (_, margin) = app.split(Rect::new(0, 0, 130, 40));
        assert_eq!(margin.map(|m| m.width), Some(64));
        // A note's marker clicked shows it there too.
        app.pane_keys = false;
        let post = json!({"id": "repro", "title": "Repro", "markdown": "x"});
        app.margin.apply("m1", &post, None, None);
        app.follow("sonny:note/repro");
        let (_, margin) = app.split(Rect::new(0, 0, 130, 40));
        assert!(margin.is_some());
        // Its ↑ gives the conversation the room back.
        app.follow("sonny:jump/repro");
        let (chat, _) = app.split(Rect::new(0, 0, 130, 40));
        assert_eq!(chat.width, 130);
    }

    #[tokio::test]
    async fn a_note_taken_away_in_another_window_goes_from_this_one() {
        let told = margin::closed_message("repro");
        let client = daemon_answering(move |_| {
            json!({"items": [{"node": 7, "item": {"type": "message", "role": "user",
                "content": [{"type": "input_text", "text": told}]}}]})
        })
        .await;
        let mut app = app(client);
        let post = json!({"id": "repro", "title": "Repro", "markdown": "x"});
        app.margin.apply("m1", &post, None, None);
        app.daemon(turn_event("steered", 4, json!({"node": 7})))
            .await;
        assert!(app.margin.note("repro").is_none());
        // Not drawn as the person's words.
        assert!(
            app.pending
                .iter()
                .all(|l| !l.to_string().contains("margin note"))
        );
    }

    #[tokio::test]
    async fn a_replayed_note_call_that_never_ran_leaves_the_margin_as_it_was() {
        let call = |id: &str, arguments: Value| {
            json!({"type": "function_call", "call_id": id, "name": "margin",
                   "arguments": arguments.to_string()})
        };
        let denied = r#"{"error":"approval_denied","detail":"no"}"#;
        let items = json!({
            "1": call("m1", json!({"id": "kept", "title": "Kept", "markdown": "x"})),
            "2": {"type": "function_call_output", "call_id": "m1", "output": "In the margin."},
            "3": call("m2", json!({"id": "kept", "remove": true})),
            "4": {"type": "function_call_output", "call_id": "m2", "output": denied},
            "5": call("m3", json!({"id": "never", "title": "Never", "markdown": "y"})),
            "6": {"type": "function_call_output", "call_id": "m3", "output": denied},
        });
        let client = daemon_answering(move |asked| match asked["op"].as_str() {
            Some("history_nodes") => json!({
                "nodes": (1..=6).rev().map(|node| json!({"node": node})).collect::<Vec<_>>(),
                "next_from": null,
            }),
            _ => json!({"items": asked["nodes"].as_array().into_iter().flatten()
                .map(|node| json!({"node": node, "item": items[node.to_string()]}))
                .collect::<Vec<_>>()}),
        })
        .await;
        let mut app = app(client);
        app.history().await;
        assert!(app.margin.note("kept").is_some());
        assert!(app.margin.note("never").is_none());
    }

    #[tokio::test]
    async fn taking_a_note_away_tells_the_agent_which_with_its_next_call() {
        let sent = Arc::new(std::sync::Mutex::new(Vec::new()));
        let seen = sent.clone();
        let client = daemon_answering(move |asked| {
            if asked["op"] == "submit" {
                seen.lock().unwrap().push(asked.clone());
            }
            json!({"turn": 12})
        })
        .await;
        let mut app = app(client);
        let post = json!({"id": "repro", "title": "Repro", "markdown": "x"});
        app.margin.apply("m1", &post, None, None);
        app.follow("sonny:close/repro");
        for what in std::mem::take(&mut app.pressed) {
            app.press(&what).await;
        }
        assert!(app.margin.note("repro").is_none());
        let sent = sent.lock().unwrap();
        assert_eq!(sent[0]["delivery"], "attach");
        let told = sent[0]["prompt"].as_str().unwrap();
        assert_eq!(margin::closed(told), Some("repro"));
        assert_eq!(sent[0]["origin"], "margin");
        assert!(quiet_message(told));
        // No clip in the footer for it.
        assert_eq!(app.reaction_turn, Some(12));
    }

    #[tokio::test]
    async fn a_note_the_agent_could_not_be_told_of_stays_to_close_again() {
        let client = daemon_answering(|_| json!({"error": "bot_busy", "detail": "busy"})).await;
        let mut app = app(client);
        let post = json!({"id": "repro", "title": "Repro", "markdown": "x"});
        app.margin.apply("m1", &post, None, None);
        app.follow("sonny:close/repro");
        for what in std::mem::take(&mut app.pressed) {
            app.press(&what).await;
        }
        assert!(app.margin.note("repro").is_some());
        assert!(app.notice.is_some());
    }
}
