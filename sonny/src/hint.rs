//! Key hints in one grammar, as Codex's `key_hint` writes them: the key
//! in the default colour and what it does muted after it, a verb with no
//! "to", lower-case key names, `·` between pairs, and esc last.
use ratatui::{style::Stylize, text::Span};

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
