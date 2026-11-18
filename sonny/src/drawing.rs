//! Pictures drawn beside the window's loop: a diagram, a formula, a page
//! or a picture file the cache does not have yet is drawn by one worker
//! thread, in the order asked, while the loop goes on taking keys. Where
//! it goes, a row says it is being drawn; once it is, the window is woken
//! and every such row whose picture is ready is drawn again in its place.
//! The same picture asked for again while it is drawn is drawn once.
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

use crate::image::Drawn;

type Draw = Box<dyn FnOnce() -> Result<Drawn, String> + Send>;
type Redraw = Arc<dyn Fn() -> Vec<Line<'static>> + Send + Sync>;

/// A row standing where a picture goes, by the mark it wears: the picture
/// it waits for, how to draw its rows again, and when that picture was
/// ready, once it is.
struct Placed {
    key: u64,
    redraw: Redraw,
    landed: Option<u64>,
    /// The thread that drew the row: its window's, which lets it go.
    owner: std::thread::ThreadId,
}

#[derive(Default)]
struct State {
    /// The pictures asked for and not drawn yet, by key: queued or drawing.
    running: HashSet<u64>,
    jobs: VecDeque<(u64, Option<PathBuf>, Draw)>,
    worker: bool,
    /// What was drawn and no file kept, taken by the next look for it; and
    /// what could not be drawn, and why.
    done: HashMap<u64, Result<Drawn, String>>,
    /// Rows standing for pictures, by the number their mark carries.
    placed: HashMap<u32, Placed>,
    /// That number for each picture with rows standing for it: one of its
    /// own among those standing, however its key is cut to fit.
    ids: HashMap<u64, u32>,
    last_id: u32,
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

/// How many pictures were made ready so far.
pub fn landed() -> u64 {
    LANDED.load(Ordering::Acquire)
}

/// What was drawn of `key` and not kept in a file, once; or why it could
/// not be drawn, which stands, so a picture that cannot be drawn is not
/// tried again at every look.
pub fn take(key: u64) -> Option<Result<Drawn, String>> {
    let mut state = state();
    let state = state.as_mut()?;
    match state.done.get(&key)? {
        Err(error) => Some(Err(error.clone())),
        Ok(_) => state.done.remove(&key),
    }
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
            state.running.remove(&key);
            if !kept {
                state.done.insert(key, drawn);
            }
            let landed = LANDED.fetch_add(1, Ordering::AcqRel) + 1;
            for placed in state.placed.values_mut().filter(|p| p.key == key) {
                placed.landed = Some(landed);
            }
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

/// The mark a row standing for picture `id` wears: an empty span, never
/// drawn, its underline colour the 24-bit number, as a wrapped row's lead
/// is marked ([`crate::transcript::lead`]) in an indexed one.
fn mark(id: u32) -> Span<'static> {
    let style = Style::new()
        .underline_color(Color::Rgb((id >> 16) as u8, (id >> 8) as u8, id as u8))
        .add_modifier(Modifier::HIDDEN);
    Span::styled("", style)
}

fn marked(span: &Span) -> Option<u32> {
    match (span.content.is_empty(), span.style.underline_color) {
        (true, Some(Color::Rgb(r, g, b))) if span.style.add_modifier.contains(Modifier::HIDDEN) => {
            Some(u32::from_be_bytes([0, r, g, b]))
        }
        _ => None,
    }
}

/// The row that stands where picture `key` goes while it is drawn, saying
/// so; `redraw` draws its rows once it is.
pub fn placeholder(
    key: u64,
    what: &str,
    redraw: impl Fn() -> Vec<Line<'static>> + Send + Sync + 'static,
) -> Vec<Line<'static>> {
    let mut state = state();
    let state = state.get_or_insert_with(State::default);
    // Ready already: the next look draws it.
    let landed = (!state.running.contains(&key)).then(landed);
    let id = match state.ids.get(&key) {
        Some(id) => *id,
        None => {
            // The next number no standing row has.
            let mut id = state.last_id;
            loop {
                id = id % 0xFF_FFFF + 1;
                if !state.placed.contains_key(&id) {
                    break;
                }
            }
            state.last_id = id;
            state.ids.insert(key, id);
            id
        }
    };
    state.placed.insert(
        id,
        Placed {
            key,
            redraw: Arc::new(redraw),
            landed,
            owner: std::thread::current().id(),
        },
    );
    // The mark first: a row's lead swapped ([`crate::transcript::relead`])
    // keeps the empty spans it starts with.
    vec![Line::from(vec![
        mark(id),
        Span::raw("  "),
        Span::raw(format!("drawing {what}…")).dim(),
    ])]
}

/// Whether `line` stands for a picture still to come.
pub fn waits(line: &Line) -> bool {
    line.spans.iter().any(|span| marked(span).is_some())
}

/// The rows in place of `line`, when it stands for a picture now ready:
/// what was before its words leads each of them.
pub fn ready(line: &Line<'static>) -> Option<Vec<Line<'static>>> {
    let (at, mark) = (line.spans.iter().enumerate()).find_map(|(at, s)| Some((at, marked(s)?)))?;
    let redraw = {
        let state = state();
        let state = state.as_ref()?;
        let placed = state.placed.get(&mark)?;
        if state.running.contains(&placed.key) {
            return None;
        }
        placed.redraw.clone()
    };
    // The lead: every span but the mark and the words, which end it.
    let lead = (line.spans.iter().enumerate())
        .filter(|(i, _)| *i != at && *i + 1 != line.spans.len())
        .map(|(_, span)| span.clone());
    let lead = Line::from(lead.collect::<Vec<_>>());
    let rows = redraw()
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

/// The rows this thread drew that stood for pictures ready by the count
/// `seen` let go: its window looked at them since.
pub fn settle(seen: u64) {
    let owner = std::thread::current().id();
    if let Some(state) = state().as_mut() {
        let ids = &mut state.ids;
        state.placed.retain(|_, placed| {
            let kept = placed.owner != owner || placed.landed.is_none_or(|at| at > seen);
            if !kept {
                ids.remove(&placed.key);
            }
            kept
        });
    }
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
        assert!(take(key).is_some_and(|d| d.is_ok_and(|d| d.rows == 2)));
        assert!(take(key).is_none());
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
        let waiting = placeholder(key, "the diagram", || {
            vec![Line::from("  first"), Line::from("  second")]
        });
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
        let seen = landed();
        assert!(fill(&mut rows));
        assert_eq!(text(&rows), ["above", "│ first", "│ second", "below"]);
        settle(seen);
        let state = state();
        assert!(!state.as_ref().unwrap().ids.contains_key(&key));
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
        let mut rows = placeholder(one, "the diagram", || vec![Line::from("  one")]);
        rows.extend(placeholder(two, "the formula", || {
            vec![Line::from("  two")]
        }));
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
        start(key, None, Box::new(|| Err("no".into())));
        let mut lines = vec![Line::from("words")];
        lines.extend(placeholder(key, "the diagram", || {
            vec![Line::from("  first"), Line::from("  second")]
        }));
        lines.push(Line::from("more words"));
        let mut screen = Screen::default();
        let marks = (0..3).map(|at| (at, Mark::Tag(Tag::More(7)))).collect();
        screen.push_marked(lines, marks);
        screen.push_marked(vec![Line::from("end")], vec![(0, Mark::Tag(Tag::End(7)))]);
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
}
