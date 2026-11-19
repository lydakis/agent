//! `/status`: the project's agents on one card, read from the daemon and
//! Rex with no model call. Its agents are grouped by where they stand, as
//! a thread list is, the lead first in its group: what needs the person,
//! what works, and, folded away, what sits idle and what is marked done.
//! Each row is one line: what the work is, then who does it (its title,
//! then the agent's name), its state where its group does not say it, for
//! how long, and at the right its code reviews; one that needs the person says what for under
//! it. More, on a key: what it follows, what runs for it in the background
//! and where its window is, and the picked one's model, profile and latest
//! step. A task's own helpers fold under it.
use ratatui::{
    style::{Color, Stylize},
    text::{Line, Span},
};
use std::collections::HashMap;
use unicode_width::UnicodeWidthStr;

#[derive(Clone, Default)]
pub struct Row {
    /// The agent's name in its project: `lead`, or the task's.
    pub name: String,
    /// The agent's own name, which a click on its row opens.
    pub bot: String,
    /// What its work is, in the person's words: a task's as started, a
    /// lead's its project's; empty for one with none.
    pub title: String,
    pub state: String,
    /// Whether a turn of its runs.
    pub running: bool,
    /// Seconds its running turn has been at it, or its last turn took.
    pub seconds: Option<u64>,
    /// When its running turn started or its last turn ended (ms), which
    /// orders the rows.
    pub at: Option<u64>,
    pub model: String,
    /// The profile it runs as, its seat's own too: `lead`, `superlead`.
    pub profile: String,
    pub latest: String,
    /// Where it is in Rex: `tab 2`, `tab 1, split`, or empty.
    pub place: String,
    /// The pull request its last reply links, as `owner/repo#N`.
    pub pr: String,
    /// What runs for it with no one typing, as `/background` counts it:
    /// `↻ 2 triggers`, or empty.
    pub background: String,
    /// The icons of the sources it follows, `[sonny.sources.NAME] icon`.
    pub follows: String,
    /// The agent that made it, when one did.
    pub maker: String,
    pub group: Group,
    /// The workflow run this row is, whose strip stands where an
    /// agent's state does.
    pub run: Option<Box<crate::workflow::view::Live>>,
    /// Its line in [`tasks`], once made: a row kept from one count to the
    /// next keeps it.
    pub line: Option<Task>,
}

/// A task as a template note's `tasks()` gives it, shared by every listing
/// of the tasks that holds it.
pub type Task = std::sync::Arc<serde_json::Value>;

/// Where a task stands, as `/status` groups the tasks, in its order.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord)]
pub enum Group {
    /// Waiting on the person's approval or answer, or held on a cause
    /// they can fix.
    NeedsYou,
    /// Its last turn failed: it asks nothing, but it stopped short.
    Failed,
    /// A turn running, queued, rate limited, or waiting on other tasks.
    Working,
    /// Its turn ended, stopped, or never started, with nothing open; a
    /// pull request it opened shows on its row.
    #[default]
    Idle,
    /// Marked done, by itself, its lead or the person: nothing left to do,
    /// until new work reopens it.
    Done,
}

impl Group {
    /// The group as a word for scripts: `needs_you`, `failed`, `working`,
    /// `idle`, `done`.
    pub fn key(self) -> &'static str {
        match self {
            Group::NeedsYou => "needs_you",
            Group::Failed => "failed",
            Group::Working => "working",
            Group::Idle => "idle",
            Group::Done => "done",
        }
    }

    /// The group in a count of `n` of its agents: `1 needs you`,
    /// `2 need you`, `3 working`.
    fn counted(self, n: usize) -> &'static str {
        match self {
            Group::NeedsYou if n == 1 => "needs you",
            Group::NeedsYou => "need you",
            Group::Failed => "failed",
            Group::Working => "working",
            Group::Idle => "idle",
            Group::Done => "done",
        }
    }

    /// The state word a row in this group leaves unsaid: its heading
    /// says it.
    fn own(self) -> &'static str {
        match self {
            Group::NeedsYou => "needs you",
            Group::Failed => "failed",
            Group::Working => "working",
            Group::Idle => crate::started::IDLE,
            Group::Done => crate::started::DONE,
        }
    }

    fn heading(self) -> &'static str {
        match self {
            Group::NeedsYou => "Needs you",
            Group::Failed => "Failed",
            Group::Working => "Working",
            Group::Idle => "Idle",
            Group::Done => "Done",
        }
    }
}

/// Where a task stands: `state` as `/status` names it, whether a turn
/// runs, whether it has a question open for the person, and whether it is
/// marked done, which only a task with no turn running can be.
pub fn group(state: &str, running: bool, asking: bool, done: bool) -> Group {
    match state {
        _ if done && !running => Group::Done,
        "needs you" => Group::NeedsYou,
        // Held on a cause the person can fix: theirs to set right.
        _ if state.starts_with(HELD) => Group::NeedsYou,
        _ if asking => Group::NeedsYou,
        "failed" => Group::Failed,
        _ if running => Group::Working,
        _ => Group::Idle,
    }
}

/// The tasks among `rows`, as a template note's `tasks()` gives them:
/// each its name, agent, state, group (`needs_you`, `working`, `idle`,
/// `done`), whether a turn runs, its pull request and latest step, and
/// the agent that made it.
pub fn tasks(rows: &mut [Row]) -> Vec<Task> {
    (rows.iter_mut())
        .filter(|row| row.name != "lead")
        .map(|row| {
            (row.line.get_or_insert_with(|| {
                // Times that hold still while nothing changes: when its turn
                // started, or ended, in seconds, which `| ago` keeps current;
                // and how long its last turn took, once it is over.
                std::sync::Arc::new(serde_json::json!({"name": row.name, "agent": row.bot,
                    "state": row.state, "group": row.group.key(), "running": row.running,
                    "pr": row.pr, "latest": row.latest, "at": row.at.map(|ms| ms / 1000),
                    "seconds": row.seconds.filter(|_| !row.running), "maker": row.maker}))
            }))
            .clone()
        })
        .collect()
}

/// Whether two listings of the tasks say the same: a task either holds is
/// compared only where the two do not share it.
pub fn same_tasks(one: &[Task], other: &[Task]) -> bool {
    one.len() == other.len()
        && (one.iter().zip(other))
            .all(|(one, other)| std::sync::Arc::ptr_eq(one, other) || one == other)
}

/// The pull request `text` links last, as `owner/repo#N`: a GitHub pull
/// or a GitLab merge request.
pub fn pull_request(text: &str) -> Option<String> {
    pull_requests(text).pop()
}

/// Every pull request `text` links, in its order, each once at its last.
pub fn pull_requests(text: &str) -> Vec<String> {
    let mut found: Vec<String> = Vec::new();
    for pr in text
        .split(|c: char| c.is_whitespace() || "()<>[]\"'`".contains(c))
        .filter_map(|word| {
            let rest = word
                .strip_prefix("https://")
                .or_else(|| word.strip_prefix("http://"))?;
            let parts: Vec<&str> = rest.split('/').collect();
            let (repo, number) = match parts.as_slice() {
                [_, owner, repo, "pull", number, ..] => (format!("{owner}/{repo}"), *number),
                [_, path @ .., "-", "merge_requests", number] => (path.join("/"), *number),
                [_, path @ .., "-", "merge_requests", number, _] => (path.join("/"), *number),
                _ => return None,
            };
            let digits: String = number.chars().take_while(char::is_ascii_digit).collect();
            (!digits.is_empty() && !repo.is_empty()).then(|| format!("{repo}#{digits}"))
        })
    {
        found.retain(|seen| *seen != pr);
        found.push(pr);
    }
    found
}

/// A turn the daemon's stop parked, which its next start goes on with.
pub const PARKED: &str = "parked by a restart";
/// A turn the pull cord parked until work resumes.
pub const PAUSED: &str = "paused";
/// How long a turn's wait for its pool goes unsaid: past it, the state
/// says when it is tried again.
const PACED_SAID_MS: u64 = 5 * 60_000;

/// A paced turn's state, as what it waits on says (`waiting_on`), at
/// `now` in milliseconds: parked by a stop or the pause, or waiting out a
/// rate limit or a failed call's backoff, with the time it is tried again
/// once that is more than five minutes off.
pub fn paced(on: &serde_json::Value, now: u64) -> String {
    if on["stopped"] == true {
        return PARKED.into();
    }
    if on["paused"] == true {
        return PAUSED.into();
    }
    let what = waits_out(on);
    match on["resume_at_ms"].as_u64() {
        Some(at) if at > now + PACED_SAID_MS => format!(
            "{what} until {}",
            crate::asks::clock((at / 1000) as i64, (now / 1000) as i64)
        ),
        _ => what,
    }
}

/// What a paced turn waits out (`waiting_on`): a failed call's backoff
/// says what failed, so a dropped connection never reads as a rate limit;
/// else its provider's pool, closed by a rate limit.
pub fn waits_out(on: &serde_json::Value) -> String {
    match on["retrying"].as_str() {
        Some(code) => format!("{RETRYING}{}", cause(code)),
        None => "rate limited".into(),
    }
}

/// A paced turn's state while it backs off from a failed call, before
/// what failed.
pub const RETRYING: &str = "retrying: ";

/// A state's mark, alone, in its hue: the one table every surface that
/// shows a state draws from. Muted unless it needs you, failed or works.
pub fn sign(state: &str) -> Span<'static> {
    match state {
        "needs you" => Span::raw("?").fg(Color::Yellow).bold(),
        _ if state.starts_with(HELD) => Span::raw("?").fg(Color::Yellow).bold(),
        "failed" => Span::raw("✗").fg(Color::Red),
        "working" => Span::raw("●").fg(Color::Cyan),
        _ if state.starts_with("waiting") => Span::raw("◌").dim(),
        _ if state.starts_with("rate limited") || state.starts_with(RETRYING) => {
            Span::raw("◌").dim()
        }
        "queued" | "starting" | PARKED | PAUSED => Span::raw("◌").dim(),
        crate::started::IDLE | crate::started::QUIET | "completed" => Span::raw("○").dim(),
        crate::started::INTERRUPTED | "removed" => Span::raw("■").dim(),
        crate::started::DONE => Span::raw("✓").dim(),
        _ => Span::raw("·").dim(),
    }
}

/// A state's mark and the space after it, as rows lead with it.
pub fn mark(state: &str) -> Span<'static> {
    let sign = sign(state);
    Span::styled(format!("{} ", sign.content), sign.style)
}

/// A reply's first line of prose: fenced blocks and headings skipped,
/// list markers and emphasis left out.
pub fn plain_line(text: &str) -> String {
    let mut fence = false;
    for line in text.lines() {
        let trimmed = line.trim();
        if trimmed.starts_with("```") {
            fence = !fence;
            continue;
        }
        if fence || trimmed.is_empty() || trimmed.starts_with('#') {
            continue;
        }
        let trimmed = ["- ", "* ", "+ ", "> "]
            .iter()
            .find_map(|m| trimmed.strip_prefix(m))
            .unwrap_or(trimmed);
        return trimmed.replace("**", "").replace('`', "");
    }
    String::new()
}

/// The groups `/status` folds to their headings until opened: Idle and
/// Done, so what shows is what works and what needs the person.
pub const FOLDS: [Group; 2] = [Group::Idle, Group::Done];

/// How much `/status` says past each row's agent, state and time, which
/// every row has, with its code reviews and what a row that needs the
/// person waits for: nothing more (`Quiet`, as it opens); the rest of what
/// each row tracks at its right and the picked one's model, profile and
/// latest step (`Picked`); or all
/// of that for every row, for `sonny status`'s text.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Details<'a> {
    Quiet,
    Picked(Option<&'a str>),
    All,
}

/// What `/status` shows of a project: its agents by group, the lead first
/// in its own, one line each, a task's own helpers folded under it; Idle and Done
/// folded to their headings unless `open` holds their group, and helpers
/// unless `helpers` holds their task. The line each agent's row is
/// on, with its bot, for a click there to open it; each fold's line, for a
/// click to open or shut its group; and each helpers line, with its task.
pub struct Card {
    pub lines: Vec<Line<'static>>,
    pub named: Vec<(usize, String)>,
    pub folds: Vec<(usize, Group)>,
    pub helpers: Vec<(usize, String)>,
}

/// The agents `/status` lists, in the rows' order: the lead, and each
/// task whose line of makers leads to no other task; a task's helpers by
/// the task the lead's line started, and listed too when one needs the
/// person.
struct Listing<'a> {
    listed: Vec<&'a Row>,
    under: HashMap<String, Vec<&'a Row>>,
    /// The bots that are some task's helpers.
    helping: std::collections::HashSet<&'a str>,
}

fn listing(rows: &[Row]) -> Listing<'_> {
    let tasks: Vec<&Row> = rows.iter().filter(|r| r.name != "lead").collect();
    // A task's own helpers fold under the task the lead's line started:
    // each row by the top of its line of makers among the tasks.
    let by_bot: HashMap<&str, &Row> = tasks.iter().map(|r| (r.bot.as_str(), *r)).collect();
    let top = |row: &Row| -> Option<String> {
        let mut at = row;
        let mut seen = 0;
        while let Some(maker) = by_bot.get(at.maker.as_str()) {
            at = maker;
            seen += 1;
            // A ring of makers leads to no task.
            if seen > by_bot.len() {
                return None;
            }
        }
        (at.bot != row.bot).then(|| at.bot.clone())
    };
    let mut under: HashMap<String, Vec<&Row>> = HashMap::new();
    let mut helping = std::collections::HashSet::new();
    // The lead is listed first in its group, as one of the agents.
    let mut listed: Vec<&Row> = rows.iter().filter(|r| r.name == "lead").collect();
    for row in &tasks {
        match top(row) {
            // One that needs the person or failed is listed there too.
            Some(parent) => {
                if matches!(row.group, Group::NeedsYou | Group::Failed) {
                    listed.push(row);
                }
                helping.insert(row.bot.as_str());
                under.entry(parent).or_default().push(row);
            }
            None => listed.push(row),
        }
    }
    Listing {
        listed,
        under,
        helping,
    }
}

/// How many agents each of `/status`'s groups lists, as its head and its
/// headings count them: the footer shows these same counts.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Counts {
    pub needs_you: usize,
    pub failed: usize,
    pub working: usize,
    pub idle: usize,
    pub done: usize,
}

impl Counts {
    pub fn of(self, group: Group) -> usize {
        match group {
            Group::NeedsYou => self.needs_you,
            Group::Failed => self.failed,
            Group::Working => self.working,
            Group::Idle => self.idle,
            Group::Done => self.done,
        }
    }
}

/// `/status`'s groups in the page's order.
const GROUPS: [Group; 5] = [
    Group::NeedsYou,
    Group::Failed,
    Group::Working,
    Group::Idle,
    Group::Done,
];

/// See [`Counts`].
pub fn counts(rows: &[Row]) -> Counts {
    tally(&listing(rows).listed)
}

fn tally(listed: &[&Row]) -> Counts {
    let n = |group| listed.iter().filter(|r| r.group == group).count();
    Counts {
        needs_you: n(Group::NeedsYou),
        failed: n(Group::Failed),
        working: n(Group::Working),
        idle: n(Group::Idle),
        done: n(Group::Done),
    }
}

/// The agents `rows` lists, by their own names.
pub fn bots(rows: &[Row]) -> std::collections::HashSet<String> {
    rows.iter().map(|row| row.bot.clone()).collect()
}

/// See [`Card`].
pub fn card(
    project: &str,
    rows: &[Row],
    width: usize,
    open: &[Group],
    details: Details,
    helpers: &[String],
) -> Card {
    let mut named = Vec::new();
    let mut folds = Vec::new();
    let mut toggles = Vec::new();
    let Listing {
        listed,
        under,
        helping,
    } = listing(rows);
    // The head counts what each group's heading does, in the page's
    // order: the agents listed there, a task's helpers inside its own.
    let mut head = vec![Span::raw(project.to_owned()).bold()];
    let counted = tally(&listed);
    for group in GROUPS {
        let n = counted.of(group);
        if n > 0 {
            head.push(Span::raw(" · ").dim());
            head.push(Span::raw(format!("{n} {}", group.counted(n))));
        }
    }
    let mut out = vec![Line::from(head)];
    let shown = |row: &Row| match details {
        Details::All => true,
        Details::Picked(picked) => picked == Some(row.bot.as_str()),
        Details::Quiet => false,
    };
    let trackers = details != Details::Quiet;
    // The titles share a column, so the names after them line up: as wide
    // as the widest, a helper's counted from its indent, and narrow enough
    // that the widest name still fits after it.
    let titled: Vec<(&Row, usize)> = (listed.iter().map(|row| (*row, 0)))
        .chain((under.values().flatten()).map(|row| (*row, 2)))
        .filter(|(row, _)| !row.title.is_empty() && row.run.is_none())
        .collect();
    let names = (titled.iter()).map(|(row, _)| row.name.width()).max();
    let titles = (titled.iter())
        .map(|(row, indent)| indent + row.title.width())
        .max()
        .unwrap_or(0)
        .min(width.saturating_sub(5 + names.unwrap_or(0)));
    for group in GROUPS {
        let in_group: Vec<&&Row> = listed.iter().filter(|r| r.group == group).collect();
        if in_group.is_empty() {
            continue;
        }
        out.push(Line::default());
        let n = in_group.len();
        let opened = open.contains(&group);
        let mut heading = vec![
            Span::raw(group.heading()).bold(),
            Span::raw(format!(" · {n}")).dim(),
        ];
        // Idle and Done fold to their headings, as a fold of calls does:
        // ▸ shut, ▾ open.
        let folding = FOLDS.contains(&group);
        let shut = folding && !opened;
        if folding {
            folds.push((out.len(), group));
            heading.push(Span::raw(if shut { " ▸" } else { " ▾" }).dim());
        }
        out.push(Line::from(heading));
        let showing = if shut { 0 } else { n };
        for row in in_group.iter().take(showing) {
            named.push((out.len(), row.bot.clone()));
            out.extend(row_lines(row, width, "", titles, shown(row), trackers));
            // A helper listed with what needs the person shows alone.
            let Some(helped) =
                (under.get(&row.bot)).filter(|_| !helping.contains(row.bot.as_str()))
            else {
                continue;
            };
            let unfolded = helpers.contains(&row.bot);
            let mut said = format!(
                "{} {} helper{}",
                if unfolded { "▾" } else { "▸" },
                helped.len(),
                if helped.len() == 1 { "" } else { "s" }
            );
            for state in [Group::NeedsYou, Group::Failed, Group::Working] {
                let k = helped.iter().filter(|h| h.group == state).count();
                if k > 0 {
                    said.push_str(&format!(", {k} {}", state.counted(k)));
                }
            }
            toggles.push((out.len(), row.bot.clone()));
            out.push(Line::from(vec![Span::raw("   "), Span::raw(said).dim()]));
            if unfolded {
                for helper in helped {
                    named.push((out.len(), helper.bot.clone()));
                    out.extend(row_lines(
                        helper,
                        width,
                        "  ",
                        titles,
                        shown(helper),
                        trackers,
                    ));
                }
            }
        }
    }
    Card {
        lines: out,
        named,
        folds,
        helpers: toggles,
    }
}

/// One of an agent's own tasks as the margin lists it: one that needs
/// the person, failed or works, or a workflow it runs.
#[derive(Clone, Debug, PartialEq)]
pub struct Own {
    pub bot: String,
    pub name: String,
    pub title: String,
    pub state: String,
    pub group: Group,
    /// Its helpers' groups: each agent whose line of makers leads to it.
    pub helpers: Vec<Group>,
    pub run: Option<Box<crate::workflow::view::Live>>,
}

/// The tasks among `rows` that `bot` made and that need the person,
/// failed or work, workflows it runs among them, in the order of the
/// groups; each with its helpers counted.
pub fn own(rows: &[Row], bot: &str) -> Vec<Own> {
    let by_bot: HashMap<&str, &Row> = rows.iter().map(|r| (r.bot.as_str(), r)).collect();
    // The task of `bot`'s own that `row`'s line of makers leads to.
    fn under<'a>(
        row: &'a Row,
        bot: &str,
        by_bot: &HashMap<&str, &'a Row>,
        most: usize,
    ) -> Option<&'a str> {
        let mut at = row;
        for _ in 0..=most {
            if at.maker == bot {
                return Some(at.bot.as_str());
            }
            at = by_bot.get(at.maker.as_str())?;
        }
        None
    }
    let mut helpers: HashMap<&str, Vec<Group>> = HashMap::new();
    for row in rows.iter().filter(|r| r.maker != bot && r.run.is_none()) {
        if let Some(task) = under(row, bot, &by_bot, rows.len()) {
            helpers.entry(task).or_default().push(row.group);
        }
    }
    let mut own: Vec<Own> = (rows.iter())
        .filter(|r| r.maker == bot && r.bot != bot)
        .filter(|r| matches!(r.group, Group::NeedsYou | Group::Failed | Group::Working))
        .map(|r| Own {
            bot: r.bot.clone(),
            name: r.name.clone(),
            title: r.title.clone(),
            state: r.state.clone(),
            group: r.group,
            helpers: helpers.remove(r.bot.as_str()).unwrap_or_default(),
            run: r.run.clone(),
        })
        .collect();
    own.sort_by_key(|task| task.group);
    own
}

/// The margin's tasks, `width` wide: `Tasks · 2`, then a row for each,
/// its mark, its title in a column and its name, its helpers counted
/// under it, a workflow with its strip; each row with the agent a click
/// on it goes to. None, nothing.
pub fn own_lines(own: &[Own], width: usize) -> Vec<(Line<'static>, Option<String>)> {
    if own.is_empty() {
        return Vec::new();
    }
    let mut out = vec![(
        Line::from(vec![
            Span::raw(" Tasks").bold(),
            Span::raw(format!(" · {}", own.len())).dim(),
        ]),
        None,
    )];
    let agents = || own.iter().filter(|task| task.run.is_none());
    let names = agents().map(|task| task.name.width()).max().unwrap_or(0);
    let titles = (agents().map(|task| task.title.width()).max().unwrap_or(0))
        .min(width.saturating_sub(5 + names));
    for task in own {
        let mut spans = vec![Span::raw(" "), mark(&task.state)];
        if let Some(run) = &task.run {
            // Its name and its strip, as the Pane draws it: a workflow
            // that has not ended works.
            let run = crate::workflow::view::Live {
                phase: String::new(),
                ..(**run).clone()
            };
            spans = vec![Span::raw(" "), mark("working")];
            spans.extend(run_spans(&run, width, &spans.clone(), None));
            out.push((Line::from(spans), None));
            continue;
        }
        let room = width.saturating_sub(5 + task.name.width());
        let column = titles.min(room);
        if !task.title.is_empty() && column > 1 {
            let title = crate::title::cut(&task.title, column);
            let pad = column.saturating_sub(title.width());
            spans.push(Span::raw(format!("{title}{}  ", " ".repeat(pad))));
        }
        // In the agents' color: a click goes to it.
        spans.push(Span::raw(task.name.clone()).bold().fg(Color::Blue));
        out.push((Line::from(spans), Some(task.bot.clone())));
        if !task.helpers.is_empty() {
            let n = task.helpers.len();
            let mut said = format!("└ {n} helper{}", if n == 1 { "" } else { "s" });
            for state in [Group::NeedsYou, Group::Failed, Group::Working] {
                let k = task.helpers.iter().filter(|h| **h == state).count();
                if k > 0 {
                    said.push_str(&format!(", {k} {}", state.counted(k)));
                }
            }
            out.push((
                Line::from(vec![Span::raw("   "), Span::raw(said).dim()]),
                None,
            ));
        }
    }
    out
}

/// Something broken around the agents, which `/status` says under its
/// heading after a red `✗`: a companion that keeps restarting, turns held
/// on a cause the person can fix, a temporary folder low on room, failures
/// `/background` lists that the person has not seen. When all is well it
/// says nothing.
#[derive(Clone, Debug, PartialEq)]
pub struct Alert {
    pub said: String,
    /// Where to look, muted under it.
    pub detail: String,
    /// The view a click on it opens, as a button link names it.
    pub opens: Option<&'static str>,
}

/// The failures `/background` lists that the person has not seen there,
/// `n` of them: the footer's `✗` counts them with the failed agents and
/// opens `/status`, which says so and opens `/background` on a click.
pub fn background_alert(n: usize) -> Option<Alert> {
    (n > 0).then(|| Alert {
        said: format!("{n} failed in the background"),
        detail: "/background says what".to_owned(),
        opens: Some("background"),
    })
}

/// How long a companion's run must last to count as healthy, as the
/// daemon counts it before its restart backoff starts over.
const HEALTHY_MS: u64 = 60_000;

/// The companions the daemon keeps (`companions`' list) that keep
/// restarting, as of `now_ms`: one that restarted and is down now,
/// waiting for its next start, or up for less than a healthy run. Each log
/// is in `logs`, the daemon's `STORE.companions`.
pub fn companion_alerts(companions: &serde_json::Value, now_ms: u64, logs: &str) -> Vec<Alert> {
    let listed = companions.as_array().into_iter().flatten();
    listed
        .filter(|c| {
            let restarts = c["restarts"].as_u64().unwrap_or(0);
            let up_for = c["started_ms"].as_u64().map(|at| now_ms.saturating_sub(at));
            restarts > 0 && (c["running"] != true || up_for.is_none_or(|up| up < HEALTHY_MS))
        })
        .map(|c| {
            let name = c["name"].as_str().unwrap_or("a companion");
            let restarts = c["restarts"].as_u64().unwrap_or(0);
            let exit = &c["last_exit"];
            let how = match (
                exit["error"].as_str(),
                exit["code"].as_i64(),
                exit["signal"].as_i64(),
            ) {
                (Some("spawn_failed"), ..) => "could not start".to_owned(),
                (Some(_), ..) => "lost track of it".to_owned(),
                (None, Some(code), _) => format!("exits {code}"),
                (None, None, Some(signal)) => format!("killed by signal {signal}"),
                (None, None, None) => "exits".to_owned(),
            };
            Alert {
                said: format!(
                    "{name} keeps restarting · {how} · {restarts} restart{}",
                    if restarts == 1 { "" } else { "s" }
                ),
                detail: format!("its log: {logs}/{name}.log"),
                opens: None,
            }
        })
        .collect()
}

/// Under what share of its space or of its inodes left a temporary folder
/// is an alert.
const TEMP_LOW: f64 = 0.10;

/// The temporary folders every agent's commands share (`stats`' `temp`)
/// with under a tenth of their space or inodes left, each one line that
/// says how much: when either runs out, every agent's temporary writes
/// fail at once ("No space left on device"), inodes with space to spare.
/// A file system that keeps no inode count (a zero total, or as many free
/// as in all) is read by its space alone.
pub fn temp_alerts(temp: &serde_json::Value) -> Vec<Alert> {
    let low = |free: &serde_json::Value, total: &serde_json::Value| {
        let (free, total) = (free.as_u64()?, total.as_u64()?);
        let share = free as f64 / total as f64;
        (total > 0 && free < total && share < TEMP_LOW).then(|| (share * 100.0).floor() as u64)
    };
    (temp.as_array().into_iter().flatten())
        .filter_map(|room| {
            let path = crate::tilde(room["path"].as_str()?);
            let left = match (
                low(&room["bytes_free"], &room["bytes_total"]),
                low(&room["inodes_free"], &room["inodes_total"]),
            ) {
                (Some(space), Some(inodes)) => format!("{space}% space, {inodes}% inodes left"),
                (Some(space), None) => format!("{space}% space left"),
                (None, Some(inodes)) => format!("{inodes}% inodes left"),
                (None, None) => return None,
            };
            Some(Alert {
                said: format!("temp folder {path}: {left}"),
                detail: String::new(),
                opens: None,
            })
        })
        .collect()
}

/// How `/status` names an agent whose turn is held, before the cause.
pub const HELD: &str = "held: ";

/// A provider failure in the person's words, from the daemon's code: the
/// one table every view that says why a provider failed reads (a held
/// turn, a backoff, `/model`'s listing). A code it does not know is said
/// as the daemon's own, never guessed at.
pub fn cause(code: &str) -> String {
    let what = match code {
        "provider_login_rejected" | "provider_login_expired" | "provider_login_unavailable" => {
            "ChatGPT sign-in"
        }
        _ if code.starts_with("provider_aws_credentials") => "AWS sign-in",
        "provider_quota_exhausted" => "usage limit reached",
        "provider_key_unavailable" => "API key not set",
        "provider_http_401" => "API key refused",
        "provider_http_403" => "access refused",
        "provider_unavailable" => "provider not set up",
        "workspace_required" => "no folder to work in",
        "provider_overloaded" | "provider_http_529" => "provider overloaded",
        "provider_rate_limited" | "provider_http_429" => "rate limited",
        "provider_models_limit" | "provider_response_limit" => "reply too large",
        _ if code.starts_with("provider_connection_") || code.starts_with("provider_stream_") => {
            "provider unreachable"
        }
        _ => {
            return match code.strip_prefix("provider_http_") {
                Some(status) => format!("HTTP {status}"),
                None => code.to_owned(),
            };
        }
    };
    what.to_owned()
}

/// What holds a turn, in the person's words ([`cause`]), and how to set it
/// right, from the daemon's cause (`code`) and what the provider said
/// (`detail`).
pub fn held_cause(code: &str, detail: Option<&str>) -> (String, String) {
    // The daemon's detail leads with the variable it started without.
    if code == "provider_key_unavailable"
        && let Some(key) = detail
            .and_then(|detail| detail.split_whitespace().next())
            .filter(|key| key.chars().all(|c| c.is_ascii_alphanumeric() || c == '_'))
    {
        return (
            format!("{key} not set"),
            format!("set {key} in ~/.agent/keys/env and restart the daemon"),
        );
    }
    let fix = match code {
        "provider_login_rejected" | "provider_login_expired" | "provider_login_unavailable" => {
            "run agent login chatgpt; held turns go on at once".to_owned()
        }
        "provider_quota_exhausted" => {
            "they go on when the limit resets, tried again each hour at most".to_owned()
        }
        "provider_key_unavailable" => {
            "set the key in ~/.agent/keys/env and restart the daemon".to_owned()
        }
        "provider_http_401" => "fix the key in ~/.agent/keys/env and restart the daemon".to_owned(),
        "provider_http_403" => "check the account's access, then agent retry".to_owned(),
        "provider_unavailable" => format!(
            "add {} to ~/.agent/config.toml and restart the daemon",
            detail.unwrap_or("it")
        ),
        "workspace_required" => "give the agent a folder, then agent retry".to_owned(),
        _ => format!("{code}; tried again on its own, or now with agent retry"),
    };
    (cause(code), fix)
}

/// The turns held on a cause (`held`'s answer: its counts by cause and
/// its first page), one alert per cause: what holds them, how many agents,
/// those the page names, and how to set it right.
pub fn held_alerts(listed: &serde_json::Value) -> Vec<Alert> {
    // What, its fix, how many agents, and the names the page holds.
    let mut causes: Vec<(String, String, u64, Vec<String>)> = Vec::new();
    for cause in listed["causes"].as_array().into_iter().flatten() {
        let code = cause["code"].as_str().unwrap_or("");
        let (what, fix) = held_cause(code, cause["detail"].as_str());
        // Codes that read the same are one alert; an agent held on two of
        // them counts in each.
        let bots = cause["bots"].as_u64().unwrap_or(0);
        match causes.iter_mut().find(|(known, ..)| *known == what) {
            Some((_, _, count, _)) => *count += bots,
            None => causes.push((what, fix, bots, Vec::new())),
        }
    }
    for turn in listed["held"].as_array().into_iter().flatten() {
        let blocked = &turn["blocked"];
        let (what, _) = held_cause(
            blocked["code"].as_str().unwrap_or(""),
            blocked["detail"].as_str(),
        );
        let bot = turn["bot"].as_str().unwrap_or("an agent").to_owned();
        if let Some((.., names)) = causes.iter_mut().find(|(known, ..)| *known == what)
            && !names.contains(&bot)
        {
            names.push(bot);
        }
    }
    (causes.into_iter())
        .map(|(what, fix, count, names)| {
            let count = count.max(names.len() as u64);
            let rest = count - names.len() as u64;
            Alert {
                said: format!(
                    "{what} · {count} agent{} waiting: {}{}",
                    if count == 1 { "" } else { "s" },
                    names.join(", "),
                    match rest {
                        0 => String::new(),
                        n => format!(" and {n} more"),
                    }
                ),
                detail: fix,
                opens: None,
            }
        })
        .collect()
}

/// An alert's lines at `width`: what is broken after its red `✗`, and
/// where to look muted under it.
pub fn alert_lines(alert: &Alert, width: usize) -> Vec<Line<'static>> {
    let head = Line::from(vec![
        Span::raw("✗ ").fg(Color::Red),
        Span::raw(alert.said.clone()),
    ]);
    let lead = "  ";
    let mut out = crate::transcript::wrap(head, width, Span::raw(""), Span::raw(lead));
    if !alert.detail.is_empty() {
        out.extend(crate::transcript::wrap(
            Line::from(alert.detail.clone()).dim(),
            width,
            Span::raw(lead),
            Span::raw(lead),
        ));
    }
    out
}

/// A workflow run's row after its mark, `width` wide with `before` and
/// `time` around it: its script's name, the phase it is in, then its strip where
/// an agent's state stands. The strip is never broken over two rows: a
/// name too long for both gives way first, cut to no less than a few
/// columns.
fn run_spans(
    run: &crate::workflow::view::Live,
    width: usize,
    before: &[Span<'static>],
    time: Option<&Span<'static>>,
) -> Vec<Span<'static>> {
    use crate::workflow::view;
    let phase = match run.phase.as_str() {
        "" => " ".to_owned(),
        phase => format!(" {phase} · "),
    };
    let stopping = match run.stopping {
        true => " · stopping",
        false => "",
    };
    let fixed = before.iter().map(|s| s.content.width()).sum::<usize>()
        + phase.width()
        + stopping.width()
        + time.map_or(0, |t| t.content.width());
    // The least the strip takes: each worker's cell, or the bar's.
    let workers: usize = (run.phases.iter())
        .map(|(_, c)| c.running + c.done + c.failed)
        .sum();
    let least = match workers {
        0 => "starting".len(),
        n if n <= view::STRIP_MOST => view::cells(&run.phases).len(),
        _ => view::BAR,
    };
    const NAME_LEAST: usize = 8;
    let name_room = width.saturating_sub(fixed + least).max(NAME_LEAST);
    // Its script's name alone, as the Rulebook's row has it: the strip
    // says it is a workflow. Bold in the text's own color, as an
    // automation's is: blue is an agent's name, and a run is no agent.
    let name = crate::transcript::cut(&run.name, name_room);
    let room = width.saturating_sub(fixed + name.width());
    let mut out = vec![Span::raw(name).bold(), Span::raw(phase)];
    out.extend(view::strip(&run.phases, room));
    if !stopping.is_empty() {
        out.push(Span::raw(stopping));
    }
    out
}

/// One agent's lines, after `indent`: its mark, its title in a column
/// `titles` wide, its name, its state where its group does not say it,
/// and its time, at the right its code reviews, and with `trackers` what
/// it follows, what runs for it and where its window is too; under it,
/// with `details` or while it waits on the person or failed, its latest
/// step, and with `details` its model and profile too. Short of room the
/// time goes first, then the title is cut at a word; the mark and the
/// name stay.
fn row_lines(
    row: &Row,
    width: usize,
    indent: &str,
    titles: usize,
    details: bool,
    trackers: bool,
) -> Vec<Line<'static>> {
    let mut out = Vec::new();
    // How long the running turn has been at it, or the last one took.
    let time = match row.seconds {
        Some(s) if row.running => crate::transcript::elapsed(s),
        Some(s) => format!("took {}", crate::transcript::elapsed(s)),
        None => String::new(),
    };
    let mark = match row.group {
        Group::Done => Span::raw("✓ ").dim(),
        // A workflow that has not ended works.
        _ if row.run.is_some() => mark("working"),
        _ => mark(&row.state),
    };
    let mut time = match time.is_empty() {
        true => None,
        false => Some(Span::raw(format!(" · {time}")).dim()),
    };
    let mut spans = vec![Span::raw(format!(" {indent}")), mark];
    match &row.run {
        Some(run) => spans.extend(run_spans(run, width, &spans, time.as_ref())),
        None => {
            // The state word only where its group's heading does not say it.
            let state = (row.state != row.group.own() && !row.state.is_empty())
                .then(|| Span::raw(format!(" · {}", row.state)));
            let name = row.name.width();
            let fixed = spans.iter().map(|s| s.content.width()).sum::<usize>()
                + name
                + state.as_ref().map_or(0, |s| s.content.width());
            let column = titles
                .saturating_sub(indent.width())
                .min(width.saturating_sub(fixed + 2));
            if !row.title.is_empty() && column > 1 {
                // The time goes first, then the title is cut at a word.
                let timed = time.as_ref().map_or(0, |t| t.content.width());
                if fixed + 2 + column + timed > width {
                    time = None;
                }
                let title = crate::title::cut(&row.title, column);
                let pad = column.saturating_sub(title.width());
                spans.push(Span::raw(format!("{title}{}  ", " ".repeat(pad))));
            }
            // In the agents' color: a click opens it.
            spans.push(Span::raw(row.name.clone()).bold().fg(Color::Blue));
            spans.extend(state);
        }
    }
    spans.extend(time);
    // Its code reviews always; the rest only when asked for more.
    let trackers: Vec<&str> = match trackers {
        true => vec![
            row.follows.as_str(),
            row.pr.as_str(),
            row.background.as_str(),
            row.place.as_str(),
        ],
        false => vec![row.pr.as_str()],
    }
    .into_iter()
    .filter(|f| !f.is_empty())
    .collect();
    let left: usize = spans.iter().map(|s| s.content.width()).sum();
    let right = trackers.join(" · ");
    let room = width.saturating_sub(left + 2);
    if !right.is_empty() && room > 0 {
        // At the right edge, as far as it fits: what is followed and the
        // code reviews first.
        let right = crate::transcript::cut(&right, room);
        let gap = width.saturating_sub(left + right.width()).max(2);
        spans.push(Span::raw(" ".repeat(gap)));
        spans.push(Span::raw(right).dim());
    }
    out.push(Line::from(spans));
    let pad = format!("   {indent}");
    if details {
        let model = match row.model.as_str() {
            "" => String::new(),
            id => crate::models::named(id),
        };
        let profile = match row.profile.as_str() {
            "" => String::new(),
            name => format!("{name} profile"),
        };
        let facts: Vec<&str> = [model.as_str(), profile.as_str()]
            .into_iter()
            .filter(|f| !f.is_empty())
            .collect();
        if !facts.is_empty() {
            let line = Line::from(facts.join(" · ")).dim();
            out.extend(crate::transcript::wrap(
                line,
                width,
                Span::raw(pad.clone()),
                Span::raw(pad.clone()),
            ));
        }
    }
    let says = matches!(row.group, Group::NeedsYou | Group::Failed);
    if !row.latest.is_empty() && (details || says) {
        // The agent's words, read as the Markdown they are.
        let line = Line::from(crate::transcript::inline(&row.latest, Default::default())).dim();
        out.extend(crate::transcript::capped(
            line,
            width,
            Span::raw(format!("{pad}└ ")).dim(),
            1,
        ));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_workflow_s_row_draws_its_strip_where_the_counts_were() {
        use crate::workflow::view::{Counts, Live};
        let text = |row: &Row, width: usize| -> String {
            let lines = row_lines(row, width, "", 0, false, false);
            assert_eq!(lines.len(), 1, "one row");
            lines[0].spans.iter().map(|s| s.content.as_ref()).collect()
        };
        let mut row = Row {
            name: "workflow blind-role-eval-score".into(),
            state: "Score · 2 working, 4 finished".into(),
            running: true,
            seconds: Some(19),
            group: Group::Working,
            run: Some(Box::new(Live {
                name: "blind-role-eval-score".into(),
                phase: "Score".into(),
                phases: vec![
                    (
                        "Find".into(),
                        Counts {
                            done: 3,
                            ..Default::default()
                        },
                    ),
                    (
                        "Score".into(),
                        Counts {
                            running: 2,
                            done: 1,
                            failed: 0,
                        },
                    ),
                ],
                ..Default::default()
            })),
            ..Default::default()
        };
        assert_eq!(
            text(&row, 80),
            " ● blind-role-eval-score Score · ✓✓✓ ✓●● · 19s"
        );
        // The run's name is bold in the text's own color: blue is an agent's.
        let lines = row_lines(&row, 80, "", 0, false, false);
        let name = (lines[0].spans.iter())
            .find(|s| s.content == "blind-role-eval-score")
            .expect("the run's name");
        assert!(
            name.style
                .add_modifier
                .contains(ratatui::style::Modifier::BOLD)
        );
        assert_eq!(name.style.fg, None, "no hue on a run's name");
        // Short of room, the name gives way and the strip stays whole.
        assert_eq!(text(&row, 40), " ● blind-role-eva… Score · ✓✓✓ ✓●● · 19s");
        // Past 24 workers a bar and its counts, as far as they fit.
        let run = row.run.as_mut().unwrap();
        run.phases[1].1.running = 40;
        assert_eq!(
            text(&row, 100),
            " ● blind-role-eval-score Score · ✓●●●●●●●●●●●  40 working, 4 finished · 19s"
        );
        assert_eq!(
            text(&row, 60),
            " ● blind-role-eval-score Score · ✓●●●●●●●●●●● · 19s"
        );
        let run = row.run.as_mut().unwrap();
        run.stopping = true;
        run.phase.clear();
        run.phases[1].1.running = 2;
        assert_eq!(
            text(&row, 80),
            " ● blind-role-eval-score ✓✓✓ ✓●● · stopping · 19s"
        );
    }

    #[test]
    fn one_table_says_every_provider_failure_truly() {
        for (code, said) in [
            ("provider_http_401", "API key refused"),
            ("provider_key_unavailable", "API key not set"),
            ("provider_http_403", "access refused"),
            ("provider_login_expired", "ChatGPT sign-in"),
            ("provider_aws_credentials_expired", "AWS sign-in"),
            ("provider_connection_timeout", "provider unreachable"),
            ("provider_stream_failed", "provider unreachable"),
            ("provider_http_501", "HTTP 501"),
            ("provider_http_529", "provider overloaded"),
            // Reached fine, its answer too big: not "unreachable".
            ("provider_models_limit", "reply too large"),
            ("provider_response_limit", "reply too large"),
            // Unknown: the daemon's own word, never a guess.
            ("provider_incomplete", "provider_incomplete"),
        ] {
            assert_eq!(cause(code), said, "{code}");
        }
        // A held turn says what the table says.
        assert_eq!(held_cause("provider_http_401", None).0, "API key refused");
    }

    #[test]
    fn a_turn_held_on_a_missing_key_names_it_and_where_it_goes() {
        let detail = "OPENAI_API_KEY (provider openai) was not set when the daemon started; \
                      it takes its keys from /home/a/.agent/keys/env as it starts";
        assert_eq!(
            held_cause("provider_key_unavailable", Some(detail)),
            (
                "OPENAI_API_KEY not set".to_owned(),
                "set OPENAI_API_KEY in ~/.agent/keys/env and restart the daemon".to_owned()
            )
        );
        // A detail with no variable first still says what and how.
        assert_eq!(
            held_cause("provider_key_unavailable", Some("openai: unset")).0,
            "API key not set"
        );
    }

    #[test]
    fn a_paced_turn_says_what_holds_it() {
        use serde_json::json;
        let now = 1_800_000_000_000;
        assert_eq!(
            paced(&json!({"resume_at_ms": null, "stopped": true}), now),
            PARKED
        );
        assert_eq!(
            paced(&json!({"resume_at_ms": null, "paused": true}), now),
            PAUSED
        );
        assert_eq!(
            paced(&json!({"resume_at_ms": now + 30_000}), now),
            "rate limited"
        );
        let later = now + 2 * 3_600_000;
        assert_eq!(
            paced(&json!({"resume_at_ms": later}), now),
            format!(
                "rate limited until {}",
                crate::asks::clock((later / 1000) as i64, (now / 1000) as i64)
            )
        );
        // A dropped connection's backoff says so, not a rate limit.
        assert_eq!(
            paced(
                &json!({"resume_at_ms": now + 4_000, "retrying": "provider_stream_failed"}),
                now
            ),
            "retrying: provider unreachable"
        );
        assert_eq!(
            paced(
                &json!({"resume_at_ms": later, "retrying": "provider_http_529"}),
                now
            ),
            format!(
                "retrying: provider overloaded until {}",
                crate::asks::clock((later / 1000) as i64, (now / 1000) as i64)
            )
        );
        for state in [
            PARKED,
            PAUSED,
            "rate limited until 14:05",
            "retrying: provider unreachable",
        ] {
            assert_eq!(mark(state).content, "◌ ", "{state}");
        }
    }

    #[test]
    fn the_latest_step_reads_as_prose() {
        assert_eq!(
            plain_line("```mermaid\nflowchart LR\n```\n## Done\n- Added `x` on **line 2**"),
            "Added x on line 2"
        );
    }

    #[test]
    fn a_companion_that_keeps_restarting_is_named_one_that_recovered_is_not() {
        let now = 10_000_000;
        let listed = serde_json::json!([
            // Never started again: fine.
            {"name": "sonny-lookout", "running": true, "started_ms": 1, "restarts": 0},
            // Restarted once, up for an hour since: recovered.
            {"name": "sonny-tool-host", "running": true, "started_ms": now - 3_600_000,
                "restarts": 1, "last_exit": {"code": 1}},
            // A removed binary: down, waiting for its next start.
            {"name": "sonny-reviewer", "running": false, "started_ms": null, "restarts": 14,
                "last_exit": {"error": "spawn_failed", "detail": "No such file"}},
            // Up again only seconds ago, after dying on a signal.
            {"name": "helper", "running": true, "started_ms": now - 5_000, "restarts": 1,
                "last_exit": {"code": null, "signal": 9}},
        ]);
        let alerts = companion_alerts(&listed, now, "~/.agent/state.sqlite.companions");
        let said: Vec<&str> = alerts.iter().map(|a| a.said.as_str()).collect();
        assert_eq!(
            said,
            [
                "sonny-reviewer keeps restarting · could not start · 14 restarts",
                "helper keeps restarting · killed by signal 9 · 1 restart",
            ]
        );
        assert_eq!(
            alerts[0].detail,
            "its log: ~/.agent/state.sqlite.companions/sonny-reviewer.log"
        );
        let lines = alert_lines(&alerts[0], 80);
        let mark = (lines[0].spans.iter())
            .find(|s| !s.content.is_empty())
            .unwrap();
        assert_eq!(mark.content, "✗ ");
        assert_eq!(mark.style.fg, Some(Color::Red));
        assert_eq!(
            lines[1].to_string(),
            "  its log: ~/.agent/state.sqlite.companions/sonny-reviewer.log"
        );
    }

    #[test]
    fn a_temporary_folder_low_on_space_or_inodes_is_one_line_saying_how_much() {
        let room = |path: &str, bytes: (u64, u64), inodes: (u64, u64)| {
            serde_json::json!({"path": path, "bytes_free": bytes.0, "bytes_total": bytes.1,
                "inodes_free": inodes.0, "inodes_total": inodes.1})
        };
        let temp = serde_json::json!([
            // Space to spare, inodes gone: what failed every agent's writes.
            room("/tmp", (75, 100), (3_000, 100_000)),
            room("/var/tmp", (4, 100), (90, 100)),
            room("/both", (9, 100), (1, 100)),
            room("/fine", (10, 100), (10, 100)),
            // No inode count kept, or as many as are free.
            room("/btrfs", (50, 100), (0, 0)),
            room("/apfs", (50, 100), (7, 7)),
        ]);
        let alerts = temp_alerts(&temp);
        let said: Vec<&str> = alerts.iter().map(|a| a.said.as_str()).collect();
        assert_eq!(
            said,
            [
                "temp folder /tmp: 3% inodes left",
                "temp folder /var/tmp: 4% space left",
                "temp folder /both: 9% space, 1% inodes left",
            ]
        );
        // The fact alone: no advice under it.
        assert!(alerts.iter().all(|a| a.detail.is_empty()));
        assert!(temp_alerts(&serde_json::Value::Null).is_empty());
    }

    #[test]
    fn turns_held_on_one_cause_are_one_alert_naming_each_agent_and_the_fix() {
        let held = serde_json::json!({"held": [
            {"bot": "api", "blocked": {"code": "provider_login_rejected", "since_ms": 1}},
            {"bot": "web", "blocked": {"code": "provider_login_expired", "since_ms": 2}},
            {"bot": "web", "blocked": {"code": "provider_login_expired", "since_ms": 2}},
            {"bot": "docs", "blocked": {"code": "provider_unavailable", "detail": "anthropic", "since_ms": 3}},
        ], "causes": [
            {"code": "provider_login_rejected", "turns": 1, "bots": 1},
            {"code": "provider_login_expired", "turns": 2, "bots": 1},
            {"code": "provider_unavailable", "turns": 1, "bots": 1, "detail": "anthropic"},
        ]});
        let alerts = held_alerts(&held);
        assert_eq!(alerts.len(), 2);
        assert_eq!(
            alerts[0].said,
            "ChatGPT sign-in · 2 agents waiting: api, web"
        );
        assert!(alerts[0].detail.starts_with("run agent login chatgpt"));
        assert_eq!(
            alerts[1].said,
            "provider not set up · 1 agent waiting: docs"
        );
        assert!(
            alerts[1]
                .detail
                .contains("add anthropic to ~/.agent/config.toml")
        );
        // Counted in all, named as far as the page goes.
        let many = serde_json::json!({"held": [
            {"bot": "api", "blocked": {"code": "provider_quota_exhausted", "since_ms": 1}},
        ], "causes": [{"code": "provider_quota_exhausted", "turns": 7000, "bots": 6000}]});
        assert_eq!(
            held_alerts(&many)[0].said,
            "usage limit reached · 6000 agents waiting: api and 5999 more"
        );
        assert_eq!(
            group("held: ChatGPT sign-in", true, false, false),
            Group::NeedsYou
        );
    }

    #[test]
    fn tasks_are_grouped_by_where_they_stand() {
        assert_eq!(group("working", true, false, false), Group::Working);
        assert_eq!(group("waiting on fix", true, false, true), Group::Working);
        assert_eq!(group("needs you", true, false, false), Group::NeedsYou);
        // A question asked ends the turn; it still needs the person.
        assert_eq!(group("idle", false, true, false), Group::NeedsYou);
        // A failed turn asks nothing: a group of its own, unless its
        // agent also asks something.
        assert_eq!(group("failed", false, false, false), Group::Failed);
        assert_eq!(group("failed", false, true, false), Group::NeedsYou);
        // A pull request shows on the row, not as a group of its own.
        assert_eq!(group("idle", false, false, false), Group::Idle);
        assert_eq!(group("interrupted", false, false, false), Group::Idle);
        assert_eq!(group("no reply", false, false, false), Group::Idle);
        assert_eq!(group("failed", false, true, true), Group::Done);
    }

    #[test]
    fn every_state_has_one_mark_and_only_three_have_a_hue() {
        let yellow = Some(Color::Yellow);
        for (state, drawn, hue) in [
            ("needs you", "?", yellow),
            ("held: sign in to ChatGPT", "?", yellow),
            ("failed", "✗", Some(Color::Red)),
            ("working", "●", Some(Color::Cyan)),
            ("waiting on fix, docs", "◌", None),
            ("waiting on its reviewer", "◌", None),
            ("queued", "◌", None),
            ("rate limited", "◌", None),
            ("retrying: provider unreachable", "◌", None),
            ("starting", "◌", None),
            ("idle", "○", None),
            ("no reply", "○", None),
            ("interrupted", "■", None),
            ("done", "✓", None),
        ] {
            let sign = sign(state);
            assert_eq!((&*sign.content, sign.style.fg), (drawn, hue), "{state}");
            // Muted is the quiet weight; the three with a hue are not.
            let dim = sign
                .style
                .add_modifier
                .contains(ratatui::style::Modifier::DIM);
            assert_eq!(dim, hue.is_none(), "{state}");
            assert_eq!(mark(state).content, format!("{drawn} "));
        }
    }

    #[test]
    fn a_reply_names_the_pull_request_it_links_last() {
        assert_eq!(
            pull_request(
                "Opened https://github.com/o/r/pull/12, then (https://github.com/o/r/pull/235/files)."
            ),
            Some("o/r#235".into())
        );
        assert_eq!(
            pull_request("MR: <https://gitlab.com/g/sub/r/-/merge_requests/7>"),
            Some("g/sub/r#7".into())
        );
        assert_eq!(
            pull_request("See https://github.com/o/r/issues/3 and #4"),
            None
        );
    }

    fn row(name: &str, state: &str, group: Group) -> Row {
        Row {
            name: name.into(),
            bot: format!("calc.{name}"),
            state: state.into(),
            running: matches!(group, Group::Working),
            seconds: Some(42),
            model: "chatgpt/gpt-6.1-sol".into(),
            group,
            maker: "calc.lead".into(),
            ..Row::default()
        }
    }

    #[test]
    fn the_margin_lists_this_agent_s_own_tasks_that_need_you_failed_or_work() {
        use crate::workflow::view::{Counts, Live};
        let titled = |title: &str, row: Row| Row {
            title: title.into(),
            ..row
        };
        let helper = |name: &str, state: &str, group: Group, maker: &str| Row {
            maker: maker.into(),
            ..row(name, state, group)
        };
        let rows = [
            row("lead", "working", Group::Working),
            titled(
                "Round half cents to even",
                row("halves", "working", Group::Working),
            ),
            helper("halves-a", "working", Group::Working, "calc.halves"),
            helper("halves-b", "idle", Group::Idle, "calc.halves"),
            // A helper's own helper counts for the task too.
            helper("halves-c", "needs you", Group::NeedsYou, "calc.halves-a"),
            titled(
                "Check the README against the code",
                row("readme", "needs you", Group::NeedsYou),
            ),
            // Idle and done are /status's, and another agent's tasks are
            // its own.
            titled("Stop dividing by zero", row("divide", "idle", Group::Idle)),
            titled("Tidy", row("tidy", "done", Group::Done)),
            helper("other", "working", Group::Working, "calc.elsewhere"),
            Row {
                name: "workflow blind-role-eval-score".into(),
                bot: "calc.lead~r6".into(),
                state: "Score · 2 working, 3 finished".into(),
                maker: "calc.lead".into(),
                group: Group::Working,
                run: Some(Box::new(Live {
                    name: "blind-role-eval-score".into(),
                    phase: "Score".into(),
                    phases: vec![
                        (
                            "Find".into(),
                            Counts {
                                done: 3,
                                ..Default::default()
                            },
                        ),
                        (
                            "Score".into(),
                            Counts {
                                running: 2,
                                ..Default::default()
                            },
                        ),
                    ],
                    ..Default::default()
                })),
                ..Row::default()
            },
        ];
        let own = own(&rows, "calc.lead");
        let names: Vec<&str> = own.iter().map(|task| task.name.as_str()).collect();
        assert_eq!(
            names,
            ["readme", "halves", "workflow blind-role-eval-score"]
        );
        assert_eq!(
            own[1].helpers,
            [Group::Working, Group::Idle, Group::NeedsYou]
        );
        let text = |width| -> Vec<String> {
            (own_lines(&own, width).into_iter())
                .map(|(line, _)| line.spans.iter().map(|s| s.content.as_ref()).collect())
                .collect::<Vec<String>>()
                .into_iter()
                .map(|line| line.trim_end().to_owned())
                .collect()
        };
        assert_eq!(
            text(48),
            [
                " Tasks · 3",
                " ? Check the README against the code  readme",
                " ● Round half cents to even           halves",
                "   └ 3 helpers, 1 needs you, 1 working",
                " ● blind-role-eval-score ✓✓✓ ●●",
            ]
        );
        // Narrow, the title is cut at a word; the name stays.
        assert_eq!(text(30)[1], " ? Check the README…    readme");
        // A click on a task goes to it; the heading, its helpers and a
        // workflow go nowhere.
        let goes: Vec<Option<String>> = own_lines(&own, 48)
            .into_iter()
            .map(|(_, bot)| bot)
            .collect();
        assert_eq!(
            goes,
            [
                None,
                Some("calc.readme".into()),
                Some("calc.halves".into()),
                None,
                None
            ]
        );
        // None of its own going: no block.
        assert!(own_lines(&super::own(&rows, "calc.halves-c"), 48).is_empty());
    }

    #[test]
    fn tasks_for_blanks_give_times_that_hold_still_while_nothing_changes() {
        let working = Row {
            at: Some(5_000),
            ..row("add", "working", Group::Working)
        };
        let done = Row {
            at: Some(9_000),
            ..row("sub", "done", Group::Done)
        };
        let lead = row("lead", "working", Group::Working);
        let tasks = tasks(&mut [lead, working, done]);
        assert_eq!(tasks.len(), 2, "the lead is not a task");
        assert_eq!(
            (tasks[0]["at"].as_u64(), tasks[0]["seconds"].as_u64()),
            (Some(5), None)
        );
        assert_eq!(
            (tasks[1]["at"].as_u64(), tasks[1]["seconds"].as_u64()),
            (Some(9), Some(42))
        );
    }

    #[test]
    fn a_row_says_what_the_work_is_then_who_does_it() {
        let rows = [
            Row {
                title: "Tally invoices".into(),
                seconds: None,
                ..row("lead", "waiting on halves", Group::Working)
            },
            Row {
                title: "Check the README against the code".into(),
                seconds: None,
                ..row("readme", "needs you", Group::NeedsYou)
            },
            Row {
                title: "Round half cents to even".into(),
                seconds: Some(130),
                pr: "#412".into(),
                ..row("halves", "working", Group::Working)
            },
            // One with no title: its name alone.
            row("old", "working", Group::Working),
        ];
        let lines = text(&card("Tally invoices", &rows, 72, &[], Details::Quiet, &[]));
        let trimmed: Vec<&str> = lines.iter().map(|l| l.trim_end()).collect();
        assert_eq!(
            trimmed,
            [
                "Tally invoices · 1 needs you · 3 working",
                "",
                "Needs you · 1",
                " ? Check the README against the code  readme",
                "",
                "Working · 3",
                " ◌ Tally invoices                     lead · waiting on halves",
                " ● Round half cents to even           halves · 2m 10s               #412",
                " ● old · 42s",
            ]
        );
        // Too long for its row: the time goes first, then the title is
        // cut at a word; the mark and the name stay, the names still in
        // a column.
        let lines = text(&card("Tally invoices", &rows, 60, &[], Details::Quiet, &[]));
        assert_eq!(
            lines[7].trim_end(),
            " ● Round half cents to even           halves · 2m 10s   #412"
        );
        let lines = text(&card("Tally invoices", &rows, 44, &[], Details::Quiet, &[]));
        assert_eq!(
            lines[7].trim_end(),
            " ● Round half cents to even           halves"
        );
        let lines = text(&card("Tally invoices", &rows, 36, &[], Details::Quiet, &[]));
        assert_eq!(lines[3].trim_end(), " ? Check the README against…  readme");
        assert_eq!(lines[7].trim_end(), " ● Round half cents to even   halves");
        let lines = text(&card("Tally invoices", &rows, 30, &[], Details::Quiet, &[]));
        assert_eq!(lines[7].trim_end(), " ● Round half cents…    halves");
        // Too narrow for any word: the name stays whole.
        let lines = text(&card("Tally invoices", &rows, 14, &[], Details::Quiet, &[]));
        assert!(lines[7].contains("halves"), "{}", lines[7]);
    }

    fn text(card: &Card) -> Vec<String> {
        (card.lines.iter())
            .map(|l| l.spans.iter().map(|s| s.content.as_ref()).collect())
            .collect()
    }

    #[test]
    fn each_agent_is_its_state_and_time_and_more_says_the_rest() {
        let rows = [
            Row {
                profile: "superlead".into(),
                place: "tab 1".into(),
                ..row("lead", "working", Group::Working)
            },
            Row {
                latest: "asks: staging or prod?".into(),
                ..row("zero", "needs you", Group::NeedsYou)
            },
            Row {
                profile: "task".into(),
                pr: "2 reviews · o/r#9".into(),
                follows: "▲".into(),
                background: "↻ 1 automation".into(),
                place: "tab 2".into(),
                latest: "Opened the pull request.".into(),
                ..row("equal", "idle", Group::Idle)
            },
            row("old", "done", Group::Done),
        ];
        // As it opens: each agent, its state and time, the lead first in
        // its group; what one that needs the person waits for; Idle and
        // Done folded.
        let quiet = card("calc", &rows, 80, &[], Details::Quiet, &[]);
        assert_eq!(
            text(&quiet),
            [
                "calc · 1 needs you · 1 working · 1 idle · 1 done",
                "",
                "Needs you · 1",
                " ? zero · took 42s",
                "   └ asks: staging or prod?",
                "",
                "Working · 1",
                " ● lead · 42s",
                "",
                "Idle · 1 ▸",
                "",
                "Done · 1 ▸",
            ]
        );
        assert_eq!(quiet.folds, [(9, Group::Idle), (11, Group::Done)]);
        // Its code reviews show at the right as it opens, the rest not.
        let idle = card("calc", &rows, 80, &[Group::Idle], Details::Quiet, &[]);
        let lines = text(&idle);
        assert_eq!(lines[10].width(), 80);
        assert!(lines[10].starts_with(" ○ equal · took 42s "));
        assert!(lines[10].ends_with(" 2 reviews · o/r#9"), "{}", lines[10]);
        assert!(!lines[10].contains("tab 2") && !lines[10].contains('▲'));
        // More: what each tracks at its right.
        let plain = card(
            "calc",
            &rows,
            80,
            &[Group::Idle],
            Details::Picked(None),
            &[],
        );
        let lines = text(&plain);
        // What it waits for shows whether picked or not.
        assert_eq!(lines[3], " ? zero · took 42s");
        assert_eq!(lines[4], "   └ asks: staging or prod?");
        assert_eq!(
            lines[7].trim_end(),
            " ● lead · 42s                                                              tab 1"
        );
        assert_eq!(lines[7].width(), 80);
        assert_eq!(lines[9], "Idle · 1 ▾");
        assert_eq!(
            lines[10],
            " ○ equal · took 42s               ▲ · 2 reviews · o/r#9 · ↻ 1 automation · tab 2"
        );
        assert_eq!(lines[12], "Done · 1 ▸");
        assert_eq!(lines.len(), 13);
        assert_eq!(plain.folds, [(9, Group::Idle), (12, Group::Done)]);
        // Picked, its model, profile and latest step show under it.
        let picked = card(
            "calc",
            &rows,
            80,
            &[Group::Idle],
            Details::Picked(Some("calc.equal")),
            &[],
        );
        let lines = text(&picked);
        assert_eq!(lines[11], "   GPT 6.1 Sol · chatgpt · task profile");
        assert_eq!(lines[12], "   └ Opened the pull request.");
        let named: Vec<&str> = picked.named.iter().map(|(_, b)| b.as_str()).collect();
        assert_eq!(named, ["calc.zero", "calc.lead", "calc.equal"]);
        let opened = card("calc", &rows, 80, &[Group::Done], Details::Quiet, &[]);
        let lines = text(&opened);
        assert_eq!(lines[lines.len() - 2], "Done · 1 ▾");
        assert_eq!(lines[lines.len() - 1], " ✓ old · took 42s");
    }

    #[test]
    fn what_failed_is_its_own_group_after_what_needs_you() {
        let rows = [
            row("lead", "working", Group::Working),
            Row {
                latest: "asks: staging or prod?".into(),
                ..row("ask", "needs you", Group::NeedsYou)
            },
            Row {
                latest: "provider_failed: login expired".into(),
                ..row("broke", "failed", Group::Failed)
            },
            Row {
                maker: "calc.broke".into(),
                ..row("helper", "failed", Group::Failed)
            },
        ];
        let card = card("calc", &rows, 80, &[], Details::Quiet, &[]);
        assert_eq!(
            text(&card),
            [
                "calc · 1 needs you · 2 failed · 1 working",
                "",
                "Needs you · 1",
                " ? ask · took 42s",
                "   └ asks: staging or prod?",
                "",
                "Failed · 2",
                " ✗ broke · took 42s",
                "   └ provider_failed: login expired",
                "   ▸ 1 helper, 1 failed",
                " ✗ helper · took 42s",
                "",
                "Working · 1",
                " ● lead · 42s",
            ]
        );
        let counted = counts(&rows);
        assert_eq!(
            (counted.needs_you, counted.failed, counted.working),
            (1, 2, 1)
        );
        assert_eq!(background_alert(0), None);
        let alert = background_alert(2).unwrap();
        assert_eq!(alert.said, "2 failed in the background");
        assert_eq!(alert.opens, Some("background"));
    }

    #[test]
    fn idle_folds_to_its_heading_until_opened() {
        let rows: Vec<Row> = (0..11)
            .map(|n| row(&format!("t{n}"), "idle", Group::Idle))
            .collect();
        let shut = card("calc", &rows, 80, &[], Details::Quiet, &[]);
        let lines = text(&shut);
        assert_eq!(lines[2], "Idle · 11 ▸");
        assert_eq!(lines.len(), 3);
        assert!(shut.named.is_empty());
        assert_eq!(shut.folds, [(2, Group::Idle)]);
        let open = card("calc", &rows, 80, &[Group::Idle], Details::Quiet, &[]);
        let lines = text(&open);
        assert_eq!(lines[2], "Idle · 11 ▾");
        assert_eq!(open.named.len(), 11);
        assert_eq!(open.folds, [(2, Group::Idle)]);
    }

    #[test]
    fn a_tasks_own_helpers_fold_under_it() {
        let helper = |name: &str, state: &str, group: Group, maker: &str| Row {
            maker: format!("calc.{maker}"),
            ..row(name, state, group)
        };
        let rows = [
            row("fix", "working", Group::Working),
            helper("review", "working", Group::Working, "fix"),
            helper("tests", "idle", Group::Idle, "fix"),
            // Its helper's helper is the task's too.
            helper("lint", "idle", Group::Idle, "tests"),
            // One that needs the person is listed with what needs them.
            helper("ask", "needs you", Group::NeedsYou, "fix"),
        ];
        let shut = card("calc", &rows, 80, &[], Details::Picked(None), &[]);
        let lines = text(&shut);
        assert_eq!(lines[2], "Needs you · 1");
        assert_eq!(lines[3], " ? ask · took 42s");
        assert_eq!(lines[5], "Working · 1");
        assert_eq!(lines[6], " ● fix · 42s");
        assert_eq!(lines[7], "   ▸ 4 helpers, 1 needs you, 1 working");
        assert_eq!(lines.len(), 8);
        assert_eq!(shut.helpers, [(7, "calc.fix".to_owned())]);
        let open = card(
            "calc",
            &rows,
            80,
            &[],
            Details::Picked(None),
            &["calc.fix".to_owned()],
        );
        let lines = text(&open);
        assert_eq!(lines[7], "   ▾ 4 helpers, 1 needs you, 1 working");
        assert_eq!(lines[8], "   ● review · 42s");
        assert_eq!(lines[9], "   ○ tests · took 42s");
        assert_eq!(lines[10], "   ○ lint · took 42s");
        assert_eq!(lines[11], "   ? ask · took 42s");
        let named: Vec<&str> = open.named.iter().map(|(_, b)| b.as_str()).collect();
        assert_eq!(
            named,
            [
                "calc.ask",
                "calc.fix",
                "calc.review",
                "calc.tests",
                "calc.lint",
                "calc.ask"
            ]
        );
    }
}
