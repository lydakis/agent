//! Pictures kept once drawn, so attaching again, or redrawing in the
//! other theme, does not run node, resvg or a browser for each one.
//! Keyed by the fence's language and source and everything the drawing
//! depends on: the pane's width, its cell size and the background.
use crate::image::Drawn;
use std::{
    hash::{Hash, Hasher},
    path::PathBuf,
};

/// The picture as drawn before, else as `draw` draws it now.
pub fn drawn(
    lang: &str,
    source: &str,
    width: usize,
    draw: impl FnOnce() -> Result<Drawn, String>,
) -> Result<Drawn, String> {
    // Counted only, as the tool host counts a note's rows: drawn afresh,
    // with nothing read from or written to a shared folder.
    if crate::term::measured() {
        return draw();
    }
    let path = path(lang, source, width);
    if let Some(drawn) = path.as_ref().and_then(read) {
        return Ok(drawn);
    }
    let drawn = draw()?;
    if let Some(path) = path {
        let _ = write(&path, &drawn);
    }
    Ok(drawn)
}

fn path(lang: &str, source: &str, width: usize) -> Option<PathBuf> {
    let dir = std::env::temp_dir().join("sonny-pictures");
    std::fs::create_dir_all(&dir).ok()?;
    let mut hasher = std::hash::DefaultHasher::new();
    (
        lang,
        source,
        crate::image::now(width),
        crate::term::cell(),
        crate::term::light(),
    )
        .hash(&mut hasher);
    Some(dir.join(format!("{:016x}.bin", hasher.finish())))
}

/// Columns, rows and whether it was cut, then the PNG.
fn read(path: &PathBuf) -> Option<Drawn> {
    let bytes = std::fs::read(path).ok()?;
    let (head, png) = bytes.split_at_checked(9)?;
    Some(Drawn {
        cols: u32::from_le_bytes(head[0..4].try_into().ok()?),
        rows: u32::from_le_bytes(head[4..8].try_into().ok()?),
        cut: head[8] == 1,
        png: png.to_vec(),
    })
}

fn write(path: &PathBuf, drawn: &Drawn) -> std::io::Result<()> {
    let mut bytes = Vec::with_capacity(9 + drawn.png.len());
    bytes.extend_from_slice(&drawn.cols.to_le_bytes());
    bytes.extend_from_slice(&drawn.rows.to_le_bytes());
    bytes.push(u8::from(drawn.cut));
    bytes.extend_from_slice(&drawn.png);
    // Whole or not at all, should another Sonny read it meanwhile.
    let part = path.with_extension(format!("{}.part", std::process::id()));
    write_new(&part, &bytes)?;
    std::fs::rename(part, path)
}

/// `bytes` written to a file made new at `path`: refused when anything is
/// there already, a link left in a shared folder included, so a write
/// never goes on through a link to where it points.
pub fn write_new(path: &std::path::Path, bytes: impl AsRef<[u8]>) -> std::io::Result<()> {
    use std::io::Write;
    let mut file = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(path)?;
    file.write_all(bytes.as_ref())
}

#[cfg(test)]
mod tests {
    #[cfg(unix)]
    #[test]
    fn a_new_file_is_never_written_through_a_link_left_in_its_place() {
        let dir = std::env::temp_dir().join(format!("sonny-write-new-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let (link, target) = (dir.join("kept.svg"), dir.join("elsewhere"));
        std::os::unix::fs::symlink(&target, &link).unwrap();
        assert!(super::write_new(&link, "<svg/>").is_err());
        assert!(!target.exists());
        // Nor over a file already there; a new one is written whole.
        std::fs::write(&target, "theirs").unwrap();
        assert!(super::write_new(&target, "ours").is_err());
        assert_eq!(std::fs::read_to_string(&target).unwrap(), "theirs");
        super::write_new(&dir.join("new.svg"), "<svg/>").unwrap();
        assert_eq!(
            std::fs::read_to_string(dir.join("new.svg")).unwrap(),
            "<svg/>"
        );
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
