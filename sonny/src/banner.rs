//! The welcome at the top of every lead's transcript, as Claude Code heads
//! its own: Sonny, the robot the client is named for, as a portrait drawn
//! with half blocks, two pixels a cell, above the conversation's first item.
use ratatui::{
    style::{Color, Style, Stylize},
    text::{Line, Span},
};

/// The welcome for the lead in `folder`, on `model`, at `width` columns:
/// the portrait beside its words, or, too narrow for the picture, the
/// words alone.
pub fn welcome(folder: &str, model: &str, width: usize) -> Vec<Line<'static>> {
    let version = env!("CARGO_PKG_VERSION");
    let accent = Style::new().fg(Color::Rgb(255, 190, 60));
    let art = pixels(&PORTRAIT, &PORTRAIT_COLORS);
    let words = vec![
        Line::from(vec![
            "Welcome to Sonny".bold(),
            format!("  {version}").dim(),
        ]),
        Line::default(),
        Line::from("This folder's lead. Ask for anything:".dim()),
        Line::from("it plans, starts tasks in their own copies,".dim()),
        Line::from("and tells you when they need you.".dim()),
        Line::default(),
        Line::from(format!("{folder} · {model}").dim()),
        Line::default(),
        hints(accent),
    ];
    let art_width = art.first().map_or(0, Line::width);
    let words_width = words.iter().map(Line::width).max().unwrap_or(0);
    if width < 2 + art_width + GAP + words_width {
        return words;
    }
    // The words beside the picture, centred on it.
    let top = art.len().saturating_sub(words.len()) / 2;
    let mut words = words.into_iter();
    art.into_iter()
        .enumerate()
        .map(|(row, art)| {
            let mut spans = vec![Span::raw("  ")];
            spans.extend(art.spans);
            if row >= top
                && let Some(line) = words.next()
            {
                spans.push(Span::raw(" ".repeat(GAP)));
                spans.extend(line.spans);
            }
            Line::from(spans)
        })
        .collect()
}

/// Columns between the picture and its words.
const GAP: usize = 4;

fn hints(accent: Style) -> Line<'static> {
    Line::from(vec![
        Span::styled("/", accent),
        " commands  ".dim(),
        Span::styled("/model", accent),
        " switch model  ".dim(),
        Span::styled("/status", accent),
        " tasks".dim(),
    ])
}

/// Rows of `art`, two pixel rows to a line: the upper pixel as the half
/// block's colour and the lower as its background.
fn pixels(art: &[&str], colors: &[(u8, u8, u8)]) -> Vec<Line<'static>> {
    let color = |row: Option<&&str>, x: usize| {
        let key = row?.as_bytes().get(x)?;
        let index =
            (*key as char).to_digit(36)? as usize + if key.is_ascii_uppercase() { 26 } else { 0 };
        let (r, g, b) = *colors.get(index)?;
        Some(Color::Rgb(r, g, b))
    };
    let width = art.first().map_or(0, |row| row.len());
    (0..art.len())
        .step_by(2)
        .map(|y| {
            Line::from(
                (0..width)
                    .map(|x| match (color(art.get(y), x), color(art.get(y + 1), x)) {
                        (Some(top), Some(bottom)) => {
                            Span::styled("▀", Style::new().fg(top).bg(bottom))
                        }
                        (Some(top), None) => Span::styled("▀", Style::new().fg(top)),
                        (None, Some(bottom)) => Span::styled("▄", Style::new().fg(bottom)),
                        (None, None) => Span::raw(" "),
                    })
                    .collect::<Vec<_>>(),
            )
        })
        .collect()
}

/// Sonny's portrait, 30 by 38 pixels drawn from shapes: one character per
/// pixel, `.` for none, else an index into [`PORTRAIT_COLORS`].
const PORTRAIT: [&str; 38] = [
    "..............................",
    "..............NN..............",
    "...........XXqqqNee...........",
    "..........JJJXXqqNeR..........",
    ".........J11JJXXqNedQ.........",
    "........111111JXqqvldQ........",
    ".......1111111JJXqZCYlQ.......",
    ".......11111111JXqMdlYc.......",
    ".......11111111JXqCldQE.......",
    "......JJ111111JJXqQCYlQQ......",
    "......XJ111111JXqqmdlYY3......",
    "......XXJJ11JJXXqNjdQQEY......",
    "......qXXJNNNXXqqPsZQQQl......",
    "......qqqkSSSGqqGSSSxQyB......",
    "......NNqFOankNNznaOk6Hk......",
    ".......NNNI5MqNeNM5oTbb.......",
    ".......eeeePeeek6sVWTbb.......",
    ".......PPPPPPPPkHFFTbbG.......",
    "........ssssssUWk66TbG........",
    "........FFFFFHkTTHHbGG........",
    ".........6666kWWTkWbG.........",
    ".........HHHHHkkWWTGG.........",
    ".........kkkTLLLLGTGG.........",
    "..........TTTTbbTTTG..........",
    "..........TTTTTTTTTb..........",
    "...........TTTTTTTT...........",
    "............wpffpw............",
    "............AAAAAA............",
    "............AAAAAA............",
    "............4AAAA4............",
    "..........UUKAAAAKUU..........",
    ".........XNN2wAAw8............",
    "......JJXXqeLuAAuLsssUUF......",
    "....11JJXXqNNNeeePPssUhgw7D...",
    "...111JJXXqNNNeeePPss76tirK0..",
    "..1111JJXXqNNNeeePPsWA9A000zp.",
    ".11111JJXXqNNNeeePPs2000000Ki.",
    "111111JJXXqNNNeeePPsD00000000A",
];

const PORTRAIT_COLORS: [(u8, u8, u8); 62] = [
    (28, 29, 34),
    (241, 241, 241),
    (133, 135, 140),
    (159, 185, 217),
    (79, 80, 85),
    (84, 120, 160),
    (207, 210, 214),
    (168, 169, 174),
    (105, 107, 112),
    (53, 54, 59),
    (118, 145, 174),
    (186, 190, 198),
    (135, 165, 207),
    (180, 203, 229),
    (223, 226, 229),
    (155, 158, 164),
    (94, 96, 100),
    (177, 180, 184),
    (38, 39, 44),
    (209, 219, 230),
    (198, 201, 205),
    (169, 194, 224),
    (193, 210, 230),
    (127, 138, 152),
    (184, 196, 210),
    (125, 127, 133),
    (233, 233, 234),
    (113, 114, 119),
    (217, 219, 221),
    (88, 89, 94),
    (44, 45, 50),
    (187, 202, 222),
    (73, 74, 80),
    (178, 189, 203),
    (179, 198, 221),
    (194, 195, 200),
    (33, 34, 39),
    (194, 202, 212),
    (140, 170, 211),
    (99, 100, 105),
    (164, 190, 220),
    (212, 214, 218),
    (181, 186, 194),
    (202, 206, 210),
    (210, 223, 236),
    (235, 237, 240),
    (48, 49, 54),
    (157, 162, 169),
    (185, 207, 229),
    (228, 230, 232),
    (130, 136, 147),
    (221, 222, 225),
    (176, 199, 226),
    (214, 219, 226),
    (176, 181, 189),
    (189, 194, 200),
    (214, 216, 222),
    (208, 213, 219),
    (194, 198, 203),
    (235, 236, 236),
    (137, 168, 209),
    (183, 202, 223),
];

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_pixel_names_a_color_and_rows_are_even() {
        let width = PORTRAIT[0].len();
        for row in PORTRAIT {
            assert_eq!(row.len(), width);
            for key in row.chars().filter(|c| *c != '.') {
                let index = key.to_digit(36).unwrap() as usize
                    + if key.is_ascii_uppercase() { 26 } else { 0 };
                assert!(index < PORTRAIT_COLORS.len(), "{key}");
            }
        }
        let lines = pixels(&PORTRAIT, &PORTRAIT_COLORS);
        assert_eq!(lines.len(), PORTRAIT.len().div_ceil(2));
        assert!(lines.iter().all(|line| line.width() == width));
    }
}
