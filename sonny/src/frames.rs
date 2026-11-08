//! What the screen is written through: each frame held until it ends, then
//! written whole inside a synchronized update (mode 2026), as Ink and Codex
//! write theirs, or not at all when it is byte for byte the frame written
//! before it.
//!
//! A frame shown whole never shows half written, and the cursor is hidden
//! while its cells land, shown again only where the frame puts it: written
//! cell by cell in view, the cursor flickered over the screen, and what
//! follows the cursor (an emoji picker, an input method) could open
//! wherever the cursor passed. A terminal that does not know the mode
//! ignores it.
//!
//! An idle window draws ten frames a second, and each, with nothing
//! changed, still wrote the same few bytes: the cell diff is empty, but the
//! style reset and the cursor's place close every frame, and the update's
//! markers wrap it. Every byte of a frame puts something at a place it
//! names (a cell, a style, the cursor), so writing the same bytes again
//! changes nothing on the screen: such a frame is dropped, and an idle
//! window writes nothing. Anything written between frames (a full clear)
//! means the next frame is written whatever it holds.

use std::io::{Stdout, Write};
use std::sync::{LazyLock, Mutex};

use ratatui::backend::{ClearType, CrosstermBackend, WindowSize};
use ratatui::buffer::Cell;
use ratatui::crossterm::cursor::Hide;
use ratatui::crossterm::terminal::{BeginSynchronizedUpdate, EndSynchronizedUpdate};
use ratatui::crossterm::{execute, queue};
use ratatui::layout::{Position, Size};

/// A frame's bytes on their way to `out`.
pub struct Hold<W> {
    out: W,
    /// Whether a frame is being written, its bytes held.
    open: bool,
    /// The open frame's bytes.
    now: Vec<u8>,
    /// The bytes of the last frame written, none once anything else was.
    last: Vec<u8>,
}

impl<W: Write> Hold<W> {
    pub const fn new(out: W) -> Self {
        Self {
            out,
            open: false,
            now: Vec::new(),
            last: Vec::new(),
        }
    }

    fn begin(&mut self) {
        self.open = true;
        self.now.clear();
    }

    /// The open frame written, or dropped as the last frame's very bytes.
    /// A frame that failed is written all the same, and the next one
    /// whatever it holds.
    fn end(&mut self, drawn: bool) -> std::io::Result<()> {
        self.open = false;
        if drawn && (self.now.is_empty() || self.now == self.last) {
            return Ok(());
        }
        self.last.clear();
        queue!(self.out, BeginSynchronizedUpdate, Hide)?;
        let written = self.out.write_all(&self.now);
        // Ended even after a failed write, so the terminal is never left
        // holding frames back.
        execute!(self.out, EndSynchronizedUpdate)?;
        written?;
        if drawn {
            std::mem::swap(&mut self.last, &mut self.now);
        }
        Ok(())
    }

    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        if self.open {
            self.now.extend_from_slice(bytes);
            return Ok(bytes.len());
        }
        self.last.clear();
        self.out.write(bytes)
    }

    fn flush(&mut self) -> std::io::Result<()> {
        match self.open {
            true => Ok(()),
            false => self.out.flush(),
        }
    }
}

/// The terminal's: one per process, as the screen is.
static SCREEN: LazyLock<Mutex<Hold<Stdout>>> =
    LazyLock::new(|| Mutex::new(Hold::new(std::io::stdout())));

fn screen() -> std::sync::MutexGuard<'static, Hold<Stdout>> {
    SCREEN
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

/// Stdout through [`SCREEN`]: what the terminal's backend writes to.
pub struct Screen;

impl Write for Screen {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        screen().write(bytes)
    }

    fn flush(&mut self) -> std::io::Result<()> {
        screen().flush()
    }
}

/// The terminal's backend: crossterm's, writing to `W`, its size the one
/// [`crate::term::size`] keeps rather than read from the terminal on
/// every ask, which `Terminal` makes twice a frame and Sonny three times
/// more.
pub struct Backend<W: Write>(CrosstermBackend<W>);

impl<W: Write> Backend<W> {
    pub const fn new(out: W) -> Self {
        Self(CrosstermBackend::new(out))
    }
}

impl Backend<Screen> {
    /// What the screen is written through: [`Screen`], held a frame at a
    /// time.
    pub const fn screen() -> Self {
        Self::new(Screen)
    }
}

impl<W: Write> Write for Backend<W> {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        self.0.write(bytes)
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Write::flush(&mut self.0)
    }
}

impl<W: Write> ratatui::backend::Backend for Backend<W> {
    type Error = std::io::Error;

    fn draw<'a, I>(&mut self, content: I) -> std::io::Result<()>
    where
        I: Iterator<Item = (u16, u16, &'a Cell)>,
    {
        self.0.draw(content)
    }
    fn append_lines(&mut self, n: u16) -> std::io::Result<()> {
        self.0.append_lines(n)
    }
    fn hide_cursor(&mut self) -> std::io::Result<()> {
        self.0.hide_cursor()
    }
    fn show_cursor(&mut self) -> std::io::Result<()> {
        self.0.show_cursor()
    }
    fn get_cursor_position(&mut self) -> std::io::Result<Position> {
        self.0.get_cursor_position()
    }
    fn set_cursor_position<P: Into<Position>>(&mut self, position: P) -> std::io::Result<()> {
        self.0.set_cursor_position(position)
    }
    fn clear(&mut self) -> std::io::Result<()> {
        self.0.clear()
    }
    fn clear_region(&mut self, clear_type: ClearType) -> std::io::Result<()> {
        self.0.clear_region(clear_type)
    }
    fn size(&self) -> std::io::Result<Size> {
        let size = crate::term::size()?;
        Ok(Size::new(size.columns, size.rows))
    }
    fn window_size(&mut self) -> std::io::Result<WindowSize> {
        let size = crate::term::size()?;
        Ok(WindowSize {
            columns_rows: Size::new(size.columns, size.rows),
            pixels: Size::new(size.width, size.height),
        })
    }
    fn flush(&mut self) -> std::io::Result<()> {
        ratatui::backend::Backend::flush(&mut self.0)
    }
    fn scroll_region_up(
        &mut self,
        region: std::ops::Range<u16>,
        lines: u16,
    ) -> std::io::Result<()> {
        self.0.scroll_region_up(region, lines)
    }
    fn scroll_region_down(
        &mut self,
        region: std::ops::Range<u16>,
        lines: u16,
    ) -> std::io::Result<()> {
        self.0.scroll_region_down(region, lines)
    }
}

/// One frame drawn to the screen, everything `draw` writes through
/// [`Screen`] held until it ends.
pub fn frame(draw: impl FnOnce() -> std::io::Result<()>) -> std::io::Result<()> {
    screen().begin();
    let drawn = draw();
    let ended = screen().end(drawn.is_ok());
    drawn.and(ended)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A frame drawn into `hold`, writing `bytes`, and what reached the
    /// terminal for it.
    fn drawn(hold: &mut Hold<Vec<u8>>, bytes: &[u8]) -> Vec<u8> {
        hold.out.clear();
        hold.begin();
        hold.write(bytes).unwrap();
        hold.end(true).unwrap();
        std::mem::take(&mut hold.out)
    }

    /// A frame that failed after writing `bytes`.
    fn failed(hold: &mut Hold<Vec<u8>>, bytes: &[u8]) -> Vec<u8> {
        hold.out.clear();
        hold.begin();
        hold.write(bytes).unwrap();
        hold.end(false).unwrap();
        std::mem::take(&mut hold.out)
    }

    #[test]
    fn a_frame_is_shown_whole_with_the_cursor_hidden_while_it_lands() {
        let mut hold = Hold::new(Vec::new());
        assert_eq!(
            drawn(&mut hold, b"CELLS"),
            b"\x1b[?2026h\x1b[?25lCELLS\x1b[?2026l"
        );
        // A frame that fails still ends the update, so the terminal never
        // holds what comes after back, and the next frame is written.
        let mut hold = Hold::new(Vec::new());
        assert_eq!(
            failed(&mut hold, b"HALF"),
            b"\x1b[?2026h\x1b[?25lHALF\x1b[?2026l"
        );
    }

    #[test]
    fn a_frame_that_changes_nothing_writes_nothing() {
        let mut hold = Hold::new(Vec::new());
        assert!(!drawn(&mut hold, b"TAIL").is_empty());
        // An idle window: the same bytes, frame after frame.
        for _ in 0..3 {
            assert_eq!(drawn(&mut hold, b"TAIL"), b"");
        }
        assert!(!drawn(&mut hold, b"CELL TAIL").is_empty());
        assert!(
            !drawn(&mut hold, b"TAIL").is_empty(),
            "back is a change too"
        );
        // Anything written between frames, and the next is written whole.
        hold.write(b"CLEAR").unwrap();
        hold.out.clear();
        assert!(!drawn(&mut hold, b"TAIL").is_empty());
        // As after a frame that failed.
        assert!(!failed(&mut hold, b"TAIL").is_empty());
        assert!(!drawn(&mut hold, b"TAIL").is_empty());
    }
}
