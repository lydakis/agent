//! Pictures drawn beside the window's loop: a diagram, a formula, a page
//! or a picture file the cache does not have yet is drawn by one worker
//! thread, in the order asked, while the loop goes on taking keys. Where
//! it goes, a row says it is being drawn; once it is, the window is woken
//! and every such row whose picture is ready is drawn again in its place,
//! as is one pushed later from rows held meanwhile.
//! The same picture asked for again while it is drawn is drawn once.
//!
//! A row standing for a picture carries the picture's key, and how to
//! draw its rows again is kept beside the picture in the cache's folder:
//! the window keeps nothing for a picture once it is drawn, however long
//! the conversation.
use std::{
    collections::{HashMap, HashSet, VecDeque},
    path::PathBuf,
    sync::{
        Arc, Condvar, Mutex, Weak,
        atomic::{AtomicU64, Ordering},
    },
};

use ratatui::{
    style::{Color, Modifier, Style, Stylize},
    text::{Line, Span},
};
use serde::{Deserialize, Serialize};

use crate::image::Drawn;

type Draw = Box<dyn FnOnce() -> Result<Drawn, String> + Send>;

/// How the rows of a picture are drawn again once it is ready: from the
/// cache, by what it was drawn from.
#[derive(Clone, Serialize, Deserialize)]
pub enum Redraw {
    /// A fence of `lang`.
    Fence {
        lang: String,
        source: String,
        width: usize,
    },
    /// An HTML page kept at `path`.
    Page { path: PathBuf, width: usize },
    /// A picture file, named by its description and where it was linked.
    File {
        path: PathBuf,
        width: usize,
        alt: String,
        src: String,
    },
    /// Nothing: whoever shows it draws it again from its file.
    Nothing,
    /// Rows as they are, for tests.
    #[cfg(test)]
    Rows(Vec<String>),
}

impl Redraw {
    fn rows(&self) -> Vec<Line<'static>> {
        match self {
            Redraw::Fence {
                lang,
                source,
                width,
            } => crate::transcript::fence_picture(lang.clone(), source.clone(), *width),
            Redraw::Page { path, width } => {
                let source = std::fs::read_to_string(path).unwrap_or_default();
                crate::transcript::kept_page(path.clone(), &source, *width)
            }
            Redraw::File {
                path,
                width,
                alt,
                src,
            } => crate::transcript::picture_again(path, *width, alt, src),
            Redraw::Nothing => Vec::new(),
            #[cfg(test)]
            Redraw::Rows(rows) => rows.iter().map(|row| Line::from(row.clone())).collect(),
        }
    }
}

#[derive(Default)]
struct State {
    /// The pictures asked for and not drawn yet, by key: queued or drawing.
    running: HashSet<u64>,
    jobs: VecDeque<(u64, Option<PathBuf>, Draw)>,
    worker: bool,
    /// How to draw again the rows standing for a picture still drawing;
    /// kept beside it in the folder once it is drawn.
    asked: HashMap<u64, Redraw>,
    /// Where there is no folder for pictures, or it could not be written:
    /// what was drawn, or why it could not be, so it is not tried again at
    /// each look; and how to draw its rows again.
    done: HashMap<u64, Result<Drawn, String>>,
    unkept: HashMap<u64, Redraw>,
    /// The windows woken as a picture is ready.
    wake: Vec<Weak<tokio::sync::Notify>>,
}

static STATE: Mutex<Option<State>> = Mutex::new(None);
static JOBS: Condvar = Condvar::new();
/// Counts the pictures made ready, so a window knows to look.
static LANDED: AtomicU64 = AtomicU64::new(0);

fn state() -> std::sync::MutexGuard<'static, Option<State>> {
    STATE.lock().unwrap_or_else(|e| e.into_inner())
}

/// `woken` notified each time a picture is ready.
pub fn wake(woken: &Arc<tokio::sync::Notify>) {
    let mut state = state();
    let state = state.get_or_insert_with(State::default);
    state.wake.retain(|w| w.strong_count() > 0);
    state.wake.push(Arc::downgrade(woken));
}

/// The windows woken to draw again, as for a picture ready, for something
/// else made ready beside the loop.
pub fn ring() {
    let wake = {
        let mut state = state();
        let state = state.get_or_insert_with(State::default);
        state.wake.retain(|w| w.strong_count() > 0);
        (state.wake.iter())
            .filter_map(Weak::upgrade)
            .collect::<Vec<_>>()
    };
    for woken in wake {
        woken.notify_one();
    }
}

/// How many pictures were made ready so far.
pub fn landed() -> u64 {
    LANDED.load(Ordering::Acquire)
}

/// What was drawn of `key` and not kept in a file, or why it could not
/// be drawn.
pub fn take(key: u64) -> Option<Result<Drawn, String>> {
    state().as_ref()?.done.get(&key).cloned()
}

/// `draw` run beside the loop, kept in `path` once drawn, unless the
/// picture `key` is drawn already.
pub fn start(key: u64, path: Option<PathBuf>, draw: Draw) {
    let mut state = state();
    let state = state.get_or_insert_with(State::default);
    if !state.running.insert(key) {
        return;
    }
    state.jobs.push_back((key, path, draw));
    if !std::mem::replace(&mut state.worker, true) {
        std::thread::Builder::new()
            .name("sonny-drawing".into())
            .spawn(work)
            .expect("a thread for drawing");
    }
    JOBS.notify_one();
}

/// Where how to draw again the rows of picture `key` is kept.
fn redraw_path(drawn: &std::path::Path) -> PathBuf {
    drawn.with_extension("redraw")
}

/// The worker: each picture in turn, kept, and the windows woken.
fn work() {
    loop {
        let (key, path, draw) = {
            let mut guard = state();
            loop {
                if let Some(job) = guard.as_mut().and_then(|s| s.jobs.pop_front()) {
                    break job;
                }
                guard = JOBS.wait(guard).unwrap_or_else(|e| e.into_inner());
            }
        };
        let drawn = std::panic::catch_unwind(std::panic::AssertUnwindSafe(draw))
            .unwrap_or_else(|_| Err("drawing it failed".into()));
        // Kept where every later look finds it; only what no file holds
        // waits here.
        let kept = match (&drawn, &path) {
            (Ok(drawn), Some(path)) => crate::cache::write(path, drawn).is_ok(),
            _ => false,
        };
        let wake = {
            let mut state = state();
            let state = state.get_or_insert_with(State::default);
            // How to draw its rows again kept beside it, under the lock,
            // so no row asked for meanwhile is missed: small, and once
            // for each picture.
            if let Some(redraw) = state.asked.remove(&key) {
                let written = path.as_ref().is_some_and(|path| {
                    serde_json::to_vec(&redraw).is_ok_and(|bytes| {
                        crate::cache::write_whole(&redraw_path(path), &bytes).is_ok()
                    })
                });
                if !written {
                    state.unkept.insert(key, redraw);
                }
            }
            state.running.remove(&key);
            if !kept {
                state.done.insert(key, drawn);
            }
            LANDED.fetch_add(1, Ordering::AcqRel);
            state.wake.retain(|w| w.strong_count() > 0);
            state
                .wake
                .iter()
                .filter_map(Weak::upgrade)
                .collect::<Vec<_>>()
        };
        for woken in wake {
            woken.notify_one();
        }
    }
}

/// The marks a row standing for picture `key` wears: three empty spans,
/// never drawn, their underline colours the key, 24 bits at a time with
/// a check in the last, as a wrapped row's lead is marked
/// ([`crate::transcript::lead`]) in an indexed one.
fn marks(key: u64) -> [Span<'static>; 3] {
    let parts = [
        (key >> 40) as u32,
        (key >> 16) as u32 & 0xFF_FFFF,
        ((key as u32 & 0xFFFF) << 8) | CHECK,
    ];
    parts.map(|part| {
        let style = Style::new()
            .underline_color(Color::Rgb(
                (part >> 16) as u8,
                (part >> 8) as u8,
                part as u8,
            ))
            .add_modifier(Modifier::HIDDEN);
        Span::styled("", style)
    })
}

const CHECK: u32 = 0xD7;

fn marked(span: &Span) -> Option<u32> {
    match (span.content.is_empty(), span.style.underline_color) {
        (true, Some(Color::Rgb(r, g, b))) if span.style.add_modifier.contains(Modifier::HIDDEN) => {
            Some(u32::from_be_bytes([0, r, g, b]))
        }
        _ => None,
    }
}

/// Where `line`'s marks start, and the key they carry.
fn key_of(line: &Line) -> Option<(usize, u64)> {
    let at = line.spans.iter().position(|span| marked(span).is_some())?;
    let parts = line.spans.get(at..at + 3)?;
    let [a, b, c] = [&parts[0], &parts[1], &parts[2]].map(marked);
    let (a, b, c) = (a?, b?, c?);
    (c & 0xFF == CHECK).then_some((
        at,
        (u64::from(a) << 40) | (u64::from(b) << 16) | u64::from(c >> 8),
    ))
}

/// The row that stands where picture `key` goes while it is drawn, saying
/// so; `redraw` draws its rows once it is. Drawn already, its rows.
pub fn placeholder(key: u64, what: &str, redraw: Redraw) -> Vec<Line<'static>> {
    {
        let mut state = state();
        let state = state.get_or_insert_with(State::default);
        // Drawn already, as it was asked for: its rows, unless whoever
        // shows it draws it again from its file, which a row standing
        // for it tells them to.
        let drawn = !state.running.contains(&key);
        if !drawn || matches!(redraw, Redraw::Nothing) {
            match drawn {
                // Its window looks again at its next pass.
                true => {
                    state.unkept.insert(key, redraw);
                    LANDED.fetch_add(1, Ordering::AcqRel);
                }
                false => {
                    state.asked.insert(key, redraw);
                }
            }
            // The marks first: a row's lead swapped
            // ([`crate::transcript::relead`]) keeps the empty spans it
            // starts with.
            let mut spans = marks(key).to_vec();
            spans.push(Span::raw("  "));
            spans.push(Span::raw(format!("drawing {what}…")).dim());
            return vec![Line::from(spans)];
        }
    }
    redraw.rows()
}

/// Whether `line` stands for a picture still to come.
pub fn waits(line: &Line) -> bool {
    line.spans.iter().any(|span| marked(span).is_some())
}

/// The rows in place of `line`, when it stands for a picture now ready:
/// what was before its words leads each of them.
pub fn ready(line: &Line<'static>) -> Option<Vec<Line<'static>>> {
    let (at, key) = key_of(line)?;
    let kept = {
        let state = state();
        let state = state.as_ref()?;
        if state.running.contains(&key) {
            return None;
        }
        state.unkept.get(&key).cloned()
    };
    let redraw = kept.or_else(|| {
        let path = redraw_path(&crate::cache::path(key)?);
        serde_json::from_slice(&std::fs::read(path).ok()?).ok()
    })?;
    // The lead: every span but the marks and the words, which end it.
    let lead = (line.spans.iter().enumerate())
        .filter(|(i, _)| !(at..at + 3).contains(i) && *i + 1 != line.spans.len())
        .map(|(_, span)| span.clone());
    let lead = Line::from(lead.collect::<Vec<_>>());
    let rows = redraw
        .rows()
        .into_iter()
        .map(|row| {
            let mut row = crate::transcript::relead(row, &lead);
            row.style = line.style.patch(row.style);
            row
        })
        .collect();
    Some(rows)
}

/// Every row of `rows` that stands for a picture now ready drawn in its
/// place; whether any was.
pub fn fill(rows: &mut Vec<Line<'static>>) -> bool {
    if !rows.iter().any(waits) {
        return false;
    }
    let mut out = Vec::with_capacity(rows.len());
    let mut any = false;
    for row in rows.drain(..) {
        match ready(&row) {
            Some(drawn) => {
                any = true;
                out.extend(drawn);
            }
            None => out.push(row),
        }
    }
    *rows = out;
    any
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{Duration, Instant};

    fn drawn(rows: u32) -> Drawn {
        Drawn {
            png: vec![1, 2, 3],
            cols: 4,
            rows,
            cut: false,
        }
    }

    fn until(what: impl Fn() -> bool) {
        let began = Instant::now();
        while !what() {
            assert!(began.elapsed() < Duration::from_secs(10), "never ready");
            std::thread::sleep(Duration::from_millis(5));
        }
    }

    /// Picture `key` asked for, drawn once the sender sends.
    fn gated(key: u64, path: Option<PathBuf>) -> std::sync::mpsc::Sender<()> {
        let (go, gate) = std::sync::mpsc::channel::<()>();
        let gate = Mutex::new(gate);
        start(
            key,
            path,
            Box::new(move || {
                let _ = gate.lock().unwrap().recv();
                Ok(drawn(2))
            }),
        );
        go
    }

    #[test]
    fn a_drawn_picture_leaves_nothing_in_memory_and_its_rows_still_take_it() {
        let key = 0xD1A6_0000_0000_0006 ^ u64::from(std::process::id());
        let Some(path) = crate::cache::path(key) else {
            return;
        };
        let go = gated(key, Some(path.clone()));
        let held = placeholder(key, "the diagram", Redraw::Rows(vec!["  drawn".into()]));
        go.send(()).unwrap();
        until(|| state().as_ref().is_some_and(|s| !s.running.contains(&key)));
        {
            let state = state();
            let state = state.as_ref().unwrap();
            assert!(!state.asked.contains_key(&key));
            assert!(!state.unkept.contains_key(&key));
            assert!(!state.done.contains_key(&key));
        }
        // Long after, its row takes it from what the folder kept.
        let rows = ready(&held[0]).expect("ready");
        let text: Vec<String> = rows.iter().map(|r| r.to_string()).collect();
        assert_eq!(text, ["  drawn"]);
        let _ = std::fs::remove_file(redraw_path(&path));
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn a_row_carries_its_picture_s_whole_key() {
        for key in [0, 1, u64::MAX, 0xD1A6_0000_00AB_C003, 0x0123_4567_89AB_CDEF] {
            let mut spans = marks(key).to_vec();
            spans.push(Span::raw("  drawing…"));
            let line = Line::from(spans);
            assert!(waits(&line));
            assert_eq!(key_of(&line), Some((0, key)));
        }
    }

    #[test]
    fn the_same_picture_asked_twice_while_drawn_is_drawn_once() {
        let key = 0xD1A6_0001;
        let runs = Arc::new(AtomicU64::new(0));
        let (go, gate) = std::sync::mpsc::channel::<()>();
        let gate = Mutex::new(gate);
        let counted = runs.clone();
        start(
            key,
            None,
            Box::new(move || {
                counted.fetch_add(1, Ordering::SeqCst);
                let _ = gate.lock().unwrap().recv();
                Ok(drawn(2))
            }),
        );
        let again = runs.clone();
        start(
            key,
            None,
            Box::new(move || {
                again.fetch_add(1, Ordering::SeqCst);
                Ok(drawn(2))
            }),
        );
        go.send(()).unwrap();
        until(|| state().as_ref().is_some_and(|s| !s.running.contains(&key)));
        assert_eq!(runs.load(Ordering::SeqCst), 1);
        // Kept for every look, as a file would keep it.
        for _ in 0..2 {
            assert!(take(key).is_some_and(|d| d.is_ok_and(|d| d.rows == 2)));
        }
    }

    #[test]
    fn a_row_waiting_on_a_picture_takes_its_rows_once_it_is_ready() {
        let key = 0xD1A6_0002;
        let (go, gate) = std::sync::mpsc::channel::<()>();
        let gate = Mutex::new(gate);
        start(
            key,
            None,
            Box::new(move || {
                let _ = gate.lock().unwrap().recv();
                Err("no".into())
            }),
        );
        let mut rows = vec![Line::from("above")];
        let waiting = placeholder(
            key,
            "the diagram",
            Redraw::Rows(vec!["  first".into(), "  second".into()]),
        );
        // A lead in place of the row's own two columns, as a note's.
        rows.extend(
            waiting
                .into_iter()
                .map(|row| crate::transcript::relead(row, &Line::from("│ "))),
        );
        rows.push(Line::from("below"));
        let text = |rows: &[Line]| -> Vec<String> { rows.iter().map(|r| r.to_string()).collect() };
        assert_eq!(text(&rows), ["above", "│ drawing the diagram…", "below"]);
        // Still drawing: it stays.
        assert!(!fill(&mut rows));
        go.send(()).unwrap();
        until(|| ready(&rows[1]).is_some());
        assert!(fill(&mut rows));
        assert_eq!(text(&rows), ["above", "│ first", "│ second", "below"]);
    }

    #[test]
    fn pictures_whose_keys_end_alike_wait_in_rows_of_their_own() {
        // Two keys alike in their low 24 bits, still drawing.
        let (one, two) = (0xD1A6_0000_00AB_C003, 0x0000_0001_00AB_C003);
        let (go, gate) = std::sync::mpsc::channel::<()>();
        let gate = Mutex::new(gate);
        start(
            one,
            None,
            Box::new(move || {
                let _ = gate.lock().unwrap().recv();
                Err("no".into())
            }),
        );
        start(two, None, Box::new(|| Err("no".into())));
        let mut rows = placeholder(one, "the diagram", Redraw::Rows(vec!["  one".into()]));
        rows.extend(placeholder(
            two,
            "the formula",
            Redraw::Rows(vec!["  two".into()]),
        ));
        go.send(()).unwrap();
        until(|| ready(&rows[0]).is_some() && ready(&rows[1]).is_some());
        assert!(fill(&mut rows));
        let text: Vec<String> = rows.iter().map(|r| r.to_string()).collect();
        assert_eq!(text, ["  one", "  two"]);
    }

    #[test]
    fn a_picture_in_a_run_of_tagged_rows_keeps_the_run_whole() {
        use crate::screen::{Mark, Screen, Tag};
        let key = 0xD1A6_0004;
        let go = gated(key, None);
        let mut lines = vec![Line::from("words")];
        lines.extend(placeholder(
            key,
            "the diagram",
            Redraw::Rows(vec!["  first".into(), "  second".into()]),
        ));
        lines.push(Line::from("more words"));
        let mut screen = Screen::default();
        let marks = (0..3).map(|at| (at, Mark::Tag(Tag::More(7)))).collect();
        screen.push_marked(lines, marks);
        screen.push_marked(vec![Line::from("end")], vec![(0, Mark::Tag(Tag::End(7)))]);
        go.send(()).unwrap();
        until(|| state().as_ref().is_some_and(|s| !s.running.contains(&key)));
        assert!(screen.fill_drawn());
        assert_eq!(
            screen.texts(),
            ["words", "  first", "  second", "more words", "end"]
        );
        assert_eq!(screen.count_tagged(Tag::More(7)), 4);
        assert_eq!(screen.count_tagged(Tag::End(7)), 1);
        assert!(!screen.waits());
    }

    #[test]
    fn a_picture_is_drawn_for_the_terminal_as_it_was_when_asked() {
        let asked = crate::term::drawn();
        let light = crate::term::light();
        let drawn = std::thread::spawn(move || crate::term::drawn_as(asked, crate::term::light))
            .join()
            .unwrap();
        assert_eq!(drawn, light);
    }

    #[test]
    fn rows_held_until_after_their_picture_is_ready_still_take_it() {
        use crate::screen::Screen;
        let key = 0xD1A6_0005;
        let go = gated(key, None);
        // Held elsewhere (a call's rows) while the picture is drawn.
        let held = placeholder(key, "the picture", Redraw::Rows(vec!["  drawn".into()]));
        go.send(()).unwrap();
        until(|| state().as_ref().is_some_and(|s| !s.running.contains(&key)));
        let mut screen = Screen::default();
        screen.push(held);
        assert!(screen.pushed_waiting());
        assert!(screen.fill_drawn());
        assert_eq!(screen.texts(), ["  drawn"]);
        assert!(!screen.pushed_waiting());
    }
}
