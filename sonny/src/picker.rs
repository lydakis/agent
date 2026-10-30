//! A list picked with the arrow keys, drawn in place under the lines
//! before it, as Codex and Claude Code ask their first-run questions:
//! Enter takes the marked row, typing narrows the rows, Esc or Ctrl+C
//! cancels. Once picked, the list folds into one line naming the pick.
use ratatui::crossterm::{
    event::{self, Event, KeyCode, KeyEventKind, KeyModifiers},
    terminal::{disable_raw_mode, enable_raw_mode},
};
use std::io::Write;

#[derive(Clone)]
pub struct Row {
    pub label: String,
    /// Dimmed after the label.
    pub detail: String,
    /// A word in the accent colour after the detail, as `found`.
    pub badge: Option<String>,
}

impl Row {
    pub fn new(label: impl Into<String>, detail: impl Into<String>) -> Self {
        Self {
            label: label.into(),
            detail: detail.into(),
            badge: None,
        }
    }

    pub fn badge(mut self, badge: impl Into<String>) -> Self {
        self.badge = Some(badge.into());
        self
    }
}

/// What the person picked: a row's index, or with `typed` allowed, what
/// they typed when it narrowed the rows to none.
#[derive(Debug, PartialEq)]
pub enum Picked {
    Row(usize),
    Typed(String),
}

/// Rows drawn at once; the marked row scrolls the rest into view.
const SHOWN: usize = 10;

const ACCENT: &str = "\x1b[38;2;255;190;60m";
const BOLD: &str = "\x1b[1m";
const DIM: &str = "\x1b[2m";
const RESET: &str = "\x1b[0m";

/// Asks `title` over `rows` with `marked` first marked. With `typed`,
/// text that narrows the rows to none is itself the answer.
pub fn pick(title: &str, rows: &[Row], marked: usize, typed: bool) -> Result<Picked, String> {
    let mut list = List {
        filter: String::new(),
        marked,
        top: 0,
    };
    enable_raw_mode().map_err(|e| e.to_string())?;
    let mut drawn = 0;
    let picked = loop {
        drawn = draw(title, rows, &mut list, typed, drawn);
        match event::read() {
            Ok(Event::Key(key)) if key.kind != KeyEventKind::Release => {
                if let Some(done) = list.key(key.code, key.modifiers, rows, typed) {
                    break done;
                }
            }
            Ok(Event::Paste(text)) => list.type_in(&text),
            Ok(_) => {}
            Err(error) => break Err(error.to_string()),
        }
    };
    let _ = disable_raw_mode();
    // The list folds into the question and its answer.
    print!("\x1b[{drawn}F\x1b[J");
    if let Ok(picked) = &picked {
        let answer = match picked {
            Picked::Row(index) => rows[*index].label.as_str(),
            Picked::Typed(text) => text.as_str(),
        };
        println!("{BOLD}{title}{RESET} {ACCENT}{answer}{RESET}");
    }
    let _ = std::io::stdout().flush();
    picked
}

struct List {
    filter: String,
    /// The marked row's place among those shown.
    marked: usize,
    /// The first of those shown that is drawn.
    top: usize,
}

impl List {
    fn shown(&self, rows: &[Row]) -> Vec<usize> {
        let filter = self.filter.to_lowercase();
        (0..rows.len())
            .filter(|&index| {
                let row = &rows[index];
                row.label.to_lowercase().contains(&filter)
                    || row.detail.to_lowercase().contains(&filter)
            })
            .collect()
    }

    fn type_in(&mut self, text: &str) {
        self.filter.push_str(text.trim());
        self.marked = 0;
        self.top = 0;
    }

    /// The pick a key makes, or none while the list stays open.
    fn key(
        &mut self,
        code: KeyCode,
        modifiers: KeyModifiers,
        rows: &[Row],
        typed: bool,
    ) -> Option<Result<Picked, String>> {
        let shown = self.shown(rows);
        match code {
            KeyCode::Char('c') if modifiers.contains(KeyModifiers::CONTROL) => {
                return Some(Err("setup cancelled".into()));
            }
            KeyCode::Esc if self.filter.is_empty() => return Some(Err("setup cancelled".into())),
            KeyCode::Esc => self.type_over(String::new()),
            KeyCode::Up => self.marked = self.marked.saturating_sub(1),
            KeyCode::Down if self.marked + 1 < shown.len() => self.marked += 1,
            KeyCode::Enter => {
                return match shown.get(self.marked) {
                    Some(&index) => Some(Ok(Picked::Row(index))),
                    None if typed && !self.filter.is_empty() => {
                        Some(Ok(Picked::Typed(self.filter.clone())))
                    }
                    None => None,
                };
            }
            KeyCode::Backspace => {
                let mut filter = self.filter.clone();
                filter.pop();
                self.type_over(filter);
            }
            KeyCode::Char(c) => self.type_in(&c.to_string()),
            _ => {}
        }
        None
    }

    fn type_over(&mut self, filter: String) {
        self.filter = filter;
        self.marked = 0;
        self.top = 0;
    }
}

/// Draws the list over the `drawn` lines it drew before; how many it drew.
fn draw(title: &str, rows: &[Row], list: &mut List, typed: bool, drawn: usize) -> usize {
    let shown = list.shown(rows);
    if list.marked < list.top {
        list.top = list.marked;
    } else if list.marked >= list.top + SHOWN {
        list.top = list.marked + 1 - SHOWN;
    }
    let mut out = String::new();
    if drawn > 0 {
        out.push_str(&format!("\x1b[{drawn}F"));
    }
    out.push_str("\x1b[J");
    let mut lines = vec![format!("{BOLD}{title}{RESET} {}", list.filter)];
    let width = shown
        .iter()
        .map(|&index| rows[index].label.chars().count())
        .max()
        .unwrap_or(0);
    // A line that wrapped would throw off the count redrawing goes back by.
    let columns = ratatui::crossterm::terminal::size().map_or(80, |(c, _)| c as usize);
    for (place, &index) in shown.iter().enumerate().skip(list.top).take(SHOWN) {
        let row = &rows[index];
        let pad = " ".repeat(width - row.label.chars().count());
        let badge = row.badge.as_deref().unwrap_or("");
        let room = columns.saturating_sub(5 + width + badge.chars().count() + 2);
        let detail = clip(&row.detail, room);
        let badge = match badge.is_empty() {
            true => String::new(),
            false => format!("  {ACCENT}{badge}{RESET}"),
        };
        lines.push(match place == list.marked {
            true => format!(
                "{ACCENT}›{RESET} {BOLD}{}{RESET}{pad}  {DIM}{detail}{RESET}{badge}",
                row.label
            ),
            false => format!("  {}{pad}  {DIM}{detail}{RESET}{badge}", row.label),
        });
    }
    let hidden = shown.len().saturating_sub(list.top + SHOWN);
    let hint = match (shown.is_empty(), typed) {
        (true, true) => "nothing listed matches; Enter takes what you typed".to_owned(),
        (true, false) => "nothing matches; Backspace to widen".to_owned(),
        _ if hidden > 0 => format!("{hidden} more · ↑↓ move · Enter choose · type to narrow"),
        _ => "↑↓ move · Enter choose · type to narrow".to_owned(),
    };
    lines.push(format!("{DIM}{hint}{RESET}"));
    out.push_str(&lines.join("\r\n"));
    out.push_str("\r\n");
    print!("{out}");
    let _ = std::io::stdout().flush();
    lines.len()
}

/// `text` in at most `room` characters, cut with an ellipsis.
fn clip(text: &str, room: usize) -> String {
    match text.chars().count() <= room {
        true => text.to_owned(),
        false => {
            let kept: String = text.chars().take(room.saturating_sub(1)).collect();
            format!("{kept}…")
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rows() -> Vec<Row> {
        vec![
            Row::new("anthropic/claude-a", "Claude Opus"),
            Row::new("anthropic/claude-b", "Claude Haiku"),
            Row::new("openai/gpt-x", ""),
        ]
    }

    #[test]
    fn arrows_move_typing_narrows_and_enter_picks() {
        let rows = rows();
        let none = KeyModifiers::NONE;
        let mut list = List {
            filter: String::new(),
            marked: 0,
            top: 0,
        };
        assert_eq!(list.key(KeyCode::Down, none, &rows, false), None);
        assert_eq!(list.key(KeyCode::Down, none, &rows, false), None);
        // The last row stays marked past the end.
        assert_eq!(list.key(KeyCode::Down, none, &rows, false), None);
        assert_eq!(
            list.key(KeyCode::Enter, none, &rows, false),
            Some(Ok(Picked::Row(2)))
        );
        for c in "haiku".chars() {
            list.key(KeyCode::Char(c), none, &rows, false);
        }
        assert_eq!(list.shown(&rows), vec![1]);
        assert_eq!(
            list.key(KeyCode::Enter, none, &rows, false),
            Some(Ok(Picked::Row(1)))
        );
        // Text that matches nothing is the answer only where typing is.
        list.type_over("bedrock/unlisted".into());
        assert_eq!(list.key(KeyCode::Enter, none, &rows, false), None);
        assert_eq!(
            list.key(KeyCode::Enter, none, &rows, true),
            Some(Ok(Picked::Typed("bedrock/unlisted".into())))
        );
        // Esc clears the text first, then cancels.
        assert_eq!(list.key(KeyCode::Esc, none, &rows, true), None);
        assert!(list.filter.is_empty());
        assert!(matches!(
            list.key(KeyCode::Esc, none, &rows, true),
            Some(Err(_))
        ));
    }
}
