//! The folder's lead: opened with no `--agent`, Sonny talks to the
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

/// How a lead moves work and words between agents, the skill [`LEAD`]
/// loads whole (`skills: [coordination]`), kept apart so coordination has
/// one place to change.
pub const COORDINATION: &str = include_str!("../skills/coordination/SKILL.md");

/// The roles Sonny ships, which the person's files of the same names
/// replace, or build on with `extends`.
pub const BUILTINS: [(&str, &str); 3] = [("lead", LEAD), ("task", TASK), ("superlead", SUPERLEAD)];

/// The most bytes each shipped role may hold, with the skills it loads
/// whole. Every agent reads its role
/// on every call, so a role only grows by cutting elsewhere: merge or drop
/// a sentence, or leave to memory what the person can teach. Budgets come
/// down as evals show shorter text does as well, and go up only with the
/// owner's say.
#[cfg(test)]
const BUDGETS: [(&str, usize); 3] = [
    ("lead", 16 * 1024 + 512),
    ("task", 4 * 1024),
    ("superlead", 3 * 1024),
];

/// The project in `dir` (`agent_client::project::read`), its file's
/// `threads_*` keys renamed `task_*` and its `coordinator` key `lead`
/// first, once for the file each (`TASK_KEYS`, `LEAD_KEY`, recorded in
/// `~/.agent/sonny/migrated` when it renamed some), so old keys a checkout
/// brings back after are refused by name.
pub fn read(dir: &Path) -> Result<Value, String> {
    use agent_client::project::{FILE, LEAD_KEY, TASK_KEYS, rename_lead_key, rename_task_keys};
    if let Some(home) = std::env::var_os("HOME") {
        let record = Path::new(&home).join(".agent/sonny/migrated");
        let file = dir.join(FILE);
        let file = file.to_string_lossy();
        crate::upgrade::once_for(&record, TASK_KEYS, &file, || rename_task_keys(dir));
        crate::upgrade::once_for(&record, LEAD_KEY, &file, || rename_lead_key(dir));
    }
    agent_client::project::read(dir)
}

/// [`BUILTINS`] as agents read them: without the commons while it is off.
pub fn builtins() -> [(&'static str, std::borrow::Cow<'static, str>); 3] {
    BUILTINS.map(|(name, text)| (name, crate::commons::role(text)))
}

/// The profile `name` for an agent in `dir`, which must exist: the folder's
/// or the person's file, else the one Sonny ships.
pub fn profile(dir: &Path, name: &str) -> Result<agent_client::policy::Profile, String> {
    let roles = builtins();
    let roles = roles.each_ref().map(|(name, text)| (*name, text.as_ref()));
    let coordination = crate::commons::role(COORDINATION);
    let skills = [("coordination", coordination.as_ref())];
    agent_client::policy::profile_with(dir, name, &roles, &skills)
        .map_err(|e| format!("{}: {e}", e.code()))?
        .ok_or_else(|| missing_profile(name))
}

/// The role `name` as Sonny ships it, none of the person's files read;
/// `None` for a name Sonny ships no role of.
fn shipped(name: &str) -> Option<agent_client::policy::Profile> {
    let roles = builtins();
    let roles = roles.each_ref().map(|(name, text)| (*name, text.as_ref()));
    let coordination = crate::commons::role(COORDINATION);
    let skills = [("coordination", coordination.as_ref())];
    agent_client::policy::shipped_profile(name, &roles, &skills)
        .ok()
        .flatten()
}

fn missing_profile(name: &str) -> String {
    let missing = agent_client::policy::Failure::Missing {
        name: name.to_owned(),
        kind: "profile",
    };
    format!("{}: {missing}", missing.code())
}

/// The name of the profile a project's lead runs as: its file's `profile`,
/// else the config's `[sonny] lead_profile`, else `lead`.
pub fn lead_profile(info: &Value) -> Result<String, String> {
    named(info, "profile", |sonny| sonny.lead_profile, "lead")
}

/// The name of the profile every agent but a lead is told after the rest:
/// the project file's `task_profile`, else the config's
/// `[sonny] task_profile`, else `task`.
pub fn task_profile(info: &Value) -> Result<String, String> {
    named(info, "task_profile", |sonny| sonny.task_profile, "task")
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
        body => format!("{text}\n\n{body}"),
    })
}

/// The mark that starts the name of each bot Sonny makes for itself, not
/// for the person (the reviewer's base and its forks): a leading dot, as
/// a hidden file has. No project name starts with one
/// (`agent_client::project::valid_name`), so no agent named from a folder
/// shares the names, and what lists the person's agents (the sky, /status,
/// the agent picker) leaves them out by it.
pub const INTERNAL: &str = ".";

/// `name` as one of Sonny's own bots, under [`INTERNAL`].
pub fn internal(name: &str) -> String {
    format!("{INTERNAL}{name}")
}

/// Whether `bot` is one of Sonny's own, under [`INTERNAL`].
pub fn is_internal(bot: &str) -> bool {
    bot.starts_with(INTERNAL)
}

/// The namespace Sonny gives the agent named `bot` in the daemon: its
/// name up to the first dot, which its agent tools list (`shop.*`) and name
/// the agents it starts in (`shop.NAME`); `None` for a name with none
/// before a dot.
pub fn namespace(bot: &str) -> Option<&str> {
    bot.split_once('.')
        .map(|(namespace, _)| namespace)
        .filter(|namespace| !namespace.is_empty())
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

/// Whether `record`, a bot as the daemon lists it, is a project's lead:
/// named as one ([`is_lead`]) and made by no agent, as Sonny makes leads
/// for the person. An agent may name one it makes as a lead, but that
/// doesn't make it one: what a lead may do, such as run its folder's
/// setup as the person, goes by who made it, not by its name.
pub fn is_lead_record(record: &serde_json::Value) -> bool {
    is_lead(record["name"].as_str().unwrap_or_default()) && !record["created_by"].is_string()
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

/// The short name of a lead, as `/switch` and `/new` take it: `lead` for
/// the first.
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
    /// One `/new` makes: named so, else after the person's first words to
    /// it, in the role `profile`, else the project's.
    Made {
        name: Option<String>,
        words: String,
        profile: Option<String>,
    },
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

/// The project whose leads work in `dir`, and the folder they work in:
/// the one `open` opens a lead of.
pub fn folder_project(dir: &Path) -> Result<(String, PathBuf), String> {
    let dir = folder_of(&dir.canonicalize().map_err(|e| e.to_string())?)?;
    Ok((default_name(&dir)?, dir))
}

/// One page of every project's leads, by name from past `after`, and the
/// name the next page starts past: the daemon's own paging.
pub async fn every_lead(
    client: &Client,
    after: Option<&str>,
) -> Result<(Vec<Value>, Option<String>), String> {
    let page = client
        .request(
            "bots",
            json!({"name": "*.lead*", "limit": 256, "after": after}),
        )
        .await
        .map_err(|e| e.to_string())?;
    let leads = (page["bots"].as_array().into_iter().flatten())
        .filter(|bot| is_lead(bot["name"].as_str().unwrap_or("")))
        .cloned()
        .collect();
    Ok((leads, page["next_after"].as_str().map(str::to_owned)))
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
    let info = read(dir)?;
    Ok(info["name"].as_str().unwrap_or("project").to_owned())
}

/// The lead's role: `chosen`, the profile `/new` made it in, else the one
/// its project is set to, `lead` unless set otherwise; the folder's or the
/// person's file, else Sonny's.
fn role(dir: &Path, chosen: Option<&str>) -> Result<agent_client::policy::Profile, String> {
    match chosen {
        Some(name) => profile(dir, name),
        None => profile(dir, &lead_profile(&read(dir)?)?),
    }
}

/// [`role`], or, when the project's own role cannot be used, Sonny's of
/// the same name (else its lead's), with why in `left_out`. A role the
/// person picked by name is theirs to fix: that fails.
fn role_or_shipped(
    dir: &Path,
    chosen: Option<&str>,
    left_out: &mut Vec<String>,
) -> Result<agent_client::policy::Profile, String> {
    let error = match role(dir, chosen) {
        Ok(role) => return Ok(role),
        Err(error) if chosen.is_some() => return Err(error),
        Err(error) => error,
    };
    let name = (read(dir).ok())
        .and_then(|info| lead_profile(&info).ok())
        .filter(|name| shipped(name).is_some())
        .unwrap_or_else(|| "lead".into());
    let role = shipped(&name).ok_or(error.clone())?;
    left_out.push(format!(
        "the lead's role: {error}; it runs as Sonny's own {name} role"
    ));
    Ok(role)
}

/// A lead's instructions as composed now, and what was left out of them.
pub struct Composed {
    pub text: String,
    /// Each file (or the role) that could not be used, and why: the
    /// lead opens without it, and it and the person are told.
    pub left_out: Vec<String>,
    /// Where the heads of its memory indexes sit in `text`, between the
    /// skill and profile lists and the role: what agents save, which alone
    /// never gives a lead new instructions between its turns. None when
    /// the text does not read as composed so, which a test keeps from
    /// happening.
    pub memory: Option<std::ops::Range<usize>>,
}

/// A lead's instructions as Sonny composes them for `dir` now, in the
/// role `chosen` or its project's: its role, the folder's rules, skills
/// and memory, then the project's task rule.
fn compose(dir: &Path, chosen: Option<&str>) -> Result<Composed, String> {
    compose_with(dir, chosen, &memory(dir)?)
}

/// [`compose`], with the `memory` indexes given.
fn compose_with(dir: &Path, chosen: Option<&str>, memory: &[PathBuf]) -> Result<Composed, String> {
    let mut left_out = Vec::new();
    let role = role_or_shipped(dir, chosen, &mut left_out)?;
    let mut composed = folder(dir, &role, memory)?;
    left_out.append(&mut composed.left_out);
    let info = read(dir)?;
    let rule = agent_client::project::rule(&info);
    composed.text = format!("{}\n\n{rule}", composed.text);
    composed.left_out = left_out;
    Ok(composed)
}

/// The headings composing writes before a lead's memory: one that follows
/// what a lead's instructions hold before their memory now is a list or a
/// file gone since, not memory.
const BEFORE_MEMORY: [&str; 4] = [
    "\n\n# Instructions from ",
    "\n\n# Skills\n",
    "\n\n# Profiles\n",
    "\n\n# Role: ",
];

/// The role with the folder's rules, skills and memory and who it works
/// for, before the project's task rule. A rule, skill or memory file that
/// cannot be read, or does not fit, is left out whole and said in
/// `left_out`: a broken link in a skills folder never keeps a lead from
/// opening.
fn folder(
    dir: &Path,
    role: &agent_client::policy::Profile,
    memory: &[PathBuf],
) -> Result<Composed, String> {
    let composed = agent_client::policy::hiding(&agent_client::hidden::known(), || {
        agent_client::policy::instructions_leaving_out(dir, Some(role), memory)
    })
    .map_err(|e| e.to_string())?;
    // Memory goes in right before the role, each index's head after a
    // blank line: where, read from what went in.
    let role_block = match role.body.is_empty() {
        true => String::new(),
        false => format!("\n\n# Role: {}\n\n{}", role.name, role.body),
    };
    let saved: usize = composed.memory.iter().map(|index| 2 + index.bytes).sum();
    let memory = (composed.text.strip_suffix(&role_block))
        .map(str::len)
        .and_then(|end| Some(end.checked_sub(saved)?..end))
        .filter(|at| {
            (composed.text.get(at.clone())).is_some_and(|m| m.is_empty() || m.starts_with("\n\n"))
        });
    // Paths under the home folder as `~`, so a narrow window shows what
    // is missing; the lead's shell reads them the same.
    let home = std::env::var("HOME").unwrap_or_default();
    let left_out = (composed.left_out.iter())
        .map(|failure| {
            let said = failure.to_string();
            let said = match home.as_str() {
                "" | "/" => said,
                home => said.replace(&format!("{home}/"), "~/"),
            };
            format!("left out of the lead's instructions: {said}")
        })
        .collect();
    Ok(Composed {
        text: composed.text,
        left_out,
        memory,
    })
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
    let composed = agent_client::policy::hiding(&agent_client::hidden::known(), || {
        agent_client::policy::instructions(dir, None, &memory)
    })
    .map_err(|e| format!("{}: {e}", e.code()))?;
    as_task(dir, &read(dir)?, &composed.text)
}

/// The folder's instructions for a lead, when they are not the ones it
/// `held` last: a lead takes all its folder's, memory too, as its window
/// opens, as Codex reads its instructions once as a session starts, and
/// between its turns only what the person changed ([`changed`]). With
/// what was left out of them, which the lead goes without either way.
pub fn review(
    dir: &Path,
    held: &str,
    chosen: Option<&str>,
) -> Result<(Option<String>, Vec<String>), String> {
    let Composed { text, left_out, .. } = compose(dir, chosen)?;
    Ok(((text != held).then_some(text), left_out))
}

/// The folder's instructions for a lead at its turn's end, when its role,
/// rules, skills, profiles or task rule are not the ones it `held`: the
/// person changed what it is told, and it takes it for its next turn, at
/// the cost of one call that reads no cache. Its memory changing alone is
/// no such change: agents save facts all the time, and each would cost its
/// next call the whole prompt cache, so a lead takes the memory there is
/// as its window opens, and with a change of the rest.
///
/// `known` is what the last look at this lead found ([`Renewal::known`]):
/// while it still holds those instructions, a change is the rest composed
/// without memory changing, which memory never moves, though near the
/// size limit its growth shortens the skill lists. Without it, the lead
/// holds the same text as now around its memory.
pub fn changed(
    dir: &Path,
    held: &str,
    chosen: Option<&str>,
    known: Option<&Known>,
) -> Result<Renewal, String> {
    changed_with(dir, held, chosen, &memory(dir)?, known)
}

/// What [`changed`] found.
pub struct Renewal {
    /// The instructions the lead is to take, when they changed.
    pub text: Option<String>,
    /// What was left out of them.
    pub left_out: Vec<String>,
    /// What the next look at the lead is given, once it holds `text`, or
    /// still what it held.
    pub known: Known,
}

/// What a lead held when last looked at, and what its folder told it then
/// but memory, each by its fingerprint.
#[derive(Clone, Debug, PartialEq)]
pub struct Known {
    held: String,
    rest: String,
}

/// [`changed`], with the `memory` indexes given.
fn changed_with(
    dir: &Path,
    held: &str,
    chosen: Option<&str>,
    memory: &[PathBuf],
    known: Option<&Known>,
) -> Result<Renewal, String> {
    let Composed {
        text,
        left_out,
        memory,
    } = compose_with(dir, chosen, memory)?;
    let rest = fingerprint(&compose_with(dir, chosen, &[])?.text);
    let same = text == held
        || match known {
            Some(known) if known.held == fingerprint(held) => known.rest == rest,
            _ => memory.is_some_and(|at| all_but_memory(held, &text[..at.start], &text[at.end..])),
        };
    let text = (!same).then_some(text);
    let held = fingerprint(text.as_deref().unwrap_or(held));
    Ok(Renewal {
        text,
        left_out,
        known: Known { held, rest },
    })
}

/// Whether `held` is `before`, then memory or none, then `after`: what
/// a lead's instructions hold around their memory now.
fn all_but_memory(held: &str, before: &str, after: &str) -> bool {
    let Some(memory) = (held.strip_prefix(before)).and_then(|rest| rest.strip_suffix(after)) else {
        return false;
    };
    memory.is_empty()
        || (memory.starts_with("\n\n") && !BEFORE_MEMORY.iter().any(|at| memory.starts_with(at)))
}

/// Where a note of what a lead opened without comes from.
pub const START_ORIGIN: &str = "sonny.start";

/// What a lead is told of what its window could not use as it opened.
pub fn start_note(warnings: &[String]) -> String {
    let mut note = String::from("Sonny opened without what it could not use:\n");
    for warning in warnings {
        note.push_str(&format!("- {warning}\n"));
    }
    note.push_str("Tell the person, and fix what is yours to fix.");
    note
}

/// Tell `bot` of `warnings` with its next model call: attached, so no
/// turn starts for it, and once a day for the same warnings, however
/// often the window opens.
pub async fn tell_warnings(client: &Client, bot: &str, warnings: &[String]) -> Result<(), String> {
    if warnings.is_empty() {
        return Ok(());
    }
    let note = start_note(warnings);
    let day = crate::home::now_ms() / 86_400_000;
    let key = format!(
        "sonny-start-{}-{day}",
        fingerprint(&format!("{bot}\n{note}"))
    );
    let asked = json!({"bot": bot, "prompt": note, "request_id": key,
        "delivery": "attach", "origin": START_ORIGIN});
    match client.request("submit", asked).await {
        Ok(_) => Ok(()),
        // Told today already.
        Err(error) if error.code == "idempotency_conflict" => Ok(()),
        Err(error) => Err(error.to_string()),
    }
}

/// The window's one line for `warnings`, the first in full; whether its
/// agent is `told` of them all.
pub fn warnings_notice(warnings: &[String], told: bool) -> Option<String> {
    let told = match told {
        true => "; the agent is told",
        false => "",
    };
    match warnings {
        [] => None,
        [one] => Some(format!("{one}{told}")),
        [first, rest @ ..] => Some(format!("{first}; and {} more{told}", rest.len())),
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
    self::read(dir).map(|_| ())
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
    let home = std::env::var_os("HOME").map(PathBuf::from);
    open_in(client, dir, model, pick, home.as_deref()).await
}

/// [`open`], with `home` the home folder.
async fn open_in(
    client: &Client,
    dir: &Path,
    model: Option<&str>,
    pick: &Pick,
    home: Option<&Path>,
) -> Result<Lead, String> {
    let dir = folder_in(&dir.canonicalize().map_err(|e| e.to_string())?, home)?;
    let project = default_name(&dir)?;
    let first = format!("{project}.lead");
    let chosen = match pick {
        Pick::Made { profile, .. } => profile.as_deref(),
        _ => None,
    };
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
        create(client, &dir, model, None, chosen, home).await?;
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
                    "lead_exists: {bot} is there already; /switch {} opens it",
                    lead_short(&bot)
                ));
            }
            // Made by another thread since the look above: theirs.
            if !create(client, &dir, model, Some(&bot), chosen, home).await? {
                return Err(format!(
                    "lead_exists: {bot} is there already; /switch {} opens it",
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
                    "lead_missing: {project} has no lead {name}; /new {name} makes it"
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
        Pick::Made {
            name: Some(name), ..
        } => return make(lead_bot(&project, name)).await,
        Pick::Made { words, .. } => {
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

/// Whether `dir` (canonical) is the home folder itself, where no project
/// is made, as git makes no repository outside one: its `.agents` is the
/// person's own (skills, profiles, memory). A lead made there before
/// still opens; the folders below are folders like any other.
fn is_home(dir: &Path, home: Option<&Path>) -> bool {
    home.is_some_and(|home| home.canonicalize().unwrap_or_else(|_| home.to_owned()) == dir)
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
            let info = read(at)?;
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

/// The agent named `bot`, when the daemon keeps one.
pub async fn find(client: &Client, bot: &str) -> Result<Option<Value>, String> {
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
/// this made it, not found it there already. None is made in `home`.
async fn create(
    client: &Client,
    dir: &Path,
    model: Option<&str>,
    bot: Option<&str>,
    chosen: Option<&str>,
    home: Option<&Path>,
) -> Result<bool, String> {
    if is_home(dir, home) {
        return Err(
            "home_folder: ~ is your home folder, not a project; run sonny in a \
                    project's folder, or set opens to an agent"
                .into(),
        );
    }
    // What it was made without is said where it opens, as each window
    // reviews its instructions.
    let role = role_or_shipped(dir, chosen, &mut Vec::new())?;
    let info = read(dir)?;
    // A project file that names a model keeps it; otherwise asked for,
    // the role's, AGENT_MODEL's, the config's `model`, then the newest
    // agent's, so a bare `sonny` needs no flag. With none, the person picks
    // one, kept as the config's `model` for later projects. Its effort: the
    // config's `[sonny] lead_effort`, else AGENT_EFFORT with AGENT_MODEL's
    // model, as the CLI takes them.
    let (mut effort, model) = (None, model.map(str::to_owned));
    // A config that does not read is said as the window opens; until it
    // reads, a lead is made with the defaults.
    let config = agent_client::config::load().unwrap_or_default();
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
        web_search_off: config.web_search_off,
        ..Default::default()
    };
    let made = make(client, dir, &role, &picks, bot, chosen).await?;
    Ok(made["created"] == true)
}

/// Make the project in `dir` with its lead in `role`, told the task rule;
/// or `bot`, another lead of it. A role `chosen` by name, not the
/// project's, is kept on the lead as its profile.
async fn make(
    client: &Client,
    dir: &Path,
    role: &agent_client::policy::Profile,
    picks: &agent_client::project::Picks,
    bot: Option<&str>,
    chosen: Option<&str>,
) -> Result<Value, String> {
    let instructions = folder(dir, role, &memory(dir)?)?.text;
    // Gated and confined from the start, as a new lead's mode makes it,
    // whoever opens it first.
    let mode = crate::approvals::default();
    let tools: Vec<String> = match &role.tools {
        Some(tools) => tools.clone(),
        // The tools the client will give it; one the gate names that the
        // lead doesn't get is left out there.
        None => {
            let model = picks.model.as_deref().or(role.model.as_deref());
            agent_client::default_tools(
                client.providers(),
                model.unwrap_or(""),
                picks.web_search,
                &picks.web_search_off,
            )
            .into_iter()
            .map(String::from)
            .collect()
        }
    };
    // Writing its own state folder, by the name it is made under.
    let lead = (bot.map(str::to_owned)).or_else(|| {
        let info = read(dir).ok()?;
        info["lead"].as_str().map(str::to_owned)
    });
    let state = lead.as_deref().and_then(crate::state::folder);
    let mut settings = crate::approvals::sandbox(mode, dir, state.as_deref());
    let held = crate::served::Role::LEAD.within(&json!(settings));
    let declare = json!(crate::declared::tools(held));
    let gate = crate::approvals::gate(mode, &crate::approvals::gateable(&tools, &declare));
    if let Some(state) = &state {
        settings.insert("env".into(), json!({crate::state::VAR: state}));
    }
    // As in `create`: a config that does not read is said where the lead
    // opens, and the defaults stand until it reads.
    let config = agent_client::config::load().unwrap_or_default();
    settings.insert("trailer".into(), json!(crate::stamp::trailer(&config)));
    settings.insert("steps_only".into(), crate::exec::steps_only(&config));
    if let Some(ms) = config.hold_ms {
        settings.insert("hold_ms".into(), json!(ms));
    }
    if let Some(outputs) = crate::outputs() {
        settings.insert("outputs".into(), json!(outputs));
    }
    let policy = json!({
        "instructions": instructions,
        "compaction_instructions": agent_client::policy::DEFAULT_COMPACTION_INSTRUCTIONS,
        "model": role.model,
        "tools": role.tools,
        "declare": declare,
        "gate": gate,
        "settings": settings,
        "profile": chosen,
    });
    agent_client::project::create(client, dir, &policy, client.providers(), picks, bot).await
}

/// A `new_project` call of `caller`'s: the project Sonny makes for a
/// folder, on the caller's daemon (`created: false` when it was there
/// already), its lead opened in a workspace of its own.
pub async fn add(caller: &str, model: Option<&str>, arguments: &Value) -> Result<Value, String> {
    let (dir, mut picks) = picks(arguments, model)?;
    if is_home(&dir, std::env::var_os("HOME").as_deref().map(Path::new)) {
        return Err(format!(
            "home_folder: {} is the person's home folder, not a project; name a project's folder",
            dir.display()
        ));
    }
    let config = agent_client::config::load()?;
    if picks.effort.is_none() {
        picks.effort = config.sonny.lead_effort;
    }
    picks.web_search = config.web_search;
    picks.web_search_off = config.web_search_off;
    let socket = std::env::var_os("AGENT_SOCKET")
        .map(PathBuf::from)
        .ok_or("AGENT_SOCKET is not set")?;
    let (client, _events) = Client::connect(&socket)
        .await
        .map_err(|e| format!("{}: {}", e.code, e.detail.unwrap_or_default()))?;
    let mut made = make(&client, &dir, &role(&dir, None)?, &picks, None, None).await;
    if let Ok(made) = &mut made {
        open_new_lead(&client, &socket, caller, made).await;
        made.as_object_mut().map(|m| m.remove("record"));
    }
    client.close().await;
    made
}

/// A lead made for another agent opens in a workspace of its own, a Herdr or
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
    let opened = crate::named::me()
        .map_err(|e| e.to_string())
        .and_then(|me| {
            let bot = format!("{project}.lead");
            crate::open_lead(socket, store.as_deref(), project, &bot, &like, &me)
        });
    match opened {
        Ok((home, _, _)) => made["opened_in"] = json!(home),
        Err(error) => made["not_opened"] = json!(error),
    }
}

/// The folder and picks a `new_project` call names: the caller's model
/// when it names none.
fn picks(
    arguments: &Value,
    model: Option<&str>,
) -> Result<(PathBuf, agent_client::project::Picks), String> {
    let text = |key: &str| {
        arguments[key]
            .as_str()
            .filter(|v| !v.is_empty())
            .map(str::to_owned)
    };
    let folder = text("folder").ok_or("project_invalid: a folder")?;
    let title = (text("title").map(|title| title.trim().to_owned()))
        .filter(|title| !title.is_empty())
        .ok_or("project_invalid: a title, what the person calls the project")?;
    if title.contains(['\n', '\r']) {
        return Err("project_invalid: a title is one line".into());
    }
    let mut picks = agent_client::project::Picks {
        title: Some(title),
        model: text("model").or_else(|| model.map(str::to_owned)),
        effort: text("effort"),
        ..Default::default()
    };
    let (task_model, task_effort) = (text("task_model"), text("task_effort"));
    let task_in = text("task_in");
    let in_project = match task_in.as_deref() {
        None | Some("worktree") => false,
        Some("project") => true,
        Some(other) => return Err(format!("project_invalid: task_in {other:?}")),
    };
    if task_model.is_some() || task_effort.is_some() || task_in.is_some() {
        picks.tasks = Some(agent_client::project::TaskPicks {
            model: task_model,
            effort: task_effort,
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
    // Never Agent's own folder or one in it: its store, keys, rules and
    // settings, which every sandbox keeps read-only, would be the new
    // lead's folder to write.
    let own = std::env::var_os("HOME")
        .map(|home| PathBuf::from(home).join(".agent"))
        .and_then(|own| own.canonicalize().ok());
    if let Some(own) = own.filter(|own| dir.starts_with(own)) {
        return Err(format!(
            "project_invalid: {} is in {}, Agent's own folder",
            dir.display(),
            own.display()
        ));
    }
    Ok((dir, picks))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_lead_is_one_no_agent_made_whatever_its_name() {
        assert!(is_lead_record(&serde_json::json!({"name": "shop.lead"})));
        assert!(is_lead_record(
            &serde_json::json!({"name": "shop.lead.ui", "created_by": null})
        ));
        assert!(!is_lead_record(
            &serde_json::json!({"name": "shop.lead", "created_by": "shop.fix"})
        ));
        assert!(!is_lead_record(&serde_json::json!({"name": "shop.fix"})));
    }

    #[test]
    fn sonny_s_own_bots_share_no_name_with_a_folder_s() {
        let base = internal("reviewer.auto");
        assert!(is_internal(&base) && is_internal(&format!("{base}.4.123.0")));
        // No project, so none of its agents, starts with the mark.
        assert!(!agent_client::project::valid_name(&base));
        assert!(!agent_client::project::valid_name(".reviewer"));
        for bot in [
            "reviewer.auto",
            "shop.lead",
            "approver.auto",
            "a.b.reviewer",
        ] {
            assert!(!is_internal(bot), "{bot}");
        }
        assert_eq!(lead_project(&base), None);
    }

    #[test]
    fn shipped_roles_fit_their_budgets() {
        for (name, text) in BUILTINS {
            let (_, budget) = BUDGETS
                .iter()
                .find(|(n, _)| *n == name)
                .expect("a budget per role");
            // As the profile is read: each loaded skill's body under its
            // header.
            let loaded: usize = agent_client::policy::front_matter(text)
                .0
                .skills
                .iter()
                .flatten()
                .map(|skill| {
                    assert_eq!(skill, "coordination", "a shipped skill {name} loads");
                    let body = agent_client::policy::front_matter(COORDINATION).1.trim();
                    format!("\n\n## Skill: {skill}\n\n{body}").len()
                })
                .sum();
            assert!(
                text.len() + loaded <= *budget,
                "sonny/agents/{name}.md and the skills it loads are {} bytes, over its {budget}: cut as much as you add",
                text.len() + loaded
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
        let picked = |arguments: Value| picks(&arguments, None);
        for bad in [json!({}), json!({"folder": "/", "task_in": "elsewhere"})] {
            assert!(
                picked(bad.clone())
                    .unwrap_err()
                    .starts_with("project_invalid: "),
                "{bad}"
            );
        }
        assert!(
            picked(json!({"folder": "/no/such/folder/here", "title": "Gone"}))
                .unwrap_err()
                .starts_with("project_invalid: ")
        );
        // A project is made with what the person calls it, on one line.
        for bad in [
            json!({"folder": "/"}),
            json!({"folder": "/", "title": "Two\nlines"}),
        ] {
            assert!(
                picked(bad.clone())
                    .unwrap_err()
                    .starts_with("project_invalid: "),
                "{bad}"
            );
        }
        let (_, picks) = picked(
            json!({"folder": "/", "title": " Tally invoices ", "model": "a/b", "task_in": "project"}),
        )
        .unwrap();
        assert_eq!(picks.title.as_deref(), Some("Tally invoices"));
        assert_eq!(picks.model.as_deref(), Some("a/b"));
        assert!(
            picks
                .tasks
                .is_some_and(|t| t.in_project && t.model.is_none())
        );
        // Tasks may take an effort of their own on the lead's model.
        let (_, picks) =
            picked(json!({"folder": "/", "title": "Root", "task_effort": "xhigh"})).unwrap();
        assert!(picks.tasks.is_some_and(|t| t.model.is_none()
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
        assert!(
            role(&dir, None)
                .unwrap_err()
                .starts_with("profile_not_found: ")
        );
        std::fs::write(
            dir.join(".agents/agents/my-lead.md"),
            "---\nextends: lead\n---\nReview each change before it lands.\n",
        )
        .unwrap();
        let lead = role(&dir, None).unwrap();
        assert_eq!(lead.name, "my-lead");
        // The lead's text, the person's or Sonny's, comes first, and the
        // skills it loads come last.
        let skill = "\n\n## Skill: coordination\n\n";
        assert!(
            lead.body
                .contains(&format!("Review each change before it lands.{skill}"))
        );
        let base = profile(&dir, "lead").unwrap().body;
        let (base, coordination) = base.split_once(skill).unwrap();
        assert!(lead.body.starts_with(base));
        assert!(lead.body.ends_with(coordination));
        let text = compose(&dir, None).unwrap().text;
        assert!(text.contains("# Role: my-lead\n\n"), "{text}");
        // A variant of the lead is no role to start a peer in.
        assert!(!text.contains("- my-lead:"));
        // Sonny ships the superlead, which a project opts into the same way.
        std::fs::write(
            dir.join(".agents/project.toml"),
            "name = \"top\"\nprofile = \"superlead\"\n",
        )
        .unwrap();
        let lead = role(&dir, None).unwrap();
        assert!(lead.body.starts_with(base));
        assert!(lead.body.contains("you lead their leads"));
        assert!(lead.body.ends_with(coordination));
        assert!(!compose(&dir, None).unwrap().text.contains("- superlead:"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_lead_opens_without_a_broken_skill_or_role_and_says_so() {
        let dir = std::env::temp_dir().join(format!("sonny-left-out-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join(".agents/skills")).unwrap();
        let dir = dir.canonicalize().unwrap();
        // A skill copied as a link to where it no longer is.
        std::os::unix::fs::symlink("/nowhere/review", dir.join(".agents/skills/review")).unwrap();
        let composed = compose(&dir, None).unwrap();
        assert_eq!(
            composed.left_out,
            [format!(
                "left out of the lead's instructions: cannot read {}: a link to a missing file",
                dir.join(".agents/skills/review").display()
            )]
        );
        assert!(!composed.text.contains("- review:"));
        // A lead.md that cannot be read: the lead runs as Sonny's, and says so.
        std::fs::create_dir_all(dir.join(".agents/agents")).unwrap();
        std::os::unix::fs::symlink("gone.md", dir.join(".agents/agents/lead.md")).unwrap();
        assert!(role(&dir, None).is_err());
        let composed = compose(&dir, None).unwrap();
        assert!(composed.left_out[0].starts_with("the lead's role: instructions_unreadable: "));
        assert!(composed.left_out[0].ends_with("it runs as Sonny's own lead role"));
        assert!(composed.text.contains("# Role: lead\n\n"));
        // One picked by name is the person's to fix.
        assert!(compose(&dir, Some("lead")).is_err());
        // The window says the first, and how many more.
        assert_eq!(
            warnings_notice(&["a".into(), "b".into(), "c".into()], true).as_deref(),
            Some("a; and 2 more; the agent is told")
        );
        assert_eq!(warnings_notice(&["a".into()], false).as_deref(), Some("a"));
        assert!(start_note(&["a".into()]).contains("\n- a\n"));
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
    fn only_the_home_folder_itself_is_home() {
        let base = std::env::temp_dir().join(format!("sonny-is-home-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&base);
        let home = base.join("home");
        std::fs::create_dir_all(home.join("code/app")).unwrap();
        // The home folder by another name is the home folder.
        std::os::unix::fs::symlink(&home, base.join("link")).unwrap();
        let real = home.canonicalize().unwrap();
        assert!(is_home(&real, Some(&home)));
        assert!(is_home(&real, Some(&base.join("link"))));
        // A folder below it is a folder like any other, its own project.
        let app = real.join("code/app");
        assert!(!is_home(&app, Some(&home)));
        assert_eq!(folder_in(&app, Some(&home)).unwrap(), app);
        assert!(!is_home(&real, None));
        let _ = std::fs::remove_dir_all(&base);
    }

    #[tokio::test]
    async fn no_project_is_made_in_the_home_folder_but_a_lead_there_opens() {
        let base = std::env::temp_dir().join(format!("sonny-home-lead-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&base);
        let home = base.join("home");
        std::fs::create_dir_all(&home).unwrap();
        let home = home.canonicalize().unwrap();
        let open = |leads: Value, pick: Pick| {
            let home = home.clone();
            async move {
                let client =
                    crate::tests::daemon_answering(move |asked| match asked["op"].as_str() {
                        Some("bots") => leads.clone(),
                        _ => json!({"error": {"code": "unexpected", "message": "asked"}}),
                    })
                    .await;
                open_in(&client, &home, Some("a/b"), &pick, Some(&home)).await
            }
        };
        // None there: refused, and nothing written in the home folder.
        let error = open(json!({"bots": []}), Pick::Last).await.err().unwrap();
        assert!(
            error.starts_with("home_folder: ~ is your home folder"),
            "{error}"
        );
        assert!(std::fs::read_dir(&home).unwrap().next().is_none());
        // One made there before opens, as it always did.
        let lead = json!({"bots": [{"name": "home.lead", "workspace": home}]});
        let opened = open(lead.clone(), Pick::Last).await.unwrap();
        assert_eq!((opened.bot.as_str(), opened.new), ("home.lead", false));
        // Another lead is not made there either.
        let error = open(lead, Pick::New(Some("fix".into())))
            .await
            .err()
            .unwrap();
        assert!(error.starts_with("home_folder: "), "{error}");
        assert!(std::fs::read_dir(&home).unwrap().next().is_none());
        let _ = std::fs::remove_dir_all(&base);
    }

    #[tokio::test]
    async fn new_project_refuses_the_home_folder() {
        let Ok(home) = std::env::var("HOME") else {
            return;
        };
        // Refused before any daemon is reached.
        let named = json!({"folder": home, "title": "Home"});
        let error = add("work.lead", Some("a/b"), &named).await.unwrap_err();
        assert!(error.starts_with("home_folder: "), "{error}");
        assert!(error.contains("the person's home folder"), "{error}");
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
    fn a_lead_takes_its_folder_s_instructions_only_when_they_changed() {
        let dir = std::env::temp_dir().join(format!("sonny-review-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join(".agents/skills/deploy")).unwrap();
        std::fs::write(
            dir.join(".agents/skills/deploy/SKILL.md"),
            "# Deploy\n\nShip it.",
        )
        .unwrap();
        let (taken, _) = review(&dir, "set by hand", None).unwrap();
        let taken = taken.expect("the folder's, over what was set by hand");
        assert!(taken.contains("- deploy:"));
        assert_eq!(review(&dir, &taken, None).unwrap().0, None);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_lead_takes_a_changed_role_rule_or_skill_between_turns_but_never_its_memory_alone() {
        let base = std::env::temp_dir().join(format!("sonny-changed-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&base);
        let (dir, index) = (base.join("shop"), base.join("memory/MEMORY.md"));
        std::fs::create_dir_all(dir.join(".agents/skills/deploy")).unwrap();
        std::fs::create_dir_all(index.parent().unwrap()).unwrap();
        let write = |path: &Path, text: &str| {
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(path, text).unwrap();
        };
        write(&dir.join("AGENTS.md"), "Run the tests.\n");
        write(
            &dir.join(".agents/skills/deploy/SKILL.md"),
            "# Deploy\n\nShip it.",
        );
        write(&index, "# Memory\n\n- first.md: a fact saved first\n");
        let memory = [index.clone()];
        let renewed = |held: &str| changed_with(&dir, held, None, &memory, None).unwrap().text;
        // Memory is found where composing put it, with the role after it.
        let composed = compose_with(&dir, None, &memory).unwrap();
        let at = composed.memory.clone().expect("memory found");
        assert!(composed.text[at].contains("a fact saved first"));
        let held = renewed("set by hand").expect("the folder's");
        assert_eq!(renewed(&held), None);
        // Facts saved, or none at all, are no change: each would cost the
        // lead's next call the whole prompt cache.
        write(
            &index,
            "# Memory\n\n- first.md: a fact saved first\n- next.md: another\n",
        );
        assert_eq!(renewed(&held), None);
        let without = (changed_with(&dir, "", None, &[], None).unwrap().text).unwrap();
        assert_eq!(renewed(&without), None);
        assert_eq!(
            changed_with(&dir, &held, None, &[], None).unwrap().text,
            None
        );
        // A rule changed is, and it brings the memory there is now.
        write(&dir.join("AGENTS.md"), "Run the tests twice.\n");
        let held = renewed(&held).expect("the rule changed");
        assert!(held.contains("Run the tests twice.") && held.contains("another"));
        assert_eq!(renewed(&held), None);
        // So is the role, which holds the most.
        write(&dir.join(".agents/agents/lead.md"), "Lead the shop.\n");
        let held = renewed(&held).expect("the role changed");
        assert!(held.contains("Lead the shop."));
        // A skill added is, and one gone, last of the list or the list
        // itself, is too: what follows the rest is no memory.
        write(&dir.join(".agents/skills/zeta/SKILL.md"), "# Zeta\n\nLast.");
        let with_zeta = renewed(&held).expect("a skill added");
        std::fs::remove_dir_all(dir.join(".agents/skills/zeta")).unwrap();
        assert!(renewed(&with_zeta).is_some());
        std::fs::remove_dir_all(dir.join(".agents/skills")).unwrap();
        let no_skills = renewed(&with_zeta).expect("the skills gone");
        write(
            &dir.join(".agents/skills/deploy/SKILL.md"),
            "# Deploy\n\nShip it.",
        );
        let skills = renewed(&no_skills).expect("a list again");
        std::fs::remove_dir_all(dir.join(".agents/skills")).unwrap();
        assert!(renewed(&skills).is_some());
        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn near_the_size_limit_memory_that_shortens_the_skill_lists_is_still_no_change() {
        let base = std::env::temp_dir().join(format!("sonny-changed-full-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&base);
        let (dir, index) = (base.join("shop"), base.join("memory/MEMORY.md"));
        let write = |path: &Path, text: &str| {
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(path, text).unwrap();
        };
        for n in 0..6 {
            let about = format!(
                "Use this skill number {n} when {}",
                "the subject comes up ".repeat(8)
            );
            let skill = format!("---\nname: skill{n}\ndescription: {about}\n---\nDo it.");
            write(
                &dir.join(format!(".agents/skills/skill{n}/SKILL.md")),
                &skill,
            );
        }
        write(&dir.join(".agents/agents/lead.md"), "Lead the shop.\n");
        write(&index, "# Memory\n\n- first.md: a fact saved first\n");
        let memory = [index.clone()];
        // Rules that leave the lists a little room.
        let rule = |fill: usize| format!("Run the tests.\n{}\n", "x".repeat(fill));
        write(&dir.join("AGENTS.md"), &rule(1));
        let small = compose_with(&dir, None, &memory).unwrap().text.len();
        let fill = agent_client::policy::MAX_INSTRUCTIONS - small - 200;
        write(&dir.join("AGENTS.md"), &rule(fill));
        let first = changed_with(&dir, "set by hand", None, &memory, None).unwrap();
        let held = first.text.expect("the folder's");
        assert!(
            held.contains(&"the subject comes up ".repeat(5)),
            "summaries at their longest"
        );
        // Facts saved take the room the lists had: their summaries shorten.
        let facts: String = (0..40)
            .map(|n| format!("- fact{n}.md: a fact saved later\n"))
            .collect();
        write(
            &index,
            &format!("# Memory\n\n- first.md: a fact saved first\n{facts}"),
        );
        let now = compose_with(&dir, None, &memory).unwrap().text;
        assert!(
            !now.contains(&"the subject comes up ".repeat(5)),
            "the lists shortened"
        );
        // Read only around its memory, that would look like a change...
        let blind = changed_with(&dir, &held, None, &memory, None).unwrap();
        assert!(blind.text.is_some());
        // ...but the lead's last look knows the rest is as it was.
        let looked = changed_with(&dir, &held, None, &memory, Some(&first.known)).unwrap();
        assert_eq!(looked.text, None);
        assert_eq!(looked.known, first.known);
        // A rule changed is still a change.
        write(
            &dir.join("AGENTS.md"),
            &rule(fill).replace("Run the tests.", "Run the tests!"),
        );
        let edited = changed_with(&dir, &held, None, &memory, Some(&looked.known)).unwrap();
        let taken = edited.text.expect("the rule changed");
        assert!(taken.contains("Run the tests!"));
        // And what it takes is what the next look knows it holds.
        let next = changed_with(&dir, &taken, None, &memory, Some(&edited.known)).unwrap();
        assert_eq!(next.text, None);
        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn the_project_file_keeps_its_other_lines_when_the_model_moves() {
        let file =
            "# mine\nname = \"shop\"\nmodel = \"openai/a\"   # pinned\ntask_in = \"project\"\n";
        assert_eq!(
            with_model(file, "anthropic/b", Some("high")),
            "# mine\nname = \"shop\"\nmodel = \"anthropic/b\"\neffort = \"high\"\ntask_in = \"project\"\n"
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
