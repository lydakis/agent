//! What the person set in Sonny, kept for every folder it opens: the
//! chime, the view and the text's width in `~/.agent/config.toml`'s
//! `[sonny]`, as `/config` shows them. A missing or unreadable file is the
//! defaults.

#[derive(Clone, Debug, PartialEq)]
pub struct Settings {
    /// Whether finished and waiting work rings; off by default.
    /// Notifications show either way.
    pub chime: bool,
    /// Whether the agent's steps between what is said are drawn, or only
    /// the person's words and the agent's (the default).
    pub steps: bool,
    /// Whether every tool call is drawn, or only the bot's words and the
    /// calls that move the work on (the clean view, the default).
    pub verbose: bool,
    /// The widest the conversation is written; 0 for the window's whole
    /// width.
    pub text_width: u64,
    /// When a note opens the margin; `/config` changes it.
    pub margin_opens: agent_client::config::MarginOpens,
    /// Where a click on an agent's name takes the person.
    pub agent_opens: agent_client::config::AgentOpens,
    /// The most notes a margin holds.
    pub margin_notes: u64,
    /// The most lines a margin note runs to.
    pub margin_note_lines: u64,
    /// Experimental: whether a window that follows other leads opens with
    /// project colors on; `/colors` toggles them there.
    pub project_colors: bool,
    /// The color each project pins, by project.
    pub project_color: std::collections::BTreeMap<String, String>,
    /// The person's own margin notes: name, title and template.
    pub notes: Vec<(String, String, String)>,
    /// The sources the person set up, by name, for template notes.
    pub sources: Vec<String>,
}

impl Default for Settings {
    fn default() -> Self {
        Self::from(agent_client::config::Sonny::default())
    }
}

impl Settings {
    pub fn load() -> Self {
        Self::load_said().0
    }

    /// [`Settings::load`], and why the config did not read when it did
    /// not: Sonny opens on its defaults, and says so.
    pub fn load_said() -> (Self, Option<String>) {
        match agent_client::config::load() {
            Ok(config) => (Self::from(config.sonny), None),
            Err(error) => (
                Self::from(Default::default()),
                Some(format!("{error}; Sonny uses its defaults until it reads")),
            ),
        }
    }

    fn from(sonny: agent_client::config::Sonny) -> Self {
        Settings {
            chime: sonny.chime,
            steps: sonny.steps,
            verbose: sonny.verbose,
            text_width: sonny.text_width,
            margin_opens: sonny.margin_opens,
            agent_opens: sonny.agent_opens,
            margin_notes: sonny.margin_notes,
            margin_note_lines: sonny.margin_note_lines,
            project_colors: sonny.project_colors,
            project_color: sonny.project_color,
            notes: (sonny.notes.into_iter())
                .map(|note| (note.name, note.title, note.template))
                .collect(),
            // Only those the person set up keep what they printed for a
            // blank to read: the commons keeps none.
            sources: (sonny.sources.into_iter().map(|source| source.name)).collect(),
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
        assert!(!Settings::default().chime);
        let loud = agent_client::config::Sonny {
            chime: true,
            verbose: true,
            ..sonny()
        };
        let settings = Settings::from(loud);
        assert!(settings.chime && settings.verbose);
    }
}
