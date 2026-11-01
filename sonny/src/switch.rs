//! `/switch`: every agent in one list over the composer, found by typing as
//! every list is ([`crate::fuzzy`]), the pick opened in this window. The
//! folder's leads come first, then the project's other agents, then other
//! projects' leads, then the rest; the one this window is marked, and the
//! one beside the conversation is picked to start. A last row makes a new
//! lead in the folder, named by what was typed when it can name one. Like
//! Claude Code's and Codex's `/resume`, one list to search rather than a
//! command for each kind of agent.
use crate::fuzzy::{Found, Picks, order};
use serde_json::Value;

/// One agent, as listed.
pub struct Agent {
    pub name: String,
    /// What it is doing and where, muted beside its name.
    pub about: String,
}

/// What Enter takes.
#[derive(Debug, PartialEq)]
pub enum Chosen {
    Agent(String),
    /// A new lead of the folder, by its short name, else numbered.
    Lead(Option<String>),
}

pub struct Picker {
    agents: Vec<Agent>,
    /// Typed to narrow the list.
    pub filter: String,
    pick: usize,
    /// The agent this window is.
    pub current: String,
    /// The folder's project, when this window is its lead's: a new lead
    /// can be made there.
    project: Option<String>,
    picks: Picks,
}

/// A row shown: an agent, or the new lead.
pub enum Row<'a> {
    Agent(&'a Agent, Found),
    Lead(Option<String>),
}

impl Picker {
    /// The agents in `bots` (the daemon's `bots` rows), ordered for a
    /// window of `current` in folder `workspace`, a lead of `project` or of
    /// none; `beside` picked when listed.
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
        Self {
            agents,
            filter: String::new(),
            pick,
            current: current.to_owned(),
            project: project.map(str::to_owned),
            picks,
        }
    }

    /// The rows the filter leaves, best first, then the new lead's.
    pub fn rows(&self) -> Vec<Row<'_>> {
        let found = (self.agents.iter()).map(|agent| crate::fuzzy::find(&agent.name, &self.filter));
        let typed = !self.filter.is_empty();
        let mut rows: Vec<Row> = order(found, |at| match typed {
            true => self.picks.weight_now(&self.agents[at].name),
            false => 0.0,
        })
        .into_iter()
        .map(|(at, found)| Row::Agent(&self.agents[at], found))
        .collect();
        if let Some(project) = &self.project {
            // Named by what was typed when no lead has that name yet.
            let name = Some(self.filter.trim())
                .filter(|name| crate::project::lead_named(name))
                .filter(|name| {
                    let bot = crate::project::lead_bot(project, name);
                    !self.agents.iter().any(|agent| agent.name == bot)
                })
                .map(str::to_owned);
            rows.push(Row::Lead(name));
        }
        rows
    }

    /// Where the pick is among `rows()`.
    pub fn at(&self) -> usize {
        self.pick.min(self.rows().len().saturating_sub(1))
    }

    pub fn up(&mut self) {
        let len = self.rows().len().max(1);
        self.pick = (self.at() + len - 1) % len;
    }

    pub fn down(&mut self) {
        let len = self.rows().len().max(1);
        self.pick = (self.at() + 1) % len;
    }

    /// Narrowed to what is typed, the best row picked that is not this
    /// window's own agent.
    pub fn narrow(&mut self, filter: String) {
        self.filter = filter;
        self.pick = (self.rows().iter())
            .position(|row| !matches!(row, Row::Agent(agent, _) if agent.name == self.current))
            .unwrap_or(0);
    }

    /// The row picked, when there is one.
    pub fn chosen(&self) -> Option<Chosen> {
        Some(match self.rows().into_iter().nth(self.at())? {
            Row::Agent(agent, _) => Chosen::Agent(agent.name.clone()),
            Row::Lead(name) => Chosen::Lead(name),
        })
    }

    /// Notes that `name` was picked, so it ranks first next time.
    pub fn picked(&mut self, name: &str) {
        self.picks.pick_now(name);
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
            .map(|row| match row {
                Row::Agent(agent, _) => agent.name.clone(),
                Row::Lead(name) => format!("new {}", name.unwrap_or_default()),
            })
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
                "notes",
                "new "
            ]
        );
        // The first that is not this window is picked.
        assert_eq!(picker.chosen(), Some(Chosen::Agent("shop.lead.web".into())));
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
        assert_eq!(picker.chosen(), Some(Chosen::Agent("shop.fix".into())));
        // Narrowed, the best that is not this window is picked.
        picker.narrow("lead".into());
        assert_eq!(picker.chosen(), Some(Chosen::Agent("shop.lead.web".into())));
        picker.narrow("blog".into());
        assert_eq!(names(&picker), ["blog.lead", "new blog"]);
        assert_eq!(picker.chosen(), Some(Chosen::Agent("blog.lead".into())));
        picker.up();
        assert_eq!(picker.chosen(), Some(Chosen::Lead(Some("blog".into()))));
        // A lead's name already taken names no new one.
        picker.narrow("web".into());
        assert_eq!(names(&picker), ["shop.lead.web", "new "]);
        // Nothing found: the new lead, named so, is the only row.
        picker.narrow("zz9".into());
        assert_eq!(names(&picker), ["new zz9"]);
        // A name no lead can have makes one numbered.
        picker.narrow("zz.9".into());
        assert_eq!(picker.chosen(), Some(Chosen::Lead(None)));
    }

    #[test]
    fn a_window_of_no_folders_lead_makes_no_lead() {
        let picker = Picker::new(&bots(), "notes", None, "/w/notes", None, Picks::default());
        assert!(!names(&picker).iter().any(|name| name.starts_with("new")));
        assert_eq!(names(&picker)[0], "blog.lead");
    }
}
