//! Boards in `/commons`: a project's lists of work that agents pull from,
//! drawn from what the commons listing already keeps of each thread. An
//! item is a thread of work on a board, with a state its step posts set;
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
    Taken,
    Open,
    Waiting,
    Done,
    Canceled,
}

impl State {
    pub const ALL: [State; 6] = [
        State::Failed,
        State::Taken,
        State::Open,
        State::Waiting,
        State::Done,
        State::Canceled,
    ];

    fn parse(word: &str) -> Option<Self> {
        Some(match word {
            "failed" => State::Failed,
            "taken" => State::Taken,
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
            State::Taken => "taken",
            State::Open => "open",
            State::Waiting => "waiting",
            State::Done => "done",
            State::Canceled => "canceled",
        }
    }

    fn heading(self) -> &'static str {
        match self {
            State::Failed => "Failed",
            State::Taken => "Taken",
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
            State::Taken => Span::raw("●").fg(Color::Cyan),
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
    /// The agent that took it, by its own name.
    pub holder: Option<String>,
    pub labels: Vec<(String, String)>,
    /// The agents and profiles it is kept for, as `for` names them.
    pub kept_for: Vec<String>,
    /// The threads of the items it needs.
    pub needs: Vec<String>,
    /// The closing post's text: a done one's result, a failed one's why.
    pub result: Option<String>,
    /// When its last post was written, in seconds.
    pub at: i64,
    /// When its holder last wrote in it, in seconds.
    pub holder_at: Option<i64>,
}

/// The item a thread's listing line says, if it is one: `summary` as the
/// commons listing keeps it, its `item` as [`crate::board::item`] gave
/// it, and `at` its last post's time.
pub fn item_of(summary: &Value, at: i64) -> Option<Item> {
    let item = summary.get("work").filter(|item| item.is_object())?;
    let text = |value: &Value| value.as_str().filter(|s| !s.is_empty()).map(str::to_owned);
    let mut labels: Vec<(String, String)> = (item["labels"].as_object().into_iter().flatten())
        .filter_map(|(key, value)| Some((key.clone(), value_text(value)?)))
        .collect();
    labels.sort();
    Some(Item {
        thread: summary["thread"].as_str()?.to_owned(),
        title: (item["title"].as_str())
            .or(summary["title"].as_str())
            .unwrap_or_default()
            .to_owned(),
        board: text(&item["board"])?,
        state: State::parse(item["state"].as_str()?)?,
        holder: text(&item["holder"]),
        labels,
        kept_for: (item["for"].as_array().into_iter().flatten())
            .filter_map(|one| one.as_str().map(str::to_owned))
            .collect(),
        needs: (item["needs"].as_array().into_iter().flatten())
            .filter_map(|need| need.as_str().map(str::to_owned))
            .collect(),
        result: text(&item["result"]),
        at,
        holder_at: item["holder_at"].as_i64(),
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
    match bot
        .strip_prefix(here)
        .and_then(|rest| rest.strip_prefix('.'))
    {
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
    /// The profiles they were made as.
    pub profiles: std::collections::HashSet<String>,
}

impl Present {
    /// Whether `bot` is an agent of this project the listing no longer has.
    pub fn closed(&self, bot: &str) -> bool {
        !self.project.is_empty()
            && !self.bots.is_empty()
            && bot
                .strip_prefix(&self.project)
                .is_some_and(|rest| rest.starts_with('.'))
            && !self.bots.contains(bot)
    }

    /// Whether no agent the window lists may take `item`, open or waiting
    /// on this project's board and kept for agents or profiles none of
    /// them is: the lead starts one, or the work waits.
    pub fn nobody_for(&self, item: &Item) -> bool {
        matches!(item.state, State::Open | State::Waiting)
            && !item.kept_for.is_empty()
            && !self.bots.is_empty()
            && project_of(&item.board) == self.project
            && !(item.kept_for.iter()).any(|one| match one.strip_prefix(crate::board::PROFILE) {
                Some(profile) => self.profiles.contains(profile),
                None => self.bots.contains(one),
            })
    }
}

/// Whom `item` is kept for, in words: `for profile:reviewer`, and
/// `nobody eligible` after it when no agent the window lists is.
fn kept_words(item: &Item, present: &Present) -> Option<String> {
    if item.kept_for.is_empty() {
        return None;
    }
    let mut said = format!("for {}", item.kept_for.join(", "));
    if present.nobody_for(item) {
        said.push_str(", nobody eligible");
    }
    Some(said)
}

/// An item's state in words, with its holder where it has one: `open`,
/// `taken by errors-2`, `errors-2 closed`.
pub fn state_words(item: &Item, here: &str, present: &Present) -> String {
    match (&item.holder, item.state) {
        (Some(holder), State::Taken) if present.closed(holder) => {
            format!("{} closed", named(holder, here))
        }
        (Some(holder), State::Taken) => format!("taken by {}", named(holder, here)),
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
        (Some(holder), State::Taken) if present.closed(holder) => (
            vec![Span::raw(format!("{} closed", named(holder, here))).dim()],
            None,
        ),
        (Some(holder), State::Taken) => (
            vec![Span::raw(named(holder, here)).fg(Color::Blue)],
            Some(holder.clone()),
        ),
        _ => match kept_words(item, present) {
            Some(kept) if matches!(item.state, State::Open | State::Waiting) => {
                (vec![Span::raw(kept).dim()], None)
            }
            _ => (Vec::new(), None),
        },
    };
    // Taken work shows when its holder last wrote there.
    let at = match item.state {
        State::Taken => item.holder_at.unwrap_or(item.at),
        _ => item.at,
    };
    let age = age(now - at);
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
        let labels_w = if labels_on && !labels.is_empty() {
            2 + labels.len()
        } else {
            0
        };
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

/// A plain thread's row on its board's page, `width` wide: its title in
/// the columns an item's mark leaves, and at the right who last wrote and
/// when, `at`, which stay.
pub fn plain_row(thread: &Value, at: i64, width: usize, now: i64, here: &str) -> Line<'static> {
    let by = named(thread["last_by"].as_str().unwrap_or_default(), here);
    let right = format!("{by} · {}", age(now - at));
    let room = width.saturating_sub(3 + 2 + right.chars().count());
    let title = crate::title::cut(thread["title"].as_str().unwrap_or_default(), room);
    let used = 3 + crate::title::width(&title);
    let pad = width.saturating_sub(used + right.chars().count()).max(2);
    Line::from(vec![
        Span::raw("   "),
        Span::raw(title),
        Span::raw(" ".repeat(pad)),
        Span::raw(right).dim(),
    ])
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
                .filter(|need| {
                    others
                        .get(need.as_str())
                        .is_none_or(|n| n.state != State::Done)
                })
                .collect();
            let first = not_done.first()?;
            let first =
                (others.get(first.as_str())).map_or(first.as_str(), |need| need.title.as_str());
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
/// holder or a label says what was typed (`crate::find::says`).
pub fn found_on_page(item: &Item, query: &str) -> bool {
    let labels: Vec<String> = (item.labels.iter())
        .map(|(k, v)| format!("{k}={v}"))
        .collect();
    ([item.title.as_str(), item.holder.as_deref().unwrap_or("")].into_iter())
        .chain(labels.iter().map(String::as_str))
        .any(|words| crate::find::says(query, words))
}

/// What one step post did to its work, in words: `taken`, `dropped`,
/// `took it over from errors-2, closed`. None for a post that is no step,
/// or the first post opening work, which is the work's text.
pub fn action_words(post: &Value, first: bool, here: &str) -> Option<String> {
    let did = post["do"].as_str()?;
    let mut said = match did {
        "open" if first => return None,
        "open" => match post["board"].as_str() {
            Some(board) => format!("opened it as work on {board}"),
            None => "opened it again".to_owned(),
        },
        "take" => match post["took_from"].as_str() {
            Some(from) => format!("took it over from {}, closed", named(from, here)),
            None => "took it".to_owned(),
        },
        "drop" => "dropped it".to_owned(),
        "done" => "done".to_owned(),
        "fail" => "failed it".to_owned(),
        "set" => String::new(),
        other => other.to_owned(),
    };
    // What its fields changed, after what it did.
    let mut changed: Vec<String> = Vec::new();
    if let Some(labels) = post["labels"].as_object() {
        let mut labeled: Vec<String> = (labels.iter())
            .map(|(key, value)| match value_text(value) {
                Some(value) => format!("{key}={value}"),
                None => format!("{key} off"),
            })
            .collect();
        labeled.sort();
        changed.push(format!("labeled {}", labeled.join(" ")));
    }
    if let Some(named) = post["for"].as_array() {
        let named: Vec<&str> = named.iter().filter_map(Value::as_str).collect();
        changed.push(match named.is_empty() {
            true => "kept it for anyone".to_owned(),
            false => format!("kept it for {}", named.join(", ")),
        });
    }
    if let Some(needs) = post["needs"]
        .as_array()
        .filter(|_| !(first || did == "open"))
    {
        let needs: Vec<&str> = needs.iter().filter_map(Value::as_str).collect();
        changed.push(match needs.is_empty() {
            true => "needs nothing now".to_owned(),
            false => format!("needs {}", needs.join(", ")),
        });
    }
    for change in changed {
        if !said.is_empty() {
            said.push_str(", ");
        }
        said.push_str(&change);
    }
    Some(said).filter(|said| !said.is_empty())
}

/// A link on a row: the row, its columns from and to, and what it opens.
pub type Linked = (usize, usize, usize, Target);

/// The row under an item's name, which says where it stands: `work on
/// BOARD`, its state and holder, who it is for, how many items it needs,
/// its labels. The board, the holder and the items it needs are links,
/// each by its columns and what it opens.
pub fn item_head(
    item: &Item,
    here: &str,
    present: &Present,
    width: usize,
) -> (Vec<Line<'static>>, Vec<Linked>) {
    let mut parts: Vec<Vec<(Span<'static>, Option<Target>)>> = Vec::new();
    parts.push(vec![
        (Span::raw("work on ").dim(), None),
        (
            Span::raw(item.board.clone()).bold(),
            Some(Target::Board(item.board.clone())),
        ),
    ]);
    match (&item.holder, item.state) {
        (Some(holder), State::Taken) if present.closed(holder) => parts.push(vec![(
            Span::raw(format!("{} closed", named(holder, here))).dim(),
            None,
        )]),
        (Some(holder), State::Taken) => parts.push(vec![
            (Span::raw("taken by ").dim(), None),
            (
                Span::raw(named(holder, here)).fg(Color::Blue),
                Some(Target::Agent(holder.clone())),
            ),
        ]),
        (_, state) => parts.push(vec![(Span::raw(state.word()).dim(), None)]),
    }
    if let Some(kept) = kept_words(item, present) {
        parts.push(vec![(Span::raw(kept).dim(), None)]);
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
/// or its holder says what was typed (`crate::find::says`).
pub fn found_in_list(item: &Item, query: &str) -> bool {
    [item.title.as_str(), item.holder.as_deref().unwrap_or("")]
        .into_iter()
        .any(|words| crate::find::says(query, words))
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use crate::commons::View;
    use ratatui::crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
    use serde_json::json;
    use std::path::{Path, PathBuf};

    fn home(name: &str) -> PathBuf {
        let home =
            std::env::temp_dir().join(format!("sonny-board-view-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&home);
        home
    }

    /// The commons tool called as `name` of its project, every post of
    /// the thread it replies to shown to it first.
    fn send(home: &Path, name: &str, arguments: Value) -> Value {
        let project = name.split('.').next().unwrap_or(name);
        if let Some(thread) = arguments["reply"]["thread"].as_str()
            && let Ok(posts) =
                crate::commons::posts(&crate::commons::dir(home).join(format!("{thread}.jsonl")))
        {
            crate::commons::told(home, name, thread, &(0..posts.len()).collect::<Vec<_>>());
        }
        let caller = crate::board::Caller {
            name,
            id: None,
            reader: name,
            model: None,
            project: Some(project),
            profile: None,
            lead: name.ends_with(".lead"),
        };
        crate::commons::call_as(home, &caller, &arguments, &|_, _| false).unwrap()
    }

    fn add(home: &Path, by: &str, board: &str, name: &str, title: &str, extra: Value) {
        let mut add =
            json!({"name": name, "title": title, "text": "x", "board": board, "work": true});
        for (key, value) in extra.as_object().into_iter().flatten() {
            add[key] = value.clone();
        }
        send(home, by, json!({"post": add}));
    }

    fn act(home: &Path, by: &str, thread: &str, what: &str, text: &str) {
        send(
            home,
            by,
            json!({"reply": {"thread": thread, "do": what, "text": text}}),
        );
    }

    /// A plain thread, on no board.
    fn thread(home: &Path, by: &str, name: &str, title: &str) {
        let project = by.split('.').next().unwrap_or(by);
        crate::commons::call(
            home,
            by,
            None,
            by,
            None,
            Some(project),
            &json!({"post": {"name": name, "title": title, "text": "?"}}),
        )
        .unwrap();
    }

    /// Every post of `thread` written `seconds` earlier.
    fn age_by(home: &Path, thread: &str, seconds: i64) {
        let path = crate::commons::dir(home).join(format!("{thread}.jsonl"));
        let text = std::fs::read_to_string(&path).unwrap();
        let mut out = String::new();
        for line in text.lines().filter(|l| !l.trim().is_empty()) {
            let mut post: Value = serde_json::from_str(line).unwrap();
            post["at"] = json!(post["at"].as_i64().unwrap() - seconds);
            out.push_str(&format!("{post}\n"));
        }
        std::fs::write(&path, out).unwrap();
    }

    fn text(line: &Line) -> String {
        line.spans
            .iter()
            .map(|span| span.content.as_ref())
            .collect()
    }

    /// Whether `text` ends in an age of seconds, `0s` to `59s`.
    fn seconds_old(text: &str) -> bool {
        let digits = text.strip_suffix('s').unwrap_or_default();
        let digits = &digits[digits.trim_end_matches(|c: char| c.is_ascii_digit()).len()..];
        !digits.is_empty() && digits.parse::<u32>().is_ok_and(|s| s < 60)
    }

    fn card(view: &mut View, width: usize) -> Vec<String> {
        view.card(width).0.iter().map(text).collect()
    }

    fn press(view: &mut View, code: KeyCode) -> crate::commons::Key {
        view.press(KeyEvent::from(code))
    }

    fn ctrl_f(view: &mut View) {
        view.press(KeyEvent::new(KeyCode::Char('f'), KeyModifiers::CONTROL));
    }

    fn typed(view: &mut View, words: &str) {
        for c in words.chars() {
            press(view, KeyCode::Char(c));
        }
    }

    fn picked(view: &mut View, width: usize) -> String {
        let (lines, at) = view.card(width);
        text(&lines[at])
    }

    /// shop's typed errors: two claimed, one open with a label, two
    /// waiting, one done; shop's release, all open; docs' pages, failed
    /// and done; and one thread on no board.
    pub(crate) fn commons(name: &str) -> PathBuf {
        let home = home(name);
        let h = &home;
        let te = "shop/typed-errors";
        add(
            h,
            "shop.lead",
            te,
            "api",
            "Add typed errors to the api",
            json!({}),
        );
        add(
            h,
            "shop.lead",
            te,
            "cli",
            "Add typed errors to the cli",
            json!({}),
        );
        add(
            h,
            "shop.lead",
            te,
            "flaky",
            "Fix the flaky error test",
            json!({"labels": {"kind": "bug"}}),
        );
        add(
            h,
            "shop.lead",
            te,
            "enum",
            "Remove the old error enum",
            json!({"needs": ["api", "cli"]}),
        );
        add(
            h,
            "shop.lead",
            te,
            "review",
            "Review the typed errors change",
            json!({"needs": ["enum"], "for": ["profile:reviewer"]}),
        );
        add(
            h,
            "shop.lead",
            te,
            "docs",
            "Write the error docs",
            json!({}),
        );
        act(h, "shop.errors-2", "api", "take", "mine");
        act(h, "shop.errors-3", "cli", "take", "mine");
        act(h, "shop.errors-1", "docs", "take", "mine");
        act(
            h,
            "shop.errors-1",
            "docs",
            "done",
            "Docs written.\nAll of them.",
        );
        add(
            h,
            "shop.lead",
            "shop/release",
            "notes",
            "Write the release notes",
            json!({}),
        );
        add(
            h,
            "docs.lead",
            "docs/api-pages",
            "pages",
            "Draw the api pages",
            json!({}),
        );
        add(
            h,
            "docs.lead",
            "docs/api-pages",
            "links",
            "Fix the broken links",
            json!({}),
        );
        act(h, "docs.pager", "pages", "take", "mine");
        act(h, "docs.pager", "pages", "fail", "The generator is gone.");
        act(h, "docs.pager", "links", "take", "mine");
        act(h, "docs.pager", "links", "done", "Fixed.");
        thread(
            h,
            "mail.lead",
            "backoff",
            "Provider 429s: how long to back off?",
        );
        home
    }

    #[test]
    fn boards_sit_over_the_threads_this_project_s_first_one_row_each() {
        let home = &commons("section");
        let mut view = View::open(home, "shop".into());
        let lines = card(&mut view, 80);
        assert!(
            lines[0].starts_with("every project · 3 boards · 1 thread ·"),
            "{}",
            lines[0]
        );
        let at = |needle: &str| {
            (lines.iter().position(|l| l.contains(needle)))
                .unwrap_or_else(|| panic!("{needle}: {lines:#?}"))
        };
        assert!(at("Boards · 3") < at("shop/release"));
        // This window's project's first, then by name: never by state.
        assert!(at("shop/release") < at("shop/typed-errors"));
        assert!(at("shop/typed-errors") < at("docs/api-pages"));
        assert!(at("docs/api-pages") < at("Threads · 1"));
        assert!(at("Threads · 1") < at("Provider 429s"));
        // Its most pressing item's mark, its name whole, a count a state
        // that has any, in the page's order, and its last post's age.
        let row = &lines[at("shop/typed-errors")];
        assert!(row.starts_with(" ● shop/typed-errors"), "{row:?}");
        assert!(row.contains("● 2  ○ 1  ◌ 2  ✓ 1"), "{row:?}");
        // Seconds old: written as the test began, however slow the machine.
        assert!(seconds_old(row.trim_end()), "{row:?}");
        assert!(lines[at("docs/api-pages")].starts_with(" ✗ docs/api-pages"));
        assert!(lines[at("docs/api-pages")].contains("✗ 1  ✓ 1"));
        // An item is no row among the threads.
        assert!(
            !lines.iter().any(|l| l.contains("Add typed errors")),
            "{lines:#?}"
        );
        // In a project its boards come first, so the pick starts on one.
        assert!(picked(&mut view, 80).contains("shop/release"));
        let _ = std::fs::remove_dir_all(home);
    }

    #[test]
    fn a_row_keeps_its_place_when_an_item_moves() {
        let home = &commons("order");
        let mut view = View::open(home, "web".into());
        let order = |view: &mut View| -> Vec<String> {
            (card(view, 80).into_iter())
                .filter(|l| l.contains('/') && !l.contains('·'))
                .collect::<Vec<_>>()
                .iter()
                .map(|l| l.split_whitespace().nth(1).unwrap_or_default().to_owned())
                .collect()
        };
        let before = order(&mut view);
        assert_eq!(
            before,
            ["docs/api-pages", "shop/release", "shop/typed-errors"]
        );
        act(home, "shop.errors-4", "notes", "take", "mine");
        view.reload();
        assert_eq!(order(&mut view), before);
        let lines = card(&mut view, 80);
        let release = lines.iter().find(|l| l.contains("shop/release")).unwrap();
        assert!(release.starts_with(" ● shop/release"), "{release:?}");
        let _ = std::fs::remove_dir_all(home);
    }

    #[test]
    fn finished_boards_fold_to_one_last_row() {
        let home = &commons("fold");
        for thread in ["pages", "links"] {
            age_by(home, thread, FINISHED_AFTER + 60);
        }
        // Failed is not over: a board stays while one failed.
        let mut view = View::open(home, "shop".into());
        assert!(
            card(&mut view, 80)
                .iter()
                .any(|l| l.contains("docs/api-pages"))
        );
        act(home, "docs.pager", "pages", "open", "again");
        act(home, "docs.pager", "pages", "take", "mine");
        act(home, "docs.pager", "pages", "done", "Drawn.");
        age_by(home, "pages", FINISHED_AFTER + 60);
        view.reload();
        let lines = card(&mut view, 80);
        assert!(lines.iter().any(|l| l.contains("Boards · 2")), "{lines:#?}");
        let fold = lines
            .iter()
            .position(|l| l.contains("▸ 1 finished board"))
            .unwrap();
        assert!(!lines[..fold].iter().any(|l| l.contains("docs/api-pages")));
        assert!(
            lines[fold + 1].is_empty(),
            "the fold is the boards' last row"
        );
        // → opens it in its place.
        press(&mut view, KeyCode::Down);
        press(&mut view, KeyCode::Down);
        assert!(picked(&mut view, 80).contains("finished board"));
        press(&mut view, KeyCode::Right);
        let lines = card(&mut view, 80);
        assert!(lines.iter().any(|l| l.contains("▾ 1 finished board")));
        assert!(
            lines.iter().any(|l| l.starts_with(" ✓ docs/api-pages")),
            "{lines:#?}"
        );
        let _ = std::fs::remove_dir_all(home);
    }

    #[test]
    fn m_lists_every_item_among_the_threads_with_its_board_and_state() {
        let home = &commons("more");
        let mut view = View::open(home, "shop".into());
        assert!(view.keys().contains("m items"), "{}", view.keys());
        press(&mut view, KeyCode::Char('m'));
        let lines = card(&mut view, 100);
        assert!(
            lines.iter().any(|l| l == "Threads · 1 · 9 items"),
            "{lines:#?}"
        );
        let api = lines
            .iter()
            .position(|l| l.contains("Add typed errors to the api"))
            .unwrap();
        assert_eq!(lines[api], "● Add typed errors to the api");
        assert!(
            lines[api + 1].starts_with("  shop/typed-errors · taken by errors-2 · 2 posts · by shop.lead · last shop.errors-2 · "),
            "{:?}",
            lines[api + 1]
        );
        let backoff = lines
            .iter()
            .position(|l| l.contains("Provider 429s"))
            .unwrap();
        assert!(
            lines[backoff].starts_with("  Provider"),
            "a thread keeps its two columns"
        );
        // Another project's holder by its whole name.
        let pages = lines
            .iter()
            .position(|l| l.contains("Draw the api pages"))
            .unwrap();
        assert!(lines[pages].starts_with("✗ "));
        assert!(
            lines[pages + 1].starts_with("  docs/api-pages · failed · "),
            "{:?}",
            lines[pages + 1]
        );
        press(&mut view, KeyCode::Char('m'));
        assert!(
            !card(&mut view, 100)
                .iter()
                .any(|l| l.contains("Add typed errors"))
        );
        let _ = std::fs::remove_dir_all(home);
    }

    #[test]
    fn a_board_opens_as_its_page_by_state_and_back_keeps_the_pick() {
        let home = &commons("page");
        let mut view = View::open(home, "shop".into());
        press(&mut view, KeyCode::Down);
        assert!(picked(&mut view, 80).contains("shop/typed-errors"));
        press(&mut view, KeyCode::Enter);
        assert!(view.on_page());
        let lines = card(&mut view, 80);
        assert_eq!(
            lines[0],
            "shop/typed-errors · 2 taken · 1 open · 2 waiting · 1 done"
        );
        assert_eq!(lines[1], "board of shop");
        let at = |needle: &str| {
            (lines.iter().position(|l| l.contains(needle)))
                .unwrap_or_else(|| panic!("{needle}: {lines:#?}"))
        };
        assert!(at("Taken · 2") < at("Open · 1"));
        assert!(at("Open · 1") < at("Waiting · 2"));
        assert!(at("Waiting · 2") < at("Done · 1 ▸"));
        // Done starts folded.
        assert!(!lines.iter().any(|l| l.contains("Write the error docs")));
        let api = &lines[at("Add typed errors to the api")];
        assert!(api.starts_with(" ● Add typed errors to the api"), "{api:?}");
        let age = api
            .trim_end()
            .rsplit_once("errors-2 · ")
            .map(|(_, age)| age);
        assert!(age.is_some_and(seconds_old), "{api:?}");
        let flaky = &lines[at("Fix the flaky error test")];
        assert!(
            flaky.contains("Fix the flaky error test  kind=bug"),
            "{flaky:?}"
        );
        let review = at("Review the typed errors change");
        assert!(
            lines[review].contains("for profile:reviewer"),
            "{:?}",
            lines[review]
        );
        assert_eq!(lines[review + 1], "   └ needs Remove the old error enum");
        let gone = at(" ◌ Remove the old error enum");
        assert_eq!(lines[gone + 1], "   └ needs Add typed errors to the api +1");
        // The holder is a link to its agent.
        let card = view.card_rows(80);
        assert!(
            card.agents
                .iter()
                .any(|(_, _, _, bot)| bot == "shop.errors-2")
        );
        // → on Done opens it: a done item's result's first line under it.
        while !picked(&mut view, 80).contains("Done") {
            press(&mut view, KeyCode::Down);
        }
        press(&mut view, KeyCode::Right);
        let lines = card_of(&mut view);
        let docs = lines
            .iter()
            .position(|l| l.contains("Write the error docs"))
            .unwrap();
        assert_eq!(lines[docs + 1], "   └ Docs written.");
        // ← goes back with the pick on the board left.
        press(&mut view, KeyCode::Left);
        assert!(!view.on_page());
        assert!(picked(&mut view, 80).contains("shop/typed-errors"));
        let _ = std::fs::remove_dir_all(home);
    }

    fn card_of(view: &mut View) -> Vec<String> {
        card(view, 80)
    }

    #[test]
    fn an_item_opens_as_its_thread_with_where_it_stands_and_what_agents_did() {
        let home = &commons("item");
        send(
            home,
            "shop.errors-2",
            json!({"reply": {"thread": "api", "text": "x", "labels": {"kind": "refactor"}}}),
        );
        let mut view = View::open(home, "shop".into());
        press(&mut view, KeyCode::Down);
        press(&mut view, KeyCode::Enter);
        while !picked(&mut view, 80).contains("Add typed errors to the api") {
            press(&mut view, KeyCode::Down);
        }
        press(&mut view, KeyCode::Enter);
        assert!(view.reading(), "{:#?}", card(&mut view, 80));
        let lines = card(&mut view, 80);
        assert_eq!(lines[0], "Add typed errors to the api");
        assert_eq!(lines[1], "api");
        assert_eq!(
            lines[2],
            "work on shop/typed-errors · taken by errors-2 · kind=refactor"
        );
        assert!(
            lines
                .iter()
                .any(|l| l.starts_with("shop.errors-2 · took it · ")),
            "{lines:#?}"
        );
        assert!(
            lines
                .iter()
                .any(|l| l.starts_with("shop.errors-2 · labeled kind=refactor · ")),
            "{lines:#?}"
        );
        // The board and the holder are links, picked among with the arrows.
        assert_eq!(view.keys(), "↑↓ pick · esc back");
        press(&mut view, KeyCode::Down);
        let (shown, _) = view.card(80);
        assert!(
            (shown[2].spans.iter()).any(|s| s.content == "shop/typed-errors"
                && s.style
                    .add_modifier
                    .contains(ratatui::style::Modifier::REVERSED)),
            "{:?}",
            shown[2]
        );
        press(&mut view, KeyCode::Down);
        assert_eq!(
            press(&mut view, KeyCode::Enter),
            crate::commons::Key::Open(format!("{}shop.errors-2", crate::links::AGENT))
        );
        // The board's link opens its page; ← back to the item, then to
        // the board it came from with the pick on it.
        press(&mut view, KeyCode::Up);
        press(&mut view, KeyCode::Enter);
        assert_eq!(view.depth(), 3);
        assert!(card(&mut view, 80)[0].starts_with("shop/typed-errors · "));
        press(&mut view, KeyCode::Left);
        press(&mut view, KeyCode::Left);
        assert!(picked(&mut view, 80).contains("Add typed errors to the api"));
        let _ = std::fs::remove_dir_all(home);
    }

    #[test]
    fn the_arrows_pass_from_the_head_s_links_to_a_post_s_and_enter_follows_the_one_picked() {
        let home = &commons("post-links");
        // Sent twice: the first shows the lead the posts it missed.
        for _ in 0..2 {
            crate::commons::call(
                home,
                "shop.lead",
                None,
                "shop.lead",
                None,
                Some("shop"),
                &json!({"reply": {"thread": "api", "text": "See [the spec](https://spec.example/errors)."}}),
            )
            .unwrap();
        }
        let mut view = View::open(home, "shop".into());
        press(&mut view, KeyCode::Down);
        press(&mut view, KeyCode::Enter);
        while !picked(&mut view, 80).contains("Add typed errors to the api") {
            press(&mut view, KeyCode::Down);
        }
        press(&mut view, KeyCode::Enter);
        // The page laid on the pane's screen as the pane lays it after
        // each key: the post's link is the screen's own.
        let mut screen = crate::screen::Screen::default();
        let area = ratatui::layout::Rect::new(0, 0, 80, 40);
        let lay = |view: &mut View, screen: &mut crate::screen::Screen| {
            screen.replace_marked(view.card(80).0, Vec::new());
            screen.render(area, &mut ratatui::buffer::Buffer::empty(area), |_| None);
        };
        lay(&mut view, &mut screen);
        let reversed = |view: &mut View| {
            let (lines, _) = view.card(80);
            (lines.iter().flat_map(|l| &l.spans))
                .filter(|s| (s.style.add_modifier).contains(ratatui::style::Modifier::REVERSED))
                .map(|s| s.content.to_string())
                .collect::<Vec<_>>()
        };
        // The board, the holder, then the post's link: one pick at a time.
        assert_eq!(view.arrow(true, &mut screen), crate::commons::Key::Stay);
        lay(&mut view, &mut screen);
        assert_eq!(reversed(&mut view), ["shop/typed-errors"]);
        view.arrow(true, &mut screen);
        lay(&mut view, &mut screen);
        assert_eq!(reversed(&mut view), ["errors-2"]);
        assert_eq!(screen.picked_link(), None);
        assert_eq!(view.arrow(true, &mut screen), crate::commons::Key::Stay);
        // Drawn again without the holder's pick, the post's stays.
        lay(&mut view, &mut screen);
        assert_eq!(screen.picked_link(), Some("https://spec.example/errors"));
        assert!(reversed(&mut view).is_empty(), "{:?}", reversed(&mut view));
        // Enter leaves the page's own links alone: the post's link is
        // the one followed.
        assert_eq!(press(&mut view, KeyCode::Enter), crate::commons::Key::Stay);
        assert_eq!(view.depth(), 2);
        // The post shows its address too, a link of its own; past it none
        // is left down the page: it scrolls. Up twice is the holder again.
        assert_eq!(view.arrow(true, &mut screen), crate::commons::Key::Stay);
        lay(&mut view, &mut screen);
        assert_eq!(screen.picked_link(), Some("https://spec.example/errors"));
        assert_eq!(
            view.arrow(true, &mut screen),
            crate::commons::Key::Scroll(false)
        );
        lay(&mut view, &mut screen);
        assert_eq!(screen.picked_link(), Some("https://spec.example/errors"));
        view.arrow(false, &mut screen);
        lay(&mut view, &mut screen);
        assert_eq!(screen.picked_link(), Some("https://spec.example/errors"));
        view.arrow(false, &mut screen);
        lay(&mut view, &mut screen);
        assert_eq!(screen.picked_link(), None);
        assert_eq!(reversed(&mut view), ["errors-2"]);
        let _ = std::fs::remove_dir_all(home);
    }

    #[test]
    fn enter_leaves_a_head_link_scrolled_out_of_view_alone() {
        let home = &commons("head-out-of-view");
        let mut view = View::open(home, "shop".into());
        press(&mut view, KeyCode::Down);
        press(&mut view, KeyCode::Enter);
        while !picked(&mut view, 80).contains("Add typed errors to the api") {
            press(&mut view, KeyCode::Down);
        }
        press(&mut view, KeyCode::Enter);
        view.card(80);
        // The board's link picked, then the head scrolled away.
        press(&mut view, KeyCode::Down);
        let depth = view.depth();
        let enter = KeyEvent::from(KeyCode::Enter);
        assert_eq!(view.key(enter, 40..80), crate::commons::Key::Stay);
        assert_eq!(view.depth(), depth);
        // In view again, it is followed.
        view.key(enter, 0..40);
        assert_eq!(view.depth(), depth + 1);
        let _ = std::fs::remove_dir_all(home);
    }

    #[test]
    fn a_closing_post_shows_its_text_and_a_takeover_says_so() {
        let home = &commons("closing");
        let mut view = View::open(home, "shop".into());
        view.open_thread("pages");
        let lines = card(&mut view, 80);
        assert_eq!(lines[2], "work on docs/api-pages · failed");
        let failed = lines
            .iter()
            .position(|l| l.starts_with("docs.pager · failed it · "))
            .unwrap();
        assert_eq!(lines[failed + 1], "  The generator is gone.");
        // A take of a closed holder's work takes it over.
        let caller = crate::board::Caller {
            name: "shop.errors-4",
            id: None,
            reader: "shop.errors-4",
            model: None,
            project: Some("shop"),
            profile: None,
            lead: false,
        };
        let posts = crate::commons::posts(&crate::commons::dir(home).join("api.jsonl")).unwrap();
        crate::commons::told(
            home,
            "shop.errors-4",
            "api",
            &(0..posts.len()).collect::<Vec<_>>(),
        );
        crate::commons::call_as(
            home,
            &caller,
            &json!({"reply": {"thread": "api", "do": "take", "text": "mine now"}}),
            &|_, _| true,
        )
        .unwrap();
        view.open_thread("api");
        let lines = card(&mut view, 80);
        assert!(
            lines
                .iter()
                .any(|l| l.starts_with("shop.errors-4 · took it over from errors-2, closed · ")),
            "{lines:#?}"
        );
        let _ = std::fs::remove_dir_all(home);
    }

    #[test]
    fn a_holder_the_window_no_longer_lists_is_closed() {
        let home = &commons("closed");
        let mut view = View::open(home, "shop".into());
        view.set_present(Present {
            project: "shop".into(),
            bots: ["shop.lead", "shop.errors-3"].map(str::to_owned).into(),
            ..Present::default()
        });
        press(&mut view, KeyCode::Down);
        press(&mut view, KeyCode::Enter);
        let lines = card(&mut view, 80);
        let api = lines
            .iter()
            .find(|l| l.contains("Add typed errors to the api"))
            .unwrap();
        assert!(api.contains("errors-2 closed"), "{api:?}");
        let cli = lines
            .iter()
            .find(|l| l.contains("Add typed errors to the cli"))
            .unwrap();
        assert!(cli.contains("errors-3 · "), "{cli:?}");
        // Another project's agent is never said closed: the window does
        // not list them.
        assert!(
            !Present {
                project: "shop".into(),
                bots: ["shop.lead".into()].into(),
                ..Present::default()
            }
            .closed("docs.pager")
        );
        // Nor before the window has counted any.
        assert!(
            !Present {
                project: "shop".into(),
                ..Present::default()
            }
            .closed("shop.errors-2")
        );
        let _ = std::fs::remove_dir_all(home);
    }

    #[test]
    fn work_kept_for_a_profile_nobody_listed_has_says_nobody_is_eligible() {
        let home = &commons("nobody");
        let review = (crate::commons::summaries(&crate::commons::dir(home)).into_iter())
            .find(|(name, _)| name == "review")
            .and_then(|(_, summary)| item_of(&summary, 0))
            .unwrap();
        let mut present = Present {
            project: "shop".into(),
            bots: ["shop.lead", "shop.errors-3"].map(str::to_owned).into(),
            profiles: ["lead", "worker"].map(str::to_owned).into(),
        };
        assert!(present.nobody_for(&review));
        let (row, _) = page_row(&review, 100, 0, "shop", &present);
        assert!(
            text(&row).contains("for profile:reviewer, nobody eligible"),
            "{row:?}"
        );
        present.profiles.insert("reviewer".into());
        assert!(!present.nobody_for(&review));
        let (row, _) = page_row(&review, 100, 0, "shop", &present);
        assert!(!text(&row).contains("nobody"), "{row:?}");
        // Before the window has counted anyone, it says nothing.
        assert!(!Present::default().nobody_for(&review));
        let _ = std::fs::remove_dir_all(home);
    }

    #[test]
    fn taken_work_shows_when_its_holder_last_wrote_and_plain_threads_sit_beside_it() {
        let home = &home("holder-at");
        add(
            home,
            "shop.lead",
            "shop/x",
            "store",
            "Type the store",
            json!({}),
        );
        act(home, "shop.w", "store", "take", "mine");
        age_by(home, "store", 7200);
        // Another's post later is not the holder's.
        send(
            home,
            "shop.lead",
            json!({"reply": {"thread": "store", "text": "how goes?"}}),
        );
        send(
            home,
            "shop.lead",
            json!({"post": {"name": "plan", "title": "The plan",
            "text": "x", "board": "shop/x"}}),
        );
        let summary = (crate::commons::summaries(&crate::commons::dir(home)).into_iter())
            .find(|(name, _)| name == "store")
            .map(|(_, summary)| summary)
            .unwrap();
        let item = item_of(&summary, summary["at"].as_i64().unwrap()).unwrap();
        let now = item.at;
        let (row, _) = page_row(&item, 80, now, "shop", &Present::default());
        assert!(text(&row).ends_with("w · 2h"), "{row:?}");
        let mut view = View::open(home, "shop".into());
        assert!(picked(&mut view, 80).contains("shop/x"));
        press(&mut view, KeyCode::Enter);
        let lines = card(&mut view, 80);
        let at = lines.iter().position(|l| l.starts_with("Threads · 1"));
        let at = at.unwrap_or_else(|| panic!("{lines:#?}"));
        assert!(lines[at + 1].contains("The plan"), "{lines:#?}");
        // The plain thread is picked and opened as any thread.
        press(&mut view, KeyCode::Down);
        assert!(picked(&mut view, 80).contains("The plan"));
        let _ = std::fs::remove_dir_all(home);
    }

    #[test]
    fn find_in_the_list_keeps_a_board_with_its_matching_items_hung_under_it() {
        let home = &commons("find");
        let mut view = View::open(home, "shop".into());
        let before = picked(&mut view, 80);
        ctrl_f(&mut view);
        typed(&mut view, "enum");
        let lines = card(&mut view, 80);
        assert_eq!(lines.last().unwrap(), "find enum");
        assert!(
            lines.iter().any(|l| l.starts_with(" ● shop/typed-errors")),
            "{lines:#?}"
        );
        assert!(
            lines
                .iter()
                .any(|l| l.trim_end() == "   ◌ Remove the old error enum"),
            "{lines:#?}"
        );
        assert!(
            !lines.iter().any(|l| l.contains("shop/release")),
            "{lines:#?}"
        );
        assert!(!lines.iter().any(|l| l.contains("Provider")), "{lines:#?}");
        // The pick goes to the first match, not the board that holds it.
        assert!(picked(&mut view, 80).contains("Remove the old error enum"));
        assert_eq!(
            view.keys(),
            "↑↓ pick · enter open · ctrl+f find · esc cancel"
        );
        // A board by its name; a thread by its title.
        for _ in 0..4 {
            press(&mut view, KeyCode::Backspace);
        }
        typed(&mut view, "429");
        let lines = card(&mut view, 80);
        assert!(lines.iter().any(|l| l.contains("Provider 429s")));
        assert!(!lines.iter().any(|l| l.contains("Boards")), "{lines:#?}");
        // esc drops the find first, the pick as it was; then closes.
        press(&mut view, KeyCode::Esc);
        assert_eq!(picked(&mut view, 80), before);
        assert!(!card(&mut view, 80).iter().any(|l| l.starts_with("find")));
        assert_eq!(press(&mut view, KeyCode::Esc), crate::commons::Key::Close);
        let _ = std::fs::remove_dir_all(home);
    }

    #[test]
    fn with_m_a_find_shows_an_item_once_under_its_board() {
        let home = &commons("find-m");
        let mut view = View::open(home, "shop".into());
        press(&mut view, KeyCode::Char('m'));
        ctrl_f(&mut view);
        typed(&mut view, "error");
        let lines = card(&mut view, 80);
        let enums = lines
            .iter()
            .filter(|l| l.contains("Remove the old error enum"));
        assert_eq!(enums.count(), 1, "{lines:#?}");
        // The pick passes down every match, none of them twice.
        let mut seen = Vec::new();
        for _ in 0..12 {
            seen.push(picked(&mut view, 80));
            press(&mut view, KeyCode::Down);
        }
        seen.dedup();
        let mut once = seen.clone();
        once.sort();
        once.dedup();
        assert_eq!(seen.len(), once.len(), "{seen:#?}");
        let _ = std::fs::remove_dir_all(home);
    }

    #[test]
    fn find_on_a_board_reads_titles_holders_and_labels() {
        let home = &commons("find-page");
        let mut view = View::open(home, "shop".into());
        press(&mut view, KeyCode::Down);
        press(&mut view, KeyCode::Enter);
        assert_eq!(view.keys(), "↑↓ pick · enter open · ctrl+f find · esc back");
        ctrl_f(&mut view);
        typed(&mut view, "kind=bug");
        let lines = card(&mut view, 80);
        assert!(lines.iter().any(|l| l.contains("Fix the flaky error test")));
        assert!(
            !lines.iter().any(|l| l.contains("Add typed errors")),
            "{lines:#?}"
        );
        assert!(!lines.iter().any(|l| l.contains("Claimed")), "{lines:#?}");
        press(&mut view, KeyCode::Esc);
        // A fold is searched and stands open while finding.
        ctrl_f(&mut view);
        typed(&mut view, "errors-1");
        let lines = card(&mut view, 80);
        assert!(
            lines.iter().any(|l| l.contains("Write the error docs")),
            "{lines:#?}"
        );
        press(&mut view, KeyCode::Esc);
        assert!(view.on_page(), "esc drops the find before the page");
        press(&mut view, KeyCode::Esc);
        assert!(!view.on_page());
        let _ = std::fs::remove_dir_all(home);
    }

    #[test]
    fn too_long_for_its_row_a_board_gives_up_its_age_then_its_counts() {
        let home = &commons("narrow");
        let mut view = View::open(home, "shop".into());
        for width in [150, 120, 60, 44, 30] {
            let lines = card(&mut view, width);
            let row = lines
                .iter()
                .find(|l| l.contains("shop/typed-errors"))
                .unwrap();
            assert!(row.starts_with(" ● shop/typed-errors"), "{width}: {row:?}");
            assert!(row.trim_end().chars().count() <= width, "{width}: {row:?}");
        }
        let mut rows = |w| {
            let lines = card(&mut view, w);
            lines
                .into_iter()
                .find(|l| l.contains("shop/typed-errors"))
                .unwrap()
        };
        assert!(rows(44).contains("● 2  ○ 1  ◌ 2  ✓ 1") && rows(44).trim_end().ends_with('s'));
        assert!(rows(40).contains("● 2  ○ 1  ◌ 2  ✓ 1") && !rows(40).trim_end().ends_with('s'));
        assert_eq!(rows(30).trim_end(), " ● shop/typed-errors  ● 2  ○ 1");
        // A page row: the labels go first, then the age; the holder stays.
        press(&mut view, KeyCode::Down);
        press(&mut view, KeyCode::Enter);
        let mut flaky = |w| {
            card(&mut view, w)
                .into_iter()
                .find(|l| l.contains("flaky"))
                .unwrap()
        };
        assert!(flaky(60).contains("kind=bug"));
        assert!(
            !flaky(36).contains("kind=bug") && flaky(36).trim_end().ends_with('s'),
            "{:?}",
            flaky(36)
        );
        let mut api = |w| {
            card(&mut view, w)
                .into_iter()
                .find(|l| l.contains("errors-2"))
                .unwrap()
        };
        assert!(api(30).trim_end().ends_with("errors-2"), "{:?}", api(30));
        assert!(api(30).contains('…'), "{:?}", api(30));
        let _ = std::fs::remove_dir_all(home);
    }

    #[test]
    fn enter_on_a_board_s_page_opens_its_first_item() {
        let home = &commons("first");
        let mut view = View::open(home, "shop".into());
        press(&mut view, KeyCode::Down);
        press(&mut view, KeyCode::Enter);
        assert!(picked(&mut view, 80).contains("Add typed errors to the"));
        press(&mut view, KeyCode::Enter);
        assert!(view.reading(), "{:#?}", card(&mut view, 80));
        let _ = std::fs::remove_dir_all(home);
    }

    #[test]
    fn with_no_board_the_list_is_as_it_was() {
        let home = &home("none");
        thread(
            home,
            "mail.lead",
            "backoff",
            "Provider 429s: how long to back off?",
        );
        let mut view = View::open(home, "shop".into());
        let lines = card(&mut view, 80);
        assert!(
            lines[0].starts_with("every project · 1 thread ·"),
            "{}",
            lines[0]
        );
        assert!(
            !lines
                .iter()
                .any(|l| l.starts_with("Boards") || l.starts_with("Threads"))
        );
        assert!(!view.keys().contains("m "), "{}", view.keys());
        let _ = std::fs::remove_dir_all(home);
    }

    #[test]
    fn with_boards_off_the_list_is_as_it_was_before_boards() {
        let home = &commons("off");
        let mut view = View::open(home, "shop".into()).without_boards();
        let lines = card(&mut view, 80);
        // No Boards section and no Threads heading: every thread a thread,
        // an item's thread too, by its title alone.
        assert!(
            !lines
                .iter()
                .any(|l| l.starts_with("Boards") || l.starts_with("Threads")),
            "{lines:#?}"
        );
        assert!(
            !lines.iter().any(|l| l.contains("shop/typed-errors")),
            "{lines:#?}"
        );
        assert!(
            lines
                .iter()
                .any(|l| l.trim() == "Add typed errors to the api"),
            "{lines:#?}"
        );
        assert!(!view.keys().contains("m "), "{}", view.keys());
        // An item's thread opens as a thread, with no row for where it
        // stands.
        while !picked(&mut view, 80).contains("Add typed errors to the api") {
            press(&mut view, KeyCode::Down);
        }
        press(&mut view, KeyCode::Enter);
        let lines = card(&mut view, 80);
        assert!(
            !lines.iter().any(|l| l.starts_with("work on ")),
            "{lines:#?}"
        );
        let _ = std::fs::remove_dir_all(home);
    }

    /// What 50 boards of 20 items each, 1,000 items, cost `/commons`: to
    /// open, to draw, to draw again, to read again with nothing changed
    /// and with one thread changed, a board's page, and the list with `m`.
    #[test]
    #[ignore = "timing: cargo test --release -p sonny fifty_boards -- --ignored --nocapture"]
    fn fifty_boards_and_a_thousand_items_cost() {
        use std::time::{Duration, Instant};
        let home = home("cost");
        for b in 0..50 {
            for i in 0..20 {
                add(
                    &home,
                    "shop.lead",
                    &format!("shop/board-{b:02}"),
                    &format!("b{b:02}-{i:02}"),
                    &format!("Fix item {i} of board {b}"),
                    json!({"labels": {"kind": "bug"}}),
                );
            }
        }
        for b in 0..50 {
            act(&home, "shop.fix", &format!("b{b:02}-00"), "take", "on it");
            act(&home, "shop.fix", &format!("b{b:02}-01"), "take", "on it");
            act(&home, "shop.fix", &format!("b{b:02}-01"), "done", "fixed");
        }
        let best = |runs: usize, mut f: Box<dyn FnMut() + '_>| -> Duration {
            (0..runs)
                .map(|_| {
                    let at = Instant::now();
                    f();
                    at.elapsed()
                })
                .min()
                .unwrap()
        };
        let at = Instant::now();
        let mut view = View::open(&home, "shop".into());
        let open = at.elapsed();
        let at = Instant::now();
        view.card_rows(120);
        let first = at.elapsed();
        let redraw = best(20, Box::new(|| drop(view.card_rows(120))));
        let reread = best(20, Box::new(|| view.reload()));
        // One thread changed: claimed, then released, each read and drawn.
        let changed = (0..10)
            .map(|n| {
                let what = if n % 2 == 0 { "take" } else { "drop" };
                act(&home, "shop.fix", "b07-05", what, "x");
                let at = Instant::now();
                view.reload();
                view.card_rows(120);
                at.elapsed()
            })
            .min()
            .unwrap();
        press(&mut view, KeyCode::Enter);
        let page = best(20, Box::new(|| drop(view.card_rows(120))));
        press(&mut view, KeyCode::Left);
        press(&mut view, KeyCode::Char('m'));
        let items = best(20, Box::new(|| drop(view.card_rows(120))));
        println!(
            "open {open:?} · first draw {first:?} · redraw {redraw:?} · read again, nothing changed {reread:?} \
             · one thread changed, read and drawn {changed:?} · a board's page {page:?} · with m {items:?}"
        );
        let _ = std::fs::remove_dir_all(&home);
    }
}
