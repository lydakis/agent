//! What the terminal can do, asked at start: its background (OSC 11, or
//! the colour scheme it reports, mode 2031), so tints read on light and
//! dark; its theme's foreground, red and green (OSC 10 and 4), which
//! Sonny's own grays and tints start from; whether it draws Kitty
//! graphics; and its cell size in pixels, so an image fills whole cells. A terminal that follows the Mac's light and
//! dark appearance (Rex does) is followed as it switches.
use std::{
    io::Write,
    sync::{
        OnceLock,
        atomic::{AtomicBool, Ordering},
    },
    time::{Duration, Instant},
};

struct Probe {
    graphics: bool,
    /// A cell's width and height in pixels, as answered at start.
    cell: (u32, u32),
}

static PROBE: OnceLock<Probe> = OnceLock::new();
static LIGHT: AtomicBool = AtomicBool::new(true);
/// The background changed since [`switched`] was last asked.
static SWITCHED: AtomicBool = AtomicBool::new(false);
static THEME: OnceLock<Theme> = OnceLock::new();
/// The theme changed since it was asked, so its colors are not known.
static STALE: AtomicBool = AtomicBool::new(false);

pub type Rgb = (u8, u8, u8);

/// The theme's own colors, each as the terminal answered at start; none
/// it did not answer, and none at all once the theme has switched.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct Theme {
    pub foreground: Option<Rgb>,
    pub background: Option<Rgb>,
    pub red: Option<Rgb>,
    pub green: Option<Rgb>,
}

pub fn theme() -> Theme {
    match STALE.load(Ordering::Relaxed) {
        true => Theme::default(),
        false => THEME.get().copied().unwrap_or_default(),
    }
}

pub fn light() -> bool {
    LIGHT.load(Ordering::Relaxed)
}

/// Whether the background switched between light and dark since asked.
pub fn switched() -> bool {
    SWITCHED.swap(false, Ordering::Relaxed)
}

/// Whether images can be drawn: Kitty graphics answered OK.
pub fn graphics() -> bool {
    PROBE.get().is_some_and(|p| p.graphics)
}

/// A cell's size in pixels: as the terminal gives it now with the window
/// size, else as it answered at start (Rex answers 8×16 for a block it has
/// not laid out yet, and the real size once it has).
pub fn cell() -> (u32, u32) {
    if let Ok(size) = ratatui::crossterm::terminal::window_size()
        && size.columns > 0
        && size.rows > 0
        && size.width >= size.columns
        && size.height >= size.rows
    {
        return (
            (size.width / size.columns) as u32,
            (size.height / size.rows) as u32,
        );
    }
    PROBE.get().map_or((8, 16), |p| p.cell)
}

/// Wait, up to two seconds, for the pane's size in pixels, which Rex
/// fills in only once it has laid a new block out (80×24 and no pixels
/// until then): pictures drawn before it are sized for a pane that is not
/// there.
pub async fn laid_out() {
    if !graphics() {
        return;
    }
    let deadline = Instant::now() + Duration::from_secs(2);
    while Instant::now() < deadline {
        if ratatui::crossterm::terminal::window_size().is_ok_and(|size| size.width > 0) {
            return;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

/// Ask the terminal. Call in raw mode, before anything else reads input:
/// the answers arrive on stdin.
pub fn detect() {
    let reply = query().unwrap_or_default();
    let mac = appearance();
    let _ = THEME.set(answered(&reply));
    let light = parse(&reply)
        .map(|rgb| luminance(rgb) > 0.5)
        .or_else(|| scheme(&reply))
        .or_else(colorfgbg)
        .or(mac)
        .unwrap_or(true);
    LIGHT.store(light, Ordering::Relaxed);
    let _ = PROBE.set(Probe {
        graphics: reply.contains("_Gi=31;OK"),
        cell: cell_size(&reply).unwrap_or((8, 16)),
    });
    // A terminal on this Mac that matches its appearance now is taken to
    // follow it, as Rex and Ghostty can.
    if mac == Some(light) && std::env::var_os("SSH_CONNECTION").is_none() {
        std::thread::spawn(|| {
            loop {
                std::thread::sleep(Duration::from_secs(3));
                if let Some(now) = appearance()
                    && LIGHT.swap(now, Ordering::Relaxed) != now
                {
                    STALE.store(true, Ordering::Relaxed);
                    SWITCHED.store(true, Ordering::Relaxed);
                }
            }
        });
    }
}

/// The Mac's appearance: light unless `AppleInterfaceStyle` says Dark.
#[cfg(target_os = "macos")]
fn appearance() -> Option<bool> {
    Some(!crate::mac::dark())
}

#[cfg(not(target_os = "macos"))]
fn appearance() -> Option<bool> {
    None
}

/// The colour scheme a terminal reports (mode 2031): `CSI ? 997 ; 1 n`
/// is dark, `2` light.
fn scheme(reply: &str) -> Option<bool> {
    let at = reply.find("\x1b[?997;")? + "\x1b[?997;".len();
    match reply.as_bytes().get(at)? {
        b'1' => Some(false),
        b'2' => Some(true),
        _ => None,
    }
}

/// `COLORFGBG` (`15;0` is light on dark), when set.
fn colorfgbg() -> Option<bool> {
    let colours = std::env::var("COLORFGBG").ok()?;
    let background = colours.rsplit(';').next()?.parse::<u8>().ok()?;
    Some(matches!(background, 7 | 15))
}

/// OSC 11 and 10, red and green (OSC 4), a Kitty graphics query and the
/// cell size, then DA1, which every terminal answers, so the wait ends as
/// soon as the rest is in.
fn query() -> Option<String> {
    let mut stdout = std::io::stdout();
    write!(
        stdout,
        "\x1b]11;?\x1b\\\x1b]10;?\x1b\\\x1b]4;1;?\x1b\\\x1b]4;2;?\x1b\\\
         \x1b_Gi=31,s=1,v=1,a=q,t=d,f=24;AAAA\x1b\\\x1b[16t\x1b[?996n\x1b[c"
    )
    .ok()?;
    stdout.flush().ok()?;
    let deadline = Instant::now() + Duration::from_millis(300);
    let mut got = Vec::new();
    loop {
        let left = deadline.saturating_duration_since(Instant::now());
        if left.is_zero() {
            break;
        }
        let mut poll = libc::pollfd {
            fd: 0,
            events: libc::POLLIN,
            revents: 0,
        };
        // SAFETY: one valid pollfd, and its count is 1.
        let ready = unsafe { libc::poll(&mut poll, 1, left.as_millis() as libc::c_int) };
        if ready <= 0 {
            break;
        }
        let mut buf = [0u8; 256];
        // SAFETY: `buf` is writable for its whole length.
        let n = unsafe { libc::read(0, buf.as_mut_ptr().cast(), buf.len()) };
        if n <= 0 {
            break;
        }
        got.extend_from_slice(&buf[..n as usize]);
        // DA1's answer, `ESC [ ? … c`, comes last.
        if let Some(at) = find(&got, b"\x1b[?")
            && got[at..].contains(&b'c')
        {
            break;
        }
    }
    Some(String::from_utf8_lossy(&got).into_owned())
}

/// `ESC [ 6 ; height ; width t`.
fn cell_size(reply: &str) -> Option<(u32, u32)> {
    let rest = reply.split("\x1b[6;").nth(1)?;
    let (sizes, _) = rest.split_once('t')?;
    let (height, width) = sizes.split_once(';')?;
    let (width, height) = (width.parse().ok()?, height.parse().ok()?);
    (width > 0 && height > 0).then_some((width, height))
}

fn find(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    haystack.windows(needle.len()).position(|w| w == needle)
}

/// `ESC ] 11 ; rgb:RRRR/GGGG/BBBB` (each 1 to 4 hex digits) as 0..1.
fn parse(reply: &str) -> Option<(f32, f32, f32)> {
    color(reply, "]11;")
}

/// The colors the reply names.
fn answered(reply: &str) -> Theme {
    let rgb = |answer: &str| {
        color(reply, answer).map(|(r, g, b)| {
            let byte = |v: f32| (v * 255.0).round() as u8;
            (byte(r), byte(g), byte(b))
        })
    };
    Theme {
        foreground: rgb("]10;"),
        background: rgb("]11;"),
        red: rgb("]4;1;"),
        green: rgb("]4;2;"),
    }
}

/// The color an OSC answer starting `answer` names, as 0..1.
fn color(reply: &str, answer: &str) -> Option<(f32, f32, f32)> {
    let rgb = reply.split(answer).nth(1)?.strip_prefix("rgb:")?;
    let mut parts = rgb.split('/').map(|part| {
        let hex: String = part.chars().take_while(char::is_ascii_hexdigit).collect();
        let max = (16u32.pow(hex.len() as u32) - 1) as f32;
        u32::from_str_radix(&hex, 16).ok().map(|v| v as f32 / max)
    });
    Some((parts.next()??, parts.next()??, parts.next()??))
}

fn luminance((r, g, b): (f32, f32, f32)) -> f32 {
    0.2126 * r + 0.7152 * g + 0.0722 * b
}

/// Put `text` on the clipboard: through `pbcopy` on the Mac Sonny runs on,
/// else OSC 52, which the terminal answers wherever it is.
pub fn copy(text: &str) {
    if cfg!(target_os = "macos")
        && std::env::var_os("SSH_CONNECTION").is_none()
        && let Ok(mut child) = std::process::Command::new("/usr/bin/pbcopy")
            .stdin(std::process::Stdio::piped())
            .spawn()
    {
        if let Some(mut stdin) = child.stdin.take() {
            let _ = stdin.write_all(text.as_bytes());
        }
        let _ = child.wait();
        return;
    }
    use base64::Engine;
    let encoded = base64::engine::general_purpose::STANDARD.encode(text);
    let mut stdout = std::io::stdout();
    let _ = write!(stdout, "\x1b]52;c;{encoded}\x07");
    let _ = stdout.flush();
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_background_reads_as_light_or_dark() {
        let light = parse("\x1b]11;rgb:fdfd/f9f9/eded\x1b\\\x1b[?62;22c").unwrap();
        assert!(luminance(light) > 0.5);
        let dark = parse("\x1b]11;rgb:1e/1e/2e\x07").unwrap();
        assert!(luminance(dark) < 0.5);
        assert!(parse("\x1b[?62;22c").is_none());
        let reply = "\x1b]11;rgb:1e1e/1e1e/2e2e\x1b\\\x1b]10;rgb:cd/d6/f4\x07\
                     \x1b]4;1;rgb:f3f3/8b8b/a8a8\x1b\\\x1b[?62;22c";
        assert_eq!(
            answered(reply),
            Theme {
                foreground: Some((0xcd, 0xd6, 0xf4)),
                background: Some((0x1e, 0x1e, 0x2e)),
                red: Some((0xf3, 0x8b, 0xa8)),
                green: None,
            }
        );
        assert_eq!(
            cell_size("\x1b_Gi=31;OK\x1b\\\x1b[6;18;9t\x1b[?62;22c"),
            Some((9, 18))
        );
    }
}
