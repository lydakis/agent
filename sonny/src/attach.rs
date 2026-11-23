//! Pictures and files the person gives the agent with a message: a
//! picture on the clipboard, taken with Ctrl-V as Codex and Claude Code
//! take it, or files whose paths are pasted or dropped on the terminal. A
//! picture is kept until the message goes, behind a placeholder in the
//! composer; another file is its path, which the agent reads. Over SSH a
//! dropped file is on the machine the person is at, not this one: a
//! machine linked here (`sonny link`) hands it over, and its clipboard's
//! picture.
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

/// The pictures models take in another form: a phone's photos (HEIC,
/// HEIF), what the Photos app and cmux save of a dropped image (those, TIFF,
/// BMP), AVIF. A Mac reads them all with `sips`, which makes each a JPEG
/// (a photo) or a PNG to send.
const CONVERTED: [(&str, &str); 7] = [
    ("heic", "jpeg"),
    ("heif", "jpeg"),
    ("avif", "jpeg"),
    ("tif", "png"),
    ("tiff", "png"),
    ("bmp", "png"),
    ("jxl", "jpeg"),
];

/// What `sips` makes of a picture models do not take as it is.
fn converted_to(path: &Path) -> Option<&'static str> {
    let ext = path.extension()?.to_str()?.to_ascii_lowercase();
    (CONVERTED.iter()).find_map(|(from, to)| (*from == ext).then_some(*to))
}

/// Whether a file is a picture: one models take, or one a Mac converts.
pub fn is_picture(path: &Path) -> bool {
    media_type(path).is_some() || converted_to(path).is_some()
}

/// A picture as it crosses a link: `{"media_type", "data"}`, its bytes
/// in base64.
pub fn to_json(picture: &Picture) -> serde_json::Value {
    use base64::Engine;
    serde_json::json!({"media_type": picture.media_type,
        "data": base64::engine::general_purpose::STANDARD.encode(&picture.data)})
}

/// A picture that came over a link, checked as one read here is.
pub fn from_json(value: &serde_json::Value) -> Result<Picture, String> {
    use base64::Engine;
    let media_type = ["image/png", "image/jpeg", "image/gif", "image/webp"]
        .into_iter()
        .find(|known| value["media_type"].as_str() == Some(known))
        .ok_or("not a picture models take")?;
    let data = (base64::engine::general_purpose::STANDARD)
        .decode(value["data"].as_str().unwrap_or_default())
        .map_err(|e| e.to_string())?;
    picture(media_type, data)
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

/// A composer's words and the pictures behind their placeholders, as a
/// window kept for the window it becomes after an update: one JSON
/// object, `{"text", "pictures"}`, each picture in its place (`null` for
/// one the words no longer show). Read by the next release, so it keeps
/// this shape.
pub fn draft(text: &str, pictures: &[Option<Picture>]) -> String {
    use base64::Engine;
    let pictures = (pictures.iter().enumerate())
        .map(|(at, picture)| match picture {
            Some(p) if text.contains(&placeholder(at + 1)) => serde_json::json!({
                "media_type": p.media_type,
                "data": base64::engine::general_purpose::STANDARD.encode(&p.data),
            }),
            _ => serde_json::Value::Null,
        })
        .collect::<Vec<_>>();
    serde_json::json!({"text": text, "pictures": pictures}).to_string()
}

/// What [`draft`] kept: the words, and the pictures in their places.
pub fn undraft(kept: &str) -> (String, Vec<Option<Picture>>) {
    use base64::Engine;
    let Ok(value) = serde_json::from_str::<serde_json::Value>(kept) else {
        return (kept.to_owned(), Vec::new());
    };
    let text = value["text"].as_str().unwrap_or_default().to_owned();
    let pictures = (value["pictures"].as_array().into_iter().flatten())
        .map(|picture| {
            let media_type = ["image/png", "image/jpeg", "image/gif", "image/webp"]
                .into_iter()
                .find(|known| picture["media_type"].as_str() == Some(known))?;
            let data = (base64::engine::general_purpose::STANDARD)
                .decode(picture["data"].as_str()?)
                .ok()?;
            Some(Picture { media_type, data })
        })
        .collect();
    (text, pictures)
}

/// A word of a paste a terminal pastes for files dropped on it.
#[derive(Clone, Debug, PartialEq)]
pub enum Dropped {
    /// A picture here.
    Picture(PathBuf),
    /// Another file or a folder here: the agent reads it at its path.
    Here(PathBuf),
    /// A file at this path on another machine: the one the person is at,
    /// when this one is reached over SSH.
    Elsewhere(String),
    /// A file here that cannot be looked at, and why: on a Mac, most often
    /// a folder the terminal was not let into (Downloads, Desktop).
    Unread(String, String),
}

/// The files a paste names, when every word of it names one: what a
/// terminal pastes for files dropped on it, with spaces escaped or quoted,
/// or as `file://` addresses. A whole path here not on this machine is a
/// file elsewhere. None when the paste is all files here none of them a
/// picture, which is text as it was pasted.
pub fn dropped(pasted: &str, workspace: &Path) -> Option<Vec<Dropped>> {
    let words = shell_words(pasted.trim())?;
    if words.is_empty() {
        return None;
    }
    let dropped = (words.into_iter())
        .map(|word| {
            let word = match word.strip_prefix("file://") {
                Some(rest) => unescape_url(rest.strip_prefix("localhost").unwrap_or(rest))?,
                None => word,
            };
            let path = match word.strip_prefix("~/") {
                Some(rest) => PathBuf::from(std::env::var_os("HOME")?).join(rest),
                None => workspace.join(&word),
            };
            Some(match std::fs::metadata(&path) {
                Ok(found) if found.is_file() && is_picture(&path) => Dropped::Picture(path),
                Ok(_) => Dropped::Here(path),
                Err(_) if !word.starts_with('/') => return None,
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => Dropped::Elsewhere(word),
                Err(e) => Dropped::Unread(word, e.to_string()),
            })
        })
        .collect::<Option<Vec<_>>>()?;
    let any = (dropped.iter()).any(|word| !matches!(word, Dropped::Here(_)));
    any.then_some(dropped)
}

/// `path` as a shell would read it back as one word: as it is when it
/// holds nothing a shell splits or reads, else in single quotes.
pub fn shell_quoted(path: &str) -> String {
    let plain = (path.chars()).all(|c| c.is_alphanumeric() || "/._-+,:@%=~".contains(c));
    match plain && !path.is_empty() {
        true => path.to_owned(),
        false => format!("'{}'", path.replace('\'', "'\\''")),
    }
}

/// `text` split as a shell splits words: at unquoted spaces, tabs and
/// line breaks (its field separators, not every Unicode space: a Mac's
/// screenshot is named with a narrow no-break space before its AM or
/// PM), with backslash escapes and quotes taken off. None for an
/// unclosed quote.
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
            ' ' | '\t' | '\n' | '\r' => words.extend(word.take()),
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

/// The picture in an image file, a PNG scaled to fit; one models do not
/// take as it is, converted on a Mac.
pub fn load(path: &Path) -> Result<Picture, String> {
    let name = path.display();
    let Some(media_type) = media_type(path) else {
        return converted(path).map_err(|e| format!("{name}: {e}"));
    };
    let data = std::fs::read(path).map_err(|e| format!("{name}: {e}"))?;
    picture(media_type, data).map_err(|e| format!("{name}: {e}"))
}

/// A picture models do not take as it is, made a JPEG or PNG no longer
/// on its longest side than [`LONGEST`] by the Mac's own `sips`.
fn converted(path: &Path) -> Result<Picture, String> {
    let to = converted_to(path).ok_or("not an image")?;
    if !cfg!(target_os = "macos") {
        let ext = path.extension().and_then(|e| e.to_str()).unwrap_or("this");
        return Err(format!(
            "{ext} pictures are read on a Mac, and this is not one"
        ));
    }
    let dir = std::env::temp_dir().join(format!("sonny-picture-{}", nonce()));
    std::fs::create_dir_all(&dir).map_err(|e| e.to_string())?;
    let out = dir.join(format!("picture.{to}"));
    let made = std::process::Command::new("/usr/bin/sips")
        .args(["-s", "format", to, "-Z", &LONGEST.to_string()])
        .arg(path)
        .arg("--out")
        .arg(&out)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status();
    let data = std::fs::read(&out);
    let _ = std::fs::remove_dir_all(&dir);
    match made {
        Ok(status) if status.success() => {}
        Ok(_) => return Err("sips could not read it as a picture".into()),
        Err(e) => return Err(format!("sips: {e}")),
    }
    let data = data.map_err(|e| e.to_string())?;
    picture(
        if to == "png" {
            "image/png"
        } else {
            "image/jpeg"
        },
        data,
    )
}

/// Sixteen random hex digits: a name no other picture or file has.
pub fn nonce() -> String {
    use std::io::Read;
    let mut bytes = [0u8; 8];
    if let Ok(mut random) = std::fs::File::open("/dev/urandom") {
        let _ = random.read_exact(&mut bytes);
    }
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

pub(crate) fn picture(media_type: &'static str, data: Vec<u8>) -> Result<Picture, String> {
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

/// The picture on the clipboard of the machine the person is at: one
/// linked here when this one is reached over SSH, else this one's.
pub fn pasted() -> Result<Option<Picture>, String> {
    match over_ssh() && crate::link::handing() {
        true => crate::link::clipboard(),
        false => clipboard(),
    }
}

/// Whether the person reaches this machine over SSH, so the keyboard,
/// clipboard and files they drop are on another.
pub fn over_ssh() -> bool {
    ["SSH_CONNECTION", "SSH_TTY"]
        .iter()
        .any(|name| std::env::var_os(name).is_some_and(|v| !v.is_empty()))
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
    fn a_draft_keeps_its_words_and_the_pictures_they_show() {
        let png = Picture {
            media_type: "image/png",
            data: vec![1, 2, 3],
        };
        let jpeg = Picture {
            media_type: "image/jpeg",
            data: vec![4],
        };
        let pictures = [Some(png.clone()), Some(jpeg), None];
        // The second picture's placeholder was deleted: it is let go.
        let kept = draft("look at [Image #1]", &pictures);
        let (text, back) = undraft(&kept);
        assert_eq!(text, "look at [Image #1]");
        assert_eq!(back, vec![Some(png), None, None]);
    }

    #[test]
    fn dropped_files_read_as_their_paths() {
        let dir = std::env::temp_dir().join(format!("sonny-attach-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let shot = dir.join("Screen Shot 1.png");
        std::fs::write(&shot, b"").unwrap();
        std::fs::write(dir.join("b.JPG"), b"").unwrap();
        std::fs::write(dir.join("notes.txt"), b"").unwrap();
        let picture = |path: &PathBuf| Dropped::Picture(path.clone());
        let escaped = format!("{}/Screen\\ Shot\\ 1.png ", dir.display());
        assert_eq!(dropped(&escaped, &dir), Some(vec![picture(&shot)]));
        let quoted = format!("'{}' \"b.JPG\"", shot.display());
        assert_eq!(
            dropped(&quoted, &dir),
            Some(vec![picture(&shot), picture(&dir.join("b.JPG"))])
        );
        let url = format!("file://{}/Screen%20Shot%201.png", dir.display());
        assert_eq!(dropped(&url, &dir), Some(vec![picture(&shot)]));
        // A Mac names a screenshot with a narrow no-break space before
        // its PM, which a terminal drops unescaped: part of the name.
        let mac = dir.join("Screenshot 2026-11-04 at 10.15.32\u{202f}PM.png");
        std::fs::write(&mac, b"").unwrap();
        let shot_dropped = format!(
            "{}/Screenshot\\ 2026-11-04\\ at\\ 10.15.32\u{202f}PM.png",
            dir.display()
        );
        assert_eq!(dropped(&shot_dropped, &dir), Some(vec![picture(&mac)]));
        let two = format!("{shot_dropped}\n{}\r\n", dir.join("b.JPG").display());
        assert_eq!(
            dropped(&two, &dir),
            Some(vec![picture(&mac), picture(&dir.join("b.JPG"))])
        );
        // cmux escapes parentheses, as a Downloads copy's name has, and
        // pastes each of several pictures on its own after a space.
        let copy = dir.join("image (1).png");
        std::fs::write(&copy, b"").unwrap();
        let escaped = format!(" {}/image\\ \\(1\\).png", dir.display());
        assert_eq!(dropped(&escaped, &dir), Some(vec![picture(&copy)]));
        // Photos as a phone and the Photos app save them: pictures too.
        let photo = dir.join("IMG_0001.HEIC");
        std::fs::write(&photo, b"").unwrap();
        assert_eq!(dropped("IMG_0001.HEIC", &dir), Some(vec![picture(&photo)]));
        // With a picture, another file is its path.
        let mixed = format!("'{}' notes.txt", shot.display());
        assert_eq!(
            dropped(&mixed, &dir),
            Some(vec![picture(&shot), Dropped::Here(dir.join("notes.txt"))])
        );
        // A whole path not here names a file on the machine the person is
        // at, when this one is reached over SSH.
        let mine = "/Users/someone/Downloads/report final.pdf";
        let pasted = "/Users/someone/Downloads/report\\ final.pdf";
        assert_eq!(
            dropped(pasted, &dir),
            Some(vec![Dropped::Elsewhere(mine.into())])
        );
        // Files here none of them a picture, and words, are text.
        assert_eq!(dropped("notes.txt", &dir), None);
        assert_eq!(dropped("b.JPG and more", &dir), None);
        assert_eq!(dropped("gone.png", &dir), None);
        assert_eq!(dropped("'b.JPG", &dir), None);
        assert_eq!(dropped("  ", &dir), None);
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn a_path_goes_back_as_one_word() {
        assert_eq!(shell_quoted("/tmp/a.png"), "/tmp/a.png");
        assert_eq!(shell_quoted("/tmp/a b.pdf"), "'/tmp/a b.pdf'");
        assert_eq!(shell_quoted("/tmp/it's.pdf"), "'/tmp/it'\\''s.pdf'");
        let words = shell_words(&shell_quoted("/tmp/it's (1).pdf")).unwrap();
        assert_eq!(words, ["/tmp/it's (1).pdf"]);
    }

    #[test]
    fn a_picture_crosses_a_link_as_it_is_read_here() {
        let png = resvg::tiny_skia::Pixmap::new(4, 4)
            .unwrap()
            .encode_png()
            .unwrap();
        let picture = Picture {
            media_type: "image/png",
            data: png,
        };
        assert_eq!(from_json(&to_json(&picture)).unwrap(), picture);
        let svg = serde_json::json!({"media_type": "image/svg+xml", "data": ""});
        assert!(from_json(&svg).is_err());
    }

    #[test]
    fn a_picture_models_do_not_take_is_converted_on_a_mac() {
        assert!(is_picture(Path::new("a.heic")) && is_picture(Path::new("b.TIFF")));
        assert!(!is_picture(Path::new("c.pdf")));
        assert_eq!(converted_to(Path::new("a.HEIC")), Some("jpeg"));
        assert_eq!(converted_to(Path::new("b.bmp")), Some("png"));
        if !cfg!(target_os = "macos") {
            let said = load(Path::new("/nowhere/a.heic")).unwrap_err();
            assert!(said.contains("Mac"), "{said}");
        }
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
