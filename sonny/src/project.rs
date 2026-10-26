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

/// A lead's instructions as the app composes them for `dir` now (its role,
/// the folder's rules, skills and memory, then the project's task rule),
/// and, when the person reads it inside Rex, where they are.
pub fn instructions(dir: &Path, rex: Option<&str>) -> Result<String, String> {
    let memory = agent_client::policy::memory_indexes(dir).map_err(|e| e.to_string())?;
    let composed = agent_client::policy::instructions(dir, Some(&role(dir)?), &memory)
        .map_err(|e| e.to_string())?
        .text;
    let rule = agent_client::project::rule(&agent_client::project::read(dir)?);
    let mut text = format!("{composed}\n\n{rule}");
    if let Some(session) = rex {
        text.push_str(&format!(
            "\n\n# Rex\n\nThe person reads you in Sonny, inside Rex, the terminal \
             multiplexer, in its session `{session}`. Each task you start opens there \
             as a tab while it works, so they can watch it. Read the rex-windows skill \
             to run a long command where they can see it, or to place a task's window \
             where they ask."
        ));
    }
    Ok(text)
}

/// The lead for `dir`, made first when the folder is not a project yet.
pub async fn open(
    client: &Client,
    dir: &Path,
    socket: &Path,
    model: Option<&str>,
) -> Result<Lead, String> {
    let dir = dir.canonicalize().map_err(|e| e.to_string())?;
    // Inside a project, its lead, wherever below the project's folder.
    if let Some(project) = agent_client::policy::project(&dir).map_err(|e| e.to_string())? {
        let bot = format!("{project}.lead");
        if find(client, &bot).await?.is_some() {
            return Ok(Lead {
                project,
                bot,
                new: false,
            });
        }
    }
    let project = default_name(&dir)?;
    let bot = format!("{project}.lead");
    if let Some(found) = find(client, &bot).await? {
        if found["workspace"].as_str() == dir.to_str() {
            return Ok(Lead {
                project,
                bot,
                new: false,
            });
        }
        return Err(format!(
            "project_exists: {bot} works in {}; open sonny there or pass --bot",
            found["workspace"].as_str().unwrap_or("another folder")
        ));
    }
    create(client, &dir, socket, &bot, model).await?;
    Ok(Lead {
        project,
        bot,
        new: true,
    })
}

async fn find(client: &Client, bot: &str) -> Result<Option<Value>, String> {
    let page = client
        .request("bots", json!({"name": bot, "limit": 1}))
        .await
        .map_err(|e| e.to_string())?;
    Ok(page["bots"].get(0).filter(|b| b["name"] == bot).cloned())
}

/// Where Sonny keeps the model it last ran on.
fn remembered_path() -> Option<PathBuf> {
    std::env::var_os("HOME").map(|home| PathBuf::from(home).join(".agent/sonny/model"))
}

/// The model Sonny last ran on, kept for the next folder it opens.
pub fn remembered() -> Option<String> {
    let text = std::fs::read_to_string(remembered_path()?).ok()?;
    Some(text.trim().to_owned()).filter(|m| m.contains('/'))
}

pub fn remember(model: &str) {
    if let Some(path) = remembered_path()
        && remembered().as_deref() != Some(model)
    {
        if let Some(dir) = path.parent() {
            let _ = std::fs::create_dir_all(dir);
        }
        let _ = std::fs::write(path, format!("{model}\n"));
    }
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
    // Asked for, the project file's, the role's, the last Sonny ran on, then
    // the newest agent's, so a bare `sonny` needs no flag.
    let model = match model
        .map(str::to_owned)
        .or(chosen)
        .or_else(|| role.model.clone())
    {
        Some(model) => model,
        None => match std::env::var("AGENT_MODEL").ok().or_else(remembered) {
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
                "instructions": instructions, "tools": tools}),
        )
        .await
        .map_err(|e| e.to_string())?;
    Ok(())
}
