//! **Projects**: every project where the conversation was, one row a
//! project, so the person goes from one lead to another without a
//! multiplexer. A project is its lead. Projects nest as their folders do,
//! keep the order of their titles whatever their agents do, and those
//! quiet for a week fold to one last row. The pane beside it shows the
//! picked project's agents, as it shows a project's own at rest.
//!
//! Above the projects, under a muted `Pinned`, the agents the person
//! pinned ([`crate::pinned`]), numbered: `1` to `9` open them at once.
//!
//! This file holds the page: what it lists, in what order, how its keys
//! move the pick and how it is drawn. What each project's agents are doing
//! is read as `/status` reads it, by the window ([`crate::status`]).
use crate::status::{self, Counts, Group};
use ratatui::{
    crossterm::event::{KeyCode, KeyEvent, KeyModifiers},
    style::{Color, Stylize},
    text::{Line, Span},
};
use unicode_width::UnicodeWidthStr;

/// A project none of whose agents has had a turn in this long folds
/// away, as the Rulebook says: seven days.
pub const IDLE_AFTER_MS: u64 = 7 * 24 * 60 * 60 * 1000;

/// A project as the page lists it.
#[derive(Clone, Debug, Default, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct Project {
    /// Its lead, which `enter` goes to, by its own name.
    pub lead: String,
    /// What it is called where it has no title: its project's name, or
    /// `shop lead billing` for another lead of the same folder.
    pub name: String,
    /// What the person calls it, where its file or its lead says.
    pub title: String,
    /// The folder its lead works in, which nests it.
    pub folder: String,
    /// Whether its lead runs as the superlead.
    pub superlead: bool,
    /// Its agents' counts, as `/status`'s head counts them.
    pub counts: Counts,
    /// The state of its most pressing agent, as **States** orders them.
    pub state: String,
    /// When any of its agents last had a turn (ms): now, while one runs.
    pub last: u64,
    /// Its agents as the pane lists them at rest.
    pub agents: status::Agents,
    /// Its agents that are pinned, as read with the rest: a pin's mark
    /// and title come from here.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub pinned: Vec<Pin>,
}

/// A pinned agent as its project's read found it.
#[derive(Clone, Debug, Default, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct Pin {
    pub bot: String,
    /// Its title: a task's, a lead's project's; empty for one with none.
    pub title: String,
    pub state: String,
}

impl Project {
    /// The words its row leads with: its title, else its name.
    pub fn called(&self) -> &str {
        match self.title.is_empty() {
            true => &self.name,
            false => &self.title,
        }
    }

    fn found(&self, query: &str) -> bool {
        crate::find::says(query, &self.title) || crate::find::says(query, &self.name)
    }
}

/// The state of the most pressing of `rows`, in **States**' order, the
/// lead first where two are as pressing: what a project's row leads with.
pub fn pressing(rows: &[status::Row]) -> String {
    (rows.iter())
        .filter(|row| row.run.is_none())
        .min_by_key(|row| (row.group, row.name != "lead"))
        .map(|row| row.state.clone())
        .unwrap_or_else(|| crate::started::IDLE.to_owned())
}

/// What a row of the page is.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Shown {
    /// The project at this index of the page's projects, this deep.
    Project(usize, usize),
    /// The idle projects, folded to one row: how many, and whether open.
    Fold(usize, bool),
}

/// Where the pick is: on a project by its lead, so it stays on that
/// project however the rows change, on the fold, or on a pinned agent by
/// its name.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Pick {
    Project(String),
    Fold,
    Pin(String),
}

/// A line under the head of **Projects**: one drawn as it is, with the
/// row it shows and where its name is a link, or the shown row at this
/// place, drawn only once in view.
enum Body {
    Line(
        Line<'static>,
        Option<usize>,
        Option<(std::ops::Range<usize>, String)>,
    ),
    Shown(usize),
}

/// A row the pick can be on, in the page's order: the pinned agents (by
/// their place among the pins), then the projects and the fold.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Row {
    Pin(usize),
    Shown(Shown),
}

/// What a key did.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Act {
    /// Nothing more to do.
    Stay,
    /// The page closes: the conversation is back, and what the pane showed.
    Close,
    /// Go to this agent: the window becomes its.
    Go(String),
    /// Pin this agent, or unpin it where it is pinned.
    Pin(String),
    /// This pinned agent's number was pressed, and it is not there.
    Gone(String),
}

/// The page.
#[derive(Debug, Default)]
pub struct View {
    projects: Vec<Project>,
    /// The lead of the project the window is in.
    here: String,
    /// The agent the window shows: its lead, or one of its tasks.
    window: String,
    pick: Option<Pick>,
    /// Whether the idle projects are listed in place of their fold.
    fold_open: bool,
    /// What `ctrl+f` narrows the rows to, while finding, and the pick
    /// as it was before.
    pub finding: Option<crate::find::Find<Option<Pick>>>,
    /// Whether a read landed: until then the page says `reading…`.
    pub read: bool,
    /// Whether a read listed the project the window is in. Until one does
    /// (what was kept from an earlier read can leave it out), the pick
    /// waits on it, and the first that does opens the fold it is in.
    here_read: bool,
    /// The first row in view, kept so the pick stays in view.
    top: usize,
    /// Whether the wheel moved the rows since the pick last moved: the
    /// pick is then let out of view until a key moves it.
    scrolled: bool,
    /// The rows in view, as last drawn, for the mouse: each line with the
    /// row it draws, among [`View::rows`].
    drawn: Vec<Option<usize>>,
    /// The pinned agents' names, in their order: the first is `1`.
    pins: Vec<String>,
    /// The pinned agents the last read did not find: not there any more.
    gone: Vec<String>,
    /// Where each pinned agent's name was drawn as a link: its line, its
    /// columns, the agent.
    linked: Vec<(usize, std::ops::Range<usize>, String)>,
}

/// The leads of `projects` in the order the page lists them, the idle
/// ones' fold open: where `alt+n` goes once this project has no one.
pub fn order(projects: Vec<Project>, now: u64) -> Vec<String> {
    let view = View {
        projects,
        fold_open: true,
        read: true,
        ..Default::default()
    };
    (view.shown(now).into_iter())
        .filter_map(|row| match row {
            Shown::Project(at, _) => Some(view.projects[at].lead.clone()),
            Shown::Fold(..) => None,
        })
        .collect()
}

impl View {
    /// The page in a window whose project's lead is `here`: the pick
    /// starts on it.
    pub fn new(here: &str) -> Self {
        View {
            here: here.to_owned(),
            window: here.to_owned(),
            pick: Some(Pick::Project(here.to_owned())),
            ..Default::default()
        }
    }

    /// The page in a window that shows `bot`, one of its project's tasks:
    /// going to the project's lead goes there, not back.
    pub fn showing(mut self, bot: &str) -> Self {
        self.window = bot.to_owned();
        self
    }

    /// The projects as read now. The pick stays on its project; where
    /// that project is gone, on the first row. The first read that lists
    /// the project the window is in opens the fold when that project is
    /// in it, so the pick can start there.
    pub fn set(&mut self, projects: Vec<Project>, now: u64) {
        self.projects = projects;
        self.read = true;
        if !self.here_read
            && let Some(at) = self.at(&self.here)
        {
            self.here_read = true;
            if self.idle(now)[at] {
                self.fold_open = true;
            }
        }
        let rows = self.rows(now);
        let kept = match &self.pick {
            Some(Pick::Project(lead)) if !self.here_read && *lead == self.here => true,
            Some(pick) => rows.iter().any(|row| self.is(row, pick)),
            None => false,
        };
        if !kept {
            self.pick = self.first_project(&rows);
        }
    }

    /// The pinned agents' names, as the window knows them: their rows and
    /// numbers show at once, before any read. A pick on one unpinned
    /// stays on the row that moves up into its place.
    pub fn set_pins(&mut self, pins: Vec<String>, now: u64) {
        let was = self.picked_row(&self.rows(now));
        self.pins = pins;
        if let Some(Pick::Pin(name)) = &self.pick
            && !self.pins.contains(name)
        {
            // The pick stays where it was: on the row that moved up.
            let rows = self.rows(now);
            self.pick = (was.and_then(|at| rows.get(at.min(rows.len().saturating_sub(1)))))
                .map(|row| self.pick_of(*row));
            self.settle(now);
        }
    }

    /// The pinned agents a read looked for and did not find.
    pub fn set_gone(&mut self, gone: Vec<String>) {
        self.gone = gone;
    }

    /// The pins not there any more, as the last read found them, taken
    /// as the page closes, to be shown again as it opens.
    pub fn take_gone(&mut self) -> Vec<String> {
        std::mem::take(&mut self.gone)
    }

    #[cfg(test)]
    pub fn pins(&self) -> &[String] {
        &self.pins
    }

    /// The pick where the page starts it, or where one went: the first
    /// project, the pinned agents passed over, as the pick starts on the
    /// project the window is in.
    fn first_project(&self, rows: &[Row]) -> Option<Pick> {
        (rows.iter().find(|row| matches!(row, Row::Shown(_))))
            .or(rows.first())
            .map(|row| self.pick_of(*row))
    }

    /// The pinned agent `name` as a read found it, with its project.
    fn pinned(&self, name: &str) -> Option<(&Project, &Pin)> {
        self.projects.iter().find_map(|project| {
            (project.pinned.iter())
                .find(|pin| pin.bot == name)
                .map(|pin| (project, pin))
        })
    }

    /// What a pinned agent's row leads with: its title, a lead's its
    /// project's, else nothing (its name stands alone).
    fn pin_title(&self, name: &str) -> String {
        match self.pinned(name) {
            Some((project, _)) if project.lead == name => project.called().to_owned(),
            Some((_, pin)) => pin.title.clone(),
            None => String::new(),
        }
    }

    fn pin_found(&self, at: usize, query: &str) -> bool {
        let name = &self.pins[at];
        crate::find::says(query, name) || crate::find::says(query, &self.pin_title(name))
    }

    /// The pinned agents drawn, by their place among the pins: all, or
    /// while finding those whose title or name says what is typed.
    fn pins_shown(&self) -> Vec<usize> {
        let query = (self.finding.as_ref())
            .map(|find| find.query.as_str())
            .filter(|q| !q.is_empty());
        (0..self.pins.len())
            .filter(|at| query.is_none_or(|query| self.pin_found(*at, query)))
            .collect()
    }

    /// Every row the pick can be on, in order: the pinned agents drawn,
    /// then the projects and the fold.
    fn rows(&self, now: u64) -> Vec<Row> {
        let mut rows: Vec<Row> = self.pins_shown().into_iter().map(Row::Pin).collect();
        rows.extend(self.shown(now).into_iter().map(Row::Shown));
        rows
    }

    /// The projects as last read, taken as the page closes, to be kept.
    pub fn take_projects(&mut self) -> Vec<Project> {
        std::mem::take(&mut self.projects)
    }

    #[cfg(test)]
    pub fn projects(&self) -> &[Project] {
        &self.projects
    }

    fn at(&self, lead: &str) -> Option<usize> {
        self.projects.iter().position(|p| p.lead == lead)
    }

    /// The project picked, which the pane shows: on a pinned agent, its
    /// project, once a read found it.
    pub fn picked(&self) -> Option<&Project> {
        match &self.pick {
            Some(Pick::Project(lead)) => self.at(lead).map(|at| &self.projects[at]),
            Some(Pick::Pin(name)) => self.pinned(name).map(|(project, _)| project),
            _ => None,
        }
    }

    /// Each project's holder: the project whose folder is the nearest
    /// one its own folder is inside of.
    fn holders(&self) -> Vec<Option<usize>> {
        (self.projects.iter())
            .map(|project| {
                (self.projects.iter().enumerate())
                    .filter(|(_, other)| inside(&project.folder, &other.folder))
                    .max_by_key(|(_, other)| other.folder.len())
                    .map(|(at, _)| at)
            })
            .collect()
    }

    /// Which projects fold away: none of whose agents had a turn in seven
    /// days, and that hold no project that had.
    fn idle(&self, now: u64) -> Vec<bool> {
        let holders = self.holders();
        let mut idle: Vec<bool> = (self.projects.iter())
            .map(|project| now.saturating_sub(project.last) >= IDLE_AFTER_MS)
            .collect();
        // A project that had a turn keeps every project holding it out of
        // the fold, up its line of holders.
        for at in 0..self.projects.len() {
            if idle[at] {
                continue;
            }
            let mut up = holders[at];
            for _ in 0..self.projects.len() {
                let Some(holder) = up else { break };
                idle[holder] = false;
                up = holders[holder];
            }
        }
        idle
    }

    /// The rows in order: the projects nested by folder, each holder's in
    /// the order of their titles, then the idle ones' fold. While finding,
    /// the projects that match and those that hold them, with no fold.
    pub fn shown(&self, now: u64) -> Vec<Shown> {
        let holders = self.holders();
        let idle = self.idle(now);
        let keep: Vec<bool> = match (self.finding.as_ref())
            .map(|find| find.query.as_str())
            .filter(|q| !q.is_empty())
        {
            Some(query) => {
                let mut keep: Vec<bool> = self.projects.iter().map(|p| p.found(query)).collect();
                for at in 0..self.projects.len() {
                    if !keep[at] || !self.projects[at].found(query) {
                        continue;
                    }
                    let mut up = holders[at];
                    for _ in 0..self.projects.len() {
                        let Some(holder) = up else { break };
                        keep[holder] = true;
                        up = holders[holder];
                    }
                }
                keep
            }
            None => vec![true; self.projects.len()],
        };
        let finding = self.finding.is_some();
        // Each group's children in the order of their titles, then names.
        let order = |ats: &mut Vec<usize>| {
            ats.sort_by_cached_key(|at| {
                let project = &self.projects[*at];
                (project.called().to_lowercase(), project.lead.clone())
            })
        };
        let mut rows = Vec::new();
        // The active projects, nested; then the idle ones, nested among
        // themselves, under the fold.
        for folded in [false, true] {
            let member = |at: usize| keep[at] && (finding || idle[at] == folded);
            let mut tops: Vec<usize> = (0..self.projects.len())
                .filter(|at| member(*at))
                .filter(|at| holders[*at].is_none_or(|holder| !member(holder)))
                .collect();
            order(&mut tops);
            let mut listed = Vec::new();
            let mut stack: Vec<(usize, usize)> = tops.into_iter().rev().map(|at| (at, 0)).collect();
            while let Some((at, depth)) = stack.pop() {
                listed.push(Shown::Project(at, depth + usize::from(folded)));
                let mut children: Vec<usize> = (0..self.projects.len())
                    .filter(|child| member(*child) && holders[*child] == Some(at))
                    .collect();
                order(&mut children);
                stack.extend(children.into_iter().rev().map(|child| (child, depth + 1)));
            }
            if finding {
                rows.extend(listed);
                break;
            }
            match folded {
                false => rows.extend(listed),
                true if listed.is_empty() => {}
                true => {
                    rows.push(Shown::Fold(listed.len(), self.fold_open));
                    if self.fold_open {
                        rows.extend(listed);
                    }
                }
            }
        }
        rows
    }

    fn is(&self, row: &Row, pick: &Pick) -> bool {
        match (row, pick) {
            (Row::Shown(Shown::Project(at, _)), Pick::Project(lead)) => {
                self.projects[*at].lead == *lead
            }
            (Row::Shown(Shown::Fold(..)), Pick::Fold) => true,
            (Row::Pin(at), Pick::Pin(name)) => self.pins[*at] == *name,
            _ => false,
        }
    }

    fn pick_of(&self, row: Row) -> Pick {
        match row {
            Row::Shown(Shown::Project(at, _)) => Pick::Project(self.projects[at].lead.clone()),
            Row::Shown(Shown::Fold(..)) => Pick::Fold,
            Row::Pin(at) => Pick::Pin(self.pins[at].clone()),
        }
    }

    /// The row the pick is on.
    fn picked_row(&self, rows: &[Row]) -> Option<usize> {
        let pick = self.pick.as_ref()?;
        rows.iter().position(|row| self.is(row, pick))
    }

    /// Whether the idle projects' fold is open and the pick is in it.
    fn in_fold(&self, rows: &[Row]) -> bool {
        let Some(at) = self.picked_row(rows) else {
            return false;
        };
        let fold = (rows.iter()).position(|row| matches!(row, Row::Shown(Shown::Fold(..))));
        self.fold_open && fold.is_some_and(|fold| at >= fold)
    }

    /// Where pressing a pinned agent's number, or `enter` on its row,
    /// goes: to it, as the name the pin holds says, before any read; the
    /// view closes on the agent the window is on; one a read did not find
    /// is answered.
    fn go_pin(&self, name: &str) -> Act {
        match () {
            _ if self.gone.iter().any(|gone| gone == name) => Act::Gone(name.to_owned()),
            _ if name == self.window => Act::Close,
            _ => Act::Go(name.to_owned()),
        }
    }

    /// A key, the page having the keys.
    pub fn key(&mut self, key: &KeyEvent, now: u64) -> Act {
        self.scrolled = false;
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        // Finding: what is typed narrows the rows; the arrows still pick.
        if let Some(find) = &mut self.finding {
            match find.key(key) {
                crate::find::Typed::Dropped => {
                    // The rows and the pick as they were.
                    self.pick = self.finding.take().and_then(|find| find.before);
                    self.settle(now);
                    return Act::Stay;
                }
                crate::find::Typed::Changed => {
                    self.found(now);
                    return Act::Stay;
                }
                crate::find::Typed::Held => return Act::Stay,
                crate::find::Typed::Other => {}
            }
        }
        let rows = self.rows(now);
        let at = self.picked_row(&rows);
        let plain = !ctrl && !key.modifiers.contains(KeyModifiers::ALT);
        match key.code {
            KeyCode::Up | KeyCode::Down if !rows.is_empty() => {
                let last = rows.len() - 1;
                let next = match (key.code, at) {
                    (KeyCode::Up, Some(at)) => at.saturating_sub(1),
                    (KeyCode::Down, Some(at)) => (at + 1).min(last),
                    (KeyCode::Up, None) => last,
                    _ => 0,
                };
                self.pick = Some(self.pick_of(rows[next]));
                Act::Stay
            }
            KeyCode::Enter | KeyCode::Right => match at.map(|at| rows[at]) {
                Some(Row::Shown(Shown::Fold(_, open))) => {
                    if !open {
                        self.fold_open = true;
                    } else if key.code == KeyCode::Enter {
                        self.fold_open = false;
                    }
                    Act::Stay
                }
                Some(Row::Shown(Shown::Project(at, _))) => {
                    let lead = self.projects[at].lead.clone();
                    match lead == self.window {
                        true => Act::Close,
                        false => Act::Go(lead),
                    }
                }
                Some(Row::Pin(at)) => self.go_pin(&self.pins[at].clone()),
                None => Act::Stay,
            },
            KeyCode::Left => {
                // In the opened fold, it folds; else to the holder. On a
                // pinned agent, nothing.
                if self.in_fold(&rows) {
                    self.fold_open = false;
                    self.pick = Some(Pick::Fold);
                    return Act::Stay;
                }
                if let Some(Row::Shown(Shown::Project(at, _))) = at.map(|at| rows[at]) {
                    let holders = self.holders();
                    if let Some(holder) = holders[at]
                        && rows.iter().any(
                            |row| matches!(row, Row::Shown(Shown::Project(h, _)) if *h == holder),
                        )
                    {
                        self.pick = Some(Pick::Project(self.projects[holder].lead.clone()));
                    }
                }
                Act::Stay
            }
            // A pinned agent's number goes to it, wherever the pick is.
            KeyCode::Char(c @ '1'..='9') if plain => {
                let n = c as usize - '0' as usize;
                match self.pins.get(n - 1) {
                    Some(name) => self.go_pin(&name.clone()),
                    None => Act::Stay,
                }
            }
            // `p` pins the picked project's lead, or unpins the picked
            // pinned agent.
            KeyCode::Char('p') if plain => match &self.pick {
                Some(Pick::Project(lead)) if self.at(lead).is_some() => Act::Pin(lead.clone()),
                Some(Pick::Pin(name)) => Act::Pin(name.clone()),
                _ => Act::Stay,
            },
            _ if crate::find::opens(key) => {
                self.finding = Some(crate::find::Find::new(self.pick.clone()));
                Act::Stay
            }
            KeyCode::Char('c') if ctrl => Act::Close,
            KeyCode::Esc => Act::Close,
            _ => Act::Stay,
        }
    }

    /// The pick on a row the find still shows, else its first.
    fn settle(&mut self, now: u64) {
        let rows = self.rows(now);
        if self.picked_row(&rows).is_none() {
            self.pick = rows.first().map(|row| self.pick_of(*row));
        }
    }

    /// Words pasted while finding, as if typed.
    pub fn paste(&mut self, text: &str, now: u64) {
        let Some(find) = &mut self.finding else {
            return;
        };
        find.paste(text);
        self.scrolled = false;
        self.found(now);
    }

    /// The pick after what is found changed: it stays on a row that
    /// matches, else goes to the first that does, not to one kept only
    /// because it holds a match.
    fn found(&mut self, now: u64) {
        let rows = self.rows(now);
        let query = (self.finding.as_ref())
            .map(|find| find.query.clone())
            .unwrap_or_default();
        let matches = |row: &Row| match row {
            Row::Shown(Shown::Project(at, _)) => self.projects[*at].found(&query),
            Row::Shown(Shown::Fold(..)) => false,
            Row::Pin(at) => self.pin_found(*at, &query),
        };
        let picked = self.picked_row(&rows).map(|at| rows[at]);
        if query.is_empty() || picked.as_ref().is_some_and(matches) {
            return self.settle(now);
        }
        match rows.iter().find(|row| matches(row)) {
            Some(row) => self.pick = Some(self.pick_of(*row)),
            None => self.settle(now),
        }
    }

    /// A click on the line `line` of the page as drawn: it picks the row
    /// and presses `enter` on it.
    pub fn click(&mut self, line: usize, now: u64) -> Act {
        let Some(Some(row)) = self.drawn.get(line).copied() else {
            return Act::Stay;
        };
        let rows = self.rows(now);
        let Some(row) = rows.get(row).copied() else {
            return Act::Stay;
        };
        self.pick = Some(self.pick_of(row));
        self.key(&KeyEvent::from(KeyCode::Enter), now)
    }

    /// The wheel: the rows in view move, the pick stays.
    pub fn scroll(&mut self, down: bool) {
        self.scrolled = true;
        self.top = match down {
            true => self.top + 3,
            false => self.top.saturating_sub(3),
        };
    }

    /// The find field, while finding: how many rows say what is typed,
    /// of how many.
    pub fn field(&self) -> Option<crate::find::Field> {
        let find = self.finding.as_ref()?;
        let left = (self.projects.iter())
            .filter(|project| project.found(&find.query))
            .count()
            + (0..self.pins.len())
                .filter(|at| self.pin_found(*at, &find.query))
                .count();
        Some(crate::find::Field::rows(
            &find.query,
            left,
            self.projects.len() + self.pins.len(),
        ))
    }

    /// The page's keys, as its title row says them: with agents pinned,
    /// their numbers and `p`, which unpins where the pick is on one.
    pub fn keys(&self) -> crate::hint::Keys {
        use crate::hint::{Enter, Esc, Keys};
        if self.finding.is_some() {
            return Keys::new().pick().enter(Enter::Open).esc(Esc::Cancel);
        }
        let mut keys = Keys::new().pick().enter(Enter::Open);
        if !self.pins.is_empty() {
            keys = match self.pins.len() {
                1 => keys.key("1", "open"),
                n => keys.key(format!("1–{n}"), "open"),
            };
            keys = match &self.pick {
                Some(Pick::Pin(_)) => keys.key("p", "unpin"),
                Some(Pick::Project(lead)) if self.pins.contains(lead) => keys.key("p", "unpin"),
                Some(Pick::Project(_)) => keys.key("p", "pin"),
                _ => keys,
            };
        }
        keys.find().esc(Esc::Close)
    }

    /// Where each pinned agent's name is drawn as a link, as last drawn:
    /// its line of the page, its columns, and the agent.
    pub fn links(&self) -> &[(usize, std::ops::Range<usize>, String)] {
        &self.linked
    }

    /// The page, `width` wide and `height` tall: its title row with its
    /// keys, the head, a blank row, the pinned agents under a muted
    /// `Pinned` and a blank row, then the rows, the pick reversed and kept
    /// in view. The pinned agents are drawn before any read.
    pub fn lines(&mut self, width: usize, height: usize, now: u64) -> Vec<Line<'static>> {
        let keys = self.keys();
        let name = "projects";
        let room = width.saturating_sub(1 + name.width() + 3);
        let mut title = vec![Span::raw(" "), Span::raw(name).bold(), Span::raw("   ")];
        title.extend(crate::hint::spans(&keys.fit(room)));
        // The row under the title is the find field's: empty at rest,
        // drawn over while finding, so nothing moves when it opens.
        let mut out = vec![
            Line::from(title),
            Line::default(),
            head(&self.projects, width),
            Line::default(),
        ];
        self.drawn = vec![None, None, None, None];
        self.linked.clear();
        let rows = self.rows(now);
        let picked = self.picked_row(&rows);
        let pins = self.pins_shown();
        // Under the head, the pinned rows and the projects scroll as one
        // list, so neither hides the other on a short screen.
        let mut body: Vec<Body> = Vec::new();
        if !pins.is_empty() {
            body.push(Body::Line(
                Line::from(vec![Span::raw(" "), Span::raw("Pinned").dim()]),
                None,
                None,
            ));
            let lines = self.pin_rows(&pins, width.saturating_sub(1));
            for (i, (line, link)) in lines.into_iter().enumerate() {
                body.push(Body::Line(line, Some(i), link));
            }
            body.push(Body::Line(Line::default(), None, None));
        }
        let shown = match self.read {
            true => self.shown(now),
            false => Vec::new(),
        };
        let first = pins.len();
        let projects_at = body.len();
        if !self.read {
            body.push(Body::Line(Line::from(" reading…").dim(), None, None));
        } else if shown.is_empty() {
            let said = match &self.finding {
                Some(find) if find.typed() && pins.is_empty() => {
                    Some(format!(" nothing here matches {}", find.query))
                }
                Some(find) if find.typed() => None,
                _ => Some(" no project yet".to_owned()),
            };
            if let Some(said) = said {
                body.push(Body::Line(Line::from(said).dim(), None, None));
            }
        } else {
            body.extend((0..shown.len()).map(Body::Shown));
        }
        // Where the pick is drawn among them.
        let picked_at = picked.map(|at| match at.checked_sub(first) {
            None => 1 + at,
            Some(at) => projects_at + at,
        });
        let room_rows = height.saturating_sub(out.len()).max(1);
        // The pick stays in view, unless the wheel moved the rows since.
        if let Some(at) = picked_at.filter(|_| !self.scrolled) {
            if at < self.top {
                self.top = at;
            } else if at >= self.top + room_rows {
                self.top = at + 1 - room_rows;
            }
        }
        self.top = self.top.min(body.len().saturating_sub(room_rows));
        let labels: Vec<usize> = (shown.iter())
            .map(|row| match row {
                Shown::Project(at, depth) => label_width(&self.projects[*at], *depth),
                Shown::Fold(..) => 0,
            })
            .collect();
        let counted: Vec<usize> = (shown.iter())
            .map(|row| match row {
                Shown::Project(at, _) => spans_width(&counts(&self.projects[*at].counts)),
                Shown::Fold(..) => 0,
            })
            .collect();
        // The counts share a column after the widest title, as narrow as
        // leaves the widest counts room.
        let room = width.saturating_sub(1);
        let widest = counted.iter().copied().max().unwrap_or(0);
        let column =
            (labels.iter().copied().max().unwrap_or(0)).min(room.saturating_sub(2 + widest));
        let top = self.top;
        for (at, part) in body.into_iter().enumerate().skip(top).take(room_rows) {
            let (mut line, row) = match part {
                Body::Line(line, row, link) => {
                    if let Some((columns, bot)) = link {
                        self.linked.push((out.len(), columns, bot));
                    }
                    (line, row)
                }
                Body::Shown(i) => {
                    let line = match shown[i] {
                        Shown::Fold(n, open) => {
                            let sign = if open { "▾" } else { "▸" };
                            let noun = if n == 1 { "project" } else { "projects" };
                            Line::from(vec![
                                Span::raw(" "),
                                Span::raw(format!("{sign} {n} idle {noun}")).dim(),
                            ])
                        }
                        Shown::Project(at, depth) => self.row(at, depth, column, room),
                    };
                    (line, Some(first + i))
                }
            };
            if Some(at) == picked_at {
                crate::list::pick(&mut line);
            }
            out.push(line);
            self.drawn.push(row);
        }
        out
    }

    /// The pinned agents' rows (`pins`, by their place among them), in
    /// `room` columns: the number, the agent's mark, its title (bold on
    /// the agent the window is on) and its name in one column after the
    /// widest title, a link, but the window's own. Before a read found
    /// it, the number and the name; one not there any more, its name in
    /// plain muted words. Too long, the name goes first, then the title is
    /// cut at a word; the number and the mark stay. Each with where its
    /// name is drawn as a link.
    #[allow(clippy::type_complexity)]
    fn pin_rows(
        &self,
        pins: &[usize],
        room: usize,
    ) -> Vec<(Line<'static>, Option<(std::ops::Range<usize>, String)>)> {
        // The number and its space, then the mark and its space.
        let fixed = 1 + 2 + 2;
        let found: Vec<Option<(String, String)>> = (pins.iter())
            .map(|at| {
                let name = &self.pins[*at];
                (self.pinned(name)).map(|(_, pin)| (pin.state.clone(), self.pin_title(name)))
            })
            .collect();
        let names = (pins.iter())
            .map(|at| self.pins[*at].width())
            .max()
            .unwrap_or(0);
        let column = (found.iter().flatten())
            .filter(|(_, title)| !title.is_empty())
            .map(|(_, title)| fixed + title.width() + 2)
            .max()
            .unwrap_or(0)
            .min(room.saturating_sub(names));
        let mut out = Vec::with_capacity(pins.len());
        for (at, found) in pins.iter().zip(found) {
            let name = self.pins[*at].clone();
            let ours = name == self.window;
            let mut spans = vec![Span::raw(format!(" {} ", at + 1))];
            let named = |name: &str| match ours {
                true => Span::raw(name.to_owned()),
                false => Span::raw(name.to_owned()).bold().fg(Color::Blue),
            };
            let gone = self.gone.contains(&name);
            let (state, title) = match found {
                _ if gone => {
                    spans.push(Span::raw(name).dim());
                    out.push((Line::from(spans), None));
                    continue;
                }
                None => (None, String::new()),
                Some((state, title)) => (Some(state), title),
            };
            if let Some(state) = &state {
                spans.push(status::mark(state));
            }
            let at_name = spans_width(&spans);
            if title.is_empty() {
                spans.push(named(&name));
                let link = (!ours).then(|| (at_name..at_name + name.width(), name));
                out.push((Line::from(spans), link));
                continue;
            }
            // The name gives way first, then the title is cut.
            let whole = fixed + title.width() + 2 + name.width() <= room;
            let said = match whole {
                true => title,
                false => crate::title::cut(&title, room.saturating_sub(fixed)),
            };
            spans.push(match ours {
                true => Span::raw(said.clone()).bold(),
                false => Span::raw(said.clone()),
            });
            let mut link = None;
            if whole {
                let drawn = fixed + said.width();
                let at = column.max(drawn + 2);
                spans.push(Span::raw(" ".repeat(at - drawn)));
                spans.push(named(&name));
                link = (!ours).then(|| (at..at + name.width(), name));
            }
            out.push((Line::from(spans), link));
        }
        out
    }

    /// A project's row: indent, its mark, its title (bold for the project
    /// the window is in), a muted `superlead`, then its counts in the
    /// column. Too long, the counts go first, then the title is cut at a
    /// word; the mark stays.
    fn row(&self, at: usize, depth: usize, column: usize, room: usize) -> Line<'static> {
        let project = &self.projects[at];
        let indent = 2 * depth;
        let counts = counts(&project.counts);
        let tail = match project.superlead {
            true => "  superlead",
            false => "",
        };
        let label = label_width(project, depth);
        let goes = label + 2 + spans_width(&counts) > room;
        // Where the title must be cut: room for the indent, the mark and
        // its space, and the `superlead` where it fits.
        let fixed = indent + 2;
        // The muted `superlead` gives way before the title is cut.
        let mut tail = tail;
        if fixed + project.called().width() + tail.width() > room {
            tail = "";
        }
        let called = crate::title::cut(project.called(), room.saturating_sub(fixed + tail.width()));
        let mut spans = vec![
            Span::raw(format!(" {}", " ".repeat(indent))),
            status::mark(&project.state),
        ];
        spans.push(match project.lead == self.here {
            true => Span::raw(called.clone()).bold(),
            false => Span::raw(called.clone()),
        });
        if !tail.is_empty() {
            spans.push(Span::raw(tail).dim());
        }
        if !goes && !counts.is_empty() {
            let drawn = fixed + called.width() + tail.width();
            let at = match drawn <= column {
                true => column,
                false => drawn,
            };
            spans.push(Span::raw(" ".repeat(at - drawn + 2)));
            spans.extend(counts);
        }
        Line::from(spans)
    }
}

/// Whether `folder` is inside `other`, not `other` itself.
fn inside(folder: &str, other: &str) -> bool {
    !other.is_empty()
        && folder.len() > other.len()
        && folder.starts_with(other)
        && (other.ends_with('/') || folder.as_bytes()[other.len()] == b'/')
}

/// The columns a project's row takes before its counts.
fn label_width(project: &Project, depth: usize) -> usize {
    let tail = if project.superlead {
        "  superlead".width()
    } else {
        0
    };
    2 * depth + 2 + project.called().width() + tail
}

fn spans_width(spans: &[Span]) -> usize {
    spans.iter().map(|span| span.content.width()).sum()
}

/// A project's counts as the footer writes them, two spaces apart: `? 1`,
/// `✗ 1`, `● 2`. Idle and done agents are not counted.
pub fn counts(counts: &Counts) -> Vec<Span<'static>> {
    let mut out = Vec::new();
    for (group, state) in [
        (Group::NeedsYou, "needs you"),
        (Group::Failed, "failed"),
        (Group::Working, "working"),
    ] {
        let n = counts.of(group);
        if n == 0 {
            continue;
        }
        if !out.is_empty() {
            out.push(Span::raw("  "));
        }
        out.push(status::sign(state));
        out.push(Span::raw(format!(" {n}")).dim());
    }
    out
}

/// The head: `every project`, then a count of the agents that need the
/// person, failed or work, across every project.
fn head(projects: &[Project], width: usize) -> Line<'static> {
    let total = |group| -> usize { projects.iter().map(|p| p.counts.of(group)).sum() };
    let mut spans = vec![Span::raw(" "), Span::raw("every project").bold()];
    let mut used = 1 + "every project".width();
    for group in [Group::NeedsYou, Group::Failed, Group::Working] {
        let n = total(group);
        if n == 0 {
            continue;
        }
        let word = match group {
            Group::NeedsYou if n == 1 => "needs you",
            Group::NeedsYou => "need you",
            Group::Failed => "failed",
            _ => "working",
        };
        let said = format!(" · {n} {word}");
        if used + said.width() > width {
            break;
        }
        used += said.width();
        spans.push(Span::raw(said).dim());
    }
    Line::from(spans)
}

#[cfg(test)]
mod tests {
    use super::*;
    use ratatui::style::{Modifier, Style};

    const NOW: u64 = 100 * IDLE_AFTER_MS;

    fn project(lead: &str, title: &str, folder: &str, counts: (usize, usize, usize)) -> Project {
        Project {
            lead: lead.into(),
            name: lead.trim_end_matches(".lead").into(),
            title: title.into(),
            folder: folder.into(),
            counts: Counts {
                needs_you: counts.0,
                failed: counts.1,
                working: counts.2,
                idle: 0,
                done: 0,
            },
            state: match counts {
                (n, _, _) if n > 0 => "needs you".into(),
                (_, n, _) if n > 0 => "failed".into(),
                (_, _, n) if n > 0 => "working".into(),
                _ => "idle".into(),
            },
            last: NOW,
            ..Default::default()
        }
    }

    /// The page as the screen shows it, marks and all, no styles.
    fn text(view: &mut View, width: usize) -> Vec<String> {
        (view.lines(width, 40, NOW).iter())
            .map(|line| line.to_string().trim_end().to_owned())
            .collect()
    }

    /// The Rulebook's screen: a superlead with two projects, a third
    /// outside it, four idle ones folded.
    fn rulebook() -> View {
        let mut books = project("books.lead", "Bookkeeping", "/h/books", (0, 0, 0));
        books.superlead = true;
        let mut projects = vec![
            books,
            project("tally.lead", "Tally invoices", "/h/books/tally", (1, 0, 2)),
            project("ledger.lead", "Ledger import", "/h/books/ledger", (0, 0, 1)),
            project("site.lead", "Site redesign", "/h/site", (1, 0, 0)),
        ];
        for (i, name) in ["a", "b", "c", "d"].into_iter().enumerate() {
            let mut idle = project(
                &format!("{name}.lead"),
                "",
                &format!("/h/old/{name}"),
                (0, 0, 0),
            );
            idle.last = NOW - IDLE_AFTER_MS - i as u64;
            projects.push(idle);
        }
        let mut view = View::new("tally.lead");
        view.set(projects, NOW);
        view
    }

    #[test]
    fn a_superlead_with_two_projects_and_a_third_outside_it_draw_as_the_rulebook_has_them() {
        let mut view = rulebook();
        assert_eq!(
            text(&mut view, 70),
            [
                " projects   ↑↓ pick · enter open · ctrl+f find · esc close",
                "",
                " every project · 2 need you · 3 working",
                "",
                " ○ Bookkeeping  superlead",
                "   ● Ledger import         ● 1",
                "   ? Tally invoices        ? 1  ● 2",
                " ? Site redesign           ? 1",
                " ▸ 4 idle projects",
            ]
        );
        assert_eq!(view.picked().unwrap().lead, "tally.lead");
    }

    #[test]
    fn no_row_moves_when_an_agent_starts_or_ends_a_turn() {
        let mut view = rulebook();
        let before: Vec<Shown> = view.shown(NOW);
        // Ledger import's agent ends its turn and the site's starts one:
        // marks and counts change, the order does not, nor the pick.
        let mut projects = view.projects().to_vec();
        projects[2].counts.working = 0;
        projects[2].state = "idle".into();
        projects[3].counts.working = 3;
        projects[0].counts.failed = 1;
        projects[0].state = "failed".into();
        view.set(projects, NOW);
        assert_eq!(view.shown(NOW), before);
        assert_eq!(view.picked().unwrap().lead, "tally.lead");
        let drawn = text(&mut view, 70);
        assert_eq!(drawn[4], " ✗ Bookkeeping  superlead  ✗ 1");
        assert_eq!(drawn[5], "   ○ Ledger import");
    }

    #[test]
    fn the_keys_move_the_pick_go_in_and_come_out() {
        let mut view = rulebook();
        let key = |code| KeyEvent::from(code);
        // ↑ to Ledger, ← to the superlead that holds it, ← at the top
        // does nothing.
        assert_eq!(view.key(&key(KeyCode::Up), NOW), Act::Stay);
        assert_eq!(view.picked().unwrap().lead, "ledger.lead");
        view.key(&key(KeyCode::Left), NOW);
        assert_eq!(view.picked().unwrap().lead, "books.lead");
        view.key(&key(KeyCode::Left), NOW);
        assert_eq!(view.picked().unwrap().lead, "books.lead");
        // enter on another project goes to its lead; on this one closes.
        assert_eq!(
            view.key(&key(KeyCode::Enter), NOW),
            Act::Go("books.lead".into())
        );
        let mut view = rulebook();
        assert_eq!(view.key(&key(KeyCode::Right), NOW), Act::Close);
        // From one of its tasks' windows, its lead is gone to.
        let mut task = rulebook();
        task.window = "tally.readme".into();
        assert_eq!(
            task.key(&key(KeyCode::Right), NOW),
            Act::Go("tally.lead".into())
        );
        // → on the fold opens it, ← inside it folds it again.
        for _ in 0..3 {
            view.key(&key(KeyCode::Down), NOW);
        }
        assert_eq!(view.pick, Some(Pick::Fold));
        view.key(&key(KeyCode::Right), NOW);
        assert!(text(&mut view, 70).contains(&" ▾ 4 idle projects".to_owned()));
        view.key(&key(KeyCode::Down), NOW);
        assert_eq!(view.picked().unwrap().lead, "a.lead");
        assert_eq!(text(&mut view, 70)[9], "   ○ a");
        view.key(&key(KeyCode::Left), NOW);
        assert_eq!(view.pick, Some(Pick::Fold));
        assert_eq!(text(&mut view, 70).len(), 9);
        assert_eq!(view.key(&key(KeyCode::Tab), NOW), Act::Stay);
        assert_eq!(view.key(&key(KeyCode::Esc), NOW), Act::Close);
    }

    #[test]
    fn a_pasted_find_picks_the_first_match_and_the_wheel_lets_the_pick_out_of_view() {
        let mut view = rulebook();
        view.key(
            &KeyEvent::new(KeyCode::Char('f'), KeyModifiers::CONTROL),
            NOW,
        );
        view.paste("Site", NOW);
        assert_eq!(view.picked().unwrap().lead, "site.lead");
        assert_eq!(
            view.key(&KeyEvent::from(KeyCode::Enter), NOW),
            Act::Go("site.lead".into())
        );
        // Four rows in view, the pick on the first: the wheel moves the
        // rows past it, and a key brings it back into view.
        let mut view = rulebook();
        view.key(&KeyEvent::from(KeyCode::Right), NOW);
        for _ in 0..3 {
            view.key(&KeyEvent::from(KeyCode::Up), NOW);
        }
        view.fold_open = true;
        assert_eq!(view.picked().unwrap().lead, "books.lead");
        let rows = |view: &mut View| -> Vec<String> {
            (view.lines(70, 8, NOW).iter())
                .map(|line| line.to_string().trim_end().to_owned())
                .collect()
        };
        assert_eq!(rows(&mut view)[4], " ○ Bookkeeping  superlead");
        view.scroll(true);
        assert_eq!(rows(&mut view)[4], " ? Site redesign           ? 1");
        view.key(&KeyEvent::from(KeyCode::Down), NOW);
        assert_eq!(rows(&mut view)[4], "   ● Ledger import         ● 1");
    }

    #[test]
    fn find_narrows_by_title_and_name_keeping_what_holds_a_match() {
        let mut view = rulebook();
        let before = view.picked().map(|project| project.lead.clone());
        let ctrl_f = KeyEvent::new(KeyCode::Char('f'), KeyModifiers::CONTROL);
        view.key(&ctrl_f, NOW);
        for c in "ledg".chars() {
            view.key(&KeyEvent::from(KeyCode::Char(c)), NOW);
        }
        let drawn = text(&mut view, 70);
        assert_eq!(drawn[0], " projects   ↑↓ pick · enter open · esc cancel");
        // The row under the title is the field's, drawn over by the
        // window; the rows start where they do at rest.
        assert_eq!(drawn[1], "");
        assert_eq!(
            view.field(),
            Some(crate::find::Field::rows("ledg", 1, view.projects().len()))
        );
        assert_eq!(
            &drawn[4..],
            [
                " ○ Bookkeeping  superlead",
                "   ● Ledger import         ● 1"
            ]
        );
        // The pick moved to the first match, not the row that holds it;
        // esc drops the find first.
        assert_eq!(view.picked().unwrap().lead, "ledger.lead");
        assert_eq!(view.key(&KeyEvent::from(KeyCode::Esc), NOW), Act::Stay);
        assert_eq!(view.finding, None);
        // The pick as it was before the find.
        assert_eq!(view.picked().map(|project| project.lead.clone()), before);
        assert_eq!(view.key(&KeyEvent::from(KeyCode::Esc), NOW), Act::Close);
        // An idle project is found too.
        view.key(&ctrl_f, NOW);
        view.key(&KeyEvent::from(KeyCode::Char('d')), NOW);
        assert!(text(&mut view, 70).iter().any(|line| line == " ○ d"));
    }

    #[test]
    fn too_narrow_the_counts_go_then_the_title_is_cut_and_the_mark_stays() {
        let mut view = rulebook();
        let drawn = text(&mut view, 24);
        assert_eq!(drawn[6], "   ? Tally invoices");
        assert_eq!(drawn[4], " ○ Bookkeeping");
        let drawn = text(&mut view, 16);
        assert_eq!(drawn[6], "   ? Tally…");
        assert_eq!(drawn[4], " ○ Bookkeeping");
    }

    #[test]
    fn the_project_the_window_is_in_opens_the_fold_when_idle() {
        let mut view = rulebook();
        let mut projects = view.projects().to_vec();
        projects[1].last = 0;
        projects[1].counts = Counts {
            needs_you: 0,
            failed: 0,
            working: 0,
            idle: 0,
            done: 0,
        };
        let mut fresh = View::new("tally.lead");
        fresh.set(projects, NOW);
        assert_eq!(fresh.picked().unwrap().lead, "tally.lead");
        assert!(text(&mut fresh, 70).contains(&" ▾ 5 idle projects".to_owned()));
        view.read = true;
    }

    #[test]
    fn the_pick_waits_on_the_window_s_project_until_a_read_lists_it() {
        let mut projects = rulebook().projects().to_vec();
        projects[1].last = 0;
        projects[1].counts = Counts {
            needs_you: 0,
            failed: 0,
            working: 0,
            idle: 0,
            done: 0,
        };
        let here = projects.remove(1);
        // What another window kept, without this window's project: the
        // pick stays on it, though no row shows it yet.
        let mut view = View::new("tally.lead");
        view.set(projects.clone(), NOW);
        assert!(view.read);
        assert!(view.picked().is_none());
        // The read that lists it picks it, and opens the fold it is in.
        projects.insert(1, here);
        view.set(projects, NOW);
        assert_eq!(view.picked().unwrap().lead, "tally.lead");
        assert!(text(&mut view, 70).contains(&" ▾ 5 idle projects".to_owned()));
    }

    #[test]
    fn a_project_holding_one_that_had_a_turn_does_not_fold() {
        let mut view = rulebook();
        let mut projects = view.projects().to_vec();
        projects[0].last = 0;
        view.set(projects, NOW);
        assert!(text(&mut view, 70).contains(&" ○ Bookkeeping  superlead".to_owned()));
        assert!(!inside("/h/booksy", "/h/books"));
        assert!(inside("/h/books/x", "/h/books"));
        assert!(!inside("/h/books", "/h/books"));
    }

    /// The Rulebook's screen with three pinned: a task that needs the
    /// person, a lead that works and a task that is done.
    fn pinned() -> View {
        let mut view = rulebook();
        let mut projects = view.projects().to_vec();
        projects[1].pinned = vec![
            Pin {
                bot: "tally.readme".into(),
                title: "Check the README examples".into(),
                state: "needs you".into(),
            },
            Pin {
                bot: "tally.halves".into(),
                title: "Round half cents to even".into(),
                state: "done".into(),
            },
        ];
        projects[2].pinned = vec![Pin {
            bot: "ledger.lead".into(),
            title: String::new(),
            state: "working".into(),
        }];
        view.set_pins(
            vec![
                "tally.readme".into(),
                "ledger.lead".into(),
                "tally.halves".into(),
            ],
            NOW,
        );
        view.set(projects, NOW);
        view
    }

    fn key(view: &mut View, code: KeyCode) -> Act {
        view.key(&KeyEvent::from(code), NOW)
    }

    #[test]
    fn three_pinned_are_numbered_rows_under_a_muted_pinned_above_the_projects() {
        let mut view = pinned();
        assert_eq!(
            text(&mut view, 80),
            [
                " projects   ↑↓ pick · enter open · 1–3 open · p pin · ctrl+f find · esc close",
                "",
                " every project · 2 need you · 3 working",
                "",
                " Pinned",
                " 1 ? Check the README examples  tally.readme",
                " 2 ● Ledger import              ledger.lead",
                " 3 ✓ Round half cents to even   tally.halves",
                "",
                " ○ Bookkeeping  superlead",
                "   ● Ledger import         ● 1",
                "   ? Tally invoices        ? 1  ● 2",
                " ? Site redesign           ? 1",
                " ▸ 4 idle projects",
            ]
        );
        // The number in the text's own weight, the heading muted, the
        // name a link, as an agent's name is everywhere.
        let lines = view.lines(80, 40, NOW);
        let heading = &lines[4].spans[1];
        assert!(heading.style.add_modifier.contains(Modifier::DIM));
        let row = &lines[5];
        assert_eq!(row.spans[0].content, " 1 ");
        assert_eq!(row.spans[0].style, Style::default());
        let name = row.spans.last().unwrap();
        assert_eq!(name.content, "tally.readme");
        assert_eq!(name.style.fg, Some(Color::Blue));
        assert_eq!(
            view.links()[0],
            (5, 32..32 + "tally.readme".len(), "tally.readme".to_owned())
        );
        // The pick starts on the project the window is in.
        assert_eq!(view.picked().unwrap().lead, "tally.lead");
    }

    #[test]
    fn with_nothing_pinned_the_page_reads_as_it_did() {
        let mut bare = rulebook();
        let mut emptied = pinned();
        emptied.set_pins(Vec::new(), NOW);
        assert_eq!(text(&mut emptied, 70), text(&mut bare, 70));
        assert_eq!(emptied.picked().unwrap().lead, "tally.lead");
        // A number with nothing pinned does nothing.
        assert_eq!(key(&mut emptied, KeyCode::Char('1')), Act::Stay);
    }

    #[test]
    fn a_number_opens_its_agent_at_once_before_any_read() {
        let mut view = View::new("tally.lead");
        view.set_pins(vec!["shop.fix".into(), "tally.halves".into()], NOW);
        let drawn = text(&mut view, 60);
        assert_eq!(
            &drawn[4..],
            [" Pinned", " 1 shop.fix", " 2 tally.halves", "", " reading…"]
        );
        assert_eq!(
            key(&mut view, KeyCode::Char('2')),
            Act::Go("tally.halves".into())
        );
        // Wherever the pick is; nothing past the last.
        assert_eq!(key(&mut view, KeyCode::Char('3')), Act::Stay);
        // On the agent the window is on, the page closes.
        let mut on = View::new("tally.lead").showing("shop.fix");
        on.set_pins(vec!["shop.fix".into()], NOW);
        assert_eq!(key(&mut on, KeyCode::Char('1')), Act::Close);
        // A keyboard that needs shift for a number is taken at its word.
        assert_eq!(
            view.key(&KeyEvent::new(KeyCode::Char('1'), KeyModifiers::SHIFT), NOW),
            Act::Go("shop.fix".into())
        );
    }

    #[test]
    fn up_from_the_first_project_goes_into_pinned_and_the_pane_follows() {
        let mut view = pinned();
        // From Tally invoices: Ledger, Bookkeeping, then the last pin.
        for _ in 0..3 {
            key(&mut view, KeyCode::Up);
        }
        assert_eq!(view.pick, Some(Pick::Pin("tally.halves".into())));
        assert_eq!(view.picked().unwrap().lead, "tally.lead");
        assert_eq!(
            text(&mut view, 80)[0],
            " projects   ↑↓ pick · enter open · 1–3 open · p unpin · ctrl+f find · esc close"
        );
        key(&mut view, KeyCode::Up);
        assert_eq!(view.picked().unwrap().lead, "ledger.lead");
        // ← there does nothing; → and enter go to its agent.
        assert_eq!(key(&mut view, KeyCode::Left), Act::Stay);
        assert_eq!(view.pick, Some(Pick::Pin("ledger.lead".into())));
        assert_eq!(
            key(&mut view, KeyCode::Right),
            Act::Go("ledger.lead".into())
        );
        assert_eq!(
            key(&mut view, KeyCode::Enter),
            Act::Go("ledger.lead".into())
        );
        // At the first pin, ↑ stays; ↓ from the last goes to the projects.
        key(&mut view, KeyCode::Up);
        key(&mut view, KeyCode::Up);
        assert_eq!(view.pick, Some(Pick::Pin("tally.readme".into())));
        for _ in 0..3 {
            key(&mut view, KeyCode::Down);
        }
        assert_eq!(view.picked().unwrap().lead, "books.lead");
    }

    #[test]
    fn p_pins_a_project_s_lead_and_unpins_a_pinned_row_the_rows_after_moving_up() {
        let mut view = pinned();
        // On Tally invoices: its lead.
        assert_eq!(
            key(&mut view, KeyCode::Char('p')),
            Act::Pin("tally.lead".into())
        );
        // On the second pin: that agent; unpinned, the third moves up
        // into its place and its number, and the pick stays there.
        for _ in 0..4 {
            key(&mut view, KeyCode::Up);
        }
        assert_eq!(
            key(&mut view, KeyCode::Char('p')),
            Act::Pin("ledger.lead".into())
        );
        let mut pins = view.pins().to_vec();
        crate::pinned::apply(
            &mut pins,
            &crate::pinned::Change::Unpin("ledger.lead".into()),
        );
        view.set_pins(pins, NOW);
        let drawn = text(&mut view, 80);
        assert_eq!(drawn[5], " 1 ? Check the README examples  tally.readme");
        assert_eq!(drawn[6], " 2 ✓ Round half cents to even   tally.halves");
        assert_eq!(drawn[7], "");
        assert_eq!(view.pick, Some(Pick::Pin("tally.halves".into())));
        assert_eq!(
            key(&mut view, KeyCode::Char('2')),
            Act::Go("tally.halves".into())
        );
        // The fold pins nothing.
        let mut view = pinned();
        for _ in 0..2 {
            key(&mut view, KeyCode::Down);
        }
        assert_eq!(view.pick, Some(Pick::Fold));
        assert_eq!(key(&mut view, KeyCode::Char('p')), Act::Stay);
    }

    #[test]
    fn an_agent_s_turn_ending_moves_no_pinned_row_and_no_number() {
        let mut view = pinned();
        let before = text(&mut view, 80);
        let mut projects = view.projects().to_vec();
        projects[1].pinned[0].state = "idle".into();
        projects[2].pinned[0].state = "idle".into();
        view.set(projects, NOW);
        let after = text(&mut view, 80);
        assert_eq!(after[5], " 1 ○ Check the README examples  tally.readme");
        assert_eq!(after[6], " 2 ○ Ledger import              ledger.lead");
        assert_eq!(after[7], before[7]);
    }

    #[test]
    fn a_pin_not_there_keeps_its_row_muted_its_number_answered_and_p_clears_it() {
        let mut view = pinned();
        let mut projects = view.projects().to_vec();
        projects[1].pinned.pop();
        view.set(projects, NOW);
        view.set_gone(vec!["tally.halves".into()]);
        let lines = view.lines(80, 40, NOW);
        assert_eq!(lines[7].to_string(), " 3 tally.halves");
        let name = lines[7].spans.last().unwrap();
        assert!(name.style.add_modifier.contains(Modifier::DIM));
        assert_eq!(name.style.fg, None);
        assert_eq!(view.links().len(), 2);
        assert_eq!(
            key(&mut view, KeyCode::Char('3')),
            Act::Gone("tally.halves".into())
        );
        // ↑ to it, and p clears it.
        for _ in 0..3 {
            key(&mut view, KeyCode::Up);
        }
        assert_eq!(
            key(&mut view, KeyCode::Char('p')),
            Act::Pin("tally.halves".into())
        );
    }

    #[test]
    fn while_finding_a_number_is_typed_and_pinned_rows_narrow_by_title_and_name() {
        let mut view = pinned();
        view.key(
            &KeyEvent::new(KeyCode::Char('f'), KeyModifiers::CONTROL),
            NOW,
        );
        assert_eq!(key(&mut view, KeyCode::Char('1')), Act::Stay);
        assert_eq!(view.finding.as_ref().unwrap().query, "1");
        key(&mut view, KeyCode::Backspace);
        for c in "half".chars() {
            key(&mut view, KeyCode::Char(c));
        }
        let drawn = text(&mut view, 80);
        assert_eq!(
            &drawn[4..],
            [" Pinned", " 3 ✓ Round half cents to even  tally.halves", "",]
        );
        // The pick goes to the first match; enter goes to it.
        assert_eq!(view.pick, Some(Pick::Pin("tally.halves".into())));
        assert_eq!(
            key(&mut view, KeyCode::Enter),
            Act::Go("tally.halves".into())
        );
        // By name too; with no pinned match the section is not drawn.
        key(&mut view, KeyCode::Esc);
        view.key(
            &KeyEvent::new(KeyCode::Char('f'), KeyModifiers::CONTROL),
            NOW,
        );
        for c in "ledger.l".chars() {
            key(&mut view, KeyCode::Char(c));
        }
        assert_eq!(text(&mut view, 80)[5], " 2 ● Ledger import  ledger.lead");
        for c in "x".chars() {
            key(&mut view, KeyCode::Char(c));
        }
        assert_eq!(
            &text(&mut view, 80)[4..],
            [" nothing here matches ledger.lx"]
        );
    }

    #[test]
    fn on_a_short_screen_the_pinned_rows_and_the_projects_scroll_as_one() {
        let mut view = pinned();
        let short = |view: &mut View| -> Vec<String> {
            (view.lines(80, 9, NOW).iter())
                .map(|line| line.to_string().trim_end().to_owned())
                .collect()
        };
        // Five rows under the head: the pick goes down to the fold, which
        // is drawn, the pinned rows scrolled away above it.
        for _ in 0..12 {
            key(&mut view, KeyCode::Down);
        }
        let drawn = short(&mut view);
        assert_eq!(drawn.len(), 9);
        assert_eq!(drawn.last().unwrap(), " ▸ 4 idle projects");
        assert!(!drawn.iter().any(|line| line.contains("Pinned")));
        // Back up to the first pin: it is drawn again.
        for _ in 0..12 {
            key(&mut view, KeyCode::Up);
        }
        let drawn = short(&mut view);
        assert_eq!(view.pick, Some(Pick::Pin("tally.readme".into())));
        assert!(drawn.contains(&" 1 ? Check the README examples  tally.readme".to_owned()));
    }

    #[test]
    fn too_narrow_the_name_goes_then_the_title_is_cut_the_number_and_mark_stay() {
        let mut view = pinned();
        let drawn = text(&mut view, 45);
        assert_eq!(drawn[5], " 1 ? Check the README examples  tally.readme");
        let drawn = text(&mut view, 44);
        assert_eq!(drawn[5], " 1 ? Check the README examples");
        assert_eq!(drawn[6], " 2 ● Ledger import             ledger.lead");
        let drawn = text(&mut view, 20);
        assert_eq!(drawn[5], " 1 ? Check the…");
        assert_eq!(drawn[6], " 2 ● Ledger import");
    }

    #[test]
    fn the_agent_the_window_is_on_is_bold_and_its_name_plain() {
        let mut view = pinned().showing("tally.readme");
        let lines = view.lines(80, 40, NOW);
        let row = &lines[5];
        assert!(row.spans[2].style.add_modifier.contains(Modifier::BOLD));
        assert_eq!(row.spans.last().unwrap().style.fg, None);
        assert_eq!(view.links().len(), 2);
        assert_eq!(key(&mut view, KeyCode::Char('1')), Act::Close);
    }
}
