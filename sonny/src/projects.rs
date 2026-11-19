//! **Projects**: every project where the conversation was, one row a
//! project, so the person goes from one lead to another without a
//! multiplexer. A project is its lead. Projects nest as their folders do,
//! keep the order of their titles whatever their agents do, and those
//! quiet for a week fold to one last row. The pane beside it shows the
//! picked project's agents, as it shows a project's own at rest.
//!
//! This file holds the page: what it lists, in what order, how its keys
//! move the pick and how it is drawn. What each project's agents are doing
//! is read as `/status` reads it, by the window ([`crate::status`]).
use crate::status::{self, Counts, Group};
use ratatui::{
    crossterm::event::{KeyCode, KeyEvent, KeyModifiers},
    style::Stylize,
    text::{Line, Span},
};
use unicode_width::UnicodeWidthStr;

/// A project none of whose agents has had a turn in this long folds
/// away, as the Rulebook says: seven days.
pub const IDLE_AFTER_MS: u64 = 7 * 24 * 60 * 60 * 1000;

/// A project as the page lists it.
#[derive(Clone, Debug, Default, PartialEq)]
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
        let query = query.to_lowercase();
        self.title.to_lowercase().contains(&query) || self.name.to_lowercase().contains(&query)
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
/// project however the rows change, or on the fold.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Pick {
    Project(String),
    Fold,
}

/// What a key did.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Act {
    /// Nothing more to do.
    Stay,
    /// The page closes: the conversation is back, and what the pane showed.
    Close,
    /// Go to this lead: the window becomes its.
    Go(String),
}

/// The page.
#[derive(Debug, Default)]
pub struct View {
    projects: Vec<Project>,
    /// The lead of the project the window is in.
    here: String,
    pick: Option<Pick>,
    /// Whether the idle projects are listed in place of their fold.
    fold_open: bool,
    /// What `ctrl+f` narrows the rows to, while finding.
    pub finding: Option<String>,
    /// Whether a read landed: until then the page says `reading…`.
    pub read: bool,
    /// The first row in view, kept so the pick stays in view.
    top: usize,
    /// The rows in view, as last drawn, for the mouse: each line with the
    /// row it draws.
    drawn: Vec<Option<usize>>,
}

impl View {
    /// The page in a window whose project's lead is `here`: the pick
    /// starts on it.
    pub fn new(here: &str) -> Self {
        View {
            here: here.to_owned(),
            pick: Some(Pick::Project(here.to_owned())),
            ..Default::default()
        }
    }

    /// The projects as read now. The pick stays on its project; where
    /// that project is gone, on the first row. The first read opens the
    /// fold when the project the window is in is in it, so the pick can
    /// start there.
    pub fn set(&mut self, projects: Vec<Project>, now: u64) {
        self.projects = projects;
        if !self.read {
            self.read = true;
            let idle = self.idle(now);
            if let Some(at) = self.at(&self.here)
                && idle[at]
            {
                self.fold_open = true;
            }
        }
        let shown = self.shown(now);
        let kept = match &self.pick {
            Some(pick) => shown.iter().any(|row| self.is(row, pick)),
            None => false,
        };
        if !kept {
            self.pick = shown.first().map(|row| self.pick_of(*row));
        }
    }

    #[cfg(test)]
    pub fn projects(&self) -> &[Project] {
        &self.projects
    }

    fn at(&self, lead: &str) -> Option<usize> {
        self.projects.iter().position(|p| p.lead == lead)
    }

    /// The project picked, which the pane shows.
    pub fn picked(&self) -> Option<&Project> {
        match &self.pick {
            Some(Pick::Project(lead)) => self.at(lead).map(|at| &self.projects[at]),
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
        let keep: Vec<bool> = match self.finding.as_deref().filter(|q| !q.is_empty()) {
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

    fn is(&self, row: &Shown, pick: &Pick) -> bool {
        match (row, pick) {
            (Shown::Project(at, _), Pick::Project(lead)) => self.projects[*at].lead == *lead,
            (Shown::Fold(..), Pick::Fold) => true,
            _ => false,
        }
    }

    fn pick_of(&self, row: Shown) -> Pick {
        match row {
            Shown::Project(at, _) => Pick::Project(self.projects[at].lead.clone()),
            Shown::Fold(..) => Pick::Fold,
        }
    }

    /// The row the pick is on.
    fn picked_row(&self, shown: &[Shown]) -> Option<usize> {
        let pick = self.pick.as_ref()?;
        shown.iter().position(|row| self.is(row, pick))
    }

    /// Whether the idle projects' fold is open and the pick is in it.
    fn in_fold(&self, shown: &[Shown]) -> bool {
        let Some(at) = self.picked_row(shown) else {
            return false;
        };
        let fold = shown.iter().position(|row| matches!(row, Shown::Fold(..)));
        self.fold_open && fold.is_some_and(|fold| at >= fold)
    }

    /// A key, the page having the keys.
    pub fn key(&mut self, key: &KeyEvent, now: u64) -> Act {
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        // Finding: what is typed narrows the rows; the arrows still pick.
        if let Some(query) = &mut self.finding {
            match key.code {
                KeyCode::Esc => {
                    self.finding = None;
                    self.settle(now);
                    return Act::Stay;
                }
                KeyCode::Backspace => {
                    query.pop();
                    self.found(now);
                    return Act::Stay;
                }
                KeyCode::Char(c) if !ctrl && !key.modifiers.contains(KeyModifiers::ALT) => {
                    query.push(c);
                    self.found(now);
                    return Act::Stay;
                }
                _ => {}
            }
        }
        let shown = self.shown(now);
        let at = self.picked_row(&shown);
        match key.code {
            KeyCode::Up | KeyCode::Down if !shown.is_empty() => {
                let last = shown.len() - 1;
                let next = match (key.code, at) {
                    (KeyCode::Up, Some(at)) => at.saturating_sub(1),
                    (KeyCode::Down, Some(at)) => (at + 1).min(last),
                    (KeyCode::Up, None) => last,
                    _ => 0,
                };
                self.pick = Some(self.pick_of(shown[next]));
                Act::Stay
            }
            KeyCode::Enter | KeyCode::Right => match at.map(|at| shown[at]) {
                Some(Shown::Fold(_, open)) => {
                    if !open {
                        self.fold_open = true;
                    } else if key.code == KeyCode::Enter {
                        self.fold_open = false;
                    }
                    Act::Stay
                }
                Some(Shown::Project(at, _)) => {
                    let lead = self.projects[at].lead.clone();
                    match lead == self.here {
                        true => Act::Close,
                        false => Act::Go(lead),
                    }
                }
                None => Act::Stay,
            },
            KeyCode::Left => {
                // In the opened fold, it folds; else to the holder.
                if self.in_fold(&shown) {
                    self.fold_open = false;
                    self.pick = Some(Pick::Fold);
                    return Act::Stay;
                }
                if let Some(Shown::Project(at, _)) = at.map(|at| shown[at]) {
                    let holders = self.holders();
                    if let Some(holder) = holders[at]
                        && shown
                            .iter()
                            .any(|row| matches!(row, Shown::Project(h, _) if *h == holder))
                    {
                        self.pick = Some(Pick::Project(self.projects[holder].lead.clone()));
                    }
                }
                Act::Stay
            }
            KeyCode::Char('f') if ctrl => {
                self.finding = Some(String::new());
                Act::Stay
            }
            KeyCode::Char('c') if ctrl => Act::Close,
            KeyCode::Esc => Act::Close,
            _ => Act::Stay,
        }
    }

    /// The pick on a row the find still shows, else its first.
    fn settle(&mut self, now: u64) {
        let shown = self.shown(now);
        if self.picked_row(&shown).is_none() {
            self.pick = shown.first().map(|row| self.pick_of(*row));
        }
    }

    /// The pick after what is found changed: it stays on a project that
    /// matches, else goes to the first that does, not to one kept only
    /// because it holds a match.
    fn found(&mut self, now: u64) {
        let shown = self.shown(now);
        let query = self.finding.clone().unwrap_or_default();
        let matches = |row: &Shown| match row {
            Shown::Project(at, _) => self.projects[*at].found(&query),
            Shown::Fold(..) => false,
        };
        let picked = self.picked_row(&shown).map(|at| shown[at]);
        if query.is_empty() || picked.as_ref().is_some_and(matches) {
            return self.settle(now);
        }
        match shown.iter().find(|row| matches(row)) {
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
        let shown = self.shown(now);
        let Some(row) = shown.get(row).copied() else {
            return Act::Stay;
        };
        self.pick = Some(self.pick_of(row));
        self.key(&KeyEvent::from(KeyCode::Enter), now)
    }

    /// The wheel: the rows in view move, the pick stays.
    pub fn scroll(&mut self, down: bool) {
        self.top = match down {
            true => self.top + 3,
            false => self.top.saturating_sub(3),
        };
    }

    /// The page's keys, as its title row says them.
    pub fn keys(&self) -> crate::hint::Keys {
        use crate::hint::{Enter, Esc, Keys};
        match self.finding {
            Some(_) => Keys::new().pick().enter(Enter::Open).esc(Esc::Cancel),
            None => Keys::new()
                .pick()
                .enter(Enter::Open)
                .key("ctrl+f", "find")
                .esc(Esc::Close),
        }
    }

    /// The page, `width` wide and `height` tall: its title row with its
    /// keys, the head, a blank row, then the rows, the pick reversed and
    /// kept in view.
    pub fn lines(&mut self, width: usize, height: usize, now: u64) -> Vec<Line<'static>> {
        let keys = self.keys();
        let name = "projects";
        let room = width.saturating_sub(1 + name.width() + 3);
        let mut title = vec![Span::raw(" "), Span::raw(name).bold(), Span::raw("   ")];
        title.extend(crate::hint::spans(&keys.fit(room)));
        let mut out = vec![Line::from(title), head(&self.projects, width)];
        self.drawn = vec![None, None];
        if !self.read {
            out.push(Line::default());
            out.push(Line::from(" reading…").dim());
            self.drawn.extend([None, None]);
            return out;
        }
        out.push(Line::default());
        self.drawn.push(None);
        let shown = self.shown(now);
        if shown.is_empty() {
            let said = match &self.finding {
                Some(query) if !query.is_empty() => format!(" no project matches {query}"),
                _ => " no project yet".to_owned(),
            };
            out.push(Line::from(said).dim());
            return out;
        }
        let rows = height.saturating_sub(out.len()).max(1);
        let picked = self.picked_row(&shown);
        // The pick stays in view, two rows from either end where it can.
        if let Some(at) = picked {
            if at < self.top {
                self.top = at;
            } else if at >= self.top + rows {
                self.top = at + 1 - rows;
            }
        }
        self.top = self.top.min(shown.len().saturating_sub(rows));
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
        for (i, row) in shown.iter().enumerate().skip(self.top).take(rows) {
            let mut line = match *row {
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
            if Some(i) == picked {
                crate::list::pick(&mut line);
            }
            out.push(line);
            self.drawn.push(Some(i));
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
        assert_eq!(drawn[3], " ✗ Bookkeeping  superlead  ✗ 1");
        assert_eq!(drawn[4], "   ○ Ledger import");
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
        // → on the fold opens it, ← inside it folds it again.
        for _ in 0..3 {
            view.key(&key(KeyCode::Down), NOW);
        }
        assert_eq!(view.pick, Some(Pick::Fold));
        view.key(&key(KeyCode::Right), NOW);
        assert!(text(&mut view, 70).contains(&" ▾ 4 idle projects".to_owned()));
        view.key(&key(KeyCode::Down), NOW);
        assert_eq!(view.picked().unwrap().lead, "a.lead");
        assert_eq!(text(&mut view, 70)[8], "   ○ a");
        view.key(&key(KeyCode::Left), NOW);
        assert_eq!(view.pick, Some(Pick::Fold));
        assert_eq!(text(&mut view, 70).len(), 8);
        assert_eq!(view.key(&key(KeyCode::Tab), NOW), Act::Stay);
        assert_eq!(view.key(&key(KeyCode::Esc), NOW), Act::Close);
    }

    #[test]
    fn find_narrows_by_title_and_name_keeping_what_holds_a_match() {
        let mut view = rulebook();
        let ctrl_f = KeyEvent::new(KeyCode::Char('f'), KeyModifiers::CONTROL);
        view.key(&ctrl_f, NOW);
        for c in "ledg".chars() {
            view.key(&KeyEvent::from(KeyCode::Char(c)), NOW);
        }
        let drawn = text(&mut view, 70);
        assert_eq!(drawn[0], " projects   ↑↓ pick · enter open · esc cancel");
        assert_eq!(
            &drawn[3..],
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
        assert_eq!(drawn[5], "   ? Tally invoices");
        assert_eq!(drawn[3], " ○ Bookkeeping");
        let drawn = text(&mut view, 16);
        assert_eq!(drawn[5], "   ? Tally…");
        assert_eq!(drawn[3], " ○ Bookkeeping");
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
}
