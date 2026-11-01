//! When the person last used each agent's window, for a lead that keeps
//! only so many of its tasks' windows open ([`crate::mux::Room`]): every
//! window's Sonny marks its agent's file in `~/.agent/sonny/used/` when the
//! person focuses, types or clicks there, at most once a [`EVERY`], and the
//! lead reads the file's time only when it needs room. A file is a
//! window's own, so two windows never write one line of the same file.
use std::{
    path::PathBuf,
    time::{Duration, Instant, SystemTime},
};

/// How often at most a window marks its agent used.
pub const EVERY: Duration = Duration::from_secs(10);

/// The agent's file, under `HOME`; none for a name no file can have.
fn file(bot: &str) -> Option<PathBuf> {
    if bot.is_empty() || bot.starts_with('.') || bot.contains(['/', '\0']) {
        return None;
    }
    let home = std::env::var_os("HOME").filter(|home| !home.is_empty())?;
    Some(PathBuf::from(home).join(".agent/sonny/used").join(bot))
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
    if let Some(file) = file(bot) {
        let _ = std::fs::remove_file(file);
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
