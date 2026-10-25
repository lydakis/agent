//! What the terminal can do, asked once at start: its background (OSC 11),
//! so tints read on light and dark; whether it draws Kitty graphics; and
//! its cell size in pixels, so an image fills whole cells.
//! `SONNY_THEME=light|dark` decides the background instead.
use std::{
    io::Write,
    sync::OnceLock,
    time::{Duration, Instant},
};

struct Probe {
    light: bool,
    graphics: bool,
    /// A cell's width and height in pixels.
    cell: (u32, u32),
}

static PROBE: OnceLock<Probe> = OnceLock::new();

pub fn light() -> bool {
    PROBE.get().is_some_and(|p| p.light)
}

/// Whether images can be drawn: Kitty graphics answered OK.
pub fn graphics() -> bool {
    PROBE.get().is_some_and(|p| p.graphics)
}

pub fn cell() -> (u32, u32) {
    PROBE.get().map_or((8, 16), |p| p.cell)
}

/// Ask the terminal. Call in raw mode, before anything else reads input:
/// the answers arrive on stdin.
pub fn detect() {
    let reply = query().unwrap_or_default();
    let light = match std::env::var("SONNY_THEME").as_deref() {
        Ok("light") => true,
        Ok("dark") => false,
        _ => parse(&reply).is_some_and(|rgb| luminance(rgb) > 0.5),
    };
    let _ = PROBE.set(Probe {
        light,
        graphics: reply.contains("_Gi=31;OK"),
        cell: cell_size(&reply).unwrap_or((8, 16)),
    });
}

/// OSC 11, a Kitty graphics query and the cell size, then DA1, which every
/// terminal answers, so the wait ends as soon as the rest is in.
fn query() -> Option<String> {
    let mut stdout = std::io::stdout();
    write!(
        stdout,
        "\x1b]11;?\x1b\\\x1b_Gi=31,s=1,v=1,a=q,t=d,f=24;AAAA\x1b\\\x1b[16t\x1b[c"
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
    let rgb = reply.split("]11;rgb:").nth(1)?;
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
        assert_eq!(
            cell_size("\x1b_Gi=31;OK\x1b\\\x1b[6;18;9t\x1b[?62;22c"),
            Some((9, 18))
        );
    }
}
