//! Light or dark: the terminal's background, asked once at start (OSC 11),
//! so tints read on either. `SONNY_THEME=light|dark` decides instead.
use std::{
    io::Write,
    sync::OnceLock,
    time::{Duration, Instant},
};

static LIGHT: OnceLock<bool> = OnceLock::new();

pub fn light() -> bool {
    *LIGHT.get().unwrap_or(&false)
}

/// Ask the terminal for its background. Call in raw mode, before anything
/// else reads input: the answer arrives on stdin.
pub fn detect() {
    let light = match std::env::var("SONNY_THEME").as_deref() {
        Ok("light") => true,
        Ok("dark") => false,
        _ => query().is_some_and(|rgb| luminance(rgb) > 0.5),
    };
    let _ = LIGHT.set(light);
}

/// OSC 11, then DA1 so a terminal that ignores OSC 11 still answers and
/// the wait ends early.
fn query() -> Option<(f32, f32, f32)> {
    let mut stdout = std::io::stdout();
    write!(stdout, "\x1b]11;?\x1b\\\x1b[c").ok()?;
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
    parse(&String::from_utf8_lossy(&got))
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
    }
}
