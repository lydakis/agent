//! The folder's coordinator: opened with no `--bot`, Sonny talks to the
//! project's lead, `<project>.lead`, the same bot the app opens for that
//! folder, so the two clients share one conversation. A folder that is not
//! a project yet becomes one the way the app makes it, through the app's
//! `~/.agent/project add` when the app is installed.
use agent_client::Client;
use serde_json::{Value, json};
use std::path::{Path, PathBuf};

/// The coordinator profile the app ships, used when neither the folder nor
/// the user has a `coordinator.md` and the app's script is not installed.
const COORDINATOR: &str = include_str!("../../app/agents/coordinator.md");
const TOOLS: [&str; 6] = ["shell", "read", "write", "edit", "wait", "history"];

pub struct Lead {
    pub project: String,
    pub bot: String,
    /// Made just now, for this folder.
    pub new: bool,
    /// The folder it is brought back to: it was moved elsewhere, and the
    /// next message it is sent moves it here.
    pub moving: Option<PathBuf>,
}

/// The folder's own name reduced to name characters, as the app names a
/// project with no file.
fn default_name(dir: &Path) -> Result<String, String> {
    let info = agent_client::project::read(dir)?;
    Ok(info["name"].as_str().unwrap_or("project").to_owned())
}

/// The coordinator's role: the folder's or the user's `coordinator.md`,
/// else the app's.
fn role(dir: &Path) -> Result<agent_client::policy::Profile, String> {
    Ok(
        match agent_client::policy::profile(dir, "coordinator").map_err(|e| e.to_string())? {
            Some(role) => role,
            None => agent_client::policy::Profile::parse("coordinator", None, COORDINATOR),
        },
    )
}

/// A lead's instructions as the app composes them for `dir` now: its role,
/// the folder's rules, skills and memory, then the project's task rule
/// with the copies Sonny is set to. Rex is the rex-windows skill's to
/// explain, so they read the same in any client.
pub fn instructions(dir: &Path) -> Result<String, String> {
    let memory = agent_client::policy::memory_indexes(dir).map_err(|e| e.to_string())?;
    let composed = agent_client::policy::instructions(dir, Some(&role(dir)?), &memory)
        .map_err(|e| e.to_string())?
        .text;
    let info = agent_client::project::read(dir)?;
    let copies = crate::settings::Settings::load().copies;
    let rule = agent_client::project::rule(&info, &copies);
    let me = std::env::current_exe().map_err(|e| e.to_string())?;
    let start = start_rule(&me.to_string_lossy());
    Ok(format!("{composed}\n\n{rule}\n\n{start}"))
}

/// How the lead starts a task in Sonny: `sonny task`, at `sonny`, makes
/// its folder the way the person set Sonny up and starts it there.
fn start_rule(sonny: &str) -> String {
    let sonny = format!("\"{sonny}\"");
    format!(
        "Wherever these instructions start a new task with \"$AGENT_BIN\" run --detach --new, in a worktree or not, start it with Sonny instead, which makes its folder first; \
         messages to tasks that exist still go through \"$AGENT_BIN\" run, as above. Pick a NAME that \"$AGENT_BIN\" ls --name \"${{AGENT_BOT%%.*}}.*\" does not list yet, \
         that starts with your own name before .lead and a dot, and that is a valid git branch name; from this folder run\n\
         {sonny} task NAME --agents --setup MODEL-FLAGS -- TASK\n\
         with the task rule's --model and --effort as MODEL-FLAGS: \
         it makes the task's folder the way the person set Sonny up, starts the task there and prints the turn's handle, as run --detach does. \
         Pass --profile ROLE in place of --agents when a listed role fits, and leave out --setup for a task that only reads, \
         so the folder's .agents/setup runs only for a task that will change files. \
         It also says how the task's work comes back, which goes in the brief: on its branch agent/NAME, here already for a worktree, \
         or brought here with the git push it names for a sandfs copy of a git repository; \
         for a sandfs copy of any other folder, {sonny} task changes NAME lists what the task changed, \
         and once you have checked that, {sonny} task changes NAME --apply writes it here. \
         A start that fails leaves no folder behind; if it fails again, tell the person why instead of working around it."
    )
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
    /// are kept until the person asks for the folder's.
    Kept,
}

/// Whether a lead's instructions may follow the folder: only when Sonny
/// wrote what they hold (or saw a client compose it), so instructions the
/// person set some other way are never overwritten.
pub fn review(dir: &Path, bot: &Value, lead: &str, new: bool) -> Result<Instructions, String> {
    let composed = instructions(dir)?;
    let current = bot["instructions"].as_str().unwrap_or_default();
    Ok(if current == composed {
        wrote(lead, current);
        Instructions::Current
    } else if new || written(lead).is_some_and(|key| key == fingerprint(current)) {
        Instructions::Stale(composed)
    } else {
        Instructions::Kept
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
fn fingerprint(text: &str) -> String {
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
        || said("reasoning") != effort
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

/// `text` with its top-level `model` and `reasoning` set (or `reasoning`
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
            Some("reasoning") => {
                placed = true;
                if let Some(level) = effort {
                    lines.push(format!("reasoning = {}", quote(level)));
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
        lines.insert(at + 1, format!("reasoning = {}", quote(level)));
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
pub async fn open(
    client: &Client,
    dir: &Path,
    socket: &Path,
    model: Option<&str>,
) -> Result<Lead, String> {
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
    create(client, &dir, socket, &bot, model).await?;
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

async fn create(
    client: &Client,
    dir: &Path,
    socket: &Path,
    bot: &str,
    model: Option<&str>,
) -> Result<(), String> {
    let role = role(dir)?;
    let info = agent_client::project::read(dir)?;
    let chosen = info["model"].as_str().map(str::to_owned);
    // The project file's effort goes with its model.
    let effort = match model {
        None if chosen.is_some() => info["reasoning"].as_str().map(str::to_owned),
        _ => None,
    };
    // Asked for, the project file's, the role's, AGENT_MODEL's, then the
    // newest agent's, so a bare `sonny` needs no flag.
    let model = match model
        .map(str::to_owned)
        .or(chosen)
        .or_else(|| role.model.clone())
    {
        Some(model) => model,
        None => match std::env::var("AGENT_MODEL").ok() {
            Some(model) => model,
            None => newest(client)
                .await?
                .ok_or("model_required: no agent has run yet; pass --model PROVIDER/MODEL once")?,
        },
    };
    // The app's own way, file and all, when the app is installed.
    let script = std::env::var_os("HOME").map(|home| PathBuf::from(home).join(".agent/project"));
    if let Some(script) = script.filter(|s| s.is_file()) {
        let mut command = std::process::Command::new(script);
        command.arg("add").arg(dir).env("AGENT_SOCKET", socket);
        command.args(["--model", &model]);
        let out = command.output().map_err(|e| e.to_string())?;
        if !out.status.success() {
            return Err(String::from_utf8_lossy(&out.stderr).trim().to_owned());
        }
        return Ok(());
    }
    // Without the app: the same role and folder policy, no project file.
    let memory = agent_client::policy::memory_indexes(dir).map_err(|e| e.to_string())?;
    let instructions = agent_client::policy::instructions(dir, Some(&role), &memory)
        .map_err(|e| e.to_string())?
        .text;
    let tools: Vec<String> = role
        .tools
        .clone()
        .unwrap_or_else(|| TOOLS.iter().map(|t| t.to_string()).collect());
    client
        .request(
            "create",
            json!({"bot": bot, "workspace": dir, "model": model, "effort": effort,
                "instructions": instructions, "tools": tools,
                "compaction_instructions": agent_client::policy::DEFAULT_COMPACTION_INSTRUCTIONS}),
        )
        .await
        .map_err(|e| e.to_string())?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_lead_starts_its_tasks_with_sonny() {
        let text = start_rule("/opt/sonny");
        for said in [
            "\"/opt/sonny\" task NAME --agents --setup MODEL-FLAGS -- TASK\n",
            "Wherever these instructions start a new task with \"$AGENT_BIN\" run --detach --new, in a worktree or not, start it with Sonny instead",
            "leave out --setup for a task that only reads",
            "Pick a NAME that \"$AGENT_BIN\" ls --name \"${AGENT_BOT%%.*}.*\" does not list yet",
            "\"/opt/sonny\" task changes NAME --apply writes it here",
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
            "# mine\nname = \"shop\"\nmodel = \"anthropic/b\"\nreasoning = \"high\"\nthreads_in = \"project\"\n"
        );
        let file = "name = \"shop\"\nreasoning = \"low\"\nmodel = \"openai/a\"\n";
        assert_eq!(
            with_model(file, "openai/a", Some("high")),
            "name = \"shop\"\nreasoning = \"high\"\nmodel = \"openai/a\"\n"
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
