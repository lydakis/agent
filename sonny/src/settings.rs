//! What the person set in Sonny, kept for every folder it opens: the chime
//! and the view in `~/.agent/config.toml`'s `[sonny]`, as `/config` shows
//! them, and the screen in `~/.agent/sonny/settings.json`. A missing or
//! unreadable file is the defaults.
use serde_json::{Value, json};
use std::path::PathBuf;

#[derive(Clone, Debug, PartialEq)]
pub struct Settings {
    /// Whether finished and waiting work rings. Notifications show either way.
    pub chime: bool,
    /// Whether every tool call is drawn, or only the bot's words and the
    /// calls that move the work on (the clean view, the default).
    pub verbose: bool,
    /// Whether Sonny owns the whole screen, keeping the conversation to
    /// scroll and select itself, or writes it into the terminal's
    /// scrollback (inline, the default).
    pub fullscreen: bool,
}

impl Default for Settings {
    fn default() -> Self {
        let sonny = agent_client::config::Sonny::default();
        Settings {
            chime: sonny.chime,
            verbose: sonny.verbose,
            fullscreen: true,
        }
    }
}

fn path() -> Option<PathBuf> {
    std::env::var_os("HOME").map(|home| PathBuf::from(home).join(".agent/sonny/settings.json"))
}

impl Settings {
    pub fn load() -> Self {
        let text = path().and_then(|path| std::fs::read_to_string(path).ok());
        let saved = text.and_then(|text| serde_json::from_str::<Value>(&text).ok());
        let sonny = agent_client::config::load().map(|config| config.sonny);
        Self::from(&saved.unwrap_or_default(), sonny.unwrap_or_default())
    }

    fn from(saved: &Value, sonny: agent_client::config::Sonny) -> Self {
        let defaults = Settings::default();
        Settings {
            chime: sonny.chime,
            verbose: sonny.verbose,
            fullscreen: (saved["fullscreen"].as_bool()).unwrap_or(defaults.fullscreen),
        }
    }

    /// The chime and the view into the config, where one changed: its
    /// default as no line at all. The screen beside.
    pub fn save(&self) -> Result<(), String> {
        let was = Self::load();
        let defaults = Settings::default();
        let mut pairs = Vec::new();
        for (key, now, then, default) in [
            ("sonny.chime", self.chime, was.chime, defaults.chime),
            ("sonny.verbose", self.verbose, was.verbose, defaults.verbose),
        ] {
            if now != then {
                pairs.push((key, (now != default).then(|| now.into())));
            }
        }
        if !pairs.is_empty() {
            let config = agent_client::config::path().ok_or("HOME is not set")?;
            agent_client::config::write(&config, &pairs)?;
        }
        if self.fullscreen == was.fullscreen {
            return Ok(());
        }
        let path = path().ok_or("HOME is not set")?;
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir).map_err(|e| e.to_string())?;
        }
        let text = serde_json::to_string_pretty(&json!({"fullscreen": self.fullscreen}))
            .map_err(|e| e.to_string())?;
        std::fs::write(&path, text + "\n").map_err(|e| format!("{}: {e}", path.display()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn what_is_not_saved_keeps_its_default() {
        let sonny = agent_client::config::Sonny::default;
        assert_eq!(Settings::from(&json!({}), sonny()), Settings::default());
        assert!(!Settings::from(&json!({"fullscreen": false}), sonny()).fullscreen);
        assert!(Settings::from(&json!({"fullscreen": "no"}), sonny()).fullscreen);
        let quiet = agent_client::config::Sonny {
            chime: false,
            verbose: true,
            ..sonny()
        };
        let settings = Settings::from(&json!({}), quiet);
        assert!(!settings.chime && settings.verbose);
    }
}
