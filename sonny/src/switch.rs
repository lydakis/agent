//! `/switch`: every agent in one list over the composer, found by typing as
//! every list is ([`crate::list::Picker`]), the pick opened in this window.
//! The folder's leads come first, then the project's other agents, then
//! other projects' leads, then the rest; the one this window is marked, and
//! the one beside the conversation is picked to start. Like Claude Code's
//! and Codex's `/resume`, one list to search rather than a command for each
//! kind of agent; `/new` makes one.
use crate::fuzzy::Picks;
use serde_json::Value;

/// One agent, as listed.
pub struct Agent {
    pub name: String,
    /// What it is doing and where, muted beside its name.
    pub about: String,
}

pub struct Picker {
    agents: Vec<Agent>,
    list: crate::list::Picker,
    /// The agent this window is.
    pub current: String,
}

impl Picker {
    /// The agents in `bots` (the daemon's `bots` rows), ordered for a
    /// window of `current` in folder `workspace`, of project `project` or
    /// of none; `beside` picked when listed.
    pub fn new(
        bots: &[Value],
        current: &str,
        project: Option<&str>,
        workspace: &str,
        beside: Option<&str>,
        picks: Picks,
    ) -> Self {
        let home = project
            .map(str::to_owned)
            .or_else(|| crate::project::lead_project(current).map(str::to_owned));
        let rank = |bot: &Value| {
            let name = bot["name"].as_str().unwrap_or("");
            let here = bot["workspace"].as_str() == Some(workspace);
            let ours = home
                .as_deref()
                .is_some_and(|home| name.starts_with(&format!("{home}.")));
            let lead = crate::project::is_lead(name);
            match (lead && here, ours, lead) {
                (true, ..) => 0,
                (_, true, _) => 1,
                (_, _, true) => 2,
                _ => 3,
            }
        };
        let mut rows: Vec<&Value> = (bots.iter())
            // Side chats go once idle; they are never switched to.
            .filter(|bot| bot["expire_ms"].is_null() && bot["name"].is_string())
            .collect();
        rows.sort_by(|a, b| {
            (rank(a).cmp(&rank(b))).then_with(|| {
                let first = |bot: &Value| !bot["name"].as_str().unwrap_or("").ends_with(".lead");
                (first(a).cmp(&first(b))).then(a["name"].as_str().cmp(&b["name"].as_str()))
            })
        });
        let agents: Vec<Agent> = rows
            .into_iter()
            .map(|bot| {
                let mut about = Vec::new();
                if !bot["running_turn"].is_null() {
                    about.push("working".to_owned());
                }
                // Another project's folder tells it apart; this project's
                // are this folder or its copies, which say nothing more.
                if rank(bot) > 1
                    && let Some(folder) = bot["workspace"].as_str().filter(|w| *w != workspace)
                {
                    about.push(crate::footer::fish(&crate::tilde(folder)));
                }
                Agent {
                    name: bot["name"].as_str().unwrap_or("").to_owned(),
                    about: about.join(" · "),
                }
            })
            .collect();
        let pick = (agents.iter())
            .position(|agent| Some(agent.name.as_str()) == beside)
            .or_else(|| agents.iter().position(|agent| agent.name != current))
            .unwrap_or(0);
        // Found by name only: what an agent is doing is no name of it.
        let names = (agents.iter())
            .map(|agent| (agent.name.clone(), String::new()))
            .collect();
        Self {
            list: crate::list::Picker::new(names, pick).weighed(picks),
            agents,
            current: current.to_owned(),
        }
    }

    /// What is typed to narrow the list.
    #[cfg(test)]
    pub fn filter(&self) -> &str {
        &self.list.filter
    }

    /// The agents typing leaves, best first, each with the chars of its
    /// name typed.
    pub fn rows(&self) -> Vec<(&Agent, Vec<usize>)> {
        (self.list.shown().into_iter())
            .map(|(at, found)| (&self.agents[at], found))
            .collect()
    }

    /// Where the pick is among `rows()`.
    pub fn at(&self) -> usize {
        self.list.at()
    }

    /// Narrowed to what is typed, the best row picked that is not this
    /// window's own agent.
    pub fn narrow(&mut self, filter: String) {
        self.list.narrow(filter);
        if !self.list.filter.is_empty()
            && let Some(at) = (self.rows().iter()).position(|(agent, _)| agent.name != self.current)
        {
            self.list.pick_at(at);
        }
    }

    /// What `event` does, typing narrowing as [`Picker::narrow`] does; an
    /// agent taken is noted so it ranks first next time.
    pub fn event(&mut self, event: &ratatui::crossterm::event::Event) -> crate::list::Act {
        let before = self.list.filter.clone();
        let act = self.list.event(event);
        if self.list.filter != before {
            let filter = self.list.filter.clone();
            self.narrow(filter);
        }
        if let crate::list::Act::Take(_) = act {
            self.list.took();
        }
        act
    }

    /// The composer's line while the list is open, and its cursor.
    pub fn line(&self, asked: &str) -> (ratatui::text::Line<'static>, u16) {
        self.list.line(asked)
    }

    /// The agent picked, when typing leaves any.
    pub fn chosen(&self) -> Option<String> {
        Some(self.agents[self.list.chosen()?].name.clone())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn bots() -> Vec<Value> {
        vec![
            json!({"name": "blog.lead", "workspace": "/w/blog", "running_turn": null}),
            json!({"name": "shop.fix", "workspace": "/w/shop/.copies/fix", "running_turn": 3}),
            json!({"name": "shop.lead", "workspace": "/w/shop", "running_turn": null}),
            json!({"name": "shop.lead.web", "workspace": "/w/shop", "running_turn": null}),
            json!({"name": "shop.lead.side-1", "workspace": "/w/shop", "expire_ms": 9}),
            json!({"name": "notes", "workspace": "/w/notes", "running_turn": null}),
        ]
    }

    fn names(picker: &Picker) -> Vec<String> {
        (picker.rows().into_iter())
            .map(|(agent, _)| agent.name.clone())
            .collect()
    }

    #[test]
    fn the_folders_leads_come_first_then_the_projects_then_other_leads() {
        let picker = Picker::new(
            &bots(),
            "shop.lead",
            Some("shop"),
            "/w/shop",
            None,
            Picks::default(),
        );
        assert_eq!(
            names(&picker),
            [
                "shop.lead",
                "shop.lead.web",
                "shop.fix",
                "blog.lead",
                "notes"
            ]
        );
        // The first that is not this window is picked.
        assert_eq!(picker.chosen().as_deref(), Some("shop.lead.web"));
        let about: Vec<&str> = picker.agents.iter().map(|a| a.about.as_str()).collect();
        assert_eq!(about, ["", "", "working", "/w/blog", "/w/notes"]);
    }

    #[test]
    fn the_agent_beside_is_picked_and_typing_narrows() {
        let mut picker = Picker::new(
            &bots(),
            "shop.lead",
            Some("shop"),
            "/w/shop",
            Some("shop.fix"),
            Picks::default(),
        );
        assert_eq!(picker.chosen().as_deref(), Some("shop.fix"));
        // Narrowed, the best that is not this window is picked.
        picker.narrow("lead".into());
        assert_eq!(picker.chosen().as_deref(), Some("shop.lead.web"));
        picker.narrow("blog".into());
        assert_eq!(names(&picker), ["blog.lead"]);
        assert_eq!(picker.chosen().as_deref(), Some("blog.lead"));
        // Nothing found: nothing to take.
        picker.narrow("zz9".into());
        assert!(names(&picker).is_empty() && picker.chosen().is_none());
        // Back to nothing typed, the agent beside again.
        picker.narrow(String::new());
        assert_eq!(picker.chosen().as_deref(), Some("shop.fix"));
    }

    #[test]
    fn a_window_of_no_folders_lead_lists_every_agent() {
        let picker = Picker::new(&bots(), "notes", None, "/w/notes", None, Picks::default());
        assert_eq!(names(&picker)[0], "blog.lead");
        assert_eq!(names(&picker).len(), 5);
    }
}
