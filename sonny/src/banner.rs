//! The welcome at the top of a new project's empty transcript: Sonny, the
//! robot the client is named for, as a portrait the first time Sonny ever
//! runs and as a small icon after that. Drawn with half blocks, two pixels
//! a cell, and never above a conversation that already has history.
use ratatui::{
    style::{Color, Modifier, Style, Stylize},
    text::{Line, Span},
};

/// The welcome for a new project's lead `bot` in `folder`, on `model`, at
/// `width` columns: the portrait when Sonny has never welcomed anyone
/// (noted in `~/.agent/sonny/welcomed`), else the icon. Too narrow for
/// the picture, the words alone.
pub fn welcome(bot: &str, folder: &str, model: &str, width: usize) -> Vec<Line<'static>> {
    let first = first_time();
    let version = env!("CARGO_PKG_VERSION");
    let accent = Style::new().fg(Color::Rgb(255, 190, 60));
    let (art, words) = match first {
        true => (
            pixels(&PORTRAIT, &PORTRAIT_COLORS),
            vec![
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
            ],
        ),
        false => (
            pixels(&ICON, &ICON_COLORS),
            vec![
                Line::from(vec![
                    Span::styled("sonny", accent.add_modifier(Modifier::BOLD)),
                    format!(" {version}").dim(),
                ]),
                Line::from(format!("{bot} · {model} · {folder}").dim()),
                hints(accent),
            ],
        ),
    };
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

/// Whether this is the first welcome: true once, as it is noted.
fn first_time() -> bool {
    let Some(home) = std::env::var_os("HOME") else {
        return false;
    };
    let noted = std::path::PathBuf::from(home).join(".agent/sonny/welcomed");
    if noted.exists() {
        return false;
    }
    if let Some(dir) = noted.parent() {
        let _ = std::fs::create_dir_all(dir);
    }
    std::fs::write(&noted, "").is_ok()
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

/// The icon, 13 by 14 pixels, indexed into [`ICON_COLORS`].
const ICON: [&str; 14] = [
    "....00000....",
    "...00000011..",
    "..0000000111.",
    "..0000000011.",
    "..0000000001.",
    "..0022000220.",
    "..0000000000.",
    "...00000000..",
    "....000000...",
    ".....0000....",
    "......33.....",
    ".....4334....",
    "..0000330033.",
    ".000000003333",
];

const ICON_COLORS: [(u8, u8, u8); 5] = [
    (232, 235, 238),
    (122, 170, 226),
    (110, 168, 230),
    (34, 35, 40),
    (200, 202, 208),
];

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_pixel_names_a_color_and_rows_are_even() {
        for (art, colors) in [
            (&PORTRAIT[..], &PORTRAIT_COLORS[..]),
            (&ICON[..], &ICON_COLORS[..]),
        ] {
            let width = art[0].len();
            for row in art {
                assert_eq!(row.len(), width);
                for key in row.chars().filter(|c| *c != '.') {
                    let index = key.to_digit(36).unwrap() as usize
                        + if key.is_ascii_uppercase() { 26 } else { 0 };
                    assert!(index < colors.len(), "{key}");
                }
            }
            let lines = pixels(art, colors);
            assert_eq!(lines.len(), art.len().div_ceil(2));
            assert!(lines.iter().all(|line| line.width() == width));
        }
    }
}
