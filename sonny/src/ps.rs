//! `/ps`: the commands the project's agents left running, or every
//! agent's, by agent and oldest first, from the daemon's `processes`: each
//! one's agent, what kind it is (a background command, a terminal, a
//! detached service, or a call that waits on its approval), how long it
//! has run, a detached one's pid, and the start of its command. ↑ and ↓
//! pick a row, Enter opens its agent, and x twice stops it with all it
//! started, as the daemon's `stop_process` does, as Codex's `/ps` and
//! `/stop` do for its background terminals. One daemon read every two
//! seconds while it is open, through the running rows' index only.
use crate::background::Scope;
use ratatui::crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use ratatui::{
    style::{Color, Stylize},
    text::{Line, Span},
};
use serde_json::{Value, json};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use unicode_width::UnicodeWidthStr;

/// How often the open view reads again.
const FRESH: Duration = Duration::from_secs(2);

/// What a key did to the view.
#[derive(Debug, PartialEq)]
pub enum Key {
    Stay,
    Close,
    /// Open this agent, the picked row's.
    Open(String),
    /// Stop this agent's command, by its handle.
    Stop {
        bot: String,
        handle: String,
    },
}

/// `/ps`, open.
pub struct View {
    scope: Scope,
    /// Every agent's commands, not only the project's.
    all: bool,
    rows: Vec<Value>,
    error: Option<String>,
    /// When it was read last; none until it is, or once it must be again.
    at: Option<Instant>,
    /// The row ↑ and ↓ picked, by its place in the list.
    pub pick: Option<usize>,
    /// The handle a first x asked to stop, waiting for the second.
    stopping: Option<String>,
    /// What the last stop asked, or came to.
    pub said: Option<String>,
    /// What a stop running beside the window came to, once it has.
    came: Arc<Mutex<Option<String>>>,
}

impl View {
    pub fn new(scope: Scope) -> Self {
        Self {
            scope,
            all: false,
            rows: Vec::new(),
            error: None,
            at: None,
            pick: None,
            stopping: None,
            said: None,
            came: Arc::default(),
        }
    }

    /// Whether it should be read again now: also once a stop came to
    /// something, which it then says.
    pub fn due(&self) -> bool {
        self.at.is_none_or(|at| at.elapsed() >= FRESH)
            || self.came.lock().is_ok_and(|came| came.is_some())
    }

    /// The `processes` request for what it shows.
    pub fn query(&self) -> Value {
        match self.all {
            true => json!({}),
            false => json!({"bot": self.scope.glob}),
        }
    }

    /// What the read came back with.
    pub fn loaded(&mut self, page: Result<Value, String>) {
        self.at = Some(Instant::now());
        if let Some(came) = self.came.lock().ok().and_then(|mut came| came.take()) {
            self.said = Some(came);
        }
        match page {
            Ok(page) => {
                self.error = None;
                self.rows = page["processes"].as_array().cloned().unwrap_or_default();
                self.pick = self
                    .pick
                    .filter(|_| !self.rows.is_empty())
                    .map(|at| at.min(self.rows.len() - 1));
                // A stop asked of a row that has gone since is no stop.
                if let Some(handle) = &self.stopping
                    && !self.rows.iter().any(|row| row["handle"] == *handle)
                {
                    self.stopping = None;
                }
            }
            Err(error) => self.error = Some(error),
        }
    }

    /// A stop of `handle` going out: it says so until it comes to
    /// something, which goes in what this returns.
    pub fn stopping(&mut self, handle: &str) -> Arc<Mutex<Option<String>>> {
        self.said = Some(format!("stopping {handle}…"));
        self.came.clone()
    }

    fn picked(&self) -> Option<&Value> {
        self.pick.and_then(|at| self.rows.get(at))
    }

    pub fn key(&mut self, key: KeyEvent) -> Key {
        if key
            .modifiers
            .intersects(KeyModifiers::CONTROL | KeyModifiers::ALT | KeyModifiers::SUPER)
        {
            return match key.code {
                KeyCode::Char('c') if key.modifiers.contains(KeyModifiers::CONTROL) => Key::Close,
                _ => Key::Stay,
            };
        }
        let word = |row: &Value, field: &str| row[field].as_str().unwrap_or_default().to_owned();
        // Any key but a second x lets a stop asked for go.
        let stopping = self.stopping.take();
        match key.code {
            KeyCode::Esc => Key::Close,
            KeyCode::Enter => match self.picked() {
                Some(row) => Key::Open(word(row, "bot")),
                None => Key::Close,
            },
            KeyCode::Up | KeyCode::Char('k') => {
                self.pick = Some(self.pick.map_or(0, |at| at.saturating_sub(1)));
                Key::Stay
            }
            KeyCode::Down | KeyCode::Char('j') => {
                let last = self.rows.len().saturating_sub(1);
                self.pick = Some(self.pick.map_or(0, |at| (at + 1).min(last)));
                Key::Stay
            }
            KeyCode::Char('x') => {
                let Some(row) = self.picked() else {
                    self.said = Some("pick a command with ↑↓ first".into());
                    return Key::Stay;
                };
                let (bot, handle) = (word(row, "bot"), word(row, "handle"));
                if stopping.as_deref() == Some(handle.as_str()) {
                    self.said = None;
                    return Key::Stop { bot, handle };
                }
                self.said = Some(format!("x again stops {handle} and all it started"));
                self.stopping = Some(handle);
                Key::Stay
            }
            KeyCode::Char('a') => {
                self.all = !self.all;
                self.at = None;
                self.pick = None;
                Key::Stay
            }
            _ => Key::Stay,
        }
    }

    /// The row of the card each row starts on, in their order: one line
    /// each under the heading.
    pub fn starts(&self) -> Vec<usize> {
        match self.error {
            Some(_) => Vec::new(),
            None => (0..self.rows.len()).map(|at| at + 2).collect(),
        }
    }

    /// The row at `at` picked, as a click picks it.
    pub fn pick_at(&mut self, at: usize) {
        if at < self.rows.len() {
            self.pick = Some(at);
        }
    }

    /// The row of the card the picked command is on, to keep it in view.
    pub fn picked_row(&self) -> Option<usize> {
        self.pick.map(|at| at + 2)
    }

    /// The card at `width`, as of `now_ms`.
    pub fn card(&self, width: usize, now_ms: i64) -> Vec<Line<'static>> {
        let whose = match self.all {
            true => "every project".to_owned(),
            false => self.scope.project.clone(),
        };
        let mut head = vec![Span::raw(whose).bold()];
        if !self.rows.is_empty() {
            head.push(Span::raw(format!(" · {} running", self.rows.len())).dim());
        }
        let mut out = vec![Line::from(head), Line::default()];
        if let Some(error) = &self.error {
            out.push(Line::from(vec![
                Span::raw("✗ ").fg(Color::Red),
                Span::raw(format!("could not read what runs: {error}")),
            ]));
            return out;
        }
        if self.at.is_none() && self.rows.is_empty() {
            out.push(Line::from("reading…").dim());
            return out;
        }
        if self.rows.is_empty() {
            out.push(Line::from("Nothing the agents started is running."));
            out.push(
                Line::from("A command an agent leaves running shows here until it ends.").dim(),
            );
        }
        let names: Vec<String> = (self.rows.iter())
            .map(|row| self.name(row["bot"].as_str().unwrap_or("?")))
            .collect();
        let name_width = names.iter().map(|n| n.width()).max().unwrap_or(0).min(20);
        for (at, (row, name)) in self.rows.iter().zip(&names).enumerate() {
            let picked = self.pick == Some(at);
            out.push(line(row, name, name_width, width, now_ms, picked));
        }
        if let Some(said) = &self.said {
            out.push(Line::default());
            out.push(Line::from(said.clone()).fg(Color::Yellow));
        }
        out
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
        let scope = match self.all {
            true => "a this project",
            false => "a every project",
        };
        use crate::hint::{Enter, Esc, Keys};
        let (key, does) = scope.split_once(' ').unwrap_or((scope, ""));
        (Keys::new().pick().enter(Enter::Open))
            .key("x", "stop")
            .key(key, does)
            .esc(Esc::Close)
            .text()
    }
}

/// How long something has run, in its largest unit.
fn age(ms: i64) -> String {
    let s = ms.max(0) / 1000;
    match s {
        0..60 => format!("{s}s"),
        60..3600 => format!("{}m", s / 60),
        3600..86400 => format!("{}h", s / 3600),
        _ => format!("{}d", s / 86400),
    }
}

/// A command on one line: how long it has run, its agent and kind, a
/// detached one's pid, and the start of its command, cut to `width`.
fn line(
    row: &Value,
    name: &str,
    name_width: usize,
    width: usize,
    now_ms: i64,
    picked: bool,
) -> Line<'static> {
    let ran = match row["started_ms"].as_i64() {
        Some(started) => age(now_ms - started),
        None => "?".into(),
    };
    let kind = match row["kind"].as_str().unwrap_or("call") {
        "background" => "bg",
        "terminal" => "tty",
        "detached" => "detached",
        _ => "waiting",
    };
    let mut name = name.to_owned();
    while name.width() > name_width {
        name.pop();
    }
    // The pad before the age is its own span, so the pick reverses from
    // the row's first word.
    let pad = " ".repeat(1 + 4usize.saturating_sub(ran.width()));
    let lead = format!("{ran} {name:<name_width$} ");
    let facts = match row["pid"].as_i64() {
        Some(pid) => format!("{kind:<8} pid {pid} "),
        None => format!("{kind:<8} "),
    };
    let mut command: String = row["command"]
        .as_str()
        .unwrap_or("-")
        .chars()
        .map(|c| if c.is_control() { ' ' } else { c })
        .collect();
    let room = width.saturating_sub(pad.width() + lead.width() + facts.width() + 1);
    while command.width() > room {
        command.pop();
    }
    let mut line = Line::from(vec![
        Span::raw(pad),
        Span::raw(lead),
        Span::raw(facts).dim(),
        Span::raw(command),
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

    fn press(view: &mut View, code: KeyCode) -> Key {
        view.key(KeyEvent::from(code))
    }

    fn text(card: &[Line<'static>]) -> Vec<String> {
        card.iter()
            .map(|line| line.spans.iter().map(|s| s.content.as_ref()).collect())
            .collect()
    }

    #[test]
    fn it_lists_the_projects_commands_and_stops_one_on_a_second_x() {
        let mut view = View::new(scope());
        assert_eq!(view.query(), json!({"bot": "shop.*"}));
        assert!(view.due());
        view.loaded(Ok(json!({"processes": [
            {"handle": "proc:4", "bot": "shop.lead", "turn": 2, "call_id": "c1",
             "kind": "background", "command": "cargo build", "started_ms": 1_000, "pid": null},
            {"handle": "proc:9", "bot": "shop.fix", "turn": 5, "call_id": "c2",
             "kind": "detached", "command": "npm run dev\nmore", "started_ms": 1_000,
             "pid": 4242},
        ]})));
        assert!(!view.due());
        let card = text(&view.card(80, 1_000 + 125_000));
        assert!(card[0].contains("2 running"), "{card:?}");
        assert!(card[2].contains(" 2m lead") && card[2].contains("cargo build"));
        assert!(card[3].contains("fix") && card[3].contains("detached pid 4242"));
        assert!(card[3].ends_with("npm run dev more"), "{card:?}");
        assert!(card.iter().all(|line| line.width() <= 80), "{card:?}");
        // Nothing picked, nothing stopped.
        assert_eq!(press(&mut view, KeyCode::Char('x')), Key::Stay);
        press(&mut view, KeyCode::Down);
        press(&mut view, KeyCode::Down);
        // A first x asks; another key lets it go; two in a row stop.
        assert_eq!(press(&mut view, KeyCode::Char('x')), Key::Stay);
        assert!(view.said.as_deref().unwrap().contains("proc:9"));
        press(&mut view, KeyCode::Down);
        assert_eq!(press(&mut view, KeyCode::Char('x')), Key::Stay);
        assert_eq!(
            press(&mut view, KeyCode::Char('x')),
            Key::Stop {
                bot: "shop.fix".into(),
                handle: "proc:9".into()
            }
        );
        let came = view.stopping("proc:9");
        assert!(view.said.as_deref().unwrap().starts_with("stopping proc:9"));
        view.loaded(Ok(json!({"processes": []})));
        assert!(!view.due());
        *came.lock().unwrap() = Some("stopped proc:9".into());
        assert!(
            view.due(),
            "a stop that came to something reads again at once"
        );
        view.loaded(Ok(json!({"processes": [
            {"handle": "proc:9", "bot": "shop.fix", "kind": "detached", "started_ms": 0}]})));
        assert_eq!(view.said.as_deref(), Some("stopped proc:9"));
        press(&mut view, KeyCode::Down);
        assert_eq!(
            press(&mut view, KeyCode::Enter),
            Key::Open("shop.fix".into())
        );
        press(&mut view, KeyCode::Char('a'));
        assert_eq!(view.query(), json!({}));
    }

    #[test]
    fn an_empty_list_says_so() {
        let mut view = View::new(scope());
        assert_eq!(text(&view.card(60, 0))[2], "reading…");
        view.loaded(Ok(json!({"processes": []})));
        assert_eq!(
            text(&view.card(60, 0))[2],
            "Nothing the agents started is running."
        );
        assert_eq!(age(59_000), "59s");
        assert_eq!(age(7_200_000), "2h");
        assert_eq!(age(200_000_000), "2d");
    }
}
