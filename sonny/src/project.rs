//! The folder's coordinator: opened with no `--agent`, Sonny talks to the
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

/// A lead for a folder that holds other projects: it watches their leads
/// and routes between them. Sonny ships it beside the others, but no lead
/// runs as it unless set to (`profile = "superlead"`).
pub const SUPERLEAD: &str = include_str!("../agents/superlead.md");

/// The roles Sonny ships, which the person's files of the same names
/// replace, or build on with `extends`.
pub const BUILTINS: [(&str, &str); 3] = [("lead", LEAD), ("task", TASK), ("superlead", SUPERLEAD)];

/// The most bytes each shipped role may hold. Every agent reads its role
/// on every call, so a role only grows by cutting elsewhere: merge or drop
/// a sentence, or leave to memory what the person can teach. Budgets come
/// down as evals show shorter text does as well, and go up only with the
/// owner's say.
#[cfg(test)]
const BUDGETS: [(&str, usize); 3] = [
    ("lead", 16 * 1024),
    ("task", 4 * 1024),
    ("superlead", 3 * 1024),
];

/// [`BUILTINS`] as agents read them: without the commons while it is off.
pub fn builtins() -> [(&'static str, std::borrow::Cow<'static, str>); 3] {
    BUILTINS.map(|(name, text)| (name, crate::commons::role(text)))
}

/// The profile `name` for an agent in `dir`, which must exist: the folder's
/// or the person's file, else the one Sonny ships.
pub fn profile(dir: &Path, name: &str) -> Result<agent_client::policy::Profile, String> {
    let roles = builtins();
    let roles = roles.each_ref().map(|(name, text)| (*name, text.as_ref()));
    agent_client::policy::profile_with(dir, name, &roles)
        .map_err(|e| format!("{}: {e}", e.code()))?
        .ok_or_else(|| {
            let missing = agent_client::policy::Failure::Missing {
                name: name.to_owned(),
                kind: "profile",
            };
            format!("{}: {missing}", missing.code())
        })
}

/// The name of the profile a project's lead runs as: its file's `profile`,
/// else the config's `[sonny] lead_profile`, else `lead`.
pub fn lead_profile(info: &Value) -> Result<String, String> {
    named(info, "profile", |sonny| sonny.lead_profile, "lead")
}

/// The name of the profile every agent but a lead is told after the rest:
/// the project file's `threads_profile`, else the config's
/// `[sonny] task_profile`, else `task`.
pub fn task_profile(info: &Value) -> Result<String, String> {
    named(info, "threads_profile", |sonny| sonny.task_profile, "task")
}

fn named(
    info: &Value,
    key: &str,
    config: fn(agent_client::config::Sonny) -> Option<String>,
    default: &str,
) -> Result<String, String> {
    if let Some(name) = info[key].as_str() {
        return Ok(name.to_owned());
    }
    Ok(config(agent_client::config::load()?.sonny).unwrap_or_else(|| default.to_owned()))
}

/// `text`, an agent's composed instructions, with what every agent but a
/// lead is told after it: the body of the task profile `info`, a project's
/// fields, names (`task.md`, the folder's or the person's, else Sonny's,
/// unless set to another).
pub fn as_task(dir: &Path, info: &Value, text: &str) -> Result<String, String> {
    let task = profile(dir, &task_profile(info)?)?;
    Ok(match task.body.as_str() {
        "" => text.to_owned(),
        body => format!("{text}\n\n{}", agent_client::policy::with_cli(body)),
    })
}

/// The project `bot` leads: `shop` for its first lead, `shop.lead`, and
/// for another lead of the same folder, `shop.lead.NAME`; `None` for any
/// other bot, a lead's side chat (`shop.lead.side-1a2b`) included.
pub fn lead_project(bot: &str) -> Option<&str> {
    if let Some(project) = bot.strip_suffix(".lead") {
        return Some(project).filter(|p| !p.is_empty());
    }
    let (first, name) = bot.rsplit_once('.')?;
    let project = first.strip_suffix(".lead").filter(|p| !p.is_empty())?;
    (lead_named(name) && crate::side_source(bot).is_none()).then_some(project)
}

/// Whether `bot` is a project's lead, its first or another.
pub fn is_lead(bot: &str) -> bool {
    lead_project(bot).is_some()
}

/// Whether `name` can name another lead of a folder, as in
/// `shop.lead.NAME`: lowercase letters, digits and hyphens, at most 32,
/// and never a side chat's.
pub fn lead_named(name: &str) -> bool {
    (1..=32).contains(&name.len())
        && name
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-')
        && !name.starts_with('-')
        && !name.ends_with('-')
        && !name.starts_with("side-")
}

/// The lead of `project` that `name` names: `lead` the first one,
/// `shop.lead`, any other `shop.lead.NAME`.
pub fn lead_bot(project: &str, name: &str) -> String {
    match name {
        "" | "lead" => format!("{project}.lead"),
        name => format!("{project}.lead.{name}"),
    }
}

/// The short name of a lead, as `/lead` takes it: `lead` for the first.
pub fn lead_short(bot: &str) -> &str {
    match bot.ends_with(".lead") {
        true => "lead",
        false => bot.rsplit_once('.').map_or(bot, |(_, name)| name),
    }
}

/// A lead's name made from the person's first words to it, as
/// `fix-the-login` for "Fix the login redirect, it loops": what a thread
/// with no name of its own is called.
pub fn lead_name_from(text: &str) -> String {
    let words: Vec<String> = text
        .split(|c: char| !c.is_ascii_alphanumeric())
        .filter(|w| !w.is_empty())
        .take(4)
        .map(str::to_ascii_lowercase)
        .collect();
    let mut name = String::new();
    for word in words {
        if name.len() + word.len() + 1 > 32 {
            break;
        }
        if !name.is_empty() {
            name.push('-');
        }
        name.push_str(&word);
    }
    match lead_named(&name) {
        true => name,
        false => "thread".into(),
    }
}

/// Which of a folder's leads to open.
#[derive(Clone, Debug, PartialEq)]
pub enum Pick {
    /// The one the person last wrote to there; the first when none.
    Last,
    /// This one, by its short name, which must be there.
    Named(String),
    /// A new one, named so, else numbered (`lead.2`); the folder's first
    /// when it has none yet.
    New(Option<String>),
    /// A new thread's in T3 Code: the folder's first lead when it has none
    /// yet, else a new one named after the person's first words to it.
    Thread(String),
}

/// Where Sonny notes which lead of a folder the person last wrote to, in
/// the store of that lineage: two stores under one home have their own.
fn last_path(store: Option<&str>, dir: &Path) -> Option<PathBuf> {
    let home = std::env::var_os("HOME")?;
    let dir = dir.to_string_lossy();
    let key = match store {
        Some(store) => fingerprint(&format!("{store}\n{dir}")),
        None => fingerprint(&dir),
    };
    Some(PathBuf::from(home).join(".agent/sonny/last-lead").join(key))
}

/// Note that the person wrote to `bot`, a lead working in `dir`, in the
/// store of lineage `store`.
pub fn wrote_to(store: Option<&str>, dir: &str, bot: &str) {
    if !is_lead(bot) || dir.is_empty() {
        return;
    }
    let Some(path) = last_path(store, Path::new(dir)) else {
        return;
    };
    if std::fs::read_to_string(&path).is_ok_and(|had| had.trim() == bot) {
        return;
    }
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    let staged = path.with_extension(format!("{}.tmp", std::process::id()));
    if std::fs::write(&staged, bot).is_ok() {
        let _ = std::fs::rename(&staged, &path);
    }
}

fn last_lead(store: Option<&str>, dir: &Path) -> Option<String> {
    let text = std::fs::read_to_string(last_path(store, dir)?).ok()?;
    Some(text.trim().to_owned()).filter(|bot| is_lead(bot))
}

/// Every lead of `project` working in `dir`, first one first, then by name.
pub async fn leads(client: &Client, project: &str, dir: &Path) -> Result<Vec<Value>, String> {
    let at = dir.to_str().unwrap_or("");
    let mut found = Vec::new();
    let mut after = Value::Null;
    loop {
        let page = client
            .request(
                "bots",
                json!({"name": format!("{project}.lead*"), "limit": 256, "after": after}),
            )
            .await
            .map_err(|e| e.to_string())?;
        for bot in page["bots"].as_array().into_iter().flatten() {
            let name = bot["name"].as_str().unwrap_or("");
            if lead_project(name) == Some(project) && bot["workspace"] == at {
                found.push(bot.clone());
            }
        }
        after = page["next_after"].clone();
        if after.is_null() {
            break;
        }
    }
    found.sort_by_key(|bot| {
        let name = bot["name"].as_str().unwrap_or("").to_owned();
        (!name.ends_with(".lead"), name)
    });
    Ok(found)
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

/// The lead's role: the profile its project is set to, `lead` unless
/// set otherwise, the folder's or the person's, else Sonny's.
fn role(dir: &Path) -> Result<agent_client::policy::Profile, String> {
    let info = agent_client::project::read(dir)?;
    profile(dir, &lead_profile(&info)?)
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
    as_task(dir, &agent_client::project::read(dir)?, &composed.text)
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

/// The lead for `dir` that `pick` picks, made first when the folder is
/// not a project yet or the pick is a new one.
pub async fn open(
    client: &Client,
    dir: &Path,
    model: Option<&str>,
    pick: &Pick,
) -> Result<Lead, String> {
    let dir = folder_of(&dir.canonicalize().map_err(|e| e.to_string())?)?;
    let project = default_name(&dir)?;
    let first = format!("{project}.lead");
    let found = find(client, &first).await?;
    if let Some(found) = &found {
        let there = found["workspace"].as_str().unwrap_or("");
        // Another folder of the same name is its own project: its lead
        // stays there.
        if there != dir.to_str().unwrap_or("") && same_name(Path::new(there), &project) {
            return Err(format!(
                "project_exists: {first} is the lead of {there}, a project of the same name; \
                 name this one in .agents/project.toml or pass --agent"
            ));
        }
    }
    let Some(found) = found else {
        // The folder's first lead, whatever was asked: its name is the
        // project's, so the others can be found by it.
        // Another lead of the folder outlives the first: the one named, or
        // the one last written to.
        let other = match pick {
            Pick::Named(name) => Some(lead_bot(&project, name)),
            Pick::Last => last_lead(client.store(), &dir)
                .filter(|bot| lead_project(bot) == Some(project.as_str())),
            _ => None,
        };
        if let Some(bot) = other.filter(|bot| *bot != first) {
            let there = find(client, &bot).await?;
            if there.is_some_and(|found| found["workspace"].as_str() == dir.to_str()) {
                return Ok(Lead {
                    project,
                    bot,
                    new: false,
                    moving: None,
                    above: above(&dir),
                });
            }
            if let Pick::Named(name) = pick {
                return Err(format!("lead_missing: {project} has no lead {name} yet"));
            }
        }
        create(client, &dir, model, None).await?;
        return Ok(Lead {
            project,
            bot: first,
            new: true,
            moving: None,
            above: above(&dir),
        });
    };
    let existing = || async { leads(client, &project, &dir).await };
    let make = |bot: String| {
        let (dir, project) = (dir.clone(), project.clone());
        async move {
            // The whole of the name after `{project}.lead.`, dots and all.
            let name = bot.strip_prefix(&format!("{project}.lead.")).unwrap_or("");
            if !lead_named(name) {
                return Err(
                    "lead_invalid: a lead's name is lowercase letters, digits and hyphens, \
                     at most 32, as billing or fix-login"
                        .into(),
                );
            }
            if find(client, &bot).await?.is_some() {
                return Err(format!(
                    "lead_exists: {bot} is there already; /lead {} opens it",
                    lead_short(&bot)
                ));
            }
            // Made by another thread since the look above: theirs.
            if !create(client, &dir, model, Some(&bot)).await? {
                return Err(format!(
                    "lead_exists: {bot} is there already; /lead {} opens it",
                    lead_short(&bot)
                ));
            }
            Ok(Lead {
                project,
                bot,
                new: true,
                moving: None,
                above: None,
            })
        }
    };
    // The first name free of `taken` from `stem`, numbered from 2, cut so
    // the number fits.
    let free = |taken: &[String], stem: &str| -> String {
        let mut n = 2;
        loop {
            let number = n.to_string();
            let room = 32 - number.len() - 1;
            let cut = stem
                .get(..stem.len().min(room))
                .unwrap_or("")
                .trim_end_matches('-');
            let name = match cut {
                "" => number,
                cut => format!("{cut}-{number}"),
            };
            let bot = lead_bot(&project, &name);
            if !taken.contains(&bot) {
                return bot;
            }
            n += 1;
        }
    };
    // A new lead under the first free name: one another thread takes
    // meanwhile is passed over.
    let fresh = |stem: String, first_try: Option<String>| async move {
        let mut taken: Vec<String> = (existing().await?.iter())
            .filter_map(|bot| bot["name"].as_str().map(str::to_owned))
            .collect();
        let mut bot = first_try
            .filter(|bot| !taken.contains(bot))
            .unwrap_or_else(|| free(&taken, &stem));
        for _ in 0..8 {
            match make(bot.clone()).await {
                Err(error) if error.starts_with("lead_exists") => {
                    taken.push(bot);
                    bot = free(&taken, &stem);
                }
                made => return made,
            }
        }
        Err(format!(
            "lead_exists: no free name for a new lead from {stem}"
        ))
    };
    let bot = match pick {
        Pick::Named(name) => {
            let bot = lead_bot(&project, name);
            if bot != first && find(client, &bot).await?.is_none() {
                return Err(format!(
                    "lead_missing: {project} has no lead {name}; /lead new {name} makes it"
                ));
            }
            bot
        }
        Pick::New(Some(name)) => return make(lead_bot(&project, name)).await,
        Pick::New(None) => return fresh(String::new(), None).await,
        Pick::Thread(words) => {
            let stem = lead_name_from(words);
            let bot = lead_bot(&project, &stem);
            return fresh(stem, Some(bot)).await;
        }
        Pick::Last => last_lead(client.store(), &dir)
            .filter(|bot| lead_project(bot) == Some(project.as_str()))
            .unwrap_or_else(|| first.clone()),
    };
    let (bot, found) = match bot == first {
        true => (bot, found),
        false => match find(client, &bot).await? {
            Some(other) => (bot, other),
            // The lead last written to is gone, or of another store: the
            // first one opens.
            None if *pick == Pick::Last => (first, found),
            None => return Err(format!("lead_missing: {bot} is gone")),
        },
    };
    let there = found["workspace"].as_str().unwrap_or("");
    // One moved anywhere else is still this folder's, and comes back with
    // the next message, history and all.
    let moving = (there != dir.to_str().unwrap_or("")).then_some(dir);
    Ok(Lead {
        project,
        bot,
        new: false,
        moving,
        above: None,
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
             folder, or sonny --agent {task} for the task",
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
    // Only a provider the daemon runs: after a switch, an older bot's
    // model would fail to open.
    let stats = client
        .request("stats", json!({}))
        .await
        .map_err(|e| e.to_string())?;
    let runs = |provider: &str| stats["providers"].get(provider).is_some();
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
                && runs(provider)
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

/// Make the folder's first lead, or `bot`, another lead of it: whether
/// this made it, not found it there already.
async fn create(
    client: &Client,
    dir: &Path,
    model: Option<&str>,
    bot: Option<&str>,
) -> Result<bool, String> {
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
        fallbacks: config.fallbacks,
        ..Default::default()
    };
    let made = make(client, dir, &role, &picks, bot).await?;
    Ok(made["created"] == true)
}

/// Make the project in `dir` with its lead in `role`, told the task rule;
/// or `bot`, another lead of it.
async fn make(
    client: &Client,
    dir: &Path,
    role: &agent_client::policy::Profile,
    picks: &agent_client::project::Picks,
    bot: Option<&str>,
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
    let mut settings = crate::approvals::sandbox(mode, dir);
    if agent_client::config::load()?.hide_senders {
        settings.insert("hide_senders".into(), json!(true));
    }
    let policy = json!({
        "instructions": instructions,
        "compaction_instructions": agent_client::policy::DEFAULT_COMPACTION_INSTRUCTIONS,
        "model": role.model,
        "tools": role.tools,
        "declare": declare,
        "gate": gate,
        "settings": settings,
    });
    agent_client::project::create(client, dir, &policy, client.providers(), picks, bot).await
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
    picks.fallbacks = config.fallbacks;
    let socket = std::env::var_os("AGENT_SOCKET")
        .map(PathBuf::from)
        .ok_or("AGENT_SOCKET is not set")?;
    let (client, _events) = Client::connect(&socket)
        .await
        .map_err(|e| format!("{}: {}", e.code, e.detail.unwrap_or_default()))?;
    let mut made = make(&client, &dir, &role(&dir)?, &picks, None).await;
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
        .and_then(|me| {
            let bot = format!("{project}.lead");
            crate::open_lead(socket, store.as_deref(), project, &bot, &like, &me)
        });
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
    fn shipped_roles_fit_their_budgets() {
        for (name, text) in BUILTINS {
            let (_, budget) = BUDGETS
                .iter()
                .find(|(n, _)| *n == name)
                .expect("a budget per role");
            assert!(
                text.len() <= *budget,
                "sonny/agents/{name}.md is {} bytes, over its {budget}: cut as much as you add",
                text.len()
            );
        }
    }

    #[test]
    fn each_store_notes_its_own_last_lead_in_a_folder() {
        let dir = Path::new("/work/shop");
        let a = last_path(Some("store-a"), dir).unwrap();
        let b = last_path(Some("store-b"), dir).unwrap();
        assert_ne!(a, b);
        assert_eq!(a, last_path(Some("store-a"), dir).unwrap());
        assert_ne!(
            a,
            last_path(Some("store-a"), Path::new("/work/web")).unwrap()
        );
    }

    #[test]
    fn a_folder_has_a_first_lead_and_others_named_after_it() {
        assert_eq!(lead_project("shop.lead"), Some("shop"));
        assert_eq!(lead_project("shop.web.lead"), Some("shop.web"));
        assert_eq!(lead_project("shop.lead.billing"), Some("shop"));
        assert_eq!(lead_project("shop.web.lead.fix-2"), Some("shop.web"));
        // A side chat, a task, a lead's task and a bad name lead nothing.
        for not in [
            "shop.lead.side-1a2b",
            "shop.fix",
            "shop.lead.billing.side-3f",
            "shop.lead.Big",
            ".lead",
            "lead",
        ] {
            assert_eq!(lead_project(not), None, "{not}");
        }
        assert_eq!(lead_bot("shop", "lead"), "shop.lead");
        assert_eq!(lead_bot("shop", "billing"), "shop.lead.billing");
        assert_eq!(lead_short("shop.lead"), "lead");
        assert_eq!(lead_short("shop.lead.billing"), "billing");
        assert!(!lead_named("side-1a2b") && !lead_named("-x") && !lead_named(&"a".repeat(33)));
    }

    #[test]
    fn a_threads_lead_is_named_after_its_first_words() {
        assert_eq!(
            lead_name_from("Fix the login redirect, it loops"),
            "fix-the-login-redirect"
        );
        assert_eq!(lead_name_from("  ¿¡ !"), "thread");
        assert!(lead_named(&lead_name_from(&"word ".repeat(40))));
    }

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
    fn a_project_sets_the_profile_its_lead_runs_as() {
        let dir = std::env::temp_dir().join(format!("sonny-lead-profile-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join(".agents/agents")).unwrap();
        let dir = dir.canonicalize().unwrap();
        std::fs::write(
            dir.join(".agents/project.toml"),
            "name = \"top\"\nprofile = \"my-lead\"\n",
        )
        .unwrap();
        // Named but not written: the lead is not made without it.
        assert!(role(&dir).unwrap_err().starts_with("profile_not_found: "));
        std::fs::write(
            dir.join(".agents/agents/my-lead.md"),
            "---\nextends: lead\n---\nReview each change before it lands.\n",
        )
        .unwrap();
        let lead = role(&dir).unwrap();
        assert_eq!(lead.name, "my-lead");
        assert!(lead.body.ends_with("Review each change before it lands."));
        // The lead's text, the person's or Sonny's, comes first.
        let base = profile(&dir, "lead").unwrap().body;
        assert!(lead.body.starts_with(&base));
        let text = instructions(&dir).unwrap();
        assert!(text.contains("# Role: my-lead\n\n"), "{text}");
        // A variant of the lead is no role to start a peer in.
        assert!(!text.contains("- my-lead:"));
        // Sonny ships the superlead, which a project opts into the same way.
        std::fs::write(
            dir.join(".agents/project.toml"),
            "name = \"top\"\nprofile = \"superlead\"\n",
        )
        .unwrap();
        let lead = role(&dir).unwrap();
        assert!(lead.body.starts_with(&base));
        assert!(lead.body.contains("you lead their leads"));
        assert!(!instructions(&dir).unwrap().contains("- superlead:"));
        let _ = std::fs::remove_dir_all(&dir);
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
        assert!(error.contains("sonny --agent ws.fix"), "{error}");
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
