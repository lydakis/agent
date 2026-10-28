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
    collections::HashMap,
    io::Write,
    sync::{Arc, Mutex, OnceLock, atomic::AtomicU32, atomic::Ordering},
};

/// The most rows a picture takes.
pub const ROWS: u32 = 30;
/// A cell this tall or more is in a Retina screen's own pixels, and the
/// image is drawn at its size; a shorter one (8×16 is what Rex answers
/// before it lays a block out) is drawn at twice it, to stay sharp. A
/// picture twice as large as it needs to be is four times the data.
const RETINA_CELL: u32 = 24;

/// The diacritics that number a placeholder's row and column, from the
/// Kitty protocol's table (rowcolumn-diacritics.txt), first 128 of 297:
/// a picture as wide as the conversation's prose, or a pane's.
const DIACRITICS: [char; 128] = [
    '\u{0305}', '\u{030D}', '\u{030E}', '\u{0310}', '\u{0312}', '\u{033D}', '\u{033E}', '\u{033F}',
    '\u{0346}', '\u{034A}', '\u{034B}', '\u{034C}', '\u{0350}', '\u{0351}', '\u{0352}', '\u{0357}',
    '\u{035B}', '\u{0363}', '\u{0364}', '\u{0365}', '\u{0366}', '\u{0367}', '\u{0368}', '\u{0369}',
    '\u{036A}', '\u{036B}', '\u{036C}', '\u{036D}', '\u{036E}', '\u{036F}', '\u{0483}', '\u{0484}',
    '\u{0485}', '\u{0486}', '\u{0487}', '\u{0592}', '\u{0593}', '\u{0594}', '\u{0595}', '\u{0597}',
    '\u{0598}', '\u{0599}', '\u{059C}', '\u{059D}', '\u{059E}', '\u{059F}', '\u{05A0}', '\u{05A1}',
    '\u{05A8}', '\u{05A9}', '\u{05AB}', '\u{05AC}', '\u{05AF}', '\u{05C4}', '\u{0610}', '\u{0611}',
    '\u{0612}', '\u{0613}', '\u{0614}', '\u{0615}', '\u{0616}', '\u{0617}', '\u{0657}', '\u{0658}',
    '\u{0659}', '\u{065A}', '\u{065B}', '\u{065D}', '\u{065E}', '\u{06D6}', '\u{06D7}', '\u{06D8}',
    '\u{06D9}', '\u{06DA}', '\u{06DB}', '\u{06DC}', '\u{06DF}', '\u{06E0}', '\u{06E1}', '\u{06E2}',
    '\u{06E4}', '\u{06E7}', '\u{06E8}', '\u{06EB}', '\u{06EC}', '\u{0730}', '\u{0732}', '\u{0733}',
    '\u{0735}', '\u{0736}', '\u{073A}', '\u{073D}', '\u{073F}', '\u{0740}', '\u{0741}', '\u{0743}',
    '\u{0745}', '\u{0747}', '\u{0749}', '\u{074A}', '\u{07EB}', '\u{07EC}', '\u{07ED}', '\u{07EE}',
    '\u{07EF}', '\u{07F0}', '\u{07F1}', '\u{07F3}', '\u{0816}', '\u{0817}', '\u{0818}', '\u{0819}',
    '\u{081B}', '\u{081C}', '\u{081D}', '\u{081E}', '\u{081F}', '\u{0820}', '\u{0821}', '\u{0822}',
    '\u{0823}', '\u{0825}', '\u{0826}', '\u{0827}', '\u{0829}', '\u{082A}', '\u{082B}', '\u{082C}',
];

pub const PLACEHOLDER: char = '\u{10EEEE}';

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

/// A new image id, at most 24 bits, as a placeholder's colour carries it:
/// the process in the top 8, so another Sonny in the same terminal likely
/// differs, and a count in the low 16, so pictures still in scrollback
/// keep theirs for the first 65,535.
fn next_id() -> u32 {
    static NEXT: AtomicU32 = AtomicU32::new(0);
    let base = (std::process::id() % 255 + 1) << 16;
    base | (NEXT.fetch_add(1, Ordering::Relaxed) % 0xFFFF + 1)
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

/// What a picture shown with [`show_linked`] opens, by its image id.
static SOURCES: Mutex<Option<HashMap<u32, String>>> = Mutex::new(None);

/// [`show`], a click on the picture opening `url`: the file it was drawn
/// from, shown bigger in the pane.
pub fn show_linked(drawn: &Drawn, url: Option<String>) -> Result<Vec<Line<'static>>, String> {
    let rows = show(drawn)?;
    if let (Some(url), Some(id)) = (url, rows.first().and_then(|row| id(row.spans.get(1)?))) {
        let mut sources = SOURCES.lock().unwrap_or_else(|e| e.into_inner());
        sources.get_or_insert_with(HashMap::new).insert(id, url);
    }
    Ok(rows)
}

/// The image a span of placeholder cells shows, by the id its colour
/// carries.
pub fn id(span: &Span) -> Option<u32> {
    match (span.content.starts_with(PLACEHOLDER), span.style.fg) {
        (true, Some(Color::Rgb(r, g, b))) => Some(u32::from_be_bytes([0, r, g, b])),
        _ => None,
    }
}

/// The rows a picture `id` of `cols`×`rows` cells is shown in, opening
/// `url`, as [`show_linked`] leaves them, nothing sent.
#[cfg(test)]
pub fn linked_rows(id: u32, cols: u32, rows: u32, url: &str) -> Vec<Line<'static>> {
    let mut sources = SOURCES.lock().unwrap_or_else(|e| e.into_inner());
    sources
        .get_or_insert_with(HashMap::new)
        .insert(id, url.to_owned());
    placeholders(id, cols, rows)
}

/// What a click on the picture `id` opens, if it was shown with one.
pub fn source(id: u32) -> Option<String> {
    let sources = SOURCES.lock().unwrap_or_else(|e| e.into_inner());
    sources.as_ref()?.get(&id).cloned()
}

/// The most rows a picture takes shown on its own in the pane: the
/// window's height less the pane's title and a row or two around it.
pub fn tall() -> u32 {
    let rows = ratatui::crossterm::terminal::size().map_or(ROWS, |(_, rows)| rows as u32);
    rows.saturating_sub(6).clamp(ROWS, DIACRITICS.len() as u32)
}

/// The most of a picture file read.
const MOST_BYTES: u64 = 64 << 20;
/// The most pixels a side of a picture file may have to be read.
const MOST_SIDE: u32 = 16_384;

/// A PNG, JPEG, GIF (its first frame) or WebP file drawn at its own size
/// when it fits, else as wide as `width` allows and at most `most_rows`
/// tall.
pub fn file(path: &std::path::Path, width: usize, most_rows: u32) -> Result<Drawn, String> {
    use std::io::Read;
    let mut bytes = Vec::new();
    std::fs::File::open(path)
        .and_then(|f| f.take(MOST_BYTES).read_to_end(&mut bytes))
        .map_err(|e| format!("{}: {e}", path.display()))?;
    raster(&bytes, width, most_rows)
}

/// [`file`], from the file's bytes.
pub fn raster(bytes: &[u8], width: usize, most_rows: u32) -> Result<Drawn, String> {
    let mut reader = image::ImageReader::new(std::io::Cursor::new(bytes))
        .with_guessed_format()
        .map_err(|e| e.to_string())?;
    let mut limits = image::Limits::default();
    limits.max_image_width = Some(MOST_SIDE);
    limits.max_image_height = Some(MOST_SIDE);
    reader.limits(limits);
    let picture = reader.decode().map_err(|e| e.to_string())?.into_rgba8();
    let (w, h) = picture.dimensions();
    if w == 0 || h == 0 {
        return Err("an empty picture".into());
    }
    let (cell_w, cell_h) = crate::term::cell();
    let (cols, rows) = fit_in(
        w as f32,
        h as f32,
        (cell_w, cell_h),
        now(width),
        most_rows,
        false,
    );
    // As many pixels as the cells hold at the scale they are drawn, and
    // never more than the picture has.
    let up = up(cell_h);
    let (room_w, room_h) = (cols * cell_w * up, rows * cell_h * up);
    let scale = (room_w as f32 / w as f32)
        .min(room_h as f32 / h as f32)
        .min(1.0);
    let (to_w, to_h) = (
        ((w as f32 * scale).round() as u32).max(1),
        ((h as f32 * scale).round() as u32).max(1),
    );
    let picture = match (to_w, to_h) == (w, h) {
        true => picture,
        false => {
            image::imageops::resize(&picture, to_w, to_h, image::imageops::FilterType::Triangle)
        }
    };
    let mut png = Vec::new();
    image::DynamicImage::ImageRgba8(picture)
        .write_to(&mut std::io::Cursor::new(&mut png), image::ImageFormat::Png)
        .map_err(|e| e.to_string())?;
    Ok(Drawn {
        png,
        cols,
        rows,
        cut: false,
    })
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
    page_in(png, width, ROWS)
}

/// [`page`], at most `most_rows` tall.
pub fn page_in(png: &[u8], width: usize, most_rows: u32) -> Result<Drawn, String> {
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
    let rows = tall.min(most_rows.min(DIACRITICS.len() as u32));
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
    render_in(text, width, ROWS, false)
}

/// [`render`], at most `most_rows` tall; with `grow`, as large as that
/// and `width` let it be, as a drawing is shown on its own in the pane.
pub fn render_in(text: &str, width: usize, most_rows: u32, grow: bool) -> Result<Drawn, String> {
    let width = now(width);
    let mut options = usvg::Options {
        fontdb: fonts(),
        ..usvg::Options::default()
    };
    options.font_family = "Helvetica".into();
    let tree = usvg::Tree::from_str(text, &options).map_err(|e| e.to_string())?;
    let size = tree.size();
    let (cell_w, cell_h) = crate::term::cell();
    let (cols, rows) = fit_in(
        size.width(),
        size.height(),
        (cell_w, cell_h),
        width,
        most_rows,
        grow,
    );
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
#[cfg(test)]
fn fit(w: f32, h: f32, cell: (u32, u32), width: usize) -> (u32, u32) {
    fit_in(w, h, cell, width, ROWS, false)
}

/// [`fit`], at most `most_rows` tall.
fn fit_in(
    w: f32,
    h: f32,
    (cell_w, cell_h): (u32, u32),
    width: usize,
    most_rows: u32,
    grow: bool,
) -> (u32, u32) {
    let most_cols = (width.saturating_sub(4) as u32).clamp(1, DIACRITICS.len() as u32);
    let own = (w / cell_w as f32).ceil() as u32;
    let mut cols = if grow {
        most_cols
    } else {
        own.clamp(1, most_cols)
    };
    let mut rows = ((cols * cell_w) as f32 * h / w / cell_h as f32).ceil() as u32;
    let most_rows = most_rows.clamp(1, DIACRITICS.len() as u32);
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
        let Drawn {
            png, cols, rows, ..
        } = render(svg, 80).unwrap();
        assert_eq!((cols, rows), (20, 5));
        assert!(png.starts_with(b"\x89PNG"));
        assert!(render("<svg", 80).is_err());
    }

    #[test]
    fn a_picture_file_is_drawn_no_larger_than_it_is_and_fits_the_pane() {
        let encoded = |w: u32, h: u32| {
            let mut png = Vec::new();
            image::DynamicImage::ImageRgba8(image::RgbaImage::new(w, h))
                .write_to(&mut std::io::Cursor::new(&mut png), image::ImageFormat::Png)
                .unwrap();
            png
        };
        // 8×16 cells, drawn at twice that: 160×80 pixels is 20×5 cells.
        let small = raster(&encoded(160, 80), 80, ROWS).unwrap();
        assert_eq!((small.cols, small.rows), (20, 5));
        let shot = image::load_from_memory(&small.png).unwrap();
        assert_eq!((shot.width(), shot.height()), (160, 80));
        // A wide one narrows to the pane, keeping its shape.
        let wide = raster(&encoded(4000, 1000), 60, ROWS).unwrap();
        assert_eq!((wide.cols, wide.rows), (56, 7));
        // A tall one stops at the rows it may take.
        let tall = raster(&encoded(100, 4000), 60, 40).unwrap();
        assert_eq!(tall.rows, 40);
        // A drawing shown on its own grows to the pane.
        assert_eq!(fit_in(80.0, 32.0, (8, 16), 60, ROWS, true), (56, 12));
        assert!(raster(b"not a picture", 60, ROWS).is_err());
    }

    #[test]
    fn a_linked_picture_opens_what_it_was_drawn_from() {
        let rows = placeholders(0x00FE_0102, 3, 2);
        let id = id(&rows[0].spans[1]).unwrap();
        assert_eq!(id, 0x00FE_0102);
        assert_eq!(source(id), None);
        SOURCES
            .lock()
            .unwrap()
            .get_or_insert_with(HashMap::new)
            .insert(id, "file:///w/a.mmd".into());
        assert_eq!(source(id).as_deref(), Some("file:///w/a.mmd"));
        assert_eq!(super::id(&rows[0].spans[0]), None);
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
