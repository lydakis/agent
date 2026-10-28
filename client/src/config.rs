//! `~/.agent/config.toml`: the person's settings, kept for every client
//! and the CLI, as Codex keeps its own. Provider keys stay in
//! `~/.agent/env`. A missing file is the defaults; any key the file does
//! not know is a mistake, refused by name, not ignored.
//!
//! ```toml
//! [judge]
//! model = "openai/gpt-6-luna"   # who judges for the approver and the lookout
//! effort = "low"                # a model judge's effort
//!
//! [sonny]
//! lead_effort = "medium"        # a new lead's effort
//! task_effort = "high"          # each new task's
//! ```
//!
//! Each unset value is the model's own, or, for a task, its lead's.
use std::io::Read;
use std::path::{Path, PathBuf};

/// The largest file the reader takes.
pub const LIMIT: u64 = 64 * 1024;

#[derive(Clone, Debug, Default, PartialEq)]
pub struct Config {
    pub judge: Judge,
    pub sonny: Sonny,
}

/// The judge `agent approver` and Sonny's lookout ask.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Judge {
    /// `provider/model` or `openai-decisions/MODEL`; unset, the judged
    /// bot's own model.
    pub model: Option<String>,
    pub effort: Option<String>,
}

/// What Sonny starts its agents with.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Sonny {
    pub lead_effort: Option<String>,
    pub task_effort: Option<String>,
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
        let section = value
            .as_table()
            .ok_or_else(|| format!("unknown key {name}"))?;
        let keys: &[&str] = match name.as_str() {
            "judge" => &["model", "effort"],
            "sonny" => &["lead_effort", "task_effort"],
            _ => return Err(format!("unknown table [{name}]")),
        };
        for (key, value) in section {
            if !keys.contains(&key.as_str()) {
                return Err(format!("unknown key {name}.{key}"));
            }
            let value = match value.as_str() {
                Some("") => return Err(format!("{name}.{key} must not be empty")),
                Some(value) => value.to_owned(),
                None => return Err(format!("{name}.{key} must be a string")),
            };
            // The daemon judges models and levels when the agents are made.
            *match (name.as_str(), key.as_str()) {
                ("judge", "model") => &mut config.judge.model,
                ("judge", "effort") => &mut config.judge.effort,
                ("sonny", "lead_effort") => &mut config.sonny.lead_effort,
                _ => &mut config.sonny.task_effort,
            } = Some(value);
        }
    }
    Ok(config)
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
        for (text, error) in [
            ("model = \"a/b\"", "unknown key model"),
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
