//! Sonny, a terminal client for one bot: its conversation goes
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
mod asks;
mod cache;
mod calls;
mod diff;
mod errand;
mod herdr;
mod html;
mod image;
mod js;
mod links;
mod login;
mod lookout;
mod math;
mod mermaid;
mod models;
mod mux;
mod place;
mod project;
mod prompt;
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
mod trigger;
mod user_shell;

use agent_client::{Client, Events};
use ratatui::{
    Terminal, TerminalOptions, Viewport,
    backend::{Backend, CrosstermBackend},
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
/// How long a side chat's copy stays once idle.
const SIDE_EXPIRE: Duration = Duration::from_secs(15 * 60);
/// This window is a side chat (`--side`).
static SIDE: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

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
}

fn usage() -> String {
    "usage: sonny [--store PATH] [--socket PATH] [--model PROVIDER/MODEL] [--no-layout] [--rex-host HOST] [--no-sound]\n\
     \x20      [--copies worktree|sandfs] [--bot NAME [--new [--workspace DIR]]]\n\
     With no --bot, the folder's coordinator, made with --model the first time; its tasks\n\
     open beside it in Rex or Herdr. Type /status for the project's agents.\n\
     --copies (or /copies): how its tasks get their own copy, kept for every folder: git\n\
     worktrees (the default), or sandfs copies (experimental, built in), which take\n\
     uncommitted changes along and work outside git too.\n\
     sonny task NAME [--setup] [RUN FLAGS] -- BRIEF, from the lead's shell or a terminal, makes a task's\n\
     folder that way and starts the task there; sonny task changes NAME [--apply] lists\n\
     (or writes here) what a sandfs copy of a plain folder changed.\n\
     sonny memory ..., sonny project add DIR ... and sonny trigger ... are what\n\
     ~/.agent/memory, ~/.agent/project and ~/.agent/trigger run, which Sonny writes\n\
     at each start.\n\
     sonny ask QUESTION [--choice OPTION]..., from the lead's shell, pins a question for\n\
     the person above the composer until they answer it.\n\
     sonny lookout [--judge PROVIDER/MODEL|off] watches every lead on the daemon and\n\
     wakes it when its tasks need it; Sonny starts one. sonny lookout --replay --judge\n\
     SPEC[,SPEC]... [--limit N] runs its checks over what the store kept.\n\
     sonny agent ARGS... runs Sonny's own `agent` with ARGS, whatever `agent` is on PATH."
        .into()
}

/// `sonny show TASK [--split SIDE [--beside BOT]] [--rex-host HOST]`: put a
/// task's window where the person asked, from the lead's shell. A task gets
/// a window of its own unless told otherwise; `--split` puts it beside the
/// lead's block, or `--beside` another bot's. Another project's lead gets
/// its project's own home instead (a Rex session, a Herdr workspace),
/// running that lead's Sonny, unless it is there already.
async fn show() -> Result<(), String> {
    let usage =
        "usage: sonny show AGENT [--split right|left|above|below [--beside BOT]] [--rex-host HOST]";
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
    let (socket, project) = agents_project(caller.as_deref().unwrap_or(&bot)).await?;
    let me = std::env::current_exe().map_err(|e| e.to_string())?;
    if side.is_none()
        && let Some(other) = bot.strip_suffix(".lead").filter(|p| *p != project)
    {
        return show_lead(&socket, other, &project, host.as_deref(), &me);
    }
    let mut layout = mux::for_agent(&socket, &project, host.as_deref(), None)?;
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

/// Another project's lead in its project's own home, made when needed in
/// the multiplexer `like`'s home is in, running Sonny on it: a session for
/// each lead, and in it a window for each of its tasks.
fn show_lead(
    socket: &std::path::Path,
    project: &str,
    like: &str,
    host: Option<&str>,
    me: &std::path::Path,
) -> Result<(), String> {
    let store = std::env::var_os("AGENT_STORE").map(PathBuf::from);
    let (home, opened) = open_lead(socket, store.as_deref(), project, like, host, me)?;
    let bot = format!("{project}.lead");
    println!("{}", json!({"bot": bot, "in": home, "opened": opened}));
    Ok(())
}

/// Open `project`'s lead in that project's own home, as `show_lead` says,
/// unless its Sonny is there already. Returns the home, and whether it
/// was opened now.
fn open_lead(
    socket: &std::path::Path,
    store: Option<&std::path::Path>,
    project: &str,
    like: &str,
    host: Option<&str>,
    me: &std::path::Path,
) -> Result<(String, bool), String> {
    let mut layout = mux::for_agent(socket, project, host, Some(like))?;
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

/// The daemon's socket and the project `bot` belongs to, from an agent's
/// shell.
async fn agents_project(bot: &str) -> Result<(PathBuf, String), String> {
    let socket = std::env::var_os("AGENT_SOCKET")
        .map(PathBuf::from)
        .ok_or("run this from an agent's shell (AGENT_SOCKET is not set)")?;
    let socket = std::path::absolute(socket).map_err(|e| e.to_string())?;
    let client = Client::connect(&socket).await.map_err(|e| e.to_string())?.0;
    let project = project_of(&client, bot).await;
    Ok((socket, project))
}

/// `sonny run NAME [--split SIDE] [--rex-host HOST] -- COMMAND...`: run a
/// command an agent wants watched (a dev server, a long build) in a block
/// labelled `AGENT/NAME`, in a tab of its own, or with `--split` on that
/// side of the agent's own block, and print the block's ID. It closes with
/// the agent's window.
async fn run_command() -> Result<(), String> {
    let usage =
        "usage: sonny run NAME [--split right|left|above|below] [--rex-host HOST] -- COMMAND...";
    let mut args = std::env::args().skip(2);
    let name = args.next().filter(|n| !n.starts_with('-')).ok_or(usage)?;
    if name.contains('/') {
        return Err("a name without /".into());
    }
    let owner = std::env::var("AGENT_BOT")
        .map_err(|_| "run this from an agent's shell (AGENT_BOT is not set)")?;
    let (mut side, mut host) = (None, None);
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--split" => side = Some(args.next().ok_or(usage)?),
            "--rex-host" => host = Some(args.next().ok_or(usage)?),
            "--" => break,
            _ => return Err(usage.into()),
        }
    }
    let command: Vec<String> = args.collect();
    if command.is_empty() {
        return Err(usage.into());
    }
    let (socket, project) = agents_project(&owner).await?;
    let mut layout = mux::for_agent(&socket, &project, host.as_deref(), None)?;
    let cwd = std::env::current_dir().map_err(|e| e.to_string())?;
    let block = layout.split(&owner, &name, &cwd, &command, side.as_deref())?;
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

/// The lookout, `sonny lookout`, which wakes leads when their tasks need
/// them: started detached, logging beside the store. One already watching
/// this daemon keeps it, and the new one exits.
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
    {
        use std::os::unix::process::CommandExt;
        command.process_group(0);
    }
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
async fn connect(options: &Options) -> Result<(Arc<Client>, agent_client::Events), String> {
    if let Ok(connected) = Client::connect(&options.socket).await {
        return Ok(connected);
    }
    start_daemon(&options.store, &options.socket, false)?;
    Client::connect(&options.socket)
        .await
        .map_err(|e| e.to_string())
}

/// Start the daemon of `store` on `socket` the way the CLI starts it, and
/// wait for it to be ready. With `login`, it gets the login shell's
/// environment, for a start from launchd, which gives no provider keys.
fn start_daemon(
    store: &std::path::Path,
    socket: &std::path::Path,
    login: bool,
) -> Result<(), String> {
    let bin = agent_bin().ok_or(
        "the daemon is not running and Agent's binary was not found; install Sonny with Homebrew, or set AGENT_BIN",
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
        let reason = stderr
            .lines()
            .rev()
            .find_map(|line| serde_json::from_str::<Value>(line).ok())
            .and_then(|failure| Some(failure["detail"].as_str()?.to_owned()))
            .unwrap_or_else(|| stderr.trim().to_owned());
        return Err(format!("could not start the daemon: {reason}"));
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
    let mut side = None;
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
            "--workspace" | "--copies" => {
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
    // sandfs serves task copies' mounts from this program run again as
    // `sonny sandfs ...`, outside any runtime of Sonny's.
    sandfs::handle_relaunch(&["sandfs"]);
    let args: Vec<String> = std::env::args().skip(1).collect();
    let result = match args.split_first() {
        Some((task, rest)) if task == "task" => task::run(rest),
        // `~/.agent/memory` and `~/.agent/project` run these, from agents' shells.
        Some((verb, rest)) if verb == "memory" => {
            return std::process::ExitCode::from(agent_client::memory::cli(rest) as u8);
        }
        Some((verb, rest)) if verb == "project" => {
            return std::process::ExitCode::from(project::cli(rest) as u8);
        }
        Some((verb, rest)) if verb == trigger::VERB => {
            return std::process::ExitCode::from(trigger::cli(rest) as u8);
        }
        // launchd runs this for a trigger's fire.
        Some((flag, rest)) if flag == trigger::FIRE_FLAG => {
            return std::process::ExitCode::from(trigger::fire_cli(rest) as u8);
        }
        Some((ask, rest)) if ask == "ask" => asks::run(rest),
        Some((lookout, rest)) if lookout == "lookout" => lookout::run(rest),
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
    match std::env::args().nth(1).as_deref() {
        Some("show") => return show().await,
        Some("run") => return run_command().await,
        _ => {}
    }
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
            // Sonny on a project's lead is that lead's Sonny, wherever it
            // was opened, as by `sonny show` in the project's own home.
            (bot.clone(), bot.strip_suffix(".lead").map(str::to_owned))
        }
        None => {
            if options.new {
                return Err(usage());
            }
            let dir = std::env::current_dir().map_err(|e| e.to_string())?;
            skills::install_errand();
            if options.layout {
                mux::install_skill(options.rex_host.as_deref());
            }
            let lead = project::open(&client, &dir, options.model.as_deref()).await?;
            fresh = lead.new;
            moving = lead.moving;
            (lead.bot, Some(lead.project))
        }
    };
    // A side chat borrows its agent's pane, and Herdr keeps the agent.
    if options.side.is_none() {
        herdr::announce(&name, reopen());
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
    let mut terminal = Terminal::with_options(
        CrosstermBackend::new(std::io::stdout()),
        TerminalOptions {
            viewport: Viewport::Inline(VIEWPORT),
        },
    )
    .map_err(|e| e.to_string())?;
    let mut app = App::new(client, &name, &bot, options.clone());
    app.context = context;
    if options.side.is_some() {
        let source = name.rsplit_once(".side-").map_or(name.as_str(), |(s, _)| s);
        app.notice = Some(format!(
            "a copy of {source} that answers without acting; {source} carries on"
        ));
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
    app.lead = project::lead(&app.options.socket, &bot);
    // Task copies a restart unmounted are mounted again before any task
    // works in its folder.
    if project.is_some()
        && let Some(error) = task::remount().pop()
    {
        app.notice = Some(error);
    }
    // The lookout knows no leads: it watches a lead's tasks because Sonny
    // subscribes it to them, before the lookout starts so it reads the file.
    if let Some(project) = &project
        && let Err(error) = lookout::subscribe_lead(&app.options.store, &name, project)
    {
        app.notice = Some(format!("the lookout: {error}"));
    }
    if project.is_some()
        && let Err(error) = start_lookout(&app.options)
    {
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

/// The viewport made `height` rows tall, for a composer that grew or
/// shrank. It keeps its top: a taller one pushes history up, and a shorter
/// one leaves blank rows under it that the next history fills.
/// At most `keep` of `rows`, as near the end as still shows `row`; and
/// where `row` is among them.
fn window(mut rows: Vec<Line<'static>>, row: usize, keep: usize) -> (Vec<Line<'static>>, usize) {
    let start = rows.len().saturating_sub(keep).min(row);
    rows.drain(..start);
    rows.truncate(keep);
    (rows, row - start)
}

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
/// How often the footer reads the workspace's branch again.
const PLACE_EVERY: Duration = Duration::from_secs(3);
/// How often the lead's questions are read again.
const ASKS_EVERY: Duration = Duration::from_secs(1);

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
/// (Ghostty, Rex) can mark this pane working, blocked or done; and the
/// same to Herdr, which takes it from its own CLI instead.
fn status_report(state: &str, bot: &str) {
    use base64::Engine;
    // A side chat borrows its agent's pane, which keeps saying what the
    // agent itself is doing.
    if SIDE.load(std::sync::atomic::Ordering::Relaxed) {
        return;
    }
    herdr::report(state);
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
}

impl Live {
    fn new(gap: bool) -> Self {
        Live {
            markdown: Markdown::default(),
            partial: String::new(),
            lines: 0,
            rows: Vec::new(),
            gap,
            spilled: false,
            closed: false,
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

struct Running {
    turn: i64,
    started: Instant,
    /// What the status row says the bot is doing.
    phase: String,
}

/// The most of the person's refused commands kept for their next
/// message, well under what the daemon takes in one.
const UNSENT_BYTES: usize = 64 * 1024;

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
/// `/agents` and `/ps` also open `/status`, unlisted.
const COMMANDS: [(&str, &str); 13] = [
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
    ("/instructions", "the lead's, from its folder again"),
    ("/status", "this project's agents"),
    ("/lookout", "what the lookout watches for this agent"),
    ("/sound", "the chime when work ends, on or off"),
    (
        "/verbose",
        "every tool call, or only what moves the work on",
    ),
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
    /// The lookout's line for the lead.
    lookout: String,
}

struct App {
    client: Arc<Client>,
    bot: String,
    /// The project whose lead this is, when opened as the folder's coordinator.
    project: Option<String>,
    workspace: String,
    /// The workspace's branch and what it is a sandfs copy of, as last
    /// read, for the footer.
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
    /// Lines waiting to be written into scrollback.
    pending: Vec<Line<'static>>,
    /// Lines let go while `/status` held the screen, past what is kept.
    skipped: usize,
    any_history: bool,
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
    /// The lead's questions for the person, oldest first, as `sonny ask`
    /// left them, and when they were last read.
    asks: Vec<asks::Ask>,
    /// Where the lead's questions are kept, once its id is known.
    asks_key: Option<String>,
    asks_read: Option<Instant>,
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
    /// The live area is drawn afresh: the main screen is back from `/status`.
    redraw: bool,
    sounds: sound::Sounds,
    /// The reply text the current model call has streamed.
    said: String,
    /// Prompts sent and not in the conversation yet, by their turn: a
    /// steer until the running turn takes it in, a prompt until its turn
    /// starts. Drawn above the status row until they land.
    staged: Vec<(i64, String)>,
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
    /// Its approval mode, when it is one of Sonny's.
    mode: Option<&'static str>,
    /// `/approvals`, open over the composer: the mode picked.
    choosing: Option<usize>,
    /// The mode picked, set once no turn runs.
    gating: Option<&'static str>,
    /// The mode picked, still to give the agents the lead made, and when
    /// to try next: one busy now takes it once idle.
    spread: Option<(&'static str, Instant)>,
    /// The newest reply's text, for `/copy`.
    last_reply: String,
    /// The side chat open on the alternate screen, a Sonny of its own.
    side: Option<std::process::Child>,
    /// Keys are left to the side chat while it is open.
    keys_paused: Arc<std::sync::atomic::AtomicBool>,
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
    /// that are working or need the person: `lead · done · 2 working`. A
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

    /// Whether a block is the project's: a bot's, a command one asked for
    /// (`BOT/NAME`), or an Errand job's.
    fn owns(&self, label: &str) -> bool {
        let bot = label.split_once('/').map_or(label, |(bot, _)| bot);
        label.starts_with("errand:") || self.doing.contains_key(bot) || self.lineage.contains(bot)
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
                // The judge handed a call to the person.
                "approval_asked" => self.doing(bot, "needs you"),
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
            skipped: 0,
            any_history: false,
            width: 80,
            height: 24,
            live: None,
            streamed: false,
            running: record["running_turn"].as_i64().map(|turn| Running {
                turn,
                started: Instant::now(),
                phase: "Working".into(),
            }),
            calls: HashMap::new(),
            approvals: Vec::new(),
            asks: Vec::new(),
            asks_key: record["bot_id"].as_i64().map(|id| {
                let store =
                    std::fs::canonicalize(&options.store).unwrap_or_else(|_| options.store.clone());
                asks::key(&store, id)
            }),
            asks_read: None,
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
            redraw: false,
            sounds: sound::Sounds::new(false),
            said: String::new(),
            staged: Vec::new(),
            thought: String::new(),
            replay: None,
            options,
            down: false,
            repaint: false,
            first_node: None,
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
            mode: approvals::mode(record),
            choosing: None,
            gating: approvals::refresh(record),
            spread: None,
            last_reply: String::new(),
            side: None,
            keys_paused: Arc::default(),
            shell: None,
            shown: Vec::new(),
            unsent: Vec::new(),
            ran_here: Vec::new(),
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
        let paused = self.keys_paused.clone();
        // Poll, not a bare read: a blocked read holds crossterm's reader
        // lock, and the cursor query the inline viewport makes on a resize
        // then times out (Rex resizes a block as it lays it out).
        std::thread::spawn(move || {
            while !keys_tx.is_closed() {
                if paused.load(std::sync::atomic::Ordering::Relaxed) {
                    std::thread::sleep(Duration::from_millis(50));
                    continue;
                }
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
            if self.spread.is_some_and(|(_, at)| at <= Instant::now()) {
                self.spread().await;
            }
            self.side_closed();
            self.shell_ran().await;
            self.listed();
            self.follow_place().await;
            self.follow_asks();
            self.repaint |= term::switched();
            if self.repaint && self.running.is_none() && self.watch.is_none() && self.side.is_none()
            {
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
            self.shown.extend(user_shell::lines(&ran, self.width));
            let record = user_shell::record(&ran);
            // Refused, it is kept, to go with the next message.
            match self.send(record.clone(), Some(SHELL), false).await {
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

    /// Read the lead's open questions again, every second.
    fn follow_asks(&mut self) {
        if self.asks_read.is_some_and(|at| at.elapsed() < ASKS_EVERY) {
            return;
        }
        self.asks_read = Some(Instant::now());
        self.asks = self.asks_key.as_deref().map(asks::open).unwrap_or_default();
        if self.asks.is_empty() {
            self.chatting = false;
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
            let Some(fields) = approvals::regate(bot, mode) else {
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
    /// in the view just picked, once no turn is running: the screen and scrollback cleared, and the
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
        // Back to the first node this window drew, not only the newest page.
        match self.first_node {
            Some(first) => self.history_since(first - 1, true).await,
            None => self.history().await,
        }
        Ok(())
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
                });
                status_report("working", &self.bot);
            }
            self.approvals_for(self.running.as_ref().map(|r| r.turn))
                .await;
            // A daemon that restarted took the approver and the lookout
            // Sonny started with it.
            if self.project.is_some()
                && let Err(error) = start_lookout(&self.options)
            {
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
    fn commit_call(&mut self, name: &str, arguments: &Value, output: &str, failed: bool) {
        let draw = match self.verbose {
            true => transcript::tool,
            false => transcript::clean,
        };
        let lines = draw(
            name,
            arguments,
            Some(output),
            failed,
            &self.workspace,
            self.width,
        );
        self.commit(transcript::quiet(lines));
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
        let mut asked = json!({"bot": self.bot, "limit": HISTORY_ITEMS});
        if every {
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
            if !every || page["next_from"].is_null() {
                break;
            }
            asked["from"] = page["next_from"].clone();
        }
        nodes.reverse();
        self.seen_node = nodes.last().copied().unwrap_or(self.seen_node);
        if let Some(&first) = nodes.first() {
            self.first_node = Some(self.first_node.map_or(first, |n| n.min(first)));
        }
        let mut calls: HashMap<String, (String, Value)> = HashMap::new();
        // The bot's words, quiet if a call follows them, as they were live.
        let mut said: Option<Vec<Line<'static>>> = None;
        for chunk in nodes.chunks(100) {
            for item in self.items(chunk).await {
                let call = item["type"] == "function_call";
                if let Some(lines) = said.take_if(|_| call || item["role"].is_string()) {
                    self.commit(match call {
                        true => transcript::quiet(lines),
                        false => lines,
                    });
                }
                match (item["type"].as_str(), item["role"].as_str()) {
                    (_, Some("user")) => {
                        // What the person ran is shown as it was when it ran.
                        let lines = landed(&text(&item), &mut self.ran_here, self.width);
                        self.commit(lines);
                    }
                    (_, Some("assistant")) => {
                        let reply = text(&item);
                        if !reply.is_empty() {
                            said = Some(transcript::reply(&reply, self.width));
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
                            self.commit_call(&name, &arguments, &output, failed);
                        }
                    }
                    _ => {}
                }
            }
        }
        if let Some(lines) = said {
            self.commit(lines);
        }
        // A call still running finishes live, after the history.
        self.calls.extend(calls);
    }

    /// Drop the staged prompts that landed while Sonny was away, whose
    /// `accepted` or `steered` it missed: those whose turn no longer waits.
    async fn unstage_landed(&mut self) {
        let ids = self.staged.iter().map(|(turn, _)| *turn);
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
                    .filter(|row| matches!(row["status"].as_str(), Some("queued" | "ready")))
                    .filter_map(|row| row["turn"].as_i64()),
            );
            match page["next_after"].as_i64() {
                Some(next) => after = next,
                None => break,
            }
        }
        self.staged.retain(|(turn, _)| waiting.contains(turn));
    }

    /// What was sent and has not landed yet, dim, the way it will read
    /// once it does.
    fn staged_rows(&self, width: usize) -> Vec<Line<'static>> {
        self.staged
            .iter()
            .flat_map(|(_, prompt)| match user_shell::read(prompt) {
                Some(ran) => user_shell::header(&ran.command, width, true),
                None => prompt
                    .split('\n')
                    .enumerate()
                    .flat_map(|(at, line)| {
                        let lead = if at == 0 { "› " } else { "  " };
                        transcript::wrap(
                            Line::from(line.to_owned()).dim(),
                            width,
                            Span::raw(lead).dim(),
                            Span::raw("  "),
                        )
                    })
                    .collect(),
            })
            .collect()
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
                let held = live.markdown.end(self.width, live.lines == 0);
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
        self.pending.extend(match words {
            Words::Step => transcript::quiet(live.rows),
            Words::Reply => live.rows,
        });
        self.any_history = true;
    }

    fn live_line(&mut self, live: &mut Live, line: &str) {
        let rows = live.markdown.feed(line, self.width, live.lines == 0);
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
                    let lines = landed(&text(item), &mut self.ran_here, self.width);
                    self.commit(lines);
                }
            }
            "steered" => {
                self.staged
                    .retain(|(staged, _)| Some(*staged) != data["steer"].as_i64());
                if let Some(node) = data["node"].as_i64()
                    && let Some(item) = self.items(&[node]).await.first()
                {
                    self.end_live(Words::Reply);
                    let lines = landed(&text(item), &mut self.ran_here, self.width);
                    self.commit(lines);
                }
            }
            "queued" => self.notice = Some(format!("queued behind turn {}", data["behind"])),
            "attached" => self.notice = Some("attached to its next call".into()),
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
                        Live::new(self.any_history)
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
                    self.close_live();
                } else if let Some(item) = &item {
                    self.end_live(Words::Reply);
                    let mut live = Live::new(self.any_history);
                    let rows = transcript::reply(&text(item), self.width);
                    self.live_rows(&mut live, rows);
                    live.closed = true;
                    self.live = Some(live);
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
            "tool_started" => {
                self.said.clear();
                self.thought.clear();
                self.end_live(Words::Step);
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
                self.end_live(Words::Step);
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
                self.commit_call(&name, &arguments, &shown.0, shown.1);
                if self.calls.is_empty() && !self.held.is_empty() {
                    let held = std::mem::take(&mut self.held);
                    self.pending.extend(held);
                }
                self.approvals.retain(|call| call["call_id"] != id);
                if let Some(running) = &mut self.running {
                    running.phase = "Working".into();
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
                // A staged prompt whose turn ended without starting.
                self.staged.retain(|(staged, _)| Some(*staged) != turn);
                if self.running.as_ref().is_some_and(|r| Some(r.turn) != turn) {
                    return;
                }
                let status = data["status"].as_str().unwrap_or("unknown");
                let detail = data["error"].as_str().unwrap_or("");
                let seconds = self.running.take().map(|r| r.started.elapsed().as_secs());
                let line = transcript::finished(status, detail, seconds, self.width);
                self.close_live();
                // A turn that ended well puts its reply under the line, apart
                // from the steps above; one written as it came, or one
                // that failed, keeps the line under it.
                match self.live.take() {
                    Some(live)
                        if !live.spilled
                            && !live.rows.is_empty()
                            && matches!(status, "completed" | "steered") =>
                    {
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
                    lookout: self.lookout_line(),
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
        let lookout = self.lookout_line();
        if let Ok((_, rows)) = self.status_rows().await
            && let Some(watch) = &mut self.watch
        {
            watch.rows = rows;
            watch.jobs = jobs;
            watch.read = Instant::now();
            watch.lookout = lookout;
        }
    }

    /// What the lookout does for this window's lead.
    fn lookout_line(&self) -> String {
        match &self.project {
            Some(_) => lookout::status(&self.options.store, &self.bot),
            None => String::new(),
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
        // Side chats are not the project's agents.
        for bot in page["bots"].as_array().into_iter().flatten() {
            let Some(name) = bot["name"].as_str().filter(|_| bot["expire_ms"].is_null()) else {
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
                KeyCode::Enter if !key.modifiers.contains(KeyModifiers::SHIFT) => {
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
            Event::Paste(text) => self
                .input
                .insert(&text.replace("\r\n", "\n").replace('\r', "\n")),
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
                            self.input.clear();
                        }
                    }
                    KeyCode::Char('d') if ctrl && self.input.is_empty() => self.quit = true,
                    KeyCode::Char('d') if ctrl => self.input.delete(),
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
                    KeyCode::Char('a') if ctrl => self.input.home(),
                    KeyCode::Char('e') if ctrl => self.input.end(),
                    KeyCode::Char('b') if ctrl => self.input.left(),
                    KeyCode::Char('f') if ctrl => self.input.right(),
                    KeyCode::Char('u') if ctrl => self.input.delete_to_start(),
                    KeyCode::Char('k') if ctrl => self.input.delete_to_end(),
                    KeyCode::Char('w') if ctrl => self.input.delete_word(),
                    KeyCode::Char('b') if alt => self.input.word_left(),
                    KeyCode::Char('f') if alt => self.input.word_right(),
                    KeyCode::Char(c) if !ctrl => self.input.insert(c.encode_utf8(&mut [0; 4])),
                    KeyCode::Backspace if ctrl || alt => self.input.delete_word(),
                    KeyCode::Backspace => self.input.backspace(),
                    KeyCode::Delete => self.input.delete(),
                    KeyCode::Left if ctrl || alt => self.input.word_left(),
                    KeyCode::Right if ctrl || alt => self.input.word_right(),
                    KeyCode::Left => self.input.left(),
                    KeyCode::Right => self.input.right(),
                    KeyCode::Home => self.input.home(),
                    KeyCode::End => self.input.end(),
                    KeyCode::Up => {
                        self.input.up(self.width);
                    }
                    KeyCode::Down => {
                        self.input.down(self.width);
                    }
                    KeyCode::Enter if key.modifiers.contains(KeyModifiers::SHIFT) => {
                        self.input.insert("\n")
                    }
                    KeyCode::Enter => self.submit().await,
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
                                self.input.clear();
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
        if prompt == "/lookout" {
            self.input.clear();
            let lines = lookout::subscriptions(&self.options.store, &self.bot);
            self.commit(lines.into_iter().map(|l| Line::from(l).dim()).collect());
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
                    Ok(project::Instructions::Kept { missing }) => {
                        self.notice = Some(project::Instructions::kept_notice(&missing))
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
        if let Some(question) = prompt
            .strip_prefix("/side")
            .filter(|rest| rest.is_empty() || rest.starts_with(char::is_whitespace))
        {
            self.input.clear();
            self.side(question.trim().to_owned()).await;
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
        // What the person ran and the agent could not be given yet goes
        // first, each on its own, and this message joins the turn they start.
        let mut join = false;
        while let Some(record) = self.unsent.first().cloned() {
            if let Err(error) = self.send(record.clone(), Some(SHELL), join).await {
                self.notice = Some(error);
                return;
            }
            self.ran_here.extend(user_shell::read(&record));
            self.unsent.remove(0);
            join = true;
        }
        match self.send(prompt, None, join).await {
            Ok(()) => {
                if let Some(ask) = answering {
                    asks::done(&ask);
                    self.asks.retain(|open| open.file != ask.file);
                }
                self.input.clear();
                self.notice = None;
            }
            Err(error) => self.notice = Some(error),
        }
    }

    /// Send `prompt`: a turn of its own, or a steer of the running one (or
    /// of the one just started, `join`); staged until it lands. `origin`
    /// names what sent it when the person's words did not.
    async fn send(
        &mut self,
        prompt: String,
        origin: Option<&str>,
        join: bool,
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
                    "origin": origin}),
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

    /// `/side`: a side chat with a copy of this bot, taken where it is now,
    /// mid-turn included, and opened in a Sonny of its own on the alternate
    /// screen. The copy may call no tool, so it answers from what the bot
    /// knows; the daemon deletes it once it has been idle a quarter hour.
    /// This bot carries on, and its lines wait for the main screen.
    async fn side(&mut self, question: String) {
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
                        "prompt": question, "model": self.turn_model}),
                )
                .await
        {
            self.notice = Some(format!("the side chat did not take the question: {error}"));
        }
        let me = match std::env::current_exe() {
            Ok(me) => me,
            Err(error) => {
                self.notice = Some(error.to_string());
                return;
            }
        };
        let mut command = std::process::Command::new(me);
        command
            .arg("--store")
            .arg(&self.options.store)
            .arg("--socket")
            .arg(&self.options.socket)
            .args(["--no-sound", "--no-layout", "--bot", &name])
            .args(["--side", &forked["head"].as_i64().unwrap_or(0).to_string()]);
        if let Some(model) = &self.turn_model {
            command.args(["--model", model]);
        }
        // The key reader lets go before the side chat reads the keyboard.
        self.keys_paused
            .store(true, std::sync::atomic::Ordering::Relaxed);
        tokio::time::sleep(Duration::from_millis(80)).await;
        let _ = execute!(
            std::io::stdout(),
            DisableBracketedPaste,
            EnterAlternateScreen
        );
        let _ = disable_raw_mode();
        match command.spawn() {
            Ok(child) => self.side = Some(child),
            Err(error) => {
                self.side_back();
                self.notice = Some(format!("no side chat: {error}"));
            }
        }
    }

    /// Back from a side chat that has ended.
    fn side_closed(&mut self) {
        if let Some(child) = &mut self.side
            && !matches!(child.try_wait(), Ok(None))
        {
            self.side = None;
            self.side_back();
        }
    }

    fn side_back(&mut self) {
        let _ = enable_raw_mode();
        let _ = execute!(
            std::io::stdout(),
            LeaveAlternateScreen,
            EnableBracketedPaste
        );
        self.keys_paused
            .store(false, std::sync::atomic::Ordering::Relaxed);
        self.redraw = true;
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
        // The pane may have changed size since these lines were wrapped:
        // the viewport takes the new size first, and a line still too wide
        // is cut into rows rather than losing its end.
        if self.watch.is_some() || self.side.is_some() {
            // History waits for the main screen, its newest lines kept.
            if let Some(over) = self.pending.len().checked_sub(KEPT_UNDER_STATUS)
                && over > 0
            {
                self.pending.drain(..over);
                self.skipped += over;
            }
            return match &mut self.watch {
                Some(watch) => draw_watch(watch),
                None => Ok(()),
            };
        }
        if self.skipped > 0 {
            let note = format!(
                "… {} earlier lines went by while another screen was open",
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
        self.height = terminal.size()?.height as usize;
        let (composer_rows, (cursor_row, cursor_col)) = self.composer(width);
        // A short pane keeps the rows around the cursor, where typing goes on.
        let room = (terminal.size()?.height as usize).saturating_sub(VIEWPORT as usize - 1);
        let (composer_rows, cursor_row) = window(composer_rows, cursor_row, room.max(1));
        // The reply's open line, wrapped as it will be written: it grows
        // here row by row until its newline writes it into the history.
        let mut open_rows = self.open_rows(width);
        // Then what was sent and has not landed, kept in view below it.
        open_rows.extend(self.staged_rows(width));
        open_rows.extend(self.shell_rows(width));
        // The lead's questions stay pinned above the status row.
        open_rows.extend(asks::rows(&self.asks, width, self.chatting));
        let spare = room.saturating_sub(composer_rows.len()) + 1;
        let hidden = open_rows.len().saturating_sub(spare.max(1));
        open_rows.drain(..hidden);
        let live_rows = open_rows.len().max(1) as u16;
        fit(
            terminal,
            VIEWPORT - 2 + live_rows + composer_rows.len() as u16,
        )?;
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
                let mut linked = Vec::new();
                let cut: Vec<bool> = rows
                    .iter()
                    .map(|row| row.alignment == Some(transcript::CUT))
                    .collect();
                terminal.insert_before(rows.len() as u16, |buf| {
                    Paragraph::new(rows.to_vec()).render(buf.area, buf);
                    linked = links::cells(buf, &cut);
                })?;
                // The rows sit just above the viewport now; those still on
                // the screen get their addresses linked.
                if !linked.is_empty() {
                    let top = terminal.get_frame().area().y;
                    let above = rows.len() as u16;
                    let on_screen = linked.iter().filter_map(|(x, y, cell)| {
                        (top + y).checked_sub(above).map(|y| (*x, y, cell))
                    });
                    terminal.backend_mut().draw(on_screen)?;
                    Backend::flush(terminal.backend_mut())?;
                }
            }
        }
        terminal.draw(|frame| {
            let [live, status, _, composer, footer] = areas(frame.area(), live_rows);
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
                let [_, _, _, composer_row, _] = areas(frame.area(), live_rows);
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
            frame.render_widget(Paragraph::new(composer_rows), composer);
            frame.set_cursor_position((
                composer.x + (cursor_col as u16).min(composer.width.saturating_sub(1)),
                composer.y + (cursor_row as u16).min(composer.height.saturating_sub(1)),
            ));
            frame.render_widget(Paragraph::new(self.footer_line(width)), footer);
        })?;
        Ok(())
    }

    /// The composer's rows: the prompt wrapped to the width, the way it
    /// will read once sent, and only the rows around the cursor when it
    /// is long; or the hint. Also the cursor's row among those and its
    /// column. A list or picker open over the composer keeps it to the
    /// cursor's row.
    fn composer(&self, width: usize) -> (Vec<Line<'static>>, (usize, usize)) {
        if self.input.is_empty() {
            let hint = match self.running {
                _ if !self.asks.is_empty() && !self.chatting => "Answer, or Esc to chat",
                Some(_) => "Steer the running turn",
                None => "Ask anything",
            };
            return (
                vec![Line::from(vec![
                    Span::raw("› ").bold(),
                    Span::raw(hint).dim(),
                ])],
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
                    0 => Span::raw("› ").bold(),
                    _ => Span::raw("  "),
                };
                Line::from(vec![
                    lead,
                    Span::raw(self.input[part.start..part.end].to_owned()),
                ])
            })
            .collect();
        let keep = match self.choosing.is_some() || self.picker.is_some() {
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
        // The folder, and what it is a copy of; its branch is kept whole.
        let folder = match &self.place.copy_of {
            Some(source) => format!("{folder}, a sandfs copy of {}", tilde(source)),
            None => folder,
        };
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
    let lookout = watch.lookout.clone();
    watch.screen.autoresize()?;
    watch.screen.draw(|frame| {
        let area = frame.area();
        let width = area.width.saturating_sub(2) as usize;
        let mut lines = vec![Line::default()];
        let mut card = status::card(&project, &rows, &jobs, width);
        if !lookout.is_empty() {
            card.insert(1, Line::from(lookout.clone()).dim());
        }
        lines.extend(card.into_iter().map(|line| {
            let mut spans = vec![Span::raw(" ")];
            spans.extend(line.spans);
            Line::from(spans).style(line.style)
        }));
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

    #[test]
    fn another_projects_lead_and_its_tasks_are_never_tasks_here() {
        let lineage: std::collections::HashSet<String> = ["ws.lead".into(), "ws.fix".into()].into();
        let mut leads = std::collections::HashSet::new();
        assert_eq!(kin("ws.fix2", "ws.fix", "ws", &lineage, &leads), Kin::Task);
        assert_eq!(kin("ws.side", "", "ws", &lineage, &leads), Kin::Task);
        // Whoever made it, as `sonny project add` says no one.
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
        };
        App::new(client, "b", &json!({"name": "b"}), options)
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
        let dir = std::env::temp_dir().join(unique("sonny-asks"));
        std::fs::create_dir_all(&dir).unwrap();
        let ask = |name: &str, question: &str| {
            let file = dir.join(name);
            std::fs::write(&file, "{}").unwrap();
            asks::Ask {
                file,
                question: question.into(),
                choices: vec!["Keep it".into(), "Drop it".into()],
            }
        };
        app.asks = vec![ask("1.json", "Keep the old API?"), ask("2.json", "Tag it?")];
        app.input.set("2");
        app.submit().await;
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
        assert_eq!(app.asks.len(), 1);
        assert!(!dir.join("1.json").exists());
        assert!(dir.join("2.json").exists());
        std::fs::remove_dir_all(&dir).unwrap();
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
        assert_eq!(staged, ["! echo hello"]);
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
        assert_eq!(app.staged_rows(80).len(), 2);
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
    async fn the_footer_names_the_branch_and_what_a_copy_was_made_from() {
        let mut app = app(daemon_answering(|_| json!({})).await);
        app.workspace = "/work/tasks/fix-cart".into();
        app.model = "opus".into();
        app.place = place::Place {
            branch: Some("agent/fix-cart".into()),
            copy_of: Some("/work/shop".into()),
        };
        let wide = app.footer_line(100).to_string();
        assert!(
            wide.contains(
                "opus · /work/tasks/fix-cart, a sandfs copy of /work/shop on agent/fix-cart"
            ),
            "{wide}"
        );
        // A narrow footer cuts the folder from its start, never the branch.
        let narrow = app.footer_line(56).to_string();
        assert!(
            narrow.contains("…") && narrow.contains("/work/shop on agent/fix-cart"),
            "{narrow}"
        );
        assert_eq!(narrow.trim_end().width(), 56 - 2);
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
