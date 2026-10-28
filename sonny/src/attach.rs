//! Pictures the person gives the agent with a message: one on the
//! clipboard, taken with Ctrl-V as Codex and Claude Code take it, or an
//! image file whose path is pasted or dropped on the terminal. Each is
//! kept until the message goes, behind a placeholder in the composer.
use std::path::{Path, PathBuf};

/// A picture as it goes to the agent.
#[derive(Clone, Debug, PartialEq)]
pub struct Picture {
    pub media_type: &'static str,
    pub data: Vec<u8>,
}

/// The longest side a picture is sent at: what Anthropic's models see
/// at most (larger ones it scales down), and more than OpenAI's high
/// detail keeps of a screenshot's shorter side.
const LONGEST: u32 = 1568;

/// The most bytes a picture may have, and the pictures of one message
/// together: Anthropic's limit for one image, and the daemon's for a
/// message.
pub const MOST_BYTES: usize = 5 * 1024 * 1024;

/// What a picture is called in the composer and the message, numbered
/// from 1.
pub fn placeholder(number: usize) -> String {
    format!("[Image #{number}]")
}

/// The kind of image a file is, by its name.
fn media_type(path: &Path) -> Option<&'static str> {
    let ext = path.extension()?.to_str()?.to_ascii_lowercase();
    Some(match ext.as_str() {
        "png" => "image/png",
        "jpg" | "jpeg" => "image/jpeg",
        "gif" => "image/gif",
        "webp" => "image/webp",
        _ => return None,
    })
}

/// `pictures` as `submit` takes them; nothing for none.
pub fn submitted(pictures: &[Picture]) -> Option<serde_json::Value> {
    use base64::Engine;
    (!pictures.is_empty()).then(|| {
        pictures
            .iter()
            .map(|picture| {
                serde_json::json!({"media_type": picture.media_type,
                    "data": base64::engine::general_purpose::STANDARD.encode(&picture.data)})
            })
            .collect()
    })
}

/// The image files a paste names, when every word of it is one that is
/// there: what a terminal pastes for files dropped on it, with spaces
/// escaped or quoted, or as `file://` addresses.
pub fn paths(pasted: &str, workspace: &Path) -> Option<Vec<PathBuf>> {
    let words = shell_words(pasted.trim())?;
    if words.is_empty() {
        return None;
    }
    words
        .into_iter()
        .map(|word| {
            let word = match word.strip_prefix("file://") {
                Some(rest) => unescape_url(rest.strip_prefix("localhost").unwrap_or(rest))?,
                None => word,
            };
            let path = match word.strip_prefix("~/") {
                Some(rest) => PathBuf::from(std::env::var_os("HOME")?).join(rest),
                None => workspace.join(word),
            };
            (media_type(&path).is_some() && path.is_file()).then_some(path)
        })
        .collect()
}

/// `text` split as a shell splits words: at unquoted whitespace, with
/// backslash escapes and quotes taken off. None for an unclosed quote.
fn shell_words(text: &str) -> Option<Vec<String>> {
    let mut words = Vec::new();
    let mut word: Option<String> = None;
    let mut chars = text.chars();
    while let Some(c) = chars.next() {
        match c {
            '\\' => word.get_or_insert_default().push(chars.next()?),
            '\'' | '"' => {
                let word = word.get_or_insert_default();
                loop {
                    match chars.next()? {
                        close if close == c => break,
                        '\\' if c == '"' => word.push(chars.next()?),
                        other => word.push(other),
                    }
                }
            }
            c if c.is_whitespace() => words.extend(word.take()),
            c => word.get_or_insert_default().push(c),
        }
    }
    words.extend(word);
    Some(words)
}

/// A `file://` address's path, its `%XX` escapes read.
fn unescape_url(path: &str) -> Option<String> {
    let bytes = path.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut at = 0;
    while at < bytes.len() {
        if bytes[at] == b'%' {
            let hex = std::str::from_utf8(bytes.get(at + 1..at + 3)?).ok()?;
            out.push(u8::from_str_radix(hex, 16).ok()?);
            at += 3;
        } else {
            out.push(bytes[at]);
            at += 1;
        }
    }
    String::from_utf8(out).ok()
}

/// The picture in an image file, a PNG scaled to fit.
pub fn load(path: &Path) -> Result<Picture, String> {
    let name = path.display();
    let media_type = media_type(path).ok_or_else(|| format!("{name} is not an image"))?;
    let data = std::fs::read(path).map_err(|e| format!("{name}: {e}"))?;
    picture(media_type, data).map_err(|e| format!("{name}: {e}"))
}

fn picture(media_type: &'static str, data: Vec<u8>) -> Result<Picture, String> {
    // A PNG is scaled down first, which most often brings it under.
    let data = match media_type {
        "image/png" => fit(data)?,
        _ => data,
    };
    if data.len() > MOST_BYTES {
        return Err(format!(
            "{} MB is past the {} MB an image may be",
            data.len().div_ceil(1024 * 1024),
            MOST_BYTES / 1024 / 1024
        ));
    }
    Ok(Picture { media_type, data })
}

/// A PNG no longer on its longest side than [`LONGEST`]: as it is when it
/// fits.
fn fit(png: Vec<u8>) -> Result<Vec<u8>, String> {
    use resvg::tiny_skia;
    let shot = tiny_skia::Pixmap::decode_png(&png).map_err(|e| format!("not a PNG: {e}"))?;
    let longest = shot.width().max(shot.height());
    if longest <= LONGEST {
        return Ok(png);
    }
    let scale = LONGEST as f32 / longest as f32;
    let (width, height) = (
        ((shot.width() as f32 * scale).round() as u32).max(1),
        ((shot.height() as f32 * scale).round() as u32).max(1),
    );
    let mut canvas = tiny_skia::Pixmap::new(width, height).ok_or("image too large")?;
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
    canvas.encode_png().map_err(|e| e.to_string())
}

/// The picture on the clipboard, if it holds one: through `osascript` on
/// a Mac, `wl-paste` or `xclip` elsewhere.
pub fn clipboard() -> Result<Option<Picture>, String> {
    let png = if cfg!(target_os = "macos") {
        // AppleScript prints the PNG as «data PNGf…» in hex; an error is
        // a clipboard without one.
        let out = run("osascript", &["-e", "the clipboard as «class PNGf»"])?;
        let text = String::from_utf8_lossy(&out);
        match text.trim().strip_prefix("«data PNGf") {
            Some(hex) => Some(from_hex(hex.trim_end_matches('»'))?),
            None => None,
        }
    } else if std::env::var_os("WAYLAND_DISPLAY").is_some() {
        Some(run("wl-paste", &["--no-newline", "--type", "image/png"])?)
    } else {
        Some(run(
            "xclip",
            &["-selection", "clipboard", "-t", "image/png", "-o"],
        )?)
    };
    match png.filter(|png| png.starts_with(b"\x89PNG")) {
        Some(png) => picture("image/png", png).map(Some),
        None => Ok(None),
    }
}

/// What `program` printed, or nothing when it failed: a clipboard without
/// a picture.
fn run(program: &str, args: &[&str]) -> Result<Vec<u8>, String> {
    let out = std::process::Command::new(program)
        .args(args)
        .stdin(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .output()
        .map_err(|e| format!("{program}: {e}"))?;
    Ok(if out.status.success() {
        out.stdout
    } else {
        Vec::new()
    })
}

fn from_hex(hex: &str) -> Result<Vec<u8>, String> {
    (0..hex.len() / 2 * 2)
        .step_by(2)
        .map(|at| u8::from_str_radix(&hex[at..at + 2], 16))
        .collect::<Result<_, _>>()
        .map_err(|_| "the clipboard's picture could not be read".to_owned())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dropped_files_read_as_their_paths() {
        let dir = std::env::temp_dir().join(format!("sonny-attach-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let shot = dir.join("Screen Shot 1.png");
        std::fs::write(&shot, b"").unwrap();
        std::fs::write(dir.join("b.JPG"), b"").unwrap();
        std::fs::write(dir.join("notes.txt"), b"").unwrap();
        let escaped = format!("{}/Screen\\ Shot\\ 1.png ", dir.display());
        assert_eq!(paths(&escaped, &dir), Some(vec![shot.clone()]));
        let quoted = format!("'{}' \"b.JPG\"", shot.display());
        assert_eq!(
            paths(&quoted, &dir),
            Some(vec![shot.clone(), dir.join("b.JPG")])
        );
        let url = format!("file://{}/Screen%20Shot%201.png", dir.display());
        assert_eq!(paths(&url, &dir), Some(vec![shot]));
        // Words that are not all pictures there are text.
        assert_eq!(paths("notes.txt", &dir), None);
        assert_eq!(paths("b.JPG and more", &dir), None);
        assert_eq!(paths("gone.png", &dir), None);
        assert_eq!(paths("'b.JPG", &dir), None);
        assert_eq!(paths("  ", &dir), None);
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn a_large_png_is_scaled_to_fit_and_a_small_one_kept() {
        use resvg::tiny_skia;
        let png = |w, h| tiny_skia::Pixmap::new(w, h).unwrap().encode_png().unwrap();
        let small = png(800, 600);
        assert_eq!(fit(small.clone()).unwrap(), small);
        let fitted = tiny_skia::Pixmap::decode_png(&fit(png(3136, 1960)).unwrap()).unwrap();
        assert_eq!((fitted.width(), fitted.height()), (1568, 980));
        assert!(fit(b"not a png".to_vec()).is_err());
        assert!(picture("image/jpeg", vec![0; MOST_BYTES + 1]).is_err());
        // A PNG of noise can be too large even at 1568 pixels.
        let mut noise = tiny_skia::Pixmap::new(1568, 1200).unwrap();
        let mut state = 0x2545_f491_u32;
        for byte in noise.data_mut() {
            state ^= state << 13;
            state ^= state >> 17;
            state ^= state << 5;
            *byte = state as u8;
        }
        let noise = noise.encode_png().unwrap();
        assert!(noise.len() > MOST_BYTES);
        assert!(picture("image/png", noise).is_err());
    }

    #[test]
    fn hex_from_applescript_reads_as_bytes() {
        assert_eq!(from_hex("89504E47").unwrap(), b"\x89PNG");
        assert!(from_hex("zz").is_err());
    }
}
