//! Boards in `/commons`: a project's lists of work that agents pull from,
//! drawn from what the commons listing already keeps of each thread. An
//! item is a thread on a board, with a state the board tool's posts set;
//! [`crate::board::item`] says where each stands, and this draws it:
//! the Boards section over the threads, a board's page, an item's row
//! among the threads, and the row under an item's name that says where
//! it stands. No model call and no daemon call: the folder alone.
use ratatui::{
    style::{Color, Stylize},
    text::{Line, Span},
};
use serde_json::Value;
use std::collections::HashMap;

/// Where an item stands, in the order every list of items takes: what
/// broke, what is worked on, what waits to be taken, what cannot start
/// yet, what is over.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum State {
    Failed,
    Claimed,
    Open,
    Waiting,
    Done,
    Canceled,
}

impl State {
    pub const ALL: [State; 6] = [
        State::Failed,
        State::Claimed,
        State::Open,
        State::Waiting,
        State::Done,
        State::Canceled,
    ];

    fn parse(word: &str) -> Option<Self> {
        Some(match word {
            "failed" => State::Failed,
            "claimed" => State::Claimed,
            "open" => State::Open,
            "waiting" => State::Waiting,
            "done" => State::Done,
            "canceled" => State::Canceled,
            _ => return None,
        })
    }

    /// The state in words, as a count and a row say it.
    pub fn word(self) -> &'static str {
        match self {
            State::Failed => "failed",
            State::Claimed => "claimed",
            State::Open => "open",
            State::Waiting => "waiting",
            State::Done => "done",
            State::Canceled => "canceled",
        }
    }

    fn heading(self) -> &'static str {
        match self {
            State::Failed => "Failed",
            State::Claimed => "Claimed",
            State::Open => "Open",
            State::Waiting => "Waiting",
            State::Done => "Done",
            State::Canceled => "Canceled",
        }
    }

    /// Whether its section on a board's page starts folded.
    pub fn folds(self) -> bool {
        matches!(self, State::Done | State::Canceled)
    }

    /// Its mark, the agent's mark for the nearest thing: no mark is made
    /// for an item.
    pub fn sign(self) -> Span<'static> {
        match self {
            State::Failed => Span::raw("✗").fg(Color::Red),
            State::Claimed => Span::raw("●").fg(Color::Cyan),
            State::Open => Span::raw("○").dim(),
            State::Waiting => Span::raw("◌").dim(),
            State::Done => Span::raw("✓").dim(),
            State::Canceled => Span::raw("–").dim(),
        }
    }
}

/// One item as the view draws it.
#[derive(Clone, Debug, PartialEq)]
pub struct Item {
    pub thread: String,
    pub title: String,
    pub board: String,
    pub state: State,
    /// The agent that claimed it, by its own name.
    pub holder: Option<String>,
    pub labels: Vec<(String, String)>,
    /// The agent or profile it is kept for.
    pub kept_for: Option<String>,
    /// The threads of the items it needs.
    pub needs: Vec<String>,
    /// The closing post's text: a done one's result, a failed one's why.
    pub result: Option<String>,
    /// When its last post was written, in seconds.
    pub at: i64,
}

/// The item a thread's listing line says, if it is one: `summary` as the
/// commons listing keeps it, its `item` as [`crate::board::item`] gave
/// it, and `at` its last post's time.
pub fn item_of(summary: &Value, at: i64) -> Option<Item> {
    let item = summary.get("item").filter(|item| item.is_object())?;
    let text = |value: &Value| value.as_str().filter(|s| !s.is_empty()).map(str::to_owned);
    let mut labels: Vec<(String, String)> = (item["labels"].as_object().into_iter().flatten())
        .filter_map(|(key, value)| Some((key.clone(), value_text(value)?)))
        .collect();
    labels.sort();
    Some(Item {
        thread: summary["thread"].as_str()?.to_owned(),
        title: summary["title"].as_str().unwrap_or_default().to_owned(),
        board: text(&item["board"])?,
        state: State::parse(item["state"].as_str()?)?,
        holder: text(&item["holder"]),
        labels,
        kept_for: text(&item["for"]),
        needs: (item["needs"].as_array().into_iter().flatten())
            .filter_map(|need| need.as_str().map(str::to_owned))
            .collect(),
        result: text(&item["result"]),
        at,
    })
}

/// A label's value as written: a string as it is, anything else as JSON.
fn value_text(value: &Value) -> Option<String> {
    match value {
        Value::Null => None,
        Value::String(text) => Some(text.clone()),
        other => Some(other.to_string()),
    }
}

pub use crate::board::project_of;

/// How long a board stays in the list once everything on it is over and
/// no one posts: then it folds with the finished boards.
pub const FINISHED_AFTER: i64 = 7 * 86_400;

/// One board as the list draws it.
#[derive(Clone, Debug, PartialEq)]
pub struct Board {
    pub name: String,
    /// Its items, by their place in the items the view keeps.
    pub items: Vec<usize>,
    /// When its newest post was written, in seconds.
    pub last: i64,
    counts: [usize; 6],
}

impl Board {
    pub fn count(&self, state: State) -> usize {
        self.counts[state as usize]
    }

    /// The state of its most pressing item, which its mark is.
    pub fn state(&self) -> State {
        (State::ALL.into_iter())
            .find(|state| *state != State::Canceled && self.count(*state) > 0)
            .unwrap_or(State::Canceled)
    }

    /// Every item done or canceled, and no post in seven days.
    pub fn finished(&self, now: i64) -> bool {
        (State::ALL.into_iter())
            .filter(|state| !matches!(state, State::Done | State::Canceled))
            .all(|state| self.count(state) == 0)
            && now - self.last >= FINISHED_AFTER
    }
}

/// The boards `items` are on: this window's project's first, then the
/// rest by name, an order that never follows state.
pub fn boards(items: &[Item], here: &str) -> Vec<Board> {
    let mut by_name: HashMap<&str, usize> = HashMap::new();
    let mut out: Vec<Board> = Vec::new();
    for (at, item) in items.iter().enumerate() {
        let board = *by_name.entry(item.board.as_str()).or_insert_with(|| {
            out.push(Board {
                name: item.board.clone(),
                items: Vec::new(),
                last: 0,
                counts: [0; 6],
            });
            out.len() - 1
        });
        let board = &mut out[board];
        board.items.push(at);
        board.last = board.last.max(item.at);
        board.counts[item.state as usize] += 1;
    }
    out.sort_by(|a, b| {
        let theirs = |board: &Board| project_of(&board.name) != here;
        (theirs(a), &a.name).cmp(&(theirs(b), &b.name))
    });
    out
}

/// How long ago `seconds` was, in its largest unit: `4m`, `2h`, `1d`.
pub fn age(seconds: i64) -> String {
    let s = seconds.max(0);
    match s {
        0..60 => format!("{s}s"),
        60..3600 => format!("{}m", s / 60),
        3600..86_400 => format!("{}h", s / 3600),
        _ => format!("{}d", s / 86_400),
    }
}

/// An agent's name as Sonny writes it in a window of `here`: the short
/// one inside its own project, the whole one outside it.
pub fn named(bot: &str, here: &str) -> String {
    match bot.strip_prefix(here).and_then(|rest| rest.strip_prefix('.')) {
        Some(short) if !short.is_empty() => short.to_owned(),
        _ => bot.to_owned(),
    }
}

/// What a window knows of the agents of its own project: those listed
/// now. Its holder is closed only where this says so; another project's
/// agent, or one counted before any listing, is never said closed.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Present {
    pub project: String,
    pub bots: std::collections::HashSet<String>,
}

impl Present {
    /// Whether `bot` is an agent of this project the listing no longer has.
    pub fn closed(&self, bot: &str) -> bool {
        !self.project.is_empty()
            && !self.bots.is_empty()
            && bot.strip_prefix(&self.project).is_some_and(|rest| rest.starts_with('.'))
            && !self.bots.contains(bot)
    }
}

/// An item's state in words, with its holder where it has one: `open`,
/// `claimed by errors-2`, `errors-2 closed`.
pub fn state_words(item: &Item, here: &str, present: &Present) -> String {
    match (&item.holder, item.state) {
        (Some(holder), State::Claimed) if present.closed(holder) => {
            format!("{} closed", named(holder, here))
        }
        (Some(holder), State::Claimed) => format!("claimed by {}", named(holder, here)),
        (_, state) => state.word().to_owned(),
    }
}

/// The counts on a board's row, each state's mark and how many, two
/// spaces apart, as the footer writes counts; a canceled item is not
/// counted.
fn counts_spans(board: &Board, keep: usize) -> Vec<Span<'static>> {
    let mut spans = Vec::new();
    let counted = (State::ALL.into_iter())
        .filter(|state| *state != State::Canceled && board.count(*state) > 0)
        .take(keep);
    for state in counted {
        if !spans.is_empty() {
            spans.push(Span::raw("  "));
        }
        spans.push(state.sign());
        spans.push(Span::raw(format!(" {}", board.count(state))));
    }
    spans
}

fn spans_width(spans: &[Span]) -> usize {
    spans.iter().map(|span| span.width()).sum()
}

/// A board's row in the list, `width` wide: its mark, its name whole and
/// bold in a column `column` wide, a count for each state that has any,
/// and at the right the age of its last post. Too long for its row, the
/// age goes first, then the counts from the right; the mark and the name
/// stay.
pub fn board_row(board: &Board, column: usize, width: usize, now: i64) -> Line<'static> {
    let name = board.name.clone();
    let pad = column.saturating_sub(crate::title::width(&name));
    let lead = vec![
        Span::raw(" "),
        board.state().sign(),
        Span::raw(" "),
        Span::raw(name).bold(),
        Span::raw(" ".repeat(pad)),
    ];
    let age = age(now - board.last);
    let total = (State::ALL.into_iter())
        .filter(|state| *state != State::Canceled && board.count(*state) > 0)
        .count();
    for keep in (0..=total).rev() {
        let counts = counts_spans(board, keep);
        let used = spans_width(&lead) + 2 + spans_width(&counts);
        // With the age, a column between it and the counts at least.
        if keep == total && used + 2 + age.len() <= width {
            let mut spans = lead.clone();
            spans.push(Span::raw("  "));
            spans.extend(counts);
            spans.push(Span::raw(" ".repeat(width - used - age.len())));
            spans.push(Span::raw(age).dim());
            return Line::from(spans);
        }
        if used <= width || keep == 0 {
            let mut spans = lead.clone();
            if keep > 0 {
                spans.push(Span::raw("  "));
                spans.extend(counts);
            }
            return Line::from(spans);
        }
    }
    Line::from(lead)
}

/// The list's head facts for boards: `3 boards`, or none.
pub fn counted(n: usize, one: &str) -> String {
    format!("{n} {one}{}", if n == 1 { "" } else { "s" })
}

/// A board's page head: its name, then a count for each state that has
/// any, in words; under it whose board it is.
pub fn page_head(board: &Board) -> Vec<Line<'static>> {
    let mut head = vec![Span::raw(board.name.clone()).bold()];
    for state in State::ALL {
        let n = board.count(state);
        if n > 0 {
            head.push(Span::raw(" · ").dim());
            head.push(Span::raw(format!("{n} {}", state.word())));
        }
    }
    vec![
        Line::from(head),
        Line::from(format!("board of {}", project_of(&board.name))).dim(),
    ]
}

/// Where an agent's name was drawn on a row: the row, its first column,
/// the column after its last, and the agent.
pub type Named = (usize, usize, usize, String);

/// An item's row on its board's page, `width` wide, with the agent its
/// holder's name links, by its columns: its mark, its title in `ink`, its
/// labels muted, and at the right its holder and the age of its last
/// post. An open item kept for an agent or a profile says `for NAME`,
/// plain words. Too long for its row, the labels go first, then the age,
/// then the title is cut at a word; the mark and the holder stay.
pub fn page_row(
    item: &Item,
    width: usize,
    now: i64,
    here: &str,
    present: &Present,
) -> (Line<'static>, Option<(usize, usize, String)>) {
    let lead = vec![Span::raw(" "), item.state.sign(), Span::raw(" ")];
    let lead_width = spans_width(&lead);
    // What stands at the right, which stays.
    let (who, link): (Vec<Span<'static>>, Option<String>) = match (&item.holder, item.state) {
        (Some(holder), State::Claimed) if present.closed(holder) => (
            vec![Span::raw(format!("{} closed", named(holder, here))).dim()],
            None,
        ),
        (Some(holder), State::Claimed) => (
            vec![Span::raw(named(holder, here)).fg(Color::Blue)],
            Some(holder.clone()),
        ),
        _ => match &item.kept_for {
            Some(kept) if matches!(item.state, State::Open | State::Waiting) => {
                (vec![Span::raw(format!("for {kept}")).dim()], None)
            }
            _ => (Vec::new(), None),
        },
    };
    let age = age(now - item.at);
    let labels: String = (item.labels.iter())
        .map(|(key, value)| format!("{key}={value}"))
        .collect::<Vec<_>>()
        .join(" ");
    let title_width = crate::title::width(&item.title);
    let who_width = spans_width(&who);
    // The right side as it can be: the holder and the age, or the holder.
    let right = |with_age: bool| -> usize {
        match (who_width, with_age) {
            (0, true) => age.len(),
            (w, true) => w + 3 + age.len(),
            (w, false) => w,
        }
    };
    let fits = |labels_on: bool, with_age: bool| -> bool {
        let labels_w = if labels_on && !labels.is_empty() { 2 + labels.len() } else { 0 };
        let r = right(with_age);
        lead_width + title_width + labels_w + if r > 0 { 2 + r } else { 0 } <= width
    };
    let (labels_on, with_age) = match () {
        _ if fits(true, true) => (true, true),
        _ if fits(false, true) => (false, true),
        _ => (false, false),
    };
    let r = right(with_age);
    let room = width.saturating_sub(lead_width + if r > 0 { 2 + r } else { 0 });
    let title = crate::title::cut(&item.title, room);
    let mut spans = lead;
    spans.push(Span::raw(title));
    if labels_on && !labels.is_empty() {
        spans.push(Span::raw(format!("  {labels}")).dim());
    }
    let mut named_at = None;
    if r > 0 {
        let used = spans_width(&spans);
        spans.push(Span::raw(" ".repeat(width.saturating_sub(used + r).max(2))));
        let from = spans_width(&spans);
        if let Some(bot) = link {
            named_at = Some((from, from + who_width, bot));
        }
        spans.extend(who);
        if with_age {
            if who_width > 0 {
                spans.push(Span::raw(" · ").dim());
            }
            spans.push(Span::raw(age).dim());
        }
    }
    (Line::from(spans), named_at)
}

/// What hangs under an item's row on its board's page, if anything: what
/// a waiting item needs, by title, then `+N`; why a failed one failed;
/// a done one's result's first line.
pub fn page_detail(
    item: &Item,
    others: &HashMap<&str, &Item>,
    width: usize,
) -> Option<Line<'static>> {
    let said = match item.state {
        State::Waiting => {
            // Only what is not done yet: a need done waits for nothing.
            let not_done: Vec<&String> = (item.needs.iter())
                .filter(|need| others.get(need.as_str()).is_none_or(|n| n.state != State::Done))
                .collect();
            let first = not_done.first()?;
            let first = (others.get(first.as_str()))
                .map_or(first.as_str(), |need| need.title.as_str());
            match not_done.len() {
                1 => format!("needs {first}"),
                n => format!("needs {first} +{}", n - 1),
            }
        }
        State::Failed | State::Done => crate::status::plain_line(item.result.as_deref()?),
        _ => return None,
    };
    if said.trim().is_empty() {
        return None;
    }
    let room = width.saturating_sub(5);
    Some(Line::from(vec![
        Span::raw("   "),
        Span::raw(format!("└ {}", crate::title::cut(&said, room))).dim(),
    ]))
}

/// A section's heading on a board's page: its state, a muted count, and
/// for one that folds, `▸` shut or `▾` open.
pub fn section_heading(state: State, n: usize, open: bool) -> Line<'static> {
    let mut spans = vec![
        Span::raw(state.heading()).bold(),
        Span::raw(format!(" · {n}")).dim(),
    ];
    if state.folds() {
        spans.push(Span::raw(if open { " ▾" } else { " ▸" }).dim());
    }
    Line::from(spans)
}

/// Whether an item answers a find on a board's page: its title, its
/// holder or a label holds what was typed, case aside.
pub fn found_on_page(item: &Item, query: &str) -> bool {
    let query = query.to_lowercase();
    item.title.to_lowercase().contains(&query)
        || (item.holder.as_deref()).is_some_and(|h| h.to_lowercase().contains(&query))
        || (item.labels.iter()).any(|(k, v)| format!("{k}={v}").to_lowercase().contains(&query))
}

/// What one action post did to its item, in words: `claimed`,
/// `released`, `took it over from errors-2, closed`. None for a post
/// that is no action, or the first post, which is the item's text.
pub fn action_words(post: &Value, here: &str) -> Option<String> {
    let did = post["do"].as_str()?;
    Some(match did {
        "add" => return None,
        "item" => "made it an item".to_owned(),
        "claim" => match post["took_from"].as_str() {
            Some(from) => format!("took it over from {}, closed", named(from, here)),
            None => "claimed".to_owned(),
        },
        "release" => "released".to_owned(),
        "done" => "done".to_owned(),
        "fail" => "failed".to_owned(),
        "cancel" => "canceled".to_owned(),
        "reopen" => "reopened".to_owned(),
        "label" => {
            let mut said: Vec<String> = (post["labels"].as_object().into_iter().flatten())
                .map(|(key, value)| match value_text(value) {
                    Some(value) => format!("{key}={value}"),
                    None => format!("{key} off"),
                })
                .collect();
            said.sort();
            format!("labeled {}", said.join(" "))
        }
        "assign" => match post["for"].as_str().filter(|kept| !kept.is_empty()) {
            Some(kept) => format!("kept it for {kept}"),
            None => "kept it for anyone".to_owned(),
        },
        other => other.to_owned(),
    })
}

/// The row under an item's name, which says where it stands: `item on
/// BOARD`, its state and holder, who it is for, how many items it needs,
/// its labels. The board, the holder and the items it needs are links,
/// each by its columns and what it opens.
pub fn item_head(
    item: &Item,
    here: &str,
    present: &Present,
    width: usize,
) -> (Vec<Line<'static>>, Vec<(usize, usize, usize, Target)>) {
    let mut parts: Vec<Vec<(Span<'static>, Option<Target>)>> = Vec::new();
    parts.push(vec![
        (Span::raw("item on ").dim(), None),
        (
            Span::raw(item.board.clone()).bold(),
            Some(Target::Board(item.board.clone())),
        ),
    ]);
    match (&item.holder, item.state) {
        (Some(holder), State::Claimed) if present.closed(holder) => parts.push(vec![(
            Span::raw(format!("{} closed", named(holder, here))).dim(),
            None,
        )]),
        (Some(holder), State::Claimed) => parts.push(vec![
            (Span::raw("claimed by ").dim(), None),
            (
                Span::raw(named(holder, here)).fg(Color::Blue),
                Some(Target::Agent(holder.clone())),
            ),
        ]),
        (_, state) => parts.push(vec![(Span::raw(state.word()).dim(), None)]),
    }
    if let Some(kept) = &item.kept_for {
        parts.push(vec![(Span::raw(format!("for {kept}")).dim(), None)]);
    }
    if !item.needs.is_empty() {
        let mut part = vec![(Span::raw("needs ").dim(), None)];
        for (at, need) in item.needs.iter().enumerate() {
            if at > 0 {
                part.push((Span::raw(", ").dim(), None));
            }
            part.push((
                Span::raw(need.clone()).bold(),
                Some(Target::Thread(need.clone())),
            ));
        }
        parts.push(part);
    }
    for (key, value) in &item.labels {
        parts.push(vec![(Span::raw(format!("{key}={value}")).dim(), None)]);
    }
    // Laid out a part at a time, a row breaking only between parts, and
    // no row starting or ending with the `·` between them.
    let mut rows: Vec<Vec<Span<'static>>> = vec![Vec::new()];
    let mut links = Vec::new();
    for part in parts {
        let part_width: usize = part.iter().map(|(span, _)| span.width()).sum();
        let row = rows.last_mut().expect("a row");
        let used = spans_width(row);
        if !row.is_empty() && used + 3 + part_width > width {
            rows.push(Vec::new());
        }
        let at = rows.len() - 1;
        let row = rows.last_mut().expect("a row");
        if !row.is_empty() {
            row.push(Span::raw(" · ").dim());
        }
        for (span, target) in part {
            let from = spans_width(row);
            let to = from + span.width();
            row.push(span);
            if let Some(target) = target {
                links.push((at, from, to, target));
            }
        }
    }
    (rows.into_iter().map(Line::from).collect(), links)
}

/// What a link on an item's page opens.
#[derive(Clone, Debug, PartialEq)]
pub enum Target {
    Board(String),
    Agent(String),
    Thread(String),
}

/// Whether an item is listed by a find in the list: its title, its board
/// or its holder holds what was typed, case aside.
pub fn found_in_list(item: &Item, query: &str) -> bool {
    let query = query.to_lowercase();
    item.title.to_lowercase().contains(&query)
        || (item.holder.as_deref()).is_some_and(|h| h.to_lowercase().contains(&query))
}
