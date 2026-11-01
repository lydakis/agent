//! When the person last used each agent's window, for a lead that keeps
//! only so many of its tasks' windows open ([`crate::mux::Room`]): every
//! window's Sonny marks its agent's file in `~/.agent/sonny/used/` when the
//! person focuses, types or clicks there, at most once a [`EVERY`], and the
//! lead reads the file's time only when it needs room. A file is a
//! window's own, so two windows never write one line of the same file.
//!
//! A lead keeps, in `~/.agent/sonny/aside/`, a file for each of its tasks
//! whose window it closed for room, so that one started again still opens
//! that window when the person asks to see the task ([`aside`]).
//!
//! Beside them, in `~/.agent/sonny/held/`, a window's Sonny keeps a file
//! while its agent failed or needs the person, as it reports its status to
//! the multiplexer: such a window is never closed for room, also by a lead
//! that started again since and has not heard so itself.
use std::{
    path::PathBuf,
    time::{Duration, Instant, SystemTime},
};

/// How often at most a window marks its agent used.
pub const EVERY: Duration = Duration::from_secs(10);

/// The agent's file, under `HOME`; none for a name no file can have.
fn file(bot: &str) -> Option<PathBuf> {
    in_dir("used", bot)
}

fn in_dir(dir: &str, bot: &str) -> Option<PathBuf> {
    if bot.is_empty() || bot.starts_with('.') || bot.contains(['/', '\0']) {
        return None;
    }
    let home = std::env::var_os("HOME").filter(|home| !home.is_empty())?;
    Some(PathBuf::from(home).join(".agent/sonny").join(dir).join(bot))
}

/// This window's agent's status, as reported to the multiplexer: `error`,
/// `blocked` and `asking` hold its window, any other but `clear` (the window
/// closing, which leaves the file as it was for the next) lets it go.
pub fn reported(bot: &str, state: &str) {
    if state != "clear" {
        hold(bot, true, |held| {
            held.status = matches!(state, "error" | "blocked" | "asking")
        });
    }
}

/// Whether this window shows a question its agent asked the person that
/// is still open: one holds its window, as `/status` says it needs them.
pub fn asking(bot: &str, open: bool) {
    hold(bot, false, |held| held.asking = open);
}

/// What holds this window's agent's window, as its Sonny knows it.
#[derive(Clone, Copy, Default, PartialEq)]
struct Held {
    status: bool,
    asking: bool,
}

/// What this window's Sonny knows holds its agent's window, and what it
/// last wanted the file to say.
static HELD: std::sync::Mutex<Option<(String, Held, Option<bool>)>> = std::sync::Mutex::new(None);

/// Change what holds `bot`'s window; its file is written, off the
/// screen's thread, only when whether anything holds it changes. What a
/// window that just opened has not heard yet lets nothing go: the file a
/// window before it left stands until its status says otherwise
/// (`definite`), or something this window held lets go.
fn hold(bot: &str, definite: bool, change: impl FnOnce(&mut Held)) {
    let Ok(mut last) = HELD.lock() else {
        return;
    };
    let (mut held, written) = match last.as_ref() {
        Some((was, held, written)) if was == bot => (*held, *written),
        _ => (Held::default(), None),
    };
    change(&mut held);
    let now = file_after(held.status || held.asking, written, definite);
    *last = Some((bot.to_owned(), held, now));
    if now == written {
        return;
    }
    drop(last);
    match tokio::runtime::Handle::try_current() {
        Ok(runtime) => drop(runtime.spawn_blocking(write_held)),
        Err(_) => write_held(),
    }
}

/// Make the held file say what the window wants of it now, not what it
/// wanted when this write was asked for: writes asked one after another
/// can run in any order, and each, one at a time, writes the latest.
fn write_held() {
    static WRITING: std::sync::Mutex<()> = std::sync::Mutex::new(());
    let Ok(_writing) = WRITING.lock() else {
        return;
    };
    let Some((bot, Some(holds))) = HELD.lock().ok().and_then(|last| {
        last.as_ref()
            .map(|(bot, _, written)| (bot.clone(), *written))
    }) else {
        return;
    };
    let Some(file) = in_dir("held", &bot) else {
        return;
    };
    match holds {
        true => mark(&file),
        false => {
            let _ = std::fs::remove_file(&file);
        }
    }
}

/// Whether the file says the window is held, once this window `holds` it
/// or not: `written` is what this window last made it, none before it
/// made it anything.
fn file_after(holds: bool, written: Option<bool>, definite: bool) -> Option<bool> {
    match (holds, written) {
        (true, Some(true)) | (false, Some(false)) => written,
        (false, None) if !definite => None,
        _ => Some(holds),
    }
}

/// Whether `bot`'s window last said its agent failed or needs the person.
pub fn held(bot: &str) -> bool {
    in_dir("held", bot).is_some_and(|file| file.exists())
}

/// Where a lead keeps which of its tasks' windows it closed for room: a
/// file each, written on the layout's own thread.
pub fn aside() -> crate::mux::Aside {
    crate::mux::Aside {
        was: std::sync::Arc::new(|bot| in_dir("aside", bot).is_some_and(|file| file.exists())),
        set: std::sync::Arc::new(|bot, aside| {
            let Some(file) = in_dir("aside", bot) else {
                return;
            };
            match aside {
                true => mark(&file),
                false => {
                    let _ = std::fs::remove_file(file);
                }
            }
        }),
    }
}

/// A window's marks: the last one, kept so the next waits [`EVERY`].
#[derive(Default)]
pub struct Marks {
    last: Option<Instant>,
}

impl Marks {
    /// The person used `bot`'s window just now: its file is marked, off
    /// the screen's thread, unless it was within [`EVERY`].
    pub fn used(&mut self, bot: &str) {
        if self.last.is_some_and(|last| last.elapsed() < EVERY) {
            return;
        }
        self.last = Some(Instant::now());
        let Some(file) = file(bot) else {
            return;
        };
        tokio::task::spawn_blocking(move || mark(&file));
    }
}

fn mark(file: &std::path::Path) {
    if let Some(dir) = file.parent() {
        let _ = std::fs::create_dir_all(dir);
    }
    if let Ok(opened) = std::fs::File::options()
        .create(true)
        .truncate(false)
        .write(true)
        .open(file)
    {
        let _ = opened.set_modified(SystemTime::now());
    }
}

/// When the person last used `bot`'s window, as its Sonny marked it.
pub fn when(bot: &str) -> Option<SystemTime> {
    std::fs::metadata(file(bot)?).ok()?.modified().ok()
}

/// The agent is gone: so is its file.
pub fn forget(bot: &str) {
    for dir in ["used", "held", "aside"] {
        if let Some(file) = in_dir(dir, bot) {
            let _ = std::fs::remove_file(file);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_name_no_file_can_have_has_none() {
        for name in ["", ".hidden", "../up", "a/b"] {
            assert!(file(name).is_none(), "{name}");
        }
    }

    #[test]
    fn a_window_holds_its_place_while_its_agent_failed_or_asks() {
        // A window just opened, before it hears anything, lets go of none
        // the window before it held.
        assert_eq!(file_after(false, None, false), None);
        // It failed, or waits on the person: held.
        assert_eq!(file_after(true, None, true), Some(true));
        assert_eq!(file_after(true, Some(true), false), Some(true));
        // A question answered, or a turn started: let go.
        assert_eq!(file_after(false, Some(true), false), Some(false));
        assert_eq!(file_after(false, None, true), Some(false));
    }

    #[test]
    fn a_mark_is_read_back_as_its_time() {
        let dir = std::env::temp_dir().join(format!("sonny-used-{}", std::process::id()));
        let file = dir.join("used").join("shop.fix");
        let before = SystemTime::now() - Duration::from_secs(1);
        mark(&file);
        let marked = std::fs::metadata(&file).unwrap().modified().unwrap();
        assert!(marked >= before);
        std::fs::remove_dir_all(dir).unwrap();
    }
}
