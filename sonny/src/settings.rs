//! What the person set in Sonny, kept for every folder it opens:
//! `~/.agent/sonny/settings.json`. A missing or unreadable file is the
//! defaults.
use serde_json::{Value, json};
use std::path::PathBuf;

#[derive(Clone, Debug, PartialEq)]
pub struct Settings {
    /// Whether finished and waiting work rings. Notifications show either way.
    pub chime: bool,
    /// How a lead's tasks get their own copy of its folder, one of
    /// `agent_client::project::COPIES`: git worktrees, or sandfs copies.
    pub copies: String,
    /// Whether every tool call is drawn, or only the bot's words and the
    /// calls that move the work on (the clean view, the default).
    pub verbose: bool,
}

impl Default for Settings {
    fn default() -> Self {
        Settings {
            chime: true,
            copies: "worktree".into(),
            verbose: false,
        }
    }
}

fn path() -> Option<PathBuf> {
    std::env::var_os("HOME").map(|home| PathBuf::from(home).join(".agent/sonny/settings.json"))
}

impl Settings {
    pub fn load() -> Self {
        let text = path().and_then(|path| std::fs::read_to_string(path).ok());
        text.and_then(|text| serde_json::from_str::<Value>(&text).ok())
            .map(|saved| Self::from(&saved))
            .unwrap_or_default()
    }

    fn from(saved: &Value) -> Self {
        let defaults = Settings::default();
        Settings {
            chime: saved["chime"].as_bool().unwrap_or(defaults.chime),
            copies: saved["copies"]
                .as_str()
                .filter(|c| agent_client::project::COPIES.contains(c))
                .map_or(defaults.copies, str::to_owned),
            verbose: saved["verbose"].as_bool().unwrap_or(defaults.verbose),
        }
    }

    pub fn save(&self) -> Result<(), String> {
        let path = path().ok_or("HOME is not set")?;
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir).map_err(|e| e.to_string())?;
        }
        let text = serde_json::to_string_pretty(&json!({
            "chime": self.chime,
            "copies": self.copies,
            "verbose": self.verbose,
        }))
        .map_err(|e| e.to_string())?;
        std::fs::write(&path, text + "\n").map_err(|e| format!("{}: {e}", path.display()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn what_is_not_saved_keeps_its_default() {
        assert_eq!(Settings::from(&json!({})), Settings::default());
        assert!(!Settings::from(&json!({"chime": false})).chime);
        assert!(Settings::from(&json!({"chime": "no"})).chime);
        assert!(Settings::from(&json!({"verbose": true})).verbose);
        assert!(!Settings::from(&json!({"verbose": "yes"})).verbose);
        assert_eq!(
            Settings::from(&json!({"copies": "sandfs"})).copies,
            "sandfs"
        );
        assert_eq!(
            Settings::from(&json!({"copies": "rsync"})).copies,
            "worktree"
        );
    }
}
