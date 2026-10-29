//! What the person set in Sonny, kept for every folder it opens: the chime
//! and the view in `~/.agent/config.toml`'s `[sonny]`, as `/config` shows
//! them. A missing or unreadable file is the defaults.

#[derive(Clone, Debug, PartialEq)]
pub struct Settings {
    /// Whether finished and waiting work rings. Notifications show either way.
    pub chime: bool,
    /// Whether every tool call is drawn, or only the bot's words and the
    /// calls that move the work on (the clean view, the default).
    pub verbose: bool,
}

impl Default for Settings {
    fn default() -> Self {
        Self::from(agent_client::config::Sonny::default())
    }
}

impl Settings {
    pub fn load() -> Self {
        let sonny = agent_client::config::load().map(|config| config.sonny);
        Self::from(sonny.unwrap_or_default())
    }

    fn from(sonny: agent_client::config::Sonny) -> Self {
        Settings {
            chime: sonny.chime,
            verbose: sonny.verbose,
        }
    }

    /// The chime and the view into the config, where one changed: its
    /// default as no line at all.
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
        if pairs.is_empty() {
            return Ok(());
        }
        let config = agent_client::config::path().ok_or("HOME is not set")?;
        agent_client::config::write(&config, &pairs)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_config_says_what_rings_and_what_is_drawn() {
        let sonny = agent_client::config::Sonny::default;
        assert_eq!(Settings::from(sonny()), Settings::default());
        let quiet = agent_client::config::Sonny {
            chime: false,
            verbose: true,
            ..sonny()
        };
        let settings = Settings::from(quiet);
        assert!(!settings.chime && settings.verbose);
    }
}
