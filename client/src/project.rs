//! A project is a folder, its coordinator bot `NAME.lead`, and
//! `.agents/project.toml` in that folder, which holds mechanics only: the
//! name, the coordinator's model and effort level, and its tasks' model and
//! effort and whether they work in their own worktrees or the project
//! folder. Clients read it (the app writes it when it makes a project); the
//! daemon knows nothing of projects.
use serde_json::{Value, json};
use std::io::Read;
use std::path::Path;

pub const FILE: &str = ".agents/project.toml";
/// The largest project file the reader takes.
pub const LIMIT: u64 = 64 * 1024;
/// Every key the file may hold; anything else is a mistake, not ignored.
const KEYS: [&str; 7] = [
    "name",
    "coordinator",
    "model",
    "reasoning",
    "threads_model",
    "threads_reasoning",
    "threads_in",
];
/// Where a project's threads work: each in its own worktree, or all in
/// the project folder.
pub const THREADS_IN: [&str; 2] = ["worktree", "project"];

/// A name that is also a bot-name prefix: the daemon's name characters,
/// short enough that `NAME.lead` and its task names fit.
pub fn valid_name(name: &str) -> bool {
    (1..=64).contains(&name.len())
        && name
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"-_.".contains(&b))
        && !name.starts_with('.')
        && !name.ends_with('.')
}

/// The folder's own name, reduced to name characters.
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
    let name = &name[..name.len().min(64)];
    if name.is_empty() {
        "project".into()
    } else {
        name.into()
    }
}

/// The project in `dir` (a canonical directory): the file's fields when it
/// exists, or the defaults a new project there would take. `file` says which.
pub fn read(dir: &Path) -> Result<Value, String> {
    let path = dir.join(FILE);
    let invalid = |reason: &str| format!("project_invalid: {}: {reason}", path.display());
    let text = match std::fs::File::open(&path) {
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => None,
        Err(error) => return Err(format!("project_unreadable: {}: {error}", path.display())),
        Ok(file) => {
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
                .map_err(|e| format!("project_unreadable: {}: {e}", path.display()))?;
            if text.len() as u64 > LIMIT {
                return Err(invalid("larger than 64 KiB"));
            }
            Some(text)
        }
    };
    let Some(text) = text else {
        let name = default_name(dir);
        return Ok(json!({
            "dir": dir, "name": name, "coordinator": format!("{name}.lead"),
            "model": null, "reasoning": null, "threads_model": null,
            "threads_reasoning": null, "threads_in": "worktree", "file": false,
        }));
    };
    let table: toml::Table = text
        .parse()
        .map_err(|e: toml::de::Error| invalid(e.message()))?;
    if let Some(key) = table.keys().find(|key| !KEYS.contains(&key.as_str())) {
        return Err(invalid(&format!("unknown key {key}")));
    }
    let field = |key: &str| match table.get(key) {
        None => Ok(None),
        Some(toml::Value::String(value)) => Ok(Some(value.clone())),
        Some(_) => Err(invalid(&format!("{key} must be a string"))),
    };
    let name = field("name")?.ok_or_else(|| invalid("name is required"))?;
    if !valid_name(&name) {
        return Err(invalid("name must be 1-64 of A-Z a-z 0-9 - _ ."));
    }
    let coordinator = format!("{name}.lead");
    if field("coordinator")?.is_some_and(|c| c != coordinator) {
        return Err(invalid(&format!("coordinator must be {coordinator}")));
    }
    let model = field("model")?;
    if model.as_deref() == Some("") {
        return Err(invalid("model must not be empty"));
    }
    // The daemon judges models and levels when the agents are made.
    let said = |key: &str| {
        let value = field(key)?;
        if value.as_deref() == Some("") {
            return Err(invalid(&format!("{key} must not be empty")));
        }
        Ok(value)
    };
    let (reasoning, threads_model, threads_reasoning) = (
        said("reasoning")?,
        said("threads_model")?,
        said("threads_reasoning")?,
    );
    if threads_reasoning.is_some() && threads_model.is_none() {
        return Err(invalid("threads_reasoning goes with threads_model"));
    }
    let threads_in = field("threads_in")?.unwrap_or_else(|| "worktree".into());
    if !THREADS_IN.contains(&threads_in.as_str()) {
        return Err(invalid("threads_in must be \"worktree\" or \"project\""));
    }
    Ok(json!({
        "dir": dir, "name": name, "coordinator": coordinator, "model": model,
        "reasoning": reasoning, "threads_model": threads_model,
        "threads_reasoning": threads_reasoning, "threads_in": threads_in, "file": true,
    }))
}

/// A project's task settings, said to its coordinator as the flags its
/// starts take. The model is always named, so a role a task starts in
/// (--profile) cannot swap it: the one picked, else the lead's own, which
/// its shell holds as AGENT_MODEL (and its effort as AGENT_EFFORT). A
/// picked model goes in quoted, since a model id may hold characters a
/// shell would act on.
pub fn tasks_rule(model: Option<&str>, effort: Option<&str>, in_project: bool) -> String {
    let quote = |v: &str| format!("'{}'", v.replace('\'', r"'\''"));
    let flags = match model {
        Some(model) => format!(
            "--model {}{}",
            quote(model),
            effort.map_or(String::new(), |e| format!(" --effort {}", quote(e)))
        ),
        None => r#"--model "$AGENT_MODEL" ${AGENT_EFFORT:+--effort "$AGENT_EFFORT"}"#.into(),
    };
    let place = if in_project {
        "Every task works in this folder, with no worktree of its own."
    } else {
        "When this folder is a git repository, a task that changes files works in its own worktree, so tasks do not collide."
    };
    format!(
        "This project's tasks, as the person set them up: start every new task, in a role (--profile) or not, with {flags}. {place}"
    )
}

/// The task rule a project's file sets, as its coordinator was told it.
pub fn rule(info: &Value) -> String {
    tasks_rule(
        info["threads_model"].as_str(),
        info["threads_reasoning"].as_str(),
        info["threads_in"] == "project",
    )
}
