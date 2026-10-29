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
//!
//! [judge]
//! model = "openai/gpt-6-luna"   # who judges for the approver and the lookout
//! effort = "low"                # a model judge's effort
//!
//! [sonny]
//! lead_effort = "medium"        # a new lead's effort
//! task_effort = "high"          # each new task's
//! lead_profile = "my-lead"      # the role a new lead starts in, not lead
//! task_profile = "worker"       # what every other agent is told, not task
//! side_chat = "fork"            # "answer", the default, calls no tool
//! chime = false                 # rings when work ends or waits, the default
//! verbose = true                # every tool call drawn, not the clean view
//! suggest = false               # no guess at the next message, on by default
//!
//! [sonny.mcp_servers.linear]    # an MCP server Sonny's agents reach
//! url = "https://mcp.linear.app/mcp"
//! bearer_token_env_var = "LINEAR_TOKEN"
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
    pub judge: Judge,
    pub sonny: Sonny,
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

/// The judge `agent approver` and Sonny's lookout ask.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Judge {
    /// `provider/model` or `openai-decisions/MODEL`; unset, the judged
    /// bot's own model.
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
    /// The MCP servers Sonny's agents reach, by name.
    pub mcp_servers: BTreeMap<String, McpServer>,
    pub side_chat: SideChat,
    /// Whether finished and waiting work rings. Notifications show either way.
    pub chime: bool,
    /// Whether every tool call is drawn, or only the bot's words and the
    /// calls that move the work on (the clean view, the default).
    pub verbose: bool,
    /// Whether a copy of the agent guesses the person's next message after
    /// a turn they started, shown in the empty composer for Tab to take.
    pub suggest: bool,
}

impl Default for Sonny {
    fn default() -> Self {
        Sonny {
            lead_effort: None,
            task_effort: None,
            lead_profile: None,
            task_profile: None,
            mcp_servers: BTreeMap::new(),
            side_chat: SideChat::default(),
            suggest: true,
            chime: true,
            verbose: false,
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
}

const MCP_KEYS: [&str; 13] = [
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
        let keys: &[&str] = match name.as_str() {
            "judge" => &["model", "effort"],
            "sonny" => &[
                "lead_effort",
                "task_effort",
                "lead_profile",
                "task_profile",
                "side_chat",
                "chime",
                "verbose",
                "suggest",
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
            if !keys.contains(&key.as_str()) {
                return Err(format!("unknown key {name}.{key}"));
            }
            if let Some(switch) = match key.as_str() {
                "chime" => Some(&mut config.sonny.chime),
                "suggest" => Some(&mut config.sonny.suggest),
                "verbose" => Some(&mut config.sonny.verbose),
                _ => None,
            } {
                *switch = value
                    .as_bool()
                    .ok_or_else(|| format!("{name}.{key} must be true or false"))?;
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
            // The daemon judges models and levels when the agents are made.
            *match (name.as_str(), key.as_str()) {
                ("judge", "model") => &mut config.judge.model,
                ("judge", "effort") => &mut config.judge.effort,
                ("sonny", "lead_effort") => &mut config.sonny.lead_effort,
                ("sonny", "lead_profile") => &mut config.sonny.lead_profile,
                ("sonny", "task_profile") => &mut config.sonny.task_profile,
                _ => &mut config.sonny.task_effort,
            } = Some(value);
        }
    }
    Ok(config)
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
        std::fs::create_dir_all(dir).map_err(|e| unwritable(&e))?;
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
    fn the_file_sets_what_it_names_and_refuses_the_rest() {
        assert_eq!(parse("").unwrap(), Config::default());
        let config = parse(
            "# mine\n[judge]\nmodel = \"openai/luna\"\n\n[sonny]\nlead_effort = \"medium\"\ntask_effort = \"high\"\n",
        )
        .unwrap();
        assert_eq!(config.judge.model.as_deref(), Some("openai/luna"));
        assert_eq!(config.judge.effort, None);
        assert_eq!(config.sonny.lead_effort.as_deref(), Some("medium"));
        assert_eq!(config.sonny.task_effort.as_deref(), Some("high"));
        assert_eq!(config.sonny.lead_profile, None);
        assert_eq!(config.web_search, WebSearch::Live);
        let roles =
            parse("[sonny]\nlead_profile = \"my-lead\"\ntask_profile = \"worker\"").unwrap();
        assert_eq!(roles.sonny.lead_profile.as_deref(), Some("my-lead"));
        assert_eq!(roles.sonny.task_profile.as_deref(), Some("worker"));
        assert!(config.sonny.suggest);
        assert!(!parse("[sonny]\nsuggest = false").unwrap().sonny.suggest);
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
        assert!(config.sonny.chime && !config.sonny.verbose);
        let quiet = parse("[sonny]\nchime = false\nverbose = true").unwrap();
        assert!(!quiet.sonny.chime && quiet.sonny.verbose);
        assert!(parse("senders = \"hidden\"").unwrap().hide_senders);
        assert!(!parse("senders = \"shown\"").unwrap().hide_senders);
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
            ("colour = \"red\"", "unknown key colour"),
            ("model = 3", "model must be a string"),
            ("[web]\nsearch = true", "unknown table [web]"),
            ("[sonny]\neffort = \"low\"", "unknown key sonny.effort"),
            ("[judge]\neffort = 3", "judge.effort must be a string"),
            ("[judge]\nmodel = \"\"", "judge.model must not be empty"),
        ] {
            assert_eq!(parse(text).unwrap_err(), error, "{text}");
        }
        assert!(parse("[judge").is_err());
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
                "[sonny.mcp_servers.\"a b\"]\ncommand = \"a\"",
                "sonny.mcp_servers.a b: a name is 1 to 64 letters, digits, _ or -",
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
