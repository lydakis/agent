//! A program run on a terminal of Sonny's own and drawn in a pane: the
//! side chat's Sonny, beside the conversation instead of over it. Its
//! output is read as a terminal reads it (vt100), and keys, pastes and
//! the mouse go to it as a terminal would send them. What it asks the
//! terminal at start (the background, the cursor, which terminal this
//! is) is answered here, as the terminal Sonny runs in would. What it
//! copies (OSC 52) is kept for Sonny to put on the clipboard, as the
//! terminal Sonny runs in would have.
use std::io::{Read, Write};
use std::sync::{Arc, Mutex};

use portable_pty::{CommandBuilder, MasterPty, PtySize};
use ratatui::{
    buffer::Buffer,
    crossterm::event::{KeyCode, KeyEvent, KeyModifiers, MouseButton, MouseEvent, MouseEventKind},
    layout::Rect,
    style::{Color, Modifier, Style},
};

pub struct Embedded {
    parser: Arc<Mutex<vt100::Parser>>,
    writer: Arc<Mutex<Box<dyn Write + Send>>>,
    master: Box<dyn MasterPty + Send>,
    child: Box<dyn portable_pty::Child + Send + Sync>,
    size: (u16, u16),
    copied: Arc<Mutex<Vec<String>>>,
}

impl Embedded {
    /// `command` started on a terminal `rows` by `cols`, its screen read
    /// on a thread that calls `drawn` after each read. `light` is the
    /// background Sonny's own terminal has, told to it when it asks.
    pub fn spawn(
        command: CommandBuilder,
        rows: u16,
        cols: u16,
        light: bool,
        drawn: impl Fn() + Send + 'static,
    ) -> Result<Self, String> {
        let (rows, cols) = (rows.max(2), cols.max(10));
        let pty = portable_pty::native_pty_system()
            .openpty(PtySize {
                rows,
                cols,
                pixel_width: 0,
                pixel_height: 0,
            })
            .map_err(|e| e.to_string())?;
        let child = pty
            .slave
            .spawn_command(command)
            .map_err(|e| e.to_string())?;
        drop(pty.slave);
        let mut reader = pty.master.try_clone_reader().map_err(|e| e.to_string())?;
        let writer = Arc::new(Mutex::new(
            pty.master.take_writer().map_err(|e| e.to_string())?,
        ));
        let parser = Arc::new(Mutex::new(vt100::Parser::new(rows, cols, 0)));
        let copied = Arc::new(Mutex::new(Vec::new()));
        let (screen, replies, copies) = (parser.clone(), writer.clone(), copied.clone());
        std::thread::spawn(move || {
            let mut buf = [0u8; 16 * 1024];
            // The start of a question a read cut off, kept for the next.
            let mut held = Vec::new();
            while let Ok(n) = reader.read(&mut buf) {
                if n == 0 {
                    break;
                }
                let Ok(mut parser) = screen.lock() else {
                    break;
                };
                let mut read = std::mem::take(&mut held);
                read.extend_from_slice(&buf[..n]);
                let (cut, rest) = pieces(&read);
                let mut answers = Vec::new();
                for piece in cut {
                    match piece {
                        Piece::Output(bytes) => parser.process(bytes),
                        Piece::Asked(query) => answers.push(answer(query, &parser, light)),
                        Piece::Copied(payload) => {
                            if let Some(text) = copy_text(payload)
                                && let Ok(mut copies) = copies.lock()
                            {
                                copies.push(text);
                            }
                        }
                    }
                }
                drop(parser);
                held = rest.to_vec();
                if !answers.is_empty()
                    && let Ok(mut writer) = replies.lock()
                {
                    for answer in answers {
                        let _ = writer.write_all(&answer);
                    }
                    let _ = writer.flush();
                }
                drawn();
            }
            drawn();
        });
        Ok(Self {
            parser,
            writer,
            master: pty.master,
            child,
            size: (rows, cols),
            copied,
        })
    }

    /// What the program copied since last asked, oldest first.
    pub fn copied(&self) -> Vec<String> {
        self.copied
            .lock()
            .map(|mut copied| std::mem::take(&mut *copied))
            .unwrap_or_default()
    }

    /// Whether the program has ended.
    pub fn ended(&mut self) -> bool {
        !matches!(self.child.try_wait(), Ok(None))
    }

    fn send(&self, bytes: &[u8]) {
        if let Ok(mut writer) = self.writer.lock() {
            let _ = writer.write_all(bytes);
            let _ = writer.flush();
        }
    }

    /// A key, as a terminal sends it.
    pub fn key(&self, key: KeyEvent) {
        let application = self
            .parser
            .lock()
            .is_ok_and(|p| p.screen().application_cursor());
        if let Some(bytes) = key_bytes(key, application) {
            self.send(&bytes);
        }
    }

    /// Pasted text, bracketed when the program asked for that.
    pub fn paste(&self, text: &str) {
        let bracketed = self
            .parser
            .lock()
            .is_ok_and(|p| p.screen().bracketed_paste());
        let mut bytes = Vec::new();
        if bracketed {
            bytes.extend_from_slice(b"\x1b[200~");
        }
        bytes.extend_from_slice(text.as_bytes());
        if bracketed {
            bytes.extend_from_slice(b"\x1b[201~");
        }
        self.send(&bytes);
    }

    /// The mouse over the program's screen, whose first cell is at
    /// `(x, y)`, sent when it asked for the mouse; whether it did.
    pub fn mouse(&self, mouse: MouseEvent, x: u16, y: u16) -> bool {
        let wanted = self
            .parser
            .lock()
            .is_ok_and(|p| p.screen().mouse_protocol_mode() != vt100::MouseProtocolMode::None);
        if !wanted {
            return false;
        }
        let (column, row) = (
            mouse.column.saturating_sub(x) + 1,
            mouse.row.saturating_sub(y) + 1,
        );
        let mut code = match mouse.kind {
            MouseEventKind::Down(button) | MouseEventKind::Up(button) => match button {
                MouseButton::Left => 0,
                MouseButton::Middle => 1,
                MouseButton::Right => 2,
            },
            MouseEventKind::Drag(button) => {
                32 + match button {
                    MouseButton::Left => 0,
                    MouseButton::Middle => 1,
                    MouseButton::Right => 2,
                }
            }
            MouseEventKind::Moved => 35,
            MouseEventKind::ScrollUp => 64,
            MouseEventKind::ScrollDown => 65,
            MouseEventKind::ScrollLeft => 66,
            MouseEventKind::ScrollRight => 67,
        };
        if mouse.modifiers.contains(KeyModifiers::SHIFT) {
            code += 4;
        }
        if mouse.modifiers.contains(KeyModifiers::ALT) {
            code += 8;
        }
        if mouse.modifiers.contains(KeyModifiers::CONTROL) {
            code += 16;
        }
        let end = match mouse.kind {
            MouseEventKind::Up(_) => 'm',
            _ => 'M',
        };
        self.send(format!("\x1b[<{code};{column};{row}{end}").as_bytes());
        true
    }

    /// The program's screen drawn in `area`, the terminal it runs on made
    /// that size first; and where its cursor is, if it shows one.
    pub fn render(&mut self, area: Rect, buf: &mut Buffer) -> Option<(u16, u16)> {
        if area.width == 0 || area.height == 0 {
            return None;
        }
        let size = (area.height, area.width);
        if size != self.size {
            self.size = size;
            let _ = self.master.resize(PtySize {
                rows: size.0,
                cols: size.1,
                pixel_width: 0,
                pixel_height: 0,
            });
            if let Ok(mut parser) = self.parser.lock() {
                parser.set_size(size.0, size.1);
            }
        }
        let parser = self.parser.lock().ok()?;
        let screen = parser.screen();
        for row in 0..area.height {
            for col in 0..area.width {
                let Some(cell) = screen.cell(row, col) else {
                    continue;
                };
                if cell.is_wide_continuation() {
                    continue;
                }
                let at = &mut buf[(area.x + col, area.y + row)];
                let text = cell.contents();
                at.set_symbol(if text.is_empty() { " " } else { &text });
                let mut style = Style::new()
                    .fg(color(cell.fgcolor()))
                    .bg(color(cell.bgcolor()));
                if cell.bold() {
                    style = style.add_modifier(Modifier::BOLD);
                }
                if cell.italic() {
                    style = style.add_modifier(Modifier::ITALIC);
                }
                if cell.underline() {
                    style = style.add_modifier(Modifier::UNDERLINED);
                }
                if cell.inverse() {
                    style = style.add_modifier(Modifier::REVERSED);
                }
                at.set_style(style);
            }
        }
        if screen.hide_cursor() {
            return None;
        }
        let (row, col) = screen.cursor_position();
        Some((
            area.x + col.min(area.width - 1),
            area.y + row.min(area.height - 1),
        ))
    }
}

impl Drop for Embedded {
    fn drop(&mut self) {
        let _ = self.child.kill();
    }
}

fn color(color: vt100::Color) -> Color {
    match color {
        vt100::Color::Default => Color::Reset,
        vt100::Color::Idx(n) => Color::Indexed(n),
        vt100::Color::Rgb(r, g, b) => Color::Rgb(r, g, b),
    }
}

/// What a terminal is asked that this one answers.
#[derive(Debug, PartialEq, Clone, Copy)]
enum Query {
    /// OSC 11: the background color.
    Background,
    /// DA1: which terminal this is.
    Device,
    /// DSR 6: where the cursor is.
    Cursor,
}

#[derive(Debug, PartialEq)]
enum Piece<'a> {
    Output(&'a [u8]),
    Asked(Query),
    /// OSC 52's `c;BASE64`: text the program put on the clipboard.
    Copied(&'a [u8]),
}

/// Where OSC 52, a copy to the clipboard, starts.
const COPY: &[u8] = b"\x1b]52;";
/// The longest copy waited for over reads; a longer one is let through.
const MOST_COPIED: usize = 8 << 20;

const ASKS: [(&[u8], Query); 4] = [
    (b"\x1b]11;?\x1b\\", Query::Background),
    (b"\x1b]11;?\x07", Query::Background),
    (b"\x1b[c", Query::Device),
    (b"\x1b[6n", Query::Cursor),
];

/// Output cut where it asks the terminal something or copies, so each is
/// handled after the output before it, as a terminal would; and the end
/// of it that may be the start of one the next read finishes, held back.
fn pieces(bytes: &[u8]) -> (Vec<Piece<'_>>, &[u8]) {
    let starts = || ASKS.iter().map(|(seq, _)| *seq).chain([COPY]);
    let longest = starts().map(<[u8]>::len).max().unwrap_or(0);
    let held = (bytes.len().saturating_sub(longest - 1)..bytes.len())
        .find(|&at| {
            starts().any(|seq| seq.len() > bytes.len() - at && seq.starts_with(&bytes[at..]))
        })
        .unwrap_or(bytes.len());
    let mut out = Vec::new();
    let mut from = 0;
    let mut at = 0;
    while at < held {
        if bytes[at] != 0x1b {
            at += 1;
            continue;
        }
        if let Some((seq, query)) = ASKS.iter().find(|(seq, _)| bytes[at..].starts_with(seq)) {
            if from < at {
                out.push(Piece::Output(&bytes[from..at]));
            }
            out.push(Piece::Asked(*query));
            at += seq.len();
            from = at;
            continue;
        }
        if bytes[at..].starts_with(COPY) {
            let body = &bytes[at + COPY.len()..];
            // Ended by BEL or ST; base64 has neither.
            let end = body.iter().position(|&b| b == 0x07 || b == 0x1b);
            let ended = end.and_then(|end| match body[end] {
                0x07 => Some((end, 1)),
                _ => body.get(end + 1).map(|_| (end, 2)),
            });
            match ended {
                Some((end, closing)) => {
                    if from < at {
                        out.push(Piece::Output(&bytes[from..at]));
                    }
                    out.push(Piece::Copied(&body[..end]));
                    let (more, rest) = pieces(&body[end + closing..]);
                    out.extend(more);
                    return (out, rest);
                }
                // The rest of it is for a later read.
                None if body.len() < MOST_COPIED => {
                    if from < at {
                        out.push(Piece::Output(&bytes[from..at]));
                    }
                    return (out, &bytes[at..]);
                }
                None => {}
            }
        }
        at += 1;
    }
    if from < held {
        out.push(Piece::Output(&bytes[from..held]));
    }
    (out, &bytes[held..])
}

/// The text of OSC 52's `c;BASE64`; none when it asks what is on the
/// clipboard instead (`c;?`) or is not base64.
fn copy_text(payload: &[u8]) -> Option<String> {
    use base64::Engine;
    let (_, data) = std::str::from_utf8(payload).ok()?.rsplit_once(';')?;
    let bytes = base64::engine::general_purpose::STANDARD
        .decode(data)
        .ok()?;
    Some(String::from_utf8_lossy(&bytes).into_owned())
}

fn answer(query: Query, parser: &vt100::Parser, light: bool) -> Vec<u8> {
    match query {
        Query::Background => match light {
            true => b"\x1b]11;rgb:ffff/ffff/ffff\x1b\\".to_vec(),
            false => b"\x1b]11;rgb:0000/0000/0000\x1b\\".to_vec(),
        },
        Query::Device => b"\x1b[?62;22c".to_vec(),
        Query::Cursor => {
            let (row, col) = parser.screen().cursor_position();
            format!("\x1b[{};{}R", row + 1, col + 1).into_bytes()
        }
    }
}

/// A key as an xterm sends it; `application` is the cursor keys' mode
/// the program set.
fn key_bytes(key: KeyEvent, application: bool) -> Option<Vec<u8>> {
    let mods = key.modifiers;
    let (ctrl, alt, shift) = (
        mods.contains(KeyModifiers::CONTROL),
        mods.contains(KeyModifiers::ALT),
        mods.contains(KeyModifiers::SHIFT),
    );
    // xterm's modifier parameter: 1 plus shift 1, alt 2, ctrl 4.
    let modifier = 1 + u8::from(shift) + 2 * u8::from(alt) + 4 * u8::from(ctrl);
    let csi = |end: char| match modifier {
        1 if application => format!("\x1bO{end}").into_bytes(),
        1 => format!("\x1b[{end}").into_bytes(),
        m => format!("\x1b[1;{m}{end}").into_bytes(),
    };
    let tilde = |n: u8| match modifier {
        1 => format!("\x1b[{n}~").into_bytes(),
        m => format!("\x1b[{n};{m}~").into_bytes(),
    };
    let mut bytes = match key.code {
        // Cmd (Super) has no xterm form; as the kitty keyboard protocol
        // sends it, so Cmd+C still copies there.
        KeyCode::Char(c) if mods.contains(KeyModifiers::SUPER) => {
            return Some(format!("\x1b[{};{}u", c as u32, modifier + 8).into_bytes());
        }
        KeyCode::Char(c) if ctrl => {
            let byte = match c.to_ascii_lowercase() {
                c @ 'a'..='z' => c as u8 - b'a' + 1,
                ' ' | '@' | '2' => 0,
                '[' | '3' => 0x1b,
                '\\' | '4' => 0x1c,
                ']' | '5' => 0x1d,
                '^' | '6' => 0x1e,
                '_' | '7' | '/' => 0x1f,
                _ => return None,
            };
            vec![byte]
        }
        KeyCode::Char(c) => c.to_string().into_bytes(),
        // As the kitty keyboard protocol sends it, which tells it apart.
        KeyCode::Enter if shift => {
            return Some(format!("\x1b[13;{}u", if alt { 4 } else { 2 }).into_bytes());
        }
        KeyCode::Enter => b"\r".to_vec(),
        KeyCode::Tab => b"\t".to_vec(),
        KeyCode::BackTab => b"\x1b[Z".to_vec(),
        // As the kitty keyboard protocol sends it, which keeps the Ctrl
        // a word's deletion needs.
        KeyCode::Backspace if ctrl => return Some(format!("\x1b[127;{modifier}u").into_bytes()),
        KeyCode::Backspace => b"\x7f".to_vec(),
        KeyCode::Esc => b"\x1b".to_vec(),
        KeyCode::Up => return Some(csi('A')),
        KeyCode::Down => return Some(csi('B')),
        KeyCode::Right => return Some(csi('C')),
        KeyCode::Left => return Some(csi('D')),
        KeyCode::Home => return Some(csi('H')),
        KeyCode::End => return Some(csi('F')),
        KeyCode::Insert => return Some(tilde(2)),
        KeyCode::Delete => return Some(tilde(3)),
        KeyCode::PageUp => return Some(tilde(5)),
        KeyCode::PageDown => return Some(tilde(6)),
        _ => return None,
    };
    // Alt sends Esc first, before the key.
    if alt && key.code != KeyCode::BackTab {
        bytes.insert(0, 0x1b);
    }
    Some(bytes)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn questions_are_cut_out_of_the_output_in_order() {
        let (cut, rest) = pieces(b"ab\x1b]11;?\x1b\\cd\x1b[cef\x1b[6n");
        assert!(rest.is_empty());
        assert_eq!(
            cut,
            [
                Piece::Output(b"ab"),
                Piece::Asked(Query::Background),
                Piece::Output(b"cd"),
                Piece::Asked(Query::Device),
                Piece::Output(b"ef"),
                Piece::Asked(Query::Cursor),
            ]
        );
        // Other sequences pass through whole.
        assert_eq!(pieces(b"\x1b[2J").0, [Piece::Output(b"\x1b[2J")]);
        // A question a read cuts off waits for the rest.
        let (cut, rest) = pieces(b"ab\x1b[6");
        assert_eq!((cut, rest), (vec![Piece::Output(b"ab")], &b"\x1b[6"[..]));
        let (cut, rest) = pieces(b"\x1b[6n");
        assert_eq!((cut, rest), (vec![Piece::Asked(Query::Cursor)], &b""[..]));
    }

    #[test]
    fn copies_are_kept_for_the_clipboard() {
        let (cut, rest) = pieces(b"a\x1b]52;c;aGk=\x07b\x1b]52;c;eW8=\x1b\\\x1b[6n");
        assert!(rest.is_empty());
        assert_eq!(
            cut,
            [
                Piece::Output(b"a"),
                Piece::Copied(b"c;aGk="),
                Piece::Output(b"b"),
                Piece::Copied(b"c;eW8="),
                Piece::Asked(Query::Cursor),
            ]
        );
        assert_eq!(copy_text(b"c;aGk=").as_deref(), Some("hi"));
        assert_eq!(copy_text(b"c;?"), None);
        // A copy a read cuts off, anywhere, waits for the rest.
        assert_eq!(
            pieces(b"a\x1b]5"),
            (vec![Piece::Output(b"a")], &b"\x1b]5"[..])
        );
        let (cut, rest) = pieces(b"a\x1b]52;c;aG");
        assert_eq!(
            (cut, rest),
            (vec![Piece::Output(b"a")], &b"\x1b]52;c;aG"[..])
        );
        let (cut, rest) = pieces(b"\x1b]52;c;aGk=\x1b");
        assert_eq!((cut, rest), (vec![], &b"\x1b]52;c;aGk=\x1b"[..]));
    }

    #[test]
    fn the_cursor_is_where_the_output_left_it() {
        let mut parser = vt100::Parser::new(10, 20, 0);
        parser.process(b"\x1b[3;5H");
        assert_eq!(answer(Query::Cursor, &parser, true), b"\x1b[3;5R");
    }

    #[test]
    fn keys_go_as_an_xterm_sends_them() {
        let key = |code, mods| key_bytes(KeyEvent::new(code, mods), false).unwrap();
        assert_eq!(key(KeyCode::Char('a'), KeyModifiers::NONE), b"a");
        assert_eq!(key(KeyCode::Char('c'), KeyModifiers::CONTROL), [3]);
        assert_eq!(key(KeyCode::Char('b'), KeyModifiers::ALT), b"\x1bb");
        assert_eq!(key(KeyCode::Up, KeyModifiers::NONE), b"\x1b[A");
        assert_eq!(key(KeyCode::Left, KeyModifiers::ALT), b"\x1b[1;3D");
        assert_eq!(key(KeyCode::Enter, KeyModifiers::SHIFT), b"\x1b[13;2u");
        assert_eq!(
            key(KeyCode::Backspace, KeyModifiers::CONTROL),
            b"\x1b[127;5u"
        );
        assert_eq!(key(KeyCode::Backspace, KeyModifiers::ALT), b"\x1b\x7f");
        assert_eq!(key(KeyCode::Char('c'), KeyModifiers::SUPER), b"\x1b[99;9u");
        // Alt+Enter is a new line in Sonny's composer, as Shift+Enter is.
        assert_eq!(key(KeyCode::Enter, KeyModifiers::ALT), b"\x1b\r");
        assert_eq!(key(KeyCode::PageUp, KeyModifiers::NONE), b"\x1b[5~");
        assert_eq!(
            key_bytes(KeyEvent::new(KeyCode::Up, KeyModifiers::NONE), true).unwrap(),
            b"\x1bOA"
        );
    }
}
