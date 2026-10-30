//! How Sonny's colors meet the terminal's theme. The sixteen colors carry
//! meaning and the theme decides how they look; what Sonny works out for
//! itself starts from the theme's own colors, as the terminal answered
//! them ([`crate::term::theme`]). Muted text, drawn dim, becomes a gray
//! between the theme's foreground and background that still reads, as
//! Codex draws its secondary text: dim's strength is each terminal's own,
//! and in some themes dim text reads at under 2:1. With `NO_COLOR` set
//! (no-color.org) no color is drawn at all, and bold, dim, underline and
//! reverse carry what is left; every colored mark has its own shape.
use std::sync::OnceLock;

use ratatui::{
    buffer::{Buffer, Cell},
    style::{Color, Modifier},
};

use crate::term::Rgb;

/// `NO_COLOR` is set and not empty.
pub fn no_color() -> bool {
    static NO_COLOR: OnceLock<bool> = OnceLock::new();
    *NO_COLOR.get_or_init(|| std::env::var_os("NO_COLOR").is_some_and(|value| !value.is_empty()))
}

/// The blocks that fill part of a cell from its bottom or its left.
const PARTS: [&str; 14] = [
    "▁", "▂", "▃", "▄", "▅", "▆", "▇", "▏", "▎", "▍", "▌", "▋", "▊", "▉",
];

/// What a frame's cells become before they are written.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Look {
    /// As drawn: the theme's colors are not known.
    Drawn,
    /// Dim text in the default color drawn in this gray.
    Muted(Rgb),
    /// No color.
    Plain,
}

impl Look {
    pub fn now() -> Look {
        if no_color() {
            return Look::Plain;
        }
        let theme = crate::term::theme();
        match (theme.foreground, theme.background) {
            (Some(foreground), Some(background)) => Look::Muted(muted(foreground, background)),
            _ => Look::Drawn,
        }
    }

    pub fn finish(self, buffer: &mut Buffer) {
        if self != Look::Drawn {
            buffer.content.iter_mut().for_each(|cell| self.cell(cell));
        }
    }

    pub fn cell(self, cell: &mut Cell) {
        match self {
            Look::Drawn => {}
            Look::Muted((r, g, b)) => {
                if cell.modifier.contains(Modifier::DIM) && cell.fg == Color::Reset {
                    cell.fg = Color::Rgb(r, g, b);
                    cell.modifier.remove(Modifier::DIM);
                }
            }
            // A picture's placeholder cells name it by their color, a
            // linked one behind its link's escape.
            Look::Plain if cell.symbol().contains(crate::image::PLACEHOLDER) => {}
            Look::Plain => {
                // A part block on a colored background (a stacked bar's
                // step) fills its cell: without the background it would
                // read as a gap.
                if cell.bg != Color::Reset && PARTS.contains(&cell.symbol()) {
                    cell.set_symbol("█");
                }
                cell.fg = Color::Reset;
                cell.bg = Color::Reset;
            }
        }
    }
}

/// The gray muted text takes: the foreground taken toward the background
/// no further than keeps 4.5:1 (WCAG's for text), or, in a theme whose own
/// text reads at under 6:1, three quarters of that, so muted still differs
/// from the rest.
fn muted(foreground: Rgb, background: Rgb) -> Rgb {
    let floor = (contrast(foreground, background) * 0.75).min(4.5);
    let mut weight = 0.55;
    loop {
        let gray = blend(foreground, background, weight);
        if weight >= 1.0 || contrast(gray, background) >= floor {
            return gray;
        }
        weight += 0.05;
    }
}

/// `weight` of `a` over `b`.
pub fn blend(a: Rgb, b: Rgb, weight: f32) -> Rgb {
    let mix = |a: u8, b: u8| (f32::from(a) * weight + f32::from(b) * (1.0 - weight)).round() as u8;
    (mix(a.0, b.0), mix(a.1, b.1), mix(a.2, b.2))
}

/// WCAG's contrast ratio between two colors, 1 to 21.
pub fn contrast(a: Rgb, b: Rgb) -> f32 {
    let luminance = |(r, g, b): Rgb| {
        let linear = |c: u8| {
            let c = f32::from(c) / 255.0;
            if c <= 0.039_28 {
                c / 12.92
            } else {
                ((c + 0.055) / 1.055).powf(2.4)
            }
        };
        0.2126 * linear(r) + 0.7152 * linear(g) + 0.0722 * linear(b)
    };
    let (a, b) = (luminance(a), luminance(b));
    (a.max(b) + 0.05) / (a.min(b) + 0.05)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn muted_text_reads_and_still_differs_from_the_rest() {
        // Catppuccin Mocha, and Solarized Dark, whose own text is 4.7:1.
        for (foreground, background) in [
            ((0xcd, 0xd6, 0xf4), (0x1e, 0x1e, 0x2e)),
            ((0x83, 0x94, 0x96), (0x00, 0x2b, 0x36)),
            ((0x00, 0x00, 0x00), (0xff, 0xff, 0xff)),
        ] {
            let gray = muted(foreground, background);
            let (text, muted) = (contrast(foreground, background), contrast(gray, background));
            assert!(
                muted >= (text * 0.75).min(4.5),
                "{gray:?} on {background:?}"
            );
            assert!(muted < text, "{gray:?} on {background:?}");
        }
    }

    #[test]
    fn dim_default_text_becomes_the_gray_and_colored_text_stays() {
        let mut buffer = Buffer::empty(ratatui::layout::Rect::new(0, 0, 3, 1));
        buffer[(0, 0)].set_symbol("a").modifier = Modifier::DIM;
        buffer[(1, 0)].set_symbol("b").set_fg(Color::Green).modifier = Modifier::DIM;
        buffer[(2, 0)].set_symbol("c").set_bg(Color::Yellow);
        Look::Muted((1, 2, 3)).finish(&mut buffer);
        assert_eq!(buffer[(0, 0)].fg, Color::Rgb(1, 2, 3));
        assert!(!buffer[(0, 0)].modifier.contains(Modifier::DIM));
        assert_eq!(buffer[(1, 0)].fg, Color::Green);
        assert!(buffer[(1, 0)].modifier.contains(Modifier::DIM));

        Look::Plain.finish(&mut buffer);
        assert_eq!(buffer[(1, 0)].fg, Color::Reset);
        assert_eq!(buffer[(2, 0)].symbol(), "c");
        assert!(buffer[(1, 0)].modifier.contains(Modifier::DIM));
        assert_eq!(buffer[(2, 0)].bg, Color::Reset);
    }

    #[test]
    fn without_color_a_stacked_step_fills_its_cell_and_a_picture_keeps_its_id() {
        let mut buffer = Buffer::empty(ratatui::layout::Rect::new(0, 0, 3, 1));
        buffer[(0, 0)]
            .set_symbol("▃")
            .set_fg(Color::Magenta)
            .set_bg(Color::Cyan);
        buffer[(1, 0)].set_symbol("▃").set_fg(Color::Magenta);
        let linked = format!("\x1b]8;;file:///a.png\x1b\\{}", crate::image::PLACEHOLDER);
        buffer[(2, 0)]
            .set_symbol(&linked)
            .set_fg(Color::Rgb(0, 0, 7));
        Look::Plain.finish(&mut buffer);
        assert_eq!(buffer[(0, 0)].symbol(), "█");
        assert_eq!(buffer[(1, 0)].symbol(), "▃");
        assert_eq!(buffer[(2, 0)].fg, Color::Rgb(0, 0, 7));
    }
}
