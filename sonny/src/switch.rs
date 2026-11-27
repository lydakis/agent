//! `/switch`: every agent in one list over the composer, found by typing as
//! every list is ([`crate::list::Picker`]), the pick opened in this window.
//! The folder's leads come first, then the project's other agents, then
//! other projects' leads, then the rest; the one this window is marked, and
//! the one beside the conversation is picked to start. Like Claude Code's
//! and Codex's `/resume`, one list to search rather than a command for each
//! kind of agent; `/new` makes one. Each row leads with the agent's name,
//! a task's title muted after it with what it is doing.
use crate::fuzzy::Picks;
use serde_json::Value;

/// One agent, as listed.
pub struct Agent {
    pub name: String,
    /// What its work is, in the person's words: a task's title, which its
    /// muted description leads with, before what it is doing; empty for
    /// one with none. Its row leads with its name.
    pub title: String,
    /// What it is doing and where, muted beside its name after its title.
    pub about: String,
    /// Its state, as the state table names it.
    pub state: String,
    /// Another project's folder, which tells it apart.
    folder: Option<String>,
    /// The turn it last ended, while none runs: a question read of that
    /// turn is still open.
    pub ended: Option<i64>,
}

impl Agent {
    /// Its mark says idle; any other state is said in words too, before
    /// the folder.
    fn about(state: &str, folder: Option<&str>) -> String {
        let state = Some(state).filter(|state| *state != crate::started::IDLE);
        (state.into_iter().chain(folder))
            .collect::<Vec<_>>()
            .join(" · ")
    }
}

/// What a `bots` row says its agent is doing, as `/status` reads it: needs
/// you while it `waits` on the person; else held on a cause, waiting,
/// rate limited, retrying or working while a turn runs; an ended one as its end
/// did; one never run starting.
fn state(bot: &Value, waits: bool) -> String {
    if waits {
        return "needs you".to_owned();
    }
    let blocked = &bot["waiting_on"]["blocked"];
    if blocked.is_object() {
        let code = blocked["code"].as_str().unwrap_or("");
        let (what, _) = crate::status::held_cause(code, blocked["detail"].as_str());
        return format!("{}{what}", crate::status::HELD);
    }
    match (bot["running_turn"].is_null(), bot["status"].as_str()) {
        // A call a reviewer weighs waits on no one here.
        (false, Some("waiting")) if bot["waiting_on"]["approval"] == true => {
            "waiting on its reviewer".to_owned()
        }
        (false, Some("waiting")) => "waiting".to_owned(),
        (false, Some("paced")) => crate::status::waits_out(&bot["waiting_on"]),
        (false, _) => "working".to_owned(),
        (true, _) => match &bot["last"] {
            Value::Null => "starting".to_owned(),
            last => crate::started::state(last),
        },
    }
}

pub struct Picker {
    agents: Vec<Agent>,
    list: crate::list::Picker,
    /// The agent this window is.
    pub current: String,
    /// The list is still being read: what shows is what came so far.
    pub reading: bool,
}

/// The agents in `bots` (the daemon's `bots` rows), ordered for a window
/// of `current` in folder `workspace`, of project `project` or of none,
/// and the one to pick: `beside` when listed, else the first that is not
/// `current`. Those `waits` names wait on the person, as [`crate::needs`]
/// reads it; `title` gives each one's. Read beside the window, a list of
/// thousands built off its keys.
#[allow(clippy::too_many_arguments)]
pub fn listed(
    bots: &[Value],
    waits: impl Fn(&str) -> bool,
    mut title: impl FnMut(&Value) -> Option<String>,
    current: &str,
    project: Option<&str>,
    workspace: &str,
    beside: Option<&str>,
) -> (Vec<Agent>, usize) {
    let home = project
        .map(str::to_owned)
        .or_else(|| crate::project::lead_project(current).map(str::to_owned))
        .map(|home| format!("{home}."));
    let rank = |name: &str, bot: &Value| {
        let here = bot["workspace"].as_str() == Some(workspace);
        let ours = home.as_deref().is_some_and(|home| name.starts_with(home));
        let lead = crate::project::is_lead(name);
        match (lead && here, ours, lead) {
            (true, ..) => 0,
            (_, true, _) => 1,
            (_, _, true) => 2,
            _ => 3,
        }
    };
    let mut rows: Vec<(u8, bool, &str, &Value)> = (bots.iter())
        // Side chats go once idle; they are never switched to, nor
        // Sonny's own bots, nor a workflow's workers.
        .filter(|bot| bot["expire_ms"].is_null())
        .filter_map(|bot| {
            let name = bot["name"].as_str()?;
            let kept = !crate::project::is_internal(name) && !crate::workflow::is_worker(name);
            kept.then(|| (rank(name, bot), !name.ends_with(".lead"), name, bot))
        })
        .collect();
    // Each row's place worked out once, not at each comparison.
    rows.sort_unstable_by(|a, b| (a.0, a.1, a.2).cmp(&(b.0, b.1, b.2)));
    let agents: Vec<Agent> = rows
        .into_iter()
        .map(|(rank, _, name, bot)| {
            let state = state(bot, waits(name));
            // Another project's folder tells it apart; this project's
            // are this folder or its copies, which say nothing more.
            let folder = (bot["workspace"].as_str())
                .filter(|w| rank > 1 && *w != workspace)
                .map(|folder| crate::footer::fish(&crate::tilde(folder)));
            Agent {
                name: name.to_owned(),
                title: title(bot).unwrap_or_default(),
                about: Agent::about(&state, folder.as_deref()),
                state,
                folder,
                ended: (bot["running_turn"].is_null())
                    .then(|| bot["last"]["turn"].as_i64())
                    .flatten(),
            }
        })
        .collect();
    let pick = (agents.iter())
        .position(|agent| Some(agent.name.as_str()) == beside)
        .or_else(|| agents.iter().position(|agent| agent.name != current))
        .unwrap_or(0);
    (agents, pick)
}

/// Each agent by its name only, as the list finds them: what an agent is
/// doing is no name of it.
fn names(agents: &[Agent]) -> Vec<(String, String)> {
    (agents.iter())
        .map(|agent| (agent.name.clone(), String::new()))
        .collect()
}

impl Picker {
    /// The agents in `bots`, ordered and picked as [`listed`] does.
    #[cfg(test)]
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        bots: &[Value],
        waits: impl Fn(&str) -> bool,
        title: impl FnMut(&Value) -> Option<String>,
        current: &str,
        project: Option<&str>,
        workspace: &str,
        beside: Option<&str>,
        picks: Picks,
    ) -> Self {
        let (agents, pick) = listed(bots, waits, title, current, project, workspace, beside);
        Self::of(agents, pick, picks, current)
    }

    /// `agents` as [`listed`] gave them, `pick` picked.
    pub fn of(agents: Vec<Agent>, pick: usize, picks: Picks, current: &str) -> Self {
        Self {
            list: crate::list::Picker::new(names(&agents), pick).weighed(picks),
            agents,
            current: current.to_owned(),
            reading: false,
        }
    }

    /// The list open before anything of it is read, saying it is reading.
    pub fn reading(current: &str) -> Self {
        Self {
            reading: true,
            ..Self::of(Vec::new(), 0, Picks::default(), current)
        }
    }

    /// `agents` in place of those listed, as the list is read in parts:
    /// what was typed still narrows it, and the agent the person picked
    /// stays picked where it is still listed. `whole` once the last part
    /// came; `picks` once read.
    pub fn refill(&mut self, agents: Vec<Agent>, pick: usize, picks: Option<Picks>, whole: bool) {
        if let Some(picks) = picks {
            self.list.weigh(picks);
        }
        let kept = self.list.refill(names(&agents), pick);
        self.agents = agents;
        self.reading = !whole;
        let filter = self.list.filter().to_owned();
        if !kept && !filter.is_empty() {
            self.narrow(filter);
        }
    }

    /// Every agent listed, with the turn it last ended while none runs.
    pub fn ended(&self) -> impl Iterator<Item = (&str, i64)> {
        (self.agents.iter()).filter_map(|agent| Some((agent.name.as_str(), agent.ended?)))
    }

    /// `name`'s question of its turn `turn` is open, as its history reads:
    /// it needs the person, while that turn is still its last.
    pub fn asks(&mut self, name: &str, turn: i64) {
        let Some(agent) =
            (self.agents.iter_mut()).find(|agent| agent.name == name && agent.ended == Some(turn))
        else {
            return;
        };
        agent.state = "needs you".to_owned();
        agent.about = Agent::about(&agent.state, agent.folder.as_deref());
    }

    /// What is typed to narrow the list.
    #[cfg(test)]
    pub fn filter(&self) -> &str {
        self.list.filter()
    }

    /// The agents typing leaves, best first, each with the chars of its
    /// name typed.
    pub fn rows(&self) -> Vec<(&Agent, Vec<usize>)> {
        (self.list.shown().iter())
            .map(|(at, found)| (&self.agents[*at], found.clone()))
            .collect()
    }

    /// The rows drawn, with `room` rows free above the composer: the
    /// row the first of them is among `rows()`, and the columns the
    /// longest name shown takes, as [`crate::list::lines_of`] pads them.
    /// Only those drawn are gathered, of a list of thousands.
    pub fn drawn(&self, room: usize) -> (Vec<(&Agent, Vec<usize>)>, usize, usize) {
        use unicode_width::UnicodeWidthStr;
        let shown = self.list.shown();
        let window = crate::list::window(shown.len(), self.at(), room);
        let wide = (shown.iter())
            .map(|(at, _)| self.agents[*at].name.width())
            .max()
            .unwrap_or(0);
        let first = window.start;
        let rows = (shown[window].iter())
            .map(|(at, found)| (&self.agents[*at], found.clone()))
            .collect();
        (rows, first, wide)
    }

    /// Where the pick is among `rows()`.
    pub fn at(&self) -> usize {
        self.list.at()
    }

    /// Narrowed to what is typed, the best row picked that is not this
    /// window's own agent.
    pub fn narrow(&mut self, filter: String) {
        self.list.narrow(filter);
        if !self.list.filter().is_empty()
            && let Some(at) = (self.rows().iter()).position(|(agent, _)| agent.name != self.current)
        {
            self.list.pick_at(at);
        }
    }

    /// What `event` does, typing narrowing as [`Picker::narrow`] does; an
    /// agent taken is noted so it ranks first next time.
    pub fn event(&mut self, event: &ratatui::crossterm::event::Event) -> crate::list::Act {
        let before = self.list.filter().to_owned();
        let act = self.list.event(event);
        if self.list.filter() != before {
            let filter = self.list.filter().to_owned();
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
            json!({"name": "blog.lead", "workspace": "/w/blog", "running_turn": null,
                "last": {"status": "completed"}}),
            json!({"name": "shop.fix", "workspace": "/w/shop/.copies/fix", "running_turn": 3}),
            json!({"name": "shop.lead.ok", "workspace": "/w/shop", "running_turn": 6,
                "status": "waiting", "waiting_on": {"approval": true}}),
            json!({"name": "shop.lead.rv", "workspace": "/w/shop", "running_turn": 7,
                "status": "waiting", "waiting_on": {"approval": true}}),
            json!({"name": "shop.lead", "workspace": "/w/shop", "running_turn": null,
                "last": {"status": "failed", "error": "provider_http_500"}}),
            json!({"name": "shop.lead.web", "workspace": "/w/shop", "running_turn": 5,
                "status": "waiting", "waiting_on": {"blocked": {"code": "provider_http_401"}}}),
            json!({"name": "shop.lead.side-1", "workspace": "/w/shop", "expire_ms": 9}),
            json!({"name": "notes", "workspace": "/w/notes", "running_turn": null,
                "last": {"status": "interrupted"}}),
            json!({"name": ".reviewer.auto", "workspace": "/", "running_turn": null}),
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
            |bot| bot == "shop.lead.ok",
            |_| None,
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
                "shop.lead.ok",
                "shop.lead.rv",
                "shop.lead.web",
                "shop.fix",
                "blog.lead",
                "notes"
            ]
        );
        // The first that is not this window is picked.
        assert_eq!(picker.chosen().as_deref(), Some("shop.lead.ok"));
        let about: Vec<&str> = picker.agents.iter().map(|a| a.about.as_str()).collect();
        // Idle is said by its mark alone; every other state in words too.
        assert_eq!(
            about,
            [
                "failed",
                "needs you",
                "waiting on its reviewer",
                "held: API key refused",
                "working",
                "/w/blog",
                "interrupted · /w/notes"
            ]
        );
        let states: Vec<&str> = picker.agents.iter().map(|a| a.state.as_str()).collect();
        assert_eq!(
            states,
            [
                "failed",
                "needs you",
                "waiting on its reviewer",
                "held: API key refused",
                "working",
                "idle",
                "interrupted"
            ]
        );
    }

    #[test]
    fn the_agent_beside_is_picked_and_typing_narrows() {
        let mut picker = Picker::new(
            &bots(),
            |bot| bot == "shop.lead.ok",
            |_| None,
            "shop.lead",
            Some("shop"),
            "/w/shop",
            Some("shop.fix"),
            Picks::default(),
        );
        assert_eq!(picker.chosen().as_deref(), Some("shop.fix"));
        // Narrowed, the best that is not this window is picked.
        picker.narrow("lead".into());
        assert_eq!(picker.chosen().as_deref(), Some("shop.lead.ok"));
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
    fn a_task_s_title_is_muted_after_its_name_and_never_found_by() {
        let titled = |bot: &Value| {
            (bot["name"] == "shop.fix").then(|| "Round half cents to even".to_owned())
        };
        let mut picker = Picker::new(
            &bots(),
            |_| false,
            titled,
            "shop.lead",
            Some("shop"),
            "/w/shop",
            None,
            Picks::default(),
        );
        let items: Vec<crate::list::Item> = (picker.rows().into_iter())
            .map(|(agent, found)| {
                crate::list::Item::new(agent.name.clone(), agent.about.clone())
                    .title(agent.title.clone())
                    .sign(crate::status::sign(&agent.state))
                    .current(agent.name == picker.current)
                    .found(&found)
            })
            .collect();
        let rows: Vec<String> = (crate::list::lines(&items, 0, 80, 10).iter().skip(1))
            .map(|line| line.to_string())
            .collect();
        // Every agent is listed, each row leading with its name: no blank
        // column before the names of the lead and the helpers.
        assert_eq!(rows.len(), 7);
        for (row, (agent, _)) in rows.iter().zip(picker.rows()) {
            let past_sign: String = row.chars().skip(4).collect();
            assert_eq!(past_sign.split(' ').next(), Some(agent.name.as_str()));
        }
        assert!(rows[4].ends_with("shop.fix       Round half cents to even · working"));
        // Typing finds by name only, never by a word of the title.
        picker.narrow("halves".into());
        assert!(names(&picker).is_empty());
        picker.narrow("fix".into());
        assert_eq!(names(&picker), ["shop.fix"]);
    }

    #[test]
    fn a_window_of_no_folders_lead_lists_every_agent() {
        let picker = Picker::new(
            &bots(),
            |_| false,
            |_| None,
            "notes",
            None,
            "/w/notes",
            None,
            Picks::default(),
        );
        assert_eq!(names(&picker)[0], "blog.lead");
        assert_eq!(names(&picker).len(), 7);
    }

    fn part(bots: &[Value]) -> (Vec<Agent>, usize) {
        listed(
            bots,
            |_| false,
            |_| None,
            "shop.lead",
            Some("shop"),
            "/w/shop",
            None,
        )
    }

    #[test]
    fn a_list_read_in_parts_fills_in_under_what_the_person_did() {
        let every = bots();
        let shop: Vec<Value> = (every.iter())
            .filter(|bot| bot["name"].as_str().unwrap().starts_with("shop."))
            .cloned()
            .collect();
        // Open before anything is read; the project's own come first.
        let mut picker = Picker::reading("shop.lead");
        assert!(picker.reading && picker.rows().is_empty() && picker.chosen().is_none());
        let (agents, pick) = part(&shop);
        picker.refill(agents, pick, None, false);
        assert!(picker.reading);
        assert_eq!(picker.chosen().as_deref(), Some("shop.lead.ok"));
        // Untouched, the whole list picks as a list read at once does.
        let (agents, pick) = part(&every);
        let mut whole = Picker::reading("shop.lead");
        whole.refill(agents, pick, None, true);
        let (agents, pick) = part(&every);
        picker.refill(agents, pick, None, true);
        assert!(!picker.reading);
        assert_eq!(names(&picker), names(&whole));
        assert_eq!(picker.chosen(), whole.chosen());
        // Moved by the person, their pick stays where it was, and what
        // they typed still narrows.
        let mut picker = Picker::reading("shop.lead");
        let (agents, pick) = part(&shop);
        picker.refill(agents, pick, None, false);
        picker.narrow("web".into());
        assert_eq!(picker.chosen().as_deref(), Some("shop.lead.web"));
        let (agents, pick) = part(&every);
        picker.refill(agents, pick, None, true);
        assert_eq!(picker.filter(), "web");
        assert_eq!(picker.chosen().as_deref(), Some("shop.lead.web"));
        // Typed for one not read yet: the best row once it is.
        let mut picker = Picker::reading("shop.lead");
        let (agents, pick) = part(&shop);
        picker.refill(agents, pick, None, false);
        picker.narrow("notes".into());
        assert_eq!(picker.chosen(), None);
        let (agents, pick) = part(&every);
        picker.refill(agents, pick, None, true);
        assert_eq!(picker.chosen().as_deref(), Some("notes"));
    }

    #[test]
    fn only_the_rows_drawn_are_gathered() {
        let bots: Vec<Value> = (0..500)
            .map(|n| json!({"name": format!("shop.t{n:03}"), "workspace": "/w/shop"}))
            .collect();
        let (agents, pick) = part(&bots);
        let mut picker = Picker::reading("shop.lead");
        picker.refill(agents, pick, None, true);
        let (rows, first, wide) = picker.drawn(30);
        assert_eq!(
            (rows.len(), first, wide),
            (crate::list::ROWS, 0, "shop.t000".len())
        );
        for _ in 0..20 {
            picker.event(&ratatui::crossterm::event::Event::Key(
                ratatui::crossterm::event::KeyCode::Down.into(),
            ));
        }
        let (rows, first, _) = picker.drawn(30);
        assert_eq!(first, 20 + 1 - crate::list::ROWS);
        assert_eq!(rows.last().unwrap().0.name, "shop.t020");
    }
}
