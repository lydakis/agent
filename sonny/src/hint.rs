//! Key hints in one grammar, as Codex's `key_hint` writes them: the key
//! in the default color and what it does muted after it, a verb with no
//! "to", lower-case key names, `·` between pairs, and esc last.
//!
//! Each view declares its keys once, as [`Keys`]: the pairs it names, then
//! what `enter` and `esc` do in the rulebook's few words ([`Enter`],
//! [`Esc`]), so no view says `esc stop` where another says `esc close`.
use ratatui::{style::Stylize, text::Span};
use unicode_width::UnicodeWidthStr;

/// What `esc` does where a view names it.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Esc {
    /// Leaves a view or a list.
    Close,
    /// Goes one step back where steps remain: `/new`, a thread, a replay.
    Back,
    /// Drops something pending: a confirm, a value being typed, a pick.
    Cancel,
    /// In the composer, leaves a question or an approval for your own words.
    WriteInstead,
}

impl Esc {
    fn label(self) -> &'static str {
        match self {
            Esc::Close => "close",
            Esc::Back => "back",
            Esc::Cancel => "cancel",
            Esc::WriteInstead => "write instead",
        }
    }
}

/// What `enter` does where a view names it.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Enter {
    /// Goes to a thing: an agent, a thread, a file.
    Open,
    /// Runs a command or an automation.
    Run,
    /// Takes a value.
    Choose,
    /// Goes on to the next step.
    Next,
    /// Keeps what was typed.
    Save,
    /// A confirm's own verb: `enter compact`, `enter remove`.
    Confirm(&'static str),
}

impl Enter {
    fn label(self) -> &'static str {
        match self {
            Enter::Open => "open",
            Enter::Run => "run",
            Enter::Choose => "choose",
            Enter::Next => "next",
            Enter::Save => "save",
            Enter::Confirm(verb) => verb,
        }
    }
}

/// A view's keys, declared once: the pairs it names in order, then
/// `enter`'s and `esc`'s, `esc` always last.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Keys {
    pairs: Vec<(String, String)>,
    enter: Option<Enter>,
    esc: Option<Esc>,
}

impl Keys {
    pub fn new() -> Self {
        Self::default()
    }

    /// `↑↓ pick`, which every list names first.
    pub fn pick(self) -> Self {
        self.key("↑↓", "pick")
    }

    /// `ctrl+f find`, the same in every view of rows or text: declared
    /// here once, so the title row and the `?` sheet both say it.
    pub fn find(self) -> Self {
        self.key(crate::find::KEY, "find")
    }

    /// Another key and what it does.
    pub fn key(mut self, key: impl Into<String>, does: impl Into<String>) -> Self {
        self.pairs.push((key.into(), does.into()));
        self
    }

    /// What `enter` does, said after the pick.
    pub fn enter(mut self, enter: Enter) -> Self {
        self.enter = Some(enter);
        self
    }

    pub fn esc(mut self, esc: Esc) -> Self {
        self.esc = Some(esc);
        self
    }

    /// The pairs in the order they read: the pick, `enter`, the rest,
    /// `esc`. Each is its key and what it does.
    fn said(&self) -> Vec<(String, String, Kind)> {
        let mut out = Vec::new();
        let mut pairs = self.pairs.iter();
        let first = self.pairs.first().filter(|(key, _)| key == "↑↓");
        if let Some((key, does)) = first {
            out.push((key.clone(), does.clone(), Kind::Pick));
            pairs.next();
        }
        if let Some(enter) = self.enter {
            out.push(("enter".into(), enter.label().into(), Kind::Enter));
        }
        out.extend(pairs.map(|(key, does)| (key.clone(), does.clone(), kind_of(key, does))));
        if let Some(esc) = self.esc {
            out.push(("esc".into(), esc.label().into(), Kind::Esc));
        }
        out
    }

    /// The keys as one line: `↑↓ pick · enter open · esc close`.
    pub fn text(&self) -> String {
        join(&self.said())
    }

    /// The keys in at most `room` columns: `↑↓ pick` goes first, then the
    /// way back, then the view's letters from the right, then `enter`;
    /// `esc` stays, as the last pair does.
    pub fn fit(&self, room: usize) -> String {
        fitted(self.said(), room)
    }
}

impl std::fmt::Display for Keys {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.text())
    }
}

#[derive(Clone, Copy, Debug, PartialEq)]
enum Kind {
    /// `↑↓`, the same in every view.
    Pick,
    /// `ctrl+f find`, the same in every view.
    Find,
    /// The way back to the conversation: `ctrl+\`, `tab`.
    Back,
    /// A view's own letters.
    Rest,
    Enter,
    Esc,
}

/// What a pair, `key` and what it `does`, is as a line of keys gives way.
/// `tab` is the way back only where it moves the keys: `tab keys`, not
/// `tab complete` or `tab speed`.
fn kind_of(key: &str, does: &str) -> Kind {
    match (key, does) {
        ("esc", _) => Kind::Esc,
        ("enter", _) => Kind::Enter,
        ("↑↓", _) => Kind::Pick,
        ("tab", "keys") => Kind::Back,
        (crate::find::KEY, "find") => Kind::Find,
        (key, _) if key.starts_with("ctrl+\\") => Kind::Back,
        _ => Kind::Rest,
    }
}

/// `said` joined in at most `room` columns. The last pair stays, as `esc`
/// does wherever it is named. Before it, `↑↓ pick` goes first, since the
/// arrows are the same in every view; then `ctrl+f find`, the same in
/// every view too; then the way back; then the view's
/// letters, from the right, since a letter is the one key a person cannot
/// guess; then `enter`.
fn fitted(mut said: Vec<(String, String, Kind)>, room: usize) -> String {
    while join(&said).width() > room {
        let before = &said[..said.len().saturating_sub(1)];
        let last_of = |wanted: Kind| before.iter().rposition(|(_, _, kind)| *kind == wanted);
        let at = last_of(Kind::Pick)
            .or_else(|| last_of(Kind::Find))
            .or_else(|| last_of(Kind::Back))
            .or_else(|| last_of(Kind::Rest))
            .or_else(|| last_of(Kind::Enter));
        match at {
            Some(at) => said.remove(at),
            None => break,
        };
    }
    join(&said)
}

fn join(said: &[(String, String, Kind)]) -> String {
    let pairs: Vec<String> = (said.iter())
        .map(|(key, does, _)| match does.is_empty() {
            true => key.clone(),
            false => format!("{key} {does}"),
        })
        .collect();
    pairs.join(" · ")
}

/// A line of keys written as text, `keys`, in at most `room` columns, as
/// [`Keys::fit`] fits them.
pub fn fit(keys: &str, room: usize) -> String {
    let said: Vec<(String, String, Kind)> = (keys.trim().split(" · "))
        .filter(|pair| !pair.is_empty())
        .map(|pair| {
            let (key, does) = pair.split_once(' ').unwrap_or((pair, ""));
            (key.to_owned(), does.to_owned(), kind_of(key, does))
        })
        .collect();
    fitted(said, room)
}

/// The line under a list over the composer, `width` wide: its keys two
/// columns in, fitted as [`Keys::fit`] fits them.
pub fn under(keys: &Keys, width: usize) -> Vec<Span<'static>> {
    spans(&format!("  {}", keys.fit(width.saturating_sub(2))))
}

/// `keys`, pairs of a key and what it does with ` · ` between them, as
/// spans: each pair's first word is its key. Spaces around it stay.
pub fn spans(keys: &str) -> Vec<Span<'static>> {
    let mut spans = Vec::new();
    let body = keys.trim();
    let before = keys.len() - keys.trim_start().len();
    if before > 0 {
        spans.push(Span::raw(" ".repeat(before)));
    }
    for (at, pair) in body.split(" · ").enumerate() {
        if at > 0 {
            spans.push(Span::raw(" · ").dim());
        }
        match pair.split_once(' ') {
            Some((key, does)) => {
                spans.push(Span::raw(key.to_owned()));
                spans.push(Span::raw(format!(" {does}")).dim());
            }
            None => spans.push(Span::raw(pair.to_owned())),
        }
    }
    let after = keys.len() - keys.trim_end().len();
    if after > 0 && !body.is_empty() {
        spans.push(Span::raw(" ".repeat(after)));
    }
    spans
}

#[cfg(test)]
mod tests {
    use super::*;
    use ratatui::style::Modifier;

    #[test]
    fn a_view_names_its_keys_once_esc_last() {
        let keys = Keys::new()
            .pick()
            .key("x", "stop")
            .enter(Enter::Open)
            .esc(Esc::Close);
        assert_eq!(keys.text(), "↑↓ pick · enter open · x stop · esc close");
        let confirm = Keys::new()
            .enter(Enter::Confirm("compact"))
            .esc(Esc::Cancel);
        assert_eq!(confirm.to_string(), "enter compact · esc cancel");
    }

    #[test]
    fn too_long_the_pick_goes_first_then_letters_from_the_right_and_esc_then_enter_stay() {
        let keys = Keys::new()
            .pick()
            .key("t", "kind")
            .key("p", "place")
            .enter(Enter::Open)
            .esc(Esc::Close);
        let all = "↑↓ pick · enter open · t kind · p place · esc close";
        assert_eq!(keys.fit(80), all);
        assert_eq!(keys.fit(50), "enter open · t kind · p place · esc close");
        assert_eq!(keys.fit(40), "enter open · t kind · esc close");
        assert_eq!(keys.fit(30), "enter open · esc close");
        assert_eq!(keys.fit(12), "esc close");
        assert_eq!(keys.fit(2), "esc close");
        // The way back goes after the pick, before the view's letters.
        let back = "↑↓ pick · r run · ctrl+\\ conversation · esc close";
        assert_eq!(fit(back, 40), "r run · ctrl+\\ conversation · esc close");
        assert_eq!(fit(back, 30), "r run · esc close");
        // `tab` is the way back only where it moves the keys; where it
        // moves between values, as in `/model`, it is one of the view's.
        let tabbed = "↑↓ pick · enter open · r run · tab keys · esc close";
        assert_eq!(fit(tabbed, 32), "enter open · r run · esc close");
        let values = Keys::new()
            .pick()
            .enter(Enter::Choose)
            .key("←→", "effort")
            .key("tab", "speed")
            .esc(Esc::Close);
        assert_eq!(values.fit(40), "enter choose · ←→ effort · esc close");
        // With no esc named, the last pair stays all the same.
        let agent = "ctrl+g main view · ctrl+t its tab · ctrl+\\ conversation";
        assert_eq!(fit(agent, 40), "ctrl+g main view · ctrl+\\ conversation");
        // `ctrl+f find` gives way after the pick and before the way back
        // and the view's letters, being the same in every view.
        let found = Keys::new()
            .pick()
            .enter(Enter::Open)
            .find()
            .key("r", "run")
            .esc(Esc::Close);
        let whole = "↑↓ pick · enter open · ctrl+f find · r run · esc close";
        assert_eq!(found.fit(80), whole);
        assert_eq!(
            found.fit(45),
            "enter open · ctrl+f find · r run · esc close"
        );
        assert_eq!(found.fit(35), "enter open · r run · esc close");
        assert_eq!(fit(whole, 35), "enter open · r run · esc close");
        // A line written out fits the same way.
        assert_eq!(fit(all, 30), "enter open · esc close");
        assert_eq!(
            fit("  ↑↓ pick · enter run · esc close", 21),
            "enter run · esc close"
        );
    }

    #[test]
    fn a_key_is_plain_and_what_it_does_muted() {
        let spans = spans("↑↓ pick · esc close");
        let text: String = spans.iter().map(|span| span.content.as_ref()).collect();
        assert_eq!(text, "↑↓ pick · esc close");
        let dim: Vec<bool> = spans
            .iter()
            .map(|span| span.style.add_modifier.contains(Modifier::DIM))
            .collect();
        assert_eq!(dim, [false, true, true, false, true]);
        let padded: String = super::spans("  esc close ")
            .iter()
            .map(|span| span.content.as_ref())
            .collect();
        assert_eq!(padded, "  esc close ");
    }
}
