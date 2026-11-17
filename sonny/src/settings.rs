//! What the person set in Sonny, kept for every folder it opens: the
//! chime, the view and the text's width in `~/.agent/config.toml`'s
//! `[sonny]`, as `/config` shows them. A missing or unreadable file is the
//! defaults.

#[derive(Clone, Debug, PartialEq)]
pub struct Settings {
    /// Whether finished and waiting work rings; off by default.
    /// Notifications show either way.
    pub chime: bool,
    /// What the conversation shows: words (the default), steps, or every
    /// tool call.
    pub view: agent_client::config::View,
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
    /// project colors on.
    pub project_colors: bool,
    /// The color each project pins, by project.
    pub project_color: std::collections::BTreeMap<String, String>,
    /// The person's own margin notes: name, title and what each shows.
    pub notes: Vec<(String, String, crate::margin::Body)>,
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
            view: sonny.view,
            text_width: sonny.text_width,
            margin_opens: sonny.margin_opens,
            agent_opens: sonny.agent_opens,
            margin_notes: sonny.margin_notes,
            margin_note_lines: sonny.margin_note_lines,
            project_colors: sonny.project_colors,
            project_color: sonny.project_color,
            notes: (sonny.notes.into_iter())
                .map(|note| {
                    let body = match note.live {
                        true => crate::margin::Body::Template(note.markdown),
                        false => crate::margin::Body::Markdown(note.markdown),
                    };
                    (note.name, note.title, body)
                })
                .collect(),
        }
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
            view: agent_client::config::View::Verbose,
            ..sonny()
        };
        let settings = Settings::from(loud);
        assert!(settings.chime);
        assert_eq!(settings.view, agent_client::config::View::Verbose);
    }
}
