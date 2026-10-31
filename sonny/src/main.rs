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
mod acp;
mod approvals;
mod approver;
mod asks;
mod attach;
mod background;
mod banner;
mod boot;
mod browser;
mod cache;
mod calls;
mod cmux;
mod commons;
mod config;
mod declared;
mod diff;
mod embed;
mod exec;
mod footer;
mod fuzzy;
mod herdr;
mod highlight;
mod hint;
mod home;
mod html;
mod image;
mod js;
mod links;
mod list;
mod login;
mod look;
mod lookout;
#[cfg(target_os = "macos")]
mod mac;
mod margin;
mod math;
mod mcp;
mod mcp_import;
mod mermaid;
mod models;
mod mux;
mod pane;
mod picker;
mod place;
mod plan;
mod project;
mod prompt;
mod reactions;
mod resolved;
mod rex;
mod said;
mod screen;
mod seed;
mod served;
mod settings;
mod setup;
mod shell;
mod skills;
mod sky;
mod snapshot;
mod sound;
mod started;
mod status;
mod suggest;
mod t3;
mod table;
mod task;
mod term;
#[cfg(test)]
mod testing;
mod tool_host;
mod transcript;
mod tree;
mod trigger;
mod upgrade;
mod usage;
mod user_shell;
mod zen;

use agent_client::{Client, Events};
use ratatui::{
    Terminal,
    backend::{Backend, CrosstermBackend},
    crossterm::{
        event::{
            self, DisableBracketedPaste, DisableFocusChange, DisableMouseCapture,
            EnableBracketedPaste, EnableFocusChange, EnableMouseCapture, Event, KeyCode, KeyEvent,
            KeyEventKind, KeyModifiers, KeyboardEnhancementFlags, PopKeyboardEnhancementFlags,
            PushKeyboardEnhancementFlags,
        },
        execute,
        terminal::{
            BeginSynchronizedUpdate, EndSynchronizedUpdate, EnterAlternateScreen,
            LeaveAlternateScreen, disable_raw_mode, enable_raw_mode,
        },
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
    path::{Path, PathBuf},
    sync::Arc,
    time::{Duration, Instant},
};
use transcript::Markdown;
use unicode_width::UnicodeWidthStr;

/// Rows kept at the bottom: the reply's unfinished line, the status, a
/// blank row, the composer between a row of its band above and below, and
/// the footer.
const VIEWPORT: u16 = 7;
/// The most rows a long prompt's composer grows to; it shows its end.
const COMPOSER_ROWS: usize = 8;
/// An agent in a folder: what a profile shown in the footer was read for.
type Seat = (String, String);
/// How many lines of a message a reply quotes; the rest is the agent's
/// own history.
const QUOTE_LINES: usize = 6;
/// How many of the bot's latest replies `/react` offers.
const REACTABLE: usize = 9;
/// How much of a reply's first line a reaction quotes.
const QUOTED: usize = 72;

/// Draws in a row that may fail before the client gives up on its
/// terminal: each failed cursor query waits up to 2 s, so about a minute.
const DRAW_ATTEMPTS: u32 = 30;
/// How much history is shown on attach.
const HISTORY_ITEMS: u64 = 200;
/// How much older history is drawn at a time as the view nears its top:
/// a page drawn while the wheel turns, so small enough not to stall it.
const OLDER_ITEMS: u64 = 50;
/// How many nodes the way up to the first turn reads at a time, cut
/// into pages of [`OLDER_ITEMS`].
const WALKED_ITEMS: u64 = 400;
/// About how many pages the way up to the first turn reads between frames.
const WALKED_PAGES: usize = 32;
/// How many pages of 400 nodes a task's history is read back to find
/// where a note the margin shows of it starts.
const REFERENCE_PAGES: usize = 25;
/// The widest the conversation is written, however wide the window: a
/// reply reads as a column, not a band across the screen. Prose reads
/// best at 45 to 90 characters a line, but replies carry code, paths and
/// tables too, which most style guides keep within 100 (rustfmt's own
/// width), so they stay whole at 100 and prose stays near the measure.
const PROSE: usize = 100;

/// The blank columns between the conversation and the line beside it,
/// matched on the pane's side, as the conversation's own rows open.
const GUTTER: u16 = 2;
/// How long a side chat's copy stays once idle.
/// What follows a guess at the next message in the composer.
const TAB: &str = "  tab";
const SIDE_EXPIRE: Duration = Duration::from_secs(15 * 60);
/// This window is a side chat (`--side`).
static SIDE: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);
/// Into the alternate screen with the mouse, which Sonny owns while it
/// runs, or out of it, the keys told apart on the screen it is on.
fn own_screen(own: bool) {
    keys_apart(false);
    let _ = match own {
        true => execute!(
            std::io::stdout(),
            EnterAlternateScreen,
            EnableMouseCapture,
            EnableFocusChange
        ),
        false => execute!(
            std::io::stdout(),
            DisableFocusChange,
            DisableMouseCapture,
            LeaveAlternateScreen
        ),
    };
    keys_apart(true);
}

/// Ask the terminal to tell apart keys it otherwise sends alike, Shift-Enter
/// from Enter among them, as Codex does: the kitty keyboard protocol, its
/// escape codes made unambiguous, without release events. A terminal that
/// does not know it ignores it. The main and alternate screens each keep
/// their own, so each push is popped on the screen it was made on.
fn keys_apart(on: bool) {
    let _ = match on {
        true => execute!(
            std::io::stdout(),
            PushKeyboardEnhancementFlags(
                KeyboardEnhancementFlags::DISAMBIGUATE_ESCAPE_CODES
                    | KeyboardEnhancementFlags::REPORT_ALTERNATE_KEYS
            )
        ),
        false => execute!(std::io::stdout(), PopKeyboardEnhancementFlags),
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
    /// Lay the project's tasks out in Rex, Herdr or cmux, whichever Sonny runs in.
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
    /// Which of the folder's leads to open, with no `--agent`.
    lead: project::Pick,
}

fn usage() -> String {
    "usage: sonny [--store PATH] [--socket PATH] [--model PROVIDER/MODEL] [--no-layout] [--rex-host HOST] [--no-sound]\n\
     \x20      [--agent NAME [--new [--workspace DIR]] | --lead NAME | --new-lead [NAME]]\n\
     With no --agent, the folder's lead you last wrote to, its first made with --model the\n\
     first time; --lead opens another of the folder's leads, --new-lead makes one. Its\n\
     tasks open beside it in Rex, Herdr or cmux, each in a git worktree of its own. Type /status\n\
     for the project's agents, /lead for the folder's leads.\n\
     Agents start tasks, put windows and panes where the person looks, make projects\n\
     and follow agents in the lookout with tools Sonny gives them, which its tool host answers.\n\
     sonny memory ... and sonny trigger ... are what ~/.agent/memory and ~/.agent/trigger\n\
     run, which Sonny writes at each start.\n\
     sonny sky draws every agent on the daemon lighting up as they talk, as /sky does,\n\
     on its own in the whole terminal; --center NAME puts an agent in the middle, and\n\
     --replay [WHEN] plays the past back from WHEN: 2h, 30m or 1d ago, or 02:10 (an hour ago).\n\
     sonny lookout watches every lead on the daemon, tells it what happens in its tasks\n\
     and answers its agents' calls of Sonny's tools; Sonny starts one.\n\
     sonny agent ARGS... runs Sonny's own `agent` with ARGS, whatever `agent` is on PATH.\n\
     sonny setup asks again for the provider and the default model, as the first run does.\n\
     sonny acp [--store PATH] [--socket PATH] speaks the Agent Client Protocol on stdio\n\
     for T3 Code: a thread is its folder's lead, and each task opens as a thread of its own."
        .into()
}

/// A `show_agent` call of `caller`'s: put `bot`'s window where the person
/// asked. A task gets a window of its own unless told otherwise; `split`
/// puts it beside the caller's block, or `beside` another bot's. Another
/// project's lead gets its project's own home instead (a Rex session, a
/// Herdr or cmux workspace), running that lead's Sonny, unless it is there already.
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
        && let Some(other) = project::lead_project(bot).filter(|p| *p != project)
    {
        let store = std::env::var_os("AGENT_STORE").map(PathBuf::from);
        let (home, opened) = open_lead(&socket, store.as_deref(), other, bot, &project, &me)?;
        return Ok(json!({"bot": bot, "in": home, "opened": opened}));
    }
    let mut layout = mux::for_agent(&socket, &project, None)?;
    let command = [
        me.to_string_lossy().into_owned(),
        "--socket".into(),
        socket.to_string_lossy().into_owned(),
        "--no-sound".into(),
        "--agent".into(),
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

/// Open `bot`, a lead of `project`, in that project's own home, made when needed in
/// the multiplexer `like`'s home is in, running Sonny on it: a session for
/// each lead, and in it a window for each of its tasks. A lead whose Sonny
/// is there already is left as it is. Returns the home, and whether it
/// was opened now.
fn open_lead(
    socket: &std::path::Path,
    store: Option<&std::path::Path>,
    project: &str,
    bot: &str,
    like: &str,
    me: &std::path::Path,
) -> Result<(String, bool), String> {
    let mut layout = mux::for_agent(socket, project, Some(like))?;
    let bot = bot.to_owned();
    let mut command = vec![me.to_string_lossy().into_owned()];
    if let Some(store) = store {
        command.extend(["--store".into(), store.to_string_lossy().into_owned()]);
    }
    command.extend([
        "--socket".into(),
        socket.to_string_lossy().into_owned(),
        "--agent".into(),
        bot.clone(),
    ]);
    let label = match bot.ends_with(".lead") {
        true => "lead".to_owned(),
        false => format!("lead {}", project::lead_short(&bot)),
    };
    let opened = layout.show(&bot, &label, &command)?;
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

/// The daemon's companion that answers an `auto` gate: `sonny approver`,
/// judged by `model` unless a judge is set, which the daemon then keeps
/// running for as long as it runs, as it does the lookout. One kept already
/// keeps the judge it was given first, so leads on other models do not
/// start it again.
async fn serve_auto(client: &Client, model: &str) -> Result<(), String> {
    let bin = std::env::current_exe().map_err(|e| e.to_string())?;
    let args = daemon_args("approver");
    let kept = client
        .request("companions", json!({}))
        .await
        .map_err(|e| e.to_string())?;
    if (kept["companions"].as_array().into_iter().flatten())
        .any(|c| c["name"] == APPROVER && c["command"] == json!(bin) && c["args"] == json!(args))
    {
        return Ok(());
    }
    keep(client, APPROVER, &bin, args, json!({"AGENT_MODEL": model})).await
}

/// Sonny's companions in the daemon, so they run for as long as it does,
/// whether or not a window is open, and come back with the next daemon:
/// the lookout, `sonny lookout`, which tells leads what happens in their
/// tasks, and the tool host, `sonny tool-host`, which answers the tools
/// Sonny gives its agents. This Sonny's own build replaces another's: the
/// build is part of what the daemon keeps, so a Sonny upgraded in place
/// has the daemon start the new ones.
async fn start_companions(client: &Client) -> Result<(), String> {
    let me = std::env::current_exe().map_err(|e| e.to_string())?;
    let build = json!({"SONNY_BUILD": lookout::identity()});
    keep(client, LOOKOUT, &me, daemon_args("lookout"), build.clone()).await?;
    keep(client, TOOL_HOST, &me, daemon_args(tool_host::NAME), build).await
}

/// How long a word waits on the daemon to say whether it names an agent.
const NAME_LOOKUP: Duration = Duration::from_secs(2);

/// What Sonny names its companions in the daemon.
const LOOKOUT: &str = "sonny-lookout";
const TOOL_HOST: &str = "sonny-tool-host";
const APPROVER: &str = "approver-auto";

/// A companion's arguments: its store and socket come from the daemon
/// that runs it (`AGENT_STORE`, `AGENT_SOCKET`), which may serve the store
/// on another socket later.
fn daemon_args(command: &str) -> Vec<String> {
    vec![command.to_owned()]
}

/// Ask the daemon to keep `name` running `bin` with `args`; the same again
/// changes nothing.
async fn keep(
    client: &Client,
    name: &str,
    bin: &Path,
    args: Vec<String>,
    env: Value,
) -> Result<(), String> {
    let add = json!({"name": name, "command": bin, "args": args, "env": env});
    client
        .request("companions", json!({"add": add}))
        .await
        .map(drop)
        .map_err(|e| format!("{name}: {e}"))
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
            setup::provider(&bin, false)?;
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

/// Whether `error` met a daemon on an older protocol than this Sonny's,
/// one `agent start` replaces.
fn older_daemon(error: &agent_client::Error) -> bool {
    error.code == "daemon_protocol_mismatch"
        && (error.facts.as_ref())
            .and_then(|facts| facts.get("protocol")?.as_u64())
            .is_some_and(|protocol| protocol < agent_client::PROTOCOL)
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
/// environment, for a start from a trigger's fire, which launchd or systemd
/// gives no provider keys.
fn start_daemon(
    store: &std::path::Path,
    socket: &std::path::Path,
    login: bool,
) -> Result<(), NotStarted> {
    let bin = agent_bin().ok_or(
        "the daemon is not running and Agent's binary was not found; install Sonny with Homebrew or its release tarball, or set AGENT_BIN".to_owned(),
    )?;
    // A trigger's fire runs as a systemd service, which takes every process
    // left in it when it ends: the daemon is started in a scope of its own,
    // as a desktop starts an app, so it outlives the fire.
    let scoped = login && cfg!(target_os = "linux") && std::env::var_os("INVOCATION_ID").is_some();
    let mut command = match scoped {
        true => {
            let mut command = std::process::Command::new("systemd-run");
            command
                .args(["--user", "--scope", "--quiet", "--collect", "--"])
                .arg(&bin);
            command
        }
        false => std::process::Command::new(&bin),
    };
    if let Some(environment) = login.then(login::environment).flatten() {
        command.env_clear().envs(environment);
    }
    // systemd-run finds the user manager by its runtime folder.
    if let Some(runtime) = std::env::var_os("XDG_RUNTIME_DIR").filter(|_| scoped) {
        command.env("XDG_RUNTIME_DIR", runtime);
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
/// The store this Sonny reaches, once [`daemon_paths`] chose it.
static STORE: std::sync::OnceLock<PathBuf> = std::sync::OnceLock::new();

/// The outputs folder a new agent's cut tool outputs go to: beside the
/// store this Sonny reaches.
pub(crate) fn outputs() -> Option<String> {
    match STORE.get() {
        Some(store) => agent_client::policy::outputs_beside(store),
        None => agent_client::policy::default_outputs(),
    }
}

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
    let _ = STORE.set(store.clone());
    let socket = match socket.or_else(|| std::env::var_os("AGENT_SOCKET").map(PathBuf::from)) {
        Some(socket) => socket,
        None => agent_client::socket::default_socket(&store).map_err(|e| e.to_string())?,
    };
    Ok((store, socket))
}

fn parse() -> Result<Options, String> {
    let args = std::env::args().skip(1);
    let (mut store, mut socket, mut bot, mut new, mut model, mut workspace) =
        (None, None, None, false, None, None);
    let (mut layout, mut rex_host, mut sound) = (true, None, true);
    let (mut side, mut embedded) = (None, false);
    let mut lead = project::Pick::Last;
    let mut args = args.peekable();
    while let Some(arg) = args.next() {
        if arg == "--new-lead" {
            let name = args.next_if(|next| !next.starts_with("--"));
            lead = project::Pick::New(name);
            continue;
        }
        let mut value = || args.next().ok_or_else(usage);
        match arg.as_str() {
            "--store" => store = Some(PathBuf::from(value()?)),
            "--socket" => socket = Some(PathBuf::from(value()?)),
            "--agent" => bot = Some(value()?),
            "--new" => new = true,
            "--model" => model = Some(value()?),
            "--workspace" => workspace = Some(value()?),
            "--no-layout" => layout = false,
            "--rex-host" => rex_host = Some(value()?),
            "--no-sound" => sound = false,
            "--lead" => lead = project::Pick::Named(value()?),
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
        lead,
    })
}

/// What runs this Sonny again on `bot`, from the same folder: `sonny`
/// with its own arguments, less those that made the bot or changed a
/// setting, naming `bot` when the folder would not open it on its own, and
/// with the daemon the environment named.
fn reopen(bot: &str) -> Vec<String> {
    let mut command = vec!["sonny".to_owned()];
    let mut args = std::env::args().skip(1).peekable();
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--new" => {}
            "--workspace" | "--lead" | "--agent" => {
                args.next();
            }
            "--new-lead" => {
                args.next_if(|next| !next.starts_with("--"));
            }
            _ => command.push(arg),
        }
    }
    let named = std::env::args().any(|arg| arg == "--agent");
    // A lead by its name, the first one too, so a pane restored later
    // opens the same lead, whichever the folder opens by then.
    match project::lead_project(bot) {
        Some(_) if !named => command.extend(["--lead".into(), project::lead_short(bot).into()]),
        _ => command.extend(["--agent".into(), bot.to_owned()]),
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
        // The tool host runs this for each call of a tool Sonny answers.
        Some((tool, rest)) if tool == "tool" => served::answer_here(rest),
        // `~/.agent/memory` runs this, from agents' shells.
        Some((verb, rest)) if verb == "memory" => {
            // The person's words are read from the daemon, which only
            // Sonny reaches; the rest is memory's own.
            match rest.split_first().map(|(v, r)| (v.as_str(), r)) {
                Some(("said", since)) => {
                    return std::process::ExitCode::from(said::cli(since) as u8);
                }
                // The person's words count as a change, so a night they
                // wrote is cleaned up whether or not an agent saved.
                Some(("changed", [])) => {
                    return std::process::ExitCode::from(said::changed_cli() as u8);
                }
                _ => {}
            }
            return std::process::ExitCode::from(agent_client::memory::cli(rest) as u8);
        }
        Some((verb, rest)) if verb == trigger::VERB => {
            return std::process::ExitCode::from(trigger::cli(rest) as u8);
        }
        // launchd or systemd runs this for a trigger's fire.
        Some((flag, rest)) if flag == trigger::FIRE_FLAG => {
            return std::process::ExitCode::from(trigger::fire_cli(rest) as u8);
        }
        Some((lookout, rest)) if lookout == "lookout" => lookout::run(rest),
        Some((verb, rest)) if verb == tool_host::NAME => tool_host::run(rest),
        Some((verb, rest)) if verb == "approver" => approver::run(rest),
        Some((verb, rest)) if verb == "mcp" => mcp::cli(rest),
        Some((verb, rest)) if verb == "sky" => sky_cli(rest),
        // An agent's shell runs this, for zen it was asked for in words.
        Some((verb, rest)) if verb == "zen" => zen::cli(rest),
        Some((verb, rest)) if verb == "home" => {
            return std::process::ExitCode::from(home::cli(rest) as u8);
        }
        // Unlisted: the job `sonny.start_at_boot` keeps.
        Some((verb, [])) if verb == "boot" => {
            return std::process::ExitCode::from(boot::run() as u8);
        }
        Some((verb, rest))
            if verb == "setup" && rest.first().is_some_and(|step| step == "backup") =>
        {
            match &rest[1..] {
                [] => home::Places::home().and_then(|places| home::set_up(&places, None)),
                [url] => home::Places::home().and_then(|places| home::set_up(&places, Some(url))),
                _ => Err(format!("usage: {}", home::USAGE)),
            }
        }
        Some((verb, rest)) if verb == "setup" => tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .map_err(|e| e.to_string())
            .and_then(|runtime| {
                runtime.block_on(async {
                    let (store, socket) = daemon_paths(None, None)?;
                    match rest.is_empty() {
                        true => setup::again(&store, &socket).await,
                        false => setup::flags(rest, &socket).await,
                    }
                })
            }),
        Some((agent, rest)) if agent == "agent" => pass_to_agent(rest),
        // T3 Code runs this as a provider, a process for each thread.
        Some((verb, rest)) if verb == acp::VERB => acp::run(rest),
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

/// `sonny sky [--center NAME] [--replay [WHEN]] [--store PATH] [--socket
/// PATH]`: `/sky` on its own, the whole terminal, until q; centred on an
/// agent, by its name, its label or its project, when asked; playing the
/// past back from WHEN, an hour ago when not said.
fn sky_cli(args: &[String]) -> Result<(), String> {
    const USAGE: &str =
        "usage: sonny sky [--center NAME] [--replay [WHEN]] [--store PATH] [--socket PATH]";
    let (mut store, mut socket, mut centre, mut replay) = (None, None, None, None);
    let mut args = args.iter().peekable();
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--store" => store = args.next().map(PathBuf::from),
            "--socket" => socket = args.next().map(PathBuf::from),
            "--center" | "--centre" => centre = args.next().cloned(),
            "--replay" => {
                let now = std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .map_or(0, |d| d.as_millis() as i64);
                let when = args.next_if(|next| !next.starts_with("--"));
                replay = Some(match when {
                    Some(when) => sky::replay_from(when, now).ok_or_else(|| {
                        format!("--replay needs a time such as 2h, 30m, 1d or 02:10, not {when}")
                    })?,
                    None => now - 3_600_000,
                });
            }
            _ => return Err(USAGE.into()),
        }
    }
    let (_, socket) = daemon_paths(store, socket)?;
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(1)
        .enable_all()
        .build()
        .map_err(|e| e.to_string())?;
    sky::watch(&runtime, socket, centre, replay)
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
    home::at_start();
    boot::at_start();
    // Fonts are read before the first picture needs them.
    std::thread::spawn(image::preload);
    // So are the syntaxes, before the first code is colored.
    std::thread::spawn(highlight::preload);
    let (client, mut events) = match Client::connect(&options.socket).await {
        Ok(connected) => connected,
        Err(error) => {
            // Starting replaces an older daemon once its running turns end,
            // which can take a while; the screen is not up yet to say so.
            if older_daemon(&error) {
                eprintln!(
                    "sonny: the running daemon is older than this Sonny; it stops once its running turns end (at most 2 minutes), then this one starts"
                );
            }
            connect(&options).await?
        }
    };
    trigger::restamp_at_start(client.identity(), client.store());
    // A lead made just now starts in the row new leads start in.
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
            (bot.clone(), project::lead_project(bot).map(str::to_owned))
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
            let lead =
                project::open(&client, &dir, options.model.as_deref(), &options.lead).await?;
            fresh = lead.new;
            moving = lead.moving;
            above = lead.above;
            (lead.bot, Some(lead.project))
        }
    };
    // A side chat borrows its agent's pane, and Herdr keeps the agent; a
    // task in the pane is in its lead's.
    if options.side.is_none() && !options.embedded {
        herdr::announce(&name, reopen(&name));
        cmux::announce(&name, reopen(&name));
    }
    // Everything after now arrives live; history is read from the bot's
    // items instead of replayed events, so the replay is skipped. The bot
    // is read after that, so a turn that ended during the replay is seen
    // as ended.
    client
        .request("follow", json!({"bot": name, "after": 0}))
        .await
        .map_err(|e| e.to_string())?;
    let caught = caught_up(&mut events).await;
    let bot = client
        .request("inspect", json!({"bot": name}))
        .await
        .map_err(|e| e.to_string())?;

    enable_raw_mode().map_err(|e| e.to_string())?;
    term::detect();
    let _ = execute!(std::io::stdout(), EnableBracketedPaste);
    keys_apart(true);
    own_screen(true);
    let mut terminal =
        Terminal::new(CrosstermBackend::new(std::io::stdout())).map_err(|e| e.to_string())?;
    // A side chat opens on its window's alternate screen, still drawn on.
    let _ = terminal.clear();
    let mut app = App::new(client, &name, &bot, options.clone());
    app.context = caught.context;
    app.window = caught.window;
    app.paused = caught.paused(&app.client);
    app.turn_began().await;
    if options.side.is_some() {
        let source = name.rsplit_once(".side-").map_or(name.as_str(), |(s, _)| s);
        // One allowed no tool answers; one with its source's acts.
        let acts = !bot["allowed"]
            .as_array()
            .is_some_and(|tools| tools.is_empty());
        app.notice = Some(match acts {
            true => format!("a copy of {source} that can do anything it can; {source} carries on"),
            false => format!("a copy of {source} that answers without acting; {source} carries on"),
        });
    }
    // The person's agent can react to them; a side chat has its agent's
    // tools, or none.
    if options.side.is_none() {
        app.role = match project {
            Some(_) => served::Role::of(0),
            None => declared::role(&app.client, &bot).await,
        };
        app.declare = declared::declare(&bot, app.role);
    }
    app.sounds = sound::Sounds::new(options.sound);
    let settings = settings::Settings::load();
    app.sounds.chime = settings.chime;
    app.verbose = settings.verbose;
    app.zen = zen::focus(app.reactions_key.as_deref());
    app.margin_opens = settings.margin_opens;
    // A bot keeps the model it was made with; --model runs this window's
    // turns on another.
    if let Some(model) = options.model.as_ref().filter(|m| **m != app.model) {
        app.model = model.clone();
        app.turn_model = Some(model.clone());
    }
    // Tasks are laid out in the multiplexer the person runs Sonny in (or
    // the Rex server named); elsewhere they run out of sight, in /status.
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
            "a new lead for this folder; {above} leads the folder above: sonny --agent {above}"
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
    // They answer the tools Sonny gives every agent, a lead or not.
    if let Err(error) = start_companions(&app.client).await {
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
            &approvals::sandbox(mode, std::path::Path::new(&app.workspace)),
        )
    {
        app.gating = Some(mode);
    }
    // A bot made with no prompt-cache lifetime takes Sonny's, so an idle gap
    // of over five minutes no longer rereads it all; a chosen one stays.
    if options.side.is_none() {
        app.cache = cache_to_take(&bot);
    }
    app.project = project;
    // Sonny's welcome heads a lead's conversation, though not a side chat.
    app.welcome = options.side.is_none();
    // The first lead opened since an upgrade tells the person what changed
    // and does what the upgrade asks; a task works in its own worktree and
    // sandbox, and a side chat leaves it to its agent.
    if options.side.is_none() && project::is_lead(&name) {
        app.upgrades = skills::upgraded();
    }
    if let Some(set) = setup::just_set() {
        app.notice = Some(set);
    }
    if fresh {
        app.gating = Some(approvals::default());
    } else if approvals::reviews(app.mode)
        && let Err(error) = serve_auto(&app.client, &app.model).await
    {
        app.notice = Some(error);
    }
    let result = app.main(&mut terminal, &mut events).await;
    let _ = terminal.clear();
    own_screen(false);
    keys_apart(false);
    let _ = execute!(std::io::stdout(), DisableBracketedPaste);
    let _ = disable_raw_mode();
    status_report("clear", &name);
    // `/lead NAME`: this window becomes that lead's Sonny, as if opened on it.
    if let (Ok(()), Some(other)) = (&result, app.switch_to.take()) {
        use std::os::unix::process::CommandExt;
        let me = std::env::current_exe().map_err(|e| e.to_string())?;
        // Named by --agent, whatever folder this window is in.
        let mut args = reopen(&other).split_off(1);
        if let Some(at) = args.iter().position(|arg| arg == "--lead") {
            args.drain(at..at + 2);
        }
        if !args.iter().any(|arg| arg == "--agent") {
            args.extend(["--agent".into(), other.clone()]);
        }
        let error = std::process::Command::new(me).args(&args).exec();
        return Err(format!("could not open {other}: {error}"));
    }
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
    let info = agent_client::project::read(&workspace)?;
    let instructions = project::as_task(&workspace, &info, &composed.text)?;
    let mut asked = json!({"bot": bot, "workspace": workspace, "model": model,
        "instructions": instructions, "request_id": unique("tui-create")});
    made(client, &mut asked, &model, &workspace)?;
    client
        .request("create", asked)
        .await
        .map_err(|e| e.to_string())?;
    Ok(())
}

/// What every agent Sonny makes starts with, whoever asks for it: the
/// person's `--new` or a trigger's `--start`. Its tools and Sonny's, gated
/// and sandboxed as the default row confines them, and the
/// person's settings for its model. One way in, so no kind of agent gets
/// another policy by being made elsewhere (#404).
pub(crate) fn made(
    client: &Client,
    asked: &mut Value,
    model: &str,
    workspace: &Path,
) -> Result<(), String> {
    let config = agent_client::config::load()?;
    let declare = json!(declared::tools(served::Role::of(1)));
    let tools = agent_client::default_tools(client.providers(), model, config.web_search);
    asked["tools"] = json!(tools);
    asked["declare"] = declare.clone();
    asked["compaction_instructions"] = json!(agent_client::policy::DEFAULT_COMPACTION_INSTRUCTIONS);
    // Confined from the start, as the row it is made in confines it.
    let mode = approvals::default();
    let mut settings = approvals::sandbox(mode, workspace);
    if let Some(ttl) = agent_client::policy::cache_ttl(model) {
        settings.insert("cache_ttl".into(), json!(ttl));
    }
    if let Some(outputs) = outputs() {
        settings.insert("outputs".into(), json!(outputs));
    }
    if config.hide_senders {
        settings.insert("hide_senders".into(), json!(true));
    }
    asked["settings"] = Value::Object(settings);
    if let Some(fallback) = config.fallbacks.get(model) {
        asked["fallback"] = json!(fallback);
    }
    let tools: Vec<String> = tools.into_iter().map(String::from).collect();
    let tools = approvals::gateable(&tools, &declare);
    if let Value::Object(fields) = asked {
        fields.extend(approvals::gate(mode, &tools));
    }
    Ok(())
}

/// The prompt-cache lifetime Sonny gives the model of `bot`, as `inspect`
/// gives it, when the bot has none set. One someone chose, five minutes
/// included, stays.
fn cache_to_take(bot: &Value) -> Option<&'static str> {
    let model = format!(
        "{}/{}",
        bot["provider"].as_str().unwrap_or_default(),
        bot["model"].as_str().unwrap_or_default()
    );
    agent_client::policy::cache_ttl(&model).filter(|_| bot["settings"]["cache_ttl"].is_null())
}

/// A link, file or page, opened as the system opens it, by the first of
/// the openers the `open` crate knows that is installed; with none, the
/// link goes on the clipboard instead, as a login that can't open a
/// browser prints its URL. Over SSH with no display to open on and no
/// `BROWSER`, the opener would start on the remote machine's screen, so
/// the link is copied straight away, as `gh` and other CLIs print it.
fn open(target: &str) -> Result<(), String> {
    if remote_without_display(&|key| std::env::var_os(key)) {
        term::copy(target);
        return Err(format!("over SSH, nothing opens here; copied {target}"));
    }
    open::that_detached(target).map_err(|error| {
        term::copy(target);
        format!("couldn't open it ({error}); copied {target}")
    })
}

/// In an SSH session with no X or Wayland display (forwarded or local)
/// and no `BROWSER` of the person's to open with.
fn remote_without_display(var: &dyn Fn(&str) -> Option<std::ffi::OsString>) -> bool {
    let set = |key: &str| var(key).is_some_and(|value| !value.is_empty());
    (set("SSH_CONNECTION") || set("SSH_TTY"))
        && !["DISPLAY", "WAYLAND_DISPLAY", "BROWSER"]
            .iter()
            .any(|key| set(key))
}

/// The composer's edge: the band the person's messages sit on, from the
/// row above it to the row below, the conversation's width, as Codex
/// draws its composer and the user's messages on one shade: what was
/// written and what is being written read as one thing, the words in the
/// middle of it, and the composer's limits show in any theme. Without
/// colour (`NO_COLOR`) a muted rule above it.
fn edge(frame: &mut ratatui::Frame, gap: Rect, composer: Rect, pad: Rect) {
    let width = composer.width.min(PROSE as u16);
    if look::no_color() {
        let rule = Line::from("─".repeat(width as usize)).dim();
        frame.render_widget(Paragraph::new(rule), Rect { width, ..gap });
        return;
    }
    let band = Rect {
        y: gap.y,
        height: pad.bottom().saturating_sub(gap.y),
        width,
        ..composer
    };
    frame
        .buffer_mut()
        .set_style(band, Style::default().bg(look::band()));
}

/// The viewport's rows, top to bottom: the reply's open line (`live`
/// rows, wrapped), the status, a blank row that keeps the status off the
/// band, as one blank row parts any two things, a row of the composer's
/// band, the composer (as many rows as are left), its band's row under
/// it, and the footer.
fn areas(area: ratatui::layout::Rect, live: u16) -> [ratatui::layout::Rect; 6] {
    let [live, status, _, gap, composer, pad, footer] = Layout::vertical([
        Constraint::Length(live.max(1)),
        Constraint::Length(1),
        Constraint::Length(1),
        Constraint::Length(1),
        Constraint::Min(1),
        Constraint::Length(1),
        Constraint::Length(1),
    ])
    .areas(area);
    [live, status, gap, composer, pad, footer]
}

/// At most `keep` of `rows`, as near the end as still shows `row`; and
/// where `row` is among them.
fn window<T>(mut rows: Vec<T>, row: usize, keep: usize) -> (Vec<T>, usize) {
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

/// The agent a side chat is a copy of, from the name `/side` gives it,
/// `SOURCE.side-HEX`; `None` for any other bot. A name alone can be a
/// task's, so `side_copy` checks the record too.
pub fn side_source(bot: &str) -> Option<&str> {
    let (source, stamp) = bot.rsplit_once(".side-")?;
    let hex = |b: u8| b.is_ascii_digit() || (b'a'..=b'f').contains(&b);
    (!source.is_empty() && (1..=6).contains(&stamp.len()) && stamp.bytes().all(hex))
        .then_some(source)
}

/// The agent `record`, a bot as `bots` lists it, is a side chat of: named
/// as `/side` names one, made by that agent, and expiring as a side chat
/// does. `None` for any other bot, a task named `NAME.side-feed` included.
pub fn side_copy(record: &Value) -> Option<&str> {
    let source = side_source(record["name"].as_str()?)?;
    (record["created_by"] == source && record["expire_ms"].is_u64()).then_some(source)
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
async fn caught_up(events: &mut Events) -> Caught {
    let mut caught = Caught::default();
    let replay = async {
        while let Some(event) = events.recv().await {
            if event["event"] == "follow_live" {
                return;
            }
            if let Some(size) = context_after(&event) {
                caught.context = Some(size);
                caught.window = window_of(&event).or(caught.window);
            }
            // A pause or resume since the greeting, which no replay brings
            // again.
            match event["event"].as_str() {
                Some("paused") => caught.pause = Some(event["pause"].clone()),
                Some("resumed") => caught.pause = Some(Value::Null),
                _ => {}
            }
        }
    };
    let _ = tokio::time::timeout(Duration::from_secs(30), replay).await;
    caught
}

/// What a follow's replay brought before it went live: the context after
/// its last round and the model's window, and the pause as a pause or
/// resume meanwhile left it.
#[derive(Default)]
struct Caught {
    context: Option<u64>,
    window: Option<u64>,
    pause: Option<Value>,
}

impl Caught {
    /// The pause now: as the greeting said, unless one came since.
    fn paused(&self, client: &Client) -> Option<Value> {
        let pause = self.pause.as_ref().unwrap_or(client.paused());
        Some(pause.clone()).filter(|pause| pause.is_object())
    }
}

/// The model's context window a `usage` event says, when the daemon knew
/// it.
fn window_of(event: &Value) -> Option<u64> {
    event["data"]["context_window"]
        .as_u64()
        .filter(|tokens| *tokens > 0)
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

/// What a page of older items was drawn from: its nodes, the turn each
/// was made in, what newer pages' calls returned that it has the calls
/// of (the nodes to read, and the calls), and whether it is the first
/// page, the welcome over it.
struct Paged {
    nodes: Vec<i64>,
    turns: HashMap<i64, i64>,
    after: Vec<i64>,
    returned: std::collections::HashSet<String>,
    first: bool,
}

impl Paged {
    fn new(nodes: Vec<i64>, turns: HashMap<i64, i64>, after: &[(i64, Value)], first: bool) -> Self {
        let mut read = Vec::new();
        for (node, _) in after {
            if read.last() != Some(node) {
                read.push(*node);
            }
        }
        let returned = (after.iter())
            .map(|(_, item)| item["call_id"].as_str().unwrap_or("").to_owned())
            .collect();
        Self {
            nodes,
            turns,
            after: read,
            returned,
            first,
        }
    }
}

/// The reaction a call to react makes in this window, not another
/// agent's.
fn own_reaction(item: &Value) -> Option<(String, bool)> {
    (item["arguments"].as_str())
        .and_then(|a| serde_json::from_str::<Value>(a).ok())
        .filter(|arguments| reactions::target(arguments).is_none())
        .as_ref()
        .and_then(reactions::call)
}

/// What `items` give back undrawn, as drawing them as older items would:
/// what the calls before them returned.
fn unpaired(items: impl IntoIterator<Item = (i64, Value)>) -> Vec<(i64, Value)> {
    let mut calls = std::collections::HashSet::new();
    let mut unpaired = Vec::new();
    for (node, item) in items {
        let id = item["call_id"].as_str().unwrap_or("").to_owned();
        match item["type"].as_str() {
            Some("function_call")
                if item["name"] != reactions::TOOL || own_reaction(&item).is_some() =>
            {
                calls.insert(id);
            }
            Some("function_call_output") if !calls.remove(&id) => unpaired.push((node, item)),
            _ => {}
        }
    }
    unpaired
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
    /// Whether `from` names a person, whom no agents' colour marks.
    person: bool,
    /// The whole message while it is open, read again from the history
    /// when it opens, so a long conversation keeps none of them.
    text: Option<String>,
}

/// Messages between agents that came one after another, with nothing of
/// the agent's own words between them, drawn as one row that counts them
/// and a click opens to list them; one alone is drawn as itself.
struct Cluster {
    messages: Vec<Clustered>,
    open: bool,
}

/// A message in a [`Cluster`]: its node, what it is, and the calls the
/// agent made about it while it said nothing, shown under it once open.
struct Clustered {
    node: i64,
    kind: &'static str,
    /// Whom it is from or about, which the row gathering it names.
    names: Vec<String>,
    calls: Vec<Vec<Line<'static>>>,
}

/// The efforts Alt+T steps through for `model` (`provider/name`), whose
/// own is `effort`: `xhigh` or else `high`, then `max`, as the provider's
/// family takes them, each only when above `effort`.
fn boosts(providers: &Value, model: &str, effort: Option<&str>) -> Vec<String> {
    let provider = model
        .split_once('/')
        .map_or(model, |(provider, _)| provider);
    let takes: Vec<&str> = (providers[provider]["efforts"].as_array())
        .into_iter()
        .flatten()
        .filter_map(Value::as_str)
        .collect();
    let rank = |level: &str| takes.iter().position(|l| *l == level);
    let above = |level: &&str| match effort.and_then(rank) {
        Some(own) => rank(level).is_some_and(|at| at > own),
        None => true,
    };
    let first = ["xhigh", "high"].into_iter().find(|l| takes.contains(l));
    let most = Some("max").filter(|l| takes.contains(l));
    [first, most]
        .into_iter()
        .flatten()
        .filter(above)
        .map(str::to_owned)
        .collect()
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

/// The most nodes the daemon's `history_items` reads in one call.
const HISTORY_NODES: usize = 400;

/// The most denials `/retry` offers.
const DENIALS: usize = 8;

/// What the person sends to approve one retry of a denied call: their own
/// words, so the judge reads the consent, as Codex words it.
fn approval(denial: &Denial) -> String {
    format!(
        "I approve one retry of the action the reviewer denied: {} {}\n\n\
         Treat this as approval to perform that exact action in the same context in which it \
         was originally requested. Do not assume this also authorizes similar operations with \
         different payloads.",
        denial.name, denial.arguments
    )
}

/// The most rows of a denied call `/retry` writes out for the person to
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
    /// The effort it runs at, when the daemon said: a boost is this turn's
    /// alone, so the bot's own stays in `App::effort`.
    effort: Option<String>,
}

/// The lists of a setting's values `/permissions` and `/fast` open over
/// the composer.
#[derive(Clone, Copy, PartialEq)]
enum Choice {
    Permissions,
    Speed,
}

/// The speeds `/fast` lists, each where the provider serves it.
const SPEEDS: [(&str, &str); 3] = [
    ("standard", "the model's own speed and price"),
    ("fast", "faster, at a higher price per token"),
    ("ultrafast", "fastest, at a higher price still"),
];

/// The tier faster than standard a bot's record says it is served at.
fn speed(record: &Value) -> Option<String> {
    (record["settings"]["speed"].as_str())
        .filter(|speed| *speed != "standard")
        .map(str::to_owned)
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
/// `/agents` and `/ps` also open `/status`, `/triggers` and `/lookout`
/// open `/background`, and `/settings` opens `/config`, unlisted.
const COMMANDS: [(&str, &str); 28] = [
    ("/side", "Start a side chat with a copy of this agent"),
    ("/model", "Pick the model and effort"),
    (
        "/fast",
        "Choose how fast this agent is served, and at what price",
    ),
    (
        "/compact",
        "Summarize the lead's conversation before its next call",
    ),
    (
        "/permissions",
        "Choose what agents may do, and who approves the rest",
    ),
    ("/retry", "Allow one retry of a call the reviewer denied"),
    (
        "/instructions",
        "Reload the lead's instructions from its folder",
    ),
    ("/status", "Show this project's agents"),
    ("/lead", "Open another of the folder's leads, or make one"),
    (
        "/switch",
        "Open any agent in this window, or the one beside it",
    ),
    (
        "/goal",
        "Ask the agent to work until something is done; show or clear goals",
    ),
    (
        "/zen",
        "Talk with the agent about one thing, the rest held back; /zen again ends it",
    ),
    (
        "/margin",
        "Show or close the agent's notes beside the conversation",
    ),
    ("/background", "Run or remove automations and follows"),
    ("/commons", "Read what the agents of every project share"),
    ("/sky", "Watch every agent light up as they talk"),
    ("/usage", "Show the tokens the agents spent, over time"),
    ("/pause", "Stop every agent until /resume, saying why"),
    ("/resume", "Let the agents go on after a pause"),
    ("/mcp", "List the MCP servers agents' scripts call"),
    ("/config", "Change the settings in ~/.agent/config.toml"),
    ("/sound", "Turn the chime when work ends on or off"),
    (
        "/verbose",
        "Show every tool call, or only what moves the work on",
    ),
    ("/react", "React to one of the agent's replies"),
    ("/reply", "Answer one of the agent's replies, quoted"),
    ("/copy", "Copy the last reply as Markdown"),
    ("/open", "Open the last HTML page in your browser"),
    ("/quit", "Quit; the agents keep working"),
];

/// The slash menu's keys, while it is open: Tab completes the pick's name
/// until it is whole.
fn menu_keys(menu: &[(&str, &str, Vec<usize>)], pick: usize, typed: &str) -> Option<&'static str> {
    let picked = menu.get(pick.min(menu.len().checked_sub(1)?))?.0;
    Some(match typed == picked {
        true => "  ↑↓ pick · enter run · esc close",
        false => "  ↑↓ pick · tab complete · enter run · esc close",
    })
}

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
    /// `/usage` holds it instead.
    usage: Option<usage::View>,
    /// `/commons` holds it instead.
    commons: Option<commons::View>,
    /// `/sky` holds it instead.
    sky: Option<sky::View>,
    /// The row of the picked item (`/background`'s, `/commons`'s or
    /// `/status`'s) as the pane last drew it, so a new pick scrolls to it
    /// and scrolling otherwise stays put.
    picked: Option<usize>,
    /// The agent `/status` has picked from the keys, by its place among
    /// the agents it lists: ↑ and ↓ move it, Enter opens it.
    pick: Option<usize>,
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
    /// Boost: the effort the next message that starts a turn runs at,
    /// that turn's alone (Alt+T).
    boost: Option<String>,
    /// The tier faster than standard its calls are served at, if any.
    speed: Option<String>,
    /// How big the conversation was after its last model round, as the
    /// next round reads it, once known.
    context: Option<u64>,
    /// The model's context window, as the daemon says it with each round.
    window: Option<u64>,
    input: prompt::Prompt,
    /// Lines waiting to be written into the conversation, and what marks
    /// the rows of some, by their place among them.
    pending: Vec<Line<'static>>,
    marks: Vec<(usize, screen::Mark)>,
    any_history: bool,
    /// Sonny's welcome heads the transcript once it is drawn back to the
    /// conversation's first item, as Claude Code's does.
    welcome: bool,
    /// Sonny's blinks and glances in the welcome, until the person types
    /// or scrolls.
    eyes: Option<banner::Eyes>,
    width: usize,
    /// The terminal's rows, which bound how much of the bot's words is held.
    height: usize,
    live: Option<Live>,
    /// Whether the reply now ending arrived as deltas.
    streamed: bool,
    /// The item of the turn's output now streaming: a later one is the
    /// next message, drawn apart.
    live_item: Option<u64>,
    /// Items whose words were written when the next began, oldest first,
    /// until their messages name their nodes.
    ahead: std::collections::VecDeque<u64>,
    running: Option<Running>,
    calls: HashMap<String, (String, Value)>,
    /// Calls waiting on the person, as `approvals` lists them.
    approvals: Vec<Value>,
    /// The lead's open questions for the person, oldest first: its `ask`
    /// calls not yet answered, nor set aside.
    asks: Vec<asks::Ask>,
    /// Which of them what is typed answers, by id, so it stays on that
    /// question as others close or the conversation is drawn again; the
    /// oldest when none was picked or the one picked is gone.
    focus: Option<String>,
    /// Questions the conversation went on past, the person writing a
    /// message of their own after them: out of the composer and the
    /// count, their options still answering them.
    aside: Vec<asks::Ask>,
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
    /// The pause that holds every agent, if one does, as the daemon told.
    paused: Option<Value>,
    quit: bool,
    /// Another lead of the folder, which this window opens on instead once
    /// it quits: `/lead NAME`.
    switch_to: Option<String>,
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
    /// The staged turns sent with Tab to run after the running one, which
    /// they do not join.
    queued: Vec<i64>,
    /// Pictures given with the message being written, each behind its
    /// placeholder in the composer: [`attach::placeholder`] of its place
    /// here, from 1.
    /// Pictures attached to the draft, by their placeholders' numbers; one
    /// neither the draft nor Ctrl-Y can bring back is let go.
    pictures: Vec<Option<attach::Picture>>,
    /// The person's reactions to this bot's replies.
    reactions: reactions::Reactions,
    /// The slash commands the person ran, for the menu to rank.
    picks: fuzzy::Picks,
    /// `/compact` asks before it compacts: Enter says yes.
    confirming: bool,
    /// The bot's latest replies, by node, with their first lines: what
    /// `/react` offers, newest last.
    replies: Vec<(i64, String)>,
    /// `/react`'s list is open, at this reply from the newest.
    reacting: Option<usize>,
    /// The list is `/reply`'s: the pick is quoted, not reacted to.
    quoting: bool,
    /// What Ctrl+F looks for on the full screen, while it does.
    finding: Option<String>,
    /// The reply `/react` picked, which the next emoji sent answers.
    react_to: Option<(i64, String)>,
    /// The turn of the last reaction sent, whose attaching says nothing.
    reaction_turn: Option<i64>,
    /// Messages attached to the bot's next call, by their turns, until a
    /// call takes them in: the footer's clip.
    attached: Vec<i64>,
    /// The profile the agent runs as when not its seat's own, for the
    /// agent and folder it was read for.
    profile: std::cell::RefCell<Option<(Seat, Option<String>)>>,
    /// The tools to declare on the bot so it can react, sent once no turn
    /// runs.
    declare: Option<Value>,
    /// What its tools let it do, by where it stands in its lead's line.
    role: served::Role,
    /// The bot's reactions in the running turn so far, drawn when it ends.
    bot_reactions: Vec<String>,
    /// The bot's `react` calls not yet completed, by call id.
    react_calls: HashMap<String, (String, bool)>,
    /// What other agents, such as its lead, reacted with to the person's
    /// messages here, and when that was last looked at.
    picked: reactions::Picked,
    picked_read: Instant,
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
    /// What the terminal shows may not be the frame last drawn, so the
    /// next draw writes every cell again.
    stale: bool,
    /// The oldest node this window drew, which a repaint draws back to.
    first_node: Option<i64>,
    /// The node before it, while the bot has older ones.
    older: Option<i64>,
    /// What calls older than the first node drawn returned, from the top
    /// of the oldest page: drawn with the older page that has the calls.
    unpaired: Vec<(i64, Value)>,
    /// What each page of older items drawn above was drawn from, by its
    /// id on the screen, to draw it again once let go.
    pages: HashMap<u64, Paged>,
    /// The id the next page takes.
    next_page: u64,
    /// A read of the bot's items came back short: what was drawn from it
    /// is not all there was.
    short_read: std::cell::Cell<bool>,
    /// Home, with nothing typed, goes on up to the first turn as the pages
    /// over it are drawn, until a key or the wheel says otherwise.
    to_first: bool,
    /// How many rows the pages drawn took, and for how many items: how
    /// tall a page not drawn yet is guessed to be.
    page_rows: (usize, usize),
    /// Ctrl+F steps to the next place, older or newer, once the pages let
    /// go before it are drawn again: those looked through so far.
    find_step: Option<(bool, Vec<u64>)>,
    /// The window's width, the pane's included.
    full_width: u16,
    /// The pane beside the conversation on the full screen, with what the
    /// person opened to look at; whether keys go to it; and whether the
    /// mouse button went down in it, so a drag stays in it.
    pane: Option<pane::Pane>,
    pane_keys: bool,
    pane_mouse: bool,
    /// How many notes the agent had put up or changed when the margin was
    /// last looked at, and whether such a note opens it.
    margin_posted: u64,
    margin_opens: agent_client::config::MarginOpens,
    /// The pane closed last, with its `/status` or `/background`, which
    /// Ctrl+] opens again.
    closed: Option<(pane::Pane, Option<Watch>)>,
    /// Where the composer's text was last drawn on the full screen: its
    /// area, the first of its rows shown there, and the width it was laid
    /// out at; none while something else is drawn over it.
    composer_at: Option<(Rect, usize, usize)>,
    /// Where the footer counts the agents at work: its row and columns.
    counter_at: Vec<(u16, std::ops::Range<u16>, &'static str)>,
    /// The mouse button went down on the composer's text, on what: a
    /// spot, word or line a drag selects from, and where the text was
    /// drawn then, kept while the button is down so a redraw that scrolls
    /// the composer does not move the spot under the mouse. And the last
    /// press, to count clicks.
    composing: Option<(std::ops::Range<usize>, (Rect, usize, usize))>,
    composer_click: Option<(Instant, u16, u16, u8)>,
    /// The agent's notes beside the conversation; the margin's pane while
    /// something opened is laid over it; a note to scroll the margin to
    /// once it is drawn, one just put there or clicked in the conversation.
    margin: margin::Margin,
    under: Option<pane::Pane>,
    margin_focus: Option<u64>,
    /// Notes the person took away here whose message has not landed yet:
    /// it takes away nothing more once it does.
    closing: std::collections::HashSet<String>,
    /// Notes drawn in the conversation where they were put, while the
    /// margin is closed or the window has no room for it: by pin, each
    /// note's id and title; whether the margin was beside the
    /// conversation and how often it had changed when they were last
    /// drawn so, and whether a note put since waits to be.
    inline: HashMap<u64, (String, String)>,
    /// The title each note's marker says, by pin: drawn again when the
    /// note's changes, as a task's note read for the first time does.
    marked: HashMap<u64, String>,
    /// Other bots' notes the margin shows, by bot and note, as read from
    /// them; and when they were last read.
    references: HashMap<(String, String), margin::Reference>,
    references_read: Option<Instant>,
    /// The tasks this agent started that have a turn running, as the
    /// daemon says without windows for the project's agents; and when
    /// they were last read.
    tasks_running: Option<std::collections::HashSet<String>>,
    tasks_running_read: Option<Instant>,
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
    /// The agents named in what is drawn, as the daemon answered: a
    /// click on one's name opens it in the pane.
    names: std::cell::RefCell<links::Names>,
    /// The words being asked about, and what the daemon said of each.
    naming: Option<tokio::task::JoinHandle<Vec<(String, bool)>>>,
    /// Ctrl+O pressed: every row a click opens is opened, or folded again,
    /// once the loop can read the messages between agents.
    opening_all: bool,
    /// A program in the pane drew something: the screen is drawn again.
    woken: Arc<tokio::sync::Notify>,
    /// Every call drawn, rather than the clean view of the bot's words and
    /// the calls that move the work on.
    verbose: bool,
    /// The focus the agent is in zen on: only the person's turns and the
    /// agent's questions are drawn, the conversation alone in the middle.
    zen: Option<String>,
    /// The running turn, or the newest, was started by something other
    /// than the person: another agent, a task's news, the lookout.
    elsewhere: bool,
    /// The lead's instructions as composed now, sent once no turn runs.
    instruct: Option<String>,
    /// What Sonny's upgrade set beside the person's own edits, told to the
    /// bot once it is set up.
    upgrades: Option<skills::Upgrades>,
    /// The folder the next message moves the bot to: its lead moved away.
    move_to: Option<String>,
    /// The name under which Sonny notes the instructions it wrote.
    lead: String,
    /// When to connect again, while it or the fleet's is down.
    retry: Option<Instant>,
    /// `/model`, open over the composer.
    picker: Option<models::Picker>,
    /// `/config`, open over the composer.
    config: Option<config::View>,
    /// The model listing `/model` waits on.
    listing: Option<tokio::sync::oneshot::Receiver<Result<Value, String>>>,
    /// The model and effort picked, set once no turn runs.
    switch: Option<(String, Option<String>)>,
    /// The prompt-cache lifetime a bot made before Sonny gave its model
    /// one takes, set once no turn runs.
    cache: Option<&'static str>,
    /// `/fast`: the speed picked, set once no turn runs.
    pace: Option<&'static str>,
    /// The bot's tools, which its permissions row gates.
    tools: Vec<String>,
    /// The tools its client declared, of which a mode gates Sonny's that
    /// can drop work or run outside the sandbox.
    declared: Value,
    /// Its permissions row, when it is one of Sonny's.
    mode: Option<&'static str>,
    /// `/permissions` or `/fast`, open over the composer: the row picked.
    choosing: Option<(Choice, usize)>,
    /// The rows `/permissions` lists, as the config had them when it opened.
    rows: Vec<approvals::Mode>,
    /// Calls the approver serving `auto` is reviewing in this turn.
    reviewing: std::collections::HashSet<String>,
    /// Calls it denied while Sonny watched, newest last: what `/retry`
    /// offers to retry once.
    denials: Vec<Denial>,
    /// `/retry`, open over the composer: the denial picked.
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
    /// The number the next group of rows (a fold, a roster) takes: never
    /// one taken before, on whatever page.
    next_group: usize,
    /// The conversation's screen, put by while an older page is written
    /// on one of its own.
    newer: Option<screen::Screen>,
    /// Words between agents drawn quiet, by node: what was said, who
    /// said it, and whether a click opened it.
    relayed: HashMap<i64, Relayed>,
    /// Those messages gathered as they came, by group number; and the one
    /// the last went into, with the rows drawn by then, which the next
    /// message, or a call the agent makes about it, joins while nothing
    /// else was drawn.
    clusters: BTreeMap<usize, Cluster>,
    clustering: Option<(usize, usize)>,
    /// The rows drawn by the end of the last quiet row (a fold of calls,
    /// a note's marker, messages between agents), which the next one
    /// goes right under while nothing else was drawn.
    stacked: Option<usize>,
    /// The tasks the lead started, by the group they were started in; the
    /// group the last went into, with the rows drawn by then, as a fold
    /// has; and when what they do was last read, or
    /// none for all of them to be read now.
    tasks: BTreeMap<usize, Vec<started::Task>>,
    starting: Option<(usize, usize)>,
    tasks_read: Option<Instant>,
    /// When every task started, ended ones too, was last read.
    tasks_all_read: Option<Instant>,
    /// The width the tasks started were drawn at.
    tasks_width: usize,
    /// How many triggers and subscriptions run for this agent, as the
    /// footer's `↻` says, and when they were last counted.
    behind: (usize, Option<Instant>),
    /// How many of what `/background` lists failed since the person last
    /// opened it, as the footer's `✗` says: a count of what is unread, as
    /// a notification badge is, cleared by looking.
    failed: usize,
    /// The failures `/background` lists, and those the person saw there.
    failures: (Vec<String>, std::collections::HashSet<String>),
    /// How the turns triggers' last fires sent ended, by handle, as the
    /// daemon said: asked once a turn is seen, and again while it runs.
    turns: HashMap<String, Value>,
    /// The turn the person's words last went to from this window: only
    /// its end is followed by a guess at their next message.
    yours: Option<i64>,
    /// That guess, being made.
    suggesting: Option<tokio::task::JoinHandle<Option<String>>>,
    /// The guess, shown in the empty composer for Tab to take.
    suggestion: Option<String>,
}

/// The lead's tasks, laid out in the project's home in a multiplexer: a window for
/// each task while it works, opened as its turn starts. It follows every
/// bot on its own connection, so the lead's own view is not disturbed.
struct Fleet {
    /// Worked on a thread of its own, so the multiplexer never holds up
    /// the screen.
    layout: mux::Worker,
    /// What its jobs have to say in the lead's view, as they finish.
    said: tokio::sync::mpsc::UnboundedReceiver<String>,
    tell: tokio::sync::mpsc::UnboundedSender<String>,
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
    /// Said once in the status row when tasks open where this pane is not.
    note: Option<String>,
    /// What each bot is doing, as its window says.
    doing: HashMap<String, &'static str>,
    /// The name each window was given, kept by the worker.
    named: Arc<std::sync::Mutex<HashMap<String, String>>>,
    /// What to ring for, taken by the app after each event.
    alerts: Vec<(sound::Alert, String)>,
    /// The lead is in zen: only its own questions ring.
    zen: bool,
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

/// A lead as the person tells it apart: `shop lead` for the folder's
/// first, `shop lead billing` for `shop.lead.billing`.
fn lead_title(project: &str, bot: &str) -> String {
    match bot.ends_with(".lead") {
        true => format!("{project} lead"),
        false => format!("{project} lead {}", project::lead_short(bot)),
    }
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

/// What `bot`, made by `by`, is to `lead`, a lead of `project`, whose
/// lineage made the bots in `lineage`, with `leads` the other projects
/// seen: any other project's lead but one this project is inside, by its
/// name alone; never another lead of this folder; else a task when one of
/// the lineage made it, or, to the folder's first lead, when no agent did
/// and its name is the project's, unless it is one of theirs.
fn kin(
    bot: &str,
    by: &str,
    lead: &str,
    project: &str,
    lineage: &std::collections::HashSet<String>,
    leads: &std::collections::HashSet<String>,
) -> Kin {
    let first = lead == format!("{project}.lead");
    let theirs = leads.iter().any(|p| bot.starts_with(&format!("{p}.")));
    // A project this one is inside names all of this one's bots too.
    let outer = |p: &str| project.starts_with(&format!("{p}."));
    match project::lead_project(bot).filter(|_| bot != lead) {
        Some(other) if other == project || outer(other) => Kin::None,
        Some(other) => Kin::Lead(other.to_owned()),
        None if !theirs
            && (lineage.contains(by)
                || (first && by.is_empty() && bot.starts_with(&format!("{project}.")))) =>
        {
            Kin::Task
        }
        _ => Kin::None,
    }
}

/// The bots among `bots` (as `bots` lists them) that are `lead`'s: made by
/// it or by one of its, or, for the folder's first lead, made by no agent
/// at all; never another lead.
fn lineage_of(bots: &[Value], lead: &str, first: bool) -> std::collections::HashSet<String> {
    let made_by: HashMap<&str, &str> = (bots.iter())
        .filter_map(|bot| {
            Some((
                bot["name"].as_str()?,
                bot["created_by"].as_str().unwrap_or(""),
            ))
        })
        .collect();
    let mut mine = std::collections::HashSet::new();
    for &name in made_by.keys() {
        if name == lead || project::is_lead(name) {
            continue;
        }
        let mut at = name;
        // A chain of makers no longer than the daemon lists bots.
        for _ in 0..=made_by.len() {
            match made_by.get(at).copied() {
                Some(by) if by == lead => {
                    mine.insert(name.to_owned());
                    break;
                }
                Some(by) if by.is_empty() || project::is_lead(by) => {
                    if by.is_empty() && first {
                        mine.insert(name.to_owned());
                    }
                    break;
                }
                Some(by) => at = by,
                None => {
                    // Made by a bot no longer listed: the first lead's.
                    if first && at != name {
                        mine.insert(name.to_owned());
                    }
                    break;
                }
            }
        }
    }
    mine
}

/// What a lead's windows are named from: its project, the lead, what
/// each of its bots is doing, and every bot of its lineage.
struct Naming {
    project: String,
    lead: String,
    doing: HashMap<String, &'static str>,
    lineage: std::collections::HashSet<String>,
}

impl Naming {
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
            true => lead_title(&self.project, bot),
            false => self.short(bot).to_owned(),
        }
    }

    /// Whether a block is the project's: a bot's, or a command one asked
    /// for (`BOT/NAME`).
    fn owns(&self, label: &str) -> bool {
        let bot = label.split_once('/').map_or(label, |(bot, _)| bot);
        self.doing.contains_key(bot) || self.lineage.contains(bot)
    }

    /// The name of a window holding the blocks labelled `labels`: after
    /// the lead, or its first bot, and what that bot is doing, with a count
    /// of the others that are working or need the person: `shop lead ·
    /// done · 2 working`. None for a window that also holds the person's
    /// own blocks, as when Sonny was opened beside them, which keeps the
    /// name they gave it.
    fn window(&self, labels: &[String]) -> Option<String> {
        if !labels.iter().all(|label| self.owns(label)) {
            return None;
        }
        let bots: Vec<&String> = labels
            .iter()
            .filter(|l| self.doing.contains_key(*l))
            .collect();
        let first = bots
            .iter()
            .find(|b| **b == &self.lead)
            .or_else(|| bots.first())
            .copied()?;
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
        Some(name)
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
        let (tell, said) = tokio::sync::mpsc::unbounded_channel();
        let mut fleet = Self {
            layout: mux::Worker::new(layout),
            said,
            tell,
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
            named: Default::default(),
            alerts: Vec::new(),
            zen: false,
            ended: HashMap::new(),
            waits: Default::default(),
            returned: Vec::new(),
            resolving: Default::default(),
        };
        let own = lead.to_owned();
        fleet.layout.send(move |layout| layout.adopt_own(&own));
        // Read now, so `/status` has where each task sits when it opens.
        let _ = fleet.layout.places();
        fleet.doing(lead, "idle");
        fleet.reconcile().await;
        Some(fleet)
    }

    /// Tasks named in the project's way, and which are working, read
    /// afresh: at start, and after a reconnect whose missed events are not
    /// replayed. One that stopped meanwhile is marked idle, quietly.
    async fn reconcile(&mut self) {
        // Every page of them: a project past one page still finds a task
        // running that sorts after it.
        let mut bots = Vec::new();
        let mut after = Value::Null;
        loop {
            let asked =
                json!({"name": format!("{}.*", self.project), "limit": 256, "after": after});
            let Ok(page) = self.client.request("bots", asked).await else {
                return;
            };
            bots.extend(page["bots"].as_array().cloned().unwrap_or_default());
            after = page["next_after"].clone();
            if after.is_null() {
                break;
            }
        }
        // A project inside this one has a lead, and a home, of its own: its
        // lead opens there and its tasks are its own.
        let names = bots.iter().filter_map(|bot| bot["name"].as_str());
        self.leads.extend(inner_projects(names, &self.project));
        // With other leads in the folder, their tasks are theirs; and to
        // a lead other than the first, the first's are, gone or not.
        let first = self.lead == format!("{}.lead", self.project);
        let shared = !first
            || (bots.iter()).any(|bot| {
                let name = bot["name"].as_str().unwrap_or("");
                name != self.lead && project::lead_project(name) == Some(self.project.as_str())
            });
        let mine = shared.then(|| lineage_of(&bots, &self.lead, first));
        for bot in &bots {
            // Side chats are never tasks, as when they are made.
            let Some(name) = bot["name"].as_str().filter(|_| bot["expire_ms"].is_null()) else {
                continue;
            };
            if name != self.lead
                && (project::is_lead(name)
                    || mine.as_ref().is_some_and(|mine| !mine.contains(name)))
            {
                continue;
            }
            let running = !bot["running_turn"].is_null();
            if (self.leads.iter()).any(|p| name.starts_with(&format!("{p}."))) {
                continue;
            }
            self.lineage.insert(name.to_owned());
            // A turn that ended while away is not known by number, and
            // one that started makes the last ending stale.
            self.ended.remove(name);
            if running && name != self.lead {
                self.show(name, false);
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

    /// Open the task's window, unless it has one. With `tell`, the lead's
    /// view hears where it opened, when not beside it, or why it did not.
    fn show(&mut self, bot: &str, tell: bool) {
        let mut command = self.command.clone();
        command.extend(["--agent".into(), bot.into()]);
        let name = format!("{} · working", self.short(bot));
        let (bot_name, home, here) = (
            bot.to_owned(),
            self.layout.home().to_owned(),
            self.layout.here(),
        );
        let told = self.tell.clone();
        self.layout.send(move |layout| {
            let said = match layout.show(&bot_name, &name, &command) {
                Ok(true) if !here => Some(format!("{bot_name} is in {home}")),
                Ok(_) => None,
                Err(error) => Some(error),
            };
            if let Some(said) = said.filter(|_| tell) {
                let _ = told.send(said);
            }
        });
        self.doing(bot, "working");
    }

    /// Close the task's window, and every block it asked for.
    fn close(&self, bot: &str) {
        let bot = bot.to_owned();
        self.layout.send(move |layout| layout.close(&bot));
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
                    self.close(&task);
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
            self.close(task);
            self.doing.remove(task);
            self.name_windows();
        }
    }

    /// A bot's name in its project's session: `lead`, or the task's name.
    fn short<'a>(&self, bot: &'a str) -> &'a str {
        self.naming().short(bot)
    }

    /// What names the windows, as things stand.
    fn naming(&self) -> Naming {
        Naming {
            project: self.project.clone(),
            lead: self.lead.clone(),
            doing: self.doing.clone(),
            lineage: self.lineage.clone(),
        }
    }

    /// Note what the bot is doing, and name the windows after it.
    fn doing(&mut self, bot: &str, doing: &'static str) {
        if self.doing.get(bot) != Some(&doing) {
            self.doing.insert(bot.to_owned(), doing);
            self.name_windows();
            let short = self.short(bot).to_owned();
            let alert = match doing {
                // In zen the lead breaks through with a question alone.
                _ if self.zen && !(bot == self.lead && doing == "needs you") => None,
                // The lead's view rings for its own ends: only it knows
                // whether a turn the lookout started said anything.
                "done" if bot == self.lead => None,
                "done" => Some((sound::Alert::TaskDone, format!("{short} is done"))),
                started::QUIET => Some((
                    sound::Alert::TaskDone,
                    format!("{short} is done, saying nothing"),
                )),
                "failed" => Some((sound::Alert::Failed, format!("{short} failed"))),
                "needs you" => Some((sound::Alert::NeedsYou, format!("{short} needs you"))),
                _ => None,
            };
            self.alerts.extend(alert);
        }
    }

    /// Name each window holding the project's bots after what they are
    /// doing ([`Naming::window`]), on the worker, from its blocks then.
    fn name_windows(&mut self) {
        let naming = self.naming();
        let named = self.named.clone();
        self.layout.names(move |layout| {
            let Ok(mut named) = named.lock() else {
                return;
            };
            for (window, labels) in layout.windows() {
                let Some(name) = naming.window(&labels) else {
                    continue;
                };
                if named.get(&window) != Some(&name) {
                    layout.rename_window(&window, &name);
                    named.insert(window, name);
                }
            }
        });
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
                "tool_completed" | "turn_resumed" | "step_decided"
                    if self.doing.get(bot) == Some(&"needs you") =>
                {
                    self.doing(bot, "working")
                }
                "turn_finished" => {
                    self.doing(
                        bot,
                        match data["status"].as_str() {
                            Some("completed" | "steered") => "done",
                            // A lead says nothing to news by design.
                            Some("failed") if data["error"] == EMPTY && bot == self.lead => "done",
                            Some("failed") if data["error"] == EMPTY => started::QUIET,
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
                match kin(
                    bot,
                    by,
                    &self.lead,
                    &self.project,
                    &self.lineage,
                    &self.leads,
                ) {
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
            "accepted" if bot != self.lead && self.lineage.contains(bot) => {
                self.show(bot, true);
                None
            }
            "deleted" => {
                self.close(bot);
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
        let paused = Some(client.paused().clone()).filter(|pause| pause.is_object());
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
            boost: None,
            speed: speed(record),
            context: None,
            window: None,
            input: prompt::Prompt::default(),
            pending: Vec::new(),
            marks: Vec::new(),
            any_history: false,
            welcome: false,
            width: 80,
            height: 24,
            live: None,
            streamed: false,
            live_item: None,
            ahead: std::collections::VecDeque::new(),
            running: record["running_turn"].as_i64().map(|turn| Running {
                turn,
                started: Instant::now(),
                phase: "Working".into(),
                note: false,
                effort: None,
            }),
            calls: HashMap::new(),
            approvals: Vec::new(),
            asks: Vec::new(),
            focus: None,
            aside: Vec::new(),
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
            paused,
            quit: false,
            switch_to: None,
            started: Instant::now(),
            fleet: None,
            held: Vec::new(),
            seen_node: 0,
            turn_model: None,
            pick: 0,
            watch: None,
            behind: (0, None),
            failed: 0,
            failures: Default::default(),
            turns: HashMap::new(),
            yours: None,
            suggesting: None,
            suggestion: None,
            asks_seen: HashMap::new(),
            sounds: sound::Sounds::new(false),
            said: String::new(),
            staged: Vec::new(),
            queued: Vec::new(),
            pictures: Vec::new(),
            reactions: reactions::Reactions::default(),
            picks: fuzzy::Picks::default(),
            confirming: false,
            replies: Vec::new(),
            quoting: false,
            reacting: None,
            finding: None,
            react_to: None,
            reaction_turn: None,
            attached: Vec::new(),
            profile: Default::default(),
            declare: None,
            role: served::Role::at(2, 2),
            bot_reactions: Vec::new(),
            react_calls: HashMap::new(),
            picked: reactions::Picked::default(),
            picked_read: Instant::now(),
            eyes: Some(banner::Eyes::new(Instant::now())),
            answering: None,
            rows_drawn: 0,
            thought: String::new(),
            replay: None,
            options,
            down: false,
            repaint: false,
            stale: false,
            first_node: None,
            older: None,
            unpaired: Vec::new(),
            pages: HashMap::new(),
            next_page: 0,
            short_read: std::cell::Cell::new(false),
            to_first: false,
            page_rows: (0, 0),
            find_step: None,
            full_width: 0,
            pane: None,
            pane_keys: false,
            margin_posted: 0,
            margin_opens: Default::default(),
            pane_mouse: false,
            closed: None,
            composer_at: None,
            counter_at: Vec::new(),
            composing: None,
            composer_click: None,
            margin: margin::Margin::default(),
            under: None,
            margin_focus: None,
            closing: std::collections::HashSet::new(),
            inline: HashMap::new(),
            marked: HashMap::new(),
            references: HashMap::new(),
            references_read: None,
            tasks_running: None,
            tasks_running_read: None,
            folded: None,
            refold: false,
            plans: HashMap::new(),
            plan_count: 0,
            plan_full: None,
            jump: None,
            pressed: Vec::new(),
            names: Default::default(),
            naming: None,
            opening_all: false,
            woken: Arc::new(tokio::sync::Notify::new()),
            verbose: false,
            zen: None,
            elsewhere: false,
            instruct: None,
            upgrades: None,
            move_to: None,
            lead: String::new(),
            retry: None,
            picker: None,
            config: None,
            listing: None,
            switch: None,
            cache: None,
            pace: None,
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
            rows: Vec::new(),
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
            next_group: 0,
            newer: None,
            folding: None,
            relayed: HashMap::new(),
            clusters: BTreeMap::new(),
            clustering: None,
            stacked: None,
            tasks: BTreeMap::new(),
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
        self.picks = fuzzy::Picks::load("commands");
        self.margin = margin::Margin::load(self.reactions_key.as_deref());
        self.picked = reactions::Picked::load(self.reactions_key.as_deref());
        self.history().await;
        self.welcome_above();
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
        self.tell_upgrades().await;
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
                // What it wrote before failing is on the screen, not in
                // what the next draw compares with.
                Err(_) if failed_draws < DRAW_ATTEMPTS => {
                    failed_draws += 1;
                    self.stale = true;
                }
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
                news = async {
                    match &mut self.fleet {
                        Some(fleet) => tokio::select! {
                            Some(said) = fleet.said.recv() => Err(said),
                            event = fleet.events.recv(), if !fleet.down => Ok(event),
                            else => std::future::pending().await,
                        },
                        None => std::future::pending().await,
                    }
                } => {
                    // What the layout's worker said, between the daemon's events.
                    let event = match news {
                        Ok(event) => event,
                        Err(said) => {
                            self.fleet_said(said);
                            continue;
                        }
                    };
                    let Some(event) = event else {
                        if let Some(fleet) = &mut self.fleet {
                            fleet.down = true;
                        }
                        self.retry.get_or_insert_with(Instant::now);
                        continue;
                    };
                    let zen = self.zen.is_some();
                    let said = self.fleet.as_mut().and_then(|fleet| {
                        fleet.zen = zen;
                        fleet.event(&event)
                    });
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
                        self.fleet_said(said);
                    }
                }
                _ = tick.tick() => {}
                _ = self.woken.notified() => {}
            }
            if self.picked_read.elapsed() > Duration::from_secs(1) {
                self.picked_read = Instant::now();
                let mut nodes: Vec<i64> = (self.picked.refresh().into_iter())
                    .map(|(node, ..)| node)
                    .collect();
                nodes.dedup();
                for node in nodes {
                    self.place_picked(node);
                }
            }
            if self.pane.as_ref().is_some_and(pane::Pane::is_card)
                && self.watch.as_ref().is_some_and(|w| {
                    w.read.elapsed() > Duration::from_secs(2)
                        || w.usage.as_ref().is_some_and(usage::View::due)
                })
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
            self.suggested_now().await;
            self.shell_ran().await;
            self.listed();
            self.follow_place().await;
            self.count_behind().await;
            self.read_tasks().await;
            self.read_running_tasks().await;
            self.read_references().await;
            self.read_names().await;
            for what in std::mem::take(&mut self.pressed) {
                self.press(&what).await;
            }
            // Older items drawn above before the view reaches the first row.
            if self.older.is_some() && self.screen.near_top() {
                self.older().await;
            }
            if self.to_first {
                self.screen.scroll(isize::MAX);
                self.to_first = self.repaint || self.older.is_some() && self.screen.near_top();
                // The next page asked for once this one is drawn, not a tick on.
                if self.to_first && !self.repaint {
                    self.woken.notify_one();
                }
            }
            if let Some((older, looked)) = &mut self.find_step {
                let older = *older;
                match (self.screen.find_gap(older, looked)).filter(|_| self.finding.is_some()) {
                    // A page at a time, the screen drawn between.
                    Some(page) => {
                        looked.push(page);
                        self.draw_again(page).await;
                        // A read cut short steps over what it did not draw.
                        match self.short_read.get() {
                            true => {
                                self.find_step = None;
                                self.screen.find_next(older);
                            }
                            false => self.woken.notify_one(),
                        }
                    }
                    None => {
                        self.find_step = None;
                        self.screen.find_next(older);
                    }
                }
            }
            // A page let go drawn again before the view reaches it, or
            // once the way up to the first turn, or a step finding, is
            // done: never letting go a page found in before it is shown.
            if !self.to_first
                && self.find_step.is_none()
                && let Some(page) = self.screen.wanted()
            {
                self.draw_again(page).await;
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
            match self
                .send(record.clone(), Some(SHELL), false, false, &[])
                .await
            {
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

    /// What Sonny's upgrade set beside the person's edits, told to the bot
    /// after any turn it runs, so it asks the person whether to bring it in.
    async fn tell_upgrades(&mut self) {
        let Some(upgrades) = self.upgrades.take() else {
            return;
        };
        if upgrades.note().is_empty() {
            upgrades.told();
            return;
        }
        let asked = json!({"bot": self.bot, "prompt": upgrades.note(),
            "request_id": upgrades.request_id(), "delivery": "queue",
            "origin": skills::UPGRADE_ORIGIN});
        match self.client.request("submit", asked).await {
            Ok(_) => upgrades.told(),
            // Told it already.
            Err(error) if error.code == "idempotency_conflict" => upgrades.told(),
            Err(error) => {
                upgrades.back();
                self.notice = Some(format!("telling of Sonny's upgrade: {error}"));
            }
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
            && self.cache.is_none()
            && self.pace.is_none()
        {
            return true;
        }
        if self.running.is_some() {
            return false;
        }
        let (text, switch, gating) = (self.instruct.take(), self.switch.take(), self.gating.take());
        let cache = self.cache.take();
        let pace = self.pace.take();
        let mut declare = self.declare.take();
        // A bot that went without the tools Sonny gates, as its gate was
        // not Sonny's, has them with the mode the person picks.
        if declare.is_none() && gating.is_some() && self.mode.is_none() {
            declare = declared::declare(&json!({"declared": self.declared}), self.role);
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
        if let Some(ttl) = cache {
            asked["cache_ttl"] = json!(ttl);
        }
        if let Some(speed) = pace {
            asked["speed"] = json!(speed);
        }
        if let (Some(mode), Value::Object(fields)) = (gating, &mut asked) {
            let declared = declare.as_ref().unwrap_or(&self.declared);
            fields.extend(approvals::gate(
                mode,
                &approvals::gateable(&self.tools, declared),
            ));
            // An agent's sandbox goes with its row, a lead's, a task's or
            // a side chat's, which the daemon holds within its agent's.
            let workspace = std::path::Path::new(&self.workspace);
            fields.extend(approvals::sandbox(mode, workspace));
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
                    // The bot's own model now, for this window's turns too,
                    // and a boost armed against the old effort let go.
                    self.turn_model = None;
                    self.boost = None;
                }
                if let Some(speed) = pace {
                    self.speed = Some(speed)
                        .filter(|speed| *speed != "standard")
                        .map(str::to_owned);
                }
                if let Some(mode) = gating {
                    self.mode = Some(mode);
                    if approvals::reviews(Some(mode))
                        && let Err(error) = serve_auto(&self.client, &self.model).await
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
                self.cache = cache;
                self.pace = pace;
                return false;
            }
            Err(error) => {
                self.notice = Some(error.to_string());
                return false;
            }
        }
        true
    }

    /// Give the row picked with `/permissions` to every agent the lead made,
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
                    self.notice = Some(format!("{name}'s permissions: {error}"));
                }
            }
        }
        // The lead's own change still waiting: an agent it makes meanwhile
        // takes its old gate, so this goes on until both are set.
        if !busy && self.gating.is_none() {
            self.spread = None;
        }
    }

    /// The rows `/fast` lists: standard, and each tier faster than it
    /// the bot's provider serves.
    fn speed_rows(&self) -> Vec<(&'static str, &'static str)> {
        let serves = self.speeds();
        (SPEEDS.into_iter())
            .filter(|(speed, _)| *speed == "standard" || serves.iter().any(|s| s == speed))
            .collect()
    }

    /// Serves the bot at `speed` from now, or from when no turn runs.
    async fn pace_at(&mut self, speed: &'static str) {
        let now = (self.pace).unwrap_or(self.speed.as_deref().unwrap_or("standard"));
        if speed == now {
            return;
        }
        self.pace = Some(speed);
        self.notice = Some(match (self.configure().await, self.pace.is_some()) {
            (true, _) if speed == "standard" => "standard speed from the next call".into(),
            (true, _) => format!("{speed} from the next call, at a higher price per token"),
            (false, true) => "applies when idle".into(),
            // Refused: the notice already says why.
            (false, false) => return,
        });
    }

    /// The tiers faster than standard the bot's provider serves.
    fn speeds(&self) -> Vec<String> {
        let provider = self
            .model
            .split_once('/')
            .map_or("", |(provider, _)| provider);
        (self.client.providers()[provider]["speeds"].as_array())
            .into_iter()
            .flatten()
            .filter_map(|speed| speed.as_str().map(str::to_owned))
            .collect()
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
            self.boost = None;
        }
        if let Some(workspace) = record["workspace"].as_str() {
            self.workspace = workspace.to_owned();
        }
        self.speed = speed(record);
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
    /// cleared, and the bot's items read again as on attach. The pages of
    /// older items above are drawn again only as the view nears them.
    async fn repaint(&mut self, terminal: &mut Tui) -> Result<(), String> {
        // The pages of older items above stay, as blank rows drawn again
        // as the view nears them: only the newest rows are drawn now.
        let pages = self.screen.pages();
        let newest = (pages.last())
            .and_then(|(id, _)| self.pages.get(id))
            .and_then(|paged| paged.nodes.last().copied());
        self.screen.clear();
        if newest.is_none() {
            self.pages.clear();
        }
        terminal.clear().map_err(|e| e.to_string())?;
        self.pending.clear();
        self.marks.clear();
        self.folds.clear();
        self.folding = None;
        self.relayed.clear();
        self.clusters.clear();
        self.clustering = None;
        self.stacked = None;
        self.plans.clear();
        self.plan_full = None;
        self.refold = true;
        self.tasks.clear();
        self.starting = None;
        self.held.clear();
        self.live = None;
        self.any_history = false;
        self.seen_node = 0;
        if let Some(newest) = newest {
            // What calls older than the oldest page returned stays its own.
            let unpaired = std::mem::take(&mut self.unpaired);
            self.history_since(newest, true).await;
            self.unpaired = unpaired;
            self.screen.prepend_blank(pages);
            return Ok(());
        }
        // Back to the first node this window drew, not only the newest page.
        match self.first_node {
            Some(first) => {
                self.commit_welcome();
                self.history_since(first - 1, true).await;
            }
            None => {
                self.history().await;
                self.welcome_above();
            }
        }
        Ok(())
    }

    /// The page of items before the oldest drawn, drawn on its own and
    /// put above the rest, the view left where it is: the conversation
    /// drawn so far is not drawn again, and a turn running goes on. On
    /// the way up to the first turn, the pages over it are blank rows as
    /// tall as they may be, a few at a time, drawn once the view nears
    /// them.
    async fn older(&mut self) {
        // Newest first, those on the way up, by their ids and rows.
        let mut blanks: Vec<(u64, usize)> = Vec::new();
        while let Some(from) = self.older
            && blanks.len() < WALKED_PAGES
        {
            // On the way up, as many nodes as a read gives, cut into pages.
            let limit = match self.to_first {
                true => WALKED_ITEMS,
                false => OLDER_ITEMS,
            };
            let asked = json!({"bot": self.bot, "from": from, "limit": limit});
            let page = match self.client.request("history_nodes", asked).await {
                Ok(page) => page,
                Err(error) => {
                    self.notice = Some(error.to_string());
                    // Not asked again at once: the way up stops.
                    self.to_first = false;
                    break;
                }
            };
            let mut nodes: Vec<i64> = Vec::new();
            let mut turns: HashMap<i64, i64> = HashMap::new();
            for n in page["nodes"].as_array().into_iter().flatten() {
                let Some(node) = n["node"].as_i64() else {
                    continue;
                };
                nodes.push(node);
                if let Some(turn) = n["turn"].as_i64() {
                    turns.insert(node, turn);
                }
            }
            nodes.reverse();
            self.older = page["next_from"].as_i64();
            if nodes.is_empty() {
                self.older = None;
                break;
            }
            self.short_read.set(false);
            let mut items = match self.to_first {
                true => self.items_at(&nodes).await,
                false => Vec::new(),
            };
            // A read cut short: the rest of the way drawn a page at a time.
            if self.short_read.get() {
                self.older = Some(from);
                self.to_first = false;
                break;
            }
            while !nodes.is_empty() {
                let cut = nodes.len().saturating_sub(OLDER_ITEMS as usize);
                let ours = nodes.split_off(cut);
                self.first_node = Some(ours[0]);
                let turns = (ours.iter())
                    .filter_map(|node| Some((*node, *turns.get(node)?)))
                    .collect();
                let after = std::mem::take(&mut self.unpaired);
                let first = self.older.is_none() && nodes.is_empty();
                let paged = Paged::new(ours, turns, &after, first);
                let id = self.next_page;
                self.next_page += 1;
                let cut = items.partition_point(|(node, _)| *node < paged.nodes[0]);
                let read = items.split_off(cut);
                if !self.to_first || paged.first {
                    blanks.reverse();
                    self.screen.prepend_blank(blanks);
                    let (older, unpaired) = self.draw_page(&paged, after).await;
                    self.unpaired = unpaired;
                    self.page_rows.0 += older.rows();
                    self.page_rows.1 += paged.nodes.len();
                    self.pages.insert(id, paged);
                    self.screen.prepend(id, older);
                    return;
                }
                // Only read: what its calls returned is all drawing it gives.
                self.unpaired = unpaired(read.into_iter().chain(after));
                let (rows, items) = self.page_rows;
                blanks.push((id, paged.nodes.len() * rows.max(1) / items.max(1)));
                self.pages.insert(id, paged);
            }
        }
        blanks.reverse();
        self.screen.prepend_blank(blanks);
    }

    /// The page `id`, let go as the view moved away, drawn again as it
    /// nears it: once all its items are read, else asked for again.
    async fn draw_again(&mut self, id: u64) {
        let Some(paged) = self.pages.remove(&id) else {
            return;
        };
        self.short_read.set(false);
        let mut after = self.items_at(&paged.after).await;
        after.retain(|(_, item)| {
            paged
                .returned
                .contains(item["call_id"].as_str().unwrap_or(""))
        });
        let (older, _) = self.draw_page(&paged, after).await;
        self.pages.insert(id, paged);
        if !self.short_read.get() {
            self.screen.fill(id, older);
        }
    }

    /// The rows of a page of older items, and what calls older than it
    /// returned: written on a screen of their own, what the newer rows
    /// were drawn with put by meanwhile, so a group of calls in the page
    /// drawn again is found there, not among the newer rows.
    async fn draw_page(
        &mut self,
        paged: &Paged,
        after: Vec<(i64, Value)>,
    ) -> (screen::Screen, Vec<(i64, Value)>) {
        self.flush();
        self.newer = Some(std::mem::take(&mut self.screen));
        let any_history = std::mem::replace(&mut self.any_history, false);
        let rows_drawn = self.rows_drawn;
        let bot_reactions = std::mem::take(&mut self.bot_reactions);
        let react_calls = std::mem::take(&mut self.react_calls);
        let answering = self.answering.take();
        let folding = self.folding.take();
        let starting = self.starting.take();
        let clustering = self.clustering.take();
        let stacked = self.stacked.take();
        if paged.first {
            self.commit_welcome();
        }
        let unpaired = self
            .draw_nodes(&paged.nodes, &paged.turns, true, after, None)
            .await;
        let wrote = std::mem::replace(&mut self.any_history, any_history);
        self.rows_drawn = rows_drawn;
        self.bot_reactions = bot_reactions;
        self.react_calls = react_calls;
        self.answering = answering;
        self.folding = folding;
        self.starting = starting;
        self.clustering = clustering;
        self.stacked = stacked;
        // A blank row apart from what was the first.
        if wrote {
            self.pending.push(Line::default());
        }
        self.flush();
        let newer = self.newer.take().unwrap_or_default();
        let older = std::mem::replace(&mut self.screen, newer);
        (older, unpaired)
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
        self.live_item = None;
        self.ahead.clear();
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
            let caught = caught_up(&mut fresh).await;
            // Read after the replay, so a turn that ends meanwhile arrives
            // as an event rather than lost between the two.
            let record = client
                .request("inspect", json!({"bot": self.bot}))
                .await
                .map_err(|e| e.to_string())?;
            Ok::<_, String>((client, fresh, record, caught))
        };
        if self.down {
            let Ok((client, fresh, record, caught)) = lead.await else {
                self.retry = Some(Instant::now() + Duration::from_secs(2));
                return;
            };
            self.paused = caught.paused(&client);
            self.client = client;
            *events = fresh;
            self.down = false;
            self.notice = None;
            self.context = caught.context.or(self.context);
            self.window = caught.window.or(self.window);
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
                    effort: None,
                });
                self.turn_began().await;
                status_report("working", &self.bot);
            }
            self.approvals_for(self.running.as_ref().map(|r| r.turn))
                .await;
            // A daemon that restarted brings back its companions; one
            // started anew, as by another store's Sonny, gets them now.
            if let Err(error) = start_companions(&self.client).await {
                self.notice = Some(error);
            }
            if approvals::reviews(self.mode)
                && let Err(error) = serve_auto(&self.client, &self.model).await
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
        // In zen the agent's questions are all of its calls drawn.
        if self.zen.is_some() && name != asks::TOOL {
            return;
        }
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
            let card = asks::card(&ask, self.width);
            let from = self.pending.len() + usize::from(self.any_history);
            self.commit(card.rows);
            self.marks
                .extend(card.options.into_iter().enumerate().map(|(at, row)| {
                    let button = format!("{}answer/{at}/{id}", screen::BUTTON);
                    (from + row, screen::Mark::Link(button))
                }));
            // Drawn narrower later, as when the margin opens, its bar
            // goes on down every row.
            self.marks.extend(
                (card.hangs.into_iter().enumerate())
                    .map(|(row, hang)| (from + row, screen::Mark::Hangs(hang))),
            );
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
        // What the agent did about messages from other agents while it
        // said nothing goes under them, opened with them: an agent it
        // made stands out still.
        if !self.verbose
            && !matches!(verb.as_str(), "Started" | "Forked")
            && let Some((at, drawn)) = self.clustering
            && drawn == self.rows_drawn
            && let Some(message) = (self.clusters.get_mut(&at)).and_then(|c| c.messages.last_mut())
        {
            message.calls.push(lines.clone());
            if self.draw_cluster(at) {
                return;
            }
            self.clusters.remove(&at);
            self.clustering = None;
        }
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
            if self.kept(screen::Tag::Group(first as u64)) {
                break;
            }
            self.folds.remove(&first);
        }
        let at = self.group();
        self.folds.insert(
            at,
            Fold {
                verb,
                whats: vec![what],
                calls: vec![lines.clone()],
                open: false,
            },
        );
        // Drawn folded from the first: what it did, opened on a click.
        let fold = &self.folds[&at];
        let opens = transcript::unfolds(&fold.whats, &fold.calls);
        let lines = transcript::quiet(transcript::folded(
            &fold.verb,
            &fold.whats,
            &fold.calls,
            false,
            self.prose(),
        ));
        let from = self.commit_quiet(lines);
        let tag = screen::Mark::Tag(screen::Tag::Group(at as u64));
        self.marks
            .extend((from..self.pending.len()).map(|line| (line, tag.clone())));
        if opens {
            let open = screen::Mark::Link(format!("{}fold/{at}", screen::BUTTON));
            self.marks.push((from, open));
        }
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
        // Its name opens it at once, where it was drawn before it was made.
        self.names.get_mut().learn(task.bot.clone(), true);
        self.screen.relink();
        self.tasks_read = None;
        if let Some((at, drawn)) = self.starting
            && drawn == self.rows_drawn
            && let Some(tasks) = self.tasks.get_mut(&at)
        {
            tasks.push(task.clone());
            if self.draw_started(at) {
                return;
            }
            // Gone from the conversation: this task starts a group of its own.
            self.tasks.remove(&at);
        }
        let at = self.group();
        self.tasks.insert(at, vec![task]);
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
        let tasks = self.tasks.get(&at).map_or(&[][..], Vec::as_slice);
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

    /// Whether rows tagged `tag` are in the conversation, drawn or waiting
    /// to be, the rows put by while an older page is written included.
    fn kept(&self, tag: screen::Tag) -> bool {
        (self.marks.iter()).any(|(_, mark)| *mark == screen::Mark::Tag(tag))
            || self.screen.has(tag)
            || (self.newer.as_ref()).is_some_and(|newer| newer.has(tag))
    }

    /// A number for a new group of rows, unlike any before it.
    fn group(&mut self) -> usize {
        self.next_group += 1;
        self.next_group - 1
    }

    /// What the tasks started are doing, read again every few seconds:
    /// those still at work, and every one when news of them came.
    async fn read_tasks(&mut self) {
        // Rosters whose rows the conversation let go are neither kept nor
        // read again.
        let gone: Vec<usize> = (self.tasks.keys())
            .copied()
            .filter(|at| !self.kept(screen::Tag::Tasks(*at as u64)))
            .collect();
        for at in gone {
            self.tasks.remove(&at);
        }
        // Drawn again at the width the conversation has now, as when the
        // pane opened beside it.
        if !self.tasks.is_empty() && self.tasks_width != self.prose() {
            self.tasks_width = self.prose();
            for at in self.tasks.keys().copied().collect::<Vec<_>>() {
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
        let mut bots: Vec<String> = (self.tasks.values().flatten())
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
                Ok(page) => {
                    let turn = self.task_turn(&page).await;
                    let state = started::state(&turn);
                    // A turn that ended on a question waits on the person,
                    // as `/status` says; read once per turn it ended.
                    match (state.as_str(), turn["turn"].as_i64()) {
                        ("done", Some(last)) => {
                            let seen = match self.asks_seen.get(&bot) {
                                Some((at, seen)) if *at == last => seen.clone(),
                                _ => {
                                    let seen = self.scan(&bot, Some(last)).await;
                                    self.asks_seen.insert(bot.clone(), (last, seen.clone()));
                                    seen
                                }
                            };
                            match seen.ask.is_some() && !seen.itself {
                                true => "needs you".to_owned(),
                                // One with a goal says whether that turn met it.
                                false => {
                                    lookout::goals::standing(&self.options.store, &bot, Some(last))
                                        .map_or(state, str::to_owned)
                                }
                            }
                        }
                        _ => state,
                    }
                }
                Err(error) if error.code == "bot_not_found" => "removed".to_owned(),
                // Not read this time: it stays as it was.
                Err(_) => continue,
            };
            states.insert(bot, state);
        }
        for at in self.tasks.keys().copied().collect::<Vec<_>>() {
            let mut changed = false;
            for task in self.tasks.get_mut(&at).into_iter().flatten() {
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
    /// at once, so the turn it joined is read instead, and one attached to
    /// the next call (a reaction) waits for work that is not there, so the
    /// turn before it is read too. A failed one with why it failed.
    async fn task_turn(&self, page: &Value) -> Value {
        let turns = page["turns"].as_array().map_or(&[][..], Vec::as_slice);
        let passive = |t: &&Value| matches!(t["status"].as_str(), Some("steered" | "attached"));
        let Some(turn) = turns.iter().find(|t| !passive(t)) else {
            return turns.first().cloned().unwrap_or(Value::Null);
        };
        self.with_error(turn.clone()).await
    }

    /// A failed `turn` with why it failed, which `turns` leaves out, as
    /// `wait` has it: a model that said nothing is no failure.
    async fn with_error(&self, mut turn: Value) -> Value {
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
        let name = (self.tasks.values().flatten())
            .find(|task| task.bot == bot)
            .map_or(bot.to_owned(), |task| task.name.clone());
        let args = ["--no-sound", "--no-layout", "--embedded", "--agent", bot].map(str::to_owned);
        match self.embed(&name, &args) {
            // Its title's button opens it in the main view.
            Ok(()) => {
                if let Some(pane) = &mut self.pane {
                    pane.as_agent(bot);
                }
            }
            Err(error) => self.notice = Some(format!("could not open {name}: {error}")),
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
        let pane = pane::Pane::program(title, program);
        // Opened from `/status`, it goes back there, as a browser goes back.
        match self.pane.take() {
            Some(shown) if shown.is_card() && self.watch.is_some() => {
                self.open_pane(pane.after(shown))
            }
            shown => {
                self.pane = shown;
                self.open_pane(pane);
            }
        }
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
    /// between agents at NODE; `task/BOT`, and `agent/BOT` on BOT's name
    /// in the conversation, opens BOT in the pane; `answer/N/ID` answers the question
    /// call ID asked with its option N, while it is open.
    async fn press_row(&mut self, button: &str) {
        if let Some(at) = button
            .strip_prefix("fold/")
            .and_then(|at| at.parse::<usize>().ok())
        {
            if let Some(fold) = self.folds.get_mut(&at) {
                fold.open = !fold.open;
                self.draw_fold(at);
            } else if let Some(cluster) = self.clusters.get_mut(&at) {
                cluster.open = !cluster.open;
                self.draw_cluster(at);
            }
        }
        if let Some(bot) = (button.strip_prefix("task/")).or(button.strip_prefix("agent/")) {
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
                    Some(item) => Some(quoting(item)),
                    None => {
                        self.notice = Some("could not read that message".into());
                        return;
                    }
                },
            };
            if let Some(relayed) = self.relayed.get_mut(&node) {
                relayed.text = text;
            }
            if let Some(at) = self.cluster_of(node) {
                self.draw_cluster(at);
            }
        }
        if let Some((at, id)) = button
            .strip_prefix("answer/")
            .and_then(|rest| rest.split_once('/'))
            .and_then(|(at, id)| Some((at.parse::<usize>().ok()?, id)))
        {
            // One set aside is still answered by its options.
            let Some(ask) = (self.asks.iter().chain(&self.aside))
                .find(|ask| ask.id == id)
                .cloned()
            else {
                self.notice = Some("that question was answered already".into());
                return;
            };
            let answer = asks::answer(&ask, &(at + 1).to_string());
            match self.send(answer, None, false, false, &[]).await {
                Ok(()) => {
                    self.notice = None;
                    self.close_ask(&ask);
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
        self.commit_quiet(vec![note_marker(what, true)]);
        // Drawn out where it was put, when the margin cannot show it.
        self.refold = true;
        if let Some(note) = note {
            let pin = margin::anchor(&note.call);
            let link = format!("{}note/{id}", screen::BUTTON);
            self.mark_last(screen::Mark::Tag(screen::Tag::Note(pin)));
            self.mark_last(screen::Mark::Link(link));
            self.marked.insert(pin, note.title);
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
            let line = note_marker(title.clone(), true);
            (self.screen).replace_tagged(screen::Tag::Note(pin), vec![line], vec![(0, link)]);
            self.marked.insert(pin, title);
        }
        // A note taken away, by the person's × or by the agent, is gone
        // from the conversation too, as it is once drawn again.
        let gone: Vec<u64> = (self.marked.keys())
            .filter(|pin| !notes.iter().any(|(at, ..)| at == *pin))
            .copied()
            .collect();
        for pin in gone {
            self.marked.remove(&pin);
            (self.screen).replace_tagged(screen::Tag::Note(pin), Vec::new(), Vec::new());
        }
        if beside {
            // A marker says its note's title as it is now.
            for (pin, id, title) in notes {
                let tag = screen::Tag::Note(pin);
                if self.marked.get(&pin) == Some(&title) || !self.screen.has(tag) {
                    continue;
                }
                let link = screen::Mark::Link(format!("{}note/{id}", screen::BUTTON));
                let line = note_marker(title.clone(), true);
                (self.screen).replace_tagged(tag, vec![line], vec![(0, link)]);
                self.marked.insert(pin, title);
            }
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

    /// Whether the person's message `said` at `node` took a note away,
    /// from this window or another: the margin shows it, once, keyed by
    /// the node it landed at whichever turn took it in. One this window
    /// took away when the person asked takes nothing more away.
    fn note_closed(&mut self, node: i64, said: &str) -> bool {
        let Some(id) = margin::closed(said) else {
            return false;
        };
        let key = format!("m{node}");
        if self.margin.holds(node) {
            return true;
        }
        match self.closing.remove(id) {
            true => self.margin.applied(&key),
            false => {
                self.margin.remove(id, Some((node, &key)));
            }
        }
        true
    }

    /// Commit a denied call, in either view.
    fn commit_denied(&mut self, name: &str, arguments: &Value, reason: &str) {
        let lines = transcript::denied(name, arguments, reason, &self.workspace, self.width);
        self.commit(lines);
    }

    /// The person's message at `node` landed: drawn as it reads, what
    /// they ran as it was when it ran, its last row tagged, and the
    /// message the bot now answers. Drawn `older` than the rest, what
    /// they ran is no part of what this window ran.
    fn land(
        &mut self,
        node: i64,
        (said, quotes): (&str, &str),
        from: Option<&str>,
        person: bool,
        older: bool,
    ) {
        // Reactions from here on answer this one.
        self.bot_reactions.clear();
        // Words between agents are one quiet row a click opens; none in zen.
        if transcript::relays(said, from) {
            if self.zen.is_none() {
                self.land_relayed(node, said, from, person);
            }
        } else {
            let prose = self.prose();
            let lines = match older {
                true => landed(said, &mut Vec::new(), prose),
                false => landed(said, &mut self.ran_here, prose),
            };
            if !lines.is_empty() {
                let lines = match quotes.is_empty() {
                    true => lines,
                    false => transcript::with_quotes(lines, quotes, prose),
                };
                self.commit(lines);
                self.mark_last(screen::Mark::Tag(screen::Tag::End(node)));
                // What another agent reacted with before it was drawn.
                if !self.picked.on(node).is_empty() {
                    self.place_picked(node);
                }
            }
        }
        self.answering = Some((first_words(said), self.rows_drawn, node));
    }

    /// The message between agents at `node`: one quiet row a click opens,
    /// gathered with those right before it into one that lists them.
    fn land_relayed(&mut self, node: i64, said: &str, from: Option<&str>, person: bool) {
        let relayed = Relayed {
            head: transcript::relayed_head(said, from),
            from: from.map(str::to_owned),
            person,
            text: None,
        };
        self.relayed.insert(node, relayed);
        let message = Clustered {
            node,
            kind: transcript::relayed_kind(said, from),
            names: transcript::relayed_names(said, from),
            calls: Vec::new(),
        };
        if let Some((at, drawn)) = self.clustering
            && drawn == self.rows_drawn
            && let Some(cluster) = self.clusters.get_mut(&at)
        {
            cluster.messages.push(message);
            if self.draw_cluster(at) {
                return;
            }
            // Gone from the conversation: this one starts a cluster of
            // its own.
            let mut gone = self.clusters.remove(&at).expect("the cluster just drawn");
            let message = gone.messages.pop().expect("the message just put in");
            self.forget(gone, node);
            return self.new_cluster(message);
        }
        self.new_cluster(message);
    }

    /// `message` drawn as a cluster of its own, which those after it join.
    fn new_cluster(&mut self, message: Clustered) {
        // Clusters whose rows the conversation let go keep nothing.
        while let Some(&first) = self.clusters.keys().next() {
            if self.kept(screen::Tag::Group(first as u64)) {
                break;
            }
            if let Some(gone) = self.clusters.remove(&first) {
                self.forget(gone, message.node);
            }
        }
        let at = self.group();
        self.clusters.insert(
            at,
            Cluster {
                messages: vec![message],
                open: false,
            },
        );
        let (lines, marks) = self.cluster_lines(at);
        let from = self.commit_quiet(lines);
        let tag = screen::Mark::Tag(screen::Tag::Group(at as u64));
        self.marks
            .extend((from..self.pending.len()).map(|line| (line, tag.clone())));
        self.marks
            .extend(marks.into_iter().map(|(line, mark)| (from + line, mark)));
        self.clustering = Some((at, self.rows_drawn));
    }

    /// A cluster the conversation let go, and its messages with it, so
    /// Ctrl+O reads none of them again; a message a page drawn again put
    /// in a newer cluster stays for that one, as `landing` does for the
    /// cluster about to be made for it.
    fn forget(&mut self, cluster: Cluster, landing: i64) {
        for message in cluster.messages {
            let held = message.node == landing
                || (self.clusters.values())
                    .any(|kept| kept.messages.iter().any(|m| m.node == message.node));
            if !held {
                self.relayed.remove(&message.node);
            }
        }
    }

    /// The rows of the message between agents at `node`, open or shut,
    /// at `width`; open, the calls the agent made about it under it.
    fn relayed_lines(
        &self,
        node: i64,
        calls: &[Vec<Line<'static>>],
        width: usize,
    ) -> Vec<Line<'static>> {
        let Some(relayed) = self.relayed.get(&node) else {
            return Vec::new();
        };
        match &relayed.text {
            Some(text) => {
                let mut lines = transcript::relayed(text, relayed.from.as_deref(), true, width);
                let calls = calls.iter().flatten().cloned().collect();
                lines.extend(transcript::indented(calls, 2));
                lines
            }
            None => {
                // Only an agent's name takes the agents' colour.
                let agent = relayed.from.as_deref().filter(|_| !relayed.person);
                transcript::relayed_shut(&relayed.head, agent, width)
            }
        }
    }

    /// The rows of the cluster `at`, and the buttons on them: one message
    /// as itself; several as a row that counts them, and once open each
    /// one's own rows under it.
    fn cluster_lines(&self, at: usize) -> (Vec<Line<'static>>, Vec<(usize, screen::Mark)>) {
        let Some(cluster) = self.clusters.get(&at) else {
            return (Vec::new(), Vec::new());
        };
        let width = self.prose();
        if let [message] = cluster.messages.as_slice() {
            let lines = self.relayed_lines(message.node, &message.calls, width);
            return (lines, vec![(0, said_button(message.node))]);
        }
        let kinds: Vec<&str> = cluster.messages.iter().map(|m| m.kind).collect();
        let names: Vec<String> = (cluster.messages.iter())
            .flat_map(|m| m.names.iter().cloned())
            .collect();
        let mut lines = vec![transcript::gathered(&kinds, &names, cluster.open, width)];
        let mut marks = vec![(
            0,
            screen::Mark::Link(format!("{}fold/{at}", screen::BUTTON)),
        )];
        if cluster.open {
            for message in &cluster.messages {
                marks.push((lines.len(), said_button(message.node)));
                let rows =
                    self.relayed_lines(message.node, &message.calls, width.saturating_sub(2));
                lines.extend(transcript::indented(rows, 2));
            }
        }
        (lines, marks)
    }

    /// Opens every fold, gathered row and message between agents still in
    /// the conversation, or folds them all again when none is left shut.
    async fn open_all(&mut self) {
        let opens = |fold: &Fold| transcript::unfolds(&fold.whats, &fold.calls);
        let open = self.folds.values().any(|fold| opens(fold) && !fold.open)
            || self.clusters.values().any(|cluster| !cluster.open)
            || self.relayed.values().any(|relayed| relayed.text.is_none());
        let folds: Vec<usize> = (self.folds.iter())
            .filter(|(_, fold)| opens(fold))
            .map(|(at, _)| *at)
            .collect();
        for at in folds {
            if let Some(fold) = self.folds.get_mut(&at)
                && fold.open != open
            {
                fold.open = open;
                self.draw_fold(at);
            }
        }
        // A message's words are read again from the history, all at once,
        // as a click on one reads it.
        let shut: Vec<i64> = match open {
            true => (self.relayed.iter())
                .filter(|(_, relayed)| relayed.text.is_none())
                .map(|(node, _)| *node)
                .collect(),
            false => Vec::new(),
        };
        let read = self.items_at(&shut).await;
        for relayed in self.relayed.values_mut().filter(|_| !open) {
            relayed.text = None;
        }
        for (node, item) in read {
            if let Some(relayed) = self.relayed.get_mut(&node) {
                relayed.text = Some(quoting(&item));
            }
        }
        let clusters: Vec<usize> = self.clusters.keys().copied().collect();
        for at in clusters {
            if let Some(cluster) = self.clusters.get_mut(&at) {
                cluster.open = open;
                self.draw_cluster(at);
            }
        }
    }

    /// The cluster `at` drawn again where it is; whether it is still in
    /// the conversation.
    fn draw_cluster(&mut self, at: usize) -> bool {
        let (lines, marks) = self.cluster_lines(at);
        self.flush();
        (self.screen).replace_tagged(screen::Tag::Group(at as u64), lines, marks)
    }

    /// The cluster the message between agents at `node` is in: the one
    /// drawn, when a page drawn again put it in a newer one too.
    fn cluster_of(&self, node: i64) -> Option<usize> {
        let holding = (self.clusters.iter().rev())
            .filter(|(_, cluster)| cluster.messages.iter().any(|m| m.node == node))
            .map(|(at, _)| *at);
        let mut newest = None;
        for at in holding {
            if self.kept(screen::Tag::Group(at as u64)) {
                return Some(at);
            }
            newest.get_or_insert(at);
        }
        newest
    }

    /// Commit a quiet row of what went on around the work (a fold of
    /// calls, a note's marker, messages between agents) right under the
    /// one before it when that was one too, so they read as one block;
    /// where its first line is among the lines waiting.
    fn commit_quiet(&mut self, lines: Vec<Line<'static>>) -> usize {
        let count = lines.len();
        let gap = self.stacked != Some(self.rows_drawn);
        self.commit_after(lines, gap);
        self.stacked = Some(self.rows_drawn);
        self.pending.len() - count
    }

    /// The bot's words at `node`, as replayed: their last row tagged,
    /// and the person's reactions to them right under it.
    fn commit_said(&mut self, node: i64, lines: Vec<Line<'static>>) {
        let count = lines.len();
        self.commit(lines);
        self.mark_first(count);
        self.mark_last(screen::Mark::Tag(screen::Tag::End(node)));
        if let Some(row) = self.reacted_row(node) {
            self.commit_after(vec![row], false);
            self.mark_last(screen::Mark::Tag(screen::Tag::Reactions(node)));
        }
    }

    /// The welcome over the newest page just read, known to start the
    /// conversation only once it is.
    fn welcome_above(&mut self) {
        let (mut welcome, eyes) = self.welcome();
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
            if let Some(eyes) = eyes {
                self.marks
                    .push((eyes, screen::Mark::Tag(screen::Tag::Eyes)));
            }
        }
    }

    /// The welcome written under what is waiting, its eyes' line marked.
    fn commit_welcome(&mut self) {
        let (welcome, eyes) = self.welcome();
        let count = welcome.len();
        self.commit(welcome);
        if let Some(eyes) = eyes {
            let at = self.pending.len() - count + eyes;
            self.marks.push((at, screen::Mark::Tag(screen::Tag::Eyes)));
        }
    }

    /// The welcome, when this window has one and draws the conversation
    /// from its first item: none above a page with older ones before it,
    /// and none in zen, which draws only what is said. With its portrait,
    /// the line the eyes are on.
    fn welcome(&self) -> (Vec<Line<'static>>, Option<usize>) {
        match self.welcome && self.older.is_none() && self.zen.is_none() {
            true => banner::welcome(&tilde(&self.workspace), &self.model, self.width),
            false => (Vec::new(), None),
        }
    }

    /// Commit a finished cell, a blank line apart from the one before.
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
    /// The first of the last `count` lines waiting marked as a message's
    /// first row.
    fn mark_first(&mut self, count: usize) {
        if count > 0 && self.pending.len() >= count {
            (self.marks).push((self.pending.len() - count, screen::Mark::Opens));
        }
    }

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

    /// The words drawn that may name agents, asked about off the loop
    /// that draws and reads keys, a batch at a time: once one does, the
    /// links on screen are found again, so its name opens it.
    async fn read_names(&mut self) {
        if self.naming.as_ref().is_some_and(|job| job.is_finished()) {
            let job = self.naming.take().expect("a lookup");
            let mut found = false;
            for (word, agent) in job.await.unwrap_or_default() {
                let agent = agent && word != self.bot;
                found |= agent;
                self.names.get_mut().learn(word, agent);
            }
            if found {
                self.screen.relink();
                if let Some(pane) = &mut self.pane {
                    pane.screen.relink();
                }
            }
        }
        if self.naming.is_some() {
            return;
        }
        let wanted = self.names.get_mut().wanted();
        if wanted.is_empty() {
            return;
        }
        let (client, woken) = (self.client.clone(), self.woken.clone());
        self.naming = Some(tokio::spawn(async move {
            let mut known = Vec::new();
            for word in wanted {
                let asked = client.request("bots", json!({"name": word, "limit": 1}));
                // Unanswered, it is not taken for a name for a while.
                let agent = match tokio::time::timeout(NAME_LOOKUP, asked).await {
                    Ok(Ok(page)) => page["bots"][0]["name"] == word.as_str(),
                    _ => false,
                };
                known.push((word, agent));
            }
            woken.notify_one();
            known
        }));
    }

    /// The other bots' notes the margin shows, read again every few
    /// seconds while their bots are there: what each shows is kept once
    /// its bot is gone.
    async fn read_references(&mut self) {
        let shown = self.margin.references();
        (self.references).retain(|(bot, id), _| shown.iter().any(|(_, b, i)| b == bot && i == id));
        if shown.is_empty()
            || (self.references_read).is_some_and(|at| at.elapsed() < Duration::from_secs(3))
        {
            return;
        }
        self.references_read = Some(Instant::now());
        let mut read = std::collections::HashSet::new();
        for (note, bot, id) in shown {
            let key = (bot, id);
            let mut reference = self.references.remove(&key).unwrap_or_default();
            if read.insert(key.clone()) && !reference.gone {
                self.read_reference(&key.0, &key.1, &mut reference).await;
            }
            if let Some((title, body, folder)) = reference.shown() {
                self.margin.resolved(&note, title, body, folder);
            }
            self.references.insert(key, reference);
        }
    }

    /// The tasks this agent started with a turn running, read every few
    /// seconds where no windows for the project's agents say it.
    async fn read_running_tasks(&mut self) {
        if self.fleet.is_some()
            || (self.tasks_running_read).is_some_and(|at| at.elapsed() < Duration::from_secs(3))
        {
            return;
        }
        self.tasks_running_read = Some(Instant::now());
        // Every page of them, or the count stays as it was.
        let mut running = std::collections::HashSet::new();
        let mut after = Value::Null;
        loop {
            let asked = json!({"active": true, "limit": 256, "after": after});
            let Ok(page) = self.client.request("bots", asked).await else {
                return;
            };
            running.extend(
                (page["bots"].as_array().into_iter().flatten())
                    // Its side chats lapse, and are no tasks.
                    .filter(|bot| bot["created_by"] == self.bot.as_str())
                    .filter(|bot| bot["expire_ms"].is_null())
                    .filter_map(|bot| bot["name"].as_str().map(str::to_owned)),
            );
            after = page["next_after"].clone();
            if !after.is_string() {
                break;
            }
        }
        self.tasks_running = Some(running);
    }

    /// What `bot` did since `reference` was last read of its note `id`,
    /// and, when that note shows another bot's, what that one did.
    async fn read_reference(&self, bot: &str, id: &str, reference: &mut margin::Reference) {
        self.read_history(bot, id, reference).await;
        // Taken away there, or the bot gone, what it showed stays as
        // last read.
        if reference.removed() || reference.gone {
            return;
        }
        let Some((through, note)) = reference.shows() else {
            reference.through = None;
            return;
        };
        // Read again where it was read from, while it shows the same one.
        let mut shown = (reference.through.take())
            .filter(|shown| shown.names.as_ref() == Some(&(through.clone(), note.clone())))
            .unwrap_or_default();
        shown.names = Some((through.clone(), note.clone()));
        if !shown.gone {
            self.read_history(&through, &note, &mut shown).await;
        }
        reference.through = Some(shown);
    }

    /// What `bot` did since `reference` was last read of its note `id`.
    async fn read_history(&self, bot: &str, id: &str, reference: &mut margin::Reference) {
        // The bot first read, every time: another given its name once it
        // is gone is not the one shown, and what was read of it stays.
        let asked = json!({"bot": bot, "bot_id": reference.bot_id});
        match self.client.request("inspect", asked).await {
            Ok(record) => {
                reference.bot_id = record["bot_id"].as_i64();
                if reference.workspace.is_none() {
                    reference.workspace = record["workspace"].as_str().map(str::to_owned);
                }
            }
            Err(error) => {
                reference.gone = error.code == "bot_not_found";
                return;
            }
        }
        // The newest nodes first, read back as far as the note's start,
        // then only what came after them.
        let mut pages: Vec<Vec<Value>> = Vec::new();
        let mut newest = None;
        let mut from: Option<i64> = None;
        while pages.len() < REFERENCE_PAGES {
            let asked = match (reference.node, from) {
                (Some(node), _) => {
                    json!({"bot": bot, "limit": 400, "min_node": node + 1, "oldest_first": true})
                }
                (None, Some(from)) => json!({"bot": bot, "limit": 400, "from": from}),
                (None, None) => json!({"bot": bot, "limit": 400}),
            };
            let page = match self.client.request("history_nodes", asked).await {
                Ok(page) => page,
                Err(error) => {
                    reference.gone = error.code == "bot_not_found";
                    return;
                }
            };
            let mut nodes: Vec<i64> = (page["nodes"].as_array().into_iter().flatten())
                .filter_map(|node| node["node"].as_i64())
                .collect();
            nodes.sort_unstable();
            let Some(&last) = nodes.last() else {
                break;
            };
            newest = newest.max(Some(last));
            // In the nodes' order, and each node's as it holds them; a read
            // cut short is asked again from where it was, next time.
            let short = self.short_read.replace(false);
            let items: Vec<Value> = (self.items_of(bot, &nodes).await.into_iter())
                .map(|(_, item)| item)
                .collect();
            if self.short_read.replace(short) {
                return;
            }
            let started = margin::Reference::starts(id, &items);
            pages.push(items);
            from = page["next_from"].as_i64();
            if reference.node.is_some() || started || from.is_none() {
                break;
            }
        }
        if reference.node.is_none() {
            reference.whole = from.is_none();
        }
        for items in pages.iter().rev() {
            reference.read(id, items);
        }
        if newest.is_some() {
            reference.node = newest;
        }
        if let Some(path) = reference.file() {
            reference.file_read(margin::read(&path).ok());
        }
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
            // The daemon reads at most `HISTORY_NODES` nodes a call.
            let ask = &rest[..rest.len().min(HISTORY_NODES)];
            let Ok(read) = self
                .client
                .request("history_items", json!({"bot": bot, "nodes": ask}))
                .await
            else {
                self.short_read.set(true);
                break;
            };
            let entries = read["items"].as_array().cloned().unwrap_or_default();
            if entries.is_empty() {
                self.short_read.set(true);
                break;
            }
            rest = &rest[entries.len().min(rest.len())..];
            items.extend(entries.iter().flat_map(|entry| {
                let node = entry["node"].as_i64().unwrap_or(0);
                // Who sent a prompt, when another bot did, a client from
                // outside (the commons, an automation) or another person.
                let from = transcript::sender(
                    entry["from"]["bot"].as_str(),
                    entry["origin"].as_str(),
                    entry["person"].as_str(),
                );
                let quotes = entry.get("quotes").cloned();
                responses(&entry["item"]).into_iter().map(move |mut item| {
                    if let Some(from) = from.as_ref().filter(|_| item["role"] == "user") {
                        item["from"] = json!(from);
                        // A person, not an agent or a client, wrote it.
                        if entry["person"].is_string() && entry["from"].is_null() {
                            item["person"] = json!(true);
                        }
                    }
                    // Where its quoted words are among its parts, and who
                    // wrote them.
                    if let Some(quotes) = quotes.as_ref().filter(|_| item["role"] == "user") {
                        item["quotes"] = quotes.clone();
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
        // What changed the margin while no window drew it: the calls and
        // closes between where it was brought up to and this page.
        let mut caught_up = Some(HashMap::new());
        if !every
            && let (Some(through), Some(&first)) = (self.margin.through(), nodes.first())
            && first > through + 1
        {
            caught_up = self.margin_since(through, self.older).await;
        }
        // Drawn from the start (attaching, or a repaint), the bot's
        // reactions are read again with the rest; after a gap, those it
        // made before the gap are still held.
        // The open questions are those the replay asks and does not answer.
        let from_start = self.seen_node == 0 || after < self.seen_node;
        if from_start {
            self.bot_reactions.clear();
            self.react_calls.clear();
            self.answering = None;
            self.asks.clear();
            self.aside.clear();
            self.sent_answers.clear();
        }
        self.seen_node = nodes.last().copied().unwrap_or(self.seen_node);
        if let Some(&first) = nodes.first() {
            self.first_node = Some(self.first_node.map_or(first, |n| n.min(first)));
        }
        let unpaired = (self.draw_nodes(&nodes, &turns, false, Vec::new(), caught_up)).await;
        if from_start {
            self.unpaired = unpaired;
        }
    }

    /// The margin brought up from node `through` to the page of history
    /// drawn, which starts before `from`: only its `margin` calls that ran
    /// and the person's closes are read again, oldest first, whatever the
    /// conversation shows. The calls whose outputs are on the page are
    /// given back; read short, it stays where it was and gives back none.
    async fn margin_since(
        &mut self,
        through: i64,
        mut from: Option<i64>,
    ) -> Option<HashMap<String, Value>> {
        let mut nodes: Vec<i64> = Vec::new();
        while let Some(at) = from {
            let asked = json!({"bot": self.bot, "limit": HISTORY_ITEMS,
                "min_node": through + 1, "from": at});
            let Ok(page) = self.client.request("history_nodes", asked).await else {
                return None;
            };
            nodes.extend(
                (page["nodes"].as_array().into_iter().flatten())
                    .filter_map(|n| n["node"].as_i64())
                    .filter(|node| *node > through),
            );
            from = page["next_from"].as_i64();
        }
        nodes.sort_unstable();
        nodes.dedup();
        let mut calls: HashMap<String, Value> = HashMap::new();
        for chunk in nodes.chunks(100) {
            let short = self.short_read.replace(false);
            let items = self.items_at(chunk).await;
            if self.short_read.replace(short) {
                return None;
            }
            for (node, item) in items {
                let id = item["call_id"].as_str().unwrap_or("").to_owned();
                match (item["type"].as_str(), item["role"].as_str()) {
                    (_, Some("user")) => {
                        self.note_closed(node, &text(&item));
                    }
                    (Some("function_call"), _) if item["name"] == margin::TOOL => {
                        let arguments = (item["arguments"].as_str())
                            .and_then(|a| serde_json::from_str(a).ok())
                            .unwrap_or(Value::Null);
                        calls.insert(id, arguments);
                    }
                    (Some("function_call_output"), _) => {
                        if let Some(arguments) = calls.remove(&id)
                            && !output(&item).1
                            && denial(&item).is_none()
                            && !reactions::refused(&item)
                        {
                            self.margin.apply(&id, &arguments, None, None);
                        }
                    }
                    _ => {}
                }
            }
        }
        if let Some(&last) = nodes.last() {
            self.margin.advance(last);
        }
        Some(calls)
    }

    /// The items at `nodes`, then `after` (what a newer page's calls
    /// returned, its calls being here), drawn as they were live; what
    /// returned from calls before them is given back. Those `older` than
    /// any drawn change nothing but what is drawn: the questions, replies
    /// to react to and calls still running are the newer items' to say.
    /// The margin is brought up to them with the `margin` calls its catch-up
    /// left running, unless that fell short (`None`) or these read short.
    async fn draw_nodes(
        &mut self,
        nodes: &[i64],
        turns: &HashMap<i64, i64>,
        older: bool,
        after: Vec<(i64, Value)>,
        mut caught_up: Option<HashMap<String, Value>>,
    ) -> Vec<(i64, Value)> {
        let short = self.short_read.replace(false);
        let mut calls: HashMap<String, (String, Value)> = HashMap::new();
        let mut unpaired = Vec::new();
        // The bot's words, quiet if a call follows them, as they were live.
        let mut said: Option<(i64, Vec<Line<'static>>)> = None;
        // Whether the turn the items are in was started by something other
        // than the person, as `elsewhere` is live: its first message says,
        // and words of the person's in it make it theirs.
        let mut aside = !older && self.elsewhere;
        let mut began = std::collections::HashSet::new();
        // A page that starts inside a turn lacks its first message: in zen,
        // the turn's own prompt says who started it.
        if self.zen.is_some()
            && let Some(&turn) = nodes.first().and_then(|node| turns.get(node))
            && let Ok(read) = (self.client)
                .request(
                    "prompts",
                    json!({"bot": self.bot, "turn": turn, "bytes": 4096}),
                )
                .await
            && let Some(prompts) = read["prompts"].as_array()
        {
            let relays = |prompt: &Value| {
                let from = transcript::sender(
                    prompt["from"]["bot"].as_str(),
                    prompt["origin"].as_str(),
                    prompt["person"].as_str(),
                );
                transcript::relays(prompt["text"].as_str().unwrap_or(""), from.as_deref())
            };
            // Started elsewhere, unless the person steered it: their words
            // make it theirs, wherever the page starts.
            aside = prompts.first().is_some_and(relays) && prompts.iter().all(relays);
            began.insert(turn);
        }
        // A chunk read at a time, what it held let go before the next.
        let mut chunks = nodes.chunks(100);
        let mut after = Some(after);
        loop {
            let items = match chunks.next() {
                Some(chunk) => self.items_at(chunk).await,
                None => match after.take() {
                    Some(after) => after,
                    None => break,
                },
            };
            for (node, item) in items {
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
                        self.note_closed(node, &text(&item));
                    }
                    (_, Some("user")) => {
                        let turn = turns.get(&node).copied();
                        let first = turn.is_none_or(|turn| began.insert(turn));
                        match transcript::relays(&text(&item), item["from"].as_str()) {
                            false => aside = false,
                            true if first => aside = true,
                            true => {}
                        }
                        // The bot's reactions to the message before, as they
                        // were drawn live.
                        self.draw_bot_reactions();
                        if !older {
                            self.answered(&text(&item), item["from"].as_str());
                        }
                        self.land(
                            node,
                            (&text(&item), &quotes_of(&item)),
                            item["from"].as_str(),
                            item["person"] == true,
                            older,
                        );
                    }
                    (_, Some("assistant")) => {
                        let reply = text(&item);
                        if !reply.is_empty() && !(aside && self.zen.is_some()) {
                            let lines = transcript::reply(
                                &reply,
                                self.prose(),
                                std::path::Path::new(&self.workspace),
                            );
                            said = Some((node, lines));
                            if !older {
                                self.saw_reply(node, &reply);
                                self.last_reply = reply;
                            }
                        }
                    }
                    // Counted once it is known to have been taken.
                    // One naming another agent is drawn in that agent's window.
                    (Some("function_call"), _) if item["name"] == reactions::TOOL => {
                        if let Some(reaction) = own_reaction(&item) {
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
                        let call = match older {
                            true => calls.remove(id),
                            false => calls.remove(id).or_else(|| self.calls.remove(id)),
                        };
                        let Some((name, arguments)) = call else {
                            // A note the catch-up saw start ends here.
                            if !older
                                && !self.margin.holds(node)
                                && let Some(arguments) =
                                    caught_up.as_mut().and_then(|c| c.remove(id))
                                && !output(&item).1
                                && denial(&item).is_none()
                                && !reactions::refused(&item)
                            {
                                self.note_call(id, &arguments, turns.get(&node).copied());
                            }
                            unpaired.push((node, item));
                            continue;
                        };
                        let (output, mut failed) = output(&item);
                        // A note as the call that ran left it; an older
                        // page's calls are in the margin kept already.
                        if name == margin::TOOL
                            && !older
                            && !self.margin.holds(node)
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
                                if self.zen.is_none() {
                                    self.commit_denied(&name, &arguments, &reason);
                                }
                            }
                            None => self.commit_call(id, &name, &arguments, &output, failed),
                        }
                        if older {
                            continue;
                        }
                        if name == resolved::TOOL && !failed && !reactions::refused(&item) {
                            self.resolve(&arguments, id, turns.get(&node).copied(), false)
                                .await;
                        }
                        if name == asks::TOOL && !failed && !reactions::refused(&item) {
                            self.asked(id, &arguments);
                        }
                    }
                    _ => {}
                }
            }
        }
        if let Some((at, lines)) = said {
            self.commit_said(at, lines);
        }
        if !older {
            self.elsewhere = aside;
        }
        let cut = self.short_read.get();
        self.short_read.set(short || cut);
        if older {
            self.draw_bot_reactions();
            return unpaired;
        }
        // The margin holds what these nodes did to it.
        if let Some(&last) = nodes.iter().max()
            && caught_up.is_some()
            && !cut
        {
            self.margin.advance(last);
        }
        // A running turn's reactions are drawn when it ends.
        if self.running.is_none() {
            self.draw_bot_reactions();
        }
        // A call still running finishes live, after the history.
        self.calls.extend(calls);
        unpaired
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
            .flat_map(|(turn, prompt)| {
                let mut shown: Vec<Line<'static>> = match user_shell::read(prompt) {
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
                if self.queued.contains(turn) {
                    shown.push(Line::from("  queued for after this turn").dim().italic());
                }
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
            let lead = match open.lines == 0 && !transcript::heading(&open.partial) {
                true => "• ",
                false => "  ",
            };
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

    /// The words streamed as `item` end where the next item begins: written
    /// as a reply of their own, and found again once their message names
    /// its node.
    fn stream_ahead(&mut self, item: u64) {
        let (drawn, spilled) = (
            self.rows_drawn,
            self.live.as_ref().is_some_and(|l| l.spilled),
        );
        self.end_live(Words::Reply);
        if self.rows_drawn > drawn || spilled {
            self.mark_last(screen::Mark::Tag(screen::Tag::Streamed(item)));
        }
        self.ahead.push_back(item);
        self.said.clear();
        self.replay = None;
        self.streamed = false;
    }

    /// The words streamed as `item` are the message kept at `node`.
    fn streamed_at(&mut self, item: u64, node: i64) {
        let (from, to) = (screen::Tag::Streamed(item), screen::Tag::End(node));
        match (self.marks.iter_mut()).find(|(_, mark)| *mark == screen::Mark::Tag(from)) {
            Some((_, mark)) => *mark = screen::Mark::Tag(to),
            None => {
                self.screen.retag(from, to);
            }
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
        if !live.spilled {
            self.marks.push((self.pending.len(), screen::Mark::Opens));
        }
        self.rows_drawn += live.rows.len();
        self.pending.extend(match words {
            Words::Step => transcript::quiet(live.rows),
            Words::Reply => live.rows,
        });
        self.any_history = true;
        if let Some(node) = live.node {
            self.mark_last(screen::Mark::Tag(screen::Tag::End(node)));
            // The person's reactions made while it was being written.
            if let Some(row) = self.reacted_row(node) {
                self.commit_after(vec![row], false);
                self.mark_last(screen::Mark::Tag(screen::Tag::Reactions(node)));
            }
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
            if !live.spilled {
                self.marks.push((self.pending.len(), screen::Mark::Opens));
            }
            live.spilled = true;
            self.rows_drawn += live.rows.len();
            self.pending.append(&mut live.rows);
            self.any_history = true;
        }
    }

    async fn daemon(&mut self, event: Value) {
        let kind = event["event"].as_str().unwrap_or("");
        // Every agent's, not this one's: told to every client.
        if matches!(kind, "paused" | "resumed") {
            self.paused = (kind == "paused").then(|| event["pause"].clone());
            return;
        }
        if !self.following {
            if kind == "follow_live" {
                self.following = true;
            }
            return;
        }
        if event["bot"] != self.bot.as_str() {
            return;
        }
        // Zen set in another window of this agent: its message starts or
        // joins a turn, so a turn is when to look.
        if matches!(kind, "accepted" | "steered") {
            self.zen_taken_up();
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
                    dropped.push("permissions");
                }
                if changed("instructions") && self.instruct.take().is_some() {
                    dropped.push("instructions");
                }
                if changed("speed") && self.pace.take().is_some() {
                    dropped.push("speed");
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
                self.live_item = None;
                self.ahead.clear();
                self.said.clear();
                self.thought.clear();
                self.replay = None;
                self.running = Some(Running {
                    turn: turn.unwrap_or(0),
                    started: Instant::now(),
                    phase: "Working".into(),
                    note: from_lookout(data),
                    effort: data["effort"].as_str().map(str::to_owned),
                });
                // What this turn runs on and in: the bot may have moved, or
                // a client chosen another model for it.
                if let Some(workspace) = data["workspace"].as_str() {
                    self.workspace = workspace.to_owned();
                }
                if let Some(model) = data["model"].as_str() {
                    self.model = model.to_owned();
                }
                status_report("working", &self.bot);
                // A turn the history drew still answers the message it
                // drew, with what the bot reacted to it with so far.
                if !drawn {
                    self.bot_reactions.clear();
                    self.answering = None;
                    self.elsewhere = from_lookout(data);
                }
                if !drawn
                    && let Some(node) = data["node"].as_i64()
                    && let Some(item) = self.items(&[node]).await.first()
                    && !self.note_closed(node, &text(item))
                    && !quiet_message(&text(item))
                {
                    self.elsewhere |= transcript::relays(&text(item), item["from"].as_str());
                    self.land(
                        node,
                        (&text(item), &quotes_of(item)),
                        item["from"].as_str(),
                        item["person"] == true,
                        false,
                    );
                    self.answered(&text(item), item["from"].as_str());
                }
            }
            "steered" => {
                self.staged
                    .retain(|(staged, _)| Some(*staged) != data["steer"].as_i64());
                // A reaction was drawn when it was sent.
                if let Some(node) = data["node"].as_i64()
                    && let Some(item) = self.items(&[node]).await.first()
                    && !self.note_closed(node, &text(item))
                    && !quiet_message(&text(item))
                {
                    self.end_live(Words::Reply);
                    // What the bot reacted with so far answered the
                    // message before this one.
                    self.draw_bot_reactions();
                    // The person's words make the turn theirs.
                    if !transcript::relays(&text(item), item["from"].as_str()) {
                        self.elsewhere = false;
                    }
                    self.land(
                        node,
                        (&text(item), &quotes_of(item)),
                        item["from"].as_str(),
                        item["person"] == true,
                        false,
                    );
                    self.answered(&text(item), item["from"].as_str());
                }
            }
            "queued" => self.notice = Some(format!("queued behind turn {}", data["behind"])),
            // Shown as a clip in the footer until a call takes it in.
            "attached" if turn.is_some() && turn != self.reaction_turn => {
                self.attached.extend(turn);
            }
            "text_delta" => {
                self.thought.clear();
                // Words nobody reads in zen.
                if self.zen_hides() {
                    return;
                }
                // Each item the bot streams is a message of its own, as
                // Codex draws them.
                if let Some(item) = event["item"].as_u64() {
                    if self.live_item.is_some_and(|live| live != item) && self.streamed {
                        self.stream_ahead(self.live_item.unwrap_or(item));
                    }
                    self.live_item = Some(item);
                }
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
                // Unseen in zen, so never what a reaction, /copy or
                // /reply takes.
                if self.zen_hides() {
                    return;
                }
                if let Some(item) = &item {
                    self.last_reply = text(item);
                    if let Some(node) = data["node"].as_i64() {
                        self.saw_reply(node, &text(item));
                    }
                }
                // Words written when the next item began are this message.
                if let (Some(item), Some(node)) = (&item, data["node"].as_i64())
                    && !text(item).is_empty()
                    && let Some(streamed) = self.ahead.pop_front()
                {
                    self.streamed_at(streamed, node);
                    return;
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
                // Messages written as the next began may come again, or
                // not: what was shown stays, and the call's own is drawn.
                // The retry numbers its items afresh, so its first is still
                // the message being streamed.
                self.live_item = None;
                if !self.ahead.is_empty() {
                    self.end_live(Words::Reply);
                    self.ahead.clear();
                    self.streamed = false;
                    self.said.clear();
                    self.notice = Some("the model retried; its reply may repeat".into());
                }
                if !self.said.is_empty() {
                    self.replay = Some(std::mem::take(&mut self.said));
                }
                if let Some(running) = &mut self.running {
                    running.phase = "Retrying".into();
                }
            }
            "tool_started" if data["name"] == reactions::TOOL => {
                // Counted once it completes, and drawn when the turn ends,
                // so one taken off again is never drawn; one naming another
                // agent is drawn in that agent's window.
                if let Some(reaction) = data["arguments"]
                    .as_str()
                    .and_then(|a| serde_json::from_str::<Value>(a).ok())
                    .filter(|arguments| reactions::target(arguments).is_none())
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
                    if self.zen.is_none() {
                        self.commit_denied(&name, &arguments, &reason);
                    }
                    // The approver's, not the person's: `/retry` may
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
            "approval_requested" | "approval_asked" | "step_decided" => {
                self.approvals_for(turn).await
            }
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
                    self.window = window_of(&event).or(self.window);
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
                let quiet = ((note || empty) && !words && ended) || self.zen_hides();
                // A turn that ended well puts its reply under the line, apart
                // from the steps above; one written as it came, or one
                // that failed, keeps the line under it.
                match self.live.take() {
                    _ if (only_reacted || quiet) && ended => {}
                    // In zen a turn the person did not start ends unseen.
                    _ if self.zen_hides() => {}
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
                // Zen the agent set, or put in its own words, as the person
                // asked: the turn's end is when to look.
                self.zen_taken_up();
                if status == "completed" && !quiet && turn.is_some() && turn == self.yours {
                    self.suggest();
                }
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
        // A side chat forked from a lead resolves that lead's tasks.
        let side = (self.options.side.is_some())
            .then(|| side_source(&self.bot))
            .flatten()
            .and_then(project::lead_project);
        let Some(project) = self.project.as_deref().or(side).map(str::to_owned) else {
            return;
        };
        let project = &project;
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
            usage: None,
            commons: None,
            sky: None,
            picked: None,
            pick: None,
        });
        // Picked already, the agent named last in what shows: Enter opens
        // the one the conversation just pointed to.
        if let Some(named) = self.screen.last_agent()
            && let Some(watch) = &mut self.watch
        {
            watch.pick = card_bots(watch, self.width.max(40), &self.bot)
                .iter()
                .position(|bot| *bot == named);
        }
        self.open_pane(pane::Pane::card("/status", PICK_KEYS));
    }

    /// What runs for the project's agents with no one typing:
    /// `/background` in the pane, as `/status` is, until Esc.
    fn background(&mut self) {
        // What failed is read now: the footer stops counting it.
        self.failures.1 = self.failures.0.iter().cloned().collect();
        self.failed = 0;
        let scope = self.scope();
        let project = scope.project.clone();
        let view = background::View::open(
            scope,
            self.options.store.clone(),
            self.client.store().map(str::to_owned),
            self.bot.clone(),
            self.turns.clone(),
        );
        self.watch = Some(Watch {
            project,
            rows: Vec::new(),
            read: Instant::now(),
            lookout: String::new(),
            resolved: false,
            background: Some(view),
            usage: None,
            commons: None,
            sky: None,
            picked: None,
            pick: None,
        });
        self.open_pane(pane::Pane::card("/background", STATUS_KEYS));
    }

    /// The tokens the project's agents spent, or every agent's, over
    /// time: `/usage` in the pane, as `/status` is, until Esc.
    async fn usage(&mut self) {
        let scope = self.scope();
        let project = scope.project.clone();
        let mut view = usage::View::new(scope);
        self.read_usage(&mut view).await;
        self.watch = Some(Watch {
            project,
            rows: Vec::new(),
            read: Instant::now(),
            lookout: String::new(),
            resolved: false,
            background: None,
            usage: Some(view),
            commons: None,
            sky: None,
            picked: None,
            pick: None,
        });
        self.open_pane(pane::Pane::card("/usage", STATUS_KEYS));
    }

    /// What the agents of every project share: `/commons` in the pane,
    /// as `/status` is, until Esc.
    fn commons(&mut self) {
        let Some(home) = std::env::var_os("HOME").map(PathBuf::from) else {
            self.notice = Some("no HOME, so no commons".into());
            return;
        };
        self.watch = Some(Watch {
            project: self.scope().project,
            rows: Vec::new(),
            read: Instant::now(),
            lookout: String::new(),
            resolved: false,
            background: None,
            usage: None,
            commons: Some(commons::View::open(&home)),
            sky: None,
            picked: None,
            pick: None,
        });
        self.open_pane(pane::Pane::card("/commons", STATUS_KEYS));
    }

    /// Every agent the daemon keeps, lit up live as they talk: `/sky` in
    /// the pane, as `/status` is, until Esc. Its connection is its own.
    fn sky(&mut self) {
        self.watch = Some(Watch {
            project: self.scope().project,
            rows: Vec::new(),
            read: Instant::now(),
            lookout: String::new(),
            resolved: false,
            background: None,
            usage: None,
            commons: None,
            sky: Some(sky::View::open(self.options.socket.clone())),
            picked: None,
            pick: None,
        });
        self.open_pane(pane::Pane::card("/sky", STATUS_KEYS));
    }

    /// `view` read from the daemon again.
    async fn read_usage(&self, view: &mut usage::View) {
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_millis() as i64;
        let (query, window) = view.query(now);
        let read = self.client.request("usage", query).await;
        view.loaded(window, read.map_err(|error| error.to_string()));
    }

    async fn reread(&mut self) {
        // `/sky` follows the daemon live and reads nothing again.
        if self.watch.as_ref().is_some_and(|w| w.sky.is_some()) {
            return;
        }
        if let Some(watch) = &mut self.watch
            && let Some(view) = &mut watch.background
        {
            view.reload();
            watch.read = Instant::now();
            return;
        }
        if let Some(watch) = &mut self.watch
            && let Some(view) = &mut watch.commons
        {
            view.reload();
            watch.read = Instant::now();
            return;
        }
        if let Some(mut view) = self.watch.as_mut().and_then(|w| w.usage.take()) {
            if view.due() {
                self.read_usage(&mut view).await;
            }
            if let Some(watch) = &mut self.watch {
                watch.usage = Some(view);
                watch.read = Instant::now();
            }
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
    /// seconds, and what failed of what `/background` lists: files read,
    /// and the daemon asked at once how the turns fires sent ended.
    async fn count_behind(&mut self) {
        if self
            .behind
            .1
            .is_some_and(|at| at.elapsed() < Duration::from_secs(10))
            || self.options.side.is_some()
        {
            return;
        }
        let scope = self.scope();
        let read = |turns: &HashMap<String, Value>| {
            background::read_around(&self.options.store, self.client.store(), turns, &scope)
        };
        let mut items = read(&self.turns);
        let unended = background::unended(&items, &self.turns);
        if !unended.is_empty() {
            let ended = trigger::ended(&self.client, unended).await;
            if !ended.is_empty() {
                self.turns.extend(ended);
                items = read(&self.turns);
            }
        }
        // A trigger that ended goes once its last turn did well, also one
        // that ended after its turn did, as a `--reply-to` fire passing on
        // the answer.
        if !self.turns.is_empty()
            && let (Some(store), Ok(places)) = (self.client.store(), trigger::Places::home())
            && trigger::forget_done(&places, store, &self.turns) > 0
        {
            items = read(&self.turns);
        }
        let fired: std::collections::HashSet<&String> =
            items.iter().filter_map(|i| i.handle.as_ref()).collect();
        self.turns.retain(|handle, _| fired.contains(handle));
        items.retain(|item| background::shown(&scope, item));
        self.behind = (background::count(&items, &self.bot), Some(Instant::now()));
        self.count_failures(background::failures(&items));
        if let Some(view) = (self.watch.as_mut()).and_then(|w| w.background.as_mut()) {
            view.set_turns(&self.turns);
        }
    }

    /// The failures `/background` lists now: the footer counts those the
    /// person has not seen there.
    fn count_failures(&mut self, failures: Vec<String>) {
        let seen = &self.failures.1;
        self.failed = failures.iter().filter(|f| !seen.contains(*f)).count();
        self.failures.0 = failures;
    }

    /// What the lookout does for this window's lead.
    fn lookout_line(&self) -> String {
        let lookout = match &self.project {
            Some(_) => lookout::status(&self.options.store, &self.bot),
            None => String::new(),
        };
        tool_host::with_status(lookout, &self.options.store)
    }

    /// The agents `/status` and `/background` cover: the project's, and
    /// its lead; a lone agent's only.
    fn scope(&self) -> background::Scope {
        let (project, lead) = match &self.project {
            Some(project) => (project.clone(), self.bot.clone()),
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
        let behind = background::read(
            &self.options.store,
            self.client.store(),
            &self.turns,
            |bot| lookout::subs::matches(&pattern, bot),
        );
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
        // With other leads in the folder, their tasks are theirs: each
        // lead shows as a row of its own.
        let listed = page["bots"].as_array().cloned().unwrap_or_default();
        let shared = lead != format!("{project}.lead")
            || (listed.iter()).any(|bot| {
                let name = bot["name"].as_str().unwrap_or("");
                name != lead && project::lead_project(name) == Some(project.as_str())
            });
        let mine = (shared && project::lead_project(&lead) == Some(project.as_str()))
            .then(|| lineage_of(&listed, &lead, lead == format!("{project}.lead")));
        // Side chats are not the project's agents.
        for bot in page["bots"].as_array().into_iter().flatten() {
            let Some(name) = bot["name"].as_str().filter(|_| bot["expire_ms"].is_null()) else {
                continue;
            };
            if let Some(mine) = &mine
                && name != lead
                && !mine.contains(name)
                && project::lead_project(name) != Some(project.as_str())
            {
                continue;
            }
            let turns = self
                .client
                .request("turns", json!({"bot": name, "newest": true, "limit": 8}))
                .await
                .ok()
                .and_then(|page| page["turns"].as_array().cloned())
                .unwrap_or_default();
            let turn = turns.first().cloned().unwrap_or(Value::Null);
            // A message steered into a running turn is recorded as a turn
            // of its own that never ran: the time is the one it joined. One
            // attached to the next call has not run either.
            let ran = turns
                .iter()
                .find(|turn| !matches!(turn["status"].as_str(), Some("steered" | "attached")))
                .cloned()
                .unwrap_or(Value::Null);
            let running = !bot["running_turn"].is_null();
            let ran = match running {
                true => ran,
                false => self.with_error(ran).await,
            };
            let state = match (
                running,
                if running { &turn } else { &ran }["status"].as_str(),
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
                // One with a goal says whether that turn met it.
                (false, Some("completed" | "steered")) => {
                    lookout::goals::standing(&self.options.store, name, ran["turn"].as_i64())
                        .unwrap_or("done")
                        .into()
                }
                (false, Some("failed")) if ran["error"] == EMPTY => started::QUIET.into(),
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
                bot: name.to_owned(),
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
        let entries = read["items"].as_array().cloned().unwrap_or_default();
        let items: Vec<Value> = entries.iter().map(|entry| entry["item"].clone()).collect();
        let pr = items
            .iter()
            .find(|item| item["role"] == "assistant")
            .and_then(|item| status::pull_request(&text(item)));
        // Who sent each prompt, as `items_of` keeps it: another agent's
        // message sets no question aside.
        let flat: Vec<Value> = entries
            .iter()
            .flat_map(|entry| {
                let from = transcript::sender(
                    entry["from"]["bot"].as_str(),
                    entry["origin"].as_str(),
                    entry["person"].as_str(),
                );
                responses(&entry["item"])
                    .into_iter()
                    .rev()
                    .map(move |mut item| {
                        if let Some(from) = from.as_ref().filter(|_| item["role"] == "user") {
                            item["from"] = json!(from);
                        }
                        item
                    })
            })
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

    /// Answers the call waiting first; one allowed is recorded under it,
    /// with the command `kept` from now on when it is always allowed.
    async fn answer(&mut self, decision: &str, kept: Option<&str>) {
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
        match answered {
            Err(error) => self.notice = Some(error.to_string()),
            Ok(_) if decision == "allow" => {
                let name = call["name"].as_str().unwrap_or("tool");
                let lines = transcript::allowed(
                    name,
                    &call["arguments"],
                    kept,
                    &self.workspace,
                    self.width,
                );
                self.commit(lines);
            }
            Ok(_) => {}
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

    /// Allows the waiting command, and every command it starts with from
    /// now, outside every agent's sandbox, as Codex's "don't ask again"
    /// keeps a prefix rule: `[sonny] unsandboxed` holds it, and the lead
    /// and the agents it made take it at once.
    async fn always(&mut self, prefix: &str) {
        if let Err(error) = approvals::allow_always(prefix) {
            self.notice = Some(format!("couldn't keep {prefix}: {error}"));
            return;
        }
        self.answer("allow", Some(prefix)).await;
        // A row the person picked that waits for the turn to end is the
        // one set, with the command let out.
        if let Some(mode) = self.gating.or(self.mode) {
            self.gating = Some(mode);
            self.spread = Some((mode, Instant::now()));
            self.configure().await;
        }
    }

    /// `/compact`, once the person said yes.
    async fn compact(&mut self) {
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
    }

    /// The slash commands the input could be, while it is one word that
    /// starts with `/`, best first, with where the letters typed are in
    /// each: the person's own picks rank first among equal matches.
    fn menu(&self) -> Vec<(&'static str, &'static str, Vec<usize>)> {
        if !self.input.starts_with('/') || self.input.contains(char::is_whitespace) {
            return Vec::new();
        }
        let names = COMMANDS.iter().map(|(name, _)| &name[1..]);
        fuzzy::rank(names, &self.input[1..], |name| {
            self.picks.weight_now(&format!("/{name}"))
        })
        .into_iter()
        .map(|(at, found)| {
            let (name, about) = COMMANDS[at];
            (name, about, found.at.iter().map(|at| at + 1).collect())
        })
        .collect()
    }

    /// What the full screen takes before the rest: the mouse, a copy of
    /// the selection, and the keys that scroll. Whether it took `event`.
    fn full_screen(&mut self, event: &Event) -> bool {
        use ratatui::crossterm::event::MouseEventKind;
        // A key, or the wheel, ends the way up to the first turn, and
        // Sonny's eyes moving for good: it was a welcome.
        match event {
            Event::Key(key) if key.kind != KeyEventKind::Release => {
                self.to_first = false;
                self.eyes = None;
            }
            Event::Mouse(mouse)
                if matches!(
                    mouse.kind,
                    MouseEventKind::ScrollUp | MouseEventKind::ScrollDown
                ) =>
            {
                self.to_first = false;
                self.eyes = None;
            }
            Event::Paste(_) => self.eyes = None,
            _ => {}
        }
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
                self.find_step = None;
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
                // A count under the composer: what it counts.
                if down
                    && let Some((_, _, opens)) =
                        (self.counter_at.iter()).find(|(row, columns, _)| {
                            mouse.row == *row && columns.contains(&mouse.column)
                        })
                {
                    self.follow(&format!("{}{opens}", screen::BUTTON));
                    return true;
                }
                if !in_pane && self.composer_mouse(mouse) {
                    // A press here lets go of what was selected there.
                    if down {
                        self.screen.unselect();
                        if let Some(pane) = &mut self.pane {
                            pane.screen.unselect();
                        }
                    }
                    return true;
                }
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
                        // A press here lets go of what was selected there.
                        if down {
                            self.input.unselect();
                            if let Some(pane) = &mut self.pane {
                                pane.screen.unselect();
                            }
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
        // Ctrl+G opens the agent in the pane in the main view, wherever
        // the keys are, as its title's button does.
        if pane::to_main(key)
            && let Some(agent) = self.pane.as_ref().and_then(|p| p.agent.clone())
        {
            self.pressed.push(format!("main/{agent}"));
            return true;
        }
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
        // `/status` picks an agent from the keys and opens it, as a click
        // on it does.
        if self.finding.is_none()
            && self.pane_keys
            && matches!(key.code, KeyCode::Up | KeyCode::Down | KeyCode::Enter)
            && self.pane.as_ref().is_some_and(|pane| pane.is_card())
            && let Some(watch) = &self.watch
            && watch.background.is_none()
            && watch.usage.is_none()
            && watch.commons.is_none()
        {
            let bots = card_bots(watch, self.width.max(40), &self.bot);
            if !bots.is_empty() {
                let last = bots.len() - 1;
                // A pick the list no longer has (resolved agents hidden) is
                // no pick: Enter opens only the agent marked `›`.
                let pick = watch.pick.filter(|pick| *pick <= last);
                let pick = match (key.code, pick) {
                    (KeyCode::Up, Some(at)) => Some(at.saturating_sub(1)),
                    (KeyCode::Up, None) => Some(last),
                    (KeyCode::Down, Some(at)) => Some((at + 1).min(last)),
                    (KeyCode::Down, None) => Some(0),
                    (_, pick) => pick,
                };
                if key.code == KeyCode::Enter {
                    if let Some(at) = pick {
                        self.open_task(&bots[at]);
                    }
                } else if let Some(watch) = &mut self.watch {
                    watch.pick = pick;
                }
                return true;
            }
        }
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
            // `/commons` takes its own keys: picking a thread, reading it.
            if self.pane_keys
                && pane.is_card()
                && let Some(view) = self.watch.as_mut().and_then(|w| w.commons.as_mut())
            {
                let reading = view.reading();
                match key.code {
                    _ if switches(key) || key.code == KeyCode::Tab => self.pane_keys = false,
                    _ if copy && let Some(text) = pane.screen.selected() => {
                        pane.screen.unselect();
                        self.copied(&text);
                    }
                    KeyCode::Up if reading => pane.screen.scroll(1),
                    KeyCode::Down if reading => pane.screen.scroll(-1),
                    KeyCode::PageUp => pane.screen.page(true),
                    KeyCode::PageDown => pane.screen.page(false),
                    KeyCode::Char(' ') if reading => pane.screen.page(false),
                    KeyCode::Home => pane.screen.hold(usize::MAX),
                    KeyCode::End => pane.screen.latest(),
                    _ => {
                        if view.key(*key) == commons::Key::Close {
                            self.close_pane(true);
                        } else if view.reading() != reading {
                            // A thread opened or left starts at its top.
                            pane.screen.hold(usize::MAX);
                            if let Some(watch) = &mut self.watch {
                                watch.picked = None;
                            }
                        }
                    }
                }
                return true;
            }
            // `/usage` takes its own keys: the period, whose, cached reads.
            if self.pane_keys
                && pane.is_card()
                && let Some(view) = self.watch.as_mut().and_then(|w| w.usage.as_mut())
            {
                match key.code {
                    _ if switches(key) || key.code == KeyCode::Tab => self.pane_keys = false,
                    _ if copy && let Some(text) = pane.screen.selected() => {
                        pane.screen.unselect();
                        self.copied(&text);
                    }
                    KeyCode::Up => pane.screen.scroll(1),
                    KeyCode::Down => pane.screen.scroll(-1),
                    KeyCode::PageUp => pane.screen.page(true),
                    KeyCode::PageDown | KeyCode::Char(' ') => pane.screen.page(false),
                    KeyCode::Home => pane.screen.hold(usize::MAX),
                    KeyCode::End => pane.screen.latest(),
                    _ => {
                        if view.key(*key) == usage::Key::Close {
                            self.close_pane(true);
                        }
                    }
                }
                return true;
            }
            if self.pane_keys {
                match key.code {
                    // Back to the margin from what is over it; the margin closed.
                    KeyCode::Esc
                        if !(pane.is_card()
                            && self
                                .watch
                                .as_mut()
                                .and_then(|w| w.sky.as_mut())
                                .is_some_and(|view| view.press(KeyCode::Esc))) =>
                    {
                        self.close_pane(true)
                    }
                    KeyCode::Esc => {}
                    // `/sky`'s own keys: its legend, zen, the models' colours,
                    // a star picked and centred on; Esc lets go of the pick
                    // and the centre before it closes the pane.
                    _ if pane.is_card()
                        && !switches(key)
                        && let Some(view) = self.watch.as_mut().and_then(|w| w.sky.as_mut())
                        && view.press(key.code) => {}
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
                    && self.config.is_none()
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
                // The pages let go on the way drawn first, so what they
                // hold is found too.
                KeyCode::Up | KeyCode::Enter => self.find_step = Some((true, Vec::new())),
                KeyCode::Char('f' | 'p') if ctrl => self.find_step = Some((true, Vec::new())),
                KeyCode::Down => self.find_step = Some((false, Vec::new())),
                KeyCode::Char('n') if ctrl => self.find_step = Some((false, Vec::new())),
                // Another text: a step for the last one goes no further.
                KeyCode::Backspace => {
                    query.pop();
                    self.screen.find(query);
                    self.find_step = None;
                }
                KeyCode::Char(c) if !ctrl => {
                    query.push(c);
                    self.screen.find(query);
                    self.find_step = None;
                }
                KeyCode::PageUp => self.page_up(),
                KeyCode::PageDown => self.screen.page(false),
                _ => {}
            }
            if self.finding.is_none() {
                self.find_step = None;
                self.screen.end_find();
            }
            return true;
        }
        match key.code {
            // Over the composer, not over a list open on it.
            KeyCode::Char('f')
                if ctrl
                    && self.picker.is_none()
                    && self.config.is_none()
                    && self.choosing.is_none()
                    && self.reacting.is_none() =>
            {
                self.screen.unselect();
                self.finding = Some(String::new());
                self.screen.find("");
            }
            // Every row a click opens, opened or folded from the keys, as
            // Claude Code's Ctrl+O shows a turn's calls.
            KeyCode::Char('o') if ctrl => self.opening_all = true,
            _ if copy && let Some(text) = self.screen.selected() => {
                self.copied(&text);
                self.screen.unselect();
            }
            _ if copy && let Some(range) = self.input.selection() => {
                let text = self.input[range].to_owned();
                self.copied(&text);
                self.input.unselect();
            }
            KeyCode::Esc if self.screen.unselect() || self.input.unselect() => {}
            KeyCode::PageUp => self.page_up(),
            KeyCode::PageDown => self.screen.page(false),
            // With nothing typed, Home goes to the first turn, the pages
            // over it drawn on the way, and End back to the newest.
            KeyCode::Home | KeyCode::End
                if self.input.is_empty()
                    && self.picker.is_none()
                    && self.choosing.is_none()
                    && self.reacting.is_none() =>
            {
                self.to_first = key.code == KeyCode::Home;
                match self.to_first {
                    // The oldest rows went to make room: drawn anew from
                    // the newest page, the way up starts over.
                    true if self.screen.trimmed() => {
                        self.first_node = None;
                        self.older = None;
                        self.unpaired.clear();
                        self.repaint = true;
                    }
                    true => self.screen.scroll(isize::MAX),
                    false => self.screen.latest(),
                }
            }
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
    /// Every key Sonny takes, beside the conversation.
    fn shortcuts(&mut self) {
        let rows = [
            ("enter", "send, or steer the running turn"),
            ("tab", "queue for after the running turn"),
            ("shift+enter", "new line (alt+enter, ctrl+j)"),
            ("esc", "interrupt, or stop a ! command"),
            ("/", "commands"),
            ("!", "run a command of your own"),
            ("ctrl+o", "open or fold every row that folds"),
            ("ctrl+f", "find in the conversation"),
            ("home end", "first turn, newest turn"),
            ("pgup pgdn", "scroll"),
            ("tab", "with nothing typed, keys to the pane"),
            ("ctrl+\\", "keys to the pane and back"),
            ("ctrl+[ ctrl+]", "hide the pane, show it again"),
            ("ctrl+g", "open the pane's agent in the main view"),
            ("alt+t", "boost the next message; again for more"),
            ("ctrl+v", "paste a picture"),
            ("ctrl+c", "copy the selection, clear, or quit"),
            ("ctrl+l", "draw the screen again"),
        ];
        let width = rows.iter().map(|(key, _)| key.width()).max().unwrap_or(0);
        let lines = rows
            .iter()
            .map(|(key, does)| {
                Line::from(vec![
                    Span::raw(format!(" {key:width$}  ")),
                    Span::raw(*does).dim(),
                ])
            })
            .collect();
        self.watch = None;
        let mut pane = pane::Pane::card("shortcuts", "esc close");
        pane.set_card(lines, "esc close");
        self.open_pane(pane);
    }

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
                let mut watch = self.watch.take();
                if let Some(view) = watch.as_mut().and_then(|w| w.sky.as_mut()) {
                    view.pause();
                }
                self.closed = Some((open, watch));
                self.pane = self.under.take();
                self.pane_keys &= keep && self.pane.is_some();
            }
            None => {}
        }
    }

    /// The margin back beside the conversation when nothing else is
    /// there and it holds something, unless the person closed it, as
    /// `[sonny] margin_opens` says: a note put up or changed opens it
    /// again even then, by default, under what is open over it, as VS
    /// Code's `reveal` shows a task's output. It never closes by itself,
    /// so one opened to look at stays, empty or not.
    fn margin_back(&mut self) {
        use agent_client::config::MarginOpens;
        let posted = self.margin.posted();
        let fresh = std::mem::replace(&mut self.margin_posted, posted) != posted;
        if fresh && self.margin_opens == MarginOpens::Always && self.margin.closed() {
            self.margin.close(false);
            if self.under.is_none()
                && let Some(over) = &mut self.pane
            {
                over.over = true;
                self.under = Some(pane::Pane::margin());
            }
        }
        if self.margin.closed() {
            self.under = None;
            if let Some(over) = &mut self.pane {
                over.over = false;
            }
        }
        let shows = !self.margin.is_empty() && self.margin_opens != MarginOpens::Never;
        if self.pane.is_none() && !self.margin.closed() && (shows || self.pane_keys) {
            self.pane = Some(self.under.take().unwrap_or_else(pane::Pane::margin));
        }
    }

    /// `/margin`: the margin shown, with the keys, or closed.
    fn toggle_margin(&mut self) {
        // Not open though not closed, empty or set never to open by
        // itself, it is not shown either.
        let shown = !self.margin.closed()
            && (self.under.is_some() || self.pane.as_ref().is_some_and(pane::Pane::is_margin));
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
                self.pane_keys = true;
                self.margin_back();
            }
        }
    }

    /// What runs for the agent, for the margin's first row: its
    /// background and its context.
    fn glance(&self) -> margin::Glance {
        margin::Glance {
            name: self.bot.clone(),
            behind: self.behind.0,
            failed: self.failed,
            context: self.context,
            window: self.window,
        }
    }

    /// The other agents at work, and what waits on the person: questions
    /// and calls here, and agents that need them. Without windows for the
    /// project's agents, the tasks this one started: those running as the
    /// daemon says, drawn or not, and those that need the person as their
    /// rosters read them.
    fn agents(&self) -> (usize, usize) {
        let doing: HashMap<&str, &str> = match &self.fleet {
            Some(fleet) => (fleet.doing.iter())
                .map(|(bot, doing)| (bot.as_str(), *doing))
                .collect(),
            None => {
                let rostered = (self.tasks.values().flatten())
                    .map(|task| (task.bot.as_str(), task.state.as_str()));
                match &self.tasks_running {
                    Some(running) => (running.iter())
                        .map(|bot| (bot.as_str(), "working"))
                        .chain(rostered.filter(|(_, state)| *state == "needs you"))
                        .collect(),
                    None => rostered.collect(),
                }
            }
        };
        let count = |state: &str| {
            (doing.iter())
                .filter(|(bot, doing)| **bot != self.bot && **doing == state)
                .count()
        };
        (count("working"), self.asks.len() + count("needs you"))
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
                    self.pane_keys = true;
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
                if self.screen.show_tagged(pin, 2) {
                    return;
                }
                // Older pages drawn above, a page at a time, until it is.
                if self.older.is_none() {
                    self.notice = Some("that note's place is no longer in the conversation".into());
                    return;
                }
                self.jump = Some((pin, JUMP_PAGES));
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
            // A margin note answered, quoted the same way.
            "reply-note" => match self.margin.quoted(id) {
                Some(said) => self.quote(&said),
                None => self.notice = Some("that note is no longer in the margin".into()),
            },
            // A message answered: quoted where the person writes.
            "reply" => {
                let Ok(node) = id.parse::<i64>() else {
                    return;
                };
                let read = self
                    .client
                    .request("history_items", json!({"bot": self.bot, "nodes": [node]}))
                    .await;
                match read {
                    Ok(read) => {
                        let said = text(&read["items"][0]["item"]);
                        self.quote(&said);
                    }
                    Err(error) => self.notice = Some(error.to_string()),
                }
            }
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
                        self.closing.insert(id.to_owned());
                        self.reaction_turn = reply["turn"].as_i64();
                    }
                    Err(error) => self.notice = Some(error.to_string()),
                }
            }
            "status" if self.options.side.is_none() => self.status().await,
            // An agent's title button: this window becomes its Sonny.
            "main" => self.switch_command(id).await,
            "background" if self.options.side.is_none() => self.background(),
            "status" | "background" => {}
            _ => self.press_row(what).await,
        }
    }

    /// What the lead's tasks' layout says, as a line for the lead's view:
    /// after the call that started the task, not above it.
    fn fleet_said(&mut self, said: String) {
        self.held.push(Line::from(format!("  ↳ {said}")).dim());
        if self.calls.is_empty() {
            let held = std::mem::take(&mut self.held);
            self.commit(held);
        }
    }

    /// A jump to where a note was put, once the conversation is drawn
    /// back far enough: shown, or the next page older read.
    async fn jumped(&mut self) {
        let Some((pin, pages)) = self.jump else {
            return;
        };
        if self.repaint {
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
                let (pane, mut watch) = self.closed.take().expect("a closed pane");
                if let Some(view) = watch.as_mut().and_then(|w| w.sky.as_mut()) {
                    view.resume();
                }
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
            return (self.centred(area), None);
        };
        // The margin where the conversation leaves room for it, or while
        // the keys are in it; out of the way otherwise, and in zen.
        let prose = PROSE as u16 + GUTTER;
        let margin = pane::margin_width(area.width, prose);
        if open.is_margin() && (margin.is_none() || self.zen.is_some()) && !self.pane_keys {
            return (self.centred(area), None);
        }
        let pane = match margin.filter(|_| open.is_margin()) {
            Some(margin) => margin,
            None => pane::width(area.width, prose),
        };
        if pane == area.width {
            return match self.pane_keys {
                true => (Rect { width: 0, ..area }, Some(area)),
                false => (area, None),
            };
        }
        // A gutter each side of the line between them: the conversation
        // stops short of it, and the pane, whose rows open with a space of
        // their own, starts a column after it.
        let chat = Rect {
            width: (area.width - pane - 1).saturating_sub(GUTTER),
            ..area
        };
        let pane = Rect {
            x: area.right() - pane + 1,
            width: pane - 1,
            ..area
        };
        (chat, Some(pane))
    }

    /// The conversation alone in the window: in zen, its column in the
    /// middle, as a reader centres a page; else where it is, on the left.
    fn centred(&self, area: Rect) -> Rect {
        let column = PROSE as u16;
        match self.zen {
            Some(_) if area.width > column => Rect {
                x: area.x + (area.width - column) / 2,
                width: column,
                ..area
            },
            _ => area,
        }
    }

    /// A page up the full screen.
    fn page_up(&mut self) {
        self.screen.page(true);
    }

    /// The width the conversation is written at.
    fn prose(&self) -> usize {
        self.width.min(PROSE)
    }

    /// `text` put on the clipboard, and said so.
    /// The mouse over the composer's text, as Codex's composer takes it:
    /// a press puts the cursor there, a drag selects, which is copied,
    /// two clicks select a word and three its line. Whether it took
    /// `mouse`.
    fn composer_mouse(&mut self, mouse: &event::MouseEvent) -> bool {
        use ratatui::crossterm::event::{MouseButton, MouseEventKind};
        let held = matches!(
            mouse.kind,
            MouseEventKind::Drag(MouseButton::Left) | MouseEventKind::Up(MouseButton::Left)
        );
        let drawn = match &self.composing {
            Some((_, drawn)) if held => *drawn,
            _ => match (self.composer_at, mouse.kind) {
                (Some(drawn), MouseEventKind::Down(MouseButton::Left))
                    if drawn.0.contains((mouse.column, mouse.row).into()) =>
                {
                    drawn
                }
                _ => return false,
            },
        };
        let (area, first, width) = drawn;
        let rows = self.input.layout(width);
        // Above the rows shown, the one before them; below, the last.
        let row = match mouse.row < area.y {
            true => first.saturating_sub(1),
            false => first + (mouse.row - area.y) as usize,
        }
        .min(rows.len() - 1);
        let at = self
            .input
            .spot(&rows[row], mouse.column.saturating_sub(area.x) as usize);
        match mouse.kind {
            MouseEventKind::Down(_) => {
                let now = Instant::now();
                let clicks = match self.composer_click {
                    Some((then, x, y, n))
                        if now.duration_since(then) < screen::DOUBLE
                            && (x, y) == (mouse.column, mouse.row) =>
                    {
                        n % 3 + 1
                    }
                    _ => 1,
                };
                self.composer_click = Some((now, mouse.column, mouse.row, clicks));
                let origin = match clicks {
                    2 => self.input.word_at(at),
                    3 => self.input.line_at(at),
                    _ => at..at,
                };
                self.input.select(origin.start, origin.end);
                self.composing = Some((origin, drawn));
            }
            _ => {
                let origin = self.composing.clone().map_or(at..at, |(origin, _)| origin);
                match at < origin.start {
                    true => self.input.select(origin.end, at),
                    false => self.input.select(origin.start, at.max(origin.end)),
                }
                if matches!(mouse.kind, MouseEventKind::Up(_)) {
                    self.composing = None;
                    if let Some(range) = self.input.selection() {
                        let text = self.input[range].to_owned();
                        self.copied(&text);
                    }
                }
            }
        }
        true
    }

    fn copied(&mut self, text: &str) {
        term::copy(text);
        let count = text.chars().count();
        self.notice = Some(match count {
            1 => "copied 1 character".into(),
            _ => format!("copied {count} characters"),
        });
    }

    async fn key(&mut self, mut event: Event) {
        // The terminal may no longer show what was drawn: a multiplexer
        // sizes a pane down and back to have its program draw it whole
        // again (Herdr, tmux and screen do), a window can be resized and
        // back between two draws, and a terminal regains focus after the
        // display slept. Every cell is written again then, as vim and
        // ncurses programs do on each resize, and on Ctrl-L as a shell does.
        match &event {
            Event::Resize(..) => self.stale = true,
            Event::FocusGained => {
                self.stale = true;
                return;
            }
            Event::Key(key)
                if key.kind != KeyEventKind::Release
                    && key.code == KeyCode::Char('l')
                    && key.modifiers.contains(KeyModifiers::CONTROL) =>
            {
                self.stale = true;
                return;
            }
            _ => {}
        }
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
        // Tab takes the guess at the next message into the composer, as
        // Claude Code and Codex take theirs; it is not sent. Tab does what
        // it does otherwise when none shows.
        if let Event::Key(key) = &event
            && key.kind != KeyEventKind::Release
            && key.code == KeyCode::Tab
            && !self.pane_keys
            && let Some(guess) = self.suggested()
        {
            let guess = guess.to_owned();
            self.input.set(&guess);
            self.suggestion = None;
            return;
        }
        if self.full_screen(&event) {
            for what in std::mem::take(&mut self.pressed) {
                self.press(&what).await;
            }
            if std::mem::take(&mut self.opening_all) {
                self.open_all().await;
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
                        let picked = self.replies.get(at).cloned();
                        self.reacting = None;
                        match std::mem::take(&mut self.quoting) {
                            true => {
                                if let Some((node, _)) = picked {
                                    self.pressed.push(format!("reply/{node}"));
                                }
                            }
                            false => self.react_to = picked,
                        }
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
                            self.notice = Some(
                                match self.send(approval(&denied), None, false, false, &[]).await {
                                    Ok(()) => {
                                        "approved one retry; the retry is still reviewed".into()
                                    }
                                    Err(error) => {
                                        self.denials.insert(at, denied);
                                        error
                                    }
                                },
                            );
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
        if self.confirming {
            if let Event::Key(key) = &event
                && key.kind != KeyEventKind::Release
            {
                // Only Enter says yes; anything else keeps the conversation.
                self.confirming = false;
                match key.code {
                    KeyCode::Enter => self.compact().await,
                    _ => self.notice = Some("not compacted".into()),
                }
            }
            return;
        }
        if let Some((choice, mut pick)) = self.choosing {
            if let Event::Key(key) = event
                && key.kind != KeyEventKind::Release
            {
                let count = match choice {
                    Choice::Permissions => self.rows.len(),
                    Choice::Speed => self.speed_rows().len(),
                };
                // Another client may have moved the bot to a provider
                // that serves fewer tiers since the list opened.
                pick = pick.min(count - 1);
                match key.code {
                    KeyCode::Up => pick = (pick + count - 1) % count,
                    KeyCode::Down => pick = (pick + 1) % count,
                    KeyCode::Enter => {
                        self.choosing = None;
                        match choice {
                            Choice::Permissions => {
                                let mode = self.rows[pick].name;
                                // Set again even when it is the row the agent
                                // has, as the config may have changed it since;
                                // the agents the lead made take it too.
                                self.spread = Some((mode, Instant::now()));
                                self.gating = Some(mode);
                                self.configure().await;
                                if self.gating.is_some() {
                                    self.notice = Some("applies when idle".into());
                                }
                            }
                            Choice::Speed => self.pace_at(self.speed_rows()[pick].0).await,
                        }
                        return;
                    }
                    KeyCode::Esc => {
                        self.choosing = None;
                        return;
                    }
                    KeyCode::Char('c') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                        self.choosing = None;
                        return;
                    }
                    _ => {}
                }
                self.choosing = Some((choice, pick));
            }
            return;
        }
        if let Some(view) = &mut self.config {
            if let Event::Resize(width, _) = event {
                self.width = width as usize;
            } else if let config::Key::Close = view.event(&event) {
                self.config = None;
            }
            // The chime and the view as the file now has them.
            let settings = settings::Settings::load();
            self.sounds.chime = settings.chime;
            self.margin_opens = settings.margin_opens;
            if self.verbose != settings.verbose {
                self.verbose = settings.verbose;
                self.repaint = true;
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
                        if !ctrl
                            && !alt
                            && !self.approvals.is_empty()
                            && self.paused.is_none()
                            && self.input.is_empty() =>
                    {
                        // A call whose whole arguments could not be read
                        // can only be denied.
                        if c == 'y' && self.approvals[0]["incomplete"] == true {
                            self.notice =
                                Some("only part of this call could be read; n denies it".into());
                        } else {
                            self.answer(if c == 'y' { "allow" } else { "deny" }, None)
                                .await;
                        }
                    }
                    KeyCode::Char('a')
                        if !ctrl
                            && !alt
                            && self.paused.is_none()
                            && self.input.is_empty()
                            && let Some(prefix) =
                                self.approvals.first().and_then(approvals::always) =>
                    {
                        self.always(&prefix).await;
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
                    // Think harder for one message, as Claude Code's Alt+T
                    // toggles thinking: again for the most, again for none.
                    KeyCode::Char('t') if alt => self.boost_step(),
                    // With nothing typed, ? shows the keys, as it does in
                    // Codex and Claude Code.
                    KeyCode::Char('?')
                        if !alt
                            && self.input.is_empty()
                            && self.asks.is_empty()
                            && self.approvals.is_empty()
                            && self.react_to.is_none() =>
                    {
                        self.shortcuts()
                    }
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
                    // With nothing typed, the open questions are a list.
                    KeyCode::Up | KeyCode::Down
                        if self.input.is_empty()
                            && self.asking().is_some()
                            && self.asks.len() > 1 =>
                    {
                        self.step_ask(key.code == KeyCode::Down)
                    }
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
                    // Tab sends it to run once the running turn ends, as
                    // Codex queues; Enter steers.
                    KeyCode::Tab if self.running.is_some() => self.submit_as(true).await,
                    KeyCode::Esc if self.react_to.is_some() => self.react_to = None,
                    // As Codex's bottom pane does, Esc closes what is open
                    // over the composer before it stops anything: an open
                    // question is set aside first, as its hint says.
                    KeyCode::Esc if !self.asks.is_empty() && !self.chatting => {
                        self.chatting = true;
                    }
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

    /// Alt+T: the next message's effort up a step from the agent's own,
    /// to the most the model takes, then back to its own. The steps are
    /// the model's family's: `xhigh` (else `high`), then `max` where it
    /// has one, each only when above the effort set.
    fn boost_step(&mut self) {
        let model = self.turn_model.as_deref().unwrap_or(&self.model);
        let levels = boosts(self.client.providers(), model, self.effort.as_deref());
        if levels.is_empty() && self.boost.take().is_none() {
            self.notice = Some("this model takes no higher effort".into());
            return;
        }
        self.boost = match self.boost.as_deref() {
            None => levels.first().cloned(),
            Some(now) => (levels.iter())
                .skip_while(|level| *level != now)
                .nth(1)
                .cloned(),
        };
    }

    async fn submit(&mut self) {
        self.submit_as(false).await
    }

    /// Send what is written: a message to a busy bot joins its running
    /// turn, or with `queue` runs after it.
    async fn submit_as(&mut self, queue: bool) {
        let prompt = self.input.trim().to_owned();
        if prompt.is_empty() {
            return;
        }
        // A command run, so the menu ranks it first next time.
        let word = prompt.split_whitespace().next().unwrap_or_default();
        if COMMANDS.iter().any(|(name, _)| *name == word) {
            self.picks.pick_now(word);
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
        if matches!(prompt.as_str(), "/commons" | "/forum") {
            self.input.clear();
            self.commons();
            return;
        }
        if prompt == "/sky" {
            self.input.clear();
            self.sky();
            return;
        }
        if prompt == "/usage" {
            self.input.clear();
            if self.options.side.is_some() {
                self.notice = Some("/usage is for the agent itself, not a side chat".into());
            } else {
                self.usage().await;
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
            let mut pane = pane::Pane::card("/mcp", "esc close");
            pane.set_card(lines, "esc close");
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
        if matches!(prompt.as_str(), "/config" | "/settings") {
            self.input.clear();
            match config::View::open() {
                Ok(view) => self.config = Some(view),
                Err(error) => self.notice = Some(error),
            }
            return;
        }
        if prompt == "/fast" {
            self.input.clear();
            let rows = self.speed_rows();
            if rows.len() < 2 {
                let provider = self.model.split_once('/').map_or("", |(p, _)| p);
                self.notice = Some(format!(
                    "{provider} serves its models at standard speed only"
                ));
                return;
            }
            // On the pick still waiting for the turn to end, if there is one.
            let now = (self.pace).unwrap_or(self.speed.as_deref().unwrap_or("standard"));
            let at = rows.iter().position(|(speed, _)| *speed == now);
            self.choosing = Some((Choice::Speed, at.unwrap_or(0)));
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
        if prompt == "/resume" {
            self.input.clear();
            self.notice = Some(match self.client.request("resume", json!({})).await {
                Ok(said) if said["changed"] == true => "the agents go on".into(),
                Ok(_) => "nothing was paused".into(),
                Err(error) => format!("resume: {error}"),
            });
            return;
        }
        if prompt == "/pause" || prompt.starts_with("/pause ") {
            self.input.clear();
            let reason = prompt["/pause".len()..].trim();
            self.notice = Some(match reason {
                "" => "/pause takes why: /pause REASON".into(),
                reason => match (self.client)
                    .request("pause", json!({"reason": reason}))
                    .await
                {
                    Ok(said) if said["changed"] == true => format!(
                        "every agent is paused; {} running stopped",
                        said["interrupted"]
                    ),
                    Ok(_) => "already paused".into(),
                    Err(error) => format!("pause: {error}"),
                },
            });
            return;
        }
        // It can't be taken back, so it asks first, as an MCP tool's
        // `destructiveHint` does: Enter compacts, Esc keeps the conversation.
        if prompt == "/compact" {
            self.input.clear();
            self.confirming = true;
            return;
        }
        if prompt == "/react" || prompt == "/reply" {
            self.input.clear();
            self.quoting = prompt == "/reply";
            if self.replies.is_empty() {
                self.notice = Some(match self.quoting {
                    true => "no reply to answer yet".into(),
                    false => "no reply to react to yet".into(),
                });
            } else {
                self.reacting = Some(0);
            }
            return;
        }
        if prompt == "/retry" {
            self.input.clear();
            match self.denials.len() {
                0 => self.notice = Some("no recent reviewer denials".into()),
                newest => self.approving = Some(newest - 1),
            }
            return;
        }
        if prompt == "/permissions" {
            self.input.clear();
            let current = self.gating.or(self.mode);
            // Read now, so a row just added to the config is there.
            self.rows = approvals::modes();
            self.choosing = Some((
                Choice::Permissions,
                (self.rows.iter())
                    .position(|mode| Some(mode.name) == current)
                    .unwrap_or(0),
            ));
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
        if let Some(rest) = prompt
            .strip_prefix("/lead")
            .filter(|rest| rest.is_empty() || rest.starts_with(char::is_whitespace))
        {
            self.input.clear();
            self.lead_command(rest.trim()).await;
            return;
        }
        if let Some(rest) = prompt
            .strip_prefix("/switch")
            .filter(|rest| rest.is_empty() || rest.starts_with(char::is_whitespace))
        {
            self.input.clear();
            self.switch_command(rest.trim()).await;
            return;
        }
        // `/goal WHAT` goes to the agent as typed, which sets its goal.
        if let Some(rest) = prompt
            .strip_prefix("/goal")
            .map(str::trim)
            .filter(|rest| matches!(*rest, "" | "clear"))
        {
            self.input.clear();
            self.notice = Some(self.goal_command(rest == "clear").await);
            return;
        }
        if let Some(asked) = zen::command(&prompt, self.zen.is_some()) {
            match asked {
                Ok(command) => self.zen_command(command, queue).await,
                Err(why) => self.notice = Some(why.into()),
            }
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
        let answering = self.asking().cloned();
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
        // first, each on its own, and this message joins the turn they start;
        // queued, each waits its turn too.
        let mut join = false;
        while let Some(record) = self.unsent.first().cloned() {
            if let Err(error) =
                (self.send(record.clone(), Some(SHELL), join && !queue, queue, &[])).await
            {
                self.notice = Some(error);
                return;
            }
            self.ran_here.extend(user_shell::read(&record));
            self.unsent.remove(0);
            join = true;
        }
        let pictures = self.placed(&prompt);
        match self
            .send(prompt, None, join && !queue, queue, &pictures)
            .await
        {
            Ok(()) => {
                self.input.clear();
                self.notice = None;
                self.pictures.clear();
                if let Some(ask) = answering {
                    self.close_ask(&ask);
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
    /// of the one just started, `join`), or with `queue` a turn after
    /// whatever runs; staged until it lands. `origin` names what sent it
    /// when the person's words did not.
    async fn send(
        &mut self,
        prompt: String,
        origin: Option<&str>,
        join: bool,
        queue: bool,
        pictures: &[attach::Picture],
    ) -> Result<(), String> {
        // A message to a busy bot joins its running turn, on that turn's model.
        let delivery = match queue {
            true => "queue",
            false if join || self.running.is_some() => "steer",
            false => "reject",
        };
        let steer = delivery == "steer";
        let model = (!steer).then(|| self.turn_model.clone()).flatten();
        // A boost is the next turn's: a steer joins one running already.
        let effort = match steer || origin.is_some() {
            true => None,
            false => self.boost.clone(),
        };
        if steer && origin.is_none() && self.boost.is_some() {
            self.notice = Some("the boost waits for a message that starts a turn".into());
        }
        // A steer never moves a bot, so a moved lead comes back with the
        // next message that starts a turn.
        let workspace = (!steer).then(|| self.move_to.clone()).flatten();
        let reply = self
            .client
            .request(
                "submit",
                json!({"bot": self.bot, "request_id": unique("tui"), "prompt": prompt,
                    "delivery": delivery, "model": model, "workspace": workspace,
                    "effort": effort, "origin": origin, "images": attach::submitted(pictures)}),
            )
            .await
            .map_err(|error| error.to_string())?;
        if effort.is_some() {
            self.boost = None;
        }
        // The lead the folder opens next time.
        if origin.is_none() && self.options.side.is_none() {
            project::wrote_to(self.client.store(), &self.workspace, &self.bot);
        }
        if let Some(turn) = reply["turn"].as_i64() {
            // A steer is a submission of its own that ends as steered; the
            // person's is the turn it joins: the running one, or the one the
            // message before it started.
            if origin.is_none() {
                self.yours = match steer {
                    false => Some(turn),
                    true => (self.running.as_ref().map(|r| r.turn))
                        .or_else(|| self.staged.last().map(|(started, _)| *started)),
                };
            }
            self.staged.push((turn, prompt));
            let staged = &self.staged;
            self.queued
                .retain(|queued| staged.iter().any(|(turn, _)| turn == queued));
            if queue {
                self.queued.push(turn);
            }
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

    /// The question an `ask` call put, open until it is answered or the
    /// conversation goes on past it. One newly asked takes no focus from
    /// the one being answered, as Codex's questions take none.
    fn asked(&mut self, id: &str, arguments: &Value) {
        if let Some(ask) = asks::call(id, arguments)
            && !(self.asks.iter().chain(&self.aside)).any(|open| open.id == ask.id)
        {
            self.asks.push(ask);
        }
    }

    /// The open question what is typed answers, unless Esc set them
    /// aside for a message of the person's own.
    fn asking(&self) -> Option<&asks::Ask> {
        (!self.chatting).then(|| self.asks.get(self.at())).flatten()
    }

    /// Where the question in focus is among those open, from 0.
    fn at(&self) -> usize {
        let picked = self.focus.as_ref();
        (self.asks.iter())
            .position(|ask| Some(&ask.id) == picked)
            .unwrap_or(0)
    }

    /// `ask` answered from here: closed now, its answer kept until it
    /// lands so landing closes no other asked the same.
    fn close_ask(&mut self, ask: &asks::Ask) {
        self.asks.retain(|open| open.id != ask.id);
        self.aside.retain(|open| open.id != ask.id);
        self.sent_answers.push(ask.question.clone());
        self.settle_asks();
    }

    /// None open, typing writes again.
    fn settle_asks(&mut self) {
        if self.asks.is_empty() {
            self.chatting = false;
        }
    }

    /// The question a message answered, from this window or another,
    /// closed; or, the person's own words that answer none, every
    /// question open set aside, as the conversation went on past them.
    fn answered(&mut self, message: &str, from: Option<&str>) {
        // An answer sent from here closed its question when it was sent.
        if let Some(question) = asks::answered(message)
            && let Some(at) = self.sent_answers.iter().position(|q| q == question)
        {
            self.sent_answers.remove(at);
            return;
        }
        if asks::moves_on(message, from) {
            self.aside.append(&mut self.asks);
        } else if !asks::close(&mut self.asks, message) {
            asks::close(&mut self.aside, message);
        }
        self.settle_asks();
    }

    /// The composer's focus on the next open question, or the one
    /// before: Up and Down with nothing typed step through them, as a
    /// list's rows.
    fn step_ask(&mut self, down: bool) {
        let (count, at) = (self.asks.len(), self.at());
        let to = match down {
            true => (at + 1) % count,
            false => (at + count - 1) % count,
        };
        self.focus = self.asks.get(to).map(|ask| ask.id.clone());
    }

    /// The bot's reactions so far to the person's message, right under
    /// it, wherever it is: never a row of their own. A message no longer
    /// in the conversation, or not drawn yet, shows them once it is.
    /// Whether zen keeps what the running turn says out of view: the
    /// person did not start it.
    fn zen_hides(&self) -> bool {
        self.zen.is_some() && self.elsewhere
    }

    fn draw_bot_reactions(&mut self) {
        self.place_bot_reactions();
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
        if self.put_under(screen::Tag::End(node), tag, row.clone()) {
            return true;
        }
        match self.cluster_of(node) {
            Some(at) => self.put_under(screen::Tag::Group(at as u64), tag, row),
            None => false,
        }
    }

    /// What other agents, such as its lead, reacted with to the person's
    /// message at `node`, each with who, as the row under it (under the
    /// bot's own reactions when there are some). A message not drawn
    /// shows them once it is.
    fn place_picked(&mut self, node: i64) {
        let on: Vec<String> = (self.picked.on(node).iter())
            .map(|(by, emoji)| format!("{emoji} {by}"))
            .collect();
        let row = (!on.is_empty()).then(|| transcript::reacted(&on.join("  ")));
        self.flush();
        let under = match self.screen.has(screen::Tag::Reactions(node)) {
            true => screen::Tag::Reactions(node),
            false => screen::Tag::End(node),
        };
        self.screen.put_under(under, screen::Tag::Picked(node), row);
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
        // A reply still being written gets them under it once it is; one
        // not drawn here, once it is drawn.
        if (self.live.as_ref()).is_some_and(|live| live.node == Some(node)) {
            return;
        }
        self.notice = Some(match added {
            true => format!("{emoji} on “{first}”"),
            false => format!("took back {emoji} on “{first}”"),
        });
    }

    /// A guess at the person's next message, after a turn of theirs ended
    /// well, made by a copy of this bot on this window's model; unless the
    /// config's `[sonny] suggest` is off, or the bot asked a question,
    /// which its own card answers, or it is in zen: the guess reads all
    /// the agent holds back, and could tell it.
    fn suggest(&mut self) {
        self.suggestion = None;
        if !self.asks.is_empty()
            || self.zen.is_some()
            || !agent_client::config::load().is_ok_and(|c| c.sonny.suggest)
        {
            return;
        }
        self.suggesting = Some(tokio::spawn(suggest::guess(
            self.client.clone(),
            self.bot.clone(),
            self.turn_model.clone(),
        )));
    }

    /// The guess, once made. A turn started since lets it go; a copy
    /// still guessing finishes and is deleted on its own.
    async fn suggested_now(&mut self) {
        if self.running.is_some() || !self.staged.is_empty() {
            self.suggesting = None;
            self.suggestion = None;
            return;
        }
        if self
            .suggesting
            .as_ref()
            .is_some_and(|job| job.is_finished())
        {
            let job = self.suggesting.take().unwrap();
            self.suggestion = job.await.ok().flatten();
        }
    }

    /// The guess, while the composer shows it: empty, with nothing running
    /// and nothing open over it.
    fn suggested(&self) -> Option<&str> {
        let open = self.picker.is_some()
            || self.confirming
            || self.choosing.is_some()
            || self.approving.is_some()
            || self.reacting.is_some()
            || self.finding.is_some()
            || self.react_to.is_some();
        let shows = self.input.is_empty()
            && self.running.is_none()
            && self.asks.is_empty()
            && self.approvals.is_empty()
            && !open;
        self.suggestion.as_deref().filter(|_| shows)
    }

    /// `/side`: a side chat with a copy of this bot, taken where it is now,
    /// mid-turn included, and opened in a Sonny of its own on the alternate
    /// screen. The copy may call no tool, so it answers from what the bot
    /// knows, unless the config's `[sonny] side_chat` is `"fork"`: then it
    /// may do anything the bot may. The daemon deletes it once it has been
    /// idle a quarter hour. This bot carries on, and its lines wait for the
    /// main screen.
    async fn side(&mut self, question: String, pictures: &[attach::Picture]) {
        if self.options.side.is_some() {
            self.notice = Some("this is a side chat already".into());
            return;
        }
        let tools = match agent_client::config::load() {
            Ok(config) => config.sonny.side_chat,
            Err(error) => {
                self.notice = Some(format!("no side chat: {error}"));
                return;
            }
        };
        let stamp = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_millis();
        let name = format!("{}.side-{:x}", self.bot, stamp % 0x100_0000);
        // Made by the agent it copies, which is how Sonny's own tools know
        // the copy for one rather than by its name alone.
        let page = self
            .client
            .request("bots", json!({"name": self.bot, "limit": 1}));
        let source = match page.await {
            Ok(page) if page["bots"][0]["name"] == self.bot.as_str() => page["bots"][0].clone(),
            Ok(_) => {
                self.notice = Some(format!("no side chat: {} is gone", self.bot));
                return;
            }
            Err(error) => {
                self.notice = Some(format!("no side chat: {error}"));
                return;
            }
        };
        let mut fork = json!({"source": self.bot, "bot": name,
            "created_by": self.bot, "created_by_id": source["bot_id"],
            "expire_ms": SIDE_EXPIRE.as_millis() as u64, "request_id": name});
        // A fork with no list keeps its source's tools.
        if tools == agent_client::config::SideChat::Answer {
            fork["allow"] = json!([]);
        }
        let forked = self.client.request("fork", fork).await;
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
                        "prompt": question, "delivery": "reject", "model": self.turn_model,
                        "images": attach::submitted(pictures)}),
                )
                .await
        {
            self.notice = Some(format!("the side chat did not take the question: {error}"));
        }
        let head = forked["head"].as_i64().unwrap_or(0).to_string();
        let mut args = [
            "--no-sound",
            "--no-layout",
            "--agent",
            &name,
            "--side",
            &head,
        ]
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

    /// `/goal` shows the goals of this agent and its tasks, and `/goal
    /// clear` clears its own, as its goal tool would.
    async fn goal_command(&mut self, clear: bool) -> String {
        let arguments = match clear {
            true => json!({"clear": self.bot}),
            false => json!({}),
        };
        let said =
            lookout::goals::call(&self.options.store, &self.client, &self.bot, &arguments).await;
        match said {
            Err(error) => error,
            Ok(said) if clear => match said["cleared"] == true {
                true => "goal cleared".into(),
                false => "no goal to clear".into(),
            },
            Ok(said) => {
                let goals = said["goals"].as_array().cloned().unwrap_or_default();
                if goals.is_empty() {
                    return "no goal; /goal WHAT asks the agent to work until WHAT is done".into();
                }
                let shown: Vec<String> = goals
                    .iter()
                    .map(|goal| {
                        format!(
                            "{}: done when {}",
                            goal["bot"].as_str().unwrap_or_default(),
                            goal["done_when"].as_str().unwrap_or_default()
                        )
                    })
                    .collect();
                format!(
                    "goals: {}; /goal clear clears this agent's",
                    shown.join("; ")
                )
            }
        }
    }

    /// `/lead`: the folder's leads; `/lead NAME` opens this window on one,
    /// and `/lead new [NAME]` makes one and opens it.
    async fn lead_command(&mut self, rest: &str) {
        let Some(project) = self.project.clone().filter(|_| self.options.side.is_none()) else {
            self.notice = Some("/lead is for a folder's lead, not this agent".into());
            return;
        };
        let dir = std::path::PathBuf::from(&self.workspace);
        let mut words = rest.split_whitespace();
        let pick = match (words.next(), words.next(), words.next()) {
            (None, ..) => None,
            (Some("new"), name, None) => Some(project::Pick::New(name.map(str::to_owned))),
            (Some(name), None, _) => Some(project::Pick::Named(name.to_owned())),
            _ => {
                self.notice = Some("/lead, /lead NAME or /lead new [NAME]".into());
                return;
            }
        };
        let Some(pick) = pick else {
            self.notice = Some(match project::leads(&self.client, &project, &dir).await {
                Ok(leads) => {
                    let names: Vec<String> = (leads.iter())
                        .filter_map(|lead| lead["name"].as_str())
                        .map(|name| match name == self.bot {
                            true => format!("{} (this one)", project::lead_short(name)),
                            false => project::lead_short(name).to_owned(),
                        })
                        .collect();
                    format!(
                        "{project}'s leads here: {}; /lead NAME opens one, /lead new [NAME] makes one",
                        names.join(", ")
                    )
                }
                Err(error) => error,
            });
            return;
        };
        if let project::Pick::Named(name) = &pick
            && project::lead_bot(&project, name) == self.bot
        {
            self.notice = Some(format!("this window is the {name} lead already"));
            return;
        }
        match project::open(&self.client, &dir, None, &pick).await {
            Ok(lead) => {
                self.switch_to = Some(lead.bot);
                self.quit = true;
            }
            Err(error) => self.notice = Some(error),
        }
    }

    /// This agent's zen as its file now says, set elsewhere: drawn again
    /// in the other view when it starts or ends, and told.
    fn zen_taken_up(&mut self) {
        let focus = zen::focus(self.reactions_key.as_deref());
        if focus.is_some() != self.zen.is_some() {
            self.notice = Some(zen_notice(focus.is_some()));
            self.zen_view_changed();
        }
        self.zen = focus;
    }

    /// Zen started or ended: drawn again in the other view, and what
    /// reply, copy and react point at is found again in what it draws.
    fn zen_view_changed(&mut self) {
        self.replies.clear();
        self.last_reply.clear();
        self.repaint = true;
    }

    /// `/zen FOCUS` or `/zen off`: sent to the agent as typed, whose zen
    /// skill takes it from there, then kept for this agent and drawn.
    async fn zen_command(&mut self, command: zen::Command, queue: bool) {
        let (said, focus) = match command {
            zen::Command::On(focus) => (format!("/zen {focus}"), Some(focus)),
            zen::Command::Off => ("/zen off".to_owned(), None),
        };
        if let Err(error) = self.send(said, None, false, queue, &[]).await {
            self.notice = Some(error);
            return;
        }
        self.input.clear();
        if let Err(error) = zen::set(self.reactions_key.as_deref(), focus.as_deref()) {
            self.notice = Some(error);
        }
        let changed = self.zen.is_some() != focus.is_some();
        self.notice = Some(zen_notice(focus.is_some()));
        self.zen = focus;
        self.pane_keys = false;
        // Drawn again in the other view; a new focus redraws nothing.
        if changed {
            self.zen_view_changed();
        }
    }

    /// `/switch NAME`: this window becomes NAME's Sonny, whatever agent it
    /// is (a lead, a task, one the person made), as `sonny --agent NAME`
    /// opens it; a name of this project's may leave out the project.
    /// `/switch` alone opens the agent beside the conversation, or the one
    /// `/status` has picked, closing the pane.
    async fn switch_command(&mut self, name: &str) {
        if self.options.side.is_some() || self.options.embedded {
            self.notice = Some("/switch is for the main window; this one is beside it".into());
            return;
        }
        let name = match name {
            "" => match self.beside() {
                Some(bot) => bot,
                None => {
                    self.notice = Some(
                        "/switch NAME, or open an agent beside the conversation (/status) first"
                            .into(),
                    );
                    return;
                }
            },
            name => name.to_owned(),
        };
        let mut names = vec![name.clone()];
        if let Some(project) = &self.project
            && !name.contains('.')
        {
            names.push(format!("{project}.{name}"));
        }
        for bot in names {
            if bot == self.bot {
                self.notice = Some(format!("this window is {bot}'s already"));
                return;
            }
            match self.client.request("inspect", json!({"bot": bot})).await {
                Ok(_) => {
                    self.switch_to = Some(bot);
                    self.quit = true;
                    return;
                }
                Err(error) if error.code == "bot_not_found" => continue,
                Err(error) => {
                    self.notice = Some(error.to_string());
                    return;
                }
            }
        }
        self.notice = Some(format!("no agent named {name}"));
    }

    /// The agent beside the conversation: one open in the pane, else the
    /// one `/status` has picked.
    fn beside(&self) -> Option<String> {
        if let Some(agent) = self.pane.as_ref().and_then(|pane| pane.agent.clone()) {
            return Some(agent);
        }
        let watch = self.watch.as_ref()?;
        let open = self.pane.as_ref().is_some_and(|pane| pane.is_card());
        let listing = watch.background.is_none()
            && watch.usage.is_none()
            && watch.commons.is_none()
            && watch.sky.is_none();
        let bots = card_bots(watch, self.width.max(40), &self.bot);
        (open && listing)
            .then(|| watch.pick.and_then(|at| bots.get(at).cloned()))
            .flatten()
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
    /// status row before `y` can allow it: the row shows only the keys.
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
        // A call an exec script makes, which waits while it is asked.
        if call["step_of"].is_string() {
            lines.push(Line::from("  a call in a running script").dim());
        }
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
        // What the agent said before the call reads before it, a step as
        // the call's start would make it.
        self.end_live(Words::Step);
        self.commit(lines);
    }

    /// A frame drawn, the whole screen written again when it is stale:
    /// cleared first, and shown at once with what replaces it where the
    /// terminal holds a frame back until it is done.
    fn draw(&mut self, terminal: &mut Tui) -> std::io::Result<()> {
        if !std::mem::take(&mut self.stale) {
            return self.frame(terminal);
        }
        execute!(terminal.backend_mut(), BeginSynchronizedUpdate)?;
        let drawn = terminal.clear().and_then(|()| self.frame(terminal));
        execute!(terminal.backend_mut(), EndSynchronizedUpdate)?;
        drawn
    }

    fn frame(&mut self, terminal: &mut Tui) -> std::io::Result<()> {
        self.show_asked();
        self.margin_back();
        // /status beside the conversation, gone once the pane is.
        if self.watch.is_some() && !self.pane.as_ref().is_some_and(|p| p.keeps_card()) {
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
        // What is typed to answer a question has the question over it,
        // as a reply in a chat has what it replies to.
        let heading = self.answering_over(width.min(PROSE)).filter(|_| room > 1);
        let text_room = room - usize::from(heading.is_some());
        let (mut composer_rows, mut cursor_row) =
            window(composer_rows, cursor_row, text_room.max(1));
        // The first of the text's rows shown, for the mouse to find its spot.
        let first_row = match &self.composing {
            Some((_, (_, first, _))) => Some(*first),
            None => (!self.input.is_empty()).then(|| {
                let layout = self.input.layout(width.min(PROSE));
                self.input.cursor(&layout).0 - cursor_row
            }),
        };
        if let Some(heading) = &heading {
            composer_rows.insert(0, heading.clone());
            cursor_row += 1;
        }
        let mut composer_at = None;
        let mut counter_at = Vec::new();
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
        // Set never to open by itself, it is beside only once opened.
        let opened = self.margin_opens != agent_client::config::MarginOpens::Never
            || self.under.is_some()
            || self.pane.as_ref().is_some_and(pane::Pane::is_margin);
        let beside = !self.margin.closed()
            && self.zen.is_none()
            && opened
            && pane::margin_width(size.width, PROSE as u16 + GUTTER).is_some();
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
            if let Some(view) = &mut watch.sky {
                view.drain();
                let rows = size.height.saturating_sub(4) as usize;
                let lines: Vec<Line<'static>> = (view.card(width.saturating_sub(1), rows))
                    .into_iter()
                    .map(|line| {
                        let mut spans = vec![Span::raw(" ")];
                        spans.extend(line.spans);
                        Line::from(spans).style(line.style)
                    })
                    .collect();
                let keys = if view.replaying() {
                    SKY_REPLAY_KEYS
                } else {
                    SKY_KEYS
                };
                pane.set_card(lines, keys);
                pane.foot = (!view.zen).then(|| view.footer(width));
            } else if let Some(view) = &watch.background {
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
            } else if let Some(view) = &watch.commons {
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
                if view.reading() {
                    // A thread just opened starts at its top, after the
                    // card's swap kept the list's scroll.
                    if watch.picked != Some(usize::MAX) {
                        watch.picked = Some(usize::MAX);
                        pane.screen.show_row(0, 0);
                    }
                } else if watch.picked != Some(at) {
                    watch.picked = Some(at);
                    pane.screen.keep_in_view(at, 2);
                }
            } else if let Some(view) = &watch.usage {
                let lines = view
                    .card(width)
                    .into_iter()
                    .map(|line| {
                        let mut spans = vec![Span::raw(" ")];
                        spans.extend(line.spans);
                        Line::from(spans).style(line.style)
                    })
                    .collect();
                pane.set_card(lines, STATUS_KEYS);
                pane.foot = Some(view.footer());
            } else {
                let (mut lines, any_resolved, marks) = watch_card(watch, width, &self.bot);
                let mut picked = None;
                // The agent picked from the keys leads with `›`, as a
                // pick in Codex's lists does.
                if let Some(&(row, _)) = watch.pick.and_then(|pick| marks.get(pick))
                    && let Some(first) = lines.get_mut(row).and_then(|line| line.spans.first_mut())
                {
                    *first = Span::raw("›").bold();
                    picked = Some(row);
                }
                let keys = match any_resolved && watch.resolved {
                    true => "↑↓ pick · enter open · r hide resolved · esc close",
                    false => PICK_KEYS,
                };
                pane.set_card_marked(lines, marks, keys);
                // Scrolled to once, as a new pick, so the page keys still
                // move away from it; after the card, whose swap would
                // drop the request.
                if let Some(row) = picked
                    && watch.picked != Some(row)
                {
                    watch.picked = Some(row);
                    pane.screen.keep_in_view(row, 2);
                }
            }
        }
        // The links drawn, to be written again as hyperlinks.
        let mut linked = Vec::new();
        // Where the pane's program has its cursor.
        let mut pane_cursor = None;
        // Tab takes a guess at the next message while one shows.
        let switch = match self.suggested() {
            Some(_) => "ctrl+\\",
            None => "tab",
        };
        let paint = |frame: &mut ratatui::Frame,
                     linked: &mut Vec<(u16, u16, ratatui::buffer::Cell)>| {
            // The rows that move stay at the bottom, as a chat's composer
            // does, and the conversation sits right above them.
            let (area, pane) = self.split(frame.area());
            if let (Some(at), Some(open)) = (pane, &mut self.pane) {
                // Paths in a file are read from its folder first.
                let base = open.folder().unwrap_or(&workspace).to_owned();
                let file = |word: &str| {
                    (links::file_line(&base, word))
                        .or_else(|| links::file_line(&workspace, word))
                        .or_else(|| self.names.borrow_mut().link(word))
                };
                pane_cursor = open.render(at, frame.buffer_mut(), self.pane_keys, switch, file);
                linked.extend(open.screen.link_cells(frame.buffer_mut()));
                // The line between them, as a split's.
                if area.width > 0 {
                    let line = Rect {
                        x: area.right() + GUTTER,
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
            // `/config` lists every setting over the rows above.
            let rows = match &self.config {
                Some(_) => (rows + config::height()).min(room as usize),
                None => rows,
            };
            let [history, view] =
                Layout::vertical([Constraint::Min(0), Constraint::Length(below)]).areas(area);
            let file = |word: &str| {
                links::file_line(&workspace, word).or_else(|| self.names.borrow_mut().link(word))
            };
            let history = ratatui::layout::Rect {
                y: history.bottom().saturating_sub(rows as u16),
                width: column,
                height: (rows as u16).min(history.height),
                ..history
            };
            self.screen.render(history, frame.buffer_mut(), file);
            if let Some(eyes) = &mut self.eyes
                && let Some(y) = self.screen.drawn_at(screen::Tag::Eyes)
            {
                banner::draw_eyes(frame.buffer_mut(), history.x, y, eyes.look(Instant::now()));
            }
            linked.extend(self.screen.link_cells(frame.buffer_mut()));
            let [live, status, gap, composer, pad, footer] = areas(view, live_rows);
            frame.render_widget(Paragraph::new(open_rows.clone()), live);
            frame.render_widget(Paragraph::new(self.status_line()), status);
            let menu = self.menu();
            if !menu.is_empty() {
                let pick = self.pick.min(menu.len() - 1);
                let items: Vec<list::Item> = (menu.iter())
                    .map(|(name, about, found)| list::Item::new(*name, *about).found(found))
                    .collect();
                // Over the rows above the composer's band.
                over(frame, list::lines(&items, pick, width, gap.y as usize), gap);
            }
            if let Some(query) = &self.finding {
                let line = Line::from(vec![Span::raw("find ").dim(), Span::raw(query.clone())]);
                frame.render_widget(Paragraph::new(line), composer);
                frame.set_cursor_position((composer.x + 5 + query.width() as u16, composer.y));
                let place = match self.screen.found() {
                    _ if self.find_step.is_some() => "  looking further · ".to_owned(),
                    Some((_, 0)) if !query.is_empty() => "  not found · ".to_owned(),
                    Some((at, count)) if count > 0 => format!("  {at} of {count} · "),
                    _ => "  ".to_owned(),
                };
                let mut keys = vec![Span::raw(place)];
                keys.extend(hint::spans("↑ older · ↓ newer · esc stop"));
                let keys = Line::from(keys);
                frame.render_widget(Paragraph::new(keys), footer);
                return;
            }
            if let Some(pick) = self.reacting {
                // Newest at the bottom, nearest the composer, as they read;
                // the pick counts from the newest.
                let items: Vec<list::Item> = (self.replies.iter())
                    .map(|(node, said)| {
                        let on = self.reactions.on(*node).join(" ");
                        let room = width.saturating_sub(10 + on.width());
                        list::Item::new(transcript::cut(said, room), on)
                    })
                    .collect();
                let pick = items.len().saturating_sub(pick + 1);
                over(
                    frame,
                    list::lines(&items, pick, width, composer.y as usize),
                    composer,
                );
                let (asked, keys) = match self.quoting {
                    true => (
                        "which reply to answer",
                        "  ↑↓ pick · enter quote · esc close",
                    ),
                    false => (
                        "which reply to react to",
                        "  ↑↓ pick · enter react · esc close",
                    ),
                };
                let line = Line::from(vec![Span::raw("› ").bold(), Span::raw(asked).dim()]);
                frame.render_widget(Paragraph::new(line), composer);
                let keys = Line::from(hint::spans(keys));
                frame.render_widget(Paragraph::new(keys), footer);
                return;
            }
            if let Some(pick) = self.approving {
                // Newest last, as they happened; the reason stays in view
                // before a retry is approved.
                let room = (composer.width as usize).saturating_sub(8);
                let items: Vec<list::Item> = (self.denials.iter())
                    .map(|denied| {
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
                        list::Item::new(transcript::cut(&what, room / 2), why)
                    })
                    .collect();
                over(
                    frame,
                    list::lines(&items, pick, width, composer.y as usize),
                    composer,
                );
                let line = Line::from(vec![
                    Span::raw("› ").bold(),
                    Span::raw("reviewer denials: approve one retry").dim(),
                ]);
                frame.render_widget(Paragraph::new(line), composer);
                let keys = Line::from(hint::spans(
                    "  ↑↓ pick · enter read, again approve one retry · esc close",
                ));
                frame.render_widget(Paragraph::new(keys), footer);
                return;
            }
            if let Some((Choice::Speed, pick)) = self.choosing {
                let now = self.speed.as_deref().unwrap_or("standard");
                let items: Vec<list::Item> = (self.speed_rows().into_iter())
                    .map(|(speed, about)| list::Item::new(speed, about).current(speed == now))
                    .collect();
                let pick = pick.min(items.len() - 1);
                over(
                    frame,
                    list::lines(&items, pick, width, composer.y as usize),
                    composer,
                );
                let line = Line::from(vec![
                    Span::raw("› ").bold(),
                    Span::raw("speed for this agent").dim(),
                ]);
                frame.render_widget(Paragraph::new(line), composer);
                let keys = Line::from(hint::spans("  ↑↓ pick · enter choose · esc close"));
                frame.render_widget(Paragraph::new(keys), footer);
                return;
            }
            if self.confirming {
                let name = match &self.project {
                    Some(project) => lead_title(project, &self.bot),
                    None => self.bot.clone(),
                };
                // The question first, as a decision card's is; why it asks muted.
                let line = Line::from(vec![
                    Span::raw("› ").bold(),
                    Span::raw(format!("Compact {name}'s conversation?")).bold(),
                    Span::raw(" It can't be undone.").dim(),
                ]);
                frame.render_widget(Paragraph::new(line), composer);
                let keys = Line::from(hint::spans("  enter compact · esc cancel"));
                frame.render_widget(Paragraph::new(keys), footer);
                return;
            }
            if let Some((_, pick)) = self.choosing {
                let items: Vec<list::Item> = (self.rows.iter())
                    .map(|mode| {
                        list::Item::new(&mode.label, &mode.about)
                            .current(Some(mode.name) == self.mode)
                    })
                    .collect();
                over(
                    frame,
                    list::lines(&items, pick, width, composer.y as usize),
                    composer,
                );
                let line = Line::from(vec![
                    Span::raw("› ").bold(),
                    Span::raw("permissions for this agent and those it makes").dim(),
                ]);
                frame.render_widget(Paragraph::new(line), composer);
                let keys = Line::from(hint::spans("  ↑↓ pick · enter choose · esc close"));
                frame.render_widget(Paragraph::new(keys), footer);
                return;
            }
            if let Some(view) = &self.config {
                // Every setting where there is room, over the rows above.
                let rows = view.rows(composer.y.saturating_sub(frame.area().y) as usize);
                let area = ratatui::layout::Rect {
                    y: composer.y.saturating_sub(rows.len() as u16),
                    height: rows.len() as u16,
                    ..composer
                };
                frame.render_widget(ratatui::widgets::Clear, area);
                frame.render_widget(Paragraph::new(rows), area);
                let (line, cursor) = view.line();
                frame.render_widget(Paragraph::new(line), composer);
                if let Some(column) = cursor {
                    frame.set_cursor_position((composer.x + column, composer.y));
                }
                frame.render_widget(Paragraph::new(view.keys()), footer);
                return;
            }
            if let Some(picker) = &self.picker {
                let at = picker.at();
                let items: Vec<list::Item> = (picker.found().into_iter())
                    .map(|(model, found)| {
                        list::Item::new(model, "")
                            .current(model == self.model)
                            .found(&found)
                    })
                    .collect();
                let room = composer.y as usize;
                let mut rows = list::lines(&items, at, width, room);
                // Why the list is short, in the rows it leaves.
                for refused in (picker.refused.iter())
                    .take((list::ROWS + 1).min(room).saturating_sub(rows.len()))
                {
                    rows.push(Line::from(format!("    {refused}")).dim());
                }
                over(frame, rows, composer);
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
                let mut keys = vec![
                    Span::raw("  effort ").dim(),
                    Span::raw(format!("‹ {effort} ›")).bold(),
                ];
                keys.extend(hint::spans(
                    "  ↑↓ pick · ←→ effort · enter switch · esc close",
                ));
                let keys = Line::from(keys);
                frame.render_widget(Paragraph::new(keys), footer);
                return;
            }
            edge(frame, gap, composer, pad);
            frame.render_widget(Paragraph::new(composer_rows), composer);
            // The mouse finds the text under the row over it.
            let text = match heading {
                Some(_) => Rect {
                    y: composer.y + 1,
                    height: composer.height.saturating_sub(1),
                    ..composer
                },
                None => composer,
            };
            composer_at = first_row.map(|first| (text, first, width.min(PROSE)));
            // Keys in the pane: no cursor where they do not go.
            if !self.pane_keys {
                frame.set_cursor_position((
                    composer.x + (cursor_col as u16).min(composer.width.saturating_sub(1)),
                    composer.y + (cursor_row as u16).min(composer.height.saturating_sub(1)),
                ));
            }
            // Under the composer's band and no wider: one column, its
            // right edge where the band's is.
            // The menu's keys while it is open, as every list over the
            // composer has them, saying what Enter does with the pick.
            let (line, opens) = match menu_keys(&menu, self.pick, &self.input) {
                Some(keys) => (Line::from(hint::spans(keys)), Vec::new()),
                None => self.footer(width.min(PROSE)),
            };
            frame.render_widget(Paragraph::new(line), footer);
            counter_at = (opens.into_iter())
                .map(|(at, link)| (footer.y, footer.x + at.start..footer.x + at.end, link))
                .collect();
        };
        let look = look::Look::now();
        terminal.draw(|frame| {
            paint(frame, &mut linked);
            // A link something was drawn over since, as a list over the
            // conversation, is left as the frame drew it.
            screen::keep_shown(&mut linked, frame.buffer_mut());
            look.finish(frame.buffer_mut());
            // The pictures it shows, sent before it is written.
            image::send_shown(frame.buffer_mut());
        })?;
        self.composer_at = composer_at;
        self.counter_at = counter_at;
        // Over themselves, the cursor kept where the frame left it.
        if !linked.is_empty() {
            linked.iter_mut().for_each(|(_, _, cell)| look.cell(cell));
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

    /// The row over what is typed while it answers a question: which,
    /// and which of those open.
    fn answering_over(&self, width: usize) -> Option<Line<'static>> {
        let ask = self.asking().filter(|_| !self.input.is_empty())?;
        let open = [
            self.picker.is_some(),
            self.config.is_some(),
            self.finding.is_some(),
        ];
        (!open.contains(&true))
            .then(|| asks::answering_typed(ask, self.at(), self.asks.len(), width))
    }

    /// The composer's rows: the prompt wrapped to the width, the way it
    /// will read once sent, and only the rows around the cursor when it
    /// is long; or the hint. Also the cursor's row among those and its
    /// column. A list or picker open over the composer keeps it to the
    /// cursor's row.
    fn composer(&self, width: usize) -> (Vec<Line<'static>>, (usize, usize)) {
        // What is sent answers the open question in focus.
        let answering = self.asking();
        // The guess at the next message, for Tab to take.
        if let Some(guess) = self.suggested() {
            let room = width.saturating_sub(prompt::LEAD + TAB.width());
            return (
                vec![Line::from(vec![
                    Span::raw("› ").bold(),
                    // Italic too: a side chat's screen, read through vt100,
                    // loses the dim.
                    Span::raw(transcript::cut(guess, room)).dim().italic(),
                    Span::raw(TAB).dim(),
                ])],
                (0, prompt::LEAD),
            );
        }
        if self.input.is_empty() {
            // Named, so a message typed in the wrong pane is caught first.
            let name = match &self.project {
                Some(project) => lead_title(project, &self.bot),
                None => self.bot.clone(),
            };
            // What Enter does, muted, then its keys in the one grammar.
            let said = |words: &str, keys: &str| {
                let mut spans = vec![Span::raw(format!("{words} · ")).dim()];
                spans.extend(hint::spans(keys));
                spans
            };
            let hint = match self.running {
                _ if let Some(ask) = answering => asks::hint(
                    ask,
                    self.at(),
                    self.asks.len(),
                    width.saturating_sub(prompt::LEAD),
                ),
                _ if self.react_to.is_some() => said("An emoji for that reply", "esc cancel"),
                _ if !self.asks.is_empty() => said("A message of your own", "esc answer"),
                Some(_) => vec![Span::raw(format!("Steer {name}")).dim()],
                None => vec![Span::raw(format!("Message {name}")).dim()],
            };
            let lead = match answering {
                Some(_) => Span::raw("? ").bold().fg(Color::Yellow),
                None => Span::raw("› ").bold(),
            };
            let mut spans = vec![lead];
            spans.extend(hint);
            // Where the keys are, while nothing else is asked.
            if answering.is_none()
                && self.asks.is_empty()
                && self.react_to.is_none()
                && self.approvals.is_empty()
            {
                spans.push(Span::raw(" · ").dim());
                spans.extend(hint::spans("? shortcuts"));
            }
            return (vec![Line::from(spans)], (0, prompt::LEAD));
        }
        let layout = self.input.layout(width);
        let (row, col) = self.input.cursor(&layout);
        // A shell command is marked as one while it is typed.
        let shell = self.input.starts_with('!');
        let selected = self.input.selection();
        let keep = match self.confirming
            || self.choosing.is_some()
            || self.approving.is_some()
            || self.picker.is_some()
            || self.config.is_some()
            || self.reacting.is_some()
            || self.finding.is_some()
        {
            true => 1,
            false => COMPOSER_ROWS,
        };
        let rows = layout.len();
        let (layout, row) = match &self.composing {
            // While the mouse button is down the rows stay where the press
            // found them, so the text under the mouse is what it selects.
            Some((_, (_, first, _))) => {
                let first = (*first).min(rows - 1);
                let row = row.saturating_sub(first).min(keep - 1);
                (layout.into_iter().skip(first).take(keep).collect(), row)
            }
            None => window(layout, row, keep),
        };
        // Markdown shows as it will read, its marks kept: styled on the
        // rows shown alone, so a long paste costs no more to draw.
        let marked = prompt::marks(&self.input);
        let rows: Vec<Line<'static>> = layout
            .iter()
            .map(|part| {
                let lead = match part.start {
                    0 if shell => Span::raw("› ").bold().fg(Color::Magenta),
                    0 if answering.is_some() => Span::raw("? ").bold().fg(Color::Yellow),
                    0 => Span::raw("› ").bold(),
                    _ => Span::raw("  "),
                };
                let mut spans = vec![lead];
                spans.extend(prompt::spans(
                    &self.input,
                    part.start..part.end,
                    &marked,
                    selected.as_ref(),
                ));
                Line::from(spans)
            })
            .collect();
        (rows, (row, col))
    }

    fn status_line(&self) -> Line<'static> {
        // A pause holds every approved call too, so it is shown first, and
        // the approval keys wait for the resume.
        if let Some(pause) = &self.paused {
            let by = pause["by"].as_str().unwrap_or("you");
            let mut line = vec![
                Span::raw("⏸ ").fg(Color::Red),
                Span::raw("Every agent is paused").bold(),
                Span::raw(format!(" by {by}: ")).dim(),
                Span::raw(first_line(pause["reason"].as_str().unwrap_or_default())),
                Span::raw(" · /resume").dim(),
            ];
            if !self.approvals.is_empty() {
                line.push(Span::raw(" · an approval waits").dim());
            }
            return Line::from(line);
        }
        if let Some(call) = self.approvals.first() {
            // Only the keys: the call is written out whole right above,
            // under the same ⏸, so the row does not say it again.
            let mut line = vec![
                Span::raw("⏸ ").fg(Color::Yellow),
                Span::raw("y"),
                Span::raw(" allow once · ").dim(),
            ];
            if let Some(prefix) = approvals::always(call) {
                line.push(Span::raw("a"));
                line.push(Span::raw(format!(" always allow {prefix} · ")).dim());
            }
            line.extend([Span::raw("n"), Span::raw(" deny").dim()]);
            return Line::from(line);
        }
        if let Some(running) = self.running.as_ref().filter(|_| !self.zen_hides()) {
            let mut spans = vec![Span::raw("• ").dim()];
            // In zen, what it runs or calls stays out of view, as its calls do.
            let phase = match running.phase.split_once(' ') {
                Some(("Running" | "Calling", _)) if self.zen.is_some() => "Working",
                _ => running.phase.as_str(),
            };
            spans.extend(shimmer(phase, self.started.elapsed()));
            // What is written steers it on Enter; Tab keeps it for after.
            let queues = match self.input.trim().is_empty() {
                true => "",
                false => " • tab to queue",
            };
            spans.push(
                Span::raw(format!(
                    " ({} • esc to interrupt{queues})",
                    transcript::elapsed(running.started.elapsed().as_secs())
                ))
                .dim(),
            );
            return Line::from(spans);
        }
        match &self.notice {
            Some(notice) => Line::from(Span::raw(notice.clone())),
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

    /// A message quoted before what the person is writing, as a mail or a
    /// GitHub reply quotes: `> ` before each of its lines, the first
    /// [`QUOTE_LINES`] of them, a blank line, then the cursor.
    fn quote(&mut self, said: &str) {
        let lines: Vec<&str> = said.trim().lines().collect();
        let mut quoted: Vec<String> = (lines.iter().take(QUOTE_LINES))
            .map(|line| format!("> {line}").trim_end().to_owned())
            .collect();
        if lines.len() > QUOTE_LINES {
            quoted.push("> …".into());
        }
        let typed = self.input.trim().to_owned();
        self.input.set(&format!("{}\n\n{typed}", quoted.join("\n")));
        self.pane_keys = false;
    }

    /// The profile the agent runs as, when it isn't its seat's own: a
    /// lead's other than `lead`, a task's other than `task`, as its
    /// project or the config sets them. Read again only for another agent
    /// or folder.
    fn profile(&self) -> Option<String> {
        let key = (self.bot.clone(), self.workspace.clone());
        if let Some((read, profile)) = &*self.profile.borrow()
            && *read == key
        {
            return profile.clone();
        }
        let here = std::path::Path::new(&self.workspace);
        let info = |dir: &std::path::Path| agent_client::project::read(dir).ok();
        let profile = match (&self.project, task::source_folder(here)) {
            (Some(_), _) => (info(here).and_then(|info| project::lead_profile(&info).ok()))
                .filter(|profile| profile != "lead"),
            (None, Some(source)) => (info(&source))
                .and_then(|info| project::task_profile(&info).ok())
                .filter(|profile| profile != "task"),
            (None, None) => None,
        };
        *self.profile.borrow_mut() = Some((key, profile.clone()));
        profile
    }

    #[cfg(test)]
    fn footer_line(&self, width: usize) -> Line<'static> {
        self.footer(width).0
    }

    /// The footer, and what each count on it opens on a click: what needs
    /// the person and the agents at work `/status`, as a roster's rows
    /// do; what failed in the background `/background`.
    fn footer(&self, width: usize) -> (Line<'static>, Vec<(std::ops::Range<u16>, &'static str)>) {
        use footer::{Item, Slot};
        let mut items = Vec::new();
        // The row it runs in, unless it is the one new leads start in:
        // full access's mark in the text's own weight, as nothing holds
        // it back; no hue, as yellow is for what needs the person.
        let mode = match self.gating {
            Some(_) => None,
            None => self.mode,
        };
        if let Some(mode) = mode
            && let Some(row) = approvals::footer(mode)
        {
            let forms = match approvals::mark(mode) {
                Some(mark) => {
                    let mark = match mode {
                        "full" => Span::raw(mark),
                        _ => Span::raw(mark).dim(),
                    };
                    vec![
                        vec![mark.clone(), Span::raw(format!(" {row}")).dim()],
                        vec![mark],
                    ]
                }
                None => vec![vec![Span::raw(row).dim()]],
            };
            items.push(Item::new(Slot::Access, forms));
        }
        // The model as pickers name it; what the running turn runs at, as
        // a boost has it, else the bot's effort.
        let model = Span::raw(models::label(&self.model)).dim();
        let mut long = vec![model.clone()];
        let effort = (self.running.as_ref()).and_then(|running| running.effort.as_ref());
        if let Some(effort) = effort.or(self.effort.as_ref()) {
            long.push(Span::raw(format!(" {effort}")).dim());
        }
        // An armed boost in the text's own weight: what the next message
        // will take, said without a hue.
        if let Some(boost) = &self.boost {
            long.push(Span::raw(format!(" ↑{boost}")));
        }
        if let Some(speed) = &self.speed {
            long.push(Span::raw(format!(" {speed}")).dim());
        }
        items.push(Item::new(Slot::Model, vec![long, vec![model]]));
        // What waits for the turn to end, so a pick is not taken as lost.
        let mut waits = Vec::new();
        if let Some((model, effort)) = &self.switch {
            let effort = effort.as_ref().map(|e| format!(" {e}")).unwrap_or_default();
            waits.push(format!("→ {}{effort}", models::label(model)));
        }
        if let Some(mode) = self.gating {
            let row = approvals::footer(mode).unwrap_or_else(|| mode.to_owned());
            waits.push(match approvals::mark(mode) {
                Some(mark) => format!("{mark} {row}"),
                None => row,
            });
        }
        if let Some(speed) = self.pace {
            waits.push(format!("{speed} speed"));
        }
        if !waits.is_empty() {
            let waits = waits.join(" · ");
            items.push(Item::new(
                Slot::Pending,
                vec![
                    vec![Span::raw(format!("{waits} when idle")).dim()],
                    vec![Span::raw(waits).dim()],
                ],
            ));
        }
        // A lead or task told more than its seat's own role says so.
        if let Some(profile) = self.profile() {
            items.push(Item::new(
                Slot::Profile,
                vec![vec![Span::raw(profile).dim()], Vec::new()],
            ));
        }
        // The folder by whole parts, its end kept, as fish and Starship
        // cut it; and its branch, kept whole.
        let folder = tilde(&self.workspace);
        let branch = match &self.place.branch {
            Some(branch) => format!(" on {branch}"),
            None => String::new(),
        };
        let place = [
            folder.clone(),
            footer::fish(&folder),
            footer::base(&folder).to_owned(),
        ]
        .into_iter()
        .map(|folder| vec![Span::raw(format!("{folder}{branch}")).dim()])
        .chain([Vec::new()])
        .collect();
        items.push(Item::new(Slot::Place, place));
        if self.options.side.is_some() {
            let with = (self.bot.rsplit_once(".side-")).map_or(self.bot.as_str(), |(s, _)| s);
            items.push(Item::new(
                Slot::Side,
                vec![vec![
                    Span::raw(format!("side chat with {with} · esc back")).dim(),
                ]],
            ));
            return footer::lay(items, width);
        }
        let (working, waiting) = self.agents();
        // Marks as the roster and /status draw them, each with its word,
        // so the counts read without colour.
        if waiting > 0 {
            let forms = footer::count("?", Color::Yellow, waiting, "needs you", false);
            items.push(Item::new(Slot::Needs, forms).opens("status"));
        }
        // In zen, the focus, and only what needs the person besides.
        if let Some(focus) = &self.zen {
            let needs = match waiting {
                0 => 0,
                n => format!("  ? {n} needs you").width(),
            };
            let room = width.saturating_sub(4 + needs + "zen · ".len());
            let focus = format!("zen · {}", transcript::cut(&first_line(focus), room));
            items.retain(|item| item.slot == Slot::Needs);
            items.push(Item::new(Slot::Place, vec![vec![Span::raw(focus).dim()]]));
            return footer::lay(items, width);
        }
        // The margin's glance says these while it is in view.
        let glancing = (self.pane.as_ref()).is_some_and(|p| p.is_margin() && p.area.width > 0);
        if self.failed > 0 && !glancing {
            let forms = footer::count("✗", Color::Red, self.failed, "failed", false);
            items.push(Item::new(Slot::Failed, forms).opens("background"));
        }
        if working > 0 {
            let forms = footer::count("●", Color::Cyan, working, "working", true);
            items.push(Item::new(Slot::Working, forms).opens("status"));
        }
        // Something runs for it with no one typing: /background lists it.
        if self.behind.0 > 0 && !glancing {
            let n = self.behind.0;
            let forms = [format!("↻ {n} in background"), format!("↻ {n}")]
                .map(|text| vec![Span::raw(text).dim()]);
            let forms = forms.into_iter().chain([Vec::new()]).collect();
            items.push(Item::new(Slot::Background, forms).opens("background"));
        }
        let notes = self.margin.notes().len();
        if notes > 0 && !glancing {
            let forms = vec![vec![Span::raw(format!("✎ {notes}")).dim()], Vec::new()];
            items.push(Item::new(Slot::Notes, forms));
        }
        // Messages its next call takes in, which need nothing of the person.
        let clip = match self.attached.len() {
            0 => None,
            1 => Some("📎".to_owned()),
            n => Some(format!("📎 {n}")),
        };
        if let Some(clip) = clip {
            items.push(Item::new(
                Slot::Attached,
                vec![vec![Span::raw(clip).dim()], Vec::new()],
            ));
        }
        // How full the next round is, as what is left of its window: no
        // hue while there is room, then yellow, then red, and it never
        // goes once half is used.
        if let Some(size) = self.context
            && !glancing
        {
            let (amount, _, color) = transcript::fullness(size, self.window);
            let words = match self.window.is_some_and(|window| window > 0) {
                true => " left",
                false => " context",
            };
            let item = match color.filter(|color| *color != Color::Green) {
                Some(color) => Item::new(
                    Slot::LowContext,
                    vec![
                        vec![Span::raw(amount.clone()).fg(color), Span::raw(words).dim()],
                        vec![Span::raw(amount).fg(color)],
                    ],
                ),
                None => Item::new(
                    Slot::RoomyContext,
                    vec![
                        vec![Span::raw(format!("{amount}{words}")).dim()],
                        Vec::new(),
                    ],
                ),
            };
            items.push(item);
        }
        footer::lay(items, width)
    }
}

/// `rows` drawn over what is right above `below`, cleared first.
fn over(frame: &mut ratatui::Frame, rows: Vec<Line<'static>>, below: ratatui::layout::Rect) {
    let area = ratatui::layout::Rect {
        y: below.y.saturating_sub(rows.len() as u16),
        height: (rows.len() as u16).min(below.y),
        ..below
    };
    frame.render_widget(ratatui::widgets::Clear, area);
    frame.render_widget(Paragraph::new(rows), area);
}

/// What the pane's title row says the keys do on `/status`.
const STATUS_KEYS: &str = "esc close";
const SKY_KEYS: &str = "↑↓←→ pick · m models · r replay · z zen · ? legend · esc close";
const SKY_REPLAY_KEYS: &str = sky::REPLAY_KEYS;

/// What the title row says the keys do on `/status` itself.
const PICK_KEYS: &str = "↑↓ pick · enter open · esc close";

/// The agents `/status` lists, in its order: what its keys pick from.
fn card_bots(watch: &Watch, width: usize, me: &str) -> Vec<String> {
    (watch_card(watch, width, me).2)
        .into_iter()
        .filter_map(|(_, mark)| match mark {
            screen::Mark::Link(link) => link
                .strip_prefix(&format!("{}task/", screen::BUTTON))
                .map(str::to_owned),
            _ => None,
        })
        .collect()
}

/// `/status`'s card at `width`, each line after a space, its times
/// running turns counted on from when its rows were read; whether any task in it
/// is resolved; and the line each agent but `me` starts on, which a click
/// opens in the pane.
fn watch_card(
    watch: &Watch,
    width: usize,
    me: &str,
) -> (Vec<Line<'static>>, bool, Vec<(usize, screen::Mark)>) {
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
    let (mut card, named) = status::card(&watch.project, &rows, width, watch.resolved);
    // The lookout's line goes under the heading, the agents' under it.
    let lookout = !watch.lookout.is_empty();
    if lookout {
        card.insert(1, Line::from(watch.lookout.clone()).dim());
    }
    let marks = (named.into_iter())
        .map(|(at, bot)| (at + usize::from(lookout && at >= 1), bot))
        .filter(|(_, bot)| bot != me)
        .map(|(at, bot)| {
            (
                at,
                screen::Mark::Link(format!("{}task/{bot}", screen::BUTTON)),
            )
        })
        .collect();
    let lines = card
        .into_iter()
        .map(|line| {
            let mut spans = vec![Span::raw(" ")];
            spans.extend(line.spans);
            Line::from(spans).style(line.style)
        })
        .collect();
    (lines, any_resolved, marks)
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
    // Another lead of the same folder, by the name /lead takes.
    if project::lead_project(bot) == Some(project) {
        return match bot.ends_with(".lead") {
            true => "first lead".into(),
            false => format!("lead {}", project::lead_short(bot)),
        };
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
    let mut spans = vec![Span::raw("  ✎ ").dim(), Span::raw(title).dim().underlined()];
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

/// A message's own words and, under them, the words it quotes, each
/// after a line naming who wrote it: what reading the message in full shows.
fn quoting(item: &Value) -> String {
    text(item) + &quotes_of(item)
}

/// The words a message quotes, each under a line naming who wrote it, as
/// Markdown quotes them; nothing when it quotes none.
fn quotes_of(item: &Value) -> String {
    let mut out = String::new();
    let parts = item["content"].as_array();
    for quote in item["quotes"].as_array().into_iter().flatten() {
        let words = (quote["at"].as_u64())
            .and_then(|at| parts?.get(at as usize)?["text"].as_str())
            .unwrap_or_default();
        let who = transcript::sender(
            quote["from"]["bot"].as_str(),
            quote["origin"].as_str(),
            quote["person"].as_str(),
        )
        .unwrap_or_else(|| "you".into());
        let did = match quote["reply"] == true {
            true => "replied",
            false => "wrote",
        };
        out.push_str(&format!("\n\n{who} {did}:"));
        for line in words.lines() {
            out.push_str(&format!(
                "\n>{}{line}",
                if line.is_empty() { "" } else { " " }
            ));
        }
    }
    out
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
        // A terminal's: what it wrote, while it runs or as it ended.
        Ok(terminal)
            if terminal["output"].is_string()
                && (terminal["running"] == true || terminal.get("exit_code").is_some()) =>
        {
            let shown = terminal["output"].as_str().unwrap_or("").to_owned();
            (
                shown,
                failed || terminal["exit_code"].as_i64().is_some_and(|code| code != 0),
            )
        }
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

/// What the window says when zen starts or ends, however it was asked.
fn zen_notice(on: bool) -> String {
    match on {
        true => "zen: only your turns and the agent's questions show; /zen ends it".into(),
        false => "zen is off: the agent tells what it held".into(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn over_ssh_with_nothing_to_open_on_a_link_is_copied() {
        let with = |vars: &[(&str, &str)]| {
            let vars: Vec<(String, String)> = vars
                .iter()
                .map(|(k, v)| (k.to_string(), v.to_string()))
                .collect();
            remote_without_display(&move |key| {
                vars.iter().find(|(k, _)| k == key).map(|(_, v)| v.into())
            })
        };
        assert!(!with(&[]), "a local terminal opens");
        assert!(with(&[("SSH_CONNECTION", "1.2.3.4 5 6.7.8.9 22")]));
        assert!(with(&[("SSH_TTY", "/dev/pts/1"), ("DISPLAY", "")]));
        assert!(!with(&[
            ("SSH_CONNECTION", "c"),
            ("DISPLAY", "localhost:10.0")
        ]));
        assert!(!with(&[
            ("SSH_CONNECTION", "c"),
            ("WAYLAND_DISPLAY", "wayland-0")
        ]));
        assert!(!with(&[
            ("SSH_CONNECTION", "c"),
            ("BROWSER", "open-on-client")
        ]));
    }

    #[test]
    fn a_page_only_read_gives_back_what_drawing_it_would() {
        let call = |id: &str, name: &str, arguments: &str| json!({"type": "function_call", "call_id": id, "name": name, "arguments": arguments});
        let result = |id: &str| json!({"type": "function_call_output", "call_id": id});
        let items = vec![
            (1, result("before")),
            (2, call("ran", "shell", "{}")),
            (3, result("ran")),
            (4, call("here", "react", r#"{"emoji":"👍"}"#)),
            (5, call("there", "react", r#"{"emoji":"👍","bot":"other"}"#)),
            (6, result("here")),
            (6, result("there")),
        ];
        let after = vec![(9, result("newer"))];
        let back: Vec<i64> = unpaired(items.into_iter().chain(after))
            .into_iter()
            .map(|(node, _)| node)
            .collect();
        // A result before its call, one for another agent's reaction, and
        // one a newer page could not pair are for older pages.
        assert_eq!(back, vec![1, 6, 9]);
    }

    #[test]
    fn another_lead_of_the_folder_and_its_tasks_are_never_tasks_here() {
        let lineage: std::collections::HashSet<String> = ["ws.lead.api".into()].into();
        let leads = std::collections::HashSet::new();
        // The other leads are neither tasks nor another project's.
        assert_eq!(
            kin("ws.lead", "", "ws.lead.api", "ws", &lineage, &leads),
            Kin::None
        );
        assert_eq!(
            kin("ws.lead.web", "", "ws.lead", "ws", &lineage, &leads),
            Kin::None
        );
        // Named like the project, a bot no agent made is the first lead's.
        assert_eq!(
            kin("ws.x", "", "ws.lead.api", "ws", &lineage, &leads),
            Kin::None
        );
        assert_eq!(
            kin("ws.x", "", "ws.lead", "ws", &lineage, &leads),
            Kin::Task
        );
        assert_eq!(
            kin("ws.y", "ws.lead.api", "ws.lead.api", "ws", &lineage, &leads),
            Kin::Task
        );
        let bot = |name: &str, by: Option<&str>| json!({"name": name, "created_by": by});
        let bots = [
            bot("ws.lead", None),
            bot("ws.lead.api", None),
            bot("ws.fix", Some("ws.lead")),
            bot("ws.fix-helper", Some("ws.fix")),
            bot("ws.api-fix", Some("ws.lead.api")),
            bot("ws.loose", None),
        ];
        let mine = |lead: &str, first: bool| {
            let mut mine: Vec<String> = lineage_of(&bots, lead, first).into_iter().collect();
            mine.sort();
            mine
        };
        assert_eq!(
            mine("ws.lead", true),
            ["ws.fix", "ws.fix-helper", "ws.loose"]
        );
        assert_eq!(mine("ws.lead.api", false), ["ws.api-fix"]);
    }

    #[test]
    fn another_projects_lead_and_its_tasks_are_never_tasks_here() {
        let lineage: std::collections::HashSet<String> = ["ws.lead".into(), "ws.fix".into()].into();
        let mut leads = std::collections::HashSet::new();
        assert_eq!(
            kin("ws.fix2", "ws.fix", "ws.lead", "ws", &lineage, &leads),
            Kin::Task
        );
        assert_eq!(
            kin("ws.side", "", "ws.lead", "ws", &lineage, &leads),
            Kin::Task
        );
        // Whoever made it, as the new_project tool says no one.
        assert_eq!(
            kin("ws.web.lead", "", "ws.lead", "ws", &lineage, &leads),
            Kin::Lead("ws.web".into())
        );
        leads.insert("ws.web".to_owned());
        // That lead's tasks are its own, though named like this project's.
        assert_eq!(
            kin(
                "ws.web.fix",
                "ws.web.lead",
                "ws.lead",
                "ws",
                &lineage,
                &leads
            ),
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
            kin("shop.lead", "ws.lead", "ws.lead", "ws", &lineage, &leads),
            Kin::Lead("shop".into())
        );
        assert_eq!(
            kin("other.task", "", "ws.lead", "ws", &lineage, &leads),
            Kin::None
        );
        // The lead of a project this one is inside leaves its tasks here.
        let lineage: std::collections::HashSet<String> = ["ws.web.lead".into()].into();
        let mut leads = std::collections::HashSet::new();
        assert_eq!(
            kin("ws.lead", "", "ws.web.lead", "ws.web", &lineage, &leads),
            Kin::None
        );
        leads.insert("ws.api".to_owned());
        assert_eq!(
            kin(
                "ws.web.fix",
                "ws.web.lead",
                "ws.web.lead",
                "ws.web",
                &lineage,
                &leads
            ),
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
    pub(crate) async fn daemon_answering(
        answer: impl Fn(&Value) -> Value + Send + 'static,
    ) -> Arc<Client> {
        daemon_serving(json!({}), answer).await
    }

    /// A stand-in daemon whose ready event lists `providers`.
    async fn daemon_serving(
        providers: Value,
        answer: impl Fn(&Value) -> Value + Send + 'static,
    ) -> Arc<Client> {
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
            let ready = json!({"event": "ready", "protocol": agent_client::PROTOCOL,
                "providers": providers});
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
            lead: project::Pick::Last,
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

    #[test]
    fn boost_steps_to_the_familys_higher_efforts() {
        let providers = json!({
            "openai": {"family": "responses", "efforts": ["low", "medium", "high", "xhigh"]},
            "anthropic": {"family": "anthropic",
                "efforts": ["low", "medium", "high", "xhigh", "max"]},
            "old": {"family": "responses", "efforts": ["low", "medium", "high"]},
        });
        let at = |model, effort| boosts(&providers, model, effort);
        assert_eq!(at("anthropic/opus", None), ["xhigh", "max"]);
        assert_eq!(at("anthropic/opus", Some("xhigh")), ["max"]);
        assert_eq!(at("openai/gpt", Some("medium")), ["xhigh"]);
        assert_eq!(at("old/gpt", None), ["high"]);
        assert!(at("openai/gpt", Some("xhigh")).is_empty());
        assert!(at("nowhere/x", None).is_empty());
    }

    #[tokio::test]
    async fn a_boost_goes_with_the_next_turn_alone() {
        let asked = Arc::new(std::sync::Mutex::new(Vec::new()));
        let seen = asked.clone();
        let client = daemon_answering(move |request| {
            seen.lock().unwrap().push(request.clone());
            json!({"turn": 1})
        })
        .await;
        let mut app = app(client);
        app.boost = Some("max".into());
        assert!(app.footer_line(120).to_string().contains(" ↑max"));
        app.send("Think it through".into(), None, false, false, &[])
            .await
            .unwrap();
        app.send("And this".into(), None, false, false, &[])
            .await
            .unwrap();
        let submits: Vec<Value> = (asked.lock().unwrap().iter())
            .filter(|r| r["op"] == "submit")
            .cloned()
            .collect();
        assert_eq!(submits[0]["effort"], "max");
        assert!(submits[1]["effort"].is_null());
        assert!(app.boost.is_none());
        // The boosted turn runs at its effort, and the bot's own is what
        // the next boost steps up from.
        app.effort = Some("medium".into());
        app.daemon(turn_event("accepted", 1, json!({"effort": "max"})))
            .await;
        assert!(app.footer_line(120).to_string().contains(" max"));
        app.daemon(turn_event(
            "turn_finished",
            1,
            json!({"status": "completed"}),
        ))
        .await;
        assert_eq!(app.effort.as_deref(), Some("medium"));
        let footer = app.footer_line(120).to_string();
        assert!(
            footer.contains(" medium") && !footer.contains(" max"),
            "{footer}"
        );
        // One armed against an effort since changed is let go, and Alt+T
        // turns one off where nothing is higher.
        app.boost = Some("xhigh".into());
        app.refresh(
            &json!({"provider": "anthropic", "model": "opus", "effort": "max"}),
            true,
        );
        assert!(app.boost.is_none());
        app.boost = Some("xhigh".into());
        app.boost_step();
        assert!(app.boost.is_none() && app.notice.is_none());
    }

    #[tokio::test]
    async fn switch_opens_any_agent_here_or_the_one_beside() {
        let client = daemon_answering(|asked| match asked["bot"].as_str() {
            Some("shop.fix" | "shop.lead") => json!({"name": asked["bot"]}),
            _ => json!({"error": "bot_not_found", "detail": "no such bot"}),
        })
        .await;
        let mut app = app(client);
        app.project = Some("shop".into());
        // By its whole name, or its name in this window's project.
        app.input.set("/switch fix");
        app.submit().await;
        assert_eq!(app.switch_to.as_deref(), Some("shop.fix"));
        assert!(app.quit);
        (app.switch_to, app.quit) = (None, false);
        app.input.set("/switch nobody");
        app.submit().await;
        assert_eq!(app.notice.as_deref(), Some("no agent named nobody"));
        assert!(!app.quit);
        app.input.set("/switch b");
        app.submit().await;
        assert_eq!(app.notice.as_deref(), Some("this window is b's already"));
        // Alone, the agent open beside the conversation, as its title's
        // button does.
        app.input.set("/switch");
        app.submit().await;
        assert!(
            app.notice
                .as_deref()
                .is_some_and(|n| n.starts_with("/switch NAME"))
        );
        let mut pane = pane::Pane::card("shop.lead", STATUS_KEYS);
        pane.as_agent("shop.lead");
        app.open_pane(pane);
        app.input.set("/switch");
        app.submit().await;
        assert_eq!(app.switch_to.as_deref(), Some("shop.lead"));
        // Ctrl+G too, wherever the keys are.
        app.switch_to = None;
        let ctrl_g = event::KeyEvent::new(KeyCode::Char('g'), KeyModifiers::CONTROL);
        app.key(Event::Key(ctrl_g)).await;
        assert_eq!(app.switch_to.as_deref(), Some("shop.lead"));
        // A window drawn beside another keeps to its agent.
        (app.switch_to, app.quit) = (None, false);
        app.options.embedded = true;
        app.input.set("/switch shop.fix");
        app.submit().await;
        assert!(app.switch_to.is_none());
    }

    #[tokio::test]
    async fn a_pause_shows_who_pulled_it_until_resume() {
        let client = daemon_answering(|asked| match asked["op"].as_str() {
            Some("pause") => json!({"pause": {"reason": asked["reason"], "by": null, "at_ms": 1},
                "changed": true, "interrupted": 2}),
            Some("resume") => json!({"pause": {"reason": "rows", "by": null}, "changed": true}),
            _ => json!({}),
        })
        .await;
        let mut app = app(client);
        let shown = |app: &App| -> String {
            (app.status_line().spans.iter())
                .map(|s| s.content.as_ref())
                .collect()
        };
        app.input.set("/pause");
        app.submit().await;
        assert_eq!(
            app.notice.as_deref(),
            Some("/pause takes why: /pause REASON")
        );
        app.input.set("/pause the migration deletes rows");
        app.submit().await;
        assert_eq!(
            app.notice.as_deref(),
            Some("every agent is paused; 2 running stopped")
        );
        // Every window hears of it, whoever pulled it.
        let pause = json!({"reason": "the migration deletes rows", "by": "calc.a", "at_ms": 1});
        app.daemon(json!({"event": "paused", "pause": pause, "durable": false}))
            .await;
        assert_eq!(
            shown(&app),
            "⏸ Every agent is paused by calc.a: the migration deletes rows · /resume"
        );
        app.input.set("/resume");
        app.submit().await;
        assert_eq!(app.notice.as_deref(), Some("the agents go on"));
        // It holds the line over an approval, whose keys wait for it.
        app.approvals
            .push(json!({"name": "shell", "arguments": {"command": "rm -rf rows"}}));
        assert_eq!(
            shown(&app),
            "⏸ Every agent is paused by calc.a: the migration deletes rows · /resume · an approval waits"
        );
        app.daemon(json!({"event": "resumed", "pause": pause, "durable": false}))
            .await;
        assert!(app.paused.is_none());
        assert_eq!(shown(&app), "⏸ y allow once · n deny");
    }

    #[tokio::test]
    async fn the_cleanup_reads_only_what_the_person_wrote_since_the_last_one() {
        let said_ = |text: &str| {
            json!({"type": "message", "role": "user",
            "content": [{"type": "input_text", "text": text}]})
        };
        // The agent moved into the shop project after turn 6: each message
        // keeps the project of the folder its turn ran in.
        let shop = std::env::temp_dir().join(format!("said-shop-{}", std::process::id()));
        std::fs::create_dir_all(shop.join(".agents")).unwrap();
        std::fs::write(shop.join(".agents/project.toml"), "name = \"shop\"\n").unwrap();
        let elsewhere = std::env::temp_dir();
        let (shop_, elsewhere_) = (shop.clone(), elsewhere.clone());
        let client = daemon_answering(move |asked| {
            let args = asked;
            match (
                asked["op"].as_str().unwrap_or_default(),
                args["bot"].as_str(),
            ) {
                ("bots", _) => json!({"bots": [{"name": "shop.lead"}, {"name": "shop.old"}],
                    "next_after": null}),
                // Turns come a page at a time.
                // Turns come a page at a time. Turn 7 was queued behind 6
                // and cancelled before the cleanup; turn 4 ran before it.
                ("turns", Some("shop.lead")) if args["before"].is_null() => json!({"turns": [
                    {"turn": 8, "started_ms": 3000, "finished_ms": null, "workspace": shop_},
                    {"turn": 7, "started_ms": null, "finished_ms": 600, "workspace": shop_}],
                    "next_before": 7}),
                ("turns", Some("shop.lead")) => json!({"turns": [
                    {"turn": 6, "started_ms": 400, "finished_ms": 2000, "workspace": elsewhere_},
                    {"turn": 4, "started_ms": 100, "finished_ms": 300, "workspace": elsewhere_}],
                    "next_before": 4}),
                ("turns", _) => json!({"turns": [{"turn": 3, "finished_ms": 100}]}),
                ("history_nodes", Some("shop.lead")) => json!({"nodes": [
                    {"node": 30, "turn": 8}, {"node": 29, "turn": 8}, {"node": 28, "turn": 6},
                    {"node": 27, "turn": 6}, {"node": 26, "turn": 6}, {"node": 20, "turn": 4}],
                    "next_from": 19}),
                ("history_items", Some("shop.lead")) => {
                    let items: Vec<Value> = (args["nodes"].as_array().into_iter().flatten())
                        .filter_map(|node| {
                            let item = match node.as_i64()? {
                                30 => {
                                    json!({"node": 30, "item": said_("Keep Bedrock out of evals.")})
                                }
                                29 => json!({"node": 29, "item": {"type": "message",
                                    "role": "assistant", "content": [{"type": "output_text",
                                    "text": "Noted."}]}}),
                                28 => json!({"node": 28, "item": said_("Use the old API."),
                                    "from": {"bot": "shop.fix", "turn": 2}}),
                                27 => {
                                    json!({"node": 27, "item": said_("Squash-merge from now on.")})
                                }
                                // What started turn 6, before the last
                                // cleanup; the line above came while it ran.
                                26 => json!({"node": 26, "item": said_("Start the migration.")}),
                                _ => return None,
                            };
                            Some(item)
                        })
                        .collect();
                    json!({"items": items})
                }
                _ => json!({"error": {"code": "unexpected", "message": asked.to_string()}}),
            }
        })
        .await;
        let read = said::said(&client, None, 1000).await.unwrap();
        assert_eq!(
            read,
            json!({"since_ms": 1000, "cut": false, "said": [
                {"bot": "shop.lead", "turn": 8, "text": "Keep Bedrock out of evals.",
                    "project": "shop"},
                {"bot": "shop.lead", "turn": 6, "text": "Squash-merge from now on.",
                    "project": null}]})
        );
        std::fs::remove_dir_all(shop).unwrap();
    }

    #[tokio::test]
    async fn the_cleanup_reads_the_persons_reactions_and_picks_with_what_each_answers() {
        let message = |role: &str, text: &str| {
            let kind = if role == "user" {
                "input_text"
            } else {
                "output_text"
            };
            json!({"type": "message", "role": role, "content": [{"type": kind, "text": text}]})
        };
        let ask = json!({"question": "Ship it now?", "why": "w", "reason": "r",
            "options": [{"label": "Now"}, {"label": "Later"}], "recommended": 1});
        let items = json!([
            // An answer to an ask, with what the ask recommended.
            {"node": 17, "item": message("user", "You asked: Ship it now?\n\nLater")},
            {"node": 16, "item": {"type": "function_call", "name": "ask", "call_id": "c1",
                "arguments": ask.to_string()}},
            // One to an older reply names it; one to the last reply is the
            // emoji alone, and answers the reply before it.
            {"node": 15, "item": message("user", "> Tagged 0.6.0.\n\n🎉")},
            {"node": 14, "item": message("user", "👍")},
            {"node": 13, "item": message("user", "👀"), "from": {"bot": "shop.fix", "turn": 1}},
            {"node": 12, "item": message("assistant",
                "\nSplit the review into a task of its own.\nIt runs now.")},
            {"node": 11, "item": message("user", "Review the cart change.")}]);
        let nodes: Vec<Value> = (items.as_array().unwrap().iter())
            .map(|entry| json!({"node": entry["node"], "turn": if entry["node"] == 11 || entry["node"] == 12 { 3 } else { 4 }}))
            .collect();
        let client =
            daemon_answering(
                move |asked| match asked["op"].as_str().unwrap_or_default() {
                    "bots" => json!({"bots": [{"name": "shop.lead"}], "next_after": null}),
                    "turns" => json!({"turns": [
                {"turn": 4, "started_ms": 2500, "finished_ms": 2600},
                {"turn": 3, "started_ms": 1500, "finished_ms": 1800}], "next_before": null}),
                    "history_nodes" => json!({"nodes": nodes, "next_from": null}),
                    "history_items" => json!({"items": items}),
                    _ => json!({"error": {"code": "unexpected", "message": asked.to_string()}}),
                },
            )
            .await;
        let read = said::said(&client, None, 1000).await.unwrap();
        let entry = |turn: i64, text: &str| json!({"bot": "shop.lead", "turn": turn, "text": text, "project": null});
        let mut answer = entry(4, "You asked: Ship it now?\n\nLater");
        answer["recommended"] = json!("Now");
        let mut older = entry(4, "🎉");
        older["reacted_to"] = json!("Tagged 0.6.0.");
        let mut last = entry(4, "👍");
        last["reacted_to"] = json!("Split the review into a task of its own.");
        assert_eq!(
            read["said"],
            json!([answer, older, last, entry(3, "Review the cart change.")])
        );
    }

    #[tokio::test]
    async fn the_cleanup_counts_only_reactions_still_on_and_reads_anthropic_asks() {
        let message = |role: &str, text: &str| json!({"type": "message", "role": role, "content": [{"type": "text", "text": text}]});
        let ask = json!({"question": "Ship it now?", "why": "w", "reason": "r",
            "options": [{"label": "Now"}, {"label": "Later"}], "recommended": 1});
        let tag = json!({"question": "Tag it too?", "options": [{"label": "Yes"}, {"label": "No"}],
            "recommended": 1});
        let items = json!([
            // An answer against what the ask recommended.
            {"node": 13, "item": message("user", "You asked: Tag it too?\n\nNo")},
            // A 🙌 on the last finished reply, which waited for its call
            // while another reply was written.
            {"node": 12, "item": message("user", "🙌")},
            {"node": 11, "item": message("assistant", "Still working.")},
            // A 🚀 on the older of two replies that begin alike.
            {"node": 10, "item": message("user", "> Done.\n\n🚀")},
            {"node": 9, "item": message("user", "You asked: Ship it now?\n\nNow")},
            // A 👍 on the ask's reply, taken off and put on again: once.
            {"node": 8, "item": message("user", "👍")},
            {"node": 7, "item": message("user", "👍")},
            // An Anthropic reply holds its ask as a `tool_use` block.
            {"node": 6, "item": {"role": "assistant", "content": [
                {"type": "text", "text": "Ready to ship."},
                {"type": "tool_use", "id": "t1", "name": "ask", "input": ask},
                {"type": "tool_use", "id": "t2", "name": "ask", "input": tag}]}},
            // A 🎉 on an older reply, taken off since.
            {"node": 5, "item": message("user", "> Split the review.\n\n🎉")},
            {"node": 4, "item": message("assistant", "Split the review.")},
            {"node": 3, "item": message("assistant", "Done.")},
            {"node": 2, "item": message("assistant", "Done.")}]);
        let nodes: Vec<Value> = (items.as_array().unwrap().iter())
            .map(|entry| json!({"node": entry["node"], "turn": 2}))
            .collect();
        let client =
            daemon_answering(
                move |asked| match asked["op"].as_str().unwrap_or_default() {
                    "bots" => {
                        json!({"bots": [{"name": "shop.lead", "bot_id": 3}], "next_after": null})
                    }
                    "turns" => {
                        json!({"turns": [{"turn": 2, "started_ms": 1500, "finished_ms": 1800}],
                    "next_before": null})
                    }
                    "history_nodes" => json!({"nodes": nodes, "next_from": null}),
                    "history_items" => json!({"items": items}),
                    _ => json!({"error": {"code": "unexpected", "message": asked.to_string()}}),
                },
            )
            .await;
        let home = std::env::temp_dir().join(format!("said-reactions-{}", std::process::id()));
        let store = home.join("state.sqlite");
        let dir = home.join(".agent/sonny/reactions");
        std::fs::create_dir_all(&dir).unwrap();
        let key = reactions::key(&store, 3);
        std::fs::write(
            dir.join(format!("{key}.json")),
            r#"{"6": ["👍", "🙌"], "2": ["🚀"]}"#,
        )
        .unwrap();
        let read = said::said(&client, Some((&home, &store)), 1000)
            .await
            .unwrap();
        let entry =
            |text: &str| json!({"bot": "shop.lead", "turn": 2, "text": text, "project": null});
        let mut answer = entry("You asked: Ship it now?\n\nNow");
        answer["recommended"] = json!("Now");
        let mut liked = entry("👍");
        liked["reacted_to"] = json!("Ready to ship.");
        let mut launched = entry("🚀");
        launched["reacted_to"] = json!("Done.");
        let mut cheered = entry("🙌");
        cheered["reacted_to"] = json!("Ready to ship.");
        let mut against = entry("You asked: Tag it too?\n\nNo");
        against["recommended"] = json!("Yes");
        assert_eq!(
            read["said"],
            json!([against, cheered, launched, answer, liked])
        );
        // Without the windows' file, nothing says which were taken off.
        let read = said::said(&client, None, 1000).await.unwrap();
        let mut older = entry("🎉");
        older["reacted_to"] = json!("Split the review.");
        cheered["reacted_to"] = json!("Still working.");
        assert_eq!(
            read["said"],
            json!([against, cheered, launched, answer, liked, older])
        );
        std::fs::remove_dir_all(home).unwrap();
    }

    #[tokio::test]
    async fn a_message_the_daemon_dated_counts_by_when_it_was_written() {
        let said_ = |text: &str| {
            json!({"type": "message", "role": "user",
            "content": [{"type": "input_text", "text": text}]})
        };
        // A long turn that started before the last cleanup: one line came
        // into it before the cleanup and one after, and one reaction each.
        let items = json!([
            {"node": 15, "item": said_("👍"), "written_ms": 1500},
            {"node": 14, "item": said_("And the docs."), "written_ms": 1400},
            {"node": 13, "item": {"type": "message", "role": "assistant",
                "content": [{"type": "output_text", "text": "On it."}]}, "written_ms": 1300},
            {"node": 12, "item": said_("👀"), "written_ms": 700},
            {"node": 11, "item": said_("Use tabs."), "written_ms": 600},
            {"node": 10, "item": said_("Start the migration."), "written_ms": 400}]);
        let nodes: Vec<Value> = (items.as_array().unwrap().iter())
            .map(|entry| json!({"node": entry["node"], "turn": 6}))
            .collect();
        let client =
            daemon_answering(
                move |asked| match asked["op"].as_str().unwrap_or_default() {
                    "bots" => json!({"bots": [{"name": "shop.lead"}], "next_after": null}),
                    "turns" => {
                        json!({"turns": [{"turn": 6, "started_ms": 400, "finished_ms": null}],
                "next_before": null})
                    }
                    "history_nodes" => json!({"nodes": nodes, "next_from": null}),
                    "history_items" => json!({"items": items}),
                    _ => json!({"error": {"code": "unexpected", "message": asked.to_string()}}),
                },
            )
            .await;
        let read = said::said(&client, None, 1000).await.unwrap();
        let entry =
            |text: &str| json!({"bot": "shop.lead", "turn": 6, "text": text, "project": null});
        let mut liked = entry("👍");
        liked["reacted_to"] = json!("On it.");
        assert_eq!(read["said"], json!([liked, entry("And the docs.")]));
    }

    #[tokio::test]
    async fn a_night_the_person_wrote_is_cleaned_up_though_no_agent_saved() {
        let home = std::env::temp_dir().join(format!("said-changed-{}", std::process::id()));
        let root = home.join(".agents/memory");
        std::fs::create_dir_all(&root).unwrap();
        // The last cleanup started at 1000; nothing was saved since.
        std::fs::write(root.join(".said"), "1000").unwrap();
        let answering = |text: &'static str| {
            daemon_answering(
                move |asked| match asked["op"].as_str().unwrap_or_default() {
                    "bots" => json!({"bots": [{"name": "shop.lead"}], "next_after": null}),
                    "turns" => json!({"turns": [
                    {"turn": 2, "started_ms": 1500, "finished_ms": 1800}], "next_before": null}),
                    "history_nodes" => {
                        json!({"nodes": [{"node": 5, "turn": 2}], "next_from": null})
                    }
                    "history_items" => json!({"items": [{"node": 5, "item": {"type": "message",
                    "role": "user", "content": [{"type": "input_text", "text": text}]},
                    "origin": if text.is_empty() { json!("trigger") } else { Value::Null }}]}),
                    _ => json!({"error": {"code": "unexpected", "message": asked.to_string()}}),
                },
            )
        };
        let wrote = answering("I keep forgetting to rebase.").await;
        let done = said::changed_on(&home, &home, async { Ok::<_, String>(wrote) }).await;
        assert_eq!(done, Ok(json!({"changed": true, "said": true})));
        // Only a trigger's words since: nothing to clean up.
        let quiet = answering("").await;
        let done = said::changed_on(&home, &home, async { Ok::<_, String>(quiet) }).await;
        assert!(done.unwrap_err().starts_with("unchanged: "));
        std::fs::remove_dir_all(home).unwrap();
    }

    #[tokio::test]
    async fn after_a_turn_of_the_persons_a_copy_guesses_their_next_message_for_tab() {
        let asked = Arc::new(std::sync::Mutex::new(Vec::new()));
        let seen = asked.clone();
        let client = daemon_answering(move |asked| {
            seen.lock().unwrap().push(asked.clone());
            match (asked["op"].as_str(), asked["bot"].as_str()) {
                (Some("fork"), _) => json!({"bot_id": 9, "head": 4}),
                (Some("submit"), Some("b")) => json!({"turn": 1}),
                (Some("submit"), _) => json!({"turn": 1, "handle": "turn:copy/1"}),
                (Some("wait"), _) => json!({"results": {"turn:copy/1":
                    {"status": "completed", "text": "Suggestion: run the tests"}}}),
                _ => json!({}),
            }
        })
        .await;
        let mut app = app(client);
        app.input.set("fix the bug and run tests");
        app.submit().await;
        app.daemon(turn_event("accepted", 1, json!({}))).await;
        app.daemon(turn_event(
            "turn_finished",
            1,
            json!({"status": "completed"}),
        ))
        .await;
        for _ in 0..200 {
            app.suggested_now().await;
            if app.suggestion.is_some() {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        assert_eq!(app.suggested(), Some("run the tests"));
        let (rows, _) = app.composer(80);
        let row: String = rows[0].spans.iter().map(|s| s.content.as_ref()).collect();
        assert_eq!(row, "› run the tests  tab");
        // The copy forked from the bot, allowed no call, and was deleted.
        let ops = asked.lock().unwrap().clone();
        let fork = ops.iter().find(|op| op["op"] == "fork").unwrap();
        assert_eq!(fork["source"], "b");
        assert_eq!(fork["allow"], json!([]));
        assert!(fork["expire_ms"].is_u64());
        let copy = fork["bot"].as_str().unwrap();
        assert!(
            ops.iter()
                .any(|op| op["op"] == "delete" && op["bot"] == copy)
        );
        // Something typed hides it; emptied again, it is back.
        app.input.set("x");
        assert_eq!(app.suggested(), None);
        app.input.clear();
        // Tab takes it into the composer, unsent.
        app.key(Event::Key(ratatui::crossterm::event::KeyEvent::new(
            KeyCode::Tab,
            KeyModifiers::NONE,
        )))
        .await;
        assert_eq!(&*app.input, "run the tests");
        assert_eq!(app.suggestion, None);
        assert_eq!(
            ops.iter().filter(|op| op["op"] == "submit").count(),
            2,
            "the person's and the copy's"
        );
    }

    #[tokio::test]
    async fn goal_words_go_to_the_agent_as_typed_and_goal_alone_shows_or_clears() {
        let asked = Arc::new(std::sync::Mutex::new(Vec::new()));
        let seen = asked.clone();
        let client = daemon_answering(move |asked| {
            seen.lock().unwrap().push(asked.clone());
            match asked["op"].as_str() {
                Some("inspect") => json!({"bot_id": 1, "workspace": "/"}),
                Some("submit") => json!({"turn": 1}),
                _ => json!({}),
            }
        })
        .await;
        let mut app = app(client);
        let store = std::env::temp_dir().join(format!("goal-window-{}", std::process::id()));
        app.options.store = store.clone();
        app.input.set("/goal");
        app.submit().await;
        assert_eq!(
            app.notice.as_deref(),
            Some("no goal; /goal WHAT asks the agent to work until WHAT is done")
        );
        // The words reach the agent as the person typed them.
        app.input.set("/goal the tests pass");
        app.submit().await;
        let ops = asked.lock().unwrap().clone();
        let submits: Vec<_> = ops.iter().filter(|op| op["op"] == "submit").collect();
        assert_eq!(submits.len(), 1);
        assert!(submits[0].to_string().contains("/goal the tests pass"));
        // The agent sets one with its tool; /goal shows it and clears it.
        let set = json!({"set": {"done_when": "the tests pass", "check": "true"}});
        lookout::goals::call(&store, &app.client, "b", &set)
            .await
            .unwrap();
        app.input.set("/goal");
        app.submit().await;
        assert_eq!(
            app.notice.as_deref(),
            Some("goals: b: done when the tests pass; /goal clear clears this agent's")
        );
        app.input.set("/goal clear");
        app.submit().await;
        assert_eq!(app.notice.as_deref(), Some("goal cleared"));
        assert!(lookout::goals::read(&store, "b").is_none());
        app.input.set("/goal clear");
        app.submit().await;
        assert_eq!(app.notice.as_deref(), Some("no goal to clear"));
        let ops = asked.lock().unwrap().clone();
        assert_eq!(ops.iter().filter(|op| op["op"] == "submit").count(), 1);
        std::fs::remove_dir_all(lookout::subs::dir(&store)).unwrap();
    }

    #[tokio::test]
    async fn a_turn_the_person_steered_from_here_is_theirs() {
        let next = std::sync::atomic::AtomicI64::new(2);
        let client = daemon_answering(move |asked| match asked["op"].as_str() {
            Some("submit") => {
                json!({"turn": next.fetch_add(1, std::sync::atomic::Ordering::SeqCst)})
            }
            _ => json!({}),
        })
        .await;
        let mut app = app(client);
        // The lead's turn 1 runs; the person steers it, a submission of its own.
        app.daemon(turn_event("accepted", 1, json!({}))).await;
        app.input.set("and the reversed range too");
        app.submit().await;
        assert_eq!(app.yours, Some(1));
        app.daemon(turn_event("turn_finished", 2, json!({"status": "steered"})))
            .await;
        assert!(app.suggesting.is_none());
        app.daemon(turn_event(
            "turn_finished",
            1,
            json!({"status": "completed"}),
        ))
        .await;
        assert!(app.suggesting.is_some());
    }

    #[tokio::test]
    async fn a_turn_the_person_did_not_start_here_is_not_followed_by_a_guess() {
        let client = daemon_answering(|_| json!({})).await;
        let mut app = app(client);
        // Started by the lead, the lookout, or another window.
        app.daemon(turn_event("accepted", 3, json!({}))).await;
        app.daemon(turn_event(
            "turn_finished",
            3,
            json!({"status": "completed"}),
        ))
        .await;
        assert!(app.suggesting.is_none());
        // Nor one of the person's that failed or was stopped.
        for status in ["failed", "interrupted"] {
            app.yours = Some(4);
            app.daemon(turn_event("accepted", 4, json!({}))).await;
            app.daemon(turn_event("turn_finished", 4, json!({"status": status})))
                .await;
            assert!(app.suggesting.is_none(), "{status}");
        }
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
        app.answered("You asked: Keep the old API?\n\nDrop it", None);
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
        // Down picks the next to answer; what is typed then answers it,
        // named over the words as the sent answer will be.
        app.chatting = false;
        app.step_ask(true);
        assert_eq!(app.asking().map(|a| a.id.as_str()), Some("c3"));
        app.input.set("1");
        assert_eq!(
            app.answering_over(80).unwrap().to_string(),
            "  answering “Keep the old API?” (2 of 2)"
        );
        app.step_ask(true);
        assert_eq!(app.asking().map(|a| a.id.as_str()), Some("c2"));
        // The person's own words landing move on past them: out of the
        // composer and the count, their options still answering them.
        app.answered("how is it going?", None);
        assert!(app.asks.is_empty() && app.asking().is_none());
        assert_eq!(app.aside.len(), 2);
        app.input.clear();
        app.press("answer/0/c2").await;
        assert_eq!(
            sent.lock().unwrap().last().unwrap(),
            "You asked: Tag it?\n\nKeep it"
        );
        assert_eq!(app.aside, [ask("c3", "Keep the old API?")]);
        // Another agent's words, or a reaction, move nothing on.
        app.asks = vec![ask("c4", "Ship it?")];
        app.answered("Done: tests pass", Some("tally.halves"));
        app.answered(&reactions::message("👍", None), None);
        assert_eq!(app.asks.len(), 1);
        // One before the question in focus closing, from another window
        // or its card, keeps the focus on the question being answered.
        app.asks = vec![
            ask("c5", "Tag it?"),
            ask("c6", "Bump?"),
            ask("c7", "Notes?"),
        ];
        app.focus = Some("c6".into());
        app.answered("You asked: Tag it?\n\nKeep it", None);
        assert_eq!(app.asking().map(|a| a.id.as_str()), Some("c6"));
        app.press("answer/0/c7").await;
        assert_eq!(app.asking().map(|a| a.id.as_str()), Some("c6"));
    }

    #[tokio::test]
    async fn the_question_picked_stays_picked_when_the_conversation_is_drawn_again() {
        let asked = |id: &str, question: &str| {
            json!({"type": "function_call", "name": "ask", "call_id": id,
                "arguments": json!({"question": question}).to_string()})
        };
        let items = json!({
            "1": asked("a1", "Tag it?"),
            "2": {"type": "function_call_output", "call_id": "a1", "output": "Asked."},
            "3": asked("a2", "Bump?"),
            "4": {"type": "function_call_output", "call_id": "a2", "output": "Asked."},
        });
        let client = daemon_answering(move |asked| match asked["op"].as_str() {
            Some("history_nodes") => {
                json!({"nodes": (1..=4).rev().map(|node| json!({"node": node}))
                .collect::<Vec<_>>(), "next_from": null})
            }
            _ => json!({"items": asked["nodes"].as_array().into_iter().flatten()
                .map(|node| json!({"node": node, "item": items[node.to_string()]}))
                .collect::<Vec<_>>()}),
        })
        .await;
        let mut app = app(client);
        app.history().await;
        app.step_ask(true);
        assert_eq!(app.asking().map(|a| a.id.as_str()), Some("a2"));
        // A repaint (the theme switched) draws it all from the start.
        app.seen_node = 0;
        app.history().await;
        assert_eq!(app.asks.len(), 2);
        assert_eq!(app.asking().map(|a| a.id.as_str()), Some("a2"));
    }

    #[tokio::test]
    async fn another_agents_message_leaves_the_latest_question_open() {
        // Newest first, as `history_nodes` gives them.
        let entries = json!([
            {"node": 3, "from": {"bot": "tally.halves"}, "item": {"type": "message",
                "role": "user", "content": [{"type": "input_text", "text": "Done: tests pass"}]}},
            {"node": 2, "item": {"type": "function_call_output", "call_id": "a1",
                "output": "Asked."}},
            {"node": 1, "item": {"type": "function_call", "name": "ask", "call_id": "a1",
                "arguments": "{\"question\":\"Tag 0.4.1?\"}"}},
        ]);
        let client = daemon_answering(move |asked| match asked["op"].as_str() {
            Some("history_nodes") => {
                json!({"nodes": [{"node": 3}, {"node": 2}, {"node": 1}], "next_from": null})
            }
            _ => json!({"items": entries.clone()}),
        })
        .await;
        let app = app(client);
        assert_eq!(
            app.latest("halves").await.ask.as_deref(),
            Some("Tag 0.4.1?")
        );
    }

    /// A conversation of four turns: the person's, one another agent's
    /// message started, in which the agent asks, one the lead's task
    /// news started, and the person's again.
    async fn zen_daemon() -> Arc<Client> {
        zen_daemon_from(1).await
    }

    /// The same conversation, its history page starting at node `first`.
    async fn zen_daemon_from(first: i64) -> Arc<Client> {
        let say = |role: &str, text: &str| {
            let kind = if role == "user" {
                "input_text"
            } else {
                "output_text"
            };
            json!({"type": "message", "role": role, "content": [{"type": kind, "text": text}]})
        };
        let ask = json!({"question": "Leave zen to look at the release?",
            "why": "shop.fix says the release build broke.",
            "options": [{"label": "Now", "explanation": "We stop here."},
                {"label": "After", "explanation": "It waits for the focus."}],
            "recommended": 1, "reason": "Nobody waits on it today."});
        let news = "Task updates: what happened in your tasks since you last heard.\n\
            - calc.neg: turn:calc.neg/3 completed";
        let items: Vec<(i64, i64, Value)> = vec![
            (1, 1, say("user", "Fix the rounding")),
            (
                2,
                1,
                json!({"type": "function_call", "name": "shell", "call_id": "s1",
                "arguments": "{\"command\":\"cargo test\"}"}),
            ),
            (
                3,
                1,
                json!({"type": "function_call_output", "call_id": "s1", "output": "ok"}),
            ),
            (4, 1, say("assistant", "Rounding now, and the tests pass.")),
            (5, 2, say("user", "The release build broke.")),
            (6, 2, say("assistant", "Noted what shop.fix said.")),
            (9, 3, say("user", news)),
            (10, 3, say("assistant", "calc.neg is done.")),
            (11, 4, say("user", "Push it")),
            (12, 4, say("assistant", "Pushed as 4f2c1a0.")),
            // Asked last: the person's own words after it would set it
            // aside, the conversation having moved on past it.
            (
                13,
                4,
                json!({"type": "function_call", "name": "ask", "call_id": "a1",
                "arguments": ask.to_string()}),
            ),
            (
                14,
                4,
                json!({"type": "function_call_output", "call_id": "a1", "output": "Asked."}),
            ),
        ];
        daemon_answering(move |asked| match asked["op"].as_str() {
            Some("history_nodes") => json!({"nodes": items.iter().rev()
                .filter(|(node, _, _)| *node >= first)
                .map(|(node, turn, _)| json!({"node": node, "turn": turn}))
                .collect::<Vec<_>>(), "next_from": null}),
            Some("prompts") => match asked["turn"].as_i64() {
                Some(2) => json!({"prompts": [{"turn": 2, "text": "The release build broke.",
                    "from": {"bot": "shop.fix", "turn": 1}}]}),
                turn => json!({"prompts": [{"turn": turn, "text": "Fix the rounding"}]}),
            },
            Some("history_items") => json!({"items": asked["nodes"].as_array().into_iter()
                .flatten()
                .filter_map(|node| items.iter().find(|(n, _, _)| Some(*n) == node.as_i64()))
                .map(|(node, _, item)| match node {
                    5 => json!({"node": node, "item": item, "from": {"bot": "shop.fix"}}),
                    _ => json!({"node": node, "item": item}),
                })
                .collect::<Vec<_>>()}),
            _ => json!({}),
        })
        .await
    }

    #[tokio::test]
    async fn zen_draws_only_the_persons_turns_and_the_agents_questions() {
        let mut app = app(zen_daemon().await);
        app.width = 100;
        app.history().await;
        let rows = drawn(&mut app).join("\n");
        // Without zen, everything that moves the work on.
        assert!(rows.contains("From shop.fix"), "{rows}");
        assert!(rows.contains("Noted what shop.fix said."), "{rows}");
        assert!(rows.contains("calc.neg is done."), "{rows}");
        let mut app = app_in_zen(zen_daemon().await);
        app.history().await;
        let rows = drawn(&mut app).join("\n");
        for said in [
            "Fix the rounding",
            "Rounding now, and the tests pass.",
            "Leave zen to look at the release?",
            "Push it",
            "Pushed as 4f2c1a0.",
        ] {
            assert!(rows.contains(said), "{said} is drawn in {rows}");
        }
        for unseen in [
            "From shop.fix",
            "Noted",
            "calc.neg",
            "cargo test",
            "Task updates",
        ] {
            assert!(!rows.contains(unseen), "{unseen} is not drawn in {rows}");
        }
        // The question still needs the person.
        assert_eq!(app.asks.len(), 1);
        // No welcome above it: zen draws only what is said.
        app.welcome = true;
        assert!(app.welcome().0.is_empty());
        app.zen = None;
        assert!(!app.welcome().0.is_empty());
    }

    #[tokio::test]
    async fn in_zen_no_guess_at_the_next_message_is_made() {
        let mut app = app_in_zen(zen_daemon().await);
        app.suggest();
        assert!(app.suggesting.is_none());
        app.zen = None;
        app.suggest();
        assert!(app.suggesting.take().is_some_and(|job| {
            job.abort();
            true
        }));
    }

    /// The test agent's key, as the store and its id would make it.
    const ZEN_KEY: &str = "0000-2";

    fn app_in_zen(client: Arc<Client>) -> App {
        let mut app = app(client);
        app.reactions_key = Some(ZEN_KEY.into());
        app.width = 100;
        zen::set(Some(ZEN_KEY), Some("the rounding")).unwrap();
        app.zen = Some("the rounding".into());
        app
    }

    #[tokio::test]
    async fn in_zen_a_turn_the_person_did_not_start_runs_unseen() {
        let mut app = app_in_zen(zen_daemon().await);
        // The lookout's: no status, no words, no line when it ends.
        app.daemon(turn_event(
            "accepted",
            3,
            json!({"request_id": "lookout-ab12"}),
        ))
        .await;
        assert!(app.zen_hides());
        assert_eq!(app.status_line().to_string(), "");
        app.daemon(json!({"event": "text_delta", "bot": "b", "turn": 3,
            "text": "calc.neg is done."}))
            .await;
        app.daemon(turn_event("message", 3, json!({"node": 10})))
            .await;
        app.daemon(turn_event(
            "tool_started",
            3,
            json!({"name": "shell", "call_id": "s9",
            "arguments": "{\"command\":\"ls\"}"}),
        ))
        .await;
        app.daemon(turn_event("tool_completed", 3, json!({"call_id": "s9"})))
            .await;
        app.daemon(turn_event(
            "turn_finished",
            3,
            json!({"status": "completed"}),
        ))
        .await;
        assert_eq!(drawn(&mut app), Vec::<String>::new());
        // Another agent's message starts one: the same.
        app.daemon(turn_event("accepted", 2, json!({"node": 5})))
            .await;
        assert!(app.zen_hides());
        app.daemon(turn_event(
            "turn_finished",
            2,
            json!({"status": "failed",
            "error": "provider_unavailable"}),
        ))
        .await;
        assert_eq!(drawn(&mut app), Vec::<String>::new());
        // The person's own turn is drawn as ever, its status too.
        app.daemon(turn_event("accepted", 4, json!({"node": 11})))
            .await;
        assert!(!app.zen_hides());
        assert!(app.status_line().to_string().contains("Working"));
        app.daemon(json!({"event": "text_delta", "bot": "b", "turn": 4,
            "text": "Pushed as 4f2c1a0."}))
            .await;
        app.daemon(turn_event("message", 4, json!({"node": 12})))
            .await;
        app.daemon(turn_event(
            "turn_finished",
            4,
            json!({"status": "completed"}),
        ))
        .await;
        let rows = drawn(&mut app).join("\n");
        assert!(rows.contains("Push it"), "{rows}");
        assert!(rows.contains("Pushed as 4f2c1a0."), "{rows}");
    }

    #[tokio::test]
    async fn a_page_starting_inside_a_turn_asks_who_started_it() {
        let mut app = app_in_zen(zen_daemon_from(6).await);
        app.history().await;
        let rows = drawn(&mut app).join("\n");
        assert!(!rows.contains("Noted what shop.fix said."), "{rows}");
        assert!(rows.contains("Pushed as 4f2c1a0."), "{rows}");
    }

    #[tokio::test]
    async fn zen_set_in_another_window_is_taken_up_at_the_next_turn() {
        let mut app = app(zen_daemon().await);
        app.reactions_key = Some(ZEN_KEY.into());
        // A reply drawn before zen, from a turn zen will hide.
        app.saw_reply(5, "Noted what shop.fix said.");
        app.last_reply = "Noted what shop.fix said.".into();
        zen::set(Some(ZEN_KEY), Some("the rounding")).unwrap();
        app.daemon(turn_event("accepted", 4, json!({"node": 11})))
            .await;
        assert_eq!(app.zen.as_deref(), Some("the rounding"));
        assert!(app.repaint);
        // Reply, copy and react find their targets again in what zen draws.
        assert!(app.replies.is_empty());
        assert!(app.last_reply.is_empty());
        app.repaint = false;
        zen::set(Some(ZEN_KEY), None).unwrap();
        app.daemon(turn_event("steered", 4, json!({"node": 11})))
            .await;
        assert_eq!(app.zen, None);
        assert!(app.repaint);
    }

    #[tokio::test]
    async fn zen_the_agent_set_when_asked_in_words_is_taken_up_as_its_turn_ends() {
        let mut app = app(zen_daemon().await);
        app.reactions_key = Some(ZEN_KEY.into());
        app.daemon(turn_event("accepted", 4, json!({"node": 11})))
            .await;
        assert_eq!(app.zen, None);
        // The person asked in words; the agent wrote the focus itself.
        zen::set(Some(ZEN_KEY), Some("the rounding")).unwrap();
        app.daemon(turn_event(
            "turn_finished",
            4,
            json!({"status": "completed"}),
        ))
        .await;
        assert_eq!(app.zen.as_deref(), Some("the rounding"));
        assert!(app.repaint);
        assert_eq!(app.notice.as_deref(), Some(zen_notice(true).as_str()));
        // The agent puts the person's scribble in its own words, as with
        // a goal; the footer takes them at the turn's end, nothing redrawn.
        app.repaint = false;
        zen::set(Some(ZEN_KEY), Some("rounding in total()")).unwrap();
        app.daemon(turn_event(
            "turn_finished",
            4,
            json!({"status": "completed"}),
        ))
        .await;
        assert_eq!(app.zen.as_deref(), Some("rounding in total()"));
        assert!(!app.repaint);
    }

    #[tokio::test]
    async fn the_persons_words_steered_into_a_turn_make_it_theirs() {
        let mut app = app_in_zen(zen_daemon().await);
        app.daemon(turn_event("accepted", 2, json!({"node": 5})))
            .await;
        assert!(app.zen_hides());
        app.daemon(turn_event("steered", 2, json!({"node": 11})))
            .await;
        assert!(!app.zen_hides());
    }

    #[tokio::test]
    async fn zen_goes_to_the_agent_as_typed_and_alone_leaves() {
        let asked = Arc::new(std::sync::Mutex::new(Vec::new()));
        let seen = asked.clone();
        let client = daemon_answering(move |asked| {
            seen.lock().unwrap().push(asked.clone());
            match asked["op"].as_str() {
                Some("submit") => json!({"turn": 1}),
                _ => json!({}),
            }
        })
        .await;
        let mut app = app(client);
        app.bot = "zen-test.lead".into();
        app.reactions_key = Some("0000-9".into());
        app.input.set("/zen");
        app.submit().await;
        assert_eq!(
            app.notice.as_deref(),
            Some("/zen FOCUS: say the one thing to talk about")
        );
        app.input.set("/zen the receipts' currency");
        app.submit().await;
        assert_eq!(app.zen.as_deref(), Some("the receipts' currency"));
        assert_eq!(
            zen::focus(Some("0000-9")).as_deref(),
            Some("the receipts' currency")
        );
        assert!(app.repaint && app.input.is_empty());
        let footer = app.footer_line(100).to_string();
        assert!(
            footer.starts_with("  zen · the receipts' currency"),
            "{footer}"
        );
        assert!(!footer.contains("zen-test.lead"), "{footer}");
        app.repaint = false;
        app.input.set("/zen");
        app.submit().await;
        assert_eq!(app.zen, None);
        assert_eq!(zen::focus(Some("0000-9")), None);
        assert!(app.repaint);
        let prompts: Vec<String> = (asked.lock().unwrap().iter())
            .filter(|op| op["op"] == "submit")
            .map(|op| op["prompt"].as_str().unwrap_or("").to_owned())
            .collect();
        assert_eq!(prompts, ["/zen the receipts' currency", "/zen off"]);
    }

    #[tokio::test]
    async fn in_zen_the_footer_counts_only_what_needs_the_person() {
        let mut app = app_in_zen(zen_daemon().await);
        app.history().await;
        let (footer, counter) = app.footer(60);
        let footer = footer.to_string();
        assert!(footer.starts_with("  zen · the rounding"), "{footer}");
        assert!(footer.ends_with("? 1 needs you  "), "{footer}");
        assert_eq!(footer.width(), 60);
        let [(counter, "status")] = &counter[..] else {
            panic!("{counter:?}")
        };
        let columns: Vec<char> = footer.chars().collect();
        let counted: String = columns[counter.start as usize..counter.end as usize]
            .iter()
            .collect();
        assert_eq!(counted, "? 1 needs you");
    }

    #[test]
    fn zen_puts_the_conversation_in_the_middle_of_a_wide_window() {
        let rt = tokio::runtime::Runtime::new().unwrap();
        let mut app = rt.block_on(async { app(zen_daemon().await) });
        let wide = Rect::new(0, 0, 160, 40);
        assert_eq!(app.split(wide).0, wide);
        app.zen = Some("the rounding".into());
        assert_eq!(app.split(wide).0, Rect::new(30, 0, 100, 40));
        // A window narrower than the column keeps all of it.
        let narrow = Rect::new(0, 0, 80, 40);
        assert_eq!(app.split(narrow).0, narrow);
        // The margin stays away in zen.
        app.pane = Some(pane::Pane::margin());
        assert_eq!(app.split(wide), (Rect::new(30, 0, 100, 40), None));
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
        assert_eq!(rows[at + 4], "┃    Brew gets it tonight.");
        assert_eq!(rows[at + 8], "┃ Why 1: Nothing else is due.");
        // Its answer landing, sent from here or another window, closes it.
        app.answered("You asked: Tag 0.4.1?\n\nYes", None);
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
        // `/retry` offers it; Enter sends the person's approval of that
        // exact call, once.
        app.running = None;
        app.input.set("/retry");
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
            "I approve one retry of the action the reviewer denied: shell {\"command\":\"git push --force origin main\"}"
        ));
        assert!(prompt.contains("Do not assume this also authorizes similar operations"));
        // Nothing left to approve.
        app.input.set("/retry");
        app.submit().await;
        assert_eq!(app.notice.as_deref(), Some("no recent reviewer denials"));
    }

    #[test]
    fn a_call_the_person_allows_is_recorded_under_it() {
        let shell = json!({"command": "cargo test --release"});
        let once = transcript::allowed("shell", &shell, None, "/w", 80);
        assert_eq!(once.len(), 1);
        assert_eq!(
            once[0].to_string(),
            "✓ Request allowed for cargo test --release"
        );
        let kept = transcript::allowed("shell", &shell, Some("cargo test"), "/w", 80);
        assert_eq!(
            kept[1].to_string(),
            "  └ cargo test runs outside the sandbox from now on"
        );
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
        assert_eq!(asked["delivery"], "reject");
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
            (&json!("reject"), &json!("steer"))
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

    #[tokio::test]
    async fn each_message_a_call_streams_is_drawn_apart_and_found_by_its_node() {
        let client = daemon_with(json!({
            "2": {"type": "message", "role": "assistant",
                  "content": [{"type": "output_text", "text": "Let me check the file."}]},
            "3": {"type": "message", "role": "assistant",
                  "content": [{"type": "output_text", "text": "It is fine."}]}
        }))
        .await;
        let mut app = app(client);
        let event =
            |event: &str, data: Value| json!({"event": event, "bot": "b", "turn": 1, "data": data});
        app.daemon(event("accepted", json!({}))).await;
        for (item, text) in [
            (1, "Let me check"),
            (1, " the file."),
            (2, "It is"),
            (2, " fine."),
        ] {
            let mut delta = event("text_delta", json!({}));
            (delta["item"], delta["text"]) = (json!(item), json!(text));
            app.daemon(delta).await;
        }
        let drawn = |app: &mut App| -> Vec<String> {
            let open = app.open_rows(80);
            (app.pending.iter().chain(&open))
                .map(|line| line.to_string())
                .collect()
        };
        let rows = |drawn: &[String], text: &str| drawn.iter().filter(|l| l.contains(text)).count();
        // Apart while they stream, and as they were kept.
        for at in [None, Some(2), Some(3)] {
            if let Some(node) = at {
                app.daemon(event("message", json!({"node": node}))).await;
            }
            let drawn = drawn(&mut app);
            assert_eq!(
                [
                    rows(&drawn, "Let me check the file."),
                    rows(&drawn, "It is fine."),
                    rows(&drawn, "Let me check"),
                ],
                [1, 1, 1],
                "{drawn:?}"
            );
        }
        // Reactions find each message by its node.
        assert!(app.kept(screen::Tag::End(2)));
        assert!(!app.kept(screen::Tag::Streamed(1)));
    }

    #[tokio::test]
    async fn a_call_retried_after_two_messages_draws_what_it_kept() {
        let client = daemon_with(json!({
            "2": {"type": "message", "role": "assistant",
                  "content": [{"type": "output_text", "text": "All good."}]}
        }))
        .await;
        let mut app = app(client);
        let event =
            |event: &str, data: Value| json!({"event": event, "bot": "b", "turn": 1, "data": data});
        app.daemon(event("accepted", json!({}))).await;
        let said = |item: u64, text: &str| {
            let mut delta = event("text_delta", json!({}));
            (delta["item"], delta["text"]) = (json!(item), json!(text));
            delta
        };
        app.daemon(said(1, "Let me check the file.")).await;
        app.daemon(said(2, "It is")).await;
        // The call failed; its retry keeps one message, unlike the first.
        app.daemon(event("retry", json!({"error": "provider_http_529"})))
            .await;
        app.daemon(said(1, "All good.")).await;
        app.daemon(event("message", json!({"node": 2}))).await;
        let open = app.open_rows(80);
        let drawn: Vec<String> = (app.pending.iter().chain(&open))
            .map(|line| line.to_string())
            .collect();
        assert_eq!(
            drawn.iter().filter(|l| l.contains("All good.")).count(),
            1,
            "{drawn:?}"
        );
        assert!(app.notice.as_deref().is_some_and(|n| n.contains("retried")));
        assert!(app.ahead.is_empty());
    }

    #[tokio::test]
    async fn a_retry_whose_message_streams_under_another_item_draws_it_once() {
        let client = daemon_with(json!({
            "3": {"type": "message", "role": "assistant",
                  "content": [{"type": "output_text", "text": "All good."}]}
        }))
        .await;
        let mut app = app(client);
        let event =
            |event: &str, data: Value| json!({"event": event, "bot": "b", "turn": 1, "data": data});
        app.daemon(event("accepted", json!({}))).await;
        let said = |item: u64, text: &str| {
            let mut delta = event("text_delta", json!({}));
            (delta["item"], delta["text"]) = (json!(item), json!(text));
            delta
        };
        app.daemon(said(1, "All good.")).await;
        // The retry thinks first, so its message is the call's second item.
        app.daemon(event("retry", json!({"error": "provider_http_529"})))
            .await;
        app.daemon(said(2, "All good.")).await;
        app.daemon(event("message", json!({"node": 3}))).await;
        let open = app.open_rows(80);
        let drawn: Vec<String> = (app.pending.iter().chain(&open))
            .map(|line| line.to_string())
            .collect();
        assert_eq!(
            drawn.iter().filter(|l| l.contains("All good.")).count(),
            1,
            "{drawn:?}"
        );
        assert!(app.ahead.is_empty());
        assert!(!app.kept(screen::Tag::Streamed(1)));
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
        let footer = app.footer_line(120).to_string();
        assert!(
            footer.contains("📎") && !footer.contains("📎 2"),
            "{footer}"
        );
        app.daemon(turn_event("attached", 6, json!({"status": "attached"})))
            .await;
        assert!(app.footer_line(120).to_string().contains("📎 2"));
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
    async fn what_failed_in_the_background_is_red_left_of_what_runs() {
        let client = daemon_with(json!([])).await;
        let mut app = app(client);
        app.behind = (2, Some(Instant::now()));
        app.failed = 1;
        let line = app.footer_line(140);
        let text = line.to_string();
        assert!(text.contains("✗ 1 failed  ↻ 2 in background"), "{text}");
        // The mark red at full strength, the words muted.
        let mark = line.spans.iter().find(|s| s.content == "✗").unwrap();
        assert_eq!(mark.style.fg, Some(Color::Red));
        assert!(
            !mark.style.add_modifier.contains(Modifier::DIM),
            "red, not dimmed"
        );
        let words = line
            .spans
            .iter()
            .find(|s| s.content == " 1 failed")
            .unwrap();
        assert!(words.style.add_modifier.contains(Modifier::DIM));
        app.failed = 2;
        assert!(app.footer_line(140).to_string().contains("✗ 2 failed"));
        app.failed = 0;
        assert!(!app.footer_line(140).to_string().contains('✗'));
    }

    #[tokio::test]
    async fn a_reply_quotes_the_message_before_what_is_being_written() {
        let client = daemon_with(json!([])).await;
        let mut app = app(client);
        app.input.set("Yes, merge it");
        let said: String = (1..=8).map(|n| format!("line {n}\n")).collect();
        app.quote(&said);
        let quoted = "> line 1\n> line 2\n> line 3\n> line 4\n> line 5\n> line 6\n> …";
        assert_eq!(&*app.input, format!("{quoted}\n\nYes, merge it"));
        app.input.clear();
        app.quote("Merged it.\n\nThe tests pass.");
        assert_eq!(&*app.input, "> Merged it.\n>\n> The tests pass.\n\n");
    }

    #[tokio::test]
    async fn the_footer_names_a_profile_only_when_it_is_not_the_seats_own() {
        let client = daemon_with(json!([])).await;
        let mut app = app(client);
        let dir = std::env::temp_dir().join(format!("sonny-footer-profile-{}", std::process::id()));
        std::fs::create_dir_all(dir.join(".agents")).unwrap();
        std::fs::write(dir.join(".agents/project.toml"), "name = \"top\"\n").unwrap();
        app.workspace = dir.to_string_lossy().into_owned();
        app.project = Some("top".into());
        assert!(!app.footer_line(140).to_string().contains("lead ·"));
        // Read again once the agent or its folder changes.
        std::fs::write(
            dir.join(".agents/project.toml"),
            "name = \"top\"\nprofile = \"superlead\"\n",
        )
        .unwrap();
        app.bot = "top.lead".into();
        let text = app.footer_line(140).to_string();
        assert!(
            text.contains(&format!(" {} · superlead · ", models::label(&app.model))),
            "{text}"
        );
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[tokio::test]
    async fn a_failure_is_counted_until_the_person_looks_at_it() {
        let client = daemon_with(json!([])).await;
        let mut app = app(client);
        app.count_failures(vec!["Trigger/nightly/turn:calc.lead/4/bot_busy".into()]);
        assert_eq!(app.failed, 1);
        // /background shows it: the footer stops counting it, also when
        // counted again while it lasts.
        app.background();
        assert_eq!(app.failed, 0);
        app.count_failures(vec!["Trigger/nightly/turn:calc.lead/4/bot_busy".into()]);
        assert_eq!(app.failed, 0);
        // The next fire failing too is news.
        app.count_failures(vec!["Trigger/nightly/turn:calc.lead/9/bot_busy".into()]);
        assert_eq!(app.failed, 1);
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
        app.input.set("👍");
        app.submit().await;
        assert_eq!(app.notice.as_deref(), Some("no reply to react to yet"));
        for (node, reply) in [
            (4, "Merged it.\nThe notes are updated."),
            (8, "Tagged 0.4.0."),
        ] {
            let lines = transcript::reply(reply, 80, std::path::Path::new("/"));
            app.commit_said(node, lines);
            app.saw_reply(node, reply);
        }
        app.flush();
        // The last reply: the emoji alone, attached, right under it.
        app.input.set(":+1:");
        app.submit().await;
        let first = sent.lock().unwrap()[0].clone();
        assert_eq!(first["prompt"], "👍");
        assert_eq!(first["delivery"], "attach");
        assert!(first["model"].is_null() && first["workspace"].is_null());
        assert!(app.input.is_empty());
        assert_eq!(
            drawn(&mut app)[4..],
            ["• Tagged 0.4.0.", "   👍 "],
            "no blank row above it"
        );
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
        let rows = drawn(&mut app);
        assert_eq!(
            rows[1..4],
            ["• Merged it.", "  The notes are updated.", "   🎉 "],
            "{rows:?}"
        );
        // The same emoji again takes it off, telling the bot nothing.
        app.input.set("👍");
        app.submit().await;
        assert_eq!(sent.lock().unwrap().len(), 2);
        assert_eq!(drawn(&mut app).last().unwrap(), "• Tagged 0.4.0.");
        assert!(app.reactions.on(8).is_empty());
        // A reply no longer in the conversation gets it all the same, and
        // no row says so: the footer does.
        app.saw_reply(10, "Gone.");
        app.input.set("🚀");
        app.submit().await;
        assert_eq!(app.reactions.on(10), ["🚀"]);
        assert_eq!(drawn(&mut app).last().unwrap(), "• Tagged 0.4.0.");
        assert_eq!(app.notice.as_deref(), Some("🚀 on “Gone.”"));
        // Words with an emoji are a message, sent as one.
        app.input.set("👍 ship it");
        app.submit().await;
        assert_eq!(sent.lock().unwrap()[3]["prompt"], "👍 ship it");
        assert_eq!(sent.lock().unwrap()[3]["delivery"], "reject");
    }

    #[tokio::test]
    async fn the_person_s_message_shows_what_it_quotes_under_its_words() {
        let client = daemon_answering(|_| json!({})).await;
        let mut app = app(client);
        app.width = 80;
        let quotes = "\n\ncalc.fix replied:\n> Fixed **it**.\n> 1\n> 2\n> 3\n> 4\n> 5\n> 6\n> 7";
        app.land(6, ("ship that", quotes), None, false, false);
        let rows = drawn(&mut app);
        let rows: Vec<&str> = rows.iter().map(|row| row.trim_end()).collect();
        assert_eq!(
            rows,
            [
                "",
                "› ship that",
                "",
                "  calc.fix replied:",
                "  ▌ Fixed it.",
                "  ▌ 1",
                "  ▌ 2",
                "  ▌ 3",
                "  ▌ 4",
                "  ▌ 5",
                "  ▌ … +2 lines",
                "",
            ]
        );
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
        app.land(6, ("and tag it", ""), None, false, false);
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
        assert_eq!(drawn(&mut app), ["", "› merged, thanks", "", "   👀 👍 "]);
        app.daemon(react(1, "c3", json!({"emoji": "👀", "remove": true})))
            .await;
        app.daemon(turn_event("tool_completed", 1, json!({"call_id": "c3"})))
            .await;
        assert_eq!(drawn(&mut app), ["", "› merged, thanks", "", "   👍 "]);
        app.daemon(turn_event(
            "turn_finished",
            1,
            json!({"status": "completed"}),
        ))
        .await;
        assert_eq!(drawn(&mut app), ["", "› merged, thanks", "", "   👍 "]);
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
        assert_eq!(
            rows[4..9],
            ["", "", "› and the notes?", "", "   🎉 "],
            "{rows:?}"
        );
        assert!(rows[10].starts_with("─ Worked"), "{rows:?}");
        assert_eq!(rows[11..], ["", "• Updated."]);
        // Once that message is no longer in the conversation, a reaction
        // to it draws no row of its own.
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
        let rows = drawn(&mut app);
        assert!(!rows.iter().any(|row| row.contains("👀")), "{rows:?}");
    }

    #[tokio::test]
    async fn a_running_turn_the_history_drew_reacts_under_its_message() {
        let client = daemon_answering(|_| json!({})).await;
        let mut app = app(client);
        let react = |id: &str, emoji: &str| {
            turn_event(
                "tool_started",
                4,
                json!({"call_id": id, "name": "react",
                    "arguments": json!({"emoji": emoji}).to_string()}),
            )
        };
        // Attached mid-turn: the history drew the message and what the bot
        // reacted with so far, and the turn's start comes again after it.
        app.land(5, ("ship it", ""), None, false, false);
        app.daemon(react("c1", "👀")).await;
        app.daemon(turn_event("tool_completed", 4, json!({"call_id": "c1"})))
            .await;
        app.seen_node = 5;
        app.daemon(turn_event("accepted", 4, json!({"node": 5})))
            .await;
        app.commit(vec![Line::from("• Ran the tests")]);
        app.daemon(react("c2", "👍")).await;
        app.daemon(turn_event("tool_completed", 4, json!({"call_id": "c2"})))
            .await;
        app.daemon(turn_event(
            "turn_finished",
            4,
            json!({"status": "completed"}),
        ))
        .await;
        let rows = drawn(&mut app);
        assert_eq!(rows[..4], ["", "› ship it", "", "   👀 👍 "], "{rows:?}");
        assert!(!rows.iter().any(|row| row.contains("reacted")), "{rows:?}");
    }

    #[tokio::test]
    async fn the_leads_reaction_goes_under_the_persons_message_wherever_it_is() {
        let client = daemon_answering(|_| json!({})).await;
        let mut app = app(client);
        app.land(5, ("ship it", ""), None, false, false);
        app.commit(vec![Line::from("• Shipped.")]);
        app.picked.set(5, "calc.lead", "👀", false);
        app.place_picked(5);
        assert_eq!(
            drawn(&mut app)[..4],
            ["", "› ship it", "", "   👀 calc.lead "]
        );
        // The bot's own go first, the lead's under them.
        app.bot_reactions = vec!["👍".into()];
        app.draw_bot_reactions();
        assert_eq!(
            drawn(&mut app)[..5],
            ["", "› ship it", "", "   👍 ", "   👀 calc.lead "]
        );
        // Taken off, its row goes; no row ever says so.
        app.picked.set(5, "calc.lead", "👀", true);
        app.place_picked(5);
        let rows = drawn(&mut app);
        assert_eq!(rows[..4], ["", "› ship it", "", "   👍 "], "{rows:?}");
        assert!(
            !rows.iter().any(|row| row.contains("calc.lead")),
            "{rows:?}"
        );
        // Reacted to before it landed: under it once it does.
        app.picked.set(7, "calc.lead", "👀", false);
        app.land(7, ("and the notes", ""), None, false, false);
        assert_eq!(drawn(&mut app).last().unwrap(), "   👀 calc.lead ");
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
                "",
                "› merged, thanks",
                "",
                "   👍 ",
                "",
                "",
                "› and the notes?",
                "",
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
        app.declare = declared::declare(&json!({}), served::Role::at(2, 2));
        app.running = Some(Running {
            turn: 1,
            started: Instant::now(),
            phase: "Working".into(),
            note: false,
            effort: None,
        });
        assert!(!app.configure().await);
        assert!(asked.lock().unwrap().is_empty());
        app.running = None;
        assert!(app.configure().await);
        let sent = asked.lock().unwrap()[0].clone();
        assert_eq!(sent["op"], "configure");
        assert_eq!(
            sent["declare"],
            json!(declared::tools(served::Role::at(2, 2)))
        );
        assert!(app.declare.is_none());
    }

    #[tokio::test]
    async fn esc_sets_an_open_question_aside_before_it_stops_the_turn() {
        let asked = Arc::new(std::sync::Mutex::new(Vec::new()));
        let seen = asked.clone();
        let client = daemon_answering(move |request| {
            seen.lock().unwrap().push(request["op"].clone());
            json!({})
        })
        .await;
        let mut app = app(client);
        app.asks = vec![
            asks::call(
                "c1",
                &json!({"question": "Keep the old API?", "options": [
                    {"label": "Keep it", "explanation": "e"},
                    {"label": "Drop it", "explanation": "e"}]}),
            )
            .unwrap(),
        ];
        app.running = Some(Running {
            turn: 1,
            started: Instant::now(),
            phase: "Working".into(),
            note: false,
            effort: None,
        });
        let esc = || Event::Key(event::KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE));
        // The question asks first: Esc sets it aside, the turn runs on.
        app.key(esc()).await;
        assert!(app.chatting);
        assert!(asked.lock().unwrap().is_empty());
        // Nothing is open over the composer now: Esc stops the turn.
        app.key(esc()).await;
        assert_eq!(*asked.lock().unwrap(), [json!("interrupt")]);
    }

    #[tokio::test]
    async fn tab_queues_a_message_for_after_the_running_turn() {
        let sent = Arc::new(std::sync::Mutex::new(Vec::new()));
        let seen = sent.clone();
        let client = daemon_answering(move |asked| {
            let mut sent = seen.lock().unwrap();
            sent.push(asked.clone());
            json!({"turn": 1 + sent.len()})
        })
        .await;
        let mut app = app(client);
        app.move_to = Some("/moved".into());
        let tab = || Event::Key(event::KeyEvent::new(KeyCode::Tab, KeyModifiers::NONE));
        let enter = || Event::Key(event::KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
        // Idle, Tab sends nothing: Enter starts a turn.
        app.input.set("first");
        app.key(tab()).await;
        assert!(sent.lock().unwrap().is_empty());
        assert_eq!(app.input.trim(), "first");
        app.input.clear();
        app.running = Some(Running {
            turn: 1,
            started: Instant::now(),
            phase: "Working".into(),
            note: false,
            effort: None,
        });
        app.yours = Some(1);
        assert!(!app.status_line().to_string().contains("tab to queue"));
        app.input.set("after this");
        assert!(app.status_line().to_string().contains("tab to queue"));
        app.key(tab()).await;
        assert!(app.input.is_empty());
        // A turn of its own, once the running one ends: it may move the
        // lead, and it is the person's.
        let queued = sent.lock().unwrap()[0].clone();
        assert_eq!(queued["delivery"], "queue");
        assert_eq!(queued["prompt"], "after this");
        assert_eq!(queued["workspace"], "/moved");
        assert!(app.move_to.is_none());
        assert_eq!(app.yours, Some(2));
        let staged: Vec<String> = app.staged_rows(80).iter().map(|l| l.to_string()).collect();
        assert_eq!(
            staged,
            ["", "› after this", "  queued for after this turn", ""]
        );
        // Enter still steers.
        app.input.set("now");
        app.key(enter()).await;
        let steer = sent.lock().unwrap()[1].clone();
        assert_eq!(steer["delivery"], "steer");
        let staged: Vec<String> = app.staged_rows(80).iter().map(|l| l.to_string()).collect();
        assert_eq!(staged[3..], ["", "› now", ""]);
        // Once its turn starts it is no longer waiting.
        app.daemon(turn_event("turn_finished", 3, json!({}))).await;
        app.daemon(turn_event("accepted", 2, json!({}))).await;
        assert!(app.staged.is_empty());
    }

    #[tokio::test]
    async fn a_claude_bot_made_with_no_cache_lifetime_takes_the_hour() {
        let bot = |provider: &str, ttl: Value| json!({"provider": provider, "model": "m", "settings": {"cache_ttl": ttl}});
        assert_eq!(cache_to_take(&bot("anthropic", Value::Null)), Some("1h"));
        assert_eq!(cache_to_take(&bot("bedrock", Value::Null)), Some("1h"));
        assert_eq!(cache_to_take(&bot("anthropic", json!("1h"))), None);
        // Five minutes someone chose stays five minutes.
        assert_eq!(cache_to_take(&bot("anthropic", json!("5m"))), None);
        assert_eq!(cache_to_take(&bot("openai", Value::Null)), None);
        let asked = Arc::new(std::sync::Mutex::new(Vec::new()));
        let seen = asked.clone();
        let client = daemon_answering(move |request| {
            seen.lock().unwrap().push(request.clone());
            json!({"bot": "b", "changed": ["cache_ttl"]})
        })
        .await;
        let mut app = app(client);
        app.cache = Some("1h");
        assert!(app.configure().await);
        let sent = asked.lock().unwrap()[0].clone();
        assert_eq!(
            (&sent["op"], &sent["cache_ttl"]),
            (&json!("configure"), &json!("1h"))
        );
        assert!(app.cache.is_none());
    }

    #[tokio::test]
    async fn fast_lists_the_speeds_its_provider_serves_and_picks_one() {
        let asked = Arc::new(std::sync::Mutex::new(Vec::new()));
        let seen = asked.clone();
        let providers = json!({"anthropic": {"speeds": ["fast"]}, "bedrock": {"speeds": []},
            "openai": {"speeds": ["fast", "ultrafast"]}});
        let client = daemon_serving(providers, move |request| {
            seen.lock().unwrap().push(request.clone());
            json!({"bot": "b", "changed": ["speed"]})
        })
        .await;
        let mut app = app(client);
        let key = |code: KeyCode| Event::Key(event::KeyEvent::new(code, KeyModifiers::NONE));
        app.model = "anthropic/claude-opus-5-5".into();
        app.input.set("/fast");
        app.submit().await;
        // Standard and the one tier Anthropic serves, standard in effect.
        assert!(app.choosing == Some((Choice::Speed, 0)));
        assert_eq!(
            app.speed_rows(),
            [
                ("standard", "the model's own speed and price"),
                ("fast", "faster, at a higher price per token")
            ]
        );
        app.key(key(KeyCode::Down)).await;
        app.key(key(KeyCode::Enter)).await;
        assert!(app.choosing.is_none());
        assert_eq!(
            app.notice.as_deref(),
            Some("fast from the next call, at a higher price per token")
        );
        assert_eq!(app.speed.as_deref(), Some("fast"));
        let footer = app.footer_line(200).to_string();
        assert!(footer.contains("Opus 5.5 fast"), "{footer}");
        // It opens again on what is in effect.
        app.input.set("/fast");
        app.submit().await;
        assert!(app.choosing == Some((Choice::Speed, 1)));
        // Enter on it changes nothing; Esc closes the list.
        app.key(key(KeyCode::Enter)).await;
        assert_eq!(asked.lock().unwrap().len(), 1);
        app.input.set("/fast");
        app.submit().await;
        app.key(key(KeyCode::Up)).await;
        app.key(key(KeyCode::Enter)).await;
        assert_eq!(
            app.notice.as_deref(),
            Some("standard speed from the next call")
        );
        assert_eq!(app.speed, None);
        let speeds: Vec<_> = (asked.lock().unwrap().iter())
            .map(|sent| sent["speed"].clone())
            .collect();
        assert_eq!(speeds, [json!("fast"), json!("standard")]);
        // Nothing to pick where the provider serves standard alone.
        app.model = "bedrock/anthropic.claude-opus-5-5".into();
        app.input.set("/fast");
        app.submit().await;
        assert!(app.choosing.is_none());
        assert_eq!(
            app.notice.as_deref(),
            Some("bedrock serves its models at standard speed only")
        );
        app.model = "openai/gpt-6.1-sol".into();
        app.input.set("/fast");
        app.submit().await;
        assert_eq!(app.speed_rows().len(), 3);
        app.key(key(KeyCode::Up)).await;
        app.key(key(KeyCode::Enter)).await;
        assert_eq!(app.speed.as_deref(), Some("ultrafast"));
        // Moved meanwhile to a provider serving fewer tiers, the list
        // takes its last row rather than one that is gone.
        app.input.set("/fast");
        app.submit().await;
        assert!(app.choosing == Some((Choice::Speed, 2)));
        app.model = "anthropic/claude-opus-5-5".into();
        app.key(key(KeyCode::Enter)).await;
        assert!(app.choosing.is_none());
        assert_eq!(app.speed.as_deref(), Some("fast"));
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
        app.declared = declared::declare(&custom, served::Role::at(0, 2)).unwrap();
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
        assert_eq!(rows, ["• Merged it.", "   🎉 ", "", "", "› what next?", ""]);
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
        assert_eq!(rows[..4], ["• I'll look.", "", "• Started b.fix ▸", ""]);
        assert!(rows.iter().all(|row| !row.contains("Ran")), "{rows:?}");
        assert_eq!(rows.last().map(String::as_str), Some("• Done."));
    }

    #[tokio::test]
    async fn words_between_agents_are_one_quiet_row_a_click_opens() {
        // Opened, the message is read again from the history.
        let client = daemon_answering(|asked| {
            let node = asked["nodes"][0].as_i64().unwrap_or(0);
            let content = match node {
                4 => "The build is green.\nI pushed the fix to calc.add.",
                _ => {
                    "Task updates: what happened in your tasks since you last heard.\n\
                      - calc.neg: turn:calc.neg/3 completed"
                }
            };
            match asked["op"].as_str() {
                Some("history_items") => json!({"items": [{"node": node, "item":
                    {"type": "message", "role": "user", "content": content}}]}),
                _ => json!({}),
            }
        })
        .await;
        let mut app = app(client);
        app.width = 80;
        app.land(
            4,
            ("The build is green.\nI pushed the fix to calc.add.", ""),
            Some("calc.add"),
            false,
            false,
        );
        assert_eq!(drawn(&mut app), ["↳ From calc.add ▸"]);
        // An agent's name takes the agents' colour; a person's never does.
        let blue = |app: &App, node| {
            (app.relayed_lines(node, &[], 80).iter())
                .flat_map(|line| &line.spans)
                .any(|span| span.style.fg == Some(Color::Blue))
        };
        assert!(blue(&app, 4));
        app.relayed.insert(
            40,
            Relayed {
                head: "From ana".into(),
                from: Some("ana".into()),
                person: true,
                text: None,
            },
        );
        assert!(!blue(&app, 40));
        app.relayed.remove(&40);
        // A click on one opens it whole, and again shuts it.
        app.follow("sonny:said/4");
        let button = app.pressed.pop().unwrap();
        app.press(&button).await;
        assert_eq!(
            drawn(&mut app),
            [
                "↳ From calc.add ▾",
                "  The build is green.",
                "  I pushed the fix to calc.add.",
            ]
        );
        app.press("said/4").await;
        assert_eq!(drawn(&mut app), ["↳ From calc.add ▸"]);
        // Ctrl+O opens it too, reading it again, and folds it again.
        let ctrl_o = event::KeyEvent::new(KeyCode::Char('o'), KeyModifiers::CONTROL);
        app.key(Event::Key(ctrl_o)).await;
        assert_eq!(drawn(&mut app)[1], "  The build is green.");
        app.key(Event::Key(ctrl_o)).await;
        assert_eq!(drawn(&mut app), ["↳ From calc.add ▸"]);
        // Those that come one after another are one row that counts them,
        // with what the agent did about them while it said nothing.
        let updates = "Task updates: what happened in your tasks since you last heard.\n\
            - calc.neg: turn:calc.neg/3 completed";
        app.land(5, (updates, ""), None, false, false);
        let steer =
            r#""$AGENT_BIN" run --detach --delivery steer --bot calc.neg -- 'Check div too.'"#;
        app.commit_call(
            "c1",
            "shell",
            &json!({"command": steer}),
            "turn:calc.neg/4",
            false,
        );
        assert_eq!(
            drawn(&mut app),
            ["↳ 1 message, 1 task update · calc.add, calc.neg ▸"]
        );
        // A click lists them; a click on one opens it, the calls under it.
        app.press("fold/0").await;
        assert_eq!(
            drawn(&mut app),
            [
                "↳ 1 message, 1 task update · calc.add, calc.neg ▾",
                "  ↳ From calc.add ▸",
                "  ↳ Task updates · calc.neg completed ▸",
            ]
        );
        app.press("said/5").await;
        let open = drawn(&mut app);
        assert_eq!(open[2], "  ↳ Task updates ▾");
        assert_eq!(open[4], "    - calc.neg: turn:calc.neg/3 completed");
        assert!(open[5].starts_with("    • Steered calc.neg"), "{open:?}");
        app.press("fold/0").await;
        assert_eq!(
            drawn(&mut app),
            ["↳ 1 message, 1 task update · calc.add, calc.neg ▸"]
        );
        // The agent's words end it: what comes after starts anew.
        app.commit(vec![Line::from("• calc.neg is done.")]);
        app.land(6, (updates, ""), None, false, false);
        assert_eq!(
            drawn(&mut app)[2..],
            [
                "• calc.neg is done.",
                "",
                "↳ Task updates · calc.neg completed ▸"
            ]
        );
        // The person's own words are drawn as ever.
        app.land(7, ("thanks", ""), None, false, false);
        assert_eq!(drawn(&mut app).iter().rev().nth(1).unwrap(), "› thanks");
        // A message a page drawn again put in a newer cluster opens there,
        // not in the cluster its rows were let go from.
        app.next_group = 9;
        app.land(8, (updates, ""), None, false, false);
        let stale = Cluster {
            messages: vec![Clustered {
                node: 8,
                kind: "task update",
                names: Vec::new(),
                calls: Vec::new(),
            }],
            open: false,
        };
        app.clusters.insert(1, stale);
        app.press("said/8").await;
        assert_eq!(
            drawn(&mut app).last().unwrap(),
            "  - calc.neg: turn:calc.neg/3 completed"
        );
        // A cluster let go takes its messages with it, so Ctrl+O reads
        // none of them; one a newer cluster holds stays.
        let stale = app.clusters.remove(&1).unwrap();
        app.forget(stale, 0);
        assert!(app.relayed.contains_key(&8));
        let first = app.clusters.remove(&0).unwrap();
        // The message landing, which a new cluster is about to hold, stays.
        app.forget(first, 4);
        assert!(app.relayed.contains_key(&4) && !app.relayed.contains_key(&5));
    }

    #[tokio::test]
    async fn an_older_page_s_folds_leave_the_newer_ones_and_their_numbers() {
        let client = daemon_answering(|_| json!({})).await;
        let mut app = app(client);
        app.width = 80;
        let start = |app: &mut App, name: &str| {
            let command = format!("\"$AGENT_BIN\" run --bot {name} \"Fix {name}\"");
            app.commit_call("c", "shell", &json!({"command": command}), "", false);
        };
        start(&mut app, "calc.add");
        start(&mut app, "calc.neg");
        app.flush();
        // An older page written on a screen of its own, as older() does.
        app.newer = Some(std::mem::take(&mut app.screen));
        app.any_history = false;
        let folding = app.folding.take();
        start(&mut app, "calc.old");
        start(&mut app, "calc.older");
        app.flush();
        app.folding = folding;
        let older = std::mem::replace(&mut app.screen, app.newer.take().unwrap());
        app.screen.prepend(0, older);
        assert_eq!(app.folds.len(), 2, "the newer fold is kept");
        app.follow("sonny:fold/0");
        let button = app.pressed.pop().unwrap();
        app.press(&button).await;
        let rows = drawn(&mut app);
        assert_eq!(rows[0], "• Messaged calc.old and calc.older ▸", "{rows:?}");
        assert!(
            rows.contains(&"• Messaged calc.add and calc.neg ▾".to_string()),
            "{rows:?}"
        );
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
            ["› fix the three bugs", "", "• Messaged calc.add ▸"]
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
        // Quiet rows one after another sit together.
        assert_eq!(rows[3], "• Removed calc.add");
        assert_eq!(rows[7], "• Messaged calc.sub ▸");
    }

    #[tokio::test]
    async fn tasks_started_in_a_row_are_one_roster_a_click_opens_one_from() {
        let reads = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let read = reads.clone();
        let client = daemon_answering(move |asked| {
            let n = read.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            match asked["bot"].as_str() {
                // A reaction attached to the next call: the task is still done.
                Some("calc.add") => json!({"turns": [
                    {"turn": 2, "status": "attached"}, {"turn": 1, "status": "completed"}]}),
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
        assert_eq!(rows[0], "• Started 2 tasks · click one to open it");
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
    async fn a_task_s_note_the_margin_shows_is_read_as_it_changes_and_kept() {
        let folder = std::env::temp_dir().join(unique("task-folder"));
        std::fs::create_dir_all(&folder).unwrap();
        std::fs::write(folder.join("flow.md"), "# Flow").unwrap();
        let workspace = folder.display().to_string();
        let folder_name = workspace.clone();
        let call = |arguments: Value| {
            json!({"type": "function_call", "call_id": "t", "name": margin::TOOL,
                "arguments": arguments.to_string()})
        };
        let gone = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let ended = gone.clone();
        let client = daemon_answering(move |asked| {
            if ended.load(std::sync::atomic::Ordering::SeqCst) {
                return json!({"error": "bot_not_found"});
            }
            match asked["op"].as_str() {
                Some("inspect") => json!({"name": "calc.add", "workspace": workspace}),
                Some("history_nodes") if asked["min_node"].is_null() => {
                    json!({"nodes": [{"node": 2}, {"node": 1}]})
                }
                Some("history_nodes") => json!({"nodes": [{"node": 3}]}),
                Some("history_items") => {
                    json!({"items": (asked["nodes"].as_array().unwrap().iter())
                    .map(|node| json!({"node": node, "item": match node.as_i64() {
                        Some(1) => call(json!({"id": "flow", "title": "Flow", "markdown": "one"})),
                        Some(2) => json!({"type": "message", "role": "assistant", "content": []}),
                        _ => call(json!({"id": "flow", "file": "flow.md"})),
                    }}))
                    .collect::<Vec<_>>()})
                }
                _ => json!({}),
            }
        })
        .await;
        let mut app = app(client);
        app.note_call("c1", &json!({"id": "flow", "from": "calc.add/flow"}), None);
        let shown = |app: &App| match &app.margin.note("flow").unwrap().body {
            margin::Body::From { shown, .. } => shown.as_deref().cloned(),
            _ => None,
        };
        app.read_references().await;
        assert_eq!(shown(&app), Some(margin::Body::Markdown("one".into())));
        assert_eq!(app.margin.note("flow").unwrap().title, "Flow");
        // Its pictures are read in the task's folder.
        let margin::Body::From {
            folder: read_in, ..
        } = &app.margin.note("flow").unwrap().body
        else {
            panic!()
        };
        assert_eq!(read_in.as_deref(), Some(folder_name.as_str()));
        // What it did since, a few seconds on.
        let read_flow = margin::Body::Read {
            path: "flow.md".into(),
            text: "# Flow".into(),
        };
        app.read_references().await;
        assert_eq!(shown(&app), Some(margin::Body::Markdown("one".into())));
        app.references_read = None;
        app.read_references().await;
        assert_eq!(shown(&app), Some(read_flow.clone()));
        // The task gone, with its folder, what it showed stays.
        gone.store(true, std::sync::atomic::Ordering::SeqCst);
        std::fs::remove_dir_all(&folder).unwrap();
        app.references_read = None;
        app.read_references().await;
        assert!(app.references.values().all(|reference| reference.gone));
        assert_eq!(shown(&app), Some(read_flow.clone()));
    }

    #[tokio::test]
    async fn a_task_s_note_that_shows_its_own_task_s_is_read_through() {
        use std::sync::atomic::{AtomicU8, Ordering::SeqCst};
        // Later the task takes its note away (1), or is gone (2), and its
        // own task changes.
        let phase = Arc::new(AtomicU8::new(0));
        let on = phase.clone();
        let client = daemon_answering(move |asked| {
            let later = on.load(SeqCst);
            let arguments = match (asked["bot"].as_str(), later) {
                (Some("calc.add"), 1) => json!({"id": "flow", "remove": true}),
                (Some("calc.add"), _) => {
                    json!({"id": "flow", "title": "Flow", "from": "calc.sub/flow"})
                }
                (_, 0) => json!({"id": "flow", "title": "Sub", "markdown": "deep"}),
                _ => json!({"id": "flow", "markdown": "deeper"}),
            };
            let node = if later > 0 { 2 } else { 1 };
            match asked["op"].as_str() {
                Some("inspect") if later == 2 && asked["bot"] == "calc.add" => {
                    json!({"error": "bot_not_found"})
                }
                Some("inspect") => json!({"bot_id": 1, "workspace": "/nowhere"}),
                Some("history_nodes") => json!({"nodes": [{"node": node}]}),
                _ => json!({"items": [{"node": node, "item": {"type": "function_call",
                    "call_id": "t", "name": margin::TOOL, "arguments": arguments.to_string()}}]}),
            }
        })
        .await;
        let shown = |app: &App| match &app.margin.note("flow").unwrap().body {
            margin::Body::From { shown, .. } => shown.as_deref().cloned(),
            _ => None,
        };
        for then in [1, 2] {
            phase.store(0, SeqCst);
            let mut app = app(client.clone());
            app.note_call("c1", &json!({"id": "flow", "from": "calc.add/flow"}), None);
            app.read_references().await;
            assert_eq!(shown(&app), Some(margin::Body::Markdown("deep".into())));
            assert_eq!(app.margin.note("flow").unwrap().title, "Flow");
            // Taken away by the task, or the task gone, what it showed
            // stays as last read.
            phase.store(then, SeqCst);
            app.references_read = None;
            app.read_references().await;
            assert_eq!(shown(&app), Some(margin::Body::Markdown("deep".into())));
        }
    }

    #[tokio::test]
    async fn a_task_s_note_is_not_read_from_another_bot_given_its_name() {
        let reused = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let again = reused.clone();
        let client = daemon_answering(move |asked| {
            let reused = again.load(std::sync::atomic::Ordering::SeqCst);
            let markdown = if reused { "theirs" } else { "ours" };
            match asked["op"].as_str() {
                Some("inspect") if reused && asked["bot_id"] == 7 => {
                    json!({"error": "bot_not_found"})
                }
                Some("inspect") => {
                    json!({"name": "calc.add", "bot_id": if reused { 8 } else { 7 },
                        "workspace": "/nowhere"})
                }
                Some("history_nodes") => json!({"nodes": [{"node": if reused { 9 } else { 1 }}]}),
                _ => json!({"items": (asked["nodes"].as_array().unwrap().iter())
                    .map(|node| json!({"node": node, "item": {"type": "function_call",
                        "call_id": "t", "name": margin::TOOL, "arguments": json!({"id": "flow",
                        "title": "Flow", "markdown": markdown}).to_string()}}))
                    .collect::<Vec<_>>()}),
            }
        })
        .await;
        let mut app = app(client);
        app.note_call("c1", &json!({"id": "flow", "from": "calc.add/flow"}), None);
        let shown = |app: &App| match &app.margin.note("flow").unwrap().body {
            margin::Body::From { shown, .. } => shown.as_deref().cloned(),
            _ => None,
        };
        app.read_references().await;
        assert_eq!(shown(&app), Some(margin::Body::Markdown("ours".into())));
        // Removed, and another task given its name: what was read stays.
        reused.store(true, std::sync::atomic::Ordering::SeqCst);
        app.references_read = None;
        app.read_references().await;
        assert!(app.references.values().all(|reference| reference.gone));
        assert_eq!(shown(&app), Some(margin::Body::Markdown("ours".into())));
    }

    #[tokio::test]
    async fn a_task_s_note_is_found_far_back_in_order_and_follows_its_title() {
        let call = |arguments: Value| {
            json!({"type": "function_call", "call_id": "t", "name": margin::TOOL,
                "arguments": arguments.to_string()})
        };
        let fail = Arc::new(std::sync::atomic::AtomicBool::new(true));
        let failing = fail.clone();
        let client = daemon_answering(move |asked| match asked["op"].as_str() {
            // A read that fails once, as a dropped connection would.
            Some("history_items") if failing.swap(false, std::sync::atomic::Ordering::SeqCst) => {
                json!({"error": "unavailable"})
            }
            Some("inspect") => json!({"name": "calc.add", "workspace": "/nowhere"}),
            Some("history_nodes") if asked["min_node"] == 6 => json!({"nodes": [{"node": 6}]}),
            Some("history_nodes") if asked["min_node"] == 7 => json!({"nodes": [{"node": 7}]}),
            Some("history_nodes") if asked["from"] == 3 => json!({"nodes": [{"node": 3}]}),
            Some("history_nodes") if asked["min_node"].is_null() => {
                json!({"nodes": [{"node": 5}, {"node": 4}], "next_from": 3})
            }
            Some("history_items") => {
                json!({"items": (asked["nodes"].as_array().unwrap().iter())
                .map(|node| json!({"node": node, "item": match node.as_i64() {
                    Some(3) => call(json!({"id": "flow", "title": "Flow", "markdown": "start"})),
                    // A call its own margin turns down.
                    Some(5) => call(json!({"id": "flow", "title": 1, "markdown": "wrong"})),
                    // Two calls in one response, as some providers keep them.
                    Some(4) => json!({"role": "assistant", "content": [
                        {"type": "tool_use", "id": "u1", "name": margin::TOOL,
                            "input": {"id": "flow", "markdown": "a"}},
                        {"type": "tool_use", "id": "u2", "name": margin::TOOL,
                            "input": {"id": "flow", "markdown": "b"}}]}),
                    Some(6) => call(json!({"id": "flow", "title": "Flow two", "markdown": "c"})),
                    _ => call(json!({"id": "flow", "title": "Flow three", "markdown": "d"})),
                }}))
                .collect::<Vec<_>>()})
            }
            _ => json!({}),
        })
        .await;
        let mut app = app(client);
        app.note_call("c1", &json!({"id": "flow", "from": "calc.add/flow"}), None);
        let shown = |app: &App| match &app.margin.note("flow").unwrap().body {
            margin::Body::From { shown, .. } => shown.as_deref().cloned(),
            _ => None,
        };
        // A read cut short leaves where it was read from.
        app.read_references().await;
        assert_eq!(shown(&app), None);
        assert!(
            app.references
                .values()
                .all(|reference| reference.node.is_none())
        );
        app.references_read = None;
        app.read_references().await;
        assert_eq!(shown(&app), Some(margin::Body::Markdown("b".into())));
        assert_eq!(app.margin.note("flow").unwrap().title, "Flow");
        // Retitled there, it is here too.
        app.references_read = None;
        app.read_references().await;
        assert_eq!(shown(&app), Some(margin::Body::Markdown("c".into())));
        assert_eq!(app.margin.note("flow").unwrap().title, "Flow two");
        // Given its own title here, it keeps it.
        let titled = json!({"id": "flow", "title": "Mine", "from": "calc.add/flow"});
        app.note_call("c2", &titled, None);
        app.references_read = None;
        app.read_references().await;
        assert_eq!(shown(&app), Some(margin::Body::Markdown("d".into())));
        assert_eq!(app.margin.note("flow").unwrap().title, "Mine");
    }

    #[tokio::test]
    async fn a_task_that_ended_with_nothing_to_say_has_no_reply_and_did_not_fail() {
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
        assert_eq!(rows[1], "  ✓ quiet  no reply  Fix quiet");
        assert_eq!(rows[2], "  ✗ bad    failed    Fix bad");
    }

    #[tokio::test]
    async fn a_task_that_ended_on_a_question_needs_you() {
        let scans = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let scanned = scans.clone();
        let client = daemon_answering(move |asked| match asked["op"].as_str() {
            Some("history_nodes") => {
                scanned.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                json!({"nodes": [{"node": 3, "turn": 1}]})
            }
            Some("history_items") => {
                let arguments = json!({"question": "Merge?", "options": [
                    {"label": "Yes", "explanation": "Now."},
                    {"label": "No", "explanation": "Later."}]});
                json!({"items": [{"node": 3, "item": {"type": "function_call", "name": "ask",
                    "call_id": "a1", "arguments": arguments.to_string()}}]})
            }
            _ => json!({"turns": [{"turn": 1, "status": "completed"}]}),
        })
        .await;
        let mut app = app(client);
        app.width = 80;
        let arguments = json!({"name": "ask", "brief": "Fix ask"});
        app.commit_call("c", served::START, &arguments, "turn:calc.ask/1", false);
        app.read_tasks().await;
        assert_eq!(drawn(&mut app)[1], "  ? ask  needs you  Fix ask");
        // Its history is read once for the turn it ended.
        app.tasks_read = None;
        app.read_tasks().await;
        assert_eq!(scans.load(std::sync::atomic::Ordering::SeqCst), 1);
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
    async fn the_footer_counts_what_needs_the_person_and_a_click_opens_status() {
        use ratatui::crossterm::event::{MouseButton, MouseEvent, MouseEventKind};
        let mut app = app(daemon_answering(|_| json!({})).await);
        app.model = "opus".into();
        // Nothing counted while nothing runs or waits.
        let (line, opens) = app.footer(100);
        assert!(opens.is_empty() && !line.to_string().contains('●'));
        let ask = |id: &str| {
            asks::call(
                id,
                &json!({"question": "Merge?", "options": [
                {"label": "Yes", "explanation": "e"}, {"label": "No", "explanation": "e"}]}),
            )
            .unwrap()
        };
        app.asks = vec![ask("c1"), ask("c2")];
        // With no windows for them, the tasks started, as their roster reads them.
        let task = |name: &str, state: &str| started::Task {
            bot: format!("calc.{name}"),
            name: name.into(),
            brief: "b".into(),
            state: state.into(),
        };
        app.tasks
            .insert(0, vec![task("add", "working"), task("neg", "done")]);
        app.tasks
            .insert(1, vec![task("add", "working"), task("div", "working")]);
        let (line, opens) = app.footer(100);
        let text = line.to_string();
        let shown: Vec<(String, &str)> = (opens.iter())
            .map(|(at, link)| {
                let cells: String = (text.chars())
                    .skip(at.start as usize)
                    .take(at.len())
                    .collect();
                (cells, *link)
            })
            .collect();
        assert_eq!(
            shown,
            [
                ("? 2 needs you".to_owned(), "status"),
                ("● 2 working".to_owned(), "status")
            ]
        );
        // The agent's name is the composer's to say, not the footer's.
        assert!(text.contains("? 2 needs you  ● 2 working"), "{text}");
        assert!(!text.contains(&app.bot), "{text}");
        assert_eq!(text.width(), 100);
        let counter = opens[0].0.clone();
        // Its marks and colors as a roster's: yellow ? for what needs the
        // person, cyan ● at work.
        let marks: Vec<_> = (line.spans.iter())
            .filter(|span| span.content == "●" || span.content == "?")
            .map(|span| span.style.fg)
            .collect();
        assert_eq!(marks, [Some(Color::Yellow), Some(Color::Cyan)]);
        app.counter_at = vec![(20, 10 + counter.start..10 + counter.end, "status")];
        let click = |column| {
            Event::Mouse(MouseEvent {
                kind: MouseEventKind::Down(MouseButton::Left),
                column,
                row: 20,
                modifiers: KeyModifiers::NONE,
            })
        };
        app.key(click(9 + counter.start)).await;
        assert!(app.pane.is_none());
        app.key(click(10 + counter.start)).await;
        assert!(app.pane.as_ref().is_some_and(pane::Pane::is_card));
    }

    #[test]
    fn status_links_each_agent_but_this_one_to_open_it() {
        let row = |name: &str, bot: &str, group| status::Row {
            name: name.into(),
            bot: bot.into(),
            state: "working".into(),
            group,
            ..Default::default()
        };
        let watch = Watch {
            project: "calc".into(),
            rows: vec![
                row("lead", "calc.lead.review", status::Group::Working),
                row("first lead", "calc.lead", status::Group::Working),
                row("neg", "calc.neg", status::Group::Working),
            ],
            read: Instant::now(),
            lookout: "Lookout · on".into(),
            resolved: false,
            background: None,
            usage: None,
            commons: None,
            sky: None,
            picked: None,
            pick: None,
        };
        // A named lead's window: its own row opens nothing, and the
        // folder's other lead opens by its own name, not its label.
        let (lines, _, marks) = watch_card(&watch, 80, "calc.lead.review");
        let link = |at: usize| {
            (marks.iter())
                .find(|(on, _)| *on == at)
                .map(|(_, mark)| mark.clone())
        };
        let neg = (lines.iter())
            .position(|line| line.to_string().contains("neg working"))
            .unwrap();
        assert_eq!(
            link(neg),
            Some(screen::Mark::Link(format!(
                "{}task/calc.neg",
                screen::BUTTON
            )))
        );
        let first = (lines.iter())
            .position(|line| line.to_string().contains("first lead working"))
            .unwrap();
        assert_eq!(
            link(first),
            Some(screen::Mark::Link(format!(
                "{}task/calc.lead",
                screen::BUTTON
            )))
        );
        // This window's own agent opens nothing.
        assert_eq!(marks.len(), 2);
    }

    #[tokio::test]
    async fn the_footer_counts_tasks_at_work_whose_rosters_are_not_drawn() {
        let client = daemon_answering(|asked| {
            assert_eq!(asked["active"], true);
            // Two pages of them.
            match asked["after"].as_str() {
                None => json!({"next_after": "calc.ask", "bots": [
                    {"name": "calc.add", "created_by": "calc"},
                    {"name": "calc.ask", "created_by": "calc"}]}),
                // A side chat of its own is no task.
                Some(_) => json!({"next_after": null, "bots": [
                    {"name": "calc.mul", "created_by": "calc"},
                    {"name": "calc.side", "created_by": "calc", "expire_ms": 60000},
                    {"name": "other.fix", "created_by": "other"}]}),
            }
        })
        .await;
        let mut app = app(client);
        app.bot = "calc".into();
        // Only one roster still drawn: its task needs the person.
        app.tasks.insert(
            0,
            vec![started::Task {
                bot: "calc.ask".into(),
                name: "ask".into(),
                brief: "b".into(),
                state: "needs you".into(),
            }],
        );
        app.read_running_tasks().await;
        assert_eq!(app.agents(), (2, 1));
    }

    #[tokio::test]
    async fn the_footer_names_the_branch() {
        let mut app = app(daemon_answering(|_| json!({})).await);
        app.workspace = "/work/.agent/worktrees/fix-cart/packages/shop".into();
        app.model = "opus".into();
        app.place = place::Place {
            branch: Some("agent/fix-cart".into()),
        };
        // The row new leads start in goes unsaid; another leads the row,
        // full access's mark in the text's own weight and its word muted.
        app.mode = Some("full");
        let full = app.footer(100).0;
        assert!(
            full.to_string()
                .starts_with("  ⏵⏵ full access · Opus · /work"),
            "{full}"
        );
        let mark = full.spans.iter().find(|span| span.content == "⏵⏵").unwrap();
        assert_eq!(mark.style.fg, None);
        assert!(!mark.style.add_modifier.contains(Modifier::DIM));
        let word = full
            .spans
            .iter()
            .find(|span| span.content == " full access")
            .unwrap();
        assert!(word.style.add_modifier.contains(Modifier::DIM));
        app.mode = Some("auto");
        let wide = app.footer_line(100).to_string();
        assert!(
            wide.contains("Opus · /work/.agent/worktrees/fix-cart/packages/shop on agent/fix-cart"),
            "{wide}"
        );
        // A narrow footer shortens the folder by whole parts, as fish
        // does, never the branch.
        let narrow = app.footer_line(56).to_string();
        assert!(
            narrow.contains("Opus · /w/.a/w/f/p/shop on agent/fix-cart"),
            "{narrow}"
        );
        assert_eq!(narrow.width(), 56);
    }

    /// The footer of a busy lead at the widths the README shows, as the
    /// terminal would draw it, muted text in the gray Sonny measures for
    /// Flexoki Dark: `FOOTER_SHOTS=out.ans cargo test -- --ignored`.
    #[tokio::test]
    #[ignore]
    async fn footer_shots() {
        let Ok(out) = std::env::var("FOOTER_SHOTS") else {
            return;
        };
        let mut app = app(daemon_answering(|_| json!({})).await);
        app.model = "openai/gpt-6.1-sol".into();
        app.effort = Some("high".into());
        app.mode = Some("full");
        app.workspace = format!("{}/Developer/agent", std::env::var("HOME").unwrap());
        app.place = place::Place {
            branch: Some("main".into()),
        };
        let ask = asks::call(
            "c1",
            &json!({"question": "Merge?", "options": [
            {"label": "Yes", "explanation": "e"}, {"label": "No", "explanation": "e"}]}),
        );
        app.asks = vec![ask.unwrap()];
        let task = |name: &str| started::Task {
            bot: format!("agent.{name}"),
            name: name.into(),
            brief: "b".into(),
            state: "working".into(),
        };
        app.tasks.insert(0, vec![task("cart"), task("docs")]);
        app.failed = 1;
        app.behind = (3, Some(Instant::now()));
        for id in ["findings", "repro"] {
            let post = json!({"id": id, "title": id, "markdown": "x"});
            app.margin.apply(id, &post, None, None);
        }
        app.attached = vec![1, 2];
        app.context = Some(126_000);
        app.window = Some(200_000);
        let sgr = |span: &Span| {
            let style = span.style;
            let mut codes = Vec::new();
            if style.add_modifier.contains(Modifier::BOLD) {
                codes.push("1".to_owned());
            }
            match style.fg {
                Some(Color::Red) => codes.push("31".into()),
                Some(Color::Green) => codes.push("32".into()),
                Some(Color::Yellow) => codes.push("33".into()),
                Some(Color::Blue) => codes.push("34".into()),
                Some(Color::Magenta) => codes.push("35".into()),
                Some(Color::Cyan) => codes.push("36".into()),
                // Muted: the measured gray, as Look draws a dim cell.
                _ if style.add_modifier.contains(Modifier::DIM) => {
                    codes.push("38;2;130;129;123".into())
                }
                _ => {}
            }
            format!("\x1b[0;{}m{}\x1b[0m", codes.join(";"), span.content)
        };
        let mut ansi = String::new();
        for width in [100, 80, 60, 44] {
            let line = app.footer_line(width);
            ansi.push_str(&line.spans.iter().map(sgr).collect::<String>());
            ansi.push_str("\r\n\r\n");
        }
        std::fs::write(out, ansi).unwrap();
    }

    #[tokio::test]
    async fn shift_enter_breaks_the_line_and_a_selection_is_typed_over() {
        use ratatui::crossterm::event::{MouseButton, MouseEvent, MouseEventKind};
        let mut app = app(daemon_answering(|_| json!({})).await);
        app.input.set("one two");
        let key = |code, modifiers| Event::Key(event::KeyEvent::new(code, modifiers));
        app.key(key(KeyCode::Enter, KeyModifiers::SHIFT)).await;
        assert_eq!(&*app.input, "one two\n");
        // Drawn on row 10 from column 0, its first row shown there.
        app.input.set("one two three");
        app.composer_at = Some((Rect::new(0, 10, 40, 3), 0, 40));
        let mouse = |kind, column: usize, row| MouseEvent {
            kind,
            column: column as u16,
            row,
            modifiers: KeyModifiers::NONE,
        };
        let (down, drag) = (
            MouseEventKind::Down(MouseButton::Left),
            MouseEventKind::Drag(MouseButton::Left),
        );
        assert!(app.composer_mouse(&mouse(down, prompt::LEAD + 4, 10)));
        assert_eq!(
            (
                app.input.selection(),
                app.input.cursor(&app.input.layout(40)).1
            ),
            (None, prompt::LEAD + 4)
        );
        assert!(app.composer_mouse(&mouse(drag, prompt::LEAD + 7, 10)));
        assert_eq!(app.input.selection(), Some(4..7));
        app.key(key(KeyCode::Backspace, KeyModifiers::NONE)).await;
        assert_eq!(&*app.input, "one  three");
        // Two clicks select a word, which typing replaces.
        app.composing = None;
        app.composer_click = None;
        assert!(app.composer_mouse(&mouse(down, prompt::LEAD + 6, 10)));
        assert!(app.composer_mouse(&mouse(down, prompt::LEAD + 6, 10)));
        assert_eq!(app.input.selection(), Some(5..10));
        app.key(key(KeyCode::Char('x'), KeyModifiers::NONE)).await;
        assert_eq!(&*app.input, "one  x");
        // A press off the composer is not its own.
        assert!(!app.composer_mouse(&mouse(down, prompt::LEAD, 5)));
        // A click whose press scrolled the composer still selects nothing:
        // the release finds its spot where the press was drawn.
        let lines: Vec<String> = (0..12).map(|n| format!("line {n}")).collect();
        app.input.set(&lines.join("\n"));
        app.composing = None;
        app.composer_click = None;
        app.composer_at = Some((Rect::new(0, 10, 40, 8), 0, 40));
        assert!(app.composer_mouse(&mouse(down, prompt::LEAD + 1, 12)));
        app.composer_at = Some((Rect::new(0, 10, 40, 8), 2, 40));
        let up = MouseEventKind::Up(MouseButton::Left);
        assert!(app.composer_mouse(&mouse(up, prompt::LEAD + 1, 12)));
        assert_eq!(app.input.selection(), None);
        assert_eq!(
            app.input.cursor(&app.input.layout(40)),
            (2, prompt::LEAD + 1)
        );
        // While dragging, the rows stay where the press found them.
        app.composing = None;
        app.composer_click = None;
        app.input.home();
        for _ in 0..2 {
            app.input.up(40);
        }
        app.composer_at = Some((Rect::new(0, 10, 40, 8), 0, 40));
        assert!(app.composer_mouse(&mouse(down, prompt::LEAD, 12)));
        let (rows, _) = app.composer(40);
        assert!(rows[0].to_string().ends_with("line 0"), "{}", rows[0]);
        assert!(app.composer_mouse(&mouse(drag, prompt::LEAD + 6, 13)));
        let line = |n: usize| lines[..n].iter().map(|l| l.len() + 1).sum::<usize>();
        assert_eq!(app.input.selection(), Some(line(2)..line(3) + 6));
    }

    #[tokio::test]
    async fn ctrl_o_opens_every_fold_and_folds_them_again() {
        let mut app = app(daemon_answering(|_| json!({})).await);
        app.width = 80;
        let start = |app: &mut App, name: &str| {
            let command = format!("\"$AGENT_BIN\" run --bot {name} \"Fix {name}\"");
            app.commit_call("c", "shell", &json!({"command": command}), "", false);
        };
        start(&mut app, "calc.add");
        start(&mut app, "calc.neg");
        assert_eq!(drawn(&mut app), ["• Messaged calc.add and calc.neg ▸"]);
        let ctrl_o = event::KeyEvent::new(KeyCode::Char('o'), KeyModifiers::CONTROL);
        app.key(Event::Key(ctrl_o)).await;
        let open = drawn(&mut app);
        assert_eq!(open[0], "• Messaged calc.add and calc.neg ▾");
        assert!(open.len() > 1, "{open:?}");
        app.key(Event::Key(ctrl_o)).await;
        assert_eq!(drawn(&mut app), ["• Messaged calc.add and calc.neg ▸"]);
    }

    #[tokio::test]
    async fn status_picks_an_agent_from_the_keys() {
        let mut app = app(daemon_answering(|_| json!({})).await);
        let row = |name: &str| status::Row {
            name: name.into(),
            bot: format!("calc.{name}"),
            state: "working".into(),
            group: status::Group::Working,
            ..Default::default()
        };
        app.open_pane(pane::Pane::card("/status", PICK_KEYS));
        app.watch = Some(Watch {
            project: "calc".into(),
            rows: vec![row("add"), row("neg")],
            read: Instant::now(),
            lookout: String::new(),
            resolved: false,
            background: None,
            usage: None,
            commons: None,
            sky: None,
            picked: None,
            pick: None,
        });
        let key = |code| Event::Key(event::KeyEvent::new(code, KeyModifiers::NONE));
        let pick = |app: &App| app.watch.as_ref().unwrap().pick;
        app.key(key(KeyCode::Down)).await;
        assert_eq!(pick(&app), Some(0));
        app.key(key(KeyCode::Down)).await;
        app.key(key(KeyCode::Down)).await;
        assert_eq!(pick(&app), Some(1), "the last stays picked");
        app.key(key(KeyCode::Up)).await;
        assert_eq!(pick(&app), Some(0));
    }

    #[tokio::test]
    async fn a_question_mark_with_nothing_typed_shows_the_keys() {
        let mut app = app(daemon_answering(|_| json!({})).await);
        let key = |c| Event::Key(event::KeyEvent::new(KeyCode::Char(c), KeyModifiers::NONE));
        app.key(key('?')).await;
        assert!(app.input.is_empty());
        assert!(app.pane.as_ref().is_some_and(|pane| pane.is_card()));
        app.close_pane(false);
        app.key(key('w')).await;
        app.key(key('?')).await;
        assert_eq!(
            &*app.input, "w?",
            "typed after a word, it is a question mark"
        );
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
    async fn compact_asks_first_and_what_was_run_ranks_first() {
        let asked = Arc::new(std::sync::Mutex::new(Vec::<String>::new()));
        let seen = asked.clone();
        let mut app = app(daemon_answering(move |request| {
            let op = request["op"].as_str().unwrap_or_default();
            seen.lock().unwrap().push(op.to_owned());
            json!({})
        })
        .await);
        let enter = || Event::Key(event::KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
        let compacted = || asked.lock().unwrap().iter().any(|op| op == "compact");
        let first = |app: &App| {
            app.menu()
                .iter()
                .map(|(name, ..)| *name)
                .take(4)
                .collect::<Vec<_>>()
        };
        app.input.set("/co");
        assert_eq!(first(&app), ["/compact", "/commons", "/config", "/copy"]);
        let keys = |app: &App| menu_keys(&app.menu(), app.pick, &app.input);
        assert_eq!(
            keys(&app),
            Some("  ↑↓ pick · tab complete · enter run · esc close")
        );
        // Enter on part of its name picks /compact as any other command,
        // and it asks first; any key but Enter keeps the conversation.
        app.key(enter()).await;
        assert!(app.confirming && app.input.is_empty());
        app.key(Event::Key(event::KeyEvent::new(
            KeyCode::Char('x'),
            KeyModifiers::NONE,
        )))
        .await;
        assert!(!app.confirming && !compacted());
        assert_eq!(app.notice.as_deref(), Some("not compacted"));
        assert!(app.input.is_empty(), "the key that said no is not typed");
        // Typed whole, it asks too; Enter compacts.
        app.input.set("/compact");
        assert_eq!(keys(&app), Some("  ↑↓ pick · enter run · esc close"));
        app.key(enter()).await;
        assert!(app.confirming && !compacted());
        app.key(enter()).await;
        assert!(!app.confirming && compacted());
        // Any other command runs on part of its name, and what was run
        // more ranks first among the names that start the same.
        for _ in 0..3 {
            app.input.set("/cop");
            app.key(enter()).await;
            assert_eq!(app.notice.as_deref(), Some("no reply yet"));
        }
        app.input.set("/co");
        assert_eq!(first(&app)[0], "/copy");
        assert_eq!(
            keys(&app),
            Some("  ↑↓ pick · tab complete · enter run · esc close")
        );
        // Its letters in order find a command.
        app.input.set("/cms");
        assert_eq!(first(&app), ["/commons"]);
        let found = &app.menu()[0].2;
        assert_eq!(found, &[1, 3, 7], "where in /commons the letters are");
    }

    #[tokio::test]
    async fn a_resize_focus_or_ctrl_l_draws_the_whole_screen_again() {
        let mut app = app(daemon_answering(|_| json!({})).await);
        app.input.set("hello");
        // Sized down and back before a draw: still drawn whole.
        app.key(Event::Resize(80, 23)).await;
        app.key(Event::Resize(80, 24)).await;
        assert!(std::mem::take(&mut app.stale));
        assert_eq!(app.width, 80);
        app.key(Event::FocusGained).await;
        assert!(std::mem::take(&mut app.stale));
        let ctrl_l = event::KeyEvent::new(KeyCode::Char('l'), KeyModifiers::CONTROL);
        app.key(Event::Key(ctrl_l)).await;
        assert!(std::mem::take(&mut app.stale));
        assert_eq!(&*app.input, "hello", "nothing typed");
        app.key(Event::Key(KeyCode::Char('l').into())).await;
        assert!(!app.stale);
        assert_eq!(&*app.input, "hellol");
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
        // The window, when the daemon says it, is how full that is.
        let mut windowed = usage(380_000, 0, None);
        windowed["data"]["context_window"] = json!(1_000_000);
        assert_eq!(window_of(&windowed), Some(1_000_000));
        assert_eq!(window_of(&fell_back), None);
        let full = |size| transcript::fullness(size, Some(1_000_000));
        assert_eq!(
            full(380_000),
            ("62%".into(), " context left", Some(Color::Green))
        );
        assert_eq!(full(600_000).2, Some(Color::Yellow));
        assert_eq!(full(850_000).2, Some(Color::Red));
        assert_eq!(
            transcript::fullness(48_500, None),
            ("48.5k".into(), " context", None)
        );
    }

    #[test]
    fn a_message_read_in_full_shows_the_words_it_quotes_under_who_wrote_them() {
        let item = json!({"role": "user", "content": [
            {"type": "input_text", "text": "News from your tasks."},
            {"type": "input_text", "text": "[Sent by lookout.tasks.]"},
            {"type": "input_text", "text": "[Quoted from turn:calc.fix/5, written by a person:]"},
            {"type": "input_text", "text": "yes, ship it\nand tag it"},
            {"type": "input_text", "text": "[Quoted from the reply of turn:calc.fix/6, written by ana:]"},
            {"type": "input_text", "text": "tagged"}],
            "quotes": [{"bot": "calc.fix", "turn": 5, "at": 3},
                {"bot": "calc.fix", "turn": 6, "reply": true, "person": "ana", "at": 5}]});
        assert_eq!(
            quoting(&item),
            "News from your tasks.\n\nyou wrote:\n> yes, ship it\n> and tag it\n\n\
             ana replied:\n> tagged"
        );
        // Its own words are all a message without quotes shows.
        assert_eq!(text(&item), "News from your tasks.");
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
        // A terminal's result shows what it wrote.
        let typed = json!({"type": "function_call_output", "call_id": "t",
            "output": r#"{"output":">>> ","handle":"proc:1","running":true}"#});
        assert_eq!(output(&typed), (">>> ".into(), false));
        let ended = json!({"type": "function_call_output", "call_id": "t",
            "output": r#"{"output":"Traceback\n","exit_code":1}"#});
        assert_eq!(output(&ended), ("Traceback\n".into(), true));
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
    async fn a_note_opens_the_margin_as_set_and_it_never_closes_by_itself() {
        use agent_client::config::MarginOpens;
        let mut app = app(daemon_with(json!([])).await);
        let margin = |app: &App| app.pane.as_ref().is_some_and(pane::Pane::is_margin);
        app.margin_back();
        assert!(!margin(&app));
        let post = json!({"id": "repro", "title": "Repro", "markdown": "x"});
        app.margin.apply("m1", &post, None, None);
        app.margin_back();
        assert!(margin(&app));
        // Its last note taken away, it stays, as one opened empty does.
        app.margin
            .apply("m2", &json!({"id": "repro", "remove": true}), None, None);
        app.margin_back();
        assert!(margin(&app) && !app.margin.closed());
        // Closed by the person, a note opens it again by default.
        app.close_pane(true);
        app.margin_back();
        assert!(!margin(&app) && app.margin.closed());
        app.margin.apply("m3", &post, None, None);
        app.margin_back();
        assert!(margin(&app) && !app.margin.closed());
        // Under what is open, to go back to; emptied there, it is still
        // there to go back to.
        app.open_pane(pane::Pane::card("/status", STATUS_KEYS));
        app.close_pane(true);
        app.close_pane(true);
        app.open_pane(pane::Pane::card("/status", STATUS_KEYS));
        let change = json!({"id": "repro", "markdown": "y"});
        app.margin.apply("m4", &change, None, None);
        app.margin_back();
        assert!(app.under.is_some() && app.pane.as_ref().unwrap().over);
        app.margin
            .apply("m5", &json!({"id": "repro", "remove": true}), None, None);
        app.margin_back();
        app.close_pane(true);
        assert!(margin(&app));
        // Unless closed: a note leaves it closed until /margin.
        app.margin_opens = MarginOpens::UnlessClosed;
        app.close_pane(true);
        app.margin.apply("m6", &post, None, None);
        app.margin_back();
        assert!(!margin(&app) && app.margin.closed());
        app.toggle_margin();
        assert!(margin(&app) && app.pane_keys);
        // Never: only /margin opens it.
        app.margin_opens = MarginOpens::Never;
        app.pane = None;
        app.pane_keys = false;
        app.margin.apply("m7", &change, None, None);
        app.margin_back();
        assert!(!margin(&app) && !app.margin.closed());
        app.toggle_margin();
        assert!(margin(&app) && app.pane_keys);
        app.pane_keys = false;
        app.margin.apply("m8", &post, None, None);
        app.margin_back();
        assert!(margin(&app));
    }

    #[tokio::test]
    async fn what_opens_lays_over_the_margin_and_esc_steps_back_to_it() {
        let mut app = app(daemon_with(json!([])).await);
        let post = json!({"id": "repro", "title": "Repro", "markdown": "x"});
        app.margin.apply("m1", &post, None, None);
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
        let post = json!({"id": "repro", "title": "Repro", "markdown": "x"});
        app.margin.apply("m1", &post, None, None);
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
        let post = json!({"id": "repro", "title": "Repro", "markdown": "x"});
        app.margin.apply("m1", &post, None, None);
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
        // Retitled, as a task's note read for the first time is, its
        // marker says so.
        let retitle = json!({"id": "routing", "title": "Routing now", "markdown": "a to c"});
        app.daemon(turn_event("tool_started", 3, call("m3", retitle)))
            .await;
        app.fold_notes(true);
        assert_eq!(texts(&app)[..], ["✎ Routing now · in the margin", "after"]);
        // Closed, out again; taken away, by a × or the agent, gone from
        // the conversation too, the margin shown or not.
        app.fold_notes(false);
        assert_eq!(texts(&app).len(), 3);
        let link = Some("sonny:note/routing".to_owned());
        assert!(app.screen.links().contains(&link));
        app.margin.remove("routing", None);
        app.fold_notes(false);
        assert_eq!(texts(&app)[..], ["after"]);
        app.fold_notes(true);
        assert_eq!(texts(&app)[..], ["after"]);
        assert!(!app.screen.links().contains(&link));
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
        let post = json!({"id": "repro", "title": "Repro", "markdown": "x"});
        app.margin.apply("m1", &post, None, None);
        app.margin_back();
        let (chat, margin) = app.split(Rect::new(0, 0, 170, 40));
        // The conversation's 100, a gutter each side of the line, the rest.
        assert_eq!((chat.width, margin.map(|m| m.width)), (100, Some(66)));
        let (chat, margin) = app.split(Rect::new(0, 0, 130, 40));
        assert_eq!((chat.width, margin), (130, None));
        // Asked for with Tab, it takes its half as anything opened does.
        app.pane_keys = true;
        let (_, margin) = app.split(Rect::new(0, 0, 130, 40));
        assert_eq!(margin.map(|m| m.width), Some(63));
        // A note's marker clicked shows it there too.
        app.pane_keys = false;
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
    async fn the_margin_catches_up_on_what_changed_past_the_page_drawn() {
        let call = |id: &str, arguments: Value| {
            json!({"type": "function_call", "call_id": id, "name": "margin",
                   "arguments": arguments.to_string()})
        };
        let told = margin::closed_message("gone");
        let items = json!({
            "3": call("c5", json!({"id": "kept", "markdown": "new"})),
            "4": {"type": "function_call_output", "call_id": "c5", "output": "In the margin."},
            "5": {"type": "message", "role": "user", "content": [{"type": "input_text", "text": told}]},
            "9": {"type": "message", "role": "assistant", "content": [{"type": "output_text", "text": "Done."}]},
            "10": {"type": "message", "role": "assistant", "content": [{"type": "output_text", "text": "Still done."}]},
        });
        let client = daemon_answering(move |asked| match asked["op"].as_str() {
            // The newest page; then, asked from where it starts, the rest
            // since the margin was brought up to node 2.
            Some("history_nodes") if asked["min_node"].is_null() => json!({
                "nodes": [{"node": 10}, {"node": 9}], "next_from": 8,
            }),
            Some("history_nodes") => {
                assert_eq!(asked["min_node"], 3);
                assert_eq!(asked["from"], 8);
                json!({"nodes": (3..=8).rev().map(|node| json!({"node": node})).collect::<Vec<_>>(),
                    "next_from": null})
            }
            _ => json!({"items": asked["nodes"].as_array().into_iter().flatten()
                .map(|node| json!({"node": node, "item": items.get(node.to_string())
                    .cloned().unwrap_or(json!({"type": "reasoning"}))}))
                .collect::<Vec<_>>()}),
        })
        .await;
        let mut app = app(client);
        let post = |id: &str| json!({"id": id, "title": id, "markdown": "old"});
        app.margin.apply("c1", &post("kept"), None, None);
        app.margin.apply("c2", &post("gone"), None, None);
        app.margin.advance(2);
        app.history().await;
        let kept = app.margin.note("kept").expect("kept");
        assert_eq!(kept.body, margin::Body::Markdown("new".into()));
        assert!(app.margin.note("gone").is_none());
        assert_eq!(app.margin.through(), Some(10));
    }

    #[tokio::test]
    async fn a_note_whose_call_is_past_the_page_and_its_output_on_it_is_kept() {
        let call = json!({"type": "function_call", "call_id": "c8", "name": "margin",
            "arguments": json!({"id": "plan", "title": "Plan", "markdown": "x"}).to_string()});
        let client = daemon_answering(move |asked| match asked["op"].as_str() {
            Some("history_nodes") if asked["min_node"].is_null() => json!({
                "nodes": [{"node": 9}], "next_from": 8,
            }),
            Some("history_nodes") => json!({"nodes": [{"node": 8}], "next_from": null}),
            _ => json!({"items": asked["nodes"].as_array().into_iter().flatten()
                .map(|node| json!({"node": node, "item": match node.as_i64() {
                    Some(8) => call.clone(),
                    _ => json!({"type": "function_call_output", "call_id": "c8", "output": "In the margin."}),
                }}))
                .collect::<Vec<_>>()}),
        })
        .await;
        let mut app = app(client);
        app.margin.advance(2);
        app.history().await;
        assert!(app.margin.note("plan").is_some());
        assert_eq!(app.margin.through(), Some(9));
    }

    #[tokio::test]
    async fn a_margin_whose_catch_up_fell_short_stays_where_it_was() {
        let client = daemon_answering(move |asked| match asked["op"].as_str() {
            Some("history_nodes") if asked["min_node"].is_null() => json!({
                "nodes": [{"node": 9}], "next_from": 8,
            }),
            Some("history_nodes") => json!({"error": "bot_busy", "detail": "busy"}),
            _ => json!({"items": [{"node": 9, "item": {"type": "message", "role": "assistant",
                "content": [{"type": "output_text", "text": "Done."}]}}]}),
        })
        .await;
        let mut app = app(client);
        app.margin.advance(2);
        app.history().await;
        // What it missed is read again next time.
        assert_eq!(app.margin.through(), Some(2));
    }

    #[tokio::test]
    async fn a_note_taken_away_here_and_put_back_stays_on_a_repaint() {
        let told = margin::closed_message("repro");
        let landed = told.clone();
        let call = json!({"type": "function_call", "call_id": "c9", "name": "margin",
            "arguments": json!({"id": "repro", "title": "Repro", "markdown": "again"}).to_string()});
        let client = daemon_answering(move |asked| match asked["op"].as_str() {
            Some("history_nodes") => json!({
                "nodes": [{"node": 9}, {"node": 8}, {"node": 7}], "next_from": null,
            }),
            _ => json!({"items": asked["nodes"].as_array().into_iter().flatten()
                .map(|node| json!({"node": node, "item": match node.as_i64() {
                    Some(7) => json!({"type": "message", "role": "user",
                        "content": [{"type": "input_text", "text": landed}]}),
                    Some(8) => call.clone(),
                    _ => json!({"type": "function_call_output", "call_id": "c9", "output": "In the margin."}),
                }}))
                .collect::<Vec<_>>()}),
        })
        .await;
        let mut app = app(client);
        let post = json!({"id": "repro", "title": "Repro", "markdown": "x"});
        app.margin.apply("c1", &post, None, None);
        // Taken away here; its message lands in a turn already running,
        // after which the agent puts the note back.
        app.margin.remove("repro", None);
        app.closing.insert("repro".into());
        assert!(app.note_closed(7, &told));
        let again = json!({"id": "repro", "title": "Repro", "markdown": "again"});
        app.margin.apply("c9", &again, None, None);
        app.history().await;
        assert!(app.margin.note("repro").is_some());
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
