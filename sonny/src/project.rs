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
}

/// The folder's own name reduced to name characters, as the app names a
/// project with no file.
fn default_name(dir: &Path) -> String {
    let base = dir.file_name().and_then(|n| n.to_str()).unwrap_or("");
    let name: String = base
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '-' || c == '_' {
                c
            } else {
                '-'
            }
        })
        .collect();
    let name = name.trim_matches('-');
    if name.is_empty() {
        "project".into()
    } else {
        name[..name.len().min(64)].into()
    }
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
            return Ok(Lead { project, bot });
        }
    }
    let project = default_name(&dir);
    let bot = format!("{project}.lead");
    if let Some(found) = find(client, &bot).await? {
        if found["workspace"].as_str() == dir.to_str() {
            return Ok(Lead { project, bot });
        }
        return Err(format!(
            "project_exists: {bot} works in {}; open sonny there or pass --bot",
            found["workspace"].as_str().unwrap_or("another folder")
        ));
    }
    create(client, &dir, socket, &bot, model).await?;
    Ok(Lead { project, bot })
}

async fn find(client: &Client, bot: &str) -> Result<Option<Value>, String> {
    let page = client
        .request("bots", json!({"name": bot, "limit": 1}))
        .await
        .map_err(|e| e.to_string())?;
    Ok(page["bots"].get(0).filter(|b| b["name"] == bot).cloned())
}

async fn create(
    client: &Client,
    dir: &Path,
    socket: &Path,
    bot: &str,
    model: Option<&str>,
) -> Result<(), String> {
    // The app's own way, file and all, when the app is installed.
    let script = std::env::var_os("HOME").map(|home| PathBuf::from(home).join(".agent/project"));
    if let Some(script) = script.filter(|s| s.is_file()) {
        let mut command = std::process::Command::new(script);
        command.arg("add").arg(dir).env("AGENT_SOCKET", socket);
        if let Some(model) = model {
            command.args(["--model", model]);
        }
        let out = command.output().map_err(|e| e.to_string())?;
        if !out.status.success() {
            return Err(String::from_utf8_lossy(&out.stderr).trim().to_owned());
        }
        return Ok(());
    }
    // Without the app: the same role and folder policy, no project file.
    let role = match agent_client::policy::profile(dir, "coordinator").map_err(|e| e.to_string())? {
        Some(role) => role,
        None => agent_client::policy::Profile::parse("coordinator", None, COORDINATOR),
    };
    let model = model
        .map(str::to_owned)
        .or_else(|| role.model.clone())
        .or_else(|| std::env::var("AGENT_MODEL").ok())
        .ok_or("model_required: pass --model PROVIDER/MODEL for the new project's coordinator")?;
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
            json!({"bot": bot, "workspace": dir, "model": model,
                "instructions": instructions, "tools": tools}),
        )
        .await
        .map_err(|e| e.to_string())?;
    Ok(())
}
