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
            out.push((key.clone(), does.clone(), Kind::Rest));
            pairs.next();
        }
        if let Some(enter) = self.enter {
            out.push(("enter".into(), enter.label().into(), Kind::Enter));
        }
        out.extend(pairs.map(|(key, does)| (key.clone(), does.clone(), Kind::Rest)));
        if let Some(esc) = self.esc {
            out.push(("esc".into(), esc.label().into(), Kind::Esc));
        }
        out
    }

    /// The keys as one line: `↑↓ pick · enter open · esc close`.
    pub fn text(&self) -> String {
        join(&self.said())
    }

    /// The keys in at most `room` columns: pairs go from the right, before
    /// the last; `esc` stays, then `enter`, then the rest.
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
    Rest,
    Enter,
    Esc,
}

/// `said` joined in at most `room` columns. Pairs go from the right,
/// before the last, which stays as `esc` does wherever it is named; the
/// rest go first, then `enter`.
fn fitted(mut said: Vec<(String, String, Kind)>, room: usize) -> String {
    while join(&said).width() > room {
        let before = &said[..said.len().saturating_sub(1)];
        let at = (before.iter())
            .rposition(|(_, _, kind)| *kind == Kind::Rest)
            .or_else(|| before.iter().position(|(_, _, kind)| *kind == Kind::Enter));
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
/// [`Keys::fit`] fits them: a pair named `esc` stays, then `enter`, and
/// the others go from the right.
pub fn fit(keys: &str, room: usize) -> String {
    let said: Vec<(String, String, Kind)> = (keys.trim().split(" · "))
        .filter(|pair| !pair.is_empty())
        .map(|pair| {
            let (key, does) = pair.split_once(' ').unwrap_or((pair, ""));
            let kind = match key {
                "esc" => Kind::Esc,
                "enter" => Kind::Enter,
                _ => Kind::Rest,
            };
            (key.to_owned(), does.to_owned(), kind)
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
    fn too_long_the_pairs_go_from_the_right_and_esc_then_enter_stay() {
        let keys = Keys::new()
            .pick()
            .key("t", "kind")
            .key("p", "place")
            .enter(Enter::Open)
            .esc(Esc::Close);
        let all = "↑↓ pick · enter open · t kind · p place · esc close";
        assert_eq!(keys.fit(80), all);
        assert_eq!(keys.fit(41), "↑↓ pick · enter open · t kind · esc close");
        assert_eq!(keys.fit(30), "enter open · esc close");
        assert_eq!(keys.fit(12), "esc close");
        assert_eq!(keys.fit(2), "esc close");
        // With no esc named, the last pair stays all the same.
        let agent = "ctrl+g main view · ctrl+t its tab · ctrl+\\ conversation";
        assert_eq!(fit(agent, 40), "ctrl+g main view · ctrl+\\ conversation");
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
