//! The welcome at the top of every lead's transcript, as Claude Code heads
//! its own: Sonny, the robot the client is named for, as a portrait drawn
//! with half blocks, two pixels a cell, above the conversation's first item.
//! Until the person types or scrolls, Sonny blinks or glances now and then.
use std::time::{Duration, Instant};

use ratatui::{
    buffer::Buffer,
    style::{Color, Style, Stylize},
    text::{Line, Span},
};

/// The welcome for the agent in `folder`, a lead or a task, on `model`, at
/// `width` columns: the portrait beside its words, or, too narrow for the
/// picture, the words alone. With the portrait, which of its lines the eyes
/// are on.
pub fn welcome(
    folder: &str,
    model: &str,
    lead: bool,
    width: usize,
) -> (Vec<Line<'static>>, Option<usize>) {
    let version = env!("CARGO_PKG_VERSION");
    let accent = Style::new().fg(Color::Cyan);
    let art = pixels(&PORTRAIT, &PORTRAIT_COLORS);
    let words = vec![
        Line::from(vec![
            "Welcome to Sonny".bold(),
            format!("  {version}").dim(),
        ]),
        Line::default(),
    ];
    let what = match lead {
        true => [
            "This folder's lead. Ask for anything:",
            "it plans, starts tasks in their own copies,",
            "and tells you when they need you.",
        ],
        false => [
            "A task of this folder's lead. Steer it here:",
            "it works on what it was asked and tells",
            "its lead what it found.",
        ],
    };
    let words = [
        words,
        what.map(|line| Line::from(line.dim())).to_vec(),
        vec![
            Line::default(),
            Line::from(format!("{folder} · {}", crate::models::named(model)).dim()),
            Line::default(),
            hints(accent),
        ],
    ]
    .concat();
    let art_width = art.first().map_or(0, Line::width);
    let words_width = words.iter().map(Line::width).max().unwrap_or(0);
    // The portrait is its colors; with NO_COLOR it would be a smudge.
    if width < 2 + art_width + GAP + words_width || crate::look::no_color() {
        return (words, None);
    }
    // The words beside the picture, centred on it.
    let top = art.len().saturating_sub(words.len()) / 2;
    let mut words = words.into_iter();
    let lines = art
        .into_iter()
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
        .collect();
    (lines, Some(EYES / 2))
}

/// Columns between the picture and its words.
const GAP: usize = 4;

/// The portrait's pixel row the eyes start on: they take it and the one
/// under it, so one line of the welcome.
const EYES: usize = 14;
/// The pixels a look covers on those two rows: each eye's three.
const EYE_COLUMNS: [u16; 6] = [10, 11, 12, 17, 18, 19];

/// Shortest and longest rest between gestures: rarer than a person's few
/// seconds, so it reads as calm rather than busy.
const REST: (Duration, Duration) = (Duration::from_secs(8), Duration::from_secs(16));
/// The first gesture, soon after the welcome is drawn, so it is seen.
const FIRST: Duration = Duration::from_millis(2500);

/// Where the eyes look, or how far the lids are down.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Look {
    Open,
    Half,
    Shut,
    /// At the words beside the portrait.
    Right,
    Left,
}

const fn ms(ms: u64) -> Duration {
    Duration::from_millis(ms)
}

/// What the eyes do now and then: each a run of looks, every step at
/// least the 100 ms between draws so none is skipped, and how often it
/// is picked against the others. Mostly a blink; now and then a double
/// or a slow one, or a glance.
const GESTURES: [(u32, &[(Look, Duration)]); 6] = [
    (
        9,
        &[
            (Look::Half, ms(100)),
            (Look::Shut, ms(150)),
            (Look::Half, ms(100)),
        ],
    ),
    (
        3,
        &[
            (Look::Half, ms(100)),
            (Look::Shut, ms(150)),
            (Look::Half, ms(100)),
            (Look::Open, ms(200)),
            (Look::Half, ms(100)),
            (Look::Shut, ms(150)),
            (Look::Half, ms(100)),
        ],
    ),
    (
        2,
        &[
            (Look::Half, ms(200)),
            (Look::Shut, ms(700)),
            (Look::Half, ms(200)),
        ],
    ),
    (3, &[(Look::Right, ms(1500))]),
    (2, &[(Look::Left, ms(1200))]),
    (
        1,
        &[
            (Look::Left, ms(900)),
            (Look::Open, ms(300)),
            (Look::Right, ms(900)),
        ],
    ),
];

/// When Sonny's eyes move: now and then, at rests that vary as a
/// person's do, each time one of [`GESTURES`].
pub struct Eyes {
    /// When the gesture under way, or the next, starts.
    next: Instant,
    /// Gestures so far, which pick each rest and gesture.
    count: u64,
}

impl Eyes {
    pub fn new(now: Instant) -> Self {
        Eyes {
            next: now + FIRST,
            count: 0,
        }
    }

    /// Where the eyes look at `now`.
    pub fn look(&mut self, now: Instant) -> Look {
        let Some(mut into) = now.checked_duration_since(self.next) else {
            return Look::Open;
        };
        for (look, lasts) in gesture(self.count) {
            match into.checked_sub(*lasts) {
                Some(left) => into = left,
                None => return *look,
            }
        }
        // The gesture over (or long past, as after the Mac slept): the
        // next after a rest from now.
        self.count += 1;
        self.next = now + rest(self.count);
        Look::Open
    }
}

/// A number from 0 to 1 for `n`, the same each time: what varies the
/// rests and gestures without a source of chance.
fn hash(n: u64) -> f64 {
    let mut x = n.wrapping_add(0x9e37_79b9_7f4a_7c15);
    x = (x ^ (x >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
    x = (x ^ (x >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
    x ^= x >> 31;
    (x >> 11) as f64 / (1u64 << 53) as f64
}

/// The rest before gesture `n`: anywhere in [`REST`].
fn rest(n: u64) -> Duration {
    let (short, long) = REST;
    short + (long - short).mul_f64(hash(n))
}

/// Gesture `n`, picked by the weights in [`GESTURES`]. The first is
/// always a blink, the one people know.
fn gesture(n: u64) -> &'static [(Look, Duration)] {
    let total: u32 = GESTURES.iter().map(|(weight, _)| weight).sum();
    let mut pick = match n {
        0 => 0,
        n => (hash(n ^ 0x5eed) * f64::from(total)) as u32,
    };
    for (weight, steps) in GESTURES {
        match pick.checked_sub(weight) {
            Some(left) => pick = left,
            None => return steps,
        }
    }
    GESTURES[0].1
}

/// A pixel's colour.
type Rgb = (u8, u8, u8);

/// The eyes' pixels for `look`, as (upper, lower) on each of
/// [`EYE_COLUMNS`]; none for open, which is the portrait as drawn.
fn eye_pixels(look: Look) -> Option<[(Rgb, Rgb); 6]> {
    // The portrait's own: the dark of the socket, the iris's blues, the
    // pupil; and the lid and the soft line of a shut eye.
    const SOCKET: Rgb = (130, 136, 147);
    const IRIS: Rgb = (118, 145, 174);
    const SHADE: Rgb = (127, 138, 152);
    const PUPIL: Rgb = (84, 120, 160);
    const PALE: Rgb = (185, 207, 229);
    const PALER: Rgb = (210, 223, 236);
    const EDGE: Rgb = (184, 196, 210);
    const LID: Rgb = (198, 201, 205);
    const LASHES: Rgb = (150, 155, 165);
    Some(match look {
        Look::Open => return None,
        Look::Half => [
            (LID, PALER),
            (LID, PUPIL),
            (LID, PALE),
            (LID, PALE),
            (LID, PUPIL),
            (LID, EDGE),
        ],
        Look::Shut => [(LID, LASHES); 6],
        Look::Left => [
            (IRIS, PUPIL),
            (SHADE, PALE),
            (SOCKET, PALER),
            (IRIS, PUPIL),
            (SHADE, PALE),
            (SOCKET, EDGE),
        ],
        Look::Right => [
            (SOCKET, PALER),
            (SHADE, PALE),
            (IRIS, PUPIL),
            (SOCKET, PALE),
            (SHADE, EDGE),
            (IRIS, PUPIL),
        ],
    })
}

/// The eyes drawn as `look` over the portrait drawn in `buf`, whose eyes'
/// line is at row `y`, the portrait from column `x`.
pub fn draw_eyes(buf: &mut Buffer, x: u16, y: u16, look: Look) {
    let Some(pixels) = eye_pixels(look) else {
        return;
    };
    let rgb = |(r, g, b): (u8, u8, u8)| Color::Rgb(r, g, b);
    for (column, (upper, lower)) in EYE_COLUMNS.into_iter().zip(pixels) {
        if let Some(cell) = buf.cell_mut((x + 2 + column, y)) {
            cell.set_fg(rgb(upper)).set_bg(rgb(lower));
        }
    }
}

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
        // Open eyes are the portrait's own pixels.
        let rgb = |key: u8| {
            let index = (key as char).to_digit(36).unwrap() as usize
                + if key.is_ascii_uppercase() { 26 } else { 0 };
            PORTRAIT_COLORS[index]
        };
        let open = EYE_COLUMNS.map(|x| {
            let x = x as usize;
            (
                rgb(PORTRAIT[EYES].as_bytes()[x]),
                rgb(PORTRAIT[EYES + 1].as_bytes()[x]),
            )
        });
        // Half shut keeps the lower row as it is.
        let half = eye_pixels(Look::Half).unwrap();
        assert!(half.iter().zip(open).all(|(h, o)| h.1 == o.1));
    }

    #[test]
    fn the_eyes_are_where_the_portrait_has_them() {
        // Both eye rows are drawn on every column a lid covers, so a
        // blink changes colours only, never a cell's symbol.
        for column in EYE_COLUMNS {
            for row in [EYES, EYES + 1] {
                assert_ne!(PORTRAIT[row].as_bytes()[column as usize], b'.');
            }
        }
        let (lines, eyes) = welcome("~/x", "m", true, 200);
        let eyes = eyes.unwrap();
        let line = lines[eyes].to_string();
        assert_eq!(line.chars().nth(2 + EYE_COLUMNS[0] as usize), Some('▀'));
        assert_eq!(welcome("~/x", "m", true, 20).1, None);
        // A task's window says what a task is, not what a lead is.
        let (task, _) = welcome("~/x", "m", false, 20);
        assert_eq!(
            task[2].to_string(),
            "A task of this folder's lead. Steer it here:"
        );
    }

    #[test]
    fn the_first_gesture_is_a_blink_then_a_rest() {
        let start = Instant::now();
        let mut eyes = Eyes::new(start);
        let at = |ms: u64| start + FIRST + Duration::from_millis(ms);
        assert_eq!(eyes.look(start), Look::Open);
        assert_eq!(eyes.look(at(0)), Look::Half);
        assert_eq!(eyes.look(at(120)), Look::Shut);
        assert_eq!(eyes.look(at(260)), Look::Half);
        // Past the blink: open for the rest.
        assert_eq!(eyes.look(at(400)), Look::Open);
        assert_eq!(eyes.look(at(5000)), Look::Open);
    }

    #[test]
    fn gestures_vary_and_every_step_is_drawn() {
        for (_, steps) in GESTURES {
            assert!(steps.iter().all(|(_, lasts)| *lasts >= ms(100)));
        }
        let picked: std::collections::HashSet<_> = (1..400).map(|n| gesture(n).as_ptr()).collect();
        assert_eq!(picked.len(), GESTURES.len());
        // Mostly blinks.
        let blinks = (1..400).filter(|n| gesture(*n) == GESTURES[0].1).count();
        assert!((120..240).contains(&blinks), "{blinks}");
    }

    #[test]
    fn rests_vary_within_their_bounds() {
        let rests: Vec<Duration> = (1..200).map(rest).collect();
        assert!(rests.iter().all(|r| (REST.0..REST.1).contains(r)));
        let (min, max) = (rests.iter().min().unwrap(), rests.iter().max().unwrap());
        assert!(*max - *min > Duration::from_secs(5));
    }
}
