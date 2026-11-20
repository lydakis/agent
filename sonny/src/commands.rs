//! `/background`'s Commands section: the commands the project's agents
//! left running, from the daemon's `processes`, oldest first. Each one's
//! age, agent, kind (a background command, a terminal, a detached service,
//! or a call that waits on its approval), a detached one's pid, and the
//! start of its command. x stops one with all it started, as the daemon's
//! `stop_process` does, as Codex's `/ps` and `/stop` do for its background
//! terminals, once Enter confirms it. One daemon read every two seconds
//! while the view is open, through the running rows' index only.
use ratatui::{
    style::Stylize,
    text::{Line, Span},
};
use serde_json::Value;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use unicode_width::UnicodeWidthStr;

/// How often the open view reads them again.
const FRESH: Duration = Duration::from_secs(2);

/// The commands, as last read.
#[derive(Default)]
pub struct Commands {
    pub rows: Vec<Value>,
    pub error: Option<String>,
    /// When they were read last; none until they are, or once they must
    /// be again.
    at: Option<Instant>,
    /// What a stop running beside the window came to, once it has.
    came: Arc<Mutex<Option<String>>>,
}

impl Commands {
    /// Whether they should be read again now: also once a stop came to
    /// something.
    pub fn due(&self) -> bool {
        self.at.is_none_or(|at| at.elapsed() >= FRESH)
            || self.came.lock().is_ok_and(|came| came.is_some())
    }

    /// Whether they have been read at all.
    pub fn read(&self) -> bool {
        self.at.is_some()
    }

    /// What the read came back with, and what a stop came to since the
    /// last one, if one did.
    pub fn loaded(&mut self, page: Result<Value, String>) -> Option<String> {
        self.at = Some(Instant::now());
        match page {
            Ok(page) => {
                self.error = None;
                self.rows = page["processes"].as_array().cloned().unwrap_or_default();
            }
            Err(error) => {
                self.rows.clear();
                self.error = Some(error);
            }
        }
        self.came.lock().ok().and_then(|mut came| came.take())
    }

    /// Where a stop going out says what it came to.
    pub fn came(&self) -> Arc<Mutex<Option<String>>> {
        self.came.clone()
    }
}

/// What a row names in the stop it asks for: the start of its command,
/// or its handle.
pub fn what(row: &Value) -> String {
    crate::transcript::cut(&said(row), 32)
}

/// The whole command on one line, its controls as spaces; with none, its
/// handle.
pub fn said(row: &Value) -> String {
    let command: String = (row["command"].as_str().unwrap_or_default().chars())
        .map(|c| if c.is_control() { ' ' } else { c })
        .collect();
    match command.trim() {
        "" => row["handle"].as_str().unwrap_or_default().to_owned(),
        command => command.to_owned(),
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
/// The column [`line`] starts an agent's name at: after the age and
/// its pad.
pub fn name_column(row: &Value, now_ms: i64) -> u16 {
    let ran = match row["started_ms"].as_i64() {
        Some(started) => age(now_ms - started),
        None => "?".into(),
    };
    (1 + 4usize.saturating_sub(ran.width()) + ran.width() + 1) as u16
}

pub fn line(
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
    use serde_json::json;

    #[test]
    fn a_command_reads_as_its_age_agent_kind_and_start() {
        let row = json!({"handle": "proc:9", "bot": "shop.fix", "kind": "detached",
            "command": "npm run dev\nmore", "started_ms": 1_000, "pid": 4242});
        let drawn = line(&row, "fix", 4, 80, 1_000 + 125_000, false).to_string();
        assert!(drawn.contains(" 2m fix"), "{drawn}");
        assert!(drawn.contains("detached pid 4242"), "{drawn}");
        assert!(drawn.ends_with("npm run dev more"), "{drawn}");
        assert_eq!(line(&row, "fix", 4, 30, 0, false).width(), 29);
        assert_eq!(what(&row), "npm run dev more");
        assert_eq!(what(&json!({"handle": "proc:4"})), "proc:4");
        assert_eq!(age(59_000), "59s");
        assert_eq!(age(7_200_000), "2h");
        assert_eq!(age(200_000_000), "2d");
    }
}
