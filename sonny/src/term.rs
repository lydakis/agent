//! What the terminal can do, asked at start: its background (OSC 11, or
//! the colour scheme it reports, mode 2031), so tints read on light and
//! dark; its theme's foreground, red and green (OSC 10 and 4), which
//! Sonny's own grays and tints start from; whether it draws Kitty
//! graphics; its cell size in pixels, so an image fills whole cells; and
//! whether it joins an emoji's parts into one character (mode 2027), as
//! a skin tone onto the hand it colours. The terminal's light and dark is
//! followed as it switches: by the colour-scheme reports it sends once
//! asked (mode 2031, as Ghostty, kitty, foot and Herdr do), each followed
//! by asking its colours again, which are asked again too as the window
//! comes to the front; and for a terminal on this Mac that sends none but
//! follows the Mac's appearance (Rex does), by the Mac's.
use std::{
    io::Write,
    sync::{
        Arc, LazyLock, Mutex, OnceLock,
        atomic::{AtomicBool, AtomicU32, Ordering},
    },
    time::{Duration, Instant},
};

struct Probe {
    graphics: bool,
    /// A cell's width and height in pixels, as answered at start.
    cell: (u32, u32),
}

static PROBE: OnceLock<Probe> = OnceLock::new();
/// The theme as the terminal said it: the one window's, read by every
/// frame.
static SEEN: Seen = Seen::new();
/// The terminal answered its background at start: its colors are asked
/// again as it switches and as the window comes to the front.
static ANSWERS: AtomicBool = AtomicBool::new(false);
/// What asks a terminal for its colors: background first, so its answer
/// says which background the rest belong to.
const ASK_COLORS: &str = "\x1b]11;?\x1b\\\x1b]10;?\x1b\\\x1b]4;1;?\x1b\\\x1b]4;2;?\x1b\\";
/// Sonny turned the terminal's grapheme clustering on, to be turned off
/// again as it leaves.
static CLUSTERED: AtomicBool = AtomicBool::new(false);

pub type Rgb = (u8, u8, u8);

/// The theme's own colors, each as the terminal last answered it on the
/// background it shows now; none it did not answer there.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct Theme {
    pub foreground: Option<Rgb>,
    pub background: Option<Rgb>,
    pub red: Option<Rgb>,
    pub green: Option<Rgb>,
}

const FOREGROUND: usize = 0;
const BACKGROUND: usize = 1;
const RED: usize = 2;
const GREEN: usize = 3;
const KNOWN: u32 = 1 << 24;

/// Whether the background is light, and the theme's colors as last
/// answered on a dark background and on a light one (foreground,
/// background, red, green; each `KNOWN | rgb`, 0 when not answered
/// there), so what one background answered is known again whenever the
/// terminal is back on it. Atomics: a frame reads them for each banded
/// cell.
struct Seen {
    light: AtomicBool,
    colors: [[AtomicU32; 4]; 2],
    /// The theme changed since [`Seen::switched`] was last asked.
    switched: AtomicBool,
}

impl Seen {
    const fn new() -> Seen {
        Seen {
            light: AtomicBool::new(true),
            colors: [const { [const { AtomicU32::new(0) }; 4] }; 2],
            switched: AtomicBool::new(false),
        }
    }

    fn light(&self) -> bool {
        self.light.load(Ordering::Relaxed)
    }

    fn theme(&self) -> Theme {
        let side = &self.colors[usize::from(self.light())];
        let color = |slot: usize| unpack(side[slot].load(Ordering::Relaxed));
        Theme {
            foreground: color(FOREGROUND),
            background: color(BACKGROUND),
            red: color(RED),
            green: color(GREEN),
        }
    }

    fn switched(&self) -> bool {
        self.switched.swap(false, Ordering::Relaxed)
    }

    /// The background is `light` now: a switch when it was not.
    fn switch(&self, light: bool) {
        if self.light.swap(light, Ordering::Relaxed) != light {
            self.switched.store(true, Ordering::Relaxed);
        }
    }

    /// `rgb` is the color in `slot` on the background `light`: a switch
    /// when it changed on the background shown now.
    fn keep(&self, light: bool, slot: usize, rgb: Option<Rgb>) {
        let before = self.colors[usize::from(light)][slot].swap(pack(rgb), Ordering::Relaxed);
        if before != pack(rgb) && light == self.light() {
            self.switched.store(true, Ordering::Relaxed);
        }
    }

    /// The theme the terminal answered at start, on a background `light`.
    fn start(&self, light: bool, theme: Theme) {
        self.light.store(light, Ordering::Relaxed);
        for (slot, rgb) in [
            (FOREGROUND, theme.foreground),
            (BACKGROUND, theme.background),
            (RED, theme.red),
            (GREEN, theme.green),
        ] {
            self.keep(light, slot, rgb);
        }
        self.switched.store(false, Ordering::Relaxed);
    }

    /// A terminal's answer as Sonny runs: a colour-scheme report switches
    /// the background, and is true as the colors that go with it are to be
    /// asked; a background's answer says light or dark and is kept with
    /// it; the foreground, red and green after it are kept with that one.
    fn heard(&self, reply: &str) -> bool {
        if let Some(light) = scheme(reply) {
            self.switch(light);
            return true;
        }
        let rgb = |answer: &str| {
            color(reply, answer).map(|(r, g, b)| {
                let byte = |v: f32| (v * 255.0).round() as u8;
                (luminance((r, g, b)) > 0.5, (byte(r), byte(g), byte(b)))
            })
        };
        if let Some((light, background)) = rgb("]11;") {
            self.switch(light);
            self.keep(light, BACKGROUND, Some(background));
        }
        for (answer, slot) in [("]10;", FOREGROUND), ("]4;1;", RED), ("]4;2;", GREEN)] {
            if let Some((_, color)) = rgb(answer) {
                self.keep(self.light(), slot, Some(color));
            }
        }
        false
    }
}

fn pack(rgb: Option<Rgb>) -> u32 {
    rgb.map_or(0, |(r, g, b)| {
        KNOWN | u32::from(r) << 16 | u32::from(g) << 8 | u32::from(b)
    })
}

fn unpack(packed: u32) -> Option<Rgb> {
    (packed & KNOWN != 0).then_some(((packed >> 16) as u8, (packed >> 8) as u8, packed as u8))
}

pub fn theme() -> Theme {
    SEEN.theme()
}

pub fn light() -> bool {
    SEEN.light()
}

/// Whether the theme changed since asked: its background between light
/// and dark, or a color it answered anew.
pub fn switched() -> bool {
    SEEN.switched()
}

/// A terminal's answer as Sonny runs (crossterm's `Event::Reply`), kept;
/// a colour-scheme report asks the colors that go with it.
pub fn heard(reply: &str) {
    if SEEN.heard(reply) {
        ask();
    }
}

/// Whether images can be drawn: Kitty graphics answered OK.
pub fn graphics() -> bool {
    PROBE.get().is_some_and(|p| p.graphics)
}

/// A cell's size in pixels: as the terminal gives it now with the window
/// size, else as it answered at start (Rex answers 8×16 for a block it has
/// not laid out yet, and the real size once it has).
pub fn cell() -> (u32, u32) {
    if let Ok(size) = size()
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
        // Read anew each time, as nothing here waits for the signal its
        // resize sends.
        resized();
        if size().is_ok_and(|size| size.width > 0) {
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
    let light = parse(&reply)
        .map(|rgb| luminance(rgb) > 0.5)
        .or_else(|| scheme(&reply))
        .or_else(colorfgbg)
        .or(mac)
        .unwrap_or(true);
    let theme = answered(&reply);
    ANSWERS.store(theme.background.is_some(), Ordering::Relaxed);
    SEEN.start(light, theme);
    let _ = PROBE.set(Probe {
        graphics: reply.contains("_Gi=31;OK"),
        cell: cell_size(&reply).unwrap_or((8, 16)),
    });
    // A terminal that can cluster but has it off draws 👍🏻 as 👍 and a
    // swatch, wider than Sonny lays it out, and the cell after it then
    // covers the swatch: turned on, as Zellij does, so an emoji takes the
    // cells Sonny gives it.
    if clustering(&reply) == Some(false) {
        let mut stdout = std::io::stdout();
        if write!(stdout, "\x1b[?2027h")
            .and_then(|()| stdout.flush())
            .is_ok()
        {
            CLUSTERED.store(true, Ordering::Relaxed);
        }
    }
    // A terminal on this Mac that matches its appearance now, and sends no
    // colour-scheme reports, is taken to follow it, as Rex can.
    if mac == Some(light)
        && scheme(&reply).is_none()
        && std::env::var_os("SSH_CONNECTION").is_none()
    {
        std::thread::spawn(|| {
            loop {
                std::thread::sleep(Duration::from_secs(3));
                if let Some(now) = appearance() {
                    SEEN.switch(now);
                }
            }
        });
    }
}

/// Ask the terminal for its colour-scheme reports (mode 2031), or stop:
/// on while Sonny has the screen, as a report is input. A terminal that
/// does not know the mode ignores it.
pub fn follow(on: bool) {
    let mut stdout = std::io::stdout();
    let _ = write!(stdout, "\x1b[?2031{}", if on { 'h' } else { 'l' });
    let _ = stdout.flush();
}

/// Ask the terminal for its colors again, when it answered them at start:
/// as the window comes to the front, where a terminal that sends no
/// colour-scheme reports may have switched meanwhile. The answers come as
/// input, read by [`heard`].
pub fn ask() {
    if !ANSWERS.load(Ordering::Relaxed) {
        return;
    }
    let mut stdout = std::io::stdout();
    let _ = stdout
        .write_all(ASK_COLORS.as_bytes())
        .and_then(|()| stdout.flush());
}

/// The window's size, in cells and in pixels.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Size {
    pub columns: u16,
    pub rows: u16,
    /// Its pixels, 0 by 0 where the terminal does not say them.
    pub width: u16,
    pub height: u16,
}

/// The window's size as last read, and whether it may have changed since.
struct Kept {
    size: Option<Size>,
}

impl Kept {
    /// The size kept, read again with `read` when there is none yet or it
    /// is `stale`. A read that fails keeps nothing, so the next reads again.
    fn get(
        &mut self,
        stale: bool,
        read: impl FnOnce() -> std::io::Result<Size>,
    ) -> std::io::Result<Size> {
        match self.size {
            Some(size) if !stale => Ok(size),
            _ => {
                self.size = None;
                let size = read()?;
                self.size = Some(size);
                Ok(size)
            }
        }
    }
}

static KEPT: Mutex<Kept> = Mutex::new(Kept { size: None });

/// Set when the window may have changed size since it was last read: by
/// the kernel's SIGWINCH as it resizes the terminal, by SIGCONT as the
/// process goes on after a stop (a resize meanwhile went to the shell),
/// and by [`resized`]. None when the signals could not be watched: the
/// size is then read every time.
static STALE: LazyLock<Option<Arc<AtomicBool>>> = LazyLock::new(|| {
    let stale = Arc::new(AtomicBool::new(true));
    #[cfg(unix)]
    for signal in [signal_hook::consts::SIGWINCH, signal_hook::consts::SIGCONT] {
        signal_hook::flag::register(signal, Arc::clone(&stale)).ok()?;
    }
    Some(stale)
});

/// The window's size, read from the terminal only once it may have
/// changed: crossterm opens /dev/tty for each read, and a frame took the
/// size five times, ten frames a second, in every window.
pub fn size() -> std::io::Result<Size> {
    let mut kept = KEPT.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
    // Taken before the read, so a resize during it is read next time.
    let stale = STALE
        .as_ref()
        .is_none_or(|stale| stale.swap(false, Ordering::AcqRel));
    kept.get(stale, read_size)
}

/// The window may have changed size: the next [`size`] reads it. For
/// what the signals do not cover, as a full repaint the person asked for.
pub fn resized() {
    if let Some(stale) = STALE.as_ref() {
        stale.store(true, Ordering::Release);
    }
}

/// The window's size as the terminal says it now: its cells and pixels
/// with one ioctl, else its cells as crossterm finds them otherwise.
fn read_size() -> std::io::Result<Size> {
    use ratatui::crossterm::terminal;
    match terminal::window_size() {
        Ok(size) if size.columns > 0 && size.rows > 0 => Ok(Size {
            columns: size.columns,
            rows: size.rows,
            width: size.width,
            height: size.height,
        }),
        _ => terminal::size().map(|(columns, rows)| Size {
            columns,
            rows,
            ..Size::default()
        }),
    }
}

/// The terminal as Sonny found it: grapheme clustering off again when
/// [`detect`] turned it on. Call on leaving, with the screen given back.
pub fn restore() {
    if CLUSTERED.swap(false, Ordering::Relaxed) {
        let mut stdout = std::io::stdout();
        let _ = write!(stdout, "\x1b[?2027l");
        let _ = stdout.flush();
    }
}

/// Whether the terminal's grapheme clustering (mode 2027) is on, as its
/// answer to DECRQM, `CSI ? 2027 ; Ps $ y`, says: none for a terminal
/// that does not know the mode or cannot change it.
fn clustering(reply: &str) -> Option<bool> {
    let at = reply.find("\x1b[?2027;")? + "\x1b[?2027;".len();
    match reply.get(at..)?.split_once("$y")?.0 {
        "1" => Some(true),
        "2" => Some(false),
        _ => None,
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

/// OSC 11 and 10, red and green (OSC 4), a Kitty graphics query, the
/// cell size and the colour scheme, whether emoji are clustered (DECRQM
/// 2027), then DA1, which every terminal answers, so the wait ends as
/// soon as the rest is in.
fn query() -> Option<String> {
    let mut stdout = std::io::stdout();
    write!(
        stdout,
        "\x1b]11;?\x1b\\\x1b]10;?\x1b\\\x1b]4;1;?\x1b\\\x1b]4;2;?\x1b\\\
         \x1b_Gi=31,s=1,v=1,a=q,t=d,f=24;AAAA\x1b\\\x1b[16t\x1b[?996n\x1b[?2027$p\x1b[c"
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
    fn the_size_is_read_again_only_once_it_may_have_changed() {
        let mut kept = Kept { size: None };
        let reads = std::cell::Cell::new(0);
        let read = |columns| {
            let reads = &reads;
            move || -> std::io::Result<Size> {
                reads.set(reads.get() + 1);
                Ok(Size {
                    columns,
                    rows: 24,
                    ..Size::default()
                })
            }
        };
        // None yet: read, though nothing said it changed.
        assert_eq!(kept.get(false, read(80)).unwrap().columns, 80);
        assert_eq!(kept.get(false, read(100)).unwrap().columns, 80);
        assert_eq!(reads.get(), 1);
        assert_eq!(kept.get(true, read(100)).unwrap().columns, 100);
        // A read that fails keeps nothing: the next one reads again.
        let failed = kept.get(true, || Err(std::io::Error::other("no tty")));
        assert!(failed.is_err());
        assert_eq!(kept.get(false, read(90)).unwrap().columns, 90);
        assert_eq!(reads.get(), 3);
    }

    #[test]
    fn the_theme_follows_what_the_terminal_reports_and_answers() {
        let seen = Seen::new();
        let dark = Theme {
            foreground: Some((0xce, 0xcd, 0xc3)),
            background: Some((0x10, 0x0f, 0x0f)),
            ..Theme::default()
        };
        seen.start(false, dark);
        assert!(!seen.switched());
        assert_eq!(seen.theme(), dark);
        // The terminal reports light: switched, its colors asked, and not
        // known there until it answers them.
        assert!(seen.heard("\x1b[?997;2n"));
        assert!(seen.light() && seen.switched());
        assert_eq!(seen.theme(), Theme::default());
        assert!(!seen.heard("\x1b]11;rgb:ffff/fcfc/f0f0\x1b\\"));
        seen.heard("\x1b]10;rgb:10/0f/0f\x07");
        assert!(seen.switched());
        assert_eq!(seen.theme().background, Some((0xff, 0xfc, 0xf0)));
        assert_eq!(seen.theme().foreground, Some((0x10, 0x0f, 0x0f)));
        // The same answer again changes nothing.
        seen.heard("\x1b]10;rgb:10/0f/0f\x07");
        assert!(!seen.switched());
        // Back to dark by the background's own answer, as asked when the
        // window comes to the front: what dark answered before holds again.
        seen.heard("\x1b]11;rgb:1010/0f0f/0f0f\x1b\\");
        assert!(!seen.light() && seen.switched());
        assert_eq!(seen.theme(), dark);
        // A report that names neither is no switch.
        seen.heard("\x1b[?997;3n");
        assert!(!seen.switched());
    }

    #[test]
    fn a_color_packs_and_unpacks() {
        for rgb in [
            None,
            Some((0, 0, 0)),
            Some((255, 255, 255)),
            Some((1, 2, 3)),
        ] {
            assert_eq!(unpack(pack(rgb)), rgb);
        }
    }

    #[test]
    fn grapheme_clustering_reads_as_on_off_or_unknown() {
        let reply = |ps: &str| format!("\x1b[?997;1n\x1b[?2027;{ps}$y\x1b[?62;22c");
        assert_eq!(clustering(&reply("1")), Some(true));
        assert_eq!(clustering(&reply("2")), Some(false));
        // Not known, or set for good either way: nothing to change.
        for ps in ["0", "3", "4"] {
            assert_eq!(clustering(&reply(ps)), None, "{ps}");
        }
        assert_eq!(clustering("\x1b[?2027;2"), None);
    }

    #[test]
    fn a_background_reads_as_light_or_dark() {
        let light = parse("\x1b]11;rgb:fdfd/f9f9/eded\x1b\\\x1b[?62;22c").unwrap();
        assert!(luminance(light) > 0.5);
        let dark = parse("\x1b]11;rgb:1e/1e/2e\x07").unwrap();
        assert!(luminance(dark) < 0.5);
        assert!(parse("\x1b[?62;22c").is_none());
        let reply = "\x1b]11;rgb:1e1e/1e1e/2e2e\x1b\\\x1b]10;rgb:cd/d6/f4\x07\
                     \x1b]4;1;rgb:f3f3/8b8b/a8a8\x1b\\\x1b[?62;22c";
        assert_eq!(clustering(reply), None);
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
