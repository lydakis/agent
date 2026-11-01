//! One list over the composer for every pick, as Codex draws all its
//! popups with one component: at most `ROWS` rows scrolled to keep the pick
//! in view, a blank row between it and the conversation, the pick in
//! reverse video, `•` before what is in effect now, and the letters typed
//! to narrow it in bold, as Codex marks them.
use ratatui::{
    style::Stylize,
    text::{Line, Span},
};
use unicode_width::UnicodeWidthStr;

/// Rows shown at most, as Codex's `MAX_POPUP_ROWS`.
pub const ROWS: usize = 8;

/// One row: its name, what it is (muted), whether it is in effect, and
/// where in its name the letters typed are.
pub struct Item {
    pub name: String,
    pub about: String,
    pub current: bool,
    pub found: Vec<usize>,
}

impl Item {
    pub fn new(name: impl Into<String>, about: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            about: about.into(),
            current: false,
            found: Vec::new(),
        }
    }

    /// The chars of its name that matched what was typed.
    pub fn found(mut self, found: &[usize]) -> Self {
        self.found = found.to_vec();
        self
    }

    pub fn current(mut self, current: bool) -> Self {
        self.current = current;
        self
    }
}

/// `items` with `pick` picked, `width` wide: the blank row, then up to
/// `ROWS` rows around the pick, fewer when only `room` rows are free above
/// the composer, so the pick is always among those drawn.
pub fn lines(items: &[Item], pick: usize, width: usize, room: usize) -> Vec<Line<'static>> {
    let shown = items.len().min(ROWS).min(room.saturating_sub(1));
    let first = (pick + 1)
        .saturating_sub(shown)
        .min(items.len().saturating_sub(shown));
    let wide = items
        .iter()
        .map(|item| item.name.width())
        .max()
        .unwrap_or(0);
    let mut out = vec![Line::default()];
    for (at, item) in items.iter().enumerate().skip(first).take(shown) {
        let mark = match item.current {
            true => "  • ",
            false => "    ",
        };
        let name = format!("{:<wide$}", item.name);
        let room = width.saturating_sub(mark.width() + wide + 4);
        let about = match item.about.is_empty() {
            true => String::new(),
            false => format!("  {}", crate::transcript::cut(&item.about, room)),
        };
        let picked = at == pick;
        let mut row = vec![Span::raw(mark)];
        row.extend(marked(&name, &item.found, picked));
        row.push(match picked {
            true => Span::raw(format!("{about} ")).reversed(),
            false => Span::raw(about).dim(),
        });
        out.push(Line::from(row));
    }
    out
}

/// `name` in runs, the chars at `found` bold, all reversed when picked.
fn marked(name: &str, found: &[usize], picked: bool) -> Vec<Span<'static>> {
    let mut runs: Vec<(String, bool)> = Vec::new();
    for (at, c) in name.chars().enumerate() {
        let bold = found.contains(&at);
        match runs.last_mut() {
            Some((run, was)) if *was == bold => run.push(c),
            _ => runs.push((c.to_string(), bold)),
        }
    }
    (runs.into_iter())
        .map(|(run, bold)| {
            let span = Span::raw(run);
            let span = if bold { span.bold() } else { span };
            if picked { span.reversed() } else { span }
        })
        .collect()
}

/// What a key does to a list open over the composer.
#[derive(Debug, PartialEq)]
pub enum Act {
    /// It moved the pick or changed what is typed; the list stays open.
    Stay,
    /// Enter took this row, by its place among the rows given.
    Take(usize),
    /// Esc or Ctrl+C closed it.
    Close,
    /// A key the list has no use for, such as Left on `/model`'s.
    Other,
}

/// The narrowing and pick every list over the composer shares, as Codex's
/// popups share one selection state: rows found by typing as
/// [`crate::fuzzy`] finds them, by their name or what they are, the pick
/// kept in view, Enter taking it, Esc closing. A list keeps only its own
/// rows and what a pick means.
pub struct Picker {
    /// Each row's name and what it is, as typing matches them.
    rows: Vec<(String, String)>,
    /// Typed to narrow the rows.
    pub filter: String,
    /// The pick, among the rows shown.
    pick: usize,
    /// The row picked with nothing typed.
    first: usize,
    /// What the person picked before, which ranks a row first among
    /// rows matched alike; none for a list whose order is its meaning.
    picks: Option<crate::fuzzy::Picks>,
}

impl Picker {
    /// A list of `rows`, each its name and what it is, `pick` picked.
    pub fn new(rows: Vec<(String, String)>, pick: usize) -> Self {
        let pick = pick.min(rows.len().saturating_sub(1));
        Self {
            rows,
            filter: String::new(),
            pick,
            first: pick,
            picks: None,
        }
    }

    /// Ranked by what the person picked before, within a match's tier.
    pub fn weighed(mut self, picks: crate::fuzzy::Picks) -> Self {
        self.picks = Some(picks);
        self
    }

    /// The rows typing leaves, best first, each its place among the rows
    /// given and the chars of its name typed; a row found by what it is
    /// marks none. With nothing typed, every row in the list's order.
    pub fn shown(&self) -> Vec<(usize, Vec<usize>)> {
        use crate::fuzzy::{better, find, order};
        let found = (self.rows.iter()).map(|(name, about)| {
            let named = find(name, &self.filter);
            let best = better(named.clone(), find(about, &self.filter));
            best.map(|mut best| {
                if named.as_ref() != Some(&best) {
                    best.at.clear();
                }
                best
            })
        });
        let typed = !self.filter.is_empty();
        let weight = |at: usize| match (&self.picks, typed) {
            (Some(picks), true) => picks.weight_now(&self.rows[at].0),
            _ => 0.0,
        };
        (order(found, weight).into_iter())
            .map(|(at, found)| (at, found.at))
            .collect()
    }

    /// Where the pick is among `shown()`.
    pub fn at(&self) -> usize {
        self.pick.min(self.shown().len().saturating_sub(1))
    }

    /// The row picked, by its place among the rows given, when typing
    /// leaves any.
    pub fn chosen(&self) -> Option<usize> {
        self.shown().get(self.at()).map(|(at, _)| *at)
    }

    pub fn up(&mut self) {
        let len = self.shown().len().max(1);
        self.pick = (self.at() + len - 1) % len;
    }

    pub fn down(&mut self) {
        let len = self.shown().len().max(1);
        self.pick = (self.at() + 1) % len;
    }

    /// Narrowed to `filter`, the best row picked; with nothing typed, the
    /// row picked when the list opened.
    pub fn narrow(&mut self, filter: String) {
        self.filter = filter;
        self.pick = match self.filter.is_empty() {
            true => self.first,
            false => 0,
        };
    }

    /// Picks the row at `at` among those shown.
    pub fn pick_at(&mut self, at: usize) {
        self.pick = at;
    }

    /// Notes that the row picked was taken, so it ranks first next time.
    pub fn took(&mut self) {
        if let (Some(at), Some(_)) = (self.chosen(), &self.picks) {
            let name = self.rows[at].0.clone();
            if let Some(picks) = &mut self.picks {
                picks.pick_now(&name);
            }
            // Ranked higher now, the row taken stays the one picked.
            if let Some(now) = self.shown().iter().position(|(row, _)| *row == at) {
                self.pick = now;
            }
        }
    }

    /// What `event` does: arrows move the pick, typing and pasting
    /// narrow, Backspace takes a letter back, Enter takes the pick, Esc
    /// and Ctrl+C close.
    pub fn event(&mut self, event: &ratatui::crossterm::event::Event) -> Act {
        use ratatui::crossterm::event::{Event, KeyCode, KeyEventKind, KeyModifiers};
        match event {
            Event::Key(key) if key.kind != KeyEventKind::Release => match key.code {
                KeyCode::Char('c') if key.modifiers.contains(KeyModifiers::CONTROL) => Act::Close,
                KeyCode::Esc => Act::Close,
                KeyCode::Up => {
                    self.up();
                    Act::Stay
                }
                KeyCode::Down => {
                    self.down();
                    Act::Stay
                }
                KeyCode::Enter => match self.chosen() {
                    Some(at) => Act::Take(at),
                    None => Act::Stay,
                },
                KeyCode::Backspace => {
                    let mut filter = self.filter.clone();
                    filter.pop();
                    self.narrow(filter);
                    Act::Stay
                }
                KeyCode::Char(c)
                    if !key
                        .modifiers
                        .intersects(KeyModifiers::CONTROL | KeyModifiers::ALT) =>
                {
                    self.narrow(format!("{}{c}", self.filter));
                    Act::Stay
                }
                _ => Act::Other,
            },
            Event::Paste(text) => {
                // A pasted line's own newline is not text.
                let text = text.trim_matches(['\r', '\n']);
                self.narrow(format!("{}{text}", self.filter));
                Act::Stay
            }
            _ => Act::Other,
        }
    }

    /// The composer's line while the list is open: what is typed, else
    /// `asked` muted; and where the cursor goes in it.
    pub fn line(&self, asked: &str) -> (Line<'static>, u16) {
        let line = match self.filter.is_empty() {
            true => Line::from(vec![
                Span::raw("› ").bold(),
                Span::raw(asked.to_owned()).dim(),
            ]),
            false => Line::from(vec![Span::raw("› ").bold(), Span::raw(self.filter.clone())]),
        };
        (line, 2 + self.filter.width() as u16)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_long_list_shows_eight_rows_around_the_pick_under_a_blank_one() {
        let items: Vec<Item> = (0..21)
            .map(|n| Item::new(format!("/c{n}"), "does").current(n == 3))
            .collect();
        let text = |lines: Vec<Line>| -> Vec<String> {
            lines.iter().map(|line| line.to_string()).collect()
        };
        let top = text(lines(&items, 3, 40, 20));
        assert_eq!(top.len(), 9);
        assert_eq!(top[0], "");
        assert_eq!(top[1], "    /c0   does");
        assert_eq!(top[4], "  • /c3   does ");
        let end = text(lines(&items, 20, 40, 20));
        assert_eq!(end[8], "    /c20  does ");
        assert_eq!(end[1], "    /c13  does");
        // With four rows free above the composer, three items show, the
        // pick among them.
        let short = text(lines(&items, 20, 40, 4));
        assert_eq!(short.len(), 4);
        assert_eq!(short[3], "    /c20  does ");
    }

    #[test]
    fn the_letters_typed_are_bold_and_the_rest_plain() {
        use ratatui::style::Modifier;
        let items = [
            Item::new("/commons", "read").found(&[1, 2, 3]),
            Item::new("/compact", "go"),
        ];
        let rows = lines(&items, 1, 40, 20);
        let bold: Vec<(String, bool)> = (rows[1].spans.iter())
            .map(|s| {
                (
                    s.content.to_string(),
                    s.style.add_modifier.contains(Modifier::BOLD),
                )
            })
            .collect();
        assert_eq!(bold[1], ("/".into(), false));
        assert_eq!(bold[2], ("com".into(), true));
        assert_eq!(bold[3], ("mons".into(), false));
        assert_eq!(rows[1].to_string(), "    /commons  read");
        assert_eq!(rows[2].to_string(), "    /compact  go ");
    }

    fn picker(names: &[&str]) -> Picker {
        Picker::new(
            names
                .iter()
                .map(|n| (n.to_string(), format!("{n} about")))
                .collect(),
            names.len() - 1,
        )
    }

    fn key(code: ratatui::crossterm::event::KeyCode) -> ratatui::crossterm::event::Event {
        use ratatui::crossterm::event::{Event, KeyEvent, KeyModifiers};
        Event::Key(KeyEvent::new(code, KeyModifiers::NONE))
    }

    #[test]
    fn every_list_narrows_as_typed_and_takes_its_pick() {
        use ratatui::crossterm::event::KeyCode;
        let mut list = picker(&["standard", "fast", "flex"]);
        assert_eq!(list.chosen(), Some(2), "opens on the row it was given");
        assert_eq!(list.event(&key(KeyCode::Char('f'))), Act::Stay);
        let shown: Vec<usize> = list.shown().into_iter().map(|(at, _)| at).collect();
        assert_eq!(shown, [1, 2]);
        assert_eq!(list.chosen(), Some(1), "typing picks the best row");
        list.event(&key(KeyCode::Char('l')));
        assert_eq!(list.shown(), [(2, vec![0, 1])]);
        assert_eq!(list.event(&key(KeyCode::Enter)), Act::Take(2));
        // Backspace back to nothing picks the row the list opened on.
        list.event(&key(KeyCode::Backspace));
        list.event(&key(KeyCode::Backspace));
        assert_eq!(list.chosen(), Some(2));
        list.event(&key(KeyCode::Down));
        assert_eq!(list.chosen(), Some(0), "the pick wraps");
        list.event(&key(KeyCode::Up));
        assert_eq!(list.chosen(), Some(2));
        // Nothing left: Enter takes nothing and the list stays.
        list.event(&key(KeyCode::Char('z')));
        assert_eq!(list.chosen(), None);
        assert_eq!(list.event(&key(KeyCode::Enter)), Act::Stay);
        assert_eq!(list.event(&key(KeyCode::Esc)), Act::Close);
        assert_eq!(list.event(&key(KeyCode::Left)), Act::Other);
    }

    #[test]
    fn the_row_taken_stays_picked_as_its_pick_ranks_it_higher() {
        use ratatui::crossterm::event::KeyCode;
        let rows = vec![
            ("shop.a".into(), String::new()),
            ("shop.b".into(), String::new()),
        ];
        let mut list = Picker::new(rows, 0).weighed(crate::fuzzy::Picks::default());
        list.narrow("shop".into());
        list.event(&key(KeyCode::Down));
        assert_eq!(list.chosen(), Some(1));
        assert_eq!(list.event(&key(KeyCode::Enter)), Act::Take(1));
        list.took();
        assert_eq!(list.shown()[0].0, 1, "ranked first now");
        assert_eq!(list.chosen(), Some(1));
    }

    #[test]
    fn a_row_is_found_by_what_it_is_with_no_letters_of_its_name_marked() {
        let mut list = Picker::new(
            vec![
                ("ask".into(), "every tool call asks first".into()),
                ("full".into(), "no sandbox".into()),
            ],
            0,
        );
        list.narrow("sandbox".into());
        assert_eq!(list.shown(), [(1, vec![])]);
        let (line, cursor) = list.line("type to narrow");
        assert_eq!(line.to_string(), "› sandbox");
        assert_eq!(cursor, 9);
        list.narrow(String::new());
        assert_eq!(
            list.line("type to narrow").0.to_string(),
            "› type to narrow"
        );
    }
}
