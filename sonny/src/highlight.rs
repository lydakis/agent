//! Code in syntax colors, as bat draws it: bat's syntaxes, read with
//! syntect, in bat's `ansi` theme, whose colors are the terminal's own
//! sixteen, so code reads on a light background as on a dark one.
use std::sync::OnceLock;

use ratatui::{
    style::{Color, Modifier, Style},
    text::Span,
};
use syntect::{
    highlighting::{FontStyle, HighlightState, Highlighter, RangedHighlightIterator, Theme},
    parsing::{ParseState, ScopeStack, SyntaxReference, SyntaxSet},
};

fn syntaxes() -> &'static SyntaxSet {
    static SET: OnceLock<SyntaxSet> = OnceLock::new();
    SET.get_or_init(two_face::syntax::extra_newlines)
}

fn theme() -> &'static Theme {
    static THEME: OnceLock<Theme> = OnceLock::new();
    THEME.get_or_init(|| {
        (two_face::theme::extra())
            .get(two_face::theme::EmbeddedThemeName::Ansi)
            .clone()
    })
}

/// The syntaxes and the theme read in, as the first code colored would.
pub fn preload() {
    syntaxes();
    theme();
}

/// Code being colored a line at a time: where its syntax is after the
/// lines so far, as a string or comment goes on over lines.
#[derive(Clone)]
pub struct Code {
    parse: ParseState,
    highlight: HighlightState,
}

impl Code {
    /// Code in a language as a fence names it (`rust`, `py`, `ts`), if
    /// that is one known.
    pub fn language(name: &str) -> Option<Self> {
        let name = name.split([' ', ',', '{']).next()?.trim();
        if name.is_empty() {
            return None;
        }
        let set = syntaxes();
        let syntax = (set.find_syntax_by_token(name))
            .or_else(|| set.find_syntax_by_extension(&name.to_lowercase()))?;
        Some(Self::new(syntax))
    }

    /// The code of a file, by its name, else by its first line
    /// (`#!/bin/sh`), if either says a syntax known.
    pub fn file(path: &std::path::Path, first: &str) -> Option<Self> {
        let set = syntaxes();
        let name = path.file_name()?.to_str()?;
        let extension = path.extension().and_then(|e| e.to_str());
        let syntax = (set.find_syntax_by_extension(name))
            .or_else(|| extension.and_then(|e| set.find_syntax_by_extension(e)))
            .or_else(|| set.find_syntax_by_first_line(first))?;
        // Plain text is no syntax to color.
        (syntax.name != "Plain Text").then(|| Self::new(syntax))
    }

    fn new(syntax: &SyntaxReference) -> Self {
        let highlighter = Highlighter::new(theme());
        Self {
            parse: ParseState::new(syntax),
            highlight: HighlightState::new(&highlighter, ScopeStack::new()),
        }
    }

    /// One line in its colors. A line the syntax cannot read is shown
    /// plain, and the lines after it go on from there.
    pub fn line(&mut self, text: &str) -> Vec<Span<'static>> {
        let plain = || vec![Span::raw(text.to_owned())];
        // The syntaxes read a line with its line break.
        let line = format!("{text}\n");
        let Ok(ops) = self.parse.parse_line(&line, syntaxes()) else {
            return plain();
        };
        let highlighter = Highlighter::new(theme());
        let ranges = RangedHighlightIterator::new(&mut self.highlight, &ops, &line, &highlighter);
        let mut spans: Vec<Span<'static>> = Vec::new();
        for (style, piece, _) in ranges {
            let piece = piece.trim_end_matches('\n');
            if piece.is_empty() {
                continue;
            }
            let mut look = Style::new();
            if let Some(color) = color(style.foreground) {
                look = look.fg(color);
            }
            if style.font_style.contains(FontStyle::BOLD) {
                look = look.add_modifier(Modifier::BOLD);
            }
            if style.font_style.contains(FontStyle::ITALIC) {
                look = look.add_modifier(Modifier::ITALIC);
            }
            if style.font_style.contains(FontStyle::UNDERLINE) {
                look = look.add_modifier(Modifier::UNDERLINED);
            }
            match spans.last_mut() {
                Some(last) if last.style == look => last.content.to_mut().push_str(piece),
                _ => spans.push(Span::styled(piece.to_owned(), look)),
            }
        }
        spans
    }
}

/// A color of bat's `ansi` theme as the terminal's: one of its sixteen
/// when the alpha is 0 (the red being which), its own text color when 1.
fn color(color: syntect::highlighting::Color) -> Option<Color> {
    match color.a {
        0 => Some(match color.r {
            0 => Color::Black,
            1 => Color::Red,
            2 => Color::Green,
            3 => Color::Yellow,
            4 => Color::Blue,
            5 => Color::Magenta,
            6 => Color::Cyan,
            7 => Color::Gray,
            n => Color::Indexed(n),
        }),
        1 => None,
        _ => Some(Color::Rgb(color.r, color.g, color.b)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn colors(spans: &[Span]) -> Vec<(String, Option<Color>)> {
        spans
            .iter()
            .map(|s| (s.content.to_string(), s.style.fg))
            .collect()
    }

    #[test]
    fn rust_reads_in_the_terminals_colors() {
        let mut code = Code::language("rust").expect("rust is known");
        let spans = code.line("fn main() { let s = \"hi\"; }");
        let text: String = spans.iter().map(|s| s.content.as_ref()).collect();
        assert_eq!(text, "fn main() { let s = \"hi\"; }");
        let colors = colors(&spans);
        assert!(colors.iter().any(|(t, c)| t.contains("fn") && c.is_some()));
        assert!(colors.iter().any(|(t, c)| t.contains("hi") && c.is_some()));
        // Every color is one of the terminal's, never a fixed one.
        assert!(
            colors
                .iter()
                .all(|(_, c)| !matches!(c, Some(Color::Rgb(..))))
        );
    }

    #[test]
    fn a_comment_goes_on_over_lines() {
        let mut code = Code::language("js").expect("js is known");
        code.line("/* a comment");
        let inside = code.line("still the comment");
        let after = code.line("*/ let x = 1;");
        assert_eq!(inside.len(), 1);
        assert_eq!(inside[0].style.fg, after[0].style.fg);
    }

    #[test]
    fn files_are_known_by_name_extension_or_first_line() {
        let path = std::path::Path::new;
        assert!(Code::file(path("a/Cargo.toml"), "[package]").is_some());
        assert!(Code::file(path("Makefile"), "all:").is_some());
        assert!(Code::file(path("app.tsx"), "").is_some());
        assert!(Code::file(path("run"), "#!/bin/sh").is_some());
        assert!(Code::file(path("notes.txt"), "hello").is_none());
        assert!(Code::language("ts").is_some());
        assert!(Code::language("nothing-known").is_none());
    }
}
