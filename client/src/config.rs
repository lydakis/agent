//! `~/.agent/config.toml`: the person's settings, kept for every client
//! and the CLI, as Codex keeps its own. Provider keys stay in
//! `~/.agent/env`. A missing file is the defaults; any key the file does
//! not know is a mistake, refused by name, not ignored.
//!
//! ```toml
//! model = "anthropic/claude-opus-5-5"  # a new agent's, with none named
//! provider = "chatgpt"          # the daemon's, with no AGENT_PROVIDER
//! web_search = "disabled"       # "live", the default, lets new agents search and fetch
//! senders = "hidden"            # "shown", the default, labels what agents send
//! attach_wait = "1d"            # how long an attached note waits for a call, the default; "0" as long as it takes
//! hold = "5d"                   # how long a new agent's turn waits on a cause you can fix before it fails, the default; "0" fails at once
//!
//! [daemon]                      # what the agent CLI starts the daemon with; restart it to apply
//! processes = "cpus * 64"       # commands running at once, the default
//! load = "cpus * 2"             # new commands wait while the load average is this, the default; 0 no gate
//! input_bytes = "memory / 16"   # also lasting, active, connecting, pending, pending_bytes, readers
//! stall_timeout = "10m"         # a provider stream with no content this long is retried, the default
//!
//! [fallbacks]                   # the model a declined request goes to, by model
//! "anthropic/claude-opus-5-5" = "anthropic/claude-opus-4-8"
//!
//! [judge]
//! model = "openai/gpt-6-luna"   # who judges for the approver and the lookout
//! effort = "low"                # a model judge's effort
//!
//! [shell_environment_policy]    # what tool shells leave out, as in Codex
//! ignore_default_excludes = false  # also *KEY*, *SECRET* and *TOKEN*; true, the default, keeps them
//! exclude = ["AWS_*"]           # names to leave out, * and ? wildcards, any case
//!
//! [sonny]
//! lead_effort = "medium"        # a new lead's effort
//! task_effort = "high"          # each new task's
//! lead_profile = "my-lead"      # the role a new lead starts in, not lead
//! task_profile = "worker"       # what every other agent is told, not task
//! side_chat = "fork"            # "answer", the default, calls no tool
//! chime = false                 # rings when work ends or waits, the default
//! verbose = true                # every tool call drawn, not the clean view
//! margin_opens = "never"         # "always", the default, or "unless-closed"
//! agent_opens = "beside"        # a click on an agent's name: "tab", the default, or "beside"
//! suggest = false               # no guess at the next message, on by default
//! commons = false               # agents go without the commons, on by default
//! commons_invites = 3           # least threads a day that send invites (more with the square root of the week's agents), 0 none; answers add up to as many again
//! commons_invite_tokens = 1000000  # what an invite pays for in its thread
//! commons_digest = "1h"         # an agent hears its commons threads at most this often, unless asked sooner; 0 at once
//! ask_wait_minutes = 60         # how long an unanswered ask waits to go ahead or remind; 0 never
//! task_windows = 64            # task windows a lead keeps open, 32 by default; past it the least recently used closes, never one not looked at yet; 0 no limit
//! task_depth = 3                # generations of tasks below a lead, 2 by default (tasks' own start none); 0 no end
//! sky_haikus = 0                # haikus a day from the sky's busiest agent, 8 by default; 0 none
//! sky_haiku_window = 10         # minutes of messages and tool calls that make an agent busiest
//! sky_haiku_margin = 50         # how much busier, in percent, a challenger must be to take the title
//! sky_haiku_hold = 10           # minutes the title is held before it can pass
//! tool_processes = 8            # processes Sonny runs for agents at once in all: tool calls 3/4, goal checks 1/4; 0, the default, four times the CPUs
//! default_permissions = "strict"   # the /permissions row a new lead starts in, auto by default
//! browser_confirm = ["eval"]    # agent-browser actions the agent's gate approves; eval, download, upload and state by default
//!
//! [sonny.permissions.strict]    # a /permissions row of the person's own, as Codex's profiles
//! description = "Offline, commits ask"
//! extends = "auto"              # the row it starts from: auto, ask or read-only
//! network = false               # whether commands reach the network
//! git = "read-only"             # "write" lets commits run without asking
//! writable = ["~/.cargo"]       # folders it writes besides its own
//! unix_sockets = ["~/.colima"]  # the only sockets, or their folders, its commands reach
//! reviewer = "model"            # who approves anything more: model, you or none
//!
//! [sonny.mcp_servers.linear]    # an MCP server Sonny's agents reach
//! url = "https://mcp.linear.app/mcp"
//! bearer_token_env_var = "LINEAR_TOKEN"
//! [sonny.sources.slack]         # what Sonny's lookout runs for news
//! command = "~/bin/slack-poll"  # run with sh -c; each line it prints is one event
//! every = "1m"                  # how long after it exits it runs again
//! digest = "15m"                # a subscriber hears it at most this often; unset, as it comes
//! urgent = { from = ["boss"] }  # what cuts into a busy subscriber's turn, as a filter is written
//! ```
//!
//! Each unset value is the model's own, or, for a task, its lead's.
use std::collections::BTreeMap;
use std::io::Read;
use std::path::{Path, PathBuf};

/// The largest file the reader takes.
pub const LIMIT: u64 = 64 * 1024;

#[derive(Clone, Debug, Default, PartialEq)]
pub struct Config {
    /// `provider/model` for a new agent that names none.
    pub model: Option<String>,
    /// The daemon's `--provider` specs, as AGENT_PROVIDER takes them.
    pub provider: Option<String>,
    /// Whether a new agent gets web search where its provider offers it,
    /// and `web_fetch`, as Codex's `web_search` key says: `"live"`, the
    /// default, or `"disabled"`.
    pub web_search: WebSearch,
    /// Whether a new agent's model reads what another agent or a client
    /// sent it with no line naming the sender: `senders = "hidden"`;
    /// `"shown"`, the default, names it.
    pub hide_senders: bool,
    /// `attach_wait`: how long a message sent with delivery attach waits
    /// for a call to take it in before it goes as a steer, in
    /// milliseconds; unset is `ATTACH_WAIT_MS`, and 0 waits as long as it
    /// takes. See `attach_wait`.
    pub attach_wait_ms: Option<u64>,
    /// `hold`: how long a new agent's turn is held on a cause the person
    /// can fix (a sign-in, a usage limit, a provider down) before it fails
    /// with it, in milliseconds, as the bot setting `hold_ms`; unset leaves
    /// the daemon's five days, and 0 fails at once.
    pub hold_ms: Option<u64>,
    /// Each model's fallback, as `provider/model` in its family: a new
    /// agent on that model that names none sends a request a safety
    /// classifier declined to it, that request alone.
    pub fallbacks: BTreeMap<String, String>,
    pub judge: Judge,
    pub shell_environment_policy: ShellEnvironmentPolicy,
    pub sonny: Sonny,
    /// `[daemon]`: each limit the agent CLI starts a daemon with, by key,
    /// as the formula the file gives; see `DAEMON_LIMITS` and `formula`.
    pub daemon: BTreeMap<String, String>,
}

/// How long an attached message waits when the file says nothing: a day,
/// long enough for a quiet agent's next turn, short enough that nobody
/// sends one wondering whether it will ever be read.
pub const ATTACH_WAIT_MS: u64 = 86_400_000;
/// The longest the daemon lets an attached message wait: a year.
pub const MAX_ATTACH_WAIT_MS: u64 = 365 * ATTACH_WAIT_MS;

impl Config {
    /// The `wait_ms` a client sends with delivery attach: none when
    /// `attach_wait` is 0, which waits as long as it takes.
    pub fn attach_wait(&self) -> Option<u64> {
        Some(self.attach_wait_ms.unwrap_or(ATTACH_WAIT_MS)).filter(|&ms| ms > 0)
    }
}

/// A duration in milliseconds: digits and a unit (`ms`, `s`, `m`, `h` or
/// `d`), or a bare `0`. Any other bare number is none, so nothing guesses
/// its unit.
pub fn duration_ms(value: &str) -> Option<u64> {
    if value == "0" {
        return Some(0);
    }
    let unit = value.find(|c: char| !c.is_ascii_digit())?;
    let scale = match &value[unit..] {
        "ms" => 1,
        "s" => 1000,
        "m" => 60_000,
        "h" => 3_600_000,
        "d" => 86_400_000,
        _ => return None,
    };
    value[..unit].parse::<u64>().ok()?.checked_mul(scale)
}

/// The `[daemon]` keys, the `agent serve` flag each sets, and the formula
/// the agent CLI starts a daemon with when the file gives none: none is
/// the daemon's own default. A count rounds down; `load` keeps its
/// fraction.
pub const DAEMON_LIMITS: [(&str, &str, Option<&str>); 9] = [
    ("processes", "--max-processes", None),
    ("lasting", "--max-lasting", None),
    ("active", "--max-active", None),
    ("connecting", "--max-connecting", None),
    ("pending", "--max-pending", None),
    ("pending_bytes", "--max-pending-bytes", None),
    ("input_bytes", "--max-input-bytes", None),
    ("readers", "--max-readers", None),
    // As make -l: a busy machine's new commands wait while one runs.
    ("load", "--max-load", Some("cpus * 2")),
];

/// The machine a formula is worked out for: its logical CPUs and its
/// physical memory in bytes.
pub fn machine() -> (f64, f64) {
    let cpus = std::thread::available_parallelism().map_or(4, |n| n.get());
    // SAFETY: sysconf reads system constants.
    let (pages, size) = unsafe {
        (
            libc::sysconf(libc::_SC_PHYS_PAGES),
            libc::sysconf(libc::_SC_PAGESIZE),
        )
    };
    let memory = (pages.max(0) as f64) * (size.max(0) as f64);
    (cpus as f64, memory)
}

/// The `[daemon]` keys that are durations, as the `agent serve` flag each
/// sets takes them, with the least and most it takes in milliseconds: none
/// given is the daemon's own default.
pub const DAEMON_DURATIONS: [(&str, &str, u64, u64); 1] =
    [("stall_timeout", "--stall-timeout", 1000, 86_400_000)];

/// A `[daemon]` value worked out: a number, or `cpus` and `memory` (bytes)
/// with numbers, `KiB` `MiB` `GiB`, `+ - * /`, parentheses, and
/// `min(a, b)` and `max(a, b)`, as `"cpus * 64"` or `"max(memory / 16,
/// 64 MiB)"`. `key` names it in the error. The result is 0 or more.
pub fn formula(key: &str, text: &str, cpus: f64, memory: f64) -> Result<f64, String> {
    struct Parser<'a> {
        text: &'a [u8],
        at: usize,
        cpus: f64,
        memory: f64,
    }
    impl Parser<'_> {
        fn space(&mut self) {
            while self.text.get(self.at).is_some_and(u8::is_ascii_whitespace) {
                self.at += 1;
            }
        }
        fn eat(&mut self, byte: u8) -> bool {
            self.space();
            let found = self.text.get(self.at) == Some(&byte);
            self.at += usize::from(found);
            found
        }
        fn word(&mut self) -> &str {
            self.space();
            let start = self.at;
            while self.text.get(self.at).is_some_and(u8::is_ascii_alphabetic) {
                self.at += 1;
            }
            std::str::from_utf8(&self.text[start..self.at]).unwrap_or("")
        }
        fn sum(&mut self) -> Option<f64> {
            let mut value = self.product()?;
            loop {
                if self.eat(b'+') {
                    value += self.product()?;
                } else if self.eat(b'-') {
                    value -= self.product()?;
                } else {
                    return Some(value);
                }
            }
        }
        fn product(&mut self) -> Option<f64> {
            let mut value = self.term()?;
            loop {
                if self.eat(b'*') {
                    value *= self.term()?;
                } else if self.eat(b'/') {
                    let by = self.term()?;
                    value = (by != 0.0).then(|| value / by)?;
                } else {
                    return Some(value);
                }
            }
        }
        fn term(&mut self) -> Option<f64> {
            if self.eat(b'(') {
                let value = self.sum()?;
                return self.eat(b')').then_some(value);
            }
            self.space();
            let start = self.at;
            while self
                .text
                .get(self.at)
                .is_some_and(|b| b.is_ascii_digit() || *b == b'.')
            {
                self.at += 1;
            }
            if self.at > start {
                let number: f64 = std::str::from_utf8(&self.text[start..self.at])
                    .ok()?
                    .parse()
                    .ok()?;
                let mark = self.at;
                let scale = match self.word() {
                    "KiB" => 1024.0,
                    "MiB" => 1024.0 * 1024.0,
                    "GiB" => 1024.0 * 1024.0 * 1024.0,
                    "" => 1.0,
                    _ => {
                        self.at = mark;
                        return None;
                    }
                };
                return Some(number * scale);
            }
            match self.word() {
                "cpus" => Some(self.cpus),
                "memory" => Some(self.memory),
                pick @ ("min" | "max") => {
                    let pick = pick == "min";
                    if !self.eat(b'(') {
                        return None;
                    }
                    let a = self.sum()?;
                    if !self.eat(b',') {
                        return None;
                    }
                    let b = self.sum()?;
                    self.eat(b')')
                        .then(|| if pick { a.min(b) } else { a.max(b) })
                }
                _ => None,
            }
        }
    }
    let mut parser = Parser {
        text: text.as_bytes(),
        at: 0,
        cpus,
        memory,
    };
    let value = parser.sum();
    parser.space();
    match value {
        Some(value) if parser.at == text.len() && value.is_finite() && value >= 0.0 => Ok(value),
        _ => Err(format!(
            "daemon.{key} is a number or a formula of cpus and memory, as \"cpus * 64\", 0 or more, not {text:?}"
        )),
    }
}

/// Each `[daemon]` limit as the `agent serve` flag and value it starts a
/// daemon with, worked out for this machine, the defaults included.
pub fn daemon_flags(daemon: &BTreeMap<String, String>) -> Result<Vec<(String, String)>, String> {
    let (cpus, memory) = machine();
    let mut flags = Vec::new();
    for (key, flag, ..) in DAEMON_DURATIONS {
        if let Some(text) = daemon.get(key) {
            flags.push((flag.to_owned(), text.clone()));
        }
    }
    for (key, flag, default) in DAEMON_LIMITS {
        let Some(text) = daemon.get(key).map(String::as_str).or(default) else {
            continue;
        };
        let value = formula(key, text, cpus, memory)?;
        flags.push((
            flag.to_owned(),
            match key {
                // Kept whole: a small positive limit is still a limit.
                "load" => format!("{value}"),
                _ => format!("{}", value.floor() as u64),
            },
        ));
    }
    Ok(flags)
}

/// What Codex's default excludes leave out when they apply.
pub const DEFAULT_EXCLUDES: [&str; 3] = ["*KEY*", "*SECRET*", "*TOKEN*"];

/// Codex's `[shell_environment_policy]` excludes: variables of the daemon's
/// environment that no tool shell gets, past the provider keys it always
/// withholds. A daemon takes them when it starts.
#[derive(Clone, Debug, PartialEq)]
pub struct ShellEnvironmentPolicy {
    /// False also leaves out `DEFAULT_EXCLUDES`; true, Codex's default,
    /// keeps them.
    pub ignore_default_excludes: bool,
    /// Names to leave out, with `*` and `?` wildcards, in any case.
    pub exclude: Vec<String>,
}

impl Default for ShellEnvironmentPolicy {
    fn default() -> Self {
        ShellEnvironmentPolicy {
            ignore_default_excludes: true,
            exclude: Vec::new(),
        }
    }
}

impl ShellEnvironmentPolicy {
    /// Every pattern a daemon leaves out of tool shells.
    pub fn excludes(&self) -> Vec<String> {
        let defaults = DEFAULT_EXCLUDES
            .iter()
            .filter(|_| !self.ignore_default_excludes)
            .map(|pattern| (*pattern).to_owned());
        defaults.chain(self.exclude.iter().cloned()).collect()
    }
}

/// Codex's `web_search` modes this runtime serves. Codex's `cached`, an
/// index without the live web, is not one: Anthropic's search has no such
/// mode.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum WebSearch {
    #[default]
    Live,
    Disabled,
}

/// The judge `sonny approver` and Sonny's lookout ask.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Judge {
    /// `provider/model`; unset, the judged bot's own model.
    pub model: Option<String>,
    pub effort: Option<String>,
}

/// What Sonny starts its agents with, and how it shows them.
#[derive(Clone, Debug, PartialEq)]
pub struct Sonny {
    pub lead_effort: Option<String>,
    pub task_effort: Option<String>,
    /// The profile a lead runs as, in place of `lead`.
    pub lead_profile: Option<String>,
    /// The profile every other agent Sonny starts is told, in place of
    /// `task`.
    pub task_profile: Option<String>,
    /// What `[sonny] unsandboxed` named before command rules took its
    /// place: read only so Sonny can move it into them (upgrade migration
    /// `unsandboxed-rules`, retired with it).
    pub unsandboxed: Vec<String>,
    /// The `/permissions` row a new lead starts in; unset, `auto`.
    pub default_permissions: Option<String>,
    /// `/permissions` rows of the person's own, by name.
    pub permissions: BTreeMap<String, Permissions>,
    /// The MCP servers Sonny's agents reach, by name.
    pub mcp_servers: BTreeMap<String, McpServer>,
    pub side_chat: SideChat,
    /// Whether finished and waiting work rings. Notifications show either way.
    pub chime: bool,
    /// Whether every tool call is drawn, or only the bot's words and the
    /// calls that move the work on (the clean view, the default).
    pub verbose: bool,
    /// When a note the agent puts up or changes opens the margin.
    pub margin_opens: MarginOpens,
    /// Where a click on an agent's name takes the person; a right click
    /// takes them to the other.
    pub agent_opens: AgentOpens,
    /// Whether a copy of the agent guesses the person's next message after
    /// a turn they started, shown in the empty composer for Tab to take.
    pub suggest: bool,
    /// Whether the daemon starts when the machine does, for a box whose
    /// agents keep working after a restart. Sonny keeps the job.
    pub start_at_boot: bool,
    /// Whether agents have the commons: its tool, skill and the roles'
    /// lines about it.
    pub commons: bool,
    /// The least commons threads a day that send invites before answers
    /// earn more, the square root of the week's agents when that is more:
    /// each answered thread adds one, up to as many again.
    pub commons_invites: u64,
    /// The tokens an invite pays for in its thread.
    pub commons_invite_tokens: u64,
    /// How often at most an agent hears of new posts in its commons
    /// threads, as a `digest` of the commons source: `1h`, or `0` for as
    /// they come.
    pub commons_digest: String,
    /// How long an `ask` the person has not answered waits, in minutes,
    /// before its agent goes on with what it recommended, where it said
    /// it could, or is reminded of it; 0 lets every ask wait for them.
    pub ask_wait_minutes: u64,
    /// How many of its tasks' windows a lead keeps open in its
    /// multiplexer: past it, a new one closes the window the person used
    /// least recently, never one they have not looked at yet; 0 for no
    /// limit.
    pub task_windows: u64,
    /// How many generations below a lead its line goes: a lead's tasks
    /// are the first, and an agent of the last starts none; 0 for no end.
    pub task_depth: u64,
    /// The haikus a day the sky's busiest agent writes, each time the
    /// title passes; 0 for none.
    pub sky_haikus: u64,
    /// The minutes of messages and tool calls that make an agent busiest.
    pub sky_haiku_window: u64,
    /// How much busier, in percent, a challenger must be to take the
    /// title, and the minutes it is held before it can pass: a
    /// thermostat's margin and hold, so it does not flicker.
    pub sky_haiku_margin: u64,
    pub sky_haiku_hold: u64,
    /// The processes Sonny runs for agents at once in all: three quarters
    /// for tool calls the tool host answers, a quarter for goal checks,
    /// each at least one. 0 for four times the CPUs.
    pub tool_processes: u64,
    /// What the lookout runs for news from outside, by name.
    pub sources: Vec<Source>,
    /// The agent-browser action categories that the `browser` tool puts
    /// to the agent's gate before they run.
    pub browser_confirm: Vec<String>,
}

/// The agent-browser action categories its `browser` tool confirms unless
/// the config says otherwise: agent-browser's own destructive ones, and
/// saved state, which carries sign-ins.
pub const BROWSER_CONFIRM: [&str; 4] = ["eval", "download", "upload", "state"];

/// How often at most an agent hears of its commons threads, unless the
/// config says otherwise: an hour, as Slack's slowest email batching.
pub const COMMONS_DIGEST: &str = "1h";

/// The name of the source Sonny's lookout makes of the commons.
pub const COMMONS_SOURCE: &str = "commons";

/// A span of time as a count and `s`, `m` or `h`, as `30s`, `5m` or `1h`,
/// in seconds; `0` alone is none. `key` names it in the error.
pub fn span(key: &str, text: &str) -> Result<u64, String> {
    if text == "0" {
        return Ok(0);
    }
    let bad = || format!("{key} is a number and s, m or h, as 30s or 5m, not {text:?}");
    let (count, unit) = [("s", 1), ("m", 60), ("h", 3600)]
        .into_iter()
        .find_map(|(suffix, unit)| Some((text.strip_suffix(suffix)?, unit)))
        .ok_or_else(bad)?;
    if !count.bytes().all(|b| b.is_ascii_digit()) {
        return Err(bad());
    }
    let count: u64 = count.parse().map_err(|_| bad())?;
    count.checked_mul(unit).ok_or_else(bad)
}

impl Default for Sonny {
    fn default() -> Self {
        Sonny {
            lead_effort: None,
            task_effort: None,
            lead_profile: None,
            task_profile: None,
            unsandboxed: Vec::new(),
            default_permissions: None,
            permissions: BTreeMap::new(),
            mcp_servers: BTreeMap::new(),
            side_chat: SideChat::default(),
            suggest: true,
            commons: true,
            commons_invites: 3,
            commons_invite_tokens: 1_000_000,
            commons_digest: COMMONS_DIGEST.to_owned(),
            ask_wait_minutes: 60,
            task_windows: 32,
            task_depth: 2,
            sky_haikus: 8,
            sky_haiku_window: 10,
            sky_haiku_margin: 50,
            sky_haiku_hold: 10,
            tool_processes: 0,
            chime: true,
            verbose: false,
            start_at_boot: false,
            margin_opens: MarginOpens::default(),
            agent_opens: AgentOpens::default(),
            sources: Vec::new(),
            browser_confirm: BROWSER_CONFIRM.map(str::to_owned).to_vec(),
        }
    }
}

/// What the copy `/side` talks to may do: `"answer"`, the default, from
/// what its agent knows, calling no tool; or `"fork"`, anything its agent
/// may, with its tools, in its folder, under its approvals.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum SideChat {
    #[default]
    Answer,
    Fork,
}

/// When a note opens the margin beside the conversation, as VS Code's
/// `reveal` opens a task's terminal: `"always"`, the default, even when
/// the person closed it; `"unless-closed"`, not once they closed it with
/// Esc, until `/margin`; or `"never"`, the margin opening only with
/// `/margin`. It never closes by itself.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum MarginOpens {
    #[default]
    Always,
    UnlessClosed,
    Never,
}

/// The rows Sonny's `/permissions` always has, by id.
pub const PERMISSIONS: [&str; 4] = ["auto", "ask", "read-only", "full"];

/// A `[sonny.permissions.NAME]` row, as Codex's permission profiles: a
/// built-in row it starts from, with what it changes. Unset, each is the
/// row's own.
#[derive(Clone, Debug, PartialEq)]
pub struct Permissions {
    pub description: Option<String>,
    /// `auto`, `ask` or `read-only`.
    pub extends: String,
    /// Whether commands reach the network.
    pub network: Option<bool>,
    /// Whether commits to the folder's repository run without asking.
    pub git: Option<bool>,
    /// Folders, `~` for home, it writes besides its own.
    pub writable: Vec<String>,
    /// Unix sockets, or folders of them, `~` for home, the only ones its
    /// commands reach.
    pub unix_sockets: Vec<String>,
    /// Who approves what leaves the sandbox.
    pub reviewer: Option<Reviewer>,
}

/// Who approves a call that leaves an agent's sandbox: a reviewer model,
/// the person, or no one, so the agent is told to stay inside, as Codex's
/// `never`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Reviewer {
    Model,
    You,
    None,
}

/// Where a click on an agent's name takes the person: `"tab"`, the
/// default, to the agent's own tab, where a lead in Rex, Herdr or cmux laid
/// it out; or `"beside"`, a look at it beside the conversation. A right
/// click takes them to the other, and an agent with no tab opens beside.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum AgentOpens {
    #[default]
    Tab,
    Beside,
}

/// A command Sonny's lookout runs, whose every printed line is news for
/// the agents subscribed to it by name.
#[derive(Clone, Debug, PartialEq)]
pub struct Source {
    pub name: String,
    /// Run with `sh -c`.
    pub command: String,
    /// How long after it exits it runs again, as `30s`, `5m` or `1h`;
    /// unset, a minute.
    pub every: Option<String>,
    /// How often at most a subscriber hears it, as `every` is written,
    /// unless an event is urgent; unset or `0`, as events come.
    pub digest: Option<String>,
    /// An event pattern, as a subscription's filter is written: an event
    /// it matches is urgent, as one the command marks so.
    pub urgent: Option<serde_json::Value>,
}

/// An MCP server, under Codex's key names for `[mcp_servers.NAME]`: a
/// command Sonny starts and speaks to on its stdio, or a URL it speaks
/// streamable HTTP to.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct McpServer {
    pub command: Option<String>,
    pub args: Vec<String>,
    pub env: BTreeMap<String, String>,
    pub cwd: Option<PathBuf>,
    pub url: Option<String>,
    /// The variable whose value is sent as the bearer token.
    pub bearer_token_env_var: Option<String>,
    pub http_headers: BTreeMap<String, String>,
    pub enabled: bool,
    /// Only these of its tools, when set.
    pub enabled_tools: Option<Vec<String>>,
    pub disabled_tools: Vec<String>,
    pub startup_timeout_sec: Option<f64>,
    pub tool_timeout_sec: Option<f64>,
    /// What it offers, in a sentence, for the agents told of it.
    pub description: Option<String>,
    /// When a script's call of one of its tools asks the agent's gate
    /// first, unless `tools` says otherwise for the tool.
    pub default_tools_approval_mode: ApprovalMode,
    /// Each tool's own `approval_mode`.
    pub tools: BTreeMap<String, ApprovalMode>,
}

impl McpServer {
    /// The approval mode of a call of `tool`: its own, else the server's.
    pub fn approval_mode(&self, tool: &str) -> ApprovalMode {
        self.tools
            .get(tool)
            .copied()
            .unwrap_or(self.default_tools_approval_mode)
    }
}

/// When a script's call of an MCP tool asks the agent's gate first, under
/// Codex's names: `auto` when the tool says it is destructive and not that
/// it only reads, `writes` unless it says it only reads, `prompt` always,
/// `approve` never. An agent under no gate never asks.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum ApprovalMode {
    #[default]
    Auto,
    Writes,
    Prompt,
    Approve,
}

impl ApprovalMode {
    fn parse(at: &str, value: &toml::Value) -> Result<Self, String> {
        match value.as_str() {
            Some("auto") => Ok(Self::Auto),
            Some("writes") => Ok(Self::Writes),
            Some("prompt") => Ok(Self::Prompt),
            Some("approve") => Ok(Self::Approve),
            _ => Err(format!("{at} is auto, writes, prompt or approve")),
        }
    }

    /// Whether a call of a tool with these MCP annotations asks first. A
    /// read-only hint wins over a destructive one, as in Codex.
    pub fn asks(self, annotations: &serde_json::Value) -> bool {
        let reads = annotations["readOnlyHint"] == true;
        match self {
            Self::Auto => annotations["destructiveHint"] == true && !reads,
            Self::Writes => !reads,
            Self::Prompt => true,
            Self::Approve => false,
        }
    }
}

const MCP_KEYS: [&str; 15] = [
    "command",
    "args",
    "env",
    "cwd",
    "url",
    "bearer_token_env_var",
    "http_headers",
    "enabled",
    "enabled_tools",
    "disabled_tools",
    "startup_timeout_sec",
    "tool_timeout_sec",
    "description",
    "default_tools_approval_mode",
    "tools",
];

/// The server `[sonny.mcp_servers.NAME]` describes.
fn mcp_server(name: &str, value: &toml::Value) -> Result<McpServer, String> {
    let at = format!("sonny.mcp_servers.{name}");
    let named = !name.is_empty()
        && name.len() <= 64
        && name
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-');
    if !named {
        return Err(format!("{at}: a name is 1 to 64 letters, digits, _ or -"));
    }
    let table = value
        .as_table()
        .ok_or_else(|| format!("{at} must be a table"))?;
    let strings = |key: &str, value: &toml::Value| -> Result<Vec<String>, String> {
        let list = value
            .as_array()
            .ok_or_else(|| format!("{at}.{key} must be a list of strings"))?;
        list.iter()
            .map(|item| item.as_str().map(str::to_owned))
            .collect::<Option<_>>()
            .ok_or_else(|| format!("{at}.{key} must be a list of strings"))
    };
    let pairs = |key: &str, value: &toml::Value| -> Result<BTreeMap<String, String>, String> {
        let table = value
            .as_table()
            .ok_or_else(|| format!("{at}.{key} must be a table of strings"))?;
        table
            .iter()
            .map(|(k, v)| v.as_str().map(|v| (k.clone(), v.to_owned())))
            .collect::<Option<_>>()
            .ok_or_else(|| format!("{at}.{key} must be a table of strings"))
    };
    let seconds = |key: &str, value: &toml::Value| -> Result<f64, String> {
        match value.as_float().or(value.as_integer().map(|n| n as f64)) {
            Some(n) if n > 0.0 && n <= 3600.0 => Ok(n),
            _ => Err(format!("{at}.{key} is a number of seconds, up to 3600")),
        }
    };
    let mut server = McpServer {
        enabled: true,
        ..McpServer::default()
    };
    for (key, value) in table {
        let key = key.as_str();
        if !MCP_KEYS.contains(&key) {
            return Err(format!("unknown key {at}.{key}"));
        }
        match key {
            "args" => server.args = strings(key, value)?,
            "enabled_tools" => server.enabled_tools = Some(strings(key, value)?),
            "disabled_tools" => server.disabled_tools = strings(key, value)?,
            "env" => server.env = pairs(key, value)?,
            "http_headers" => server.http_headers = pairs(key, value)?,
            "enabled" => {
                server.enabled = value
                    .as_bool()
                    .ok_or_else(|| format!("{at}.enabled must be true or false"))?
            }
            "startup_timeout_sec" => server.startup_timeout_sec = Some(seconds(key, value)?),
            "default_tools_approval_mode" => {
                server.default_tools_approval_mode =
                    ApprovalMode::parse(&format!("{at}.{key}"), value)?
            }
            "tools" => {
                let tools = value
                    .as_table()
                    .ok_or_else(|| format!("{at}.tools must be a table of tools"))?;
                for (tool, settings) in tools {
                    let at = format!("{at}.tools.{tool}");
                    let settings = settings
                        .as_table()
                        .ok_or_else(|| format!("{at} must be a table"))?;
                    for (key, value) in settings {
                        if key != "approval_mode" {
                            return Err(format!("unknown key {at}.{key}"));
                        }
                        let mode = ApprovalMode::parse(&format!("{at}.{key}"), value)?;
                        server.tools.insert(tool.clone(), mode);
                    }
                }
            }
            "tool_timeout_sec" => server.tool_timeout_sec = Some(seconds(key, value)?),
            _ => {
                let text = string(&format!("{at}.{key}"), value)?;
                match key {
                    "command" => server.command = Some(text),
                    "cwd" => server.cwd = Some(PathBuf::from(text)),
                    "url" => server.url = Some(text),
                    "bearer_token_env_var" => server.bearer_token_env_var = Some(text),
                    _ => server.description = Some(text),
                }
            }
        }
    }
    match (&server.command, &server.url) {
        (Some(_), None) => {
            for key in ["bearer_token_env_var", "http_headers"] {
                if table.contains_key(key) {
                    return Err(format!("{at}.{key} goes with url"));
                }
            }
        }
        (None, Some(url)) => {
            if !url.starts_with("https://") && !url.starts_with("http://") {
                return Err(format!("{at}.url must be http or https"));
            }
            for key in ["args", "env", "cwd"] {
                if table.contains_key(key) {
                    return Err(format!("{at}.{key} goes with command"));
                }
            }
        }
        _ => return Err(format!("{at} takes a command or a url")),
    }
    Ok(server)
}

pub fn path() -> Option<PathBuf> {
    std::env::var_os("HOME").map(|home| PathBuf::from(home).join(".agent/config.toml"))
}

/// The agent's own home, `~/.agent`: its settings, keys, schedules, rules
/// and Sonny's files, read as the person's and some run outside the
/// sandbox. A bot whose folder holds it (Sonny opened in HOME) may write
/// there only what it is given by name, as Codex keeps `~/.codex`.
pub fn home() -> Option<PathBuf> {
    std::env::var_os("HOME").map(|home| PathBuf::from(home).join(".agent"))
}

/// The person's command rules, `~/.agent/rules`: each `*.rules` file in it
/// holds Codex's `prefix_rule` calls, which every agent's commands follow.
pub fn rules() -> Option<PathBuf> {
    std::env::var_os("HOME").map(|home| PathBuf::from(home).join(".agent/rules"))
}

/// Allows every command that starts with `words`, in `folder`'s
/// `default.rules`, as Codex's "don't ask again" appends its rule: one
/// line, `prefix_rule(pattern=[...], decision="allow")`. False when the
/// file holds that rule already.
pub fn allow_rule(folder: &Path, words: &[String]) -> Result<bool, String> {
    use std::io::Write;
    if words.is_empty() || words.iter().any(|word| word.is_empty()) {
        return Err("a rule needs words".into());
    }
    let pattern = serde_json::to_string(words).map_err(|e| e.to_string())?;
    let rule = format!("prefix_rule(pattern={pattern}, decision=\"allow\")");
    let path = folder.join("default.rules");
    let text = match std::fs::read_to_string(&path) {
        Ok(text) => text,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => String::new(),
        Err(error) => return Err(format!("{}: {error}", path.display())),
    };
    if text.lines().any(|line| line.trim() == rule) {
        return Ok(false);
    }
    std::fs::create_dir_all(folder).map_err(|e| format!("{}: {e}", folder.display()))?;
    let mut file = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&path)
        .map_err(|e| format!("{}: {e}", path.display()))?;
    // One write, so a line another writer appends lands whole beside it.
    let line = match text.is_empty() || text.ends_with('\n') {
        true => format!("{rule}\n"),
        false => format!("\n{rule}\n"),
    };
    file.write_all(line.as_bytes())
        .map_err(|e| format!("{}: {e}", path.display()))?;
    Ok(true)
}

/// The person's settings: the defaults with no HOME or no file.
pub fn load() -> Result<Config, String> {
    path().map_or(Ok(Config::default()), |path| read(&path))
}

/// The settings in the file at `path`.
pub fn read(path: &Path) -> Result<Config, String> {
    let invalid = |reason: &str| format!("config_invalid: {}: {reason}", path.display());
    let file = match std::fs::File::open(path) {
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return Ok(Config::default());
        }
        Err(error) => return Err(format!("config_unreadable: {}: {error}", path.display())),
        Ok(file) => file,
    };
    if !file
        .metadata()
        .map_err(|e| invalid(&e.to_string()))?
        .is_file()
    {
        return Err(invalid("not a regular file"));
    }
    let mut text = String::new();
    file.take(LIMIT + 1)
        .read_to_string(&mut text)
        .map_err(|e| format!("config_unreadable: {}: {e}", path.display()))?;
    if text.len() as u64 > LIMIT {
        return Err(invalid("larger than 64 KiB"));
    }
    parse(&text).map_err(|reason| invalid(&reason))
}

fn parse(text: &str) -> Result<Config, String> {
    let table: toml::Table = text
        .parse()
        .map_err(|e: toml::de::Error| e.message().to_owned())?;
    let mut config = Config::default();
    for (name, value) in &table {
        if name == "web_search" {
            config.web_search = match string(name, value)?.as_str() {
                "live" => WebSearch::Live,
                "disabled" => WebSearch::Disabled,
                other => return Err(format!("web_search is live or disabled, not {other}")),
            };
            continue;
        }
        if name == "senders" {
            config.hide_senders = match string(name, value)?.as_str() {
                "shown" => false,
                "hidden" => true,
                other => return Err(format!("senders is shown or hidden, not {other}")),
            };
            continue;
        }
        if name == "hold" {
            let text = string(name, value)?;
            let ms = duration_ms(&text)
                .filter(|&ms| ms <= MAX_ATTACH_WAIT_MS)
                .ok_or_else(|| {
                    format!("hold is a duration such as 1h or 5d, up to 365d, or \"0\", not {text}")
                })?;
            config.hold_ms = Some(ms);
            continue;
        }
        if name == "attach_wait" {
            let text = string(name, value)?;
            let ms = duration_ms(&text)
                .filter(|&ms| ms <= MAX_ATTACH_WAIT_MS)
                .ok_or_else(|| {
                    format!("attach_wait is a duration such as 1h or 1d, up to 365d, or \"0\", not {text}")
                })?;
            config.attach_wait_ms = Some(ms);
            continue;
        }
        if name == "fallbacks" {
            let models = value.as_table().ok_or("fallbacks must be a table")?;
            for (model, fallback) in models {
                let fallback = string(&format!("fallbacks.{model}"), fallback)?;
                config.fallbacks.insert(model.clone(), fallback);
            }
            continue;
        }
        if let Some(top) = match name.as_str() {
            "model" => Some(&mut config.model),
            "provider" => Some(&mut config.provider),
            _ => None,
        } {
            *top = Some(string(name, value)?);
            continue;
        }
        let section = value
            .as_table()
            .ok_or_else(|| format!("unknown key {name}"))?;
        if name == "daemon" {
            for (key, value) in section {
                if let Some((_, _, least, most)) =
                    DAEMON_DURATIONS.iter().find(|(known, ..)| known == key)
                {
                    let refuse = || {
                        format!(
                            "daemon.{key} is a duration in whole seconds from {}s to {}s, as \"10m\"",
                            least / 1000,
                            most / 1000
                        )
                    };
                    let text = value.as_str().ok_or_else(refuse)?;
                    match duration_ms(text) {
                        Some(ms) if ms % 1000 == 0 && (*least..=*most).contains(&ms) => {}
                        _ => return Err(refuse()),
                    }
                    config.daemon.insert(key.clone(), text.to_owned());
                    continue;
                }
                if !DAEMON_LIMITS.iter().any(|(known, ..)| known == key) {
                    return Err(format!("unknown key daemon.{key}"));
                }
                let text = match value {
                    toml::Value::Integer(n) if *n >= 0 => n.to_string(),
                    toml::Value::Float(n) if *n >= 0.0 && n.is_finite() => n.to_string(),
                    toml::Value::String(text) => text.clone(),
                    _ => {
                        return Err(format!(
                            "daemon.{key} must be a number 0 or more, or a formula as \"cpus * 64\""
                        ));
                    }
                };
                // Worked out here for this machine, as the agent CLI will
                // start the daemon, so a mistake is refused by name now.
                let (cpus, memory) = machine();
                formula(key, &text, cpus, memory)?;
                config.daemon.insert(key.clone(), text);
            }
            continue;
        }
        let keys: &[&str] = match name.as_str() {
            "judge" => &["model", "effort"],
            "shell_environment_policy" => &["ignore_default_excludes", "exclude"],
            "sonny" => &[
                "lead_effort",
                "task_effort",
                "lead_profile",
                "task_profile",
                "side_chat",
                "chime",
                "verbose",
                "margin_opens",
                "agent_opens",
                "suggest",
                "start_at_boot",
                "commons",
                "commons_invites",
                "commons_invite_tokens",
                "commons_digest",
                "ask_wait_minutes",
                "task_windows",
                "task_depth",
                "sky_haikus",
                "sky_haiku_window",
                "sky_haiku_margin",
                "sky_haiku_hold",
                "tool_processes",
                "unsandboxed",
                "default_permissions",
                "browser_confirm",
            ],
            _ => return Err(format!("unknown table [{name}]")),
        };
        for (key, value) in section {
            if (name.as_str(), key.as_str()) == ("sonny", "mcp_servers") {
                let servers = value
                    .as_table()
                    .ok_or("sonny.mcp_servers must be a table")?;
                for (server, value) in servers {
                    let parsed = mcp_server(server, value)?;
                    config.sonny.mcp_servers.insert(server.clone(), parsed);
                }
                continue;
            }
            if (name.as_str(), key.as_str()) == ("sonny", "sources") {
                config.sonny.sources = sources(value)?;
                continue;
            }
            if (name.as_str(), key.as_str()) == ("sonny", "permissions") {
                config.sonny.permissions = permissions(value)?;
                continue;
            }
            if !keys.contains(&key.as_str()) {
                return Err(format!("unknown key {name}.{key}"));
            }
            if name == "shell_environment_policy" {
                let policy = &mut config.shell_environment_policy;
                if key == "ignore_default_excludes" {
                    policy.ignore_default_excludes = value
                        .as_bool()
                        .ok_or_else(|| format!("{name}.{key} must be true or false"))?;
                    continue;
                }
                let invalid = || format!("{name}.{key} must be a list of names");
                let patterns = value.as_array().ok_or_else(invalid)?;
                policy.exclude = patterns
                    .iter()
                    .map(|pattern| match pattern.as_str() {
                        Some(pattern) if !pattern.trim().is_empty() => Ok(pattern.to_owned()),
                        _ => Err(invalid()),
                    })
                    .collect::<Result<_, _>>()?;
                continue;
            }
            if let Some(switch) = match key.as_str() {
                "chime" => Some(&mut config.sonny.chime),
                "suggest" => Some(&mut config.sonny.suggest),
                "commons" => Some(&mut config.sonny.commons),
                "verbose" => Some(&mut config.sonny.verbose),
                "start_at_boot" => Some(&mut config.sonny.start_at_boot),
                _ => None,
            } {
                *switch = value
                    .as_bool()
                    .ok_or_else(|| format!("{name}.{key} must be true or false"))?;
                continue;
            }
            if let Some(count) = match key.as_str() {
                "commons_invites" => Some(&mut config.sonny.commons_invites),
                "commons_invite_tokens" => Some(&mut config.sonny.commons_invite_tokens),
                "ask_wait_minutes" => Some(&mut config.sonny.ask_wait_minutes),
                "task_windows" => Some(&mut config.sonny.task_windows),
                "task_depth" => Some(&mut config.sonny.task_depth),
                "sky_haikus" => Some(&mut config.sonny.sky_haikus),
                "sky_haiku_window" => Some(&mut config.sonny.sky_haiku_window),
                "sky_haiku_margin" => Some(&mut config.sonny.sky_haiku_margin),
                "sky_haiku_hold" => Some(&mut config.sonny.sky_haiku_hold),
                "tool_processes" => Some(&mut config.sonny.tool_processes),
                _ => None,
            } {
                *count = (value.as_integer())
                    .and_then(|n| u64::try_from(n).ok())
                    .ok_or_else(|| format!("{name}.{key} must be a whole number, 0 or more"))?;
                continue;
            }
            if (name.as_str(), key.as_str()) == ("sonny", "browser_confirm") {
                let invalid =
                    || format!("{name}.{key} must be a list of agent-browser action categories");
                let categories = value.as_array().ok_or_else(invalid)?;
                config.sonny.browser_confirm = categories
                    .iter()
                    .map(|category| match category.as_str() {
                        Some(category)
                            if !category.is_empty()
                                && (category.bytes())
                                    .all(|b| b.is_ascii_alphanumeric() || b":_-.".contains(&b)) =>
                        {
                            Ok(category.to_owned())
                        }
                        _ => Err(invalid()),
                    })
                    .collect::<Result<_, _>>()?;
                continue;
            }
            if (name.as_str(), key.as_str()) == ("sonny", "unsandboxed") {
                let invalid = || format!("{name}.{key} must be a list of commands");
                let commands = value.as_array().ok_or_else(invalid)?;
                config.sonny.unsandboxed = commands
                    .iter()
                    .map(|command| match command.as_str() {
                        Some(command) if !command.trim().is_empty() => Ok(command.to_owned()),
                        _ => Err(invalid()),
                    })
                    .collect::<Result<_, _>>()?;
                continue;
            }
            let value = string(&format!("{name}.{key}"), value)?;
            if key == "side_chat" {
                config.sonny.side_chat = match value.as_str() {
                    "answer" => SideChat::Answer,
                    "fork" => SideChat::Fork,
                    other => return Err(format!("sonny.side_chat is answer or fork, not {other}")),
                };
                continue;
            }
            if key == "commons_digest" {
                span(&format!("{name}.{key}"), &value)?;
                config.sonny.commons_digest = value;
                continue;
            }
            if key == "margin_opens" {
                config.sonny.margin_opens = match value.as_str() {
                    "always" => MarginOpens::Always,
                    "unless-closed" => MarginOpens::UnlessClosed,
                    "never" => MarginOpens::Never,
                    other => {
                        return Err(format!(
                            "sonny.margin_opens is always, unless-closed or never, not {other}"
                        ));
                    }
                };
                continue;
            }
            if key == "agent_opens" {
                config.sonny.agent_opens = match value.as_str() {
                    "tab" => AgentOpens::Tab,
                    "beside" => AgentOpens::Beside,
                    other => {
                        return Err(format!("sonny.agent_opens is tab or beside, not {other}"));
                    }
                };
                continue;
            }
            // The daemon judges models and levels when the agents are made.
            *match (name.as_str(), key.as_str()) {
                ("judge", "model") => &mut config.judge.model,
                ("judge", "effort") => &mut config.judge.effort,
                ("sonny", "lead_effort") => &mut config.sonny.lead_effort,
                ("sonny", "lead_profile") => &mut config.sonny.lead_profile,
                ("sonny", "task_profile") => &mut config.sonny.task_profile,
                ("sonny", "default_permissions") => &mut config.sonny.default_permissions,
                _ => &mut config.sonny.task_effort,
            } = Some(value);
        }
    }
    if let Some(row) = &config.sonny.default_permissions
        && !PERMISSIONS.contains(&row.as_str())
        && !config.sonny.permissions.contains_key(row)
    {
        return Err(format!(
            "sonny.default_permissions is auto, ask, read-only, full or a [sonny.permissions] row, not {row}"
        ));
    }
    Ok(config)
}

/// `[sonny.permissions.NAME]` tables, each a row `/permissions` lists.
fn permissions(value: &toml::Value) -> Result<BTreeMap<String, Permissions>, String> {
    let table = (value.as_table()).ok_or("sonny.permissions must be a table of rows")?;
    let mut rows = BTreeMap::new();
    for (name, row) in table {
        let key = |field: &str| format!("sonny.permissions.{name}.{field}");
        if name.is_empty()
            || name.len() > 32
            || !(name.bytes()).all(|b| b.is_ascii_alphanumeric() || b"-_".contains(&b))
        {
            return Err(format!(
                "sonny.permissions.{name}: a row's name is 1 to 32 of A-Z a-z 0-9 _ -"
            ));
        }
        if PERMISSIONS.contains(&name.as_str()) {
            return Err(format!(
                "sonny.permissions.{name}: {name} is a built-in row; name yours otherwise"
            ));
        }
        let fields =
            (row.as_table()).ok_or_else(|| format!("sonny.permissions.{name} must be a table"))?;
        let mut parsed = Permissions {
            description: None,
            extends: "auto".into(),
            network: None,
            git: None,
            writable: Vec::new(),
            unix_sockets: Vec::new(),
            reviewer: None,
        };
        for (field, value) in fields {
            let flag = || {
                value
                    .as_bool()
                    .ok_or_else(|| format!("{} must be true or false", key(field)))
            };
            match field.as_str() {
                "description" => parsed.description = Some(string(&key(field), value)?),
                "extends" => {
                    parsed.extends = string(&key(field), value)?;
                    if !["auto", "ask", "read-only"].contains(&parsed.extends.as_str()) {
                        return Err(format!(
                            "{} is auto, ask or read-only, not {}",
                            key(field),
                            parsed.extends
                        ));
                    }
                }
                "network" => parsed.network = Some(flag()?),
                "git" => {
                    parsed.git = Some(match string(&key(field), value)?.as_str() {
                        "write" => true,
                        "read-only" => false,
                        other => {
                            return Err(format!(
                                "{} is write or read-only, not {other}",
                                key(field)
                            ));
                        }
                    })
                }
                "writable" | "unix_sockets" => {
                    let invalid = || format!("{} must be a list of paths", key(field));
                    let paths = (value.as_array().ok_or_else(invalid)?.iter())
                        .map(|path| match path.as_str() {
                            Some(path) if !path.trim().is_empty() => Ok(path.to_owned()),
                            _ => Err(invalid()),
                        })
                        .collect::<Result<_, _>>()?;
                    match field.as_str() {
                        "writable" => parsed.writable = paths,
                        _ => parsed.unix_sockets = paths,
                    }
                }
                "reviewer" => {
                    parsed.reviewer = Some(match string(&key(field), value)?.as_str() {
                        "model" => Reviewer::Model,
                        "you" => Reviewer::You,
                        "none" => Reviewer::None,
                        other => {
                            return Err(format!(
                                "{} is model, you or none, not {other}",
                                key(field)
                            ));
                        }
                    })
                }
                _ => return Err(format!("unknown key {}", key(field))),
            }
        }
        // A read-only sandbox has nothing to write and no network or socket to
        // reach.
        if parsed.extends == "read-only"
            && (parsed.network == Some(true)
                || parsed.git == Some(true)
                || !parsed.writable.is_empty()
                || !parsed.unix_sockets.is_empty())
        {
            return Err(format!(
                "sonny.permissions.{name}: a read-only row writes nothing and reaches no network"
            ));
        }
        rows.insert(name.clone(), parsed);
    }
    Ok(rows)
}

/// `[sonny.sources.NAME]` tables, each with a `command` and maybe `every`.
fn sources(value: &toml::Value) -> Result<Vec<Source>, String> {
    let table = (value.as_table()).ok_or("sonny.sources must be a table of sources")?;
    let mut sources = Vec::new();
    for (name, source) in table {
        let key = |field: &str| format!("sonny.sources.{name}.{field}");
        if name.is_empty()
            || name.len() > 64
            || !(name.bytes()).all(|b| b.is_ascii_alphanumeric() || b"-_".contains(&b))
        {
            return Err(format!(
                "sonny.sources.{name}: a source's name is 1 to 64 of A-Z a-z 0-9 _ -"
            ));
        }
        if name == COMMONS_SOURCE {
            return Err(format!(
                "sonny.sources.{name}: the commons is a source of Sonny's own; name yours another way"
            ));
        }
        let fields =
            (source.as_table()).ok_or_else(|| format!("sonny.sources.{name} must be a table"))?;
        let mut command = None;
        let mut every = None;
        let mut digest = None;
        let mut urgent = None;
        for (field, value) in fields {
            match field.as_str() {
                "command" => command = Some(string(&key(field), value)?),
                "every" => every = Some(string(&key(field), value)?),
                "digest" => {
                    let value = string(&key(field), value)?;
                    span(&key(field), &value)?;
                    digest = Some(value);
                }
                "urgent" => {
                    if !value.is_table() {
                        return Err(format!(
                            "{} is an event pattern, as {{ from = [\"ana\"] }}",
                            key(field)
                        ));
                    }
                    urgent = Some(serde_json::to_value(value).map_err(|e| e.to_string())?);
                }
                _ => return Err(format!("unknown key {}", key(field))),
            }
        }
        sources.push(Source {
            name: name.clone(),
            command: command.ok_or_else(|| format!("{} is missing", key("command")))?,
            every,
            digest,
            urgent,
        });
    }
    Ok(sources)
}

fn string(key: &str, value: &toml::Value) -> Result<String, String> {
    match value.as_str() {
        Some("") => Err(format!("{key} must not be empty")),
        Some(value) => Ok(value.to_owned()),
        None => Err(format!("{key} must be a string")),
    }
}

/// Sets each `key` (`model`, or `table.key` such as `sonny.lead_effort`)
/// to its value in the file at `path`, line by line so the person's
/// comments and order stay: a key's line is replaced where it is, and a
/// new one goes at the end of its table, or of the top level for a bare
/// key. The file is written whole, and only if it still reads.
pub fn set(path: &Path, pairs: &[(&str, &str)]) -> Result<(), String> {
    let pairs: Vec<(&str, Option<toml::Value>)> = (pairs.iter())
        .map(|(k, v)| (*k, Some(toml::Value::String((*v).to_owned()))))
        .collect();
    write(path, &pairs)
}

/// As `set`, for any value, where a key with none has its line taken out,
/// so it is its default again; one the file does not name is left so.
pub fn write(path: &Path, pairs: &[(&str, Option<toml::Value>)]) -> Result<(), String> {
    read(path)?;
    let text = match std::fs::read_to_string(path) {
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => String::new(),
        read => read.map_err(|e| format!("config_unreadable: {}: {e}", path.display()))?,
    };
    let mut lines: Vec<String> = text.lines().map(str::to_owned).collect();
    for (key, value) in pairs {
        let (table, name) = key.split_once('.').unwrap_or(("", key));
        let at = |line: &str| {
            line.trim_start().strip_prefix('[').map(|h| {
                h.trim_start_matches('[')
                    .split(']')
                    .next()
                    .unwrap_or("")
                    .trim()
                    .to_owned()
            })
        };
        // The lines of `table`: after its header, or the top level's
        // before the first header, up to the next header.
        let header = lines.iter().position(|l| at(l).as_deref() == Some(table));
        let begin = match table {
            "" => 0,
            _ => match header {
                Some(header) => header + 1,
                None if value.is_none() => continue,
                None => {
                    if lines.last().is_some_and(|l| !l.trim().is_empty()) {
                        lines.push(String::new());
                    }
                    lines.push(format!("[{table}]"));
                    lines.len()
                }
            },
        };
        let end = (begin..lines.len())
            .find(|&i| at(&lines[i]).is_some())
            .unwrap_or(lines.len());
        let named = (begin..end).find(|&i| {
            lines[i]
                .split_once('=')
                .is_some_and(|(k, _)| k.trim().trim_matches('"') == name)
        });
        let Some(value) = value else {
            if let Some(i) = named {
                lines.remove(i);
                // A table left with nothing in it goes too.
                if let Some(header) = header
                    && lines[begin..end - 1].iter().all(|l| l.trim().is_empty())
                {
                    lines.drain(header..end - 1);
                }
            }
            continue;
        };
        let line = format!("{name} = {value}");
        match named {
            Some(i) => lines[i] = line,
            None => {
                let last = (begin..end)
                    .rev()
                    .find(|&i| !lines[i].trim().is_empty())
                    .map_or(begin, |i| i + 1);
                lines.insert(last, line);
                if table.is_empty() && last == 0 && lines.len() > 1 && !lines[1].trim().is_empty() {
                    lines.insert(1, String::new());
                }
            }
        }
    }
    while lines.last().is_some_and(|l| l.trim().is_empty()) {
        lines.pop();
    }
    let before = text;
    let mut text = lines.join("\n");
    text.push('\n');
    // A key written another way, as a dotted key or an inline table, is
    // not one the lines above find: it is refused, not left as it was.
    let table: toml::Table = text.parse().unwrap_or_default();
    for (key, value) in pairs {
        let (name, inner) = key
            .split_once('.')
            .map_or((*key, None), |(t, k)| (t, Some(k)));
        let found = match inner {
            Some(inner) => table.get(name).and_then(|t| t.get(inner)),
            None => table.get(name),
        };
        if found != value.as_ref() {
            return Err(format!(
                "config_unwritable: {}: {key} is written in a form only an editor can change",
                path.display()
            ));
        }
    }
    // Nothing changed, as a default taken out of a file without it.
    if text == before || (before.is_empty() && lines.is_empty()) {
        return Ok(());
    }
    replace(path, &text)
}

/// Replaces the table `name`, a dotted path such as
/// `sonny.mcp_servers.docs`, and its subtables, with `keys` at the end of
/// the file, each on a line of its own with tables inline; `None` removes
/// it. The file's other lines stay as they were.
pub fn set_table(path: &Path, name: &str, keys: Option<&toml::Table>) -> Result<(), String> {
    read(path)?;
    let text = match std::fs::read_to_string(path) {
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => String::new(),
        read => read.map_err(|e| format!("config_unreadable: {}: {e}", path.display()))?,
    };
    let header = |line: &str| {
        let line = line.trim();
        (line.starts_with('[') && !line.starts_with("[["))
            .then(|| line.trim_matches(['[', ']']).trim().to_owned())
    };
    let ours = |table: &str| table == name || table.starts_with(&format!("{name}."));
    let mut lines = Vec::new();
    let mut inside = false;
    for line in text.lines() {
        if let Some(table) = header(line) {
            inside = ours(&table);
        }
        if !inside {
            lines.push(line.to_owned());
        }
    }
    while lines.last().is_some_and(|l| l.trim().is_empty()) {
        lines.pop();
    }
    if let Some(keys) = keys {
        if !lines.is_empty() {
            lines.push(String::new());
        }
        lines.push(format!("[{name}]"));
        for (key, value) in keys {
            lines.push(format!("{key} = {value}"));
        }
    }
    let mut text = lines.join("\n");
    text.push('\n');
    replace(path, &text)
}

/// The file at `path` replaced whole by `text`, if `text` reads.
fn replace(path: &Path, text: &str) -> Result<(), String> {
    let unwritable =
        |e: &dyn std::fmt::Display| format!("config_unwritable: {}: {e}", path.display());
    parse(text).map_err(|reason| format!("config_invalid: {}: {reason}", path.display()))?;
    if let Some(dir) = path.parent() {
        use std::os::unix::fs::DirBuilderExt;
        // `~/.agent` is its owner's alone when a client makes it, as when
        // the daemon does; one that exists keeps its mode.
        std::fs::DirBuilder::new()
            .recursive(true)
            .mode(0o700)
            .create(dir)
            .map_err(|e| unwritable(&e))?;
    }
    let staged = path.with_extension(format!("toml.{}", std::process::id()));
    std::fs::write(&staged, text)
        .and_then(|()| std::fs::rename(&staged, path))
        .map_err(|e| {
            let _ = std::fs::remove_file(&staged);
            unwritable(&e)
        })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn daemon_limits_are_numbers_or_formulas_of_the_machine() {
        let gib = 1024.0 * 1024.0 * 1024.0;
        let at = |text| formula("x", text, 8.0, 16.0 * gib);
        assert_eq!(at("64"), Ok(64.0));
        assert_eq!(at("cpus * 64"), Ok(512.0));
        assert_eq!(at("cpus*2 + 1"), Ok(17.0));
        assert_eq!(at("(cpus + 2) * 2"), Ok(20.0));
        assert_eq!(at("memory / 16"), Ok(gib));
        assert_eq!(at("max(memory / 1024, 64 MiB)"), Ok(64.0 * 1024.0 * 1024.0));
        assert_eq!(at("min(cpus, 4)"), Ok(4.0));
        assert_eq!(at("1.5"), Ok(1.5));
        for bad in [
            "",
            "cpus *",
            "cores * 2",
            "1 / 0",
            "4 - cpus",
            "64 MB",
            "(cpus",
            "cpus 2",
        ] {
            assert!(at(bad).unwrap_err().contains("daemon.x"), "{bad}");
        }
        let config = parse("[daemon]\nprocesses = \"cpus * 8\"\nload = 0\nreaders = 3\n").unwrap();
        assert_eq!(config.daemon["processes"], "cpus * 8");
        assert_eq!(config.daemon["load"], "0");
        let flags = daemon_flags(&config.daemon).unwrap();
        let cpus = machine().0 as u64;
        assert!(flags.contains(&("--max-processes".into(), (cpus * 8).to_string())));
        assert!(flags.contains(&("--max-readers".into(), "3".into())));
        assert!(flags.contains(&("--max-load".into(), "0".into())));
        // A small positive limit stays one, not 0 for none.
        let small = parse("[daemon]\nload = 0.001\n").unwrap();
        let flags = daemon_flags(&small.daemon).unwrap();
        assert!(flags.contains(&("--max-load".into(), "0.001".into())));
        // With none given, only the gate's default is passed.
        let flags = daemon_flags(&BTreeMap::new()).unwrap();
        assert_eq!(flags, [("--max-load".to_owned(), (cpus * 2).to_string())]);
        assert_eq!(
            parse("[daemon]\nprocess = 4\n").unwrap_err(),
            "unknown key daemon.process"
        );
        assert!(
            parse("[daemon]\nprocesses = \"lots\"\n")
                .unwrap_err()
                .contains("daemon.processes")
        );
        assert!(
            parse("[daemon]\nprocesses = -1\n")
                .unwrap_err()
                .contains("daemon.processes")
        );
        // A duration goes to its flag as written, checked when read.
        let stall = parse("[daemon]\nstall_timeout = \"15m\"\n").unwrap();
        let flags = daemon_flags(&stall.daemon).unwrap();
        assert!(flags.contains(&("--stall-timeout".into(), "15m".into())));
        for bad in ["600", "1500ms", "0", "2d", "\"soon\""] {
            let text = match bad.starts_with('"') {
                true => bad.to_owned(),
                false if bad.chars().all(|c| c.is_ascii_digit()) => bad.to_owned(),
                false => format!("\"{bad}\""),
            };
            assert!(
                parse(&format!("[daemon]\nstall_timeout = {text}\n"))
                    .unwrap_err()
                    .contains("daemon.stall_timeout"),
                "{bad}"
            );
        }
        // A formula is checked for this machine: one below 0 here is refused
        // when the file is read, not when a daemon starts.
        let over = machine().0 as u64 + 1;
        assert!(
            parse(&format!("[daemon]\nprocesses = \"cpus - {over}\"\n"))
                .unwrap_err()
                .contains("daemon.processes")
        );
    }

    #[test]
    fn a_hold_is_the_daemon_s_unless_the_file_says_otherwise() {
        assert_eq!(Config::default().hold_ms, None);
        assert_eq!(parse("hold = \"2d\"").unwrap().hold_ms, Some(172_800_000));
        assert_eq!(parse("hold = \"0\"").unwrap().hold_ms, Some(0));
        for refused in ["\"366d\"", "\"30\"", "1"] {
            let error = parse(&format!("hold = {refused}")).unwrap_err();
            assert!(error.contains("hold"), "{refused}: {error}");
        }
    }

    #[test]
    fn an_attached_note_waits_a_day_unless_the_file_says_otherwise() {
        assert_eq!(Config::default().attach_wait(), Some(ATTACH_WAIT_MS));
        let hour = parse("attach_wait = \"1h\"").unwrap();
        assert_eq!(hour.attach_wait(), Some(3_600_000));
        let never = parse("attach_wait = \"0\"").unwrap();
        assert_eq!(never.attach_wait(), None, "0 waits as long as it takes");
        assert_eq!(
            parse("attach_wait = \"365d\"").unwrap().attach_wait(),
            Some(MAX_ATTACH_WAIT_MS)
        );
        for refused in ["\"366d\"", "\"30\"", "\"1w\"", "\"\"", "1"] {
            let error = parse(&format!("attach_wait = {refused}")).unwrap_err();
            assert!(error.contains("attach_wait"), "{refused}: {error}");
        }
    }

    #[test]
    fn the_file_sets_what_it_names_and_refuses_the_rest() {
        assert_eq!(parse("").unwrap(), Config::default());
        let config = parse(
            "# mine\n[judge]\nmodel = \"openai/luna\"\n\n[sonny]\nlead_effort = \"medium\"\ntask_effort = \"high\"\nunsandboxed = [\"mcpx\", \"docker compose\"]\n",
        )
        .unwrap();
        assert_eq!(config.judge.model.as_deref(), Some("openai/luna"));
        assert_eq!(config.judge.effort, None);
        assert_eq!(config.sonny.lead_effort.as_deref(), Some("medium"));
        assert_eq!(config.sonny.task_effort.as_deref(), Some("high"));
        assert_eq!(config.sonny.lead_profile, None);
        assert_eq!(config.sonny.unsandboxed, ["mcpx", "docker compose"]);
        assert_eq!(config.web_search, WebSearch::Live);
        let roles =
            parse("[sonny]\nlead_profile = \"my-lead\"\ntask_profile = \"worker\"").unwrap();
        assert_eq!(roles.sonny.lead_profile.as_deref(), Some("my-lead"));
        assert_eq!(roles.sonny.task_profile.as_deref(), Some("worker"));
        assert!(config.sonny.suggest);
        assert!(!parse("[sonny]\nsuggest = false").unwrap().sonny.suggest);
        assert!(config.sonny.commons);
        assert!(!parse("[sonny]\ncommons = false").unwrap().sonny.commons);
        let sonny = parse("[sonny]\ncommons_invites = 0\ncommons_invite_tokens = 500")
            .unwrap()
            .sonny;
        assert_eq!(
            (sonny.commons_invites, sonny.commons_invite_tokens),
            (0, 500)
        );
        assert_eq!(config.sonny.ask_wait_minutes, 60);
        let asks = parse("[sonny]\nask_wait_minutes = 0").unwrap().sonny;
        assert_eq!(asks.ask_wait_minutes, 0);
        assert!(parse("[sonny]\nask_wait_minutes = \"1h\"").is_err());
        assert_eq!(config.sonny.browser_confirm, BROWSER_CONFIRM);
        let browser =
            parse("[sonny]\nbrowser_confirm = [\"eval\", \"plugin:vault:credential.read\"]");
        assert_eq!(
            browser.unwrap().sonny.browser_confirm,
            ["eval", "plugin:vault:credential.read"]
        );
        assert!(
            parse("[sonny]\nbrowser_confirm = []")
                .unwrap()
                .sonny
                .browser_confirm
                .is_empty()
        );
        assert!(parse("[sonny]\nbrowser_confirm = [\"eval,upload\"]").is_err());
        assert!(parse("[sonny]\nbrowser_confirm = \"eval\"").is_err());
        assert!(parse("[sonny]\ncommons_invites = -1").is_err());
        assert!(parse("[sonny]\ncommons_invites = \"3\"").is_err());
        let sky = (config.sonny.sky_haikus, config.sonny.sky_haiku_window);
        assert_eq!(sky, (8, 10));
        let sky = (config.sonny.sky_haiku_margin, config.sonny.sky_haiku_hold);
        assert_eq!(sky, (50, 10));
        assert_eq!(
            parse("[sonny]\nsky_haikus = 0").unwrap().sonny.sky_haikus,
            0
        );
        assert!(parse("[sonny]\nsky_haiku_margin = 1.5").is_err());
        assert_eq!(config.sonny.task_depth, 2);
        assert_eq!(
            parse("[sonny]\ntask_depth = 0").unwrap().sonny.task_depth,
            0
        );
        assert!(parse("[sonny]\ntask_depth = -1").is_err());
        assert_eq!(
            parse("[sonny]\nsuggest = \"no\"").unwrap_err(),
            "sonny.suggest must be true or false"
        );
        let off = parse("web_search = \"disabled\"").unwrap();
        assert_eq!(off.web_search, WebSearch::Disabled);
        assert!(!config.hide_senders);
        assert_eq!(config.sonny.side_chat, SideChat::Answer);
        let fork = parse("[sonny]\nside_chat = \"fork\"").unwrap();
        assert_eq!(fork.sonny.side_chat, SideChat::Fork);
        assert_eq!(config.sonny.margin_opens, MarginOpens::Always);
        let never = parse("[sonny]\nmargin_opens = \"never\"").unwrap();
        assert_eq!(never.sonny.margin_opens, MarginOpens::Never);
        assert_eq!(config.sonny.agent_opens, AgentOpens::Tab);
        let beside = parse("[sonny]\nagent_opens = \"beside\"").unwrap();
        assert_eq!(beside.sonny.agent_opens, AgentOpens::Beside);
        assert!(parse("[sonny]\nagent_opens = \"window\"").is_err());
        let closed = parse("[sonny]\nmargin_opens = \"unless-closed\"").unwrap();
        assert_eq!(closed.sonny.margin_opens, MarginOpens::UnlessClosed);
        assert!(config.sonny.chime && !config.sonny.verbose);
        let quiet = parse("[sonny]\nchime = false\nverbose = true").unwrap();
        assert!(!quiet.sonny.chime && quiet.sonny.verbose);
        assert!(parse("senders = \"hidden\"").unwrap().hide_senders);
        assert!(!parse("senders = \"shown\"").unwrap().hide_senders);
        let fallbacks = parse("[fallbacks]\n\"a/big\" = \"a/other\"\n")
            .unwrap()
            .fallbacks;
        assert_eq!(fallbacks.get("a/big").map(String::as_str), Some("a/other"));
        assert!(parse("[fallbacks]\n\"a/big\" = 1\n").is_err());
        assert!(parse("fallbacks = \"a/other\"").is_err());
        for (text, error) in [
            (
                "web_search = \"cached\"",
                "web_search is live or disabled, not cached",
            ),
            ("web_search = false", "web_search must be a string"),
            ("senders = \"off\"", "senders is shown or hidden, not off"),
            (
                "[sonny]\nchime = \"no\"",
                "sonny.chime must be true or false",
            ),
            (
                "[sonny]\nside_chat = \"all\"",
                "sonny.side_chat is answer or fork, not all",
            ),
            (
                "[sonny]\nmargin_opens = \"sometimes\"",
                "sonny.margin_opens is always, unless-closed or never, not sometimes",
            ),
            ("colour = \"red\"", "unknown key colour"),
            ("model = 3", "model must be a string"),
            ("[web]\nsearch = true", "unknown table [web]"),
            ("[sonny]\neffort = \"low\"", "unknown key sonny.effort"),
            ("[judge]\neffort = 3", "judge.effort must be a string"),
            ("[judge]\nmodel = \"\"", "judge.model must not be empty"),
            (
                "[sonny]\nunsandboxed = \"mcpx\"",
                "sonny.unsandboxed must be a list of commands",
            ),
            (
                "[sonny]\nunsandboxed = [\" \"]",
                "sonny.unsandboxed must be a list of commands",
            ),
        ] {
            assert_eq!(parse(text).unwrap_err(), error, "{text}");
        }
        assert!(parse("[judge").is_err());
    }

    #[test]
    fn permissions_rows_extend_sonny_s_own() {
        let config = parse(
            "[sonny]\ndefault_permissions = \"strict\"\n\n[sonny.permissions.strict]\ndescription = \"Offline\"\nnetwork = false\ngit = \"read-only\"\nwritable = [\"~/.cargo\"]\nunix_sockets = [\"~/.docker/run\"]\n\n[sonny.permissions.night]\nextends = \"ask\"\nreviewer = \"none\"\n",
        )
        .unwrap();
        assert_eq!(config.sonny.default_permissions.as_deref(), Some("strict"));
        let strict = &config.sonny.permissions["strict"];
        assert_eq!(
            (strict.extends.as_str(), strict.network, strict.git),
            ("auto", Some(false), Some(false))
        );
        assert_eq!(strict.writable, ["~/.cargo"]);
        assert_eq!(strict.unix_sockets, ["~/.docker/run"]);
        assert_eq!(strict.reviewer, None);
        let night = &config.sonny.permissions["night"];
        assert_eq!(
            (night.extends.as_str(), night.reviewer),
            ("ask", Some(Reviewer::None))
        );
        assert_eq!(
            parse("[sonny]\ndefault_permissions = \"read-only\"")
                .unwrap()
                .sonny
                .default_permissions
                .as_deref(),
            Some("read-only")
        );
        for (text, error) in [
            (
                "[sonny]\ndefault_permissions = \"strict\"",
                "sonny.default_permissions is auto, ask, read-only, full or a [sonny.permissions] row, not strict",
            ),
            (
                "[sonny.permissions.full]\nnetwork = false",
                "sonny.permissions.full: full is a built-in row; name yours otherwise",
            ),
            (
                "[sonny.permissions.x]\nextends = \"full\"",
                "sonny.permissions.x.extends is auto, ask or read-only, not full",
            ),
            (
                "[sonny.permissions.x]\nreviewer = \"auto\"",
                "sonny.permissions.x.reviewer is model, you or none, not auto",
            ),
            (
                "[sonny.permissions.x]\ngit = true",
                "sonny.permissions.x.git must be a string",
            ),
            (
                "[sonny.permissions.x]\nextends = \"read-only\"\nnetwork = true",
                "sonny.permissions.x: a read-only row writes nothing and reaches no network",
            ),
            (
                "[sonny.permissions.x]\nsandbox = \"read-only\"",
                "unknown key sonny.permissions.x.sandbox",
            ),
            (
                "[sonny.permissions.\"a b\"]",
                "sonny.permissions.a b: a row's name is 1 to 32 of A-Z a-z 0-9 _ -",
            ),
        ] {
            assert_eq!(parse(text).unwrap_err(), error, "{text}");
        }
    }

    #[test]
    fn mcp_servers_take_codex_s_keys_and_one_transport() {
        let config = parse(
            "[sonny.mcp_servers.docs]\nurl = \"https://example.com/mcp\"\nbearer_token_env_var = \"DOCS\"\ndescription = \"The docs\"\n\n[sonny.mcp_servers.files]\ncommand = \"npx\"\nargs = [\"-y\", \"server\"]\nenv = { ROOT = \"/tmp\" }\nenabled_tools = [\"read\"]\ntool_timeout_sec = 30\n",
        )
        .unwrap();
        let docs = &config.sonny.mcp_servers["docs"];
        assert_eq!(docs.url.as_deref(), Some("https://example.com/mcp"));
        assert_eq!(docs.bearer_token_env_var.as_deref(), Some("DOCS"));
        assert!(docs.enabled);
        let files = &config.sonny.mcp_servers["files"];
        assert_eq!(files.args, ["-y", "server"]);
        assert_eq!(files.env["ROOT"], "/tmp");
        assert_eq!(
            files.enabled_tools.as_deref(),
            Some(&["read".to_owned()][..])
        );
        assert_eq!(files.tool_timeout_sec, Some(30.0));
        assert_eq!(files.approval_mode("read"), ApprovalMode::Auto);
        let modes = parse(
            "[sonny.mcp_servers.chat]\ncommand = \"chat\"\ndefault_tools_approval_mode = \"writes\"\n\n[sonny.mcp_servers.chat.tools.post]\napproval_mode = \"prompt\"\n",
        )
        .unwrap();
        let chat = &modes.sonny.mcp_servers["chat"];
        assert_eq!(
            (chat.approval_mode("post"), chat.approval_mode("search")),
            (ApprovalMode::Prompt, ApprovalMode::Writes)
        );
        // Codex's rule: a read-only hint wins over a destructive one.
        let hints = |read: bool, destructive: bool| serde_json::json!({"readOnlyHint": read, "destructiveHint": destructive});
        let asks = |mode: ApprovalMode| {
            [hints(false, false), hints(false, true), hints(true, true)]
                .map(|annotations| mode.asks(&annotations))
        };
        assert_eq!(asks(ApprovalMode::Auto), [false, true, false]);
        assert_eq!(asks(ApprovalMode::Writes), [true, true, false]);
        assert_eq!(asks(ApprovalMode::Prompt), [true, true, true]);
        assert_eq!(asks(ApprovalMode::Approve), [false, false, false]);
        assert!(!ApprovalMode::Auto.asks(&serde_json::Value::Null));
        for (text, error) in [
            (
                "[sonny.mcp_servers.x]\nenabled = true",
                "sonny.mcp_servers.x takes a command or a url",
            ),
            (
                "[sonny.mcp_servers.x]\ncommand = \"a\"\nurl = \"https://b\"",
                "sonny.mcp_servers.x takes a command or a url",
            ),
            (
                "[sonny.mcp_servers.x]\ncommand = \"a\"\nbearer_token_env_var = \"T\"",
                "sonny.mcp_servers.x.bearer_token_env_var goes with url",
            ),
            (
                "[sonny.mcp_servers.x]\nurl = \"ftp://b\"",
                "sonny.mcp_servers.x.url must be http or https",
            ),
            (
                "[sonny.mcp_servers.x]\ncommand = \"a\"\nargs = \"b\"",
                "sonny.mcp_servers.x.args must be a list of strings",
            ),
            (
                "[sonny.mcp_servers.x]\ncommand = \"a\"\ncolour = 1",
                "unknown key sonny.mcp_servers.x.colour",
            ),
            (
                "[sonny.mcp_servers.x]\ncommand = \"a\"\ndefault_tools_approval_mode = \"never\"",
                "sonny.mcp_servers.x.default_tools_approval_mode is auto, writes, prompt or approve",
            ),
            (
                "[sonny.mcp_servers.x]\ncommand = \"a\"\n[sonny.mcp_servers.x.tools.t]\nmode = \"auto\"",
                "unknown key sonny.mcp_servers.x.tools.t.mode",
            ),
            (
                "[sonny.mcp_servers.\"a b\"]\ncommand = \"a\"",
                "sonny.mcp_servers.a b: a name is 1 to 64 letters, digits, _ or -",
            ),
        ] {
            assert_eq!(parse(text).unwrap_err(), error, "{text}");
        }
    }

    #[test]
    fn shell_excludes_are_codex_s_and_off_by_default() {
        assert!(
            Config::default()
                .shell_environment_policy
                .excludes()
                .is_empty()
        );
        let config = parse("[shell_environment_policy]\nexclude = [\"AWS_*\"]\n").unwrap();
        assert_eq!(config.shell_environment_policy.excludes(), ["AWS_*"]);
        let config = parse(
            "[shell_environment_policy]\nignore_default_excludes = false\nexclude = [\"AWS_*\"]\n",
        )
        .unwrap();
        assert_eq!(
            config.shell_environment_policy.excludes(),
            ["*KEY*", "*SECRET*", "*TOKEN*", "AWS_*"]
        );
        for (text, error) in [
            (
                "[shell_environment_policy]\nexclude = \"AWS_*\"",
                "shell_environment_policy.exclude must be a list of names",
            ),
            (
                "[shell_environment_policy]\nexclude = [\" \"]",
                "shell_environment_policy.exclude must be a list of names",
            ),
            (
                "[shell_environment_policy]\nignore_default_excludes = \"no\"",
                "shell_environment_policy.ignore_default_excludes must be true or false",
            ),
            (
                "[shell_environment_policy]\ninherit = \"core\"",
                "unknown key shell_environment_policy.inherit",
            ),
        ] {
            assert_eq!(parse(text).unwrap_err(), error, "{text}");
        }
    }

    #[test]
    fn a_span_is_a_count_and_a_unit_or_nothing() {
        assert_eq!(span("k", "0"), Ok(0));
        assert_eq!(span("k", "0m"), Ok(0));
        assert_eq!(span("k", "30s"), Ok(30));
        assert_eq!(span("k", "5m"), Ok(300));
        assert_eq!(span("k", "2h"), Ok(7200));
        for bad in [
            "",
            "5",
            "m",
            "5 m",
            "1d",
            "-1m",
            "+1m",
            "5秒",
            "99999999999999999h",
        ] {
            assert!(span("k", bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn sources_are_named_commands() {
        let config = parse(
            "[sonny]\nlead_effort = \"low\"\n\n[sonny.sources.slack]\ncommand = \"~/bin/slack-poll\"\nevery = \"5m\"\ndigest = \"15m\"\n\n[sonny.sources.mail]\ncommand = \"mail-poll\"\n",
        )
        .unwrap();
        assert_eq!(config.sonny.lead_effort.as_deref(), Some("low"));
        let digests: Vec<Option<&str>> = (config.sonny.sources.iter())
            .map(|s| s.digest.as_deref())
            .collect();
        assert_eq!(digests, [None, Some("15m")]);
        let urgent = parse(
            "[sonny.sources.slack]\ncommand = \"x\"\nurgent = { from = [\"boss\"], text = [{ \"prefix\" = \"[prod]\" }] }\n",
        )
        .unwrap();
        assert_eq!(
            urgent.sonny.sources[0].urgent,
            Some(serde_json::json!({"from": ["boss"], "text": [{"prefix": "[prod]"}]}))
        );
        assert_eq!(config.sonny.sources[0].urgent, None);
        let named: Vec<(&str, &str, Option<&str>)> = (config.sonny.sources.iter())
            .map(|s| (s.name.as_str(), s.command.as_str(), s.every.as_deref()))
            .collect();
        assert_eq!(
            named,
            [
                ("mail", "mail-poll", None),
                ("slack", "~/bin/slack-poll", Some("5m"))
            ]
        );
        for (text, error) in [
            (
                "[sonny.sources.slack]\nevery = \"1m\"",
                "sonny.sources.slack.command is missing",
            ),
            (
                "[sonny.sources.slack]\ncommand = \"x\"\ndigest = \"1d\"",
                "sonny.sources.slack.digest is a number and s, m or h, as 30s or 5m, not \"1d\"",
            ),
            (
                "[sonny.sources.slack]\ncommand = \"x\"\nurgent = \"always\"",
                "sonny.sources.slack.urgent is an event pattern, as { from = [\"ana\"] }",
            ),
            (
                "[sonny.sources.commons]\ncommand = \"x\"",
                "sonny.sources.commons: the commons is a source of Sonny's own; name yours another way",
            ),
            (
                "[sonny]\ncommons_digest = \"an hour\"",
                "sonny.commons_digest is a number and s, m or h, as 30s or 5m, not \"an hour\"",
            ),
            (
                "[sonny.sources.slack]\ncommand = \"x\"\nargs = \"y\"",
                "unknown key sonny.sources.slack.args",
            ),
            (
                "[sonny.sources.\"a b\"]\ncommand = \"x\"",
                "sonny.sources.a b: a source's name is 1 to 64 of A-Z a-z 0-9 _ -",
            ),
            (
                "[sonny]\nsources = 3",
                "sonny.sources must be a table of sources",
            ),
        ] {
            assert_eq!(parse(text).unwrap_err(), error, "{text}");
        }
    }

    #[test]
    fn setting_keys_keeps_the_rest_of_the_file() {
        let dir = std::env::temp_dir().join(format!("agent-config-set-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let file = dir.join("config.toml");
        set(&file, &[("sonny.lead_effort", "medium"), ("model", "a/b")]).unwrap();
        assert_eq!(
            std::fs::read_to_string(&file).unwrap(),
            "model = \"a/b\"\n\n[sonny]\nlead_effort = \"medium\"\n"
        );
        std::fs::write(
            &file,
            "# mine\nmodel = \"a/b\"\n\n[judge]\neffort = \"low\" # cheap\n\n[sonny]\nlead_effort = \"low\"\n",
        )
        .unwrap();
        set(
            &file,
            &[
                ("model", "c/d"),
                ("provider", "bedrock"),
                ("judge.model", "e/f"),
                ("sonny.task_effort", "high"),
            ],
        )
        .unwrap();
        assert_eq!(
            std::fs::read_to_string(&file).unwrap(),
            "# mine\nmodel = \"c/d\"\nprovider = \"bedrock\"\n\n[judge]\neffort = \"low\" # cheap\nmodel = \"e/f\"\n\n[sonny]\nlead_effort = \"low\"\ntask_effort = \"high\"\n"
        );
        let config = read(&file).unwrap();
        assert_eq!(config.provider.as_deref(), Some("bedrock"));
        assert_eq!(config.judge.model.as_deref(), Some("e/f"));
        // A key the reader does not know is refused, and the file kept.
        assert!(
            set(&file, &[("colour", "red")])
                .unwrap_err()
                .contains("unknown key colour")
        );
        assert_eq!(read(&file).unwrap(), config);
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn a_key_written_with_no_value_is_its_default_again() {
        let dir = std::env::temp_dir().join(format!("agent-config-unset-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let file = dir.join("config.toml");
        // Nothing to take out leaves no file and no table behind.
        write(&file, &[("judge.model", None)]).unwrap();
        assert!(!file.exists());
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(
            &file,
            "# mine\nmodel = \"a/b\"\n\n[judge]\neffort = \"low\"\n\n[sonny]\nlead_effort = \"low\" # cheap\nside_chat = \"fork\"\n",
        )
        .unwrap();
        write(
            &file,
            &[
                ("sonny.lead_effort", None),
                ("sonny.task_effort", Some("high".into())),
                ("sonny.chime", Some(false.into())),
                ("model", None),
                ("judge.effort", None),
            ],
        )
        .unwrap();
        assert_eq!(
            std::fs::read_to_string(&file).unwrap(),
            "# mine\n\n[sonny]\nside_chat = \"fork\"\ntask_effort = \"high\"\nchime = false\n"
        );
        // A key the lines cannot find is refused, the file kept.
        for text in ["sonny.chime = false\n", "sonny = { chime = false }\n"] {
            std::fs::write(&file, text).unwrap();
            for value in [None, Some(true.into())] {
                let error = write(&file, &[("sonny.chime", value)]).unwrap_err();
                assert!(error.starts_with("config_"), "{text}: {error}");
                assert_eq!(std::fs::read_to_string(&file).unwrap(), text);
            }
        }
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn a_table_is_replaced_whole_and_the_rest_kept() {
        let dir = std::env::temp_dir().join(format!("agent-config-table-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let file = dir.join("config.toml");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(
            &file,
            "# mine\n[sonny]\nlead_effort = \"low\"\n\n[sonny.mcp_servers.docs]\nurl = \"https://a\"\n\n[sonny.mcp_servers.docs.http_headers]\nX = \"1\"\n\n[judge]\neffort = \"low\"\n",
        )
        .unwrap();
        let mut keys = toml::Table::new();
        keys.insert("command".into(), "npx".into());
        keys.insert(
            "args".into(),
            toml::Value::Array(vec!["-y".into(), "x".into()]),
        );
        keys.insert(
            "env".into(),
            toml::Value::Table(toml::Table::from_iter([("K".to_owned(), "v".into())])),
        );
        set_table(&file, "sonny.mcp_servers.files", Some(&keys)).unwrap();
        set_table(&file, "sonny.mcp_servers.docs", None).unwrap();
        let text = std::fs::read_to_string(&file).unwrap();
        assert!(text.starts_with("# mine\n[sonny]\nlead_effort = \"low\"\n\n[judge]\neffort = \"low\"\n\n[sonny.mcp_servers.files]\n"), "{text}");
        let config = read(&file).unwrap();
        assert!(!config.sonny.mcp_servers.contains_key("docs"));
        let files = &config.sonny.mcp_servers["files"];
        assert_eq!(
            (files.command.as_deref(), &files.args[..]),
            (Some("npx"), &["-y".to_owned(), "x".to_owned()][..])
        );
        assert_eq!(files.env["K"], "v");
        assert_eq!(config.judge.effort.as_deref(), Some("low"));
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn a_missing_file_is_the_defaults() {
        let dir = std::env::temp_dir().join(format!("agent-config-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        assert_eq!(read(&dir.join("config.toml")).unwrap(), Config::default());
        std::fs::create_dir_all(dir.join("config.toml")).unwrap();
        let error = read(&dir.join("config.toml")).unwrap_err();
        assert!(error.starts_with("config_"), "{error}");
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
