//! `/permissions`: what the lead and the agents it makes may do in their
//! sandboxes, and who approves anything more, as Codex's `/permissions`
//! rows pair a sandbox with a reviewer. Four rows are Sonny's own; the
//! person's `[sonny.permissions.NAME]` rows follow them. Agents the lead
//! makes keep the row it had when it made them, until the person picks
//! one: it overrides what every one was made with.
use agent_client::approver::AUTO_UNGATED;
use agent_client::config::{Permissions, Reviewer};
use serde_json::{Map, Value, json};

/// One `/permissions` row.
#[derive(Clone, Debug, PartialEq)]
pub struct Mode {
    /// Its id, as the config and the footer name it.
    pub name: &'static str,
    /// What the list calls it.
    pub label: String,
    pub about: String,
    pub sandbox: Sandbox,
    /// Who approves a call that leaves the sandbox.
    pub reviewer: Reviewer,
}

/// Where an agent's commands, writes and edits run.
#[derive(Clone, Debug, PartialEq)]
pub enum Sandbox {
    /// Its folder, temp and `writable` written; the network reached when
    /// `network`, and its repositories committed to when `git`, their
    /// hooks and config kept read-only.
    Workspace {
        network: bool,
        git: bool,
        writable: Vec<String>,
    },
    /// Everything read, nothing written, no network.
    ReadOnly,
    /// None, as under Codex's full access.
    Full,
}

/// The row a new lead starts in when the config names none.
const DEFAULT: &str = "auto";

/// Sonny's own row `name`: the first three sandboxed, as Codex's
/// `workspace-write` with the network and the folder's repository
/// loosened (the person's pick, 10-28), and Codex's `read-only`.
fn builtin(name: &str) -> Option<Mode> {
    let workspace = Sandbox::Workspace {
        network: true,
        git: true,
        writable: Vec::new(),
    };
    let (name, label, about, sandbox, reviewer) = match name {
        "auto" => (
            "auto",
            "Auto",
            "Works in its folder; a reviewer model approves anything more",
            workspace,
            Reviewer::Model,
        ),
        "ask" => (
            "ask",
            "Ask me",
            "Works in its folder; you approve anything more",
            workspace,
            Reviewer::You,
        ),
        "read-only" => (
            "read-only",
            "Read only",
            "Reads anything, changes nothing; you approve anything more",
            Sandbox::ReadOnly,
            Reviewer::You,
        ),
        "full" => (
            "full",
            "Full access",
            "No sandbox and no reviews; agents can do anything you can",
            Sandbox::Full,
            Reviewer::None,
        ),
        _ => return None,
    };
    Some(Mode {
        name,
        label: label.into(),
        about: about.into(),
        sandbox,
        reviewer,
    })
}

/// A row of the person's config: the row it extends, with what it changes.
fn own(name: &str, row: &Permissions) -> Option<Mode> {
    let mut mode = builtin(&row.extends)?;
    mode.name = intern(name);
    mode.about = (row.description.clone())
        .unwrap_or_else(|| format!("Like {}, as ~/.agent/config.toml changes it", mode.label));
    // Its name, written as the list writes Sonny's: a capital first.
    let mut chars = name.chars();
    mode.label = chars
        .next()
        .map(|first| first.to_uppercase().chain(chars).collect())
        .unwrap_or_default();
    if let Sandbox::Workspace {
        network,
        git,
        writable,
    } = &mut mode.sandbox
    {
        *network = row.network.unwrap_or(*network);
        *git = row.git.unwrap_or(*git);
        *writable = row.writable.iter().map(|folder| home(folder)).collect();
    }
    mode.reviewer = row.reviewer.unwrap_or(mode.reviewer);
    Some(mode)
}

/// `folder` with a leading `~` for the person's home.
fn home(folder: &str) -> String {
    match (folder.strip_prefix("~/"), std::env::var("HOME")) {
        (Some(rest), Ok(home)) => format!("{home}/{rest}"),
        _ if folder == "~" => std::env::var("HOME").unwrap_or_else(|_| folder.to_owned()),
        _ => folder.to_owned(),
    }
}

/// A row's name for as long as Sonny runs, as the built-in ones are: each
/// name kept once.
fn intern(name: &str) -> &'static str {
    static NAMES: std::sync::Mutex<Vec<&'static str>> = std::sync::Mutex::new(Vec::new());
    let mut names = NAMES
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    if let Some(kept) = names.iter().find(|kept| **kept == name) {
        return kept;
    }
    let kept: &'static str = Box::leak(name.to_owned().into_boxed_str());
    names.push(kept);
    kept
}

/// Every row `/permissions` lists: Sonny's four, then the config's; only
/// Sonny's when the config can't be read, which Sonny reports on start.
pub fn modes() -> Vec<Mode> {
    let config = agent_client::config::load().unwrap_or_default();
    modes_of(&config.sonny)
}

fn modes_of(sonny: &agent_client::config::Sonny) -> Vec<Mode> {
    let builtins = agent_client::config::PERMISSIONS
        .iter()
        .filter_map(|name| builtin(name));
    let own = (sonny.permissions.iter()).filter_map(|(name, row)| own(name, row));
    builtins.chain(own).collect()
}

/// The row `name`, of Sonny's or the config's.
pub fn find(name: &str) -> Option<Mode> {
    builtin(name).or_else(|| modes().into_iter().find(|mode| mode.name == name))
}

/// The row a new lead starts in: the config's `default_permissions`, else
/// `auto`.
pub fn default() -> &'static str {
    agent_client::config::load()
        .ok()
        .and_then(|config| config.sonny.default_permissions)
        .map_or(DEFAULT, |name| intern(&name))
}

/// Whether a reviewer model approves for the row `name`, so Sonny keeps
/// one serving.
pub fn reviews(name: Option<&str>) -> bool {
    name.and_then(find)
        .is_some_and(|mode| mode.reviewer == Reviewer::Model)
}

/// What the footer says of the row `name`: nothing for the row new leads
/// start in, read when Sonny started; its name in the list otherwise.
pub fn footer(name: &str) -> Option<String> {
    static START: std::sync::OnceLock<&'static str> = std::sync::OnceLock::new();
    if name == *START.get_or_init(default) {
        return None;
    }
    Some(match builtin(name) {
        Some(mode) => mode.label.to_lowercase(),
        None => name.to_owned(),
    })
}

/// Who approves for a bot, from its record's gates: `None` for a gate
/// Sonny did not set.
fn reviewer(record: &Value) -> Option<Reviewer> {
    let gates = record["gates"].as_array().map(Vec::as_slice).unwrap_or(&[]);
    match gates {
        [] => Some(Reviewer::None),
        [gate] if gate["tag"] == "auto" => Some(Reviewer::Model),
        [gate] if gate["tag"] == "manual" => Some(Reviewer::You),
        _ => None,
    }
}

/// The lead's row, from its record's gates and sandbox; `None` for a gate
/// Sonny did not set. Of the rows that fit, the one that says most about
/// it: one that names its folders over one that leaves them out, and of
/// rows alike, the first. A bot no row fits, such as one made
/// before Sonny confined leads, or in a row since taken out of the config,
/// takes Sonny's row for whoever approves it, so its sandbox is set again;
/// one no one approves for, still confined, is left as no row of Sonny's.
pub fn mode(record: &Value) -> Option<&'static str> {
    mode_in(record, &modes())
}

/// [`mode`], of `modes`.
fn mode_in(record: &Value, modes: &[Mode]) -> Option<&'static str> {
    let reviewer = reviewer(record)?;
    let settings = &record["settings"];
    let sandbox = settings["sandbox"].as_str();
    let writable: Vec<&str> = (settings["writable"].as_array().into_iter().flatten())
        .filter_map(Value::as_str)
        .collect();
    let committing = writable.iter().any(|path| path.ends_with(".git"));
    // Whether its folder is in a repository, so a row that writes it
    // would have given it its git folder: looked for by name, as no
    // command needs to run for it.
    let repository = (record["workspace"].as_str()).is_some_and(|dir| {
        std::path::Path::new(dir)
            .ancestors()
            .any(|dir| dir.join(".git").exists())
    });
    let network = settings["network"].as_bool().unwrap_or(false);
    // Its own folders, past its repositories' git folders.
    let folders: Vec<&str> = (writable.iter().copied())
        .filter(|path| !path.ends_with(".git"))
        .collect();
    // How well a row fits: whether its folders there now are the bot's,
    // then how many there are.
    let fit = |mode: &Mode| -> Option<(bool, usize)> {
        if mode.reviewer != reviewer {
            return None;
        }
        match &mode.sandbox {
            Sandbox::Full => {
                (!matches!(sandbox, Some("workspace-write" | "read-only"))).then_some((true, 0))
            }
            Sandbox::ReadOnly => (sandbox == Some("read-only")).then_some((true, 0)),
            Sandbox::Workspace {
                network: reaches,
                git,
                writable: own,
            } => {
                // Every folder the bot writes is one of the row's, as
                // written or by its real path. A row folder that is not
                // there yet was left out, and one made since still fits,
                // so a row is never lost, and widened, because a cache
                // folder appeared.
                let there = real(own);
                let fits = sandbox == Some("workspace-write")
                    && network == *reaches
                    && (*git || !committing)
                    && (!repository || *git == committing)
                    && (folders.iter())
                        .all(|folder| own.iter().chain(&there).any(|mine| mine == folder));
                let exact = there
                    .iter()
                    .all(|folder| folders.contains(&folder.as_str()));
                fits.then_some((exact, there.len()))
            }
        }
    };
    let mut best: Option<((bool, usize), &'static str)> = None;
    for mode in modes {
        if let Some(score) = fit(mode)
            && best.is_none_or(|(most, _)| score > most)
        {
            best = Some((score, mode.name));
        }
    }
    if let Some((_, name)) = best {
        return Some(name);
    }
    match reviewer {
        Reviewer::Model => Some("auto"),
        Reviewer::You if sandbox == Some("read-only") => Some("read-only"),
        Reviewer::You => Some("ask"),
        Reviewer::None if matches!(sandbox, Some("workspace-write" | "read-only")) => None,
        Reviewer::None => Some("full"),
    }
}

/// A row to set again, for a bot whose gate is older than the one `gate`
/// makes: an `auto` one that lapses, so a doubted call is denied rather
/// than waiting for the person, or one that still holds reads back. A bot
/// another bot made is set again too, as the person's `/permissions` would.
pub fn refresh(record: &Value) -> Option<&'static str> {
    let gate = &record["gates"][0];
    let old = gate["expire_ms"].is_u64()
        || gate["tools"]
            .as_array()
            .is_some_and(|tools| tools.iter().any(|t| t == "read"));
    mode(record).filter(|_| old)
}

/// The `create` or `configure` fields for the row `name`: every tool the
/// bot has but those that touch only its own records or only read waits
/// for whoever approves, unless no one does. Reads run unjudged, as
/// Codex's sandboxes let reads through: what leaves the sandbox is a
/// write, a command or a call out. No gate is `create`'s no fields.
pub fn gate(name: &str, tools: &[String]) -> Map<String, Value> {
    let reviewer = find(name).map_or(Reviewer::None, |mode| mode.reviewer);
    let approve: Vec<&String> = tools
        .iter()
        .filter(|tool| !AUTO_UNGATED.contains(&tool.as_str()))
        .collect();
    let fields = match reviewer {
        // Nothing left to gate, such as a bot that only reads: a gate
        // needs a tool, so the bot is ungated.
        _ if approve.is_empty() => json!({"approve": []}),
        // No expiry: a call the reviewer doubts waits for the person here.
        Reviewer::Model => json!({"approve": approve, "approver": "auto"}),
        Reviewer::You => json!({"approve": approve, "approver": "manual"}),
        // No one: a call that leaves the sandbox is refused, and the agent
        // is told to do without it, as under Codex's `never`.
        Reviewer::None => json!({"approve": []}),
    };
    match fields {
        Value::Object(fields) => fields,
        _ => Map::new(),
    }
}

/// The sandbox fields, as `configure` takes them and `create`'s settings
/// hold them, for an agent in the row `name` working in `workspace`, with
/// the commands the person's config lets out of it (`[sonny]
/// unsandboxed`). A row's repositories are the folder's own and each
/// inside it, given as folders of their own so the daemon keeps their
/// hooks and config read-only; a row's other folders are those that are
/// there, by their real paths. A row no longer in the config is the
/// default's.
pub fn sandbox(name: &str, workspace: &std::path::Path) -> Map<String, Value> {
    sandbox_with(name, workspace, &unsandboxed())
}

/// [`sandbox`]'s fields, with `unsandboxed` the commands let out of it.
fn sandbox_with(
    name: &str,
    workspace: &std::path::Path,
    unsandboxed: &[String],
) -> Map<String, Value> {
    let mode = find(name)
        .or_else(|| find(default()))
        .or_else(|| builtin(DEFAULT));
    match mode {
        Some(mode) => fields(&mode, workspace, unsandboxed),
        None => confined(json!({"sandbox": "danger-full-access"})),
    }
}

/// The sandbox fields of `mode` in `workspace`.
fn fields(mode: &Mode, workspace: &std::path::Path, unsandboxed: &[String]) -> Map<String, Value> {
    let mut fields = match &mode.sandbox {
        Sandbox::Full => return confined(json!({"sandbox": "danger-full-access"})),
        Sandbox::ReadOnly => json!({"sandbox": "read-only"}),
        Sandbox::Workspace {
            network,
            git,
            writable: own,
        } => {
            let mut writable: Vec<String> = Vec::new();
            if *git {
                writable.extend(repository(workspace));
                writable.extend(repositories(workspace));
            }
            writable.extend(real(own));
            // As many as the daemon takes, the shallowest kept.
            writable.truncate(MAX_WRITABLE);
            while serde_json::to_string(&writable).map_or(0, |json| json.len()) > MAX_WRITABLE_JSON
            {
                writable.pop();
            }
            json!({"sandbox": "workspace-write", "writable": writable, "network": network})
        }
    };
    if !unsandboxed.is_empty() {
        fields["unsandboxed"] = json!(unsandboxed);
    }
    confined(fields)
}

fn confined(fields: Value) -> Map<String, Value> {
    match fields {
        Value::Object(fields) => fields,
        _ => Map::new(),
    }
}

/// `folders` as the daemon keeps them, by their real paths, leaving out
/// those that aren't there.
fn real(folders: &[String]) -> Vec<String> {
    (folders.iter())
        .filter_map(|folder| std::fs::canonicalize(folder).ok())
        .filter(|path| path.is_dir())
        .filter_map(|path| path.to_str().map(str::to_owned))
        .collect()
}

/// The commands the person's config lets agents run outside their
/// sandboxes; none when it can't be read, which Sonny reports on start.
pub fn unsandboxed() -> Vec<String> {
    agent_client::config::load()
        .map(|config| config.sonny.unsandboxed)
        .unwrap_or_default()
}

/// The command prefix the person may let out of every sandbox when they
/// allow `call` for good: the program and, when it reads as one, its
/// subcommand, as Codex suggests a prefix rule (`cargo test`, not the whole
/// line). Only a plain command offers one: a pipe, a list, a redirection
/// or an expansion runs more than its first words say.
pub fn always(call: &Value) -> Option<String> {
    if call["name"] != "shell" || call["arguments"]["sandbox_permissions"] != "require_escalated" {
        return None;
    }
    let command = call["arguments"]["command"].as_str()?.trim();
    if command.is_empty()
        || command.contains([
            '|', '&', ';', '<', '>', '$', '`', '(', ')', '\n', '\'', '"', '\\', '*', '?',
        ])
    {
        return None;
    }
    let words: Vec<&str> = command.split_whitespace().collect();
    let program = *words.first()?;
    if program.contains('=') {
        return None;
    }
    let subcommand = (words.get(1)).filter(|word| {
        word.chars().next().is_some_and(|c| c.is_ascii_lowercase())
            && word
                .chars()
                .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-')
    });
    Some(match subcommand {
        Some(subcommand) => format!("{program} {subcommand}"),
        None => program.to_owned(),
    })
}

/// Adds `prefix` to `[sonny] unsandboxed` in the person's config, so every
/// agent's sandbox lets it out from the next time Sonny sets it.
pub fn allow_always(prefix: &str) -> Result<(), String> {
    let path = agent_client::config::path().ok_or("no config file to keep it in")?;
    let mut commands = agent_client::config::read(&path)?.sonny.unsandboxed;
    if commands.iter().any(|command| command == prefix) {
        return Ok(());
    }
    commands.push(prefix.to_owned());
    let value = toml::Value::Array(commands.into_iter().map(toml::Value::String).collect());
    agent_client::config::write(&path, &[("sonny.unsandboxed", Some(value))])
}

/// The git folder of the repository `dir` is in, which its linked
/// worktrees share, by its real path as the daemon keeps it. Only one
/// named as the daemon knows a git folder, `.git` or `NAME.git`: in one
/// named otherwise, such as a separate git folder, its hooks and config
/// would not be kept, so a commit there asks instead.
pub fn repository(dir: &std::path::Path) -> Option<String> {
    let found = crate::task::git(
        dir,
        &["rev-parse", "--path-format=absolute", "--git-common-dir"],
    )
    .ok()
    .filter(|path| !path.is_empty())?;
    let real = std::fs::canonicalize(found).ok()?;
    real.to_str()
        .filter(|path| path.ends_with(".git"))
        .map(str::to_owned)
}

/// The most writable folders a bot takes, as the daemon bounds them.
const MAX_WRITABLE: usize = 64;
const MAX_WRITABLE_JSON: usize = 32 << 10;
/// How many folders deep below an agent's folder repositories are looked
/// for, and how many folders are looked in at most, so a start never walks
/// a whole tree: a workspace's repositories sit a few folders down, not
/// past its build folders.
const NESTED_DEPTH: usize = 6;
const NESTED_FOLDERS: usize = 10_000;

/// The git folders of the repositories inside `dir`, below its own,
/// shallowest first, by their real paths: each `.git` folder found, not
/// looked inside, nor below it, nor in hidden folders, which hold caches
/// and tools' state rather than the person's repositories, nor through
/// links. A `.git` file, a linked checkout's, names a folder elsewhere,
/// whose repository is not this folder's to keep.
fn repositories(dir: &std::path::Path) -> Vec<String> {
    let mut found = Vec::new();
    let mut folders = std::collections::VecDeque::from([(dir.to_owned(), 0)]);
    let mut looked = 0;
    while let Some((folder, depth)) = folders.pop_front() {
        looked += 1;
        if looked > NESTED_FOLDERS {
            break;
        }
        let Ok(entries) = std::fs::read_dir(&folder) else {
            continue;
        };
        let mut below = Vec::new();
        for entry in entries.flatten() {
            if !entry.file_type().is_ok_and(|kind| kind.is_dir()) {
                continue;
            }
            let name = entry.file_name();
            if name == ".git" {
                if depth > 0 {
                    found.extend(std::fs::canonicalize(entry.path()).ok());
                }
                below.clear();
                break;
            }
            if depth + 1 < NESTED_DEPTH && !name.to_string_lossy().starts_with('.') {
                below.push(entry.path());
            }
        }
        below.sort();
        folders.extend(below.into_iter().map(|path| (path, depth + 1)));
    }
    found
        .into_iter()
        .filter_map(|path| path.to_str().map(str::to_owned))
        .collect()
}

/// Whether `record`'s sandbox is the one `fields` give it: its mode, its
/// writable folders, which a moved folder leaves stale, and its network.
pub fn sandboxed_as(record: &Value, fields: &Map<String, Value>) -> bool {
    let fields = Value::Object(fields.clone());
    let settings = &record["settings"];
    let list = |value: &Value, key: &str| value[key].as_array().cloned().unwrap_or_default();
    settings["sandbox"] == fields["sandbox"]
        && list(settings, "writable") == list(&fields, "writable")
        && list(settings, "unsandboxed") == list(&fields, "unsandboxed")
        && settings["network"].as_bool().unwrap_or(false)
            == fields["network"].as_bool().unwrap_or(false)
}

/// Whether the bot `record` describes may run Sonny's tool `tool`: any
/// tool not in `served::GATED`, and one of those, which act outside the
/// agent's sandbox, only once a gate has judged it or for an agent with
/// no sandbox. A confined row no one reviews refuses them, as the daemon
/// refuses its other ways out of the sandbox.
pub fn may_run(record: &Value, tool: &str) -> bool {
    let confined = matches!(
        record["settings"]["sandbox"].as_str(),
        Some("workspace-write" | "read-only")
    );
    let judged = (record["gates"].as_array().into_iter().flatten())
        .any(|gate| (gate["tools"].as_array().into_iter().flatten()).any(|gated| gated == tool));
    !crate::served::GATED.contains(&tool) || judged || !confined
}

/// What a bot's gate may hold back: its own `tools`, and of the tools its
/// client declared, those Sonny gates (`served::GATED`).
pub fn gateable(tools: &[String], declared: &Value) -> Vec<String> {
    let mut gateable = tools.to_vec();
    for tool in declared.as_array().into_iter().flatten() {
        if let Some(name) = tool["name"].as_str()
            && crate::served::GATED.contains(&name)
            && !gateable.iter().any(|have| have == name)
        {
            gateable.push(name.to_owned());
        }
    }
    gateable
}

/// The agents `lead` made, and those they made, from a `bots` listing:
/// by the maker's identity, so a reused name's children are strangers.
pub fn made_by<'a>(bots: &'a [Value], lead: &str) -> Vec<&'a Value> {
    let Some(lead) = bots
        .iter()
        .find(|bot| bot["name"] == lead)
        .and_then(|bot| bot["bot_id"].as_i64())
    else {
        return Vec::new();
    };
    let mut makers = vec![lead];
    let mut made: Vec<&Value> = Vec::new();
    while let Some(maker) = makers.pop() {
        for bot in bots.iter().filter(|bot| bot["created_by_id"] == maker) {
            if let Some(id) = bot["bot_id"].as_i64()
                && id != lead
                && !made.iter().any(|seen| seen["bot_id"] == id)
            {
                made.push(bot);
                makers.push(id);
            }
        }
    }
    made
}

/// The `configure` fields that give a bot `mode`, with the sandbox the
/// mode gives an agent in its folder, or `None` when its gate is already
/// the one `gate` makes and its sandbox the mode's: the same approver over the same tools,
/// with no expiry. `record` is the bot as `inspect` gives it, with the
/// tools its client declared.
pub fn regate(record: &Value, mode: &str) -> Option<Map<String, Value>> {
    let tools: Vec<String> = record["tools"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(Value::as_str)
        .map(str::to_owned)
        .collect();
    let fields = gate(mode, &gateable(&tools, &record["declared"]));
    let wanted = match fields.get("approver") {
        Some(tag) => vec![json!({"tag": tag, "tools": fields["approve"]})],
        None => Vec::new(),
    };
    let gates = record["gates"].as_array().map(Vec::as_slice).unwrap_or(&[]);
    let sorted = |tools: &Value| {
        let mut tools: Vec<String> = tools
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(Value::as_str)
            .map(str::to_owned)
            .collect();
        tools.sort();
        tools
    };
    let has = gates.len() == wanted.len()
        && gates.iter().zip(&wanted).all(|(gate, want)| {
            gate["tag"] == want["tag"]
                && sorted(&gate["tools"]) == sorted(&want["tools"])
                && gate["expire_ms"].is_null()
        });
    let mut fields = fields;
    let mut confined = true;
    if let Some(workspace) = record["workspace"].as_str() {
        let sandbox = sandbox(mode, std::path::Path::new(workspace));
        confined = sandboxed_as(record, &sandbox);
        fields.extend(sandbox);
    }
    (!has || !confined).then_some(fields)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_agent_is_confined_but_in_full_mode() {
        let dir = std::env::temp_dir().join(format!("sonny-sandbox-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let dir = std::fs::canonicalize(&dir).unwrap();
        crate::task::git(&dir, &["init", "-q"]).unwrap();
        let git = dir.join(".git").display().to_string();
        let confined = json!({"sandbox": "workspace-write", "writable": [git], "network": true});
        for mode in ["auto", "ask"] {
            let fields = sandbox_with(mode, &dir, &[]);
            assert_eq!(Value::Object(fields.clone()), confined);
            assert!(sandboxed_as(&json!({"settings": fields}), &fields));
        }
        // Read only writes nothing, its repository included, and reaches
        // no network; the commands let out of every sandbox leave it too.
        let reading = sandbox_with("read-only", &dir, &["mcpx".to_owned()]);
        assert_eq!(
            Value::Object(reading.clone()),
            json!({"sandbox": "read-only", "unsandboxed": ["mcpx"]})
        );
        assert!(sandboxed_as(&json!({"settings": reading}), &reading));
        assert_eq!(Value::Object(sandbox_with("auto", &dir, &[])), confined);
        let full = sandbox_with("full", &dir, &[]);
        assert_eq!(
            Value::Object(full.clone()),
            json!({"sandbox": "danger-full-access"})
        );
        // A task the person moves to `full` leaves its sandbox with the
        // gate, and one already there is left alone.
        let task = json!({"workspace": dir, "gates": [],
            "settings": {"sandbox": "workspace-write", "writable": [git], "network": true}});
        assert_eq!(
            regate(&task, "full").unwrap()["sandbox"],
            "danger-full-access"
        );
        let freed = json!({"workspace": dir, "gates": [],
            "settings": {"sandbox": "danger-full-access", "writable": [], "network": false}});
        assert_eq!(regate(&freed, "full"), None);
        let unconfined = json!({"settings": {"sandbox": "danger-full-access", "writable": [], "network": false}});
        assert!(sandboxed_as(&unconfined, &full));
        // A lead made before leads were confined is confined again, and
        // one whose repository moved is given it where it is now.
        let auto = sandbox_with("auto", &dir, &[]);
        assert!(!sandboxed_as(&json!({"settings": {}}), &auto));
        let moved = json!({"settings": {"sandbox": "workspace-write", "writable": ["/gone/.git"],
            "network": true}});
        assert!(!sandboxed_as(&moved, &auto));
        // The commands the config lets out of the sandbox go with it, and
        // a lead made before they were named takes them.
        let mcpx = sandbox_with("auto", &dir, &["mcpx".to_owned()]);
        assert_eq!(mcpx["unsandboxed"], json!(["mcpx"]));
        assert!(!sandboxed_as(&json!({"settings": auto}), &mcpx));
        assert!(sandboxed_as(&json!({"settings": mcpx}), &mcpx));
        assert!(
            sandbox_with("full", &dir, &["mcpx".to_owned()])
                .get("unsandboxed")
                .is_none()
        );
        // Outside a repository, only its own folder; with a git folder
        // named otherwise, whose hooks the daemon wouldn't know to keep,
        // commits ask.
        let plain = std::env::temp_dir().join(format!("sonny-plain-{}", std::process::id()));
        std::fs::create_dir_all(&plain).unwrap();
        assert_eq!(sandbox_with("ask", &plain, &[])["writable"], json!([]));
        let apart = dir.join("apart");
        let tree = dir.join("tree");
        std::fs::create_dir_all(&tree).unwrap();
        crate::task::git(
            &tree,
            &["init", "-q", "--separate-git-dir", apart.to_str().unwrap()],
        )
        .unwrap();
        assert_eq!(sandbox_with("ask", &tree, &[])["writable"], json!([]));
        std::fs::remove_dir_all(&dir).unwrap();
        std::fs::remove_dir_all(&plain).unwrap();
    }

    #[test]
    fn the_repositories_inside_an_agent_s_folder_are_given_as_folders_of_their_own() {
        let dir = std::env::temp_dir().join(format!("sonny-nested-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let dir = std::fs::canonicalize(&dir).unwrap();
        // A workspace that isn't git, holding repositories a few folders
        // down; one inside another's tree, one in a hidden folder, one too
        // deep, and a linked checkout's `.git` file are left to their own.
        let repos = [
            "src/api",
            "src/web/ui",
            "a/b/c/d/e/f",
            ".cache/x",
            "src/api/vendor/y",
        ];
        for repo in repos {
            std::fs::create_dir_all(dir.join(repo)).unwrap();
            crate::task::git(&dir.join(repo), &["init", "-q"]).unwrap();
        }
        std::fs::create_dir_all(dir.join("src/linked")).unwrap();
        std::fs::write(dir.join("src/linked/.git"), "gitdir: /elsewhere\n").unwrap();
        let git = |repo: &str| dir.join(repo).join(".git").display().to_string();
        let fields = sandbox_with("auto", &dir, &[]);
        assert_eq!(
            fields["writable"],
            json!([git("src/api"), git("src/web/ui")])
        );
        // A repository's own folder comes first, and what is inside its
        // tree is its own.
        let fields = sandbox_with("auto", &dir.join("src/api"), &[]);
        assert_eq!(fields["writable"], json!([git("src/api")]));
        // A repository cloned since makes an agent's sandbox stale, so it
        // is given its folder too.
        let before = sandbox_with("auto", &dir, &[]);
        std::fs::create_dir_all(dir.join("src/new")).unwrap();
        crate::task::git(&dir.join("src/new"), &["init", "-q"]).unwrap();
        assert!(!sandboxed_as(
            &json!({"settings": before}),
            &sandbox_with("auto", &dir, &[])
        ));
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn modes_are_read_back_from_the_gates_they_make() {
        let tools: Vec<String> = ["shell", "read", "wait", "history"]
            .map(String::from)
            .to_vec();
        let plain = std::env::temp_dir().join(format!("sonny-rows-{}", std::process::id()));
        std::fs::create_dir_all(&plain).unwrap();
        for name in agent_client::config::PERMISSIONS {
            let fields = gate(name, &tools);
            let gates = match fields.get("approver") {
                Some(tag) => json!([{"tag": tag, "tools": fields["approve"]}]),
                None => json!([]),
            };
            let settings = sandbox_with(name, &plain, &[]);
            let record = json!({"gates": gates, "settings": settings, "workspace": plain});
            assert_eq!(mode(&record), Some(name));
        }
        std::fs::remove_dir_all(&plain).unwrap();
        // Reads run unjudged in every row, as Codex's sandboxes read.
        assert_eq!(gate("auto", &tools)["approve"], json!(["shell"]));
        assert_eq!(gate("ask", &tools)["approve"], json!(["shell"]));
        assert_eq!(gate("read-only", &tools)["approver"], json!("manual"));
        // Made before rows had sandboxes of their own: by who approves.
        let manual = json!([{"tag": "manual", "tools": ["shell"]}]);
        assert_eq!(mode(&json!({"gates": manual})), Some("ask"));
        // A bot with nothing left to gate is ungated, not refused.
        let reader: Vec<String> = ["read", "wait"].map(String::from).to_vec();
        assert_eq!(Value::Object(gate("auto", &reader)), json!({"approve": []}));
        assert!(gate("auto", &tools).get("approve_expire_ms").is_none());
        // An auto gate that still lapses is set again, a made bot's too.
        let lapsing = json!({"gates": [{"tag": "auto", "tools": ["shell"], "expire_ms": 45000}]});
        assert_eq!(refresh(&lapsing), Some("auto"));
        let mut made = lapsing.clone();
        made["created_by"] = json!("lead");
        assert_eq!(refresh(&made), Some("auto"));
        assert_eq!(
            refresh(&json!({"gates": [{"tag": "auto", "tools": ["shell"]}]})),
            None
        );
        // So is one that still judges reads.
        let reads = json!({"gates": [{"tag": "auto", "tools": ["shell", "read"]}]});
        assert_eq!(refresh(&reads), Some("auto"));
        let asked = json!({"gates": [{"tag": "manual", "tools": ["shell", "read"]}]});
        assert_eq!(refresh(&asked), Some("ask"));
        assert_eq!(mode(&json!({"gates": null})), Some("full"));
        assert_eq!(mode(&json!({"gates": [{"tag": "ci"}]})), None);
    }

    #[test]
    fn the_person_s_own_rows_follow_sonny_s_and_are_read_back() {
        let dir = std::env::temp_dir().join(format!("sonny-own-rows-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("cache")).unwrap();
        let dir = std::fs::canonicalize(&dir).unwrap();
        crate::task::git(&dir, &["init", "-q"]).unwrap();
        let cache = dir.join("cache").display().to_string();
        let later = dir.join("later").display().to_string();
        let row = |extends: &str| Permissions {
            description: None,
            extends: extends.into(),
            network: None,
            git: None,
            writable: Vec::new(),
            reviewer: None,
        };
        let sonny = agent_client::config::Sonny {
            permissions: std::collections::BTreeMap::from([
                (
                    "strict".to_owned(),
                    Permissions {
                        description: Some("Offline, commits ask".into()),
                        network: Some(false),
                        git: Some(false),
                        writable: vec![later.clone()],
                        ..row("auto")
                    },
                ),
                (
                    "night".to_owned(),
                    Permissions {
                        reviewer: Some(Reviewer::None),
                        writable: vec![cache.clone(), "/not/there".into()],
                        ..row("ask")
                    },
                ),
            ]),
            ..Default::default()
        };
        let modes = modes_of(&sonny);
        let names: Vec<&str> = modes.iter().map(|mode| mode.name).collect();
        assert_eq!(
            names,
            ["auto", "ask", "read-only", "full", "night", "strict"]
        );
        assert_eq!(modes[5].about, "Offline, commits ask");
        assert_eq!(
            modes[4].about,
            "Like Ask me, as ~/.agent/config.toml changes it"
        );
        let reviewing = json!([{"tag": "auto", "tools": ["shell"]}]);
        let record = |mode: &Mode, gates: &Value| json!({"gates": gates, "workspace": dir, "settings": fields(mode, &dir, &[])});
        // Offline, with its repository read-only, so commits ask.
        let strict = record(&modes[5], &reviewing);
        assert_eq!(
            strict["settings"],
            json!({"sandbox": "workspace-write", "writable": [], "network": false})
        );
        assert_eq!(mode_in(&strict, &modes), Some("strict"));
        // Its folder made since, it is still strict, not widened to auto.
        std::fs::create_dir(&later).unwrap();
        assert_eq!(mode_in(&strict, &modes), Some("strict"));
        // Auto in the same folder is still auto.
        assert_eq!(
            mode_in(&record(&modes[0], &reviewing), &modes),
            Some("auto")
        );
        // No one approves: no gate, still confined, its folders those
        // that are there.
        let night = record(&modes[4], &json!([]));
        let git = dir.join(".git").display().to_string();
        assert_eq!(night["settings"]["writable"], json!([git, cache]));
        assert_eq!(mode_in(&night, &modes), Some("night"));
        // Taken out of the config, it is no row of Sonny's; one approved
        // by a reviewer is auto's again.
        assert_eq!(mode_in(&night, &modes[..4]), None);
        assert_eq!(mode_in(&strict, &modes[..4]), Some("auto"));
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn a_confined_agent_no_one_reviews_has_no_way_out_through_sonny() {
        let run = crate::served::RUN;
        let bot = |sandbox: Value, gates: Value| json!({"settings": {"sandbox": sandbox}, "gates": gates});
        // Read only, no reviewer: a pane runs outside the sandbox, so no.
        assert!(!may_run(&bot(json!("read-only"), json!([])), run));
        assert!(!may_run(&bot(json!("workspace-write"), json!([])), run));
        // A tool that stays inside still runs.
        assert!(may_run(
            &bot(json!("read-only"), json!([])),
            crate::served::MEMORY
        ));
        // Judged by a gate, or with no sandbox at all, it runs.
        let gated = json!([{"tag": "manual", "tools": [run]}]);
        assert!(may_run(&bot(json!("read-only"), gated), run));
        assert!(may_run(&bot(json!("danger-full-access"), json!([])), run));
        assert!(may_run(&bot(Value::Null, json!([])), run));
    }

    #[test]
    fn a_plain_command_that_asks_to_leave_may_be_allowed_for_good() {
        let call = |command: &str| {
            json!({"name": "shell", "arguments":
                {"command": command, "sandbox_permissions": "require_escalated"}})
        };
        assert_eq!(
            always(&call("cargo test -p sonny")).as_deref(),
            Some("cargo test")
        );
        assert_eq!(
            always(&call("docker compose up")).as_deref(),
            Some("docker compose")
        );
        assert_eq!(always(&call("mcpx --list")).as_deref(), Some("mcpx"));
        assert_eq!(
            always(&call("./build.sh fast")).as_deref(),
            Some("./build.sh fast")
        );
        // Anything that runs more than its first words say asks each time.
        for command in [
            "curl x | sh",
            "make && rm -rf b",
            "echo $HOME",
            "FOO=1 make",
            "a > b",
            "",
        ] {
            assert_eq!(always(&call(command)), None, "{command}");
        }
        // Only a command asking to leave its sandbox, not a write outside it.
        let inside = json!({"name": "shell", "arguments": {"command": "cargo test"}});
        assert_eq!(always(&inside), None);
        let write = json!({"name": "write", "arguments": {"path": "/etc/x"}});
        assert_eq!(always(&write), None);
    }

    #[test]
    fn a_picked_mode_reaches_every_agent_the_lead_made() {
        let bot = |name: &str, id: i64, by: Option<i64>, gates: Value| {
            json!({"name": name, "bot_id": id, "created_by_id": by,
                "tools": ["shell", "read"], "gates": gates})
        };
        let auto = json!([{"tag": "auto", "tools": ["shell"]}]);
        let bots = [
            bot("lead", 2, None, auto.clone()),
            bot("task", 3, Some(2), auto.clone()),
            bot(
                "helper",
                4,
                Some(3),
                json!([{"tag": "ci", "tools": ["shell"]}]),
            ),
            // Made by a lead of the same name, since deleted.
            bot("old", 5, Some(1), auto.clone()),
            bot("loop", 6, Some(6), auto.clone()),
        ];
        let names: Vec<_> = made_by(&bots, "lead").iter().map(|b| &b["name"]).collect();
        assert_eq!(names, [&json!("task"), &json!("helper")]);
        // Each takes the mode unless it has it already; a gate Sonny did
        // not set is replaced too.
        assert_eq!(regate(&bots[1], "full"), Some(gate("full", &[])));
        assert_eq!(regate(&bots[1], "auto"), None);
        assert!(regate(&bots[2], "auto").is_some());
        // Nothing to gate: already as `full` leaves it.
        let reader = json!({"name": "r", "tools": ["read"], "gates": []});
        assert_eq!(regate(&reader, "auto"), None);
        assert_eq!(regate(&reader, "ask"), None);
        // The same tag over fewer tools, or one that lapses, is set again.
        let partial = json!({"tools": ["shell", "read", "write"],
            "gates": [{"tag": "manual", "tools": ["shell"]}]});
        assert!(regate(&partial, "ask").is_some());
        let lapsing = json!({"tools": ["shell", "read"],
            "gates": [{"tag": "auto", "tools": ["shell"], "expire_ms": 45000}]});
        assert!(regate(&lapsing, "auto").is_some());
        // Of the tools its client declared, those that can drop work, run
        // something outside the sandbox or send words elsewhere wait for
        // the verdict too.
        let panes = json!({"tools": ["shell", "read"], "declared": crate::declared::tools(true),
            "gates": [{"tag": "auto", "tools": ["shell"]}]});
        assert_eq!(
            regate(&panes, "auto").unwrap()["approve"],
            json!([
                "shell",
                "start_task",
                "remove_task",
                "new_project",
                "subscriptions",
                "run_in_pane",
                "triggers",
                "goal"
            ])
        );
    }
}
