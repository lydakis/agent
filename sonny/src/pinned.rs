//! **Pinned**: the few agents the person wants closer than their project,
//! numbered at the top of **Projects**, where `1` to `9` open them. So a
//! pinned agent is `←` then its number: two keys with no modifier.
//!
//! The pins are one small file of Sonny's own beside the store, the
//! agents' names in the order they were pinned, one a line, replaced by a
//! rename so two windows never leave half of one. Every window shares it,
//! and it is kept until the person unpins. A window holds the names in
//! memory, so a number never waits: the file is read and written beside
//! the window, never on the path that draws and takes keys. Nothing pins
//! or unpins by itself: an agent that is done, or not there any more,
//! keeps its pin until the person unpins it.
use std::path::{Path, PathBuf};

/// The most agents pinned at once: a number each, `1` to `9`.
pub const MOST: usize = 9;

/// The file beside `store` (as the daemon's store is found, canonical)
/// that keeps the pins.
pub fn file(store: &Path) -> PathBuf {
    let store = agent_client::socket::canonical_store(store).unwrap_or_else(|_| store.to_owned());
    let mut file = store.into_os_string();
    file.push(".pinned");
    PathBuf::from(file)
}

/// A pin or an unpin, as the person asked for it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Change {
    Pin(String),
    Unpin(String),
}

/// What a pin or an unpin came to.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Came {
    /// Pinned as this number.
    Pinned(usize),
    Unpinned,
    /// Nine are pinned already.
    Full,
}

impl Came {
    /// How Sonny answers it.
    pub fn said(&self) -> String {
        match self {
            Came::Pinned(n) => format!("pinned as {n} · ← {n} opens it"),
            Came::Unpinned => "unpinned".into(),
            Came::Full => format!("{MOST} pinned · unpin one first"),
        }
    }
}

/// `change` made to `names`: a pin goes last, an unpin moves the ones
/// after it up one. Pinning one pinned already, or unpinning one that is
/// not, changes nothing.
pub fn apply(names: &mut Vec<String>, change: &Change) -> Came {
    match change {
        Change::Pin(name) => match names.iter().position(|pinned| pinned == name) {
            Some(at) => Came::Pinned(at + 1),
            None if names.len() >= MOST => Came::Full,
            None => {
                names.push(name.clone());
                Came::Pinned(names.len())
            }
        },
        Change::Unpin(name) => {
            names.retain(|pinned| pinned != name);
            Came::Unpinned
        }
    }
}

/// The names `text` keeps, in order: one a line, each once, at most
/// [`MOST`].
fn parse(text: &str) -> Vec<String> {
    let mut names: Vec<String> = Vec::new();
    for line in text.lines().map(str::trim).filter(|line| !line.is_empty()) {
        if names.len() < MOST && !names.iter().any(|name| name == line) {
            names.push(line.to_owned());
        }
    }
    names
}

/// The pins `file` keeps; none where it is missing or can't be read.
pub fn read(file: &Path) -> Vec<String> {
    std::fs::read_to_string(file)
        .map(|text| parse(&text))
        .unwrap_or_default()
}

/// `names` written to `file` whole, by a rename: only the person reads it.
fn write(file: &Path, names: &[String]) -> Result<(), String> {
    let mut text = names.join("\n");
    if !text.is_empty() {
        text.push('\n');
    }
    agent_client::files::replace_mode(file, text.as_bytes(), 0o600)
}

/// `changes` made to the pins `file` keeps now, in order, and the file
/// replaced with what they came to, unless none changed it: so another
/// window's pins made meanwhile are kept. The pins as they stand after.
fn changed(file: &Path, changes: &[Change]) -> Vec<String> {
    // One window's changes reach the file one at a time.
    static ONE: std::sync::Mutex<()> = std::sync::Mutex::new(());
    let _one = ONE.lock().unwrap_or_else(|e| e.into_inner());
    let mut names = read(file);
    let was = names.clone();
    for change in changes {
        apply(&mut names, change);
    }
    if names != was {
        let _ = write(file, &names);
    }
    names
}

/// A window's pins: the names as it last knew them, read and written
/// beside it.
#[derive(Default)]
pub struct Pins {
    file: Option<PathBuf>,
    names: Vec<String>,
    /// The read or write under way, beside the window: what the file
    /// held after it.
    job: Option<tokio::task::JoinHandle<Vec<String>>>,
    /// Changes made since that job started, for the next one.
    waiting: Vec<Change>,
    /// Whether the names changed since [`Pins::moved`] last said so.
    moved: bool,
}

impl Pins {
    /// The pins kept beside `store`, not read yet.
    pub fn new(store: &Path) -> Self {
        Pins {
            file: (!store.as_os_str().is_empty()).then(|| file(store)),
            ..Default::default()
        }
    }

    /// The names, in their order: the first is `1`.
    pub fn names(&self) -> &[String] {
        &self.names
    }

    pub fn has(&self, name: &str) -> bool {
        self.names.iter().any(|pinned| pinned == name)
    }

    /// Read the file again beside the window, as another window may have
    /// pinned since; what it read lands with [`Pins::landed`].
    pub fn read_again(&mut self, woken: &std::sync::Arc<tokio::sync::Notify>) {
        if self.job.is_none() {
            self.start(Vec::new(), woken);
        }
    }

    /// `name` pinned, or unpinned where it is: at once here, and in the
    /// file beside the window.
    pub fn toggle(&mut self, name: &str, woken: &std::sync::Arc<tokio::sync::Notify>) -> Came {
        let change = match self.has(name) {
            true => Change::Unpin(name.to_owned()),
            false => Change::Pin(name.to_owned()),
        };
        let came = apply(&mut self.names, &change);
        if came != Came::Full {
            self.moved = true;
            self.waiting.push(change);
            if self.job.is_none() {
                let changes = std::mem::take(&mut self.waiting);
                self.start(changes, woken);
            }
        }
        came
    }

    fn start(&mut self, changes: Vec<Change>, woken: &std::sync::Arc<tokio::sync::Notify>) {
        let Some(file) = self.file.clone() else {
            return;
        };
        let woken = woken.clone();
        self.job = Some(tokio::task::spawn_blocking(move || {
            let names = changed(&file, &changes);
            woken.notify_one();
            names
        }));
    }

    /// What the read or write beside the window found, once it ended: the
    /// names as the file holds them, unless changes made since wait, which
    /// go to the file next. Whether the names changed.
    pub fn landed(&mut self, woken: &std::sync::Arc<tokio::sync::Notify>) -> bool {
        if let Some(mut job) = self.job.take_if(|job| job.is_finished()) {
            // The job is over, so this does not wait.
            let mut cx = std::task::Context::from_waker(std::task::Waker::noop());
            match std::future::Future::poll(std::pin::Pin::new(&mut job), &mut cx) {
                std::task::Poll::Ready(Ok(names)) => {
                    if self.waiting.is_empty() && names != self.names {
                        self.names = names;
                        self.moved = true;
                    }
                }
                std::task::Poll::Ready(Err(_)) => {}
                // Its task's budget spent: taken at the next look.
                std::task::Poll::Pending => self.job = Some(job),
            }
        }
        if self.job.is_none() && !self.waiting.is_empty() {
            let changes = std::mem::take(&mut self.waiting);
            self.start(changes, woken);
        }
        std::mem::take(&mut self.moved)
    }

    /// The read or write under way, and any that waits on it, run to
    /// their end, what they found taken as [`Pins::landed`] takes it.
    #[cfg(test)]
    pub async fn settled(&mut self, woken: &std::sync::Arc<tokio::sync::Notify>) {
        while let Some(job) = &self.job {
            while !job.is_finished() {
                tokio::task::yield_now().await;
            }
            let moved = self.landed(woken);
            self.moved |= moved;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_pin_goes_last_an_unpin_moves_the_rest_up_and_a_tenth_is_refused() {
        let mut names = Vec::new();
        for (n, name) in ["a.lead", "a.x", "b.lead"].iter().enumerate() {
            assert_eq!(
                apply(&mut names, &Change::Pin((*name).into())),
                Came::Pinned(n + 1)
            );
        }
        assert_eq!(
            Came::Pinned(3).said(),
            "pinned as 3 · ← 3 opens it".to_owned()
        );
        assert_eq!(
            apply(&mut names, &Change::Unpin("a.x".into())),
            Came::Unpinned
        );
        assert_eq!(names, ["a.lead", "b.lead"]);
        // Again, it goes last.
        apply(&mut names, &Change::Pin("a.x".into()));
        assert_eq!(names, ["a.lead", "b.lead", "a.x"]);
        for n in 0..6 {
            apply(&mut names, &Change::Pin(format!("c.{n}")));
        }
        assert_eq!(names.len(), MOST);
        assert_eq!(apply(&mut names, &Change::Pin("d.x".into())), Came::Full);
        assert_eq!(Came::Full.said(), "9 pinned · unpin one first");
        assert_eq!(names.len(), MOST);
    }

    #[test]
    fn the_file_is_names_a_line_each_once_at_most_nine() {
        assert_eq!(parse("a.lead\n\n a.x \na.lead\n"), ["a.lead", "a.x"]);
        let many: String = (0..12).map(|n| format!("p.{n}\n")).collect();
        assert_eq!(parse(&many).len(), MOST);
        let dir = std::env::temp_dir().join(format!("sonny-pins-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let file = dir.join("state.sqlite.pinned");
        assert!(read(&file).is_empty());
        let names = changed(&file, &[Change::Pin("a.lead".into())]);
        assert_eq!(names, ["a.lead"]);
        assert_eq!(std::fs::read_to_string(&file).unwrap(), "a.lead\n");
        // Another window's pin is kept: the change is made to the file.
        let names = changed(&file, &[Change::Pin("b.x".into())]);
        assert_eq!(names, ["a.lead", "b.x"]);
        let names = changed(&file, &[Change::Unpin("a.lead".into())]);
        assert_eq!(read(&file), names);
        assert_eq!(names, ["b.x"]);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
