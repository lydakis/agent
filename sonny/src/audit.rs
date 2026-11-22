//! `/audit`: every tool call the project's agents made, or every agent's,
//! newest first, from the daemon's audit trail: when, which agent, the
//! tool, what kind of tool it is and where it ran, who let it run, how it
//! ended, and the start of what it was given. Filtered by kind, place and
//! who decided; ↑ and ↓ pick a row and Enter opens its agent. One daemon
//! read, `audit`, each time a filter changes and every few seconds while
//! it is open: the newest rows only, which the trail answers at once.
use crate::background::Scope;
use ratatui::crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use ratatui::{
    style::{Color, Stylize},
    text::{Line, Span},
};
use serde_json::{Value, json};
use std::time::{Duration, Instant};
use unicode_width::UnicodeWidthStr;

/// How often the open view reads again.
const FRESH: Duration = Duration::from_secs(5);
/// The rows one read shows.
const ROWS: usize = 200;
/// Each filter's values in the order its key steps through them, the
/// daemon's words; none is every row.
const KINDS: [&str; 7] = [
    "shell", "file", "mcp", "browser", "desktop", "agents", "web",
];
const PLACES: [&str; 3] = ["sandbox", "outside", "external"];
const APPROVERS: [&str; 4] = ["nobody", "person", "reviewer", "rule"];

/// What a key did to the view.
#[derive(Debug, PartialEq)]
pub enum Key {
    Stay,
    Close,
    /// Open this agent, the picked row's.
    Open(String),
}

/// `/audit`, open.
pub struct View {
    scope: Scope,
    /// Every agent's calls, not only the project's.
    all: bool,
    /// Each filter's place in its values, past the last none.
    kind: usize,
    place: usize,
    approved: usize,
    rows: Vec<Value>,
    error: Option<String>,
    /// When it was read last; none until it is, or once it must be again.
    at: Option<Instant>,
    /// The row ↑ and ↓ picked, by its place in the list.
    pub pick: Option<usize>,
    /// `ctrl+f`: what is typed narrows the rows read to those that say
    /// it, and the pick as it was, which `esc` puts back. It reads
    /// nothing: the rows are the last read's.
    finding: Option<crate::find::Find<Option<usize>>>,
}

/// The value at `at` among `values`, none past them.
fn picked(values: &[&'static str], at: usize) -> Option<&'static str> {
    values.get(at).copied()
}

impl View {
    pub fn new(scope: Scope) -> Self {
        Self {
            scope,
            all: false,
            kind: KINDS.len(),
            place: PLACES.len(),
            approved: APPROVERS.len(),
            rows: Vec::new(),
            error: None,
            at: None,
            pick: None,
            finding: None,
        }
    }

    /// Whether it should be read again now.
    pub fn due(&self) -> bool {
        self.at.is_none_or(|at| at.elapsed() >= FRESH)
    }

    /// The `audit` request for what it shows.
    pub fn query(&self) -> Value {
        let mut query = json!({"limit": ROWS});
        if !self.all {
            query["bot"] = json!(self.scope.glob);
        }
        for (field, value) in [
            ("kind", picked(&KINDS, self.kind)),
            ("place", picked(&PLACES, self.place)),
            ("approved", picked(&APPROVERS, self.approved)),
        ] {
            if let Some(value) = value {
                query[field] = json!(value);
            }
        }
        query
    }

    /// What the read came back with.
    pub fn loaded(&mut self, page: Result<Value, String>) {
        self.at = Some(Instant::now());
        match page {
            Ok(page) => {
                self.error = None;
                self.rows = page["rows"].as_array().cloned().unwrap_or_default();
                // With rows, one is always picked: the first to start; while
                // finding, one that says what is typed.
                self.pick = (!self.rows.is_empty())
                    .then(|| self.pick.unwrap_or(0).min(self.rows.len() - 1));
                if self.pick.is_some_and(|at| !self.shown(at)) {
                    self.pick = self.shown_rows().first().copied();
                }
            }
            Err(error) => self.error = Some(error),
        }
    }

    /// Whether the row at `at` says what is typed to find, in the words
    /// it is drawn with: every row before anything is typed.
    fn shown(&self, at: usize) -> bool {
        let Some(find) = self.finding.as_ref().filter(|find| find.typed()) else {
            return true;
        };
        let Some(row) = self.rows.get(at) else {
            return false;
        };
        let name = self.name(row["bot"].as_str().unwrap_or("?"));
        find.says(&line(row, &name, name.width(), usize::MAX / 2, false).to_string())
    }

    /// The rows shown, by their place in the list.
    fn shown_rows(&self) -> Vec<usize> {
        (0..self.rows.len()).filter(|at| self.shown(*at)).collect()
    }

    /// Whether a find is under way.
    pub fn finding(&self) -> bool {
        self.finding.is_some()
    }

    /// The find field, while finding: how many calls say what is typed,
    /// of how many were read.
    pub fn field(&self) -> Option<crate::find::Field> {
        let find = self.finding.as_ref()?;
        let left = self.shown_rows().len();
        Some(crate::find::Field::rows(&find.query, left, self.rows.len()))
    }

    /// Words pasted while finding, as if typed: whether a find took them.
    pub fn paste(&mut self, text: &str) -> bool {
        let Some(find) = &mut self.finding else {
            return false;
        };
        find.paste(text);
        self.pick = self.shown_rows().first().copied();
        true
    }

    pub fn key(&mut self, key: KeyEvent) -> Key {
        // `ctrl+f`: what is typed narrows the rows, the pick on the first
        // that says it; `esc` drops the find and puts the pick back. The
        // arrows and `enter` stay the list's.
        if let Some(find) = &mut self.finding {
            match find.key(&key) {
                crate::find::Typed::Dropped => {
                    self.pick = (self.finding.take())
                        .and_then(|find| find.before)
                        .or(self.pick)
                        .map(|at| at.min(self.rows.len().saturating_sub(1)))
                        .filter(|_| !self.rows.is_empty());
                    return Key::Stay;
                }
                crate::find::Typed::Changed => {
                    self.pick = self.shown_rows().first().copied();
                    return Key::Stay;
                }
                crate::find::Typed::Held => return Key::Stay,
                crate::find::Typed::Other => {}
            }
        } else if crate::find::opens(&key) {
            self.finding = Some(crate::find::Find::new(self.pick));
            return Key::Stay;
        }
        if key
            .modifiers
            .intersects(KeyModifiers::CONTROL | KeyModifiers::ALT | KeyModifiers::SUPER)
        {
            return match key.code {
                KeyCode::Char('c') if key.modifiers.contains(KeyModifiers::CONTROL) => Key::Close,
                _ => Key::Stay,
            };
        }
        let step = |at: &mut usize, values: usize| *at = (*at + 1) % (values + 1);
        match key.code {
            KeyCode::Esc => return Key::Close,
            KeyCode::Enter => {
                let picked = self.pick.filter(|at| self.shown(*at));
                return match picked.and_then(|at| self.rows.get(at)) {
                    Some(row) => Key::Open(row["bot"].as_str().unwrap_or_default().to_owned()),
                    // A find that keeps nothing picks nothing to open.
                    None if self.finding.is_some() => Key::Stay,
                    None => Key::Close,
                };
            }
            KeyCode::Up | KeyCode::Down | KeyCode::Char('k' | 'j') => {
                // Among the rows shown.
                let shown = self.shown_rows();
                let down = matches!(key.code, KeyCode::Down | KeyCode::Char('j'));
                let at = self
                    .pick
                    .and_then(|pick| shown.iter().position(|at| *at == pick));
                let next = match (at, down) {
                    (Some(at), true) => (at + 1).min(shown.len().saturating_sub(1)),
                    (Some(at), false) => at.saturating_sub(1),
                    (None, _) => 0,
                };
                if let Some(next) = shown.get(next) {
                    self.pick = Some(*next);
                }
                return Key::Stay;
            }
            KeyCode::Char('a') => self.all = !self.all,
            KeyCode::Char('t') => step(&mut self.kind, KINDS.len()),
            KeyCode::Char('p') => step(&mut self.place, PLACES.len()),
            KeyCode::Char('w') => step(&mut self.approved, APPROVERS.len()),
            _ => return Key::Stay,
        }
        self.at = None;
        self.pick = None;
        Key::Stay
    }

    /// The row of the card each row starts on, in their order: one line
    /// each under the heading.
    pub fn starts(&self) -> Vec<usize> {
        match self.error {
            Some(_) => Vec::new(),
            None => (0..self.shown_rows().len()).map(|at| at + 2).collect(),
        }
    }

    /// The row at `at` among those shown picked, as a click picks it.
    pub fn pick_at(&mut self, at: usize) {
        if let Some(row) = self.shown_rows().get(at) {
            self.pick = Some(*row);
        }
    }

    /// The row of the card the picked call is on, to keep it in view.
    pub fn picked_row(&self) -> Option<usize> {
        let pick = self.pick?;
        let at = self.shown_rows().iter().position(|at| *at == pick)?;
        Some(at + 2)
    }

    /// The card at `width`.
    pub fn card(&self, width: usize) -> Vec<Line<'static>> {
        let whose = match self.all {
            true => "every project".to_owned(),
            false => self.scope.project.clone(),
        };
        let filters: Vec<&str> = [
            picked(&KINDS, self.kind),
            picked(&PLACES, self.place),
            picked(&APPROVERS, self.approved).map(|_| "by"),
        ]
        .into_iter()
        .flatten()
        .collect();
        let mut head = vec![Span::raw(whose).bold()];
        let mut said = Vec::new();
        if let Some(kind) = picked(&KINDS, self.kind) {
            said.push(kind.to_owned());
        }
        if let Some(place) = picked(&PLACES, self.place) {
            said.push(format!("ran {place}"));
        }
        if let Some(who) = picked(&APPROVERS, self.approved) {
            said.push(format!("allowed by {who}"));
        }
        if !said.is_empty() {
            head.push(Span::raw(format!(" · {}", said.join(" · "))).dim());
        }
        let mut out = vec![Line::from(head), Line::default()];
        if let Some(error) = &self.error {
            out.push(Line::from(vec![
                Span::raw("✗ ").fg(Color::Red),
                Span::raw(format!("could not read the audit trail: {error}")),
            ]));
            return out;
        }
        if self.at.is_none() && self.rows.is_empty() {
            out.push(Line::from("reading…").dim());
            return out;
        }
        if self.rows.is_empty() {
            out.push(Line::from(match filters.is_empty() {
                true => "No tool calls yet.",
                false => "No tool calls match.",
            }));
            out.push(Line::from("Each tool call an agent makes shows here as it ends.").dim());
            return out;
        }
        let shown = self.shown_rows();
        if let Some(find) = self.finding.as_ref().filter(|_| shown.is_empty()) {
            out.push(Line::from(format!("nothing here matches {}", find.query)).dim());
            return out;
        }
        let names: Vec<String> = (self.rows.iter())
            .map(|row| self.name(row["bot"].as_str().unwrap_or("?")))
            .collect();
        let name_width = names.iter().map(|n| n.width()).max().unwrap_or(0).min(20);
        for at in shown {
            out.push(line(
                &self.rows[at],
                &names[at],
                name_width,
                width,
                self.pick == Some(at),
            ));
        }
        out
    }

    /// Each agent the card's rows name, where its name runs: after the
    /// head and its gap, at the time's end.
    pub fn agents(&self) -> crate::links::Agents {
        if self.error.is_some() {
            return Vec::new();
        }
        let names: Vec<String> = (self.rows.iter())
            .map(|row| self.name(row["bot"].as_str().unwrap_or("?")))
            .collect();
        let name_width = names.iter().map(|n| n.width()).max().unwrap_or(0).min(20);
        (self.shown_rows().into_iter().enumerate())
            .filter_map(|(drawn, at)| {
                let bot = self.rows[at]["bot"].as_str()?;
                let wide = names[at].width().min(name_width) as u16;
                Some((2 + drawn, NAME_AT, NAME_AT + wide, bot.to_owned()))
            })
            .collect()
    }

    /// A bot's name as the card shows it: short in the project, whole
    /// across every agent.
    fn name(&self, bot: &str) -> String {
        match self.all {
            true => bot.to_owned(),
            false => self.scope.short(bot),
        }
    }

    pub fn keys(&self) -> String {
        // While finding, a letter is typed: the filters' letters are not
        // listed, and `esc` drops the find first.
        if self.finding.is_some() {
            return (crate::hint::Keys::new().pick())
                .enter(crate::hint::Enter::Open)
                .esc(crate::hint::Esc::Cancel)
                .text();
        }
        let scope = match self.all {
            true => "a this project",
            false => "a every project",
        };
        let (key, does) = scope.split_once(' ').unwrap_or((scope, ""));
        (crate::hint::Keys::new()
            .pick()
            .enter(crate::hint::Enter::Open))
        .find()
        .key("t", "kind")
        .key("p", "place")
        .key("w", "who")
        .key(key, does)
        .esc(crate::hint::Esc::Close)
        .text()
    }
}

/// A call on one line: its local time, agent, tool, kind and place, who
/// allowed it, how it ended, and the start of its arguments, cut to
/// `width`.
/// The column a row's agent starts at: past ` HH:MM:SS `.
const NAME_AT: u16 = 10;

fn line(row: &Value, name: &str, name_width: usize, width: usize, picked: bool) -> Line<'static> {
    let word = |field: &str| row[field].as_str().unwrap_or("-").to_owned();
    let at = crate::usage::local(row["at_ms"].as_i64().unwrap_or(0) / 1000);
    // How the call ended in this book's words, never the daemon's: a
    // failure is `✗` and words.
    let (outcome, color) = match word("outcome").as_str() {
        "completed" => ("worked".to_owned(), Color::Reset),
        what @ ("failed" | "denied") => (format!("✗ {what}"), Color::Red),
        "unknown" => ("unknown".to_owned(), Color::Yellow),
        what => (what.to_owned(), Color::Reset),
    };
    let tool = match row["step"].as_str() {
        Some(_) => format!("  {}", word("tool")),
        None => word("tool"),
    };
    let mut name = name.to_owned();
    while name.width() > name_width {
        name.pop();
    }
    let lead = format!(
        " {:02}:{:02}:{:02} {name:<name_width$} ",
        at.hour, at.minute, at.second,
    );
    let facts = format!(
        " {}/{} {} ",
        word("kind"),
        word("place"),
        match word("approved").as_str() {
            "nobody" => "·".to_owned(),
            who => who.to_owned(),
        }
    );
    let used = lead.width() + tool.width() + facts.width() + outcome.width() + 1;
    let mut preview: String = word("preview")
        .chars()
        .map(|c| if c.is_control() { ' ' } else { c })
        .collect();
    let room = width.saturating_sub(used + 1);
    while preview.width() > room {
        preview.pop();
    }
    // The pick's column is its own span, so the pick reverses from the
    // row's first word.
    let mut line = Line::from(vec![
        Span::raw(" "),
        Span::raw(lead[1..].to_owned()),
        Span::raw(tool),
        Span::raw(facts).dim(),
        Span::raw(outcome).fg(color),
        Span::raw(format!(" {preview}")).dim(),
    ]);
    if picked {
        crate::list::pick(&mut line);
    }
    line
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scope() -> Scope {
        Scope {
            project: "shop".into(),
            lead: "shop.lead".into(),
            glob: "shop.*".into(),
        }
    }

    fn press(view: &mut View, c: char) -> Key {
        view.key(KeyEvent::from(KeyCode::Char(c)))
    }

    #[test]
    fn the_filters_step_through_the_trails_words_and_back_to_every_row() {
        let mut view = View::new(scope());
        assert_eq!(view.query(), json!({"limit": ROWS, "bot": "shop.*"}));
        press(&mut view, 't');
        press(&mut view, 'p');
        press(&mut view, 'p');
        press(&mut view, 'w');
        assert_eq!(
            view.query(),
            json!({"limit": ROWS, "bot": "shop.*", "kind": "shell", "place": "outside",
                "approved": "nobody"})
        );
        for _ in 0..KINDS.len() {
            press(&mut view, 't');
        }
        press(&mut view, 'a');
        assert_eq!(
            view.query(),
            json!({"limit": ROWS, "place": "outside", "approved": "nobody"})
        );
        assert!(view.due(), "a filter changed reads again");
    }

    #[test]
    fn a_picked_row_opens_its_agent() {
        let mut view = View::new(scope());
        let row = |id: i64, bot: &str| {
            json!({"id": id, "at_ms": 0, "bot": bot, "turn": 1, "call_id": "c", "step": null,
                "tool": "shell", "kind": "shell", "place": "sandbox", "approved": "nobody",
                "access": "unknown", "ms": 4, "outcome": "completed", "preview": "{\"command\":\"ls\"}"})
        };
        view.loaded(Ok(
            json!({"rows": [row(2, "shop.fix"), row(1, "shop.lead")]}),
        ));
        assert_eq!(view.key(KeyEvent::from(KeyCode::Down)), Key::Stay);
        assert_eq!(view.key(KeyEvent::from(KeyCode::Down)), Key::Stay);
        assert_eq!(view.key(KeyEvent::from(KeyCode::Down)), Key::Stay);
        assert_eq!(
            view.key(KeyEvent::from(KeyCode::Enter)),
            Key::Open("shop.lead".into())
        );
        let card = view.card(80);
        let text: Vec<String> = card
            .iter()
            .map(|line| line.spans.iter().map(|s| s.content.as_ref()).collect())
            .collect();
        assert!(text[2].contains(" fix "), "{text:?}");
        // Each row's agent opens it from its name.
        let named: Vec<(String, String)> = (view.agents().into_iter())
            .map(|(row, from, to, bot)| {
                let name: String = (text[row].chars())
                    .skip(usize::from(from))
                    .take(usize::from(to - from))
                    .collect();
                (name, bot)
            })
            .collect();
        assert_eq!(
            named,
            [
                ("fix".to_owned(), "shop.fix".to_owned()),
                ("lead".to_owned(), "shop.lead".to_owned())
            ]
        );
        // The pick is reversed from the row's first word, never a `›`.
        assert!(
            text[3].contains("lead") && !text[3].contains('›'),
            "{text:?}"
        );
        let reversed = |line: &Line| {
            (line.spans.iter())
                .filter(|s| {
                    s.style
                        .add_modifier
                        .contains(ratatui::style::Modifier::REVERSED)
                })
                .map(|s| s.content.as_ref())
                .collect::<String>()
        };
        assert!(reversed(&card[3]).starts_with(char::is_numeric), "{text:?}");
        assert_eq!(reversed(&card[2]), "");
        assert!(text.iter().all(|line| line.width() <= 80), "{text:?}");
    }
}
