//! `/memory`: what agents saved to memory, in the pane. The facts kept
//! about the person in every project (`~/.agents/memory`) and this
//! project's (`projects/NAME`), listed from each folder's index
//! (`MEMORY.md`) with no fact's file read but a work fact's, whose index
//! line is its name and date alone. A fact opens as the pane's next page,
//! read without counting as an agent's read, its text's names of other
//! facts linked to them. `x` removes a fact through the memory tool's own
//! remove. Every read runs off the screen thread: when the view opens,
//! when `a` widens it to every project, and after a removal, never on a
//! timer.
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use agent_client::memory::{Listed, Peeked};
use ratatui::crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use ratatui::{
    style::{Color, Modifier, Stylize},
    text::{Line, Span},
};
use unicode_width::UnicodeWidthStr;

/// Where the view reads: the memory folder, this window's project in it,
/// and the folder a live fact's file blanks read from.
#[derive(Clone, Debug)]
pub struct Places {
    pub home: PathBuf,
    pub project: Option<String>,
    pub folder: PathBuf,
}

impl Places {
    fn root(&self) -> PathBuf {
        self.home.join(".agents/memory")
    }

    /// A scope's folder: a project's, or the person's (`None`).
    fn dir(&self, scope: Option<&str>) -> PathBuf {
        match scope {
            Some(project) => self.root().join("projects").join(project),
            None => self.root(),
        }
    }
}

/// A work fact, with what its own file says, as its index line does not.
#[derive(Clone, Debug, PartialEq)]
pub struct Work {
    pub scope: Option<String>,
    pub name: String,
    pub description: String,
    /// When its file was last written, in ms.
    pub saved_ms: u64,
}

/// One folder's facts, work aside, as its index lists them.
#[derive(Clone, Debug, PartialEq)]
pub struct Scope {
    /// The project's name, or `None` for the person's own facts.
    pub project: Option<String>,
    pub facts: Vec<Listed>,
    /// Every fact the index lists, work too: what a fact's text links to.
    pub names: Vec<String>,
    /// Why its index could not be read.
    pub error: Option<String>,
}

/// What the view lists: work under way, newest first; the projects', this
/// one first; the person's last.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Listing {
    pub work: Vec<Work>,
    pub scopes: Vec<Scope>,
    /// Projects past the most listed.
    pub more: usize,
}

/// The listing for `places`: this project and the person's, or with
/// `every` each project's, this one first.
pub fn list(places: &Places, every: bool) -> Listing {
    let mut projects: Vec<String> = Vec::new();
    let mut more = 0;
    if every {
        let (all, past) = agent_client::memory::projects(&places.root());
        projects = all;
        more = past;
    }
    if let Some(own) = &places.project {
        projects.retain(|name| name != own);
        projects.insert(0, own.clone());
    }
    let mut listing = Listing {
        more,
        ..Listing::default()
    };
    let scopes = projects.into_iter().map(Some).chain(std::iter::once(None));
    for scope in scopes {
        let dir = places.dir(scope.as_deref());
        let read = match std::fs::read_to_string(dir.join("MEMORY.md")) {
            Ok(index) => Ok(index),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(String::new()),
            Err(e) => Err(e.to_string()),
        };
        let mut listed = Scope {
            project: scope.clone(),
            facts: Vec::new(),
            names: Vec::new(),
            error: None,
        };
        match read {
            Err(error) => listed.error = Some(error),
            Ok(index) => {
                // A live fact's description filled as a starting agent
                // reads it; one that does not fill stays as written.
                let index = agent_client::memory::filled_lines(&index, &places.folder);
                for fact in agent_client::memory::listed(&index) {
                    listed.names.push(fact.name.clone());
                    if fact.kind != "work" {
                        listed.facts.push(fact);
                        continue;
                    }
                    // Work's index line is its name and date: what it is
                    // and when it was saved are its file's.
                    let (description, saved_ms) =
                        match agent_client::memory::peek(&dir, None, &fact.name) {
                            Ok(peeked) => (peeked.listed.description, peeked.modified_ms),
                            Err(error) => (error, 0),
                        };
                    listing.work.push(Work {
                        scope: scope.clone(),
                        name: fact.name,
                        description,
                        saved_ms,
                    });
                }
            }
        }
        listing.scopes.push(listed);
    }
    listing
        .work
        .sort_by(|a, b| b.saved_ms.cmp(&a.saved_ms).then(a.name.cmp(&b.name)));
    listing
}

/// A row that the pick lands on: a section's heading, or a fact.
#[derive(Clone, Debug, PartialEq)]
enum Item {
    Heading(String),
    Fact { scope: Option<String>, name: String },
}

/// A fact open as a page over the list.
struct Page {
    scope: Option<String>,
    name: String,
    /// The fact as read, or why it could not be; nothing while it is read.
    read: Option<Result<Peeked, String>>,
    /// The link picked, by its place among those drawn.
    pick: Option<usize>,
    /// Where each link was last drawn: its row, its columns on that row,
    /// and the fact it opens.
    links: Vec<(usize, u16, u16, Option<String>, String)>,
}

/// What a read off the screen thread came to.
enum Done {
    Listed(Listing, Option<String>),
    Opened(Option<String>, String, Result<Peeked, String>),
}

/// What a key did to the view.
#[derive(Debug, PartialEq)]
pub enum Key {
    Stay,
    Close,
    /// Scroll the page one row: up (true) or down.
    Scroll(bool),
}

/// The view's link to its clicks: a button's address is this, then what
/// was clicked.
pub const BUTTON: &str = "memory/";

/// `/memory`, open.
pub struct View {
    places: Places,
    /// `a`: every project's facts, not only this one's.
    every: bool,
    listing: Option<Listing>,
    /// A read under way, which lists nothing new until it is done.
    reading: bool,
    done: Arc<Mutex<Vec<Done>>>,
    wake: Arc<dyn Fn() + Send + Sync>,
    /// Sections opened or folded by hand, by key; the rest as they start.
    folds: HashMap<String, bool>,
    selected: usize,
    pages: Vec<Page>,
    /// The fact `x` asked to remove, waiting on `enter`.
    confirm: Option<(Option<String>, String)>,
    notice: Option<String>,
}

const WORK: &str = "work";
const YOU: &str = "you";

fn scope_key(scope: Option<&str>) -> String {
    match scope {
        Some(project) => format!("project:{project}"),
        None => YOU.into(),
    }
}

impl View {
    /// Open on `places`, read at once off the screen thread; `wake` is
    /// called when a read is done.
    pub fn open(places: Places, wake: Arc<dyn Fn() + Send + Sync>) -> Self {
        let mut view = Self::empty(places, wake);
        view.read(None);
        view
    }

    fn empty(places: Places, wake: Arc<dyn Fn() + Send + Sync>) -> Self {
        Self {
            places,
            every: false,
            listing: None,
            reading: false,
            done: Arc::new(Mutex::new(Vec::new())),
            wake,
            folds: HashMap::new(),
            selected: 0,
            pages: Vec::new(),
            confirm: None,
            notice: None,
        }
    }

    /// Read the listing again off the screen thread, after removing
    /// `remove` when asked.
    fn read(&mut self, remove: Option<(Option<String>, String)>) {
        self.reading = true;
        let (places, every) = (self.places.clone(), self.every);
        let (done, wake) = (self.done.clone(), self.wake.clone());
        std::thread::spawn(move || {
            let notice = remove.map(|(scope, name)| removed(&places, scope.as_deref(), &name));
            let listing = list(&places, every);
            lock(&done).push(Done::Listed(listing, notice));
            wake();
        });
    }

    /// Open the fact `name` of `scope` as the next page, read off the
    /// screen thread.
    fn open_fact(&mut self, scope: Option<String>, name: String) {
        self.pages.push(Page {
            scope: scope.clone(),
            name: name.clone(),
            read: None,
            pick: None,
            links: Vec::new(),
        });
        let places = self.places.clone();
        let (done, wake) = (self.done.clone(), self.wake.clone());
        std::thread::spawn(move || {
            let dir = places.dir(scope.as_deref());
            let read = agent_client::memory::peek(&dir, Some(&places.folder), &name);
            lock(&done).push(Done::Opened(scope, name, read));
            wake();
        });
    }

    /// What reads finished since: in the view now. Whether anything did.
    pub fn drain(&mut self) -> bool {
        let done: Vec<Done> = std::mem::take(&mut *lock(&self.done));
        let any = !done.is_empty();
        for done in done {
            match done {
                Done::Listed(listing, notice) => self.listed(listing, notice),
                Done::Opened(scope, name, read) => {
                    for page in &mut self.pages {
                        if page.scope == scope && page.name == name && page.read.is_none() {
                            page.read = Some(read.clone());
                        }
                    }
                }
            }
        }
        any
    }

    /// A listing read, the pick kept on the same row where it still is,
    /// else on the first fact.
    fn listed(&mut self, listing: Listing, notice: Option<String>) {
        let picked = self.items().get(self.selected).cloned();
        let first = self.listing.is_none();
        self.listing = Some(listing);
        self.reading = false;
        if notice.is_some() {
            self.notice = notice;
        }
        let items = self.items();
        self.selected = match picked.and_then(|p| items.iter().position(|i| *i == p)) {
            Some(at) => at,
            None if first => (items.iter())
                .position(|item| matches!(item, Item::Fact { .. }))
                .unwrap_or(0),
            None => self.selected.min(items.len().saturating_sub(1)),
        };
    }

    /// Whether a section starts open: work and this project's facts do;
    /// the person's, and with `a` every other project's, start folded.
    fn is_open(&self, key: &str) -> bool {
        if let Some(open) = self.folds.get(key) {
            return *open;
        }
        key == WORK
            || self
                .places
                .project
                .as_deref()
                .is_some_and(|p| key == scope_key(Some(p)))
    }

    /// The sections listed, in order: each one's key, title and facts.
    fn sections(&self) -> Vec<(String, String, Vec<Item>)> {
        let Some(listing) = &self.listing else {
            return Vec::new();
        };
        let mut out = Vec::new();
        if !listing.work.is_empty() {
            let facts = (listing.work.iter())
                .map(|work| Item::Fact {
                    scope: work.scope.clone(),
                    name: work.name.clone(),
                })
                .collect();
            out.push((WORK.into(), "Work under way".into(), facts));
        }
        for scope in &listing.scopes {
            if scope.facts.is_empty() && scope.error.is_none() {
                continue;
            }
            let key = scope_key(scope.project.as_deref());
            let title = match &scope.project {
                None => "About you".to_owned(),
                Some(_) if !self.every => "This project".to_owned(),
                Some(project) => project.clone(),
            };
            let facts = (scope.facts.iter())
                .map(|fact| Item::Fact {
                    scope: scope.project.clone(),
                    name: fact.name.clone(),
                })
                .collect();
            out.push((key, title, facts));
        }
        out
    }

    /// Every row the pick can land on, in order.
    fn items(&self) -> Vec<Item> {
        let mut out = Vec::new();
        for (key, _, facts) in self.sections() {
            let open = self.is_open(&key);
            out.push(Item::Heading(key));
            if open {
                out.extend(facts);
            }
        }
        out
    }

    fn work_of(&self, scope: &Option<String>, name: &str) -> Option<&Work> {
        (self.listing.as_ref()?.work.iter()).find(|w| w.scope == *scope && w.name == name)
    }

    fn fact_of(&self, scope: &Option<String>, name: &str) -> Option<&Listed> {
        (self.listing.as_ref()?.scopes.iter())
            .find(|s| s.project == *scope)?
            .facts
            .iter()
            .find(|fact| fact.name == name)
    }

    /// The fact names a page's text links: its own folder's, then the
    /// person's, which hold in every project; its own name aside.
    fn linkable(&self, scope: &Option<String>, own: &str) -> Vec<(Option<String>, String)> {
        let Some(listing) = &self.listing else {
            return Vec::new();
        };
        let mut out: Vec<(Option<String>, String)> = Vec::new();
        for wanted in [scope.clone(), None] {
            for listed in listing.scopes.iter().filter(|s| s.project == wanted) {
                for name in &listed.names {
                    if name != own && !out.iter().any(|(_, n)| n == name) {
                        out.push((wanted.clone(), name.clone()));
                    }
                }
            }
        }
        out
    }

    /// The fact a removal waits on is kept: a key went elsewhere.
    pub fn keep(&mut self) {
        self.confirm = None;
    }

    /// How many pages are open over the list.
    pub fn depth(&self) -> usize {
        self.pages.len()
    }

    /// Whether a fact is open as a page.
    pub fn on_page(&self) -> bool {
        !self.pages.is_empty()
    }

    /// A key, with the rows of the page now in view, for `↑` `↓` to pick
    /// among the links there before scrolling.
    pub fn key(&mut self, key: KeyEvent, in_view: std::ops::Range<usize>) -> Key {
        let confirm = self.confirm.take();
        self.notice = None;
        let code = match key.code {
            KeyCode::Char('k') => KeyCode::Up,
            KeyCode::Char('j') => KeyCode::Down,
            KeyCode::Char('h') => KeyCode::Left,
            KeyCode::Char('l') => KeyCode::Right,
            code => code,
        };
        match code {
            KeyCode::Char('c') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                return Key::Close;
            }
            // A letter with Ctrl or Alt is another shortcut.
            KeyCode::Char(_)
                if key.modifiers.intersects(
                    KeyModifiers::CONTROL | KeyModifiers::ALT | KeyModifiers::SUPER,
                ) => {}
            KeyCode::Esc if confirm.is_some() => {}
            KeyCode::Esc => return Key::Close,
            KeyCode::Enter if confirm.is_some() => {
                let (scope, name) = confirm.unwrap_or_default();
                self.read(Some((scope, name)));
            }
            _ if self.on_page() => return self.page_key(code, in_view),
            KeyCode::Up => self.selected = self.selected.saturating_sub(1),
            KeyCode::Down => {
                self.selected = (self.selected + 1).min(self.items().len().saturating_sub(1));
            }
            KeyCode::Right | KeyCode::Enter => self.go_in(code == KeyCode::Enter),
            KeyCode::Left => self.go_out(),
            KeyCode::Char('a') if !self.reading => {
                self.every = !self.every;
                self.read(None);
            }
            KeyCode::Char('x') => {
                if let Some(Item::Fact { scope, name }) = self.items().get(self.selected) {
                    self.confirm = Some((scope.clone(), name.clone()));
                }
            }
            _ => {}
        }
        Key::Stay
    }

    /// `→` or `enter` on the pick: a fact opens; a folded section opens,
    /// and `enter` folds an open one.
    fn go_in(&mut self, enter: bool) {
        match self.items().get(self.selected).cloned() {
            Some(Item::Fact { scope, name }) => self.open_fact(scope, name),
            Some(Item::Heading(key)) => {
                let open = self.is_open(&key);
                if !open || enter {
                    self.folds.insert(key, !open);
                } else {
                    // Open already: in to its first fact.
                    self.selected += 1;
                    self.selected = self.selected.min(self.items().len().saturating_sub(1));
                }
            }
            None => {}
        }
    }

    /// `←` on the pick: its section folds, the pick on its heading.
    fn go_out(&mut self) {
        let items = self.items();
        let Some(heading) = items[..=self.selected.min(items.len().saturating_sub(1))]
            .iter()
            .rposition(|item| matches!(item, Item::Heading(_)))
        else {
            return;
        };
        if let Item::Heading(key) = &items[heading]
            && self.is_open(key)
        {
            self.folds.insert(key.clone(), false);
            self.selected = heading;
        }
    }

    /// A key on a page: the arrows pick among the links in view, as Lynx
    /// does, and scroll where none is left that way; `→` or `enter`
    /// follows the picked link and `←` goes back a page.
    fn page_key(&mut self, code: KeyCode, in_view: std::ops::Range<usize>) -> Key {
        let follow = match self.pages.last_mut() {
            None => return Key::Stay,
            Some(page) => match code {
                KeyCode::Up | KeyCode::Down => {
                    let down = code == KeyCode::Down;
                    let shown: Vec<usize> = (0..page.links.len())
                        .filter(|&at| in_view.contains(&page.links[at].0))
                        .collect();
                    let next = match (page.pick, down) {
                        (Some(at), true) => shown.iter().copied().find(|&s| s > at),
                        (Some(at), false) => shown.iter().copied().rev().find(|&s| s < at),
                        (None, true) => shown.first().copied(),
                        (None, false) => shown.last().copied(),
                    };
                    match next {
                        Some(at) => page.pick = Some(at),
                        None => return Key::Scroll(!down),
                    }
                    None
                }
                KeyCode::Right | KeyCode::Enter => page
                    .pick
                    .and_then(|at| page.links.get(at))
                    .map(|(.., scope, name)| (scope.clone(), name.clone())),
                KeyCode::Left => {
                    self.pages.pop();
                    None
                }
                _ => None,
            },
        };
        if let Some((scope, name)) = follow {
            self.open_fact(scope, name);
        }
        Key::Stay
    }

    /// A click on one of the view's buttons, `what` past [`BUTTON`]: a
    /// row picked and opened, as `enter` does; a link followed.
    pub fn click(&mut self, what: &str) {
        self.confirm = None;
        if let Some(at) = what.strip_prefix("row/").and_then(|at| at.parse().ok()) {
            if at < self.items().len() {
                self.selected = at;
                self.go_in(true);
            }
        } else if let Some(at) = what
            .strip_prefix("link/")
            .and_then(|at| at.parse::<usize>().ok())
            && let Some(page) = self.pages.last_mut()
            && let Some((.., scope, name)) = page.links.get(at).cloned()
        {
            page.pick = Some(at);
            self.open_fact(scope, name);
        }
    }

    /// The card at `width`, the row the pick is on, and the buttons on
    /// its rows.
    pub fn card(
        &mut self,
        width: usize,
    ) -> (Vec<Line<'static>>, usize, Vec<(usize, crate::screen::Mark)>) {
        if self.on_page() {
            return self.page_lines(width);
        }
        self.list_lines(width)
    }

    fn head(&self) -> Line<'static> {
        let whose = match (&self.places.project, self.every) {
            (_, true) => "every project".to_owned(),
            (Some(project), false) => project.clone(),
            (None, false) => "you".to_owned(),
        };
        let mut spans = vec![Span::raw(whose).bold()];
        if let Some(listing) = &self.listing {
            let lasting: usize = listing.scopes.iter().map(|s| s.facts.len()).sum();
            let all = lasting + listing.work.len();
            let yours = (listing.scopes.iter())
                .find(|s| s.project.is_none())
                .map_or(0, |s| s.facts.len());
            let mut facts = format!(" · {all} fact{}", if all == 1 { "" } else { "s" });
            if yours > 0 {
                facts.push_str(&format!(" · {yours} about you"));
            }
            spans.push(Span::raw(facts).dim());
        }
        Line::from(spans)
    }

    fn list_lines(
        &mut self,
        width: usize,
    ) -> (Vec<Line<'static>>, usize, Vec<(usize, crate::screen::Mark)>) {
        let mut out = vec![self.head()];
        let mut marks = Vec::new();
        let Some(listing) = self.listing.clone() else {
            out.push(Line::default());
            out.push(Line::from("reading…").dim());
            return (out, 0, marks);
        };
        let sections = self.sections();
        if sections.is_empty() {
            out.push(Line::default());
            out.push(Line::from("No facts saved yet."));
            out.push(
                Line::from("Agents save what a later agent would otherwise have to ask you again.")
                    .dim(),
            );
            return (out, 0, marks);
        }
        let now = now_ms();
        let mut at = 0;
        let mut index = 0;
        for (key, title, facts) in sections {
            let open = self.is_open(&key);
            out.push(Line::default());
            let mut heading = vec![
                Span::raw("  "),
                Span::raw(title).bold(),
                Span::raw(format!(" · {}", facts.len())).dim(),
            ];
            if !open {
                heading.push(Span::raw(" ▸").dim());
            }
            let mut heading = Line::from(heading);
            if index == self.selected {
                at = out.len();
                crate::list::pick(&mut heading);
            }
            marks.push((out.len(), button(&format!("row/{index}"))));
            out.push(heading);
            index += 1;
            // A folder that could not be read says so in its place.
            let scope = (listing.scopes.iter()).find(|s| key == scope_key(s.project.as_deref()));
            if let Some(error) = scope.and_then(|s| s.error.as_ref()) {
                out.push(Line::from(vec![
                    Span::raw("  "),
                    Span::raw("✗ ").fg(Color::Red),
                    Span::raw(format!("could not read memory: {error}")),
                ]));
            }
            if !open {
                continue;
            }
            for item in facts {
                let Item::Fact { scope, name } = &item else {
                    continue;
                };
                let picked = index == self.selected;
                if picked {
                    at = out.len();
                }
                marks.push((out.len(), button(&format!("row/{index}"))));
                let (first, rest) = match (key == WORK, self.work_of(scope, name)) {
                    (true, Some(work)) => self.work_row(work, now, width),
                    _ => match self.fact_of(scope, name) {
                        Some(fact) => fact_row(fact, width),
                        None => (Line::from(name.clone()), Vec::new()),
                    },
                };
                let mut first = first;
                if picked {
                    crate::list::pick(&mut first);
                }
                out.push(first);
                out.extend(rest);
                index += 1;
            }
        }
        if listing.more > 0 {
            out.push(Line::default());
            out.push(Line::from(format!("  … {} more projects", listing.more)).dim());
        }
        (out, at, marks)
    }

    /// A work row: its name, and at the right edge its project with `a`
    /// on and how long since it was saved; what it is under it.
    fn work_row(&self, work: &Work, now: u64, width: usize) -> (Line<'static>, Vec<Line<'static>>) {
        let mut right = Vec::new();
        if self.every
            && let Some(project) = &work.scope
        {
            right.push(project.clone());
        }
        if work.saved_ms > 0 {
            right.push(ago(now.saturating_sub(work.saved_ms) / 1000));
        }
        let first = edge(
            vec![Span::raw("  "), Span::raw(work.name.clone()).bold()],
            right.join(" · "),
            width,
        );
        (first, described(&work.description, width))
    }

    fn page_lines(
        &mut self,
        width: usize,
    ) -> (Vec<Line<'static>>, usize, Vec<(usize, crate::screen::Mark)>) {
        let (scope, name) = match self.pages.last() {
            Some(page) => (page.scope.clone(), page.name.clone()),
            None => return (Vec::new(), 0, Vec::new()),
        };
        let linkable = self.linkable(&scope, &name);
        let mut out = vec![Line::from(name.clone()).bold()];
        let mut marks = Vec::new();
        let Some(page) = self.pages.last_mut() else {
            return (out, 0, marks);
        };
        let peeked = match &page.read {
            None => {
                out.push(Line::default());
                out.push(Line::from("reading…").dim());
                return (out, 0, marks);
            }
            Some(Err(error)) => {
                out.push(Line::default());
                out.push(Line::from(vec![
                    Span::raw("✗ ").fg(Color::Red),
                    Span::raw(format!("could not read {name}: {error}")),
                ]));
                return (out, 0, marks);
            }
            Some(Ok(peeked)) => peeked.clone(),
        };
        // Under the title, muted: what it is, and where it came from; an
        // agent's message there opens that agent.
        let mut about = flags(&peeked.listed);
        about.push(format!("verified {}", day(&peeked.listed.verified)));
        if let Some(project) = scope.as_ref().filter(|_| self.every) {
            about.insert(0, project.clone());
        }
        // Where it came from shares the row when there is room, else takes
        // the next, never split.
        let about = about.join(" · ");
        let mut from = vec![Span::raw("from ").dim()];
        let mut agent_at = None;
        match crate::calls::handle_parts(&peeked.source) {
            Some((agent, number)) => {
                agent_at = Some((5, agent.width(), agent.to_owned()));
                from.push(Span::raw(agent.to_owned()).fg(Color::Blue));
                from.push(Span::raw(format!("/{number}")).dim());
            }
            None => from.push(Span::raw(peeked.source.clone()).dim()),
        }
        let shared = about.width() + 3 + Line::from(from.clone()).width() < width;
        let mut offset = 0;
        match shared {
            true => {
                offset = about.width() + 3;
                from.insert(0, Span::raw(format!("{about} · ")).dim());
            }
            false => out.extend(crate::transcript::wrap(
                Line::from(about).dim(),
                width,
                Span::raw(""),
                Span::raw(""),
            )),
        }
        if let Some((at, wide, agent)) = agent_at {
            let start = (offset + at + 1) as u16;
            let buttons = vec![(
                start,
                start + wide as u16,
                format!("{}{agent}", crate::links::AGENT),
            )];
            marks.push((out.len(), crate::screen::Mark::Buttons(buttons)));
        }
        out.extend(crate::transcript::wrap(
            Line::from(from),
            width,
            Span::raw(""),
            Span::raw(""),
        ));
        out.push(Line::default());
        // A live fact as it fills now; one that does not fill says why
        // over its template.
        let text = match &peeked.filled {
            Some(Ok(filled)) => filled.clone(),
            Some(Err(why)) => {
                out.push(Line::from(vec![
                    Span::raw("✗ ").fg(Color::Red),
                    Span::raw(format!("could not fill: {why}")),
                ]));
                out.push(Line::default());
                peeked.text.clone()
            }
            None => peeked.text.clone(),
        };
        let mut markdown = crate::transcript::Markdown::default();
        let lines = crate::transcript::markdown(
            &mut markdown,
            &text,
            width,
            (Line::from(""), Line::from("")),
        );
        page.links.clear();
        for line in lines {
            let row = out.len();
            let mut line = line;
            let mut row_buttons = Vec::new();
            for (from, to, target) in found(&line, &linkable) {
                let at = page.links.len();
                let (scope, name) = target;
                let picked = page.pick == Some(at);
                let (from_col, to_col) = underline(&mut line, from, to, picked);
                row_buttons.push((
                    from_col + 1,
                    to_col + 1,
                    format!("{}{BUTTON}link/{at}", crate::screen::BUTTON),
                ));
                page.links.push((row, from_col, to_col, scope, name));
            }
            if !row_buttons.is_empty() {
                marks.push((row, crate::screen::Mark::Buttons(row_buttons)));
            }
            out.push(line);
        }
        let at = page
            .pick
            .and_then(|pick| page.links.get(pick))
            .map_or(0, |link| link.0);
        (out, at, marks)
    }

    /// The line under the card: the removal to confirm, or what a key did.
    pub fn footer(&self) -> Option<Line<'static>> {
        if let Some((_, name)) = &self.confirm {
            return Some(Line::from(vec![
                Span::raw("Remove "),
                Span::raw(name.clone()).bold(),
                Span::raw("? It can't be undone."),
            ]));
        }
        self.notice
            .as_ref()
            .map(|notice| Line::from(notice.clone()))
    }

    /// What the keys do, as the pane's title says it.
    pub fn keys(&self) -> String {
        if self.confirm.is_some() {
            return "enter remove · esc cancel".into();
        }
        if let Some(page) = self.pages.last() {
            return match page.links.is_empty() {
                true => "↑↓ scroll · ← back · esc close".into(),
                false => "↑↓ pick · enter follow · ← back · esc close".into(),
            };
        }
        let items = self.items();
        let mut keys = Vec::new();
        if items.len() > 1 {
            keys.push("↑↓ pick");
        }
        match items.get(self.selected) {
            Some(Item::Fact { .. }) => keys.push("enter open"),
            Some(Item::Heading(key)) if self.is_open(key) => keys.push("enter fold"),
            Some(Item::Heading(_)) => keys.push("enter open"),
            None => {}
        }
        keys.push(match self.every {
            true => "a this project",
            false => "a every project",
        });
        if matches!(items.get(self.selected), Some(Item::Fact { .. })) {
            keys.push("x remove");
        }
        keys.push("esc close");
        keys.join(" · ")
    }
}

fn lock<T>(mutex: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(|e| e.into_inner())
}

/// `name` removed from `scope` through the memory tool's own remove, so
/// its index is rewritten and its read count dropped: what came of it.
fn removed(places: &Places, scope: Option<&str>, name: &str) -> String {
    let mut arguments = serde_json::json!({"remove": name});
    match scope {
        Some(project) => arguments["project"] = project.into(),
        None => arguments["user"] = true.into(),
    }
    match agent_client::memory::call(&places.home, &places.folder, &arguments) {
        Ok(_) => format!("removed {name}"),
        Err(error) => format!("✗ could not remove {name}: {error}"),
    }
}

fn button(what: &str) -> crate::screen::Mark {
    crate::screen::Mark::Link(format!("{}{BUTTON}{what}", crate::screen::BUTTON))
}

/// A fact's flags, as its index line has them, its type first.
fn flags(fact: &Listed) -> Vec<String> {
    let mut out = vec![fact.kind.clone()];
    if fact.live {
        out.push("live".into());
    }
    if fact.external {
        out.push("external".into());
    }
    if fact.confirmed > 0 {
        out.push(format!("confirmed {}", fact.confirmed));
    }
    if fact.pushed_back > 0 {
        out.push(format!("pushed back {}", fact.pushed_back));
    }
    out
}

/// A lasting fact's row: its name, its type and flags muted, and at the
/// right edge when it was last verified; what it is under it. Where the
/// row has no room for its flags, they take a muted row of their own.
fn fact_row(fact: &Listed, width: usize) -> (Line<'static>, Vec<Line<'static>>) {
    let flags = flags(fact).join(" · ");
    let verified = format!("verified {}", day(&fact.verified));
    let name = || vec![Span::raw("  "), Span::raw(fact.name.clone()).bold()];
    let mut left = name();
    left.push(Span::raw(format!(" {flags}")).dim());
    if fits(&left, &verified, width) {
        return (
            edge(left, verified, width),
            described(&fact.description, width),
        );
    }
    let mut rest = vec![Line::from(format!("    {flags}")).dim()];
    rest.extend(described(&fact.description, width));
    (edge(name(), verified, width), rest)
}

/// A row's description, under its name, in at most two rows.
fn described(description: &str, width: usize) -> Vec<Line<'static>> {
    if description.is_empty() {
        return Vec::new();
    }
    crate::transcript::capped(
        Line::from(description.to_owned()),
        width,
        Span::raw("    "),
        2,
    )
}

/// Whether `left` and `right` share a row `width` wide, a cell kept clear
/// at the edge, as the pick's band takes one.
fn fits(left: &[Span<'static>], right: &str, width: usize) -> bool {
    Line::from(left.to_vec()).width() + 2 + right.width() < width
}

/// `left`, and `right` muted at the edge of a row `width` wide; after it,
/// a dot apart, where the row has no room for both.
fn edge(mut left: Vec<Span<'static>>, right: String, width: usize) -> Line<'static> {
    if right.is_empty() {
        return Line::from(left);
    }
    let used = Line::from(left.clone()).width();
    match fits(&left, &right, width) {
        true => {
            let room = width - 1;
            left.push(Span::raw(" ".repeat(room - used - right.width())));
            left.push(Span::raw(right).dim());
        }
        false => left.push(Span::raw(format!(" · {right}")).dim()),
    }
    Line::from(left)
}

/// Where in `line` a fact's name stands as a word of its own (not part
/// of a longer name, `release-signing` never inside
/// `release-signing-old`), by byte offsets in its text, and the fact.
fn found(
    line: &Line<'static>,
    names: &[(Option<String>, String)],
) -> Vec<(usize, usize, (Option<String>, String))> {
    if names.is_empty() {
        return Vec::new();
    }
    let text: String = line.spans.iter().map(|s| s.content.as_ref()).collect();
    let part = |c: char| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-';
    let mut out = Vec::new();
    let mut start = None;
    for (at, c) in text
        .char_indices()
        .chain(std::iter::once((text.len(), ' ')))
    {
        match (part(c), start) {
            (true, None) => start = Some(at),
            (false, Some(from)) => {
                start = None;
                // A name never ends in a hyphen; a word may, before a dash.
                let word = text[from..at].trim_end_matches('-');
                if let Some(target) = names.iter().find(|(_, name)| name == word) {
                    out.push((from, from + word.len(), target.clone()));
                }
            }
            _ => {}
        }
    }
    out
}

/// The bytes `from..to` of `line`'s text underlined as a link, reversed
/// when it is the one picked: the columns they take.
fn underline(line: &mut Line<'static>, from: usize, to: usize, picked: bool) -> (u16, u16) {
    let mut spans = Vec::new();
    let mut at = 0;
    let mut columns = (0, 0);
    let mut column = 0;
    for span in std::mem::take(&mut line.spans) {
        let text = span.content.to_string();
        let (start, end) = (at, at + text.len());
        at = end;
        if end <= from || start >= to {
            column += text.width();
            spans.push(span);
            continue;
        }
        let a = from.max(start) - start;
        let b = to.min(end) - start;
        if a > 0 {
            column += text[..a].width();
            spans.push(Span::styled(text[..a].to_owned(), span.style));
        }
        if from >= start {
            columns.0 = column;
        }
        let mut style = span.style.add_modifier(Modifier::UNDERLINED);
        if picked {
            style = style.add_modifier(Modifier::REVERSED);
        }
        column += text[a..b].width();
        spans.push(Span::styled(text[a..b].to_owned(), style));
        if to <= end {
            columns.1 = column;
        }
        if b < text.len() {
            column += text[b..].width();
            spans.push(Span::styled(text[b..].to_owned(), span.style));
        }
    }
    line.spans = spans;
    (columns.0 as u16, columns.1 as u16)
}

fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64
}

/// How long ago, as the pane says an age: `now`, `5m`, `2h`, `3d`.
fn ago(seconds: u64) -> String {
    match seconds {
        0..60 => "now".into(),
        60..3600 => format!("{}m", seconds / 60),
        3600..86400 => format!("{}h", seconds / 3600),
        _ => format!("{}d", seconds / 86400),
    }
}

/// A `YYYY-MM-DD` date as the pane writes a day: `3 Nov`, with the year
/// when it is not this one's.
fn day(date: &str) -> String {
    day_in(date, this_year())
}

fn day_in(date: &str, year: i64) -> String {
    const MONTHS: [&str; 12] = [
        "Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec",
    ];
    let mut parts = date.splitn(3, '-').map(|p| p.parse::<i64>().ok());
    let (Some(Some(y)), Some(Some(m)), Some(Some(d))) = (parts.next(), parts.next(), parts.next())
    else {
        return date.to_owned();
    };
    let Some(month) = MONTHS
        .get((m - 1).max(0) as usize)
        .filter(|_| (1..=12).contains(&m))
    else {
        return date.to_owned();
    };
    match y == year {
        true => format!("{d} {month}"),
        false => format!("{d} {month} {y}"),
    }
}

/// This year, from the clock (Howard Hinnant's `civil_from_days`).
fn this_year() -> i64 {
    let days = (now_ms() / 86_400_000) as i64 + 719_468;
    let era = days.div_euclid(146_097);
    let of_era = days - era * 146_097;
    let year_of_era = (of_era - of_era / 1460 + of_era / 36_524 - of_era / 146_096) / 365;
    let of_year = of_era - (365 * year_of_era + year_of_era / 4 - year_of_era / 100);
    let month = (5 * of_year + 2) / 153;
    let month = if month < 10 { month + 3 } else { month - 9 };
    year_of_era + era * 400 + i64::from(month <= 2)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A memory folder of its own, written as the memory tool writes it.
    fn places(name: &str) -> Places {
        let home = std::env::temp_dir().join(format!("sonny-memory-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&home);
        std::fs::create_dir_all(&home).unwrap();
        Places {
            home: home.clone(),
            project: Some("tally".into()),
            folder: home,
        }
    }

    fn save(places: &Places, scope: Option<&str>, name: &str, kind: &str, more: serde_json::Value) {
        let mut save = serde_json::json!({
            "name": name, "type": kind, "description": format!("what {name} says"),
            "source": "tally.lead/4", "text": format!("The fact {name}."),
        });
        for (key, value) in more.as_object().into_iter().flatten() {
            save[key] = value.clone();
        }
        let mut arguments = serde_json::json!({"save": save});
        match scope {
            Some(project) => arguments["project"] = project.into(),
            None => arguments["user"] = true.into(),
        }
        agent_client::memory::call(&places.home, &places.folder, &arguments).unwrap();
    }

    fn view(places: &Places) -> View {
        let mut view = View::empty(places.clone(), Arc::new(|| {}));
        view.listed(list(places, false), None);
        view
    }

    fn text(lines: &[Line<'static>]) -> Vec<String> {
        lines
            .iter()
            .map(|l| l.to_string().trim_end().to_owned())
            .collect()
    }

    fn key(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::NONE)
    }

    /// Reads off the screen thread, waited for.
    fn settle(view: &mut View) {
        for _ in 0..200 {
            view.drain();
            let pages_read = view.pages.iter().all(|p| p.read.is_some());
            if !view.reading && pages_read {
                return;
            }
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        panic!("a read never finished");
    }

    #[test]
    fn the_list_has_work_this_project_and_you_folded_with_flags() {
        let places = places("list");
        save(
            &places,
            Some("tally"),
            "release-signing",
            "project",
            serde_json::json!({"confirmed": 2}),
        );
        save(
            &places,
            Some("tally"),
            "rounding-fix",
            "work",
            serde_json::json!({}),
        );
        save(
            &places,
            None,
            "short-replies",
            "feedback",
            serde_json::json!({"pushed_back": 1}),
        );
        let mut view = view(&places);
        let (lines, at, _) = view.card(80);
        let lines = text(&lines);
        assert_eq!(lines[0], "tally · 3 facts · 1 about you");
        assert!(lines.contains(&"  Work under way · 1".to_owned()));
        assert!(
            lines
                .iter()
                .any(|l| l.starts_with("  rounding-fix") && l.ends_with("now"))
        );
        assert!(lines.contains(&"    what rounding-fix says".to_owned()));
        assert!(lines.contains(&"  This project · 1".to_owned()));
        let row = lines
            .iter()
            .find(|l| l.starts_with("  release-signing"))
            .unwrap();
        assert!(row.contains(" project · confirmed 2") && row.contains("verified "));
        // Too narrow for its flags, a row gives them a row of their own.
        let narrow = text(&view.card(36).0);
        let row = (narrow.iter()).position(|l| l.starts_with("  release-signing"));
        let row = row.unwrap();
        assert!(narrow[row].contains("verified "));
        assert_eq!(narrow[row + 1], "    project · confirmed 2");
        // The person's facts start folded.
        assert!(lines.contains(&"  About you · 1 ▸".to_owned()));
        assert!(!lines.iter().any(|l| l.contains("short-replies")));
        // The pick starts on the first fact.
        assert!(lines[at].starts_with("  rounding-fix"));
        // → on the folded heading opens it.
        for _ in 0..3 {
            view.key(key(KeyCode::Down), 0..0);
        }
        view.key(key(KeyCode::Right), 0..0);
        let lines = text(&view.card(80).0);
        assert!(
            lines
                .iter()
                .any(|l| l.contains("short-replies") && l.contains("feedback · pushed back 1"))
        );
        // ← folds it again.
        view.key(key(KeyCode::Down), 0..0);
        view.key(key(KeyCode::Left), 0..0);
        assert!(text(&view.card(80).0).contains(&"  About you · 1 ▸".to_owned()));
        let _ = std::fs::remove_dir_all(&places.home);
    }

    #[test]
    fn empty_and_unreadable_memory_say_so() {
        let places = places("empty");
        let mut view = View::empty(places.clone(), Arc::new(|| {}));
        assert_eq!(text(&view.card(80).0)[2], "reading…");
        view.listed(list(&places, false), None);
        let lines = text(&view.card(80).0);
        assert_eq!(
            lines[2..],
            [
                "No facts saved yet.",
                "Agents save what a later agent would otherwise have to ask you again."
            ]
        );
        // An index that is a folder cannot be read.
        std::fs::create_dir_all(places.home.join(".agents/memory/projects/tally/MEMORY.md"))
            .unwrap();
        view.listed(list(&places, false), None);
        let lines = text(&view.card(80).0);
        assert!(lines.contains(&"  This project · 0".to_owned()));
        assert!(
            lines
                .iter()
                .any(|l| l.starts_with("  ✗ could not read memory: "))
        );
        let _ = std::fs::remove_dir_all(&places.home);
    }

    #[test]
    fn every_project_lists_each_folded_after_this_one() {
        let places = places("every");
        save(
            &places,
            Some("tally"),
            "release-signing",
            "project",
            serde_json::json!({}),
        );
        save(
            &places,
            Some("calc"),
            "rounding",
            "project",
            serde_json::json!({}),
        );
        save(
            &places,
            Some("calc"),
            "halves",
            "work",
            serde_json::json!({}),
        );
        let mut view = view(&places);
        assert!(
            !text(&view.card(80).0)
                .iter()
                .any(|l| l.contains("rounding"))
        );
        view.key(key(KeyCode::Char('a')), 0..0);
        settle(&mut view);
        let lines = text(&view.card(80).0);
        assert_eq!(lines[0], "every project · 3 facts");
        let tally = lines.iter().position(|l| l == "  tally · 1").unwrap();
        let calc = lines.iter().position(|l| l == "  calc · 1 ▸").unwrap();
        assert!(tally < calc);
        // Work from every project, its project at the right edge.
        assert!(
            lines
                .iter()
                .any(|l| l.starts_with("  halves") && l.ends_with("calc · now"))
        );
        assert!(view.keys().contains("a this project"));
        let _ = std::fs::remove_dir_all(&places.home);
    }

    #[test]
    fn an_open_fact_is_a_page_whose_names_link_and_left_walks_back() {
        let places = places("open");
        save(
            &places,
            Some("tally"),
            "release-signing",
            "project",
            serde_json::json!({
                "text": "Keys live in the vault; see vault-access, not vault-access-old.",
            }),
        );
        save(
            &places,
            Some("tally"),
            "vault-access",
            "reference",
            serde_json::json!({}),
        );
        save(
            &places,
            Some("tally"),
            "vault-access-old",
            "reference",
            serde_json::json!({}),
        );
        let mut view = view(&places);
        // release-signing is picked first, by name.
        view.key(key(KeyCode::Enter), 0..0);
        settle(&mut view);
        let (lines, _, marks) = view.card(80);
        let shown = text(&lines);
        assert_eq!(shown[0], "release-signing");
        assert!(
            shown[1].starts_with("project · verified ")
                && shown[1].ends_with("· from tally.lead/4")
        );
        // The agent's name opens it.
        assert!(marks.iter().any(|(row, mark)| *row == 1
            && matches!(mark, crate::screen::Mark::Buttons(b) if b[0].2 == "sonny:agent/tally.lead")));
        // Too narrow for both, where it came from takes its own row, whole,
        // and the agent's name opens it from there.
        let (lines, _, marks) = view.card(30);
        let shown = text(&lines);
        assert_eq!(shown[2], "from tally.lead/4");
        assert!(marks.iter().any(|(row, mark)| *row == 2
            && matches!(mark, crate::screen::Mark::Buttons(b) if b[0].0 == 6 && b[0].1 == 16)));
        // Two links, each a whole name.
        let page = view.pages.last().unwrap();
        let names: Vec<&str> = page.links.iter().map(|l| l.4.as_str()).collect();
        assert_eq!(names, ["vault-access", "vault-access-old"]);
        assert_eq!(view.keys(), "↑↓ pick · enter follow · ← back · esc close");
        // ↓ picks the first link in view, enter follows it.
        assert_eq!(view.key(key(KeyCode::Down), 0..50), Key::Stay);
        view.key(key(KeyCode::Enter), 0..50);
        settle(&mut view);
        assert_eq!(text(&view.card(80).0)[0], "vault-access");
        assert_eq!(view.keys(), "↑↓ scroll · ← back · esc close");
        // With no link, ↓ scrolls.
        assert_eq!(view.key(key(KeyCode::Down), 0..50), Key::Scroll(false));
        view.key(key(KeyCode::Left), 0..0);
        assert_eq!(text(&view.card(80).0)[0], "release-signing");
        view.key(key(KeyCode::Left), 0..0);
        assert!(!view.on_page());
        // The person's opens are not counted as reads.
        assert!(
            !places
                .home
                .join(".agents/memory/projects/tally/.reads")
                .exists()
        );
        let _ = std::fs::remove_dir_all(&places.home);
    }

    #[test]
    fn x_asks_once_and_enter_removes_through_memory() {
        let places = places("remove");
        save(
            &places,
            Some("tally"),
            "release-signing",
            "project",
            serde_json::json!({}),
        );
        save(
            &places,
            Some("tally"),
            "rounding",
            "project",
            serde_json::json!({}),
        );
        let mut view = view(&places);
        view.key(key(KeyCode::Char('x')), 0..0);
        assert_eq!(
            view.footer().unwrap().to_string(),
            "Remove release-signing? It can't be undone."
        );
        assert_eq!(view.keys(), "enter remove · esc cancel");
        // Esc cancels, and keeps the view open.
        assert_eq!(view.key(key(KeyCode::Esc), 0..0), Key::Stay);
        assert!(view.footer().is_none());
        view.key(key(KeyCode::Char('x')), 0..0);
        view.key(key(KeyCode::Enter), 0..0);
        settle(&mut view);
        assert_eq!(
            view.footer().unwrap().to_string(),
            "removed release-signing"
        );
        let lines = text(&view.card(80).0);
        assert!(!lines.iter().any(|l| l.contains("release-signing")));
        let index =
            std::fs::read_to_string(places.home.join(".agents/memory/projects/tally/MEMORY.md"))
                .unwrap();
        assert!(!index.contains("release-signing") && index.contains("rounding"));
        let _ = std::fs::remove_dir_all(&places.home);
    }

    #[test]
    fn a_day_carries_its_year_only_when_it_is_not_this_one() {
        assert_eq!(day_in("2026-11-03", 2026), "3 Nov");
        assert_eq!(day_in("2025-01-30", 2026), "30 Jan 2025");
        assert_eq!(day_in("soon", 2026), "soon");
        assert!(this_year() >= 2026);
    }
}
