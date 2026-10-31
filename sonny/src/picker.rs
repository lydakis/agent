//! A list picked with the arrow keys, drawn in place under the lines
//! before it, as Codex and Claude Code ask their first-run questions:
//! Enter takes the marked row, typing narrows the rows as every list
//! does ([`crate::fuzzy`]), Esc or Ctrl+C cancels. Once picked, the list folds into one line naming the pick.
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

/// Rows drawn at most at once, fewer in a shorter terminal; the marked
/// row scrolls the rest into view.
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
        typed,
    };
    enable_raw_mode().map_err(|e| e.to_string())?;
    let mut drawn = 0;
    let picked = loop {
        drawn = draw(title, rows, &mut list, Hint::Pick { typed }, drawn);
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

/// Asks `title` over `rows`, each ticked or not as `ticked` says, Space
/// ticking the marked one; the rows ticked when Enter is pressed.
pub fn pick_many(title: &str, rows: &[Row], mut ticked: Vec<bool>) -> Result<Vec<usize>, String> {
    let mut list = List {
        filter: String::new(),
        marked: 0,
        top: 0,
        typed: false,
    };
    let shown = |ticked: &[bool]| -> Vec<Row> {
        (rows.iter().zip(ticked))
            .map(|(row, on)| Row {
                label: format!("{} {}", if *on { "[x]" } else { "[ ]" }, row.label),
                ..row.clone()
            })
            .collect()
    };
    enable_raw_mode().map_err(|e| e.to_string())?;
    let mut drawn = 0;
    let picked = loop {
        drawn = draw(title, &shown(&ticked), &mut list, Hint::Tick, drawn);
        match event::read() {
            Ok(Event::Key(key)) if key.kind != KeyEventKind::Release => match key.code {
                KeyCode::Char('c') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                    break Err("setup cancelled".to_owned());
                }
                KeyCode::Esc => break Ok(Vec::new()),
                KeyCode::Up => list.marked = list.marked.saturating_sub(1),
                KeyCode::Down if list.marked + 1 < rows.len() => list.marked += 1,
                KeyCode::Char(' ') => ticked[list.marked] = !ticked[list.marked],
                KeyCode::Char('a') => {
                    let all = !ticked.iter().all(|on| *on);
                    ticked.iter_mut().for_each(|on| *on = all);
                }
                KeyCode::Enter => {
                    break Ok((0..rows.len()).filter(|&index| ticked[index]).collect());
                }
                _ => {}
            },
            Ok(_) => {}
            Err(error) => break Err(error.to_string()),
        }
    };
    let _ = disable_raw_mode();
    print!("\x1b[{drawn}F\x1b[J");
    if let Ok(picked) = &picked {
        let names: Vec<&str> = picked
            .iter()
            .map(|&index| rows[index].label.as_str())
            .collect();
        let answer = match names.is_empty() {
            true => "none".to_owned(),
            false => names.join(", "),
        };
        println!("{BOLD}{title}{RESET} {ACCENT}{answer}{RESET}");
    }
    let _ = std::io::stdout().flush();
    picked
}

/// The keys a list's last line names.
#[derive(Clone, Copy)]
enum Hint {
    Pick { typed: bool },
    Tick,
}

struct List {
    filter: String,
    /// The marked row's place among those shown.
    marked: usize,
    /// The first of those shown that is drawn.
    top: usize,
    /// What is typed can be the answer, so only rows that hold it as typed
    /// are shown: a new name is not taken for a row that only has its
    /// letters scattered in order.
    typed: bool,
}

impl List {
    /// The rows the filter leaves, best first, by their label or detail.
    fn shown(&self, rows: &[Row]) -> Vec<usize> {
        use crate::fuzzy::{Tier, better, find, order};
        let found = (rows.iter()).map(|row| {
            better(
                find(&row.label, &self.filter),
                find(&row.detail, &self.filter),
            )
        });
        (order(found, |_| 0.0).into_iter())
            .filter(|(_, found)| !self.typed || found.tier != Tier::Letters || found.together())
            .map(|(index, _)| index)
            .collect()
    }

    fn type_in(&mut self, text: &str) {
        // A pasted line's own newline is not text; a typed space is.
        self.filter.push_str(text.trim_matches(['\r', '\n']));
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
fn draw(title: &str, rows: &[Row], list: &mut List, hint: Hint, drawn: usize) -> usize {
    let shown = list.shown(rows);
    // Lines that wrapped or scrolled off the top would throw off the
    // count redrawing goes back by, so the list fits the terminal: every
    // line cut to its width, and rows enough for its height with the
    // title, the hint and the line after them.
    let (columns, height) =
        ratatui::crossterm::terminal::size().map_or((80, 24), |(c, r)| (c as usize, r as usize));
    let fit = SHOWN.min(height.saturating_sub(3)).max(1);
    if list.marked < list.top {
        list.top = list.marked;
    } else if list.marked >= list.top + fit {
        list.top = list.marked + 1 - fit;
    }
    let mut out = String::new();
    if drawn > 0 {
        out.push_str(&format!("\x1b[{drawn}F"));
    }
    out.push_str("\x1b[J");
    let asked = clip(
        &format!("{title} {}", list.filter),
        columns.saturating_sub(1),
    );
    let at = title.len().min(asked.len());
    let at = if asked.is_char_boundary(at) {
        at
    } else {
        asked.len()
    };
    let (title, filter) = asked.split_at(at);
    let mut lines = vec![format!("{BOLD}{title}{RESET}{filter}")];
    let width = shown
        .iter()
        .map(|&index| rows[index].label.chars().count())
        .max()
        .unwrap_or(0)
        .min(columns.saturating_sub(3));
    for (place, &index) in shown.iter().enumerate().skip(list.top).take(fit) {
        let row = &rows[index];
        let label = clip(&row.label, width);
        let pad = " ".repeat(width - label.chars().count());
        // What fits after the marker and the label, the last column
        // left free: the badge, then as much of the detail as is room.
        let mut room = columns.saturating_sub(3 + width);
        let badge = match row.badge.as_deref() {
            Some(badge) if badge.chars().count() + 2 <= room => {
                room -= badge.chars().count() + 2;
                format!("  {ACCENT}{badge}{RESET}")
            }
            _ => String::new(),
        };
        let detail = match room > 2 && !row.detail.is_empty() {
            true => format!("  {DIM}{}{RESET}", clip(&row.detail, room - 2)),
            false => String::new(),
        };
        lines.push(match place == list.marked {
            true => format!("{ACCENT}›{RESET} {BOLD}{label}{RESET}{pad}{detail}{badge}"),
            false => format!("  {label}{pad}{detail}{badge}"),
        });
    }
    let hidden = shown.len().saturating_sub(list.top + fit);
    let more = match hidden {
        0 => String::new(),
        n => format!("{n} more · "),
    };
    let hint = match (hint, shown.is_empty()) {
        (Hint::Tick, _) => format!("{more}↑↓ move · space tick · a all · enter done · esc none"),
        (Hint::Pick { typed: true }, true) => {
            "nothing listed matches · enter take what you typed".to_owned()
        }
        (Hint::Pick { .. }, true) => "nothing matches · ⌫ widen".to_owned(),
        (Hint::Pick { .. }, false) => format!("{more}↑↓ move · enter choose · type to narrow"),
    };
    lines.push(format!(
        "{DIM}{}{RESET}",
        clip(&hint, columns.saturating_sub(1))
    ));
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
        false if room == 0 => String::new(),
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
    fn clipped_text_fits_its_room() {
        assert_eq!(clip("bedrock/global.anthropic.claude", 10), "bedrock/g…");
        assert_eq!(clip("short", 10), "short");
        assert_eq!(clip("anything", 0), "");
    }

    #[test]
    fn arrows_move_typing_narrows_and_enter_picks() {
        let rows = rows();
        let none = KeyModifiers::NONE;
        let mut list = List {
            filter: String::new(),
            marked: 0,
            top: 0,
            typed: false,
        };
        assert_eq!(list.key(KeyCode::Down, none, &rows, false), None);
        assert_eq!(list.key(KeyCode::Down, none, &rows, false), None);
        // The last row stays marked past the end.
        assert_eq!(list.key(KeyCode::Down, none, &rows, false), None);
        assert_eq!(
            list.key(KeyCode::Enter, none, &rows, false),
            Some(Ok(Picked::Row(2)))
        );
        for c in "claude haiku".chars() {
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
