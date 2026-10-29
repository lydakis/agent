//! The folder's coordinator: opened with no `--bot`, Sonny talks to the
//! project's lead, `<project>.lead`, the same bot whichever Sonny opens
//! that folder. A folder that is not
//! a project yet becomes one: its lead, then its `.agents/project.toml`.
//! The `new_project` tool Sonny answers makes one for an agent the same
//! way.
use agent_client::Client;
use serde_json::{Value, json};
use std::path::{Path, PathBuf};

/// The lead's role as Sonny ships it, which Sonny writes to the person's
/// `~/.agents/agents/lead.md` to edit; used when neither the folder nor
/// the person has a `lead.md`.
pub const LEAD: &str = include_str!("../agents/lead.md");

/// What Sonny ships for every agent it starts other than a lead (tasks,
/// a trigger's agents and the ones opened with `--new`), written to the
/// person's `~/.agents/agents/task.md` to edit, as [`LEAD`] is.
pub const TASK: &str = include_str!("../agents/task.md");

/// `text`, an agent's composed instructions, with what every agent but a
/// lead is told after it: the body of the folder's or the person's
/// `task.md`, else Sonny's.
pub fn as_task(dir: &Path, text: &str) -> Result<String, String> {
    let task = match agent_client::policy::profile(dir, "task")
        .map_err(|e| format!("{}: {e}", e.code()))?
    {
        Some(task) => task,
        None => agent_client::policy::Profile::parse("task", None, TASK),
    };
    Ok(match task.body.as_str() {
        "" => text.to_owned(),
        body => format!("{text}\n\n{}", agent_client::policy::with_cli(body)),
    })
}

pub struct Lead {
    pub project: String,
    pub bot: String,
    /// Made just now, for this folder.
    pub new: bool,
    /// The folder it is brought back to: it was moved elsewhere, and the
    /// next message it is sent moves it here.
    pub moving: Option<PathBuf>,
    /// Made just now below another project's folder: that project's lead.
    pub above: Option<String>,
}

/// The folder's own name reduced to name characters, the name of a
/// project with no file.
fn default_name(dir: &Path) -> Result<String, String> {
    let info = agent_client::project::read(dir)?;
    Ok(info["name"].as_str().unwrap_or("project").to_owned())
}

/// The lead's role: the folder's or the person's `lead.md`, else Sonny's.
fn role(dir: &Path) -> Result<agent_client::policy::Profile, String> {
    Ok(
        match agent_client::policy::profile(dir, "lead").map_err(|e| e.to_string())? {
            Some(role) => role,
            None => agent_client::policy::Profile::parse("lead", None, LEAD),
        },
    )
}

/// A lead's instructions as Sonny composes them for `dir` now: its role,
/// the folder's rules, skills and memory, then the project's task rule.
/// Rex is the rex-windows skill's to
/// explain, so they read the same in any client.
pub fn instructions(dir: &Path) -> Result<String, String> {
    compose(dir).map(|(text, _)| text)
}

/// The instructions and the names of the skills they list.
fn compose(dir: &Path) -> Result<(String, Vec<String>), String> {
    let (composed, skills) = folder(dir, &role(dir)?)?;
    let info = agent_client::project::read(dir)?;
    let rule = agent_client::project::rule(&info);
    Ok((format!("{composed}\n\n{rule}"), skills))
}

/// The role with the folder's rules, skills and memory and who it works
/// for, before the project's task rule, and the names of the skills they
/// list.
fn folder(
    dir: &Path,
    role: &agent_client::policy::Profile,
) -> Result<(String, Vec<String>), String> {
    let memory = memory(dir)?;
    let composed =
        agent_client::policy::instructions(dir, Some(role), &memory).map_err(|e| e.to_string())?;
    let skills = composed
        .skills
        .into_iter()
        .map(|skill| skill.name)
        .collect();
    Ok((composed.text, skills))
}

/// The memory a lead of `dir` reads: its own project's, by the name its
/// lead takes, even before the folder has a project file. Looked up from
/// the folder, one made below another project, outside git, would take
/// that project's.
fn memory(dir: &Path) -> Result<Vec<PathBuf>, String> {
    Ok(agent_client::policy::project_memory_indexes(&default_name(
        dir,
    )?))
}

/// What an agent a trigger starts in `dir` is told: the folder's rules,
/// skills and memory, in no role, then what every task is told.
pub fn plain(dir: &Path) -> Result<String, String> {
    let memory = agent_client::policy::memory_indexes(dir).map_err(|e| e.to_string())?;
    let composed = agent_client::policy::instructions(dir, None, &memory)
        .map_err(|e| format!("{}: {e}", e.code()))?;
    as_task(dir, &composed.text)
}

/// What attaching does with a lead's instructions.
#[derive(Debug, PartialEq)]
pub enum Instructions {
    /// They are what the folder composes today.
    Current,
    /// A client composed what they hold, and the folder has changed since:
    /// these replace them.
    Stale(String),
    /// They were set some other way, or before Sonny saw this lead, and
    /// are kept until the person asks for the folder's. `missing` names
    /// the folder's skills they do not list, such as the layout skill
    /// Sonny installed after they were set.
    Kept { missing: Vec<String> },
}

impl Instructions {
    /// What Sonny tells the person about instructions it keeps: the
    /// command first, so a narrow pane still shows what to do.
    pub fn kept_notice(missing: &[String]) -> String {
        let why = "its instructions were set outside its folder's files";
        match missing {
            [] => "the lead's instructions were set outside its folder's files; \
                   /instructions takes the folder's"
                .into(),
            [skill] => format!("/instructions gives the lead the {skill} skill: {why}"),
            skills => format!(
                "/instructions gives the lead the {} skills: {why}",
                skills.join(", ")
            ),
        }
    }
}

/// Whether a lead's instructions may follow the folder: only when Sonny
/// wrote what they hold (or saw a client compose it), so instructions the
/// person set some other way are never overwritten.
pub fn review(dir: &Path, bot: &Value, lead: &str, new: bool) -> Result<Instructions, String> {
    let (composed, skills) = compose(dir)?;
    let current = bot["instructions"].as_str().unwrap_or_default();
    Ok(if current == composed {
        wrote(lead, current);
        Instructions::Current
    } else if new || written(lead).is_some_and(|key| key == fingerprint(current)) {
        Instructions::Stale(composed)
    } else {
        // The composed text lists each skill as "- NAME: summary (path)".
        let missing = skills
            .into_iter()
            .filter(|name| !current.contains(&format!("- {name}:")))
            .collect();
        Instructions::Kept { missing }
    })
}

/// What names a lead across daemons: bot ids count from one in each
/// store, so the daemon's socket is part of the name.
pub fn lead(socket: &Path, bot: &Value) -> String {
    let socket = std::fs::canonicalize(socket).unwrap_or_else(|_| socket.to_owned());
    let id = bot["bot_id"].as_i64().unwrap_or_default();
    format!("{}-{id}", fingerprint(&socket.to_string_lossy()))
}

/// Where Sonny keeps a fingerprint of the instructions it last saw `lead`
/// take from its folder: one file a lead, so two Sonnys never write over
/// each other's.
fn written_path(lead: &str) -> Option<PathBuf> {
    std::env::var_os("HOME").map(|home| {
        PathBuf::from(home)
            .join(".agent/sonny/instructions")
            .join(lead)
    })
}

fn written(lead: &str) -> Option<String> {
    let text = std::fs::read_to_string(written_path(lead)?).ok()?;
    Some(text.trim().to_owned())
}

/// Note that `lead` now holds `text`, composed from its folder.
pub fn wrote(lead: &str, text: &str) {
    let Some(path) = written_path(lead) else {
        return;
    };
    let key = fingerprint(text);
    if written(lead).as_deref() == Some(key.as_str()) {
        return;
    }
    if let Some(dir) = path.parent() {
        let _ = std::fs::create_dir_all(dir);
    }
    let temp = path.with_extension(format!("{}.tmp", std::process::id()));
    if std::fs::write(&temp, format!("{key}\n")).is_ok() {
        let _ = std::fs::rename(&temp, &path);
    }
}

/// FNV-1a, as hex: stable across runs and builds.
pub fn fingerprint(text: &str) -> String {
    let hash = text.bytes().fold(0xcbf2_9ce4_8422_2325u64, |hash, byte| {
        (hash ^ u64::from(byte)).wrapping_mul(0x0100_0000_01b3)
    });
    format!("{hash:016x}")
}

/// Keep a lead's model and effort in its project's file, when the folder
/// has one, so the file and the lead agree on what it runs. Only those two
/// lines change; the rest of the file is the person's.
pub fn keep_model(dir: &Path, model: &str, effort: Option<&str>) -> Result<(), String> {
    let path = dir.join(agent_client::project::FILE);
    let text = match std::fs::read_to_string(&path) {
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(error) => return Err(format!("{}: {error}", path.display())),
        Ok(text) => text,
    };
    let edited = with_model(&text, model, effort);
    if edited == text {
        return Ok(());
    }
    // The person's file is replaced only by one that says what was asked.
    let read: toml::Table = edited.parse().map_err(|_| {
        format!(
            "{} could not be edited; set the model there",
            path.display()
        )
    })?;
    let said = |key: &str| read.get(key).and_then(toml::Value::as_str);
    if said("model") != Some(model)
        || said("effort") != effort
        || edited.len() as u64 > agent_client::project::LIMIT
    {
        return Err(format!(
            "{} could not be edited; set the model there",
            path.display()
        ));
    }
    let temp = path.with_extension(format!("toml.{}.tmp", std::process::id()));
    std::fs::write(&temp, &edited)
        .and_then(|()| std::fs::rename(&temp, &path))
        .map_err(|error| {
            let _ = std::fs::remove_file(&temp);
            format!("{}: {error}", path.display())
        })?;
    // Read back as the app reads it, so a bad edit is said, not kept quiet.
    agent_client::project::read(dir).map(|_| ())
}

/// `text` with its top-level `model` and `effort` set (or `effort`
/// removed), each in place when there, else added after `model`.
fn with_model(text: &str, model: &str, effort: Option<&str>) -> String {
    // A JSON string is a TOML basic string.
    let quote = |s: &str| Value::from(s).to_string();
    // A bare key, or one in quotes.
    fn key(line: &str) -> Option<&str> {
        let line = line.trim_start();
        let name = line.split(['=', ' ', '\t']).next().unwrap_or_default();
        line[name.len()..]
            .trim_start()
            .starts_with('=')
            .then(|| name.trim_matches(['"', '\'']))
    }
    let mut lines: Vec<String> = Vec::new();
    let (mut at, mut placed) = (None, false);
    for line in text.lines() {
        match key(line) {
            Some("model") => {
                at = Some(lines.len());
                lines.push(format!("model = {}", quote(model)));
            }
            Some("effort") => {
                placed = true;
                if let Some(level) = effort {
                    lines.push(format!("effort = {}", quote(level)));
                }
            }
            _ => lines.push(line.to_owned()),
        }
    }
    let at = at.unwrap_or_else(|| {
        lines.push(format!("model = {}", quote(model)));
        lines.len() - 1
    });
    if let Some(level) = effort.filter(|_| !placed) {
        lines.insert(at + 1, format!("effort = {}", quote(level)));
    }
    let mut out = lines.join("\n");
    out.push('\n');
    out
}

/// Whether `dir` is in a project named `name`.
fn same_name(dir: &Path, name: &str) -> bool {
    if !dir.is_dir() {
        return false;
    }
    let root = dir
        .canonicalize()
        .map_err(|e| e.to_string())
        .and_then(|dir| folder_of(&dir))
        .unwrap_or_else(|_| dir.to_owned());
    default_name(&root).is_ok_and(|found| found == name)
}

/// The lead for `dir`, made first when the folder is not a project yet.
pub async fn open(client: &Client, dir: &Path, model: Option<&str>) -> Result<Lead, String> {
    let dir = folder_of(&dir.canonicalize().map_err(|e| e.to_string())?)?;
    let project = default_name(&dir)?;
    let bot = format!("{project}.lead");
    if let Some(found) = find(client, &bot).await? {
        let there = found["workspace"].as_str().unwrap_or("");
        let moving = (there != dir.to_str().unwrap_or("")).then_some(dir);
        // Another folder of the same name is its own project: its lead
        // stays there. One moved anywhere else is still this folder's, and
        // comes back with the next message, history and all.
        if moving.is_some() && same_name(Path::new(there), &project) {
            return Err(format!(
                "project_exists: {bot} is the lead of {there}, a project of the same name; \
                 name this one in .agents/project.toml or pass --bot"
            ));
        }
        return Ok(Lead {
            project,
            bot,
            new: false,
            moving,
            above: None,
        });
    }
    create(client, &dir, model).await?;
    Ok(Lead {
        project,
        bot,
        new: true,
        moving: None,
        above: above(&dir),
    })
}

/// The folder whose lead Sonny opens in `dir` (canonical): in a git
/// repository, the nearest project in it, else the repository's top, so a
/// subfolder such as `src` reaches the repository's lead, and a worktree
/// its main checkout's; anywhere else `dir` itself, so every folder can
/// have a lead, whatever project holds the folder above it. Either way,
/// one with no project file of its own is the project's above it when
/// that project keeps its subfolders. A task's folder under `~/.agent` is
/// never a project's.
fn folder_of(dir: &Path) -> Result<PathBuf, String> {
    folder_in(dir, std::env::var_os("HOME").as_deref().map(Path::new))
}

fn folder_in(dir: &Path, home: Option<&Path>) -> Result<PathBuf, String> {
    let home = home.map(|home| home.canonicalize().unwrap_or_else(|_| home.to_owned()));
    let folder = match agent_client::policy::checkout(dir) {
        Some(top) => agent_client::policy::project_root(dir)
            .map_err(|e| e.to_string())?
            .map_or(top, |(_, root)| root),
        None => dir.to_owned(),
    };
    let folder = claimed(&folder, home.as_deref())?.unwrap_or(folder);
    if let Some(home) = home
        && let Some(task) = folder
            .strip_prefix(home.join(".agent/worktrees"))
            .ok()
            .and_then(|rest| rest.components().next())
    {
        let task = task.as_os_str().to_string_lossy();
        return Err(format!(
            "task_folder: {} is in task {task}'s folder; run sonny in its project's \
             folder, or sonny --bot {task} for the task",
            dir.display()
        ));
    }
    Ok(folder)
}

/// The project folder above `folder` that keeps the folders below it as
/// its own (`subfolders = "project"`), when `folder` has no project file:
/// the nearest project above decides, looking no higher than the home
/// folder.
fn claimed(folder: &Path, home: Option<&Path>) -> Result<Option<PathBuf>, String> {
    let file = agent_client::project::FILE;
    if folder.join(file).is_file() {
        return Ok(None);
    }
    for at in folder.ancestors().skip(1) {
        if home == Some(at) {
            break;
        }
        if at.join(file).is_file() {
            let info = agent_client::project::read(at)?;
            return Ok((info["subfolders"] == "project").then(|| at.to_owned()));
        }
    }
    Ok(None)
}

/// The lead of the project holding the folder above `dir`, when there is one.
fn above(dir: &Path) -> Option<String> {
    let (name, _) = agent_client::policy::project_root(dir.parent()?).ok()??;
    Some(format!("{name}.lead"))
}

async fn find(client: &Client, bot: &str) -> Result<Option<Value>, String> {
    let page = client
        .request("bots", json!({"name": bot, "limit": 1}))
        .await
        .map_err(|e| e.to_string())?;
    Ok(page["bots"].get(0).filter(|b| b["name"] == bot).cloned())
}

/// The model of the agent made last, from any client: what the person
/// chose most recently when Sonny has not run before.
async fn newest(client: &Client) -> Result<Option<String>, String> {
    let mut after = Value::Null;
    let mut best: Option<(i64, String)> = None;
    loop {
        let page = client
            .request("bots", json!({"after": after, "limit": 256}))
            .await
            .map_err(|e| e.to_string())?;
        for bot in page["bots"].as_array().into_iter().flatten() {
            let id = bot["bot_id"].as_i64().unwrap_or(0);
            if best.as_ref().is_none_or(|(b, _)| id > *b)
                && let (Some(provider), Some(model)) =
                    (bot["provider"].as_str(), bot["model"].as_str())
            {
                best = Some((id, format!("{provider}/{model}")));
            }
        }
        after = page["next_after"].clone();
        if after.is_null() {
            return Ok(best.map(|(_, model)| model));
        }
    }
}

async fn create(client: &Client, dir: &Path, model: Option<&str>) -> Result<(), String> {
    let role = role(dir)?;
    let info = agent_client::project::read(dir)?;
    // A project file that names a model keeps it; otherwise asked for,
    // the role's, AGENT_MODEL's, the config's `model`, then the newest
    // agent's, so a bare `sonny` needs no flag. With none, the person picks
    // one, kept as the config's `model` for later projects. Its effort: the
    // config's `[sonny] lead_effort`, else AGENT_EFFORT with AGENT_MODEL's
    // model, as the CLI takes them.
    let (mut effort, model) = (None, model.map(str::to_owned));
    let config = agent_client::config::load()?;
    let model = match model
        .or_else(|| info["model"].as_str().map(str::to_owned))
        .or_else(|| role.model.clone())
    {
        Some(model) => model,
        None => match std::env::var("AGENT_MODEL").ok() {
            Some(model) => {
                effort = std::env::var("AGENT_EFFORT").ok();
                model
            }
            None => match config.model.clone() {
                Some(model) => model,
                None => match newest(client).await? {
                    Some(model) => model,
                    None if crate::setup::interactive() => crate::setup::model(client).await?,
                    None => return Err("model_required: no default model yet; set model in ~/.agent/config.toml or pass --model PROVIDER/MODEL once".into()),
                },
            },
        },
    };
    let picks = agent_client::project::Picks {
        model: Some(model),
        effort: (config.sonny.lead_effort.clone()).or(effort.filter(|e| !e.is_empty())),
        web_search: config.web_search,
        ..Default::default()
    };
    make(client, dir, &role, &picks).await.map(|_| ())
}

/// Make the project in `dir` with its lead in `role`, told the task rule.
async fn make(
    client: &Client,
    dir: &Path,
    role: &agent_client::policy::Profile,
    picks: &agent_client::project::Picks,
) -> Result<Value, String> {
    let (instructions, _) = folder(dir, role)?;
    // Gated and confined from the start, as a new lead's mode makes it,
    // whoever opens it first.
    let mode = crate::approvals::MODES[0].0;
    let tools: Vec<String> = match &role.tools {
        Some(tools) => tools.clone(),
        // The tools the client will give it; one the gate names that the
        // lead doesn't get is left out there.
        None => {
            let model = picks.model.as_deref().or(role.model.as_deref());
            agent_client::default_tools(client.providers(), model.unwrap_or(""), picks.web_search)
                .into_iter()
                .map(String::from)
                .collect()
        }
    };
    let declare = json!(crate::declared::tools(true));
    let gate = crate::approvals::gate(mode, &crate::approvals::gateable(&tools, &declare));
    let policy = json!({
        "instructions": instructions,
        "compaction_instructions": agent_client::policy::DEFAULT_COMPACTION_INSTRUCTIONS,
        "model": role.model,
        "tools": role.tools,
        "declare": declare,
        "gate": gate,
        "settings": crate::approvals::sandbox(mode, true, dir),
    });
    agent_client::project::create(client, dir, &policy, client.providers(), picks, None).await
}

/// A `new_project` call of `caller`'s: the project Sonny makes for a
/// folder, on the caller's daemon (`created: false` when it was there
/// already), its lead opened in a home of its own.
pub async fn add(caller: &str, arguments: &Value) -> Result<Value, String> {
    let (dir, mut picks) = picks(arguments)?;
    let config = agent_client::config::load()?;
    if picks.effort.is_none() {
        picks.effort = config.sonny.lead_effort;
    }
    picks.web_search = config.web_search;
    let socket = std::env::var_os("AGENT_SOCKET")
        .map(PathBuf::from)
        .ok_or("AGENT_SOCKET is not set")?;
    let (client, _events) = Client::connect(&socket)
        .await
        .map_err(|e| format!("{}: {}", e.code, e.detail.unwrap_or_default()))?;
    let mut made = make(&client, &dir, &role(&dir)?, &picks).await;
    if let Ok(made) = &mut made {
        // Composed from its folder, so the lead follows the folder's
        // changes as one Sonny opened there does, when it opens.
        if made["created"] == true
            && let Some(text) = made["record"]["instructions"].as_str()
        {
            wrote(&lead(&socket, &made["record"]), text);
        }
        open_new_lead(&client, &socket, caller, made).await;
        made.as_object_mut().map(|m| m.remove("record"));
    }
    client.close().await;
    made
}

/// A lead made for another agent opens in a home of its own, a Herdr or
/// cmux workspace or a Rex session beside the caller's, as the person would
/// otherwise ask; `opened_in` says where, or `not_opened` why not.
async fn open_new_lead(client: &Client, socket: &Path, caller: &str, made: &mut Value) {
    let Some(project) = made["project"].as_str() else {
        return;
    };
    if made["created"] != true {
        return;
    }
    let like = crate::project_of(client, caller).await;
    let store = std::env::var_os("AGENT_STORE").map(PathBuf::from);
    let opened = std::env::current_exe()
        .map_err(|e| e.to_string())
        .and_then(|me| crate::open_lead(socket, store.as_deref(), project, &like, &me));
    match opened {
        Ok((home, _)) => made["opened_in"] = json!(home),
        Err(error) => made["not_opened"] = json!(error),
    }
}

/// The folder and picks a `new_project` call names: the caller's model
/// when it names none.
fn picks(arguments: &Value) -> Result<(PathBuf, agent_client::project::Picks), String> {
    let text = |key: &str| {
        arguments[key]
            .as_str()
            .filter(|v| !v.is_empty())
            .map(str::to_owned)
    };
    let folder = text("folder").ok_or("project_invalid: a folder")?;
    let mut picks = agent_client::project::Picks {
        model: text("model").or_else(|| std::env::var("AGENT_MODEL").ok()),
        effort: text("effort"),
        ..Default::default()
    };
    let (threads_model, threads_effort) = (text("threads_model"), text("threads_effort"));
    let threads_in = text("threads_in");
    let in_project = match threads_in.as_deref() {
        None | Some("worktree") => false,
        Some("project") => true,
        Some(other) => return Err(format!("project_invalid: threads_in {other:?}")),
    };
    if threads_model.is_some() || threads_effort.is_some() || threads_in.is_some() {
        picks.threads = Some(agent_client::project::ThreadPicks {
            model: threads_model,
            effort: threads_effort,
            in_project,
        });
    }
    let dir = Path::new(&folder)
        .canonicalize()
        .map_err(|e| format!("project_invalid: {folder}: {e}"))?;
    if !dir.is_dir() {
        return Err(format!(
            "project_invalid: {} is not a folder",
            dir.display()
        ));
    }
    Ok((dir, picks))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_new_project_reads_its_picks_and_refuses_others() {
        let picked = |arguments: Value| picks(&arguments);
        for bad in [json!({}), json!({"folder": "/", "threads_in": "elsewhere"})] {
            assert!(
                picked(bad.clone())
                    .unwrap_err()
                    .starts_with("project_invalid: "),
                "{bad}"
            );
        }
        assert!(
            picked(json!({"folder": "/no/such/folder/here"}))
                .unwrap_err()
                .starts_with("project_invalid: ")
        );
        let (_, picks) =
            picked(json!({"folder": "/", "model": "a/b", "threads_in": "project"})).unwrap();
        assert_eq!(picks.model.as_deref(), Some("a/b"));
        assert!(
            picks
                .threads
                .is_some_and(|t| t.in_project && t.model.is_none())
        );
        // Tasks may take an effort of their own on the lead's model.
        let (_, picks) = picked(json!({"folder": "/", "threads_effort": "xhigh"})).unwrap();
        assert!(picks.threads.is_some_and(|t| t.model.is_none()
            && t.effort.as_deref() == Some("xhigh")
            && !t.in_project));
    }

    #[test]
    fn a_lead_moved_away_is_told_from_a_folder_of_the_same_name() {
        let base = std::env::temp_dir().join(format!("sonny-same-name-{}", std::process::id()));
        let (one, two, other) = (base.join("a/shop"), base.join("b/shop"), base.join("tools"));
        for dir in [&one, &two, &other] {
            std::fs::create_dir_all(dir).unwrap();
        }
        // shop.lead working in another shop belongs there; in tools, or in
        // a folder gone since, it is this shop's, moved away.
        assert!(same_name(&two, "shop"));
        assert!(!same_name(&other, "shop"));
        assert!(!same_name(&base.join("gone/shop"), "shop"));
        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn every_folder_outside_git_has_its_own_lead_and_a_repository_one() {
        let base = std::env::temp_dir().join(format!("sonny-folder-of-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&base);
        let ws = base.join("ws");
        let (new, repo, bare) = (ws.join("new"), ws.join("repo"), ws.join("bare"));
        for dir in [
            &new,
            &repo.join("src/deep"),
            &bare.join("src"),
            &ws.join(".agents"),
        ] {
            std::fs::create_dir_all(dir).unwrap();
        }
        std::fs::write(ws.join(".agents/project.toml"), "name = \"ws\"\n").unwrap();
        let git = |dir: &Path| {
            let out = std::process::Command::new("git")
                .arg("-C")
                .arg(dir)
                .args(["init", "-q"])
                .output()
                .unwrap();
            assert!(out.status.success());
        };
        git(&repo);
        git(&bare);
        std::fs::create_dir_all(repo.join(".agents")).unwrap();
        std::fs::write(repo.join(".agents/project.toml"), "name = \"repo\"\n").unwrap();
        let home = base.join("home");
        let at = |dir: &Path| folder_in(&dir.canonicalize().unwrap(), Some(&home)).unwrap();
        let real = |dir: &Path| dir.canonicalize().unwrap();
        // A new folder in the workspace is a project of its own, not the
        // workspace's, and knows whose folder is above it.
        assert_eq!(at(&new), real(&new));
        assert_eq!(at(&ws), real(&ws));
        assert_eq!(above(&real(&new)).as_deref(), Some("ws.lead"));
        // Its lead reads its own memory, not the workspace's, though the
        // folder has no project file of its own until the lead is made.
        assert_eq!(
            agent_client::policy::project(&real(&new))
                .unwrap()
                .as_deref(),
            Some("ws")
        );
        let read = memory(&real(&new)).unwrap();
        assert!(
            read.last()
                .is_some_and(|index| index.ends_with("projects/new/MEMORY.md")),
            "{read:?}"
        );
        // In a repository, its project or its top, never a subfolder.
        assert_eq!(at(&repo.join("src/deep")), real(&repo));
        assert_eq!(at(&bare.join("src")), real(&bare));
        assert_eq!(above(&real(&bare)).as_deref(), Some("ws.lead"));
        // A task's folder under ~/.agent is no project's.
        let task = home.join(".agent/worktrees/ws.fix/sub");
        std::fs::create_dir_all(&task).unwrap();
        let error = folder_in(&task.canonicalize().unwrap(), Some(&home)).unwrap_err();
        assert!(error.starts_with("task_folder: "), "{error}");
        assert!(error.contains("sonny --bot ws.fix"), "{error}");
        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn a_project_that_keeps_its_subfolders_is_their_lead() {
        let base = std::env::temp_dir().join(format!("sonny-subfolders-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&base);
        let (top, a) = (base.join("top"), base.join("top/a"));
        let (pkg, own) = (a.join("src/pkg"), a.join("src/own"));
        for dir in [&top, &a, &pkg, &own, &top.join("new")] {
            std::fs::create_dir_all(dir.join(".agents")).unwrap();
        }
        let file = |dir: &Path, text: &str| {
            std::fs::write(dir.join(agent_client::project::FILE), text).unwrap()
        };
        file(&top, "name = \"top\"\n");
        file(&a, "name = \"a\"\nsubfolders = \"project\"\n");
        file(&own, "name = \"own\"\n");
        for repo in [&pkg, &own] {
            let out = std::process::Command::new("git")
                .arg("-C")
                .arg(repo)
                .args(["init", "-q"])
                .output()
                .unwrap();
            assert!(out.status.success());
        }
        std::fs::create_dir_all(pkg.join("src")).unwrap();
        let home = base.join("home");
        let at = |dir: &Path| folder_in(&dir.canonicalize().unwrap(), Some(&home)).unwrap();
        let real = |dir: &Path| dir.canonicalize().unwrap();
        // Below a, a plain folder and a repository without a project are a's.
        assert_eq!(at(&a.join("src")), real(&a));
        assert_eq!(at(&pkg), real(&a));
        assert_eq!(at(&pkg.join("src")), real(&a));
        // A project of its own stays one, and top keeps the default.
        assert_eq!(at(&own), real(&own));
        assert_eq!(at(&top.join("new")), real(&top.join("new")));
        // The home folder's file says nothing about the folders below it.
        let plain = home.join("plain");
        std::fs::create_dir_all(home.join(".agents")).unwrap();
        std::fs::create_dir_all(&plain).unwrap();
        file(&home, "name = \"me\"\nsubfolders = \"project\"\n");
        assert_eq!(at(&plain), real(&plain));
        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn kept_instructions_name_the_skills_they_miss() {
        let dir = std::env::temp_dir().join(format!("sonny-kept-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join(".agents/skills/deploy")).unwrap();
        std::fs::write(
            dir.join(".agents/skills/deploy/SKILL.md"),
            "# Deploy\n\nShip it.",
        )
        .unwrap();
        let lead = format!("kept-test-{}", std::process::id());
        let missing = |instructions: &str| {
            let bot = serde_json::json!({ "instructions": instructions });
            match review(&dir, &bot, &lead, false).unwrap() {
                Instructions::Kept { missing } => missing,
                other => panic!("kept, not {other:?}"),
            }
        };
        assert!(missing("set by hand").contains(&"deploy".to_owned()));
        assert!(!missing("set by hand\n- deploy: Deploy (x)").contains(&"deploy".to_owned()));
        assert!(
            Instructions::kept_notice(&["herdr-panes".into()])
                .starts_with("/instructions gives the lead the herdr-panes skill:")
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn leads_of_two_daemons_with_one_id_have_two_names() {
        let bot = serde_json::json!({"bot_id": 1});
        let one = lead(Path::new("/tmp/one/state.sqlite.sock"), &bot);
        let two = lead(Path::new("/tmp/two/state.sqlite.sock"), &bot);
        assert_ne!(one, two);
        assert!(one.ends_with("-1"));
    }

    #[test]
    fn the_project_file_keeps_its_other_lines_when_the_model_moves() {
        let file =
            "# mine\nname = \"shop\"\nmodel = \"openai/a\"   # pinned\nthreads_in = \"project\"\n";
        assert_eq!(
            with_model(file, "anthropic/b", Some("high")),
            "# mine\nname = \"shop\"\nmodel = \"anthropic/b\"\neffort = \"high\"\nthreads_in = \"project\"\n"
        );
        let file = "name = \"shop\"\neffort = \"low\"\nmodel = \"openai/a\"\n";
        assert_eq!(
            with_model(file, "openai/a", Some("high")),
            "name = \"shop\"\neffort = \"high\"\nmodel = \"openai/a\"\n"
        );
        assert_eq!(
            with_model(file, "openai/c", None),
            "name = \"shop\"\nmodel = \"openai/c\"\n"
        );
        // A quoted key is the same key.
        assert_eq!(
            with_model("\"model\" = \"openai/a\"\n", "openai/b", None),
            "model = \"openai/b\"\n"
        );
        // A file without a model gets one; a key that only starts alike is
        // left alone.
        assert_eq!(
            with_model("name = \"shop\"\nmodels_note = \"x\"\n", "openai/a", None),
            "name = \"shop\"\nmodels_note = \"x\"\nmodel = \"openai/a\"\n"
        );
    }
}
