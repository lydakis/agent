//! `~/.agent/config.toml`: the person's settings, kept for every client
//! and the CLI, as Codex keeps its own. Provider keys stay in
//! `~/.agent/env`. A missing file is the defaults; any key the file does
//! not know is a mistake, refused by name, not ignored.
//!
//! ```toml
//! model = "anthropic/claude-opus-5-5"  # a new agent's, with none named
//! provider = "chatgpt"          # the daemon's, with no AGENT_PROVIDER
//! web_search = "disabled"       # "live", the default, lets new agents search and fetch
//! senders = "hidden"            # "shown", the default, labels what agents send
//!
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
    /// `provider/model` for a new agent that names none.
    pub model: Option<String>,
    /// The daemon's `--provider` specs, as AGENT_PROVIDER takes them.
    pub provider: Option<String>,
    /// Whether a new agent gets web search where its provider offers it,
    /// and `web_fetch`, as Codex's `web_search` key says: `"live"`, the
    /// default, or `"disabled"`.
    pub web_search: WebSearch,
    /// Whether a new agent's model reads what another agent or a client
    /// sent it with no line naming the sender: `senders = "hidden"`;
    /// `"shown"`, the default, names it.
    pub hide_senders: bool,
    pub judge: Judge,
    pub sonny: Sonny,
}

/// Codex's `web_search` modes this runtime serves. Codex's `cached`, an
/// index without the live web, is not one: Anthropic's search has no such
/// mode.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum WebSearch {
    #[default]
    Live,
    Disabled,
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
        if name == "web_search" {
            config.web_search = match string(name, value)?.as_str() {
                "live" => WebSearch::Live,
                "disabled" => WebSearch::Disabled,
                other => return Err(format!("web_search is live or disabled, not {other}")),
            };
            continue;
        }
        if name == "senders" {
            config.hide_senders = match string(name, value)?.as_str() {
                "shown" => false,
                "hidden" => true,
                other => return Err(format!("senders is shown or hidden, not {other}")),
            };
            continue;
        }
        if let Some(top) = match name.as_str() {
            "model" => Some(&mut config.model),
            "provider" => Some(&mut config.provider),
            _ => None,
        } {
            *top = Some(string(name, value)?);
            continue;
        }
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
            let value = string(&format!("{name}.{key}"), value)?;
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

fn string(key: &str, value: &toml::Value) -> Result<String, String> {
    match value.as_str() {
        Some("") => Err(format!("{key} must not be empty")),
        Some(value) => Ok(value.to_owned()),
        None => Err(format!("{key} must be a string")),
    }
}

/// Sets each `key` (`model`, or `table.key` such as `sonny.lead_effort`)
/// to its value in the file at `path`, line by line so the person's
/// comments and order stay: a key's line is replaced where it is, and a
/// new one goes at the end of its table, or of the top level for a bare
/// key. The file is written whole, and only if it still reads.
pub fn set(path: &Path, pairs: &[(&str, &str)]) -> Result<(), String> {
    let unwritable =
        |e: &dyn std::fmt::Display| format!("config_unwritable: {}: {e}", path.display());
    read(path)?;
    let text = match std::fs::read_to_string(path) {
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => String::new(),
        read => read.map_err(|e| format!("config_unreadable: {}: {e}", path.display()))?,
    };
    let mut lines: Vec<String> = text.lines().map(str::to_owned).collect();
    for (key, value) in pairs {
        let (table, name) = key.split_once('.').unwrap_or(("", key));
        let line = format!("{name} = {}", toml::Value::String((*value).to_owned()));
        let at = |line: &str| {
            line.trim_start().strip_prefix('[').map(|h| {
                h.trim_start_matches('[')
                    .split(']')
                    .next()
                    .unwrap_or("")
                    .trim()
                    .to_owned()
            })
        };
        // The lines of `table`: after its header, or the top level's
        // before the first header, up to the next header.
        let begin = match table {
            "" => 0,
            _ => match lines.iter().position(|l| at(l).as_deref() == Some(table)) {
                Some(header) => header + 1,
                None => {
                    if lines.last().is_some_and(|l| !l.trim().is_empty()) {
                        lines.push(String::new());
                    }
                    lines.push(format!("[{table}]"));
                    lines.len()
                }
            },
        };
        let end = (begin..lines.len())
            .find(|&i| at(&lines[i]).is_some())
            .unwrap_or(lines.len());
        let named = (begin..end).find(|&i| {
            lines[i]
                .split_once('=')
                .is_some_and(|(k, _)| k.trim().trim_matches('"') == name)
        });
        match named {
            Some(i) => lines[i] = line,
            None => {
                let last = (begin..end)
                    .rev()
                    .find(|&i| !lines[i].trim().is_empty())
                    .map_or(begin, |i| i + 1);
                lines.insert(last, line);
                if table.is_empty() && last == 0 && lines.len() > 1 && !lines[1].trim().is_empty() {
                    lines.insert(1, String::new());
                }
            }
        }
    }
    let mut text = lines.join("\n");
    text.push('\n');
    parse(&text).map_err(|reason| format!("config_invalid: {}: {reason}", path.display()))?;
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir).map_err(|e| unwritable(&e))?;
    }
    let staged = path.with_extension(format!("toml.{}", std::process::id()));
    std::fs::write(&staged, text)
        .and_then(|()| std::fs::rename(&staged, path))
        .map_err(|e| {
            let _ = std::fs::remove_file(&staged);
            unwritable(&e)
        })
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
        assert_eq!(config.web_search, WebSearch::Live);
        let off = parse("web_search = \"disabled\"").unwrap();
        assert_eq!(off.web_search, WebSearch::Disabled);
        assert!(!config.hide_senders);
        assert!(parse("senders = \"hidden\"").unwrap().hide_senders);
        assert!(!parse("senders = \"shown\"").unwrap().hide_senders);
        for (text, error) in [
            (
                "web_search = \"cached\"",
                "web_search is live or disabled, not cached",
            ),
            ("web_search = false", "web_search must be a string"),
            ("senders = \"off\"", "senders is shown or hidden, not off"),
            ("colour = \"red\"", "unknown key colour"),
            ("model = 3", "model must be a string"),
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
    fn setting_keys_keeps_the_rest_of_the_file() {
        let dir = std::env::temp_dir().join(format!("agent-config-set-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let file = dir.join("config.toml");
        set(&file, &[("sonny.lead_effort", "medium"), ("model", "a/b")]).unwrap();
        assert_eq!(
            std::fs::read_to_string(&file).unwrap(),
            "model = \"a/b\"\n\n[sonny]\nlead_effort = \"medium\"\n"
        );
        std::fs::write(
            &file,
            "# mine\nmodel = \"a/b\"\n\n[judge]\neffort = \"low\" # cheap\n\n[sonny]\nlead_effort = \"low\"\n",
        )
        .unwrap();
        set(
            &file,
            &[
                ("model", "c/d"),
                ("provider", "bedrock"),
                ("judge.model", "e/f"),
                ("sonny.task_effort", "high"),
            ],
        )
        .unwrap();
        assert_eq!(
            std::fs::read_to_string(&file).unwrap(),
            "# mine\nmodel = \"c/d\"\nprovider = \"bedrock\"\n\n[judge]\neffort = \"low\" # cheap\nmodel = \"e/f\"\n\n[sonny]\nlead_effort = \"low\"\ntask_effort = \"high\"\n"
        );
        let config = read(&file).unwrap();
        assert_eq!(config.provider.as_deref(), Some("bedrock"));
        assert_eq!(config.judge.model.as_deref(), Some("e/f"));
        // A key the reader does not know is refused, and the file kept.
        assert!(
            set(&file, &[("colour", "red")])
                .unwrap_err()
                .contains("unknown key colour")
        );
        assert_eq!(read(&file).unwrap(), config);
        std::fs::remove_dir_all(&dir).unwrap();
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
