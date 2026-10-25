//! Pictures in the transcript: an SVG a reply carries in a ```svg fence,
//! drawn with resvg and shown through Kitty graphics. The image is sent
//! once and placed with Unicode placeholder cells, so it is text to the
//! terminal and scrolls, and stays, with the rest of the history.
use ratatui::{
    style::{Color, Stylize},
    text::{Line, Span},
};
use resvg::{tiny_skia, usvg};
use std::{
    io::Write,
    sync::{Arc, OnceLock, atomic::AtomicU32, atomic::Ordering},
};

/// The most rows a picture takes.
const ROWS: u32 = 30;
/// A cell this tall or more is in a Retina screen's own pixels, and the
/// image is drawn at its size; a shorter one (8×16 is what Rex answers
/// before it lays a block out) is drawn at twice it, to stay sharp. A
/// picture twice as large as it needs to be is four times the data.
const RETINA_CELL: u32 = 24;

/// The diacritics that number a placeholder's row and column, from the
/// Kitty protocol's table (rowcolumn-diacritics.txt), first 64 of 297.
const DIACRITICS: [char; 64] = [
    '\u{0305}', '\u{030D}', '\u{030E}', '\u{0310}', '\u{0312}', '\u{033D}', '\u{033E}', '\u{033F}',
    '\u{0346}', '\u{034A}', '\u{034B}', '\u{034C}', '\u{0350}', '\u{0351}', '\u{0352}', '\u{0357}',
    '\u{035B}', '\u{0363}', '\u{0364}', '\u{0365}', '\u{0366}', '\u{0367}', '\u{0368}', '\u{0369}',
    '\u{036A}', '\u{036B}', '\u{036C}', '\u{036D}', '\u{036E}', '\u{036F}', '\u{0483}', '\u{0484}',
    '\u{0485}', '\u{0486}', '\u{0487}', '\u{0592}', '\u{0593}', '\u{0594}', '\u{0595}', '\u{0597}',
    '\u{0598}', '\u{0599}', '\u{059C}', '\u{059D}', '\u{059E}', '\u{059F}', '\u{05A0}', '\u{05A1}',
    '\u{05A8}', '\u{05A9}', '\u{05AB}', '\u{05AC}', '\u{05AF}', '\u{05C4}', '\u{0610}', '\u{0611}',
    '\u{0612}', '\u{0613}', '\u{0614}', '\u{0615}', '\u{0616}', '\u{0617}', '\u{0657}', '\u{0658}',
];

const PLACEHOLDER: char = '\u{10EEEE}';

/// System fonts, read once, for text inside an SVG.
fn fonts() -> Arc<usvg::fontdb::Database> {
    static FONTS: OnceLock<Arc<usvg::fontdb::Database>> = OnceLock::new();
    FONTS
        .get_or_init(|| {
            let mut db = usvg::fontdb::Database::new();
            db.load_system_fonts();
            Arc::new(db)
        })
        .clone()
}

/// The system fonts read now, off the thread that draws.
pub fn preload() {
    fonts();
}

/// A new image id, distinct from other Sonnys' in the same terminal, and
/// at most 24 bits, as a placeholder's colour carries it.
fn next_id() -> u32 {
    static NEXT: AtomicU32 = AtomicU32::new(0);
    let base = (std::process::id() % 0xFFFF + 1) << 8;
    base | (NEXT.fetch_add(1, Ordering::Relaxed) % 255 + 1)
}

/// A picture ready to show: its PNG, the cells it fills, and whether it
/// was cut short.
pub struct Drawn {
    pub png: Vec<u8>,
    pub cols: u32,
    pub rows: u32,
    pub cut: bool,
}

/// The picture sent to the terminal, as the rows of cells that show it.
pub fn show(drawn: &Drawn) -> Result<Vec<Line<'static>>, String> {
    let id = next_id();
    send(id, drawn.cols, drawn.rows, &drawn.png).map_err(|e| e.to_string())?;
    Ok(placeholders(id, drawn.cols, drawn.rows))
}

/// The pane's width as it is now, should it have narrowed since `width`
/// was read: a picture wider than its pane wraps into nonsense.
pub fn now(width: usize) -> usize {
    ratatui::crossterm::terminal::size().map_or(width, |(cols, _)| width.min(cols as usize))
}

/// How many of a picture's pixels a cell is wide, at the scale it is drawn.
fn up(cell_h: u32) -> u32 {
    if cell_h >= RETINA_CELL { 1 } else { 2 }
}

/// The columns a page is laid out across, in CSS pixels at twice that in
/// the screen's: as wide as the pane, within reason.
pub fn page_width(width: usize) -> u32 {
    let (cell_w, cell_h) = crate::term::cell();
    let cols = (width.saturating_sub(4) as u32).clamp(1, DIACRITICS.len() as u32);
    (cols * cell_w * up(cell_h) / 2).clamp(320, 960)
}

/// A page's picture, its blank end cut off
/// and drawn at its own size: as wide as it is, and its top [`ROWS`] rows
/// when it is longer. Says whether it was cut.
pub fn page(png: &[u8], width: usize) -> Result<Drawn, String> {
    let width = now(width);
    let shot = tiny_skia::Pixmap::decode_png(png).map_err(|e| e.to_string())?;
    let (w, h) = trimmed(&shot);
    let (cell_w, cell_h) = crate::term::cell();
    let (cw, ch) = (cell_w * up(cell_h), cell_h * up(cell_h));
    let most_cols = (width.saturating_sub(4) as u32).clamp(1, DIACRITICS.len() as u32);
    let cols = w.div_ceil(cw).clamp(1, most_cols);
    let scale = (cols * cw) as f32 / w as f32;
    let scale = scale.min(1.0);
    let tall = ((h as f32 * scale) as u32).div_ceil(ch).max(1);
    let most_rows = ROWS.min(DIACRITICS.len() as u32);
    let rows = tall.min(most_rows);
    let mut canvas = tiny_skia::Pixmap::new(cols * cw, rows * ch).ok_or("image too large")?;
    canvas.draw_pixmap(
        0,
        0,
        shot.as_ref(),
        &tiny_skia::PixmapPaint {
            quality: tiny_skia::FilterQuality::Bicubic,
            ..Default::default()
        },
        tiny_skia::Transform::from_scale(scale, scale),
        None,
    );
    let png = canvas.encode_png().map_err(|e| e.to_string())?;
    Ok(Drawn {
        png,
        cols,
        rows,
        cut: tall > rows,
    })
}

/// The width and height a screenshot keeps once the rows and columns at
/// its right and bottom that are all its background are dropped.
fn trimmed(shot: &tiny_skia::Pixmap) -> (u32, u32) {
    let (w, h) = (shot.width(), shot.height());
    let pixels = shot.pixels();
    let at = |x: u32, y: u32| pixels[(y * w + x) as usize];
    let background = at(w - 1, h - 1);
    let last_row = (0..h)
        .rev()
        .find(|&y| (0..w).any(|x| at(x, y) != background))
        .unwrap_or(0);
    let last_col = (0..w)
        .rev()
        .find(|&x| (0..=last_row).any(|y| at(x, y) != background))
        .unwrap_or(0);
    // A margin as wide as the page's own, about 16 CSS pixels.
    ((last_col + 32).min(w), (last_row + 32).min(h))
}

/// The SVG as a PNG that fills `cols`×`rows` cells.
pub fn render(text: &str, width: usize) -> Result<Drawn, String> {
    let width = now(width);
    let mut options = usvg::Options {
        fontdb: fonts(),
        ..usvg::Options::default()
    };
    options.font_family = "Helvetica".into();
    let tree = usvg::Tree::from_str(text, &options).map_err(|e| e.to_string())?;
    let size = tree.size();
    let (cell_w, cell_h) = crate::term::cell();
    let (cols, rows) = fit(size.width(), size.height(), (cell_w, cell_h), width);
    let up = up(cell_h);
    let (px_w, px_h) = (cols * cell_w * up, rows * cell_h * up);
    let mut pixmap = tiny_skia::Pixmap::new(px_w, px_h).ok_or("image too large")?;
    let scale = (px_w as f32 / size.width()).min(px_h as f32 / size.height());
    let (dx, dy) = (
        (px_w as f32 - size.width() * scale) / 2.0,
        (px_h as f32 - size.height() * scale) / 2.0,
    );
    let transform = tiny_skia::Transform::from_scale(scale, scale).post_translate(dx, dy);
    resvg::render(&tree, transform, &mut pixmap.as_mut());
    let png = pixmap.encode_png().map_err(|e| e.to_string())?;
    Ok(Drawn {
        png,
        cols,
        rows,
        cut: false,
    })
}

/// Cells for a picture of `w`×`h` pixels: its own size when it fits, else
/// as wide as the pane allows, and no more than [`ROWS`] tall.
fn fit(w: f32, h: f32, (cell_w, cell_h): (u32, u32), width: usize) -> (u32, u32) {
    let most_cols = (width.saturating_sub(4) as u32).clamp(1, DIACRITICS.len() as u32 * 4);
    let most_cols = most_cols.min(DIACRITICS.len() as u32);
    let mut cols = ((w / cell_w as f32).ceil() as u32).clamp(1, most_cols);
    let mut rows = ((cols * cell_w) as f32 * h / w / cell_h as f32).ceil() as u32;
    let most_rows = ROWS.min(DIACRITICS.len() as u32);
    if rows > most_rows {
        rows = most_rows;
        cols = (((rows * cell_h) as f32 * w / h) / cell_w as f32).ceil() as u32;
        cols = cols.clamp(1, most_cols);
    }
    (cols, rows.max(1))
}

/// Transmit the PNG, kept by the terminal as a virtual placement of
/// `cols`×`rows` cells, in chunks the protocol allows.
fn send(id: u32, cols: u32, rows: u32, png: &[u8]) -> std::io::Result<()> {
    use base64::Engine;
    let data = base64::engine::general_purpose::STANDARD.encode(png);
    let chunks: Vec<&[u8]> = data.as_bytes().chunks(4096).collect();
    let mut out = Vec::with_capacity(data.len() + chunks.len() * 16 + 64);
    for (i, chunk) in chunks.iter().enumerate() {
        let more = u8::from(i + 1 < chunks.len());
        if i == 0 {
            write!(
                out,
                "\x1b_Ga=T,U=1,q=2,f=100,i={id},c={cols},r={rows},m={more};"
            )?;
        } else {
            write!(out, "\x1b_Gm={more};")?;
        }
        out.extend_from_slice(chunk);
        out.extend_from_slice(b"\x1b\\");
    }
    let mut stdout = std::io::stdout();
    stdout.write_all(&out)?;
    stdout.flush()
}

/// Rows of placeholder cells, each naming its row and column, coloured
/// with the image's id so the terminal knows which image they show.
fn placeholders(id: u32, cols: u32, rows: u32) -> Vec<Line<'static>> {
    let colour = Color::Rgb((id >> 16) as u8, (id >> 8) as u8, id as u8);
    (0..rows as usize)
        .map(|row| {
            let cells: String = (0..cols as usize)
                .flat_map(|col| [PLACEHOLDER, DIACRITICS[row], DIACRITICS[col]])
                .collect();
            Line::from(vec![Span::raw("  "), Span::raw(cells).fg(colour)])
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_picture_fits_the_pane_and_keeps_its_shape() {
        // 800×400 in a 60-column pane of 8×16 cells: as wide as it may be.
        assert_eq!(fit(800.0, 400.0, (8, 16), 60), (56, 14));
        // Small enough to show at its own size.
        assert_eq!(fit(80.0, 32.0, (8, 16), 60), (10, 2));
        // Tall ones stop at ROWS.
        let (cols, rows) = fit(100.0, 2000.0, (8, 16), 60);
        assert_eq!(rows, ROWS);
        assert!(cols < 10, "{cols}");
    }

    #[test]
    fn an_svg_renders_to_a_png_of_whole_cells() {
        let svg = r#"<svg xmlns="http://www.w3.org/2000/svg" width="160" height="80">
            <rect width="160" height="80" fill="orange"/></svg>"#;
        let Drawn { png, cols, rows, .. } = render(svg, 80).unwrap();
        assert_eq!((cols, rows), (20, 5));
        assert!(png.starts_with(b"\x89PNG"));
        assert!(render("<svg", 80).is_err());
    }

    #[test]
    fn placeholder_rows_are_one_cell_per_column() {
        let rows = placeholders(0x0001_0203, 5, 2);
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0].width(), 2 + 5);
        assert_eq!(rows[1].spans[1].style.fg, Some(Color::Rgb(1, 2, 3)));
        assert!(
            rows[1].spans[1]
                .content
                .starts_with("\u{10EEEE}\u{030D}\u{0305}")
        );
    }
}
