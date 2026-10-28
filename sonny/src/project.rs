//! The folder's coordinator: opened with no `--bot`, Sonny talks to the
//! project's lead, `<project>.lead`, the same bot whichever Sonny opens
//! that folder. A folder that is not
//! a project yet becomes one: its lead, then its `.agents/project.toml`.
//! `sonny project add`, which `~/.agent/project` runs, makes one from an
//! agent's shell the same way.
use agent_client::Client;
use serde_json::{Value, json};
use std::path::{Path, PathBuf};

/// The lead's role as Sonny ships it, which Sonny writes to the person's
/// `~/.agents/agents/lead.md` to edit; used when neither the folder nor
/// the person has a `lead.md`.
pub const LEAD: &str = include_str!("../agents/lead.md");
pub const TOOLS: [&str; 6] = ["shell", "read", "write", "edit", "wait", "history"];

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
/// the folder's rules, skills and memory, then the project's task rule
/// with the copies Sonny is set to. Rex is the rex-windows skill's to
/// explain, so they read the same in any client.
pub fn instructions(dir: &Path) -> Result<String, String> {
    compose(dir).map(|(text, _)| text)
}

/// The instructions and the names of the skills they list.
fn compose(dir: &Path) -> Result<(String, Vec<String>), String> {
    let (composed, skills) = folder(dir, &role(dir)?)?;
    let info = agent_client::project::read(dir)?;
    let copies = crate::settings::Settings::load().copies;
    let rule = agent_client::project::rule(&info, &copies);
    Ok((format!("{composed}\n\n{rule}\n\n{}", start()?), skills))
}

/// The role with the folder's rules, skills and memory and who it works
/// for, before the project's task rule, and the names of the skills they
/// list.
fn folder(
    dir: &Path,
    role: &agent_client::policy::Profile,
) -> Result<(String, Vec<String>), String> {
    let memory = agent_client::policy::memory_indexes(dir).map_err(|e| e.to_string())?;
    let composed =
        agent_client::policy::instructions(dir, Some(role), &memory).map_err(|e| e.to_string())?;
    let skills = composed
        .skills
        .into_iter()
        .map(|skill| skill.name)
        .collect();
    Ok((composed.text, skills))
}

/// What an agent a trigger starts in `dir` is told: the folder's rules,
/// skills and memory, in no role, then what every task is told.
pub fn plain(dir: &Path) -> Result<String, String> {
    let memory = agent_client::policy::memory_indexes(dir).map_err(|e| e.to_string())?;
    let composed = agent_client::policy::instructions(dir, None, &memory)
        .map_err(|e| format!("{}: {e}", e.code()))?;
    as_task(dir, &composed.text)
}

/// This Sonny's rule for starting a task, which names this binary, so it
/// is composed rather than kept in the lead's file. How to ask the person
/// is the `ask` tool's to say.
fn start() -> Result<String, String> {
    let me = std::env::current_exe().map_err(|e| e.to_string())?;
    Ok(start_rule(&me.to_string_lossy()))
}

/// How the lead starts a task in Sonny: `sonny task`, at `sonny`, makes
/// its folder the way the person set Sonny up and starts it there.
fn start_rule(sonny: &str) -> String {
    let sonny = format!("\"{sonny}\"");
    agent_client::policy::with_cli(&format!(
        "Wherever these instructions start a new task with \"$AGENT_BIN\" run --detach --new, in a worktree or not, start it with Sonny instead, which makes its folder first; \
         messages to tasks that exist still go through \"$AGENT_BIN\" run, as above. Pick a NAME that \"$AGENT_BIN\" ls --name \"${{AGENT_BOT%%.*}}.*\" does not list yet, \
         that starts with your own name before .lead and a dot, and that is a valid git branch name; from this folder run\n\
         {sonny} task NAME --agents --setup -- TASK\n\
         which picks the task's model and effort as the person set them up (add --model or --effort only for one the person asks for): \
         it makes the task's folder the way the person set Sonny up, starts the task there and prints the turn's handle, as run --detach does. \
         Pass --profile ROLE in place of --agents when a listed role fits, and leave out --setup for a task that only reads, \
         so the folder's .agents/setup runs only for a task that will change files. \
         It also says how the task's work comes back, which goes in the brief: on its branch agent/NAME, here already for a worktree, \
         or brought here with the git push it names for a sandfs copy of a git repository; \
         for a sandfs copy of any other folder, {sonny} task changes NAME lists what the task changed, \
         and once you have checked that, {sonny} task changes NAME --apply writes it here. \
         A task works in its own folder only, and each file has one writer: Sonny turns paths in this folder named in the brief into the task's, \
         and never copy a task's files into this folder yourself (cp, a redirect, tee), which can write zeros over the files there; \
         take its work back only as above. \
         A start that fails leaves no folder behind; if it fails again, tell the person why instead of working around it."
    ))
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
    let root = match agent_client::policy::project_root(dir) {
        Ok(Some((_, root))) => root,
        _ => dir.to_owned(),
    };
    default_name(&root).is_ok_and(|found| found == name)
}

/// The lead for `dir`, made first when the folder is not a project yet.
pub async fn open(client: &Client, dir: &Path, model: Option<&str>) -> Result<Lead, String> {
    let mut dir = dir.canonicalize().map_err(|e| e.to_string())?;
    // Inside a project, wherever below its folder (or in a worktree of
    // it), its lead, which works in that folder: made there first when
    // missing, and brought back when moved.
    if let Some((_, root)) = agent_client::policy::project_root(&dir).map_err(|e| e.to_string())? {
        dir = root;
    }
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
        });
    }
    create(client, &dir, model).await?;
    Ok(Lead {
        project,
        bot,
        new: true,
        moving: None,
    })
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
    // the role's, AGENT_MODEL's, then the newest agent's, so a bare
    // `sonny` needs no flag. Its effort: SONNY_LEAD_EFFORT, else
    // AGENT_EFFORT with AGENT_MODEL's model, as the CLI takes them.
    let (mut effort, model) = (None, model.map(str::to_owned));
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
            None => newest(client)
                .await?
                .ok_or("model_required: no agent has run yet; pass --model PROVIDER/MODEL once")?,
        },
    };
    let picks = agent_client::project::Picks {
        model: Some(model),
        effort: crate::setting(crate::LEAD_EFFORT).or(effort.filter(|e| !e.is_empty())),
        ..Default::default()
    };
    make(client, dir, &role, &picks).await.map(|_| ())
}

/// Make the project in `dir` with its lead in `role`, told the task rule
/// with the copies Sonny is set to and how Sonny starts tasks.
async fn make(
    client: &Client,
    dir: &Path,
    role: &agent_client::policy::Profile,
    picks: &agent_client::project::Picks,
) -> Result<Value, String> {
    let (instructions, _) = folder(dir, role)?;
    let policy = json!({
        "instructions": instructions,
        "compaction_instructions": agent_client::policy::DEFAULT_COMPACTION_INSTRUCTIONS,
        "model": role.model,
        "tools": role.tools,
        "declare": crate::declared::tools(),
    });
    let copies = crate::settings::Settings::load().copies;
    agent_client::project::create(
        client,
        dir,
        &policy,
        &json!(TOOLS),
        picks,
        &copies,
        Some(&start()?),
    )
    .await
}

const USAGE: &str = "usage: project add DIR [--model PROVIDER/MODEL] [--effort LEVEL] [--threads-model PROVIDER/MODEL] [--threads-effort LEVEL] [--threads-in worktree|project]";

/// What `~/.agent/project` says it takes.
pub fn usage() -> &'static str {
    USAGE
}

/// `sonny project add`, as `~/.agent/project` runs it from an agent's
/// shell: the project Sonny makes for a folder, on that shell's daemon,
/// printed as one JSON line (`created: false` when it was there already),
/// or an `{"error": CODE, "detail": ...}` line on stderr and exit 1.
pub fn cli(args: &[String]) -> i32 {
    let made = add_args(args).and_then(|(dir, picks)| {
        let socket = daemon_socket()?;
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .map_err(|e| e.to_string())?
            .block_on(async {
                let (client, _events) = Client::connect(&socket)
                    .await
                    .map_err(|e| format!("{}: {}", e.code, e.detail.unwrap_or_default()))?;
                let mut picks = picks;
                picks.effort = picks.effort.or_else(|| crate::setting(crate::LEAD_EFFORT));
                let mut made = make(&client, &dir, &role(&dir)?, &picks).await;
                if let Ok(made) = &mut made {
                    open_new_lead(&client, &socket, made).await;
                }
                client.close().await;
                made
            })
    });
    match made {
        Ok(mut made) => {
            made.as_object_mut().map(|m| m.remove("record"));
            println!("{made}");
            0
        }
        Err(error) => {
            eprintln!("{}", error_json(&error));
            1
        }
    }
}

/// A lead made from another agent's shell opens in a home of its own, a
/// Herdr workspace or a Rex session beside the caller's, as the person
/// would otherwise ask; `opened_in` says where, or `not_opened` why not.
async fn open_new_lead(client: &Client, socket: &Path, made: &mut Value) {
    let (Ok(caller), Some(project)) = (std::env::var("AGENT_BOT"), made["project"].as_str()) else {
        return;
    };
    if made["created"] != true {
        return;
    }
    let like = crate::project_of(client, &caller).await;
    let store = std::env::var_os("AGENT_STORE").map(PathBuf::from);
    let opened = std::env::current_exe()
        .map_err(|e| e.to_string())
        .and_then(|me| crate::open_lead(socket, store.as_deref(), project, &like, None, &me));
    match opened {
        Ok((home, _)) => made["opened_in"] = json!(home),
        Err(error) => made["not_opened"] = json!(error),
    }
}

/// The folder and picks `project add` was given.
fn add_args(args: &[String]) -> Result<(PathBuf, agent_client::project::Picks), String> {
    let usage = || USAGE.to_owned();
    let (Some("add"), Some(dir)) = (args.first().map(String::as_str), args.get(1)) else {
        return Err(usage());
    };
    let mut picks = agent_client::project::Picks::default();
    let (mut threads_model, mut threads_effort, mut threads_in) = (None, None, None);
    let mut rest = args[2..].iter();
    while let Some(flag) = rest.next() {
        let value = rest
            .next()
            .filter(|v| !v.is_empty())
            .ok_or_else(usage)?
            .clone();
        let slot = match flag.as_str() {
            "--model" => &mut picks.model,
            "--effort" => &mut picks.effort,
            "--threads-model" => &mut threads_model,
            "--threads-effort" => &mut threads_effort,
            "--threads-in" => &mut threads_in,
            _ => return Err(usage()),
        };
        if slot.replace(value).is_some() {
            return Err(usage());
        }
    }
    let in_project = match threads_in.as_deref() {
        None | Some("worktree") => false,
        Some("project") => true,
        Some(_) => return Err(usage()),
    };
    if threads_model.is_some() || threads_effort.is_some() || threads_in.is_some() {
        picks.threads = Some(agent_client::project::ThreadPicks {
            model: threads_model,
            effort: threads_effort,
            in_project,
        });
    }
    let dir = Path::new(dir)
        .canonicalize()
        .map_err(|e| format!("project_invalid: {dir}: {e}"))?;
    if !dir.is_dir() {
        return Err(format!(
            "project_invalid: {} is not a folder",
            dir.display()
        ));
    }
    Ok((dir, picks))
}

/// The daemon of the shell this runs in, found as the CLI finds it.
fn daemon_socket() -> Result<PathBuf, String> {
    if let Some(socket) = std::env::var_os("AGENT_SOCKET").filter(|s| !s.is_empty()) {
        return Ok(PathBuf::from(socket));
    }
    let store = match std::env::var_os("AGENT_STORE").filter(|s| !s.is_empty()) {
        Some(store) => PathBuf::from(store),
        None => PathBuf::from(
            std::env::var_os("HOME")
                .ok_or("no AGENT_SOCKET, AGENT_STORE or HOME to find the daemon")?,
        )
        .join(".agent/state.sqlite"),
    };
    agent_client::socket::default_socket(&store).map_err(|e| e.to_string())
}

/// One error shape, `{"error": CODE, "detail": ...}`, from `CODE: detail`.
fn error_json(message: &str) -> Value {
    match message.split_once(": ") {
        Some((code, detail))
            if !code.is_empty() && code.bytes().all(|b| b.is_ascii_lowercase() || b == b'_') =>
        {
            json!({"error": code, "detail": detail})
        }
        _ => json!({"error": "project_failed", "detail": message}),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn project_add_reads_its_flags_and_refuses_others() {
        let run = |args: &[&str]| add_args(&args.iter().map(|a| a.to_string()).collect::<Vec<_>>());
        for bad in [
            &["add"][..],
            &["rm", "/x"],
            &["add", "/x", "--model"],
            &["add", "/x", "--color", "red"],
            &["add", "/x", "--threads-in", "elsewhere"],
            &["add", "/x", "--model", "a", "--model", "b"],
        ] {
            assert!(run(bad).unwrap_err().starts_with("usage: "), "{bad:?}");
        }
        assert!(
            run(&["add", "/no/such/folder/here"])
                .unwrap_err()
                .starts_with("project_invalid: ")
        );
        let (_, picks) = run(&["add", "/", "--model", "a/b", "--threads-in", "project"]).unwrap();
        assert_eq!(picks.model.as_deref(), Some("a/b"));
        assert!(
            picks
                .threads
                .is_some_and(|t| t.in_project && t.model.is_none())
        );
        // Tasks may take an effort of their own on the lead's model.
        let (_, picks) = run(&["add", "/", "--threads-effort", "xhigh"]).unwrap();
        assert!(picks.threads.is_some_and(|t| t.model.is_none()
            && t.effort.as_deref() == Some("xhigh")
            && !t.in_project));
        assert_eq!(error_json("project_exists: x")["error"], "project_exists");
    }

    #[test]
    fn a_lead_starts_its_tasks_with_sonny() {
        let text = start_rule("/opt/sonny");
        for said in [
            "\"/opt/sonny\" task NAME --agents --setup -- TASK\n",
            "which picks the task's model and effort as the person set them up",
            "Wherever these instructions start a new task with \"$AGENT_BIN\" run --detach --new, in a worktree or not, start it with Sonny instead",
            "leave out --setup for a task that only reads",
            "Pick a NAME that \"$AGENT_BIN\" ls --name \"${AGENT_BOT%%.*}.*\" does not list yet",
            "\"/opt/sonny\" task changes NAME --apply writes it here",
            "never copy a task's files into this folder yourself",
        ] {
            assert!(text.contains(said), "{said} in {text}");
        }
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
