//! What files said, kept until they change: each path's metadata is
//! looked at, which opens nothing, and the files are read again only once
//! one of them changed, or a time what they say turns at came. A window
//! looked at the same files every few seconds, and an endpoint scanner
//! checks every open.
use std::path::{Path, PathBuf};
use std::sync::Mutex;

/// Where what was read is kept: a `static` beside what reads it.
pub type Slot<T> = Mutex<Option<Kept<T>>>;

/// What some files said when last read, with what identified each of
/// the paths they were read from then: kept while none of those changed,
/// and until `until`, a time in seconds when what was read changes with
/// time alone.
pub struct Kept<T> {
    key: String,
    paths: Vec<PathBuf>,
    stamps: Vec<Option<Stamp>>,
    until: Option<i64>,
    value: T,
}

/// What a file or folder's metadata says of its last change: a folder's
/// changes as an entry in it comes, goes or is renamed, which is how all
/// that writes the files read here writes one: whole beside its place,
/// then renamed there. The inode's own change time is in it too: a
/// permission put right changes nothing else, and what read as nothing
/// under the old one must be read again.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct Stamp {
    modified: std::time::SystemTime,
    len: u64,
    inode: u64,
    changed: (i64, i64),
}

pub fn stamp(path: &Path) -> Option<Stamp> {
    use std::os::unix::fs::MetadataExt;
    let meta = std::fs::metadata(path).ok()?;
    Some(Stamp {
        modified: meta.modified().ok()?,
        len: meta.len(),
        inode: meta.ino(),
        changed: (meta.ctime(), meta.ctime_nsec()),
    })
}

/// A change this recent may share its time with a later one the clock
/// cannot tell apart: such a stamp is no proof nothing changed since, so
/// what was read under it is read again next time, as Git does with a
/// racily clean index entry.
const RACY: std::time::Duration = std::time::Duration::from_secs(2);

impl Stamp {
    /// Whether what was read under this stamp may be kept: its change is
    /// older than any the clock could confuse with a later one.
    pub fn settled(&self) -> bool {
        std::time::SystemTime::now()
            .checked_sub(RACY)
            .is_some_and(|settled| self.modified < settled)
    }
}

/// `~/.agent/config.toml` as last read, read again only once it changed:
/// for what looks at it on a timer, as the lookout does.
pub fn config() -> Result<agent_client::config::Config, String> {
    static KEPT: Slot<Result<agent_client::config::Config, String>> = Mutex::new(None);
    let Some(path) = agent_client::config::path() else {
        return agent_client::config::load();
    };
    read_ok(&KEPT, "config", vec![path], || {
        (agent_client::config::load(), None)
    })
}

/// As [`read`], keeping only what read well: what could not be read is
/// read again next time, as a permission put right changes none of what a
/// stamp holds.
pub fn read_ok<T: Clone, E: Clone>(
    slot: &Slot<Result<T, E>>,
    key: &str,
    paths: Vec<PathBuf>,
    load: impl FnOnce() -> (Result<T, E>, Option<i64>),
) -> Result<T, E> {
    let value = read(slot, key, paths, load);
    if value.is_err() {
        *slot.lock().unwrap_or_else(|p| p.into_inner()) = None;
    }
    value
}

/// What `read` reads of `paths` for `key`, kept in `slot` from the last
/// read while none of the paths changed since, and read again otherwise.
pub fn read<T: Clone>(
    slot: &Slot<T>,
    key: &str,
    paths: Vec<PathBuf>,
    read: impl FnOnce() -> (T, Option<i64>),
) -> T {
    let mut slot = slot.lock().unwrap_or_else(|p| p.into_inner());
    // Stamped before the read, so a change during it is read next time.
    let stamps: Vec<Option<Stamp>> = paths.iter().map(|p| stamp(p)).collect();
    if let Some(kept) = slot.as_ref()
        && kept.key == key
        && kept.paths == paths
        && kept.stamps == stamps
        && kept.until.is_none_or(|until| now_s() < until)
    {
        return kept.value.clone();
    }
    let (value, until) = read();
    let racy = !stamps.iter().flatten().all(Stamp::settled);
    *slot = (!racy).then(|| Kept {
        key: key.to_owned(),
        paths,
        stamps,
        until,
        value: value.clone(),
    });
    value
}

fn now_s() -> i64 {
    (std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH))
        .map_or(0, |d| d.as_secs() as i64)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_config_that_could_not_be_read_is_read_again() {
        let home = std::env::temp_dir().join(format!("kept-config-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&home);
        let path = home.join(".agent/config.toml");
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        // A folder where the file goes reads as an error, as an unreadable
        // file does, and its stamp stays as it is once settled.
        std::fs::create_dir(&path).unwrap();
        let past = std::time::SystemTime::now() - std::time::Duration::from_secs(60);
        std::fs::File::open(&path)
            .unwrap()
            .set_modified(past)
            .unwrap();
        let slot: Slot<Result<u8, String>> = Mutex::new(None);
        let load = || match std::fs::read_to_string(&path) {
            Ok(_) => (Ok(1), None),
            Err(e) => (Err(e.to_string()), None),
        };
        assert!(read_ok(&slot, "c", vec![path.clone()], load).is_err());
        assert!(
            slot.lock().unwrap().is_none(),
            "nothing kept of a failed read"
        );
        // Put right with its stamp unchanged, it reads well at once.
        std::fs::remove_dir(&path).unwrap();
        std::fs::write(&path, "").unwrap();
        std::fs::File::options()
            .write(true)
            .open(&path)
            .unwrap()
            .set_modified(past)
            .unwrap();
        assert_eq!(read_ok(&slot, "c", vec![path.clone()], load), Ok(1));
        assert_eq!(
            read_ok(&slot, "c", vec![path.clone()], || (Ok(2), None)),
            Ok(1)
        );
        let _ = std::fs::remove_dir_all(&home);
    }

    #[test]
    fn files_are_read_again_only_once_their_folder_changes() {
        let dir = std::env::temp_dir().join(format!("bg-kept-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let slot = Mutex::new(None);
        let reads = std::cell::Cell::new(0);
        let count = |until| {
            reads.set(reads.get() + 1);
            (std::fs::read_dir(&dir).unwrap().count(), until)
        };
        let settle = || {
            // Changes older than the racy window, as a folder that last
            // changed a while ago is: its time set back past it.
            let file = std::fs::File::open(&dir).unwrap();
            let past = std::time::SystemTime::now() - std::time::Duration::from_secs(60);
            file.set_modified(past).unwrap();
        };
        let paths = || vec![dir.clone()];
        // Just changed: no proof a later change in the same tick is not
        // missed, so read each time until it settles.
        assert_eq!(read(&slot, "k", paths(), || count(None)), 0);
        assert_eq!(read(&slot, "k", paths(), || count(None)), 0);
        assert_eq!(reads.get(), 2);
        settle();
        assert_eq!(read(&slot, "k", paths(), || count(None)), 0);
        assert_eq!(read(&slot, "k", paths(), || count(None)), 0);
        assert_eq!(reads.get(), 3);
        // A file renamed in, as every writer here writes one.
        std::fs::write(dir.join("a.json.part"), "{}").unwrap();
        std::fs::rename(dir.join("a.json.part"), dir.join("a.json")).unwrap();
        settle();
        assert_eq!(read(&slot, "k", paths(), || count(None)), 1);
        assert_eq!(read(&slot, "k", paths(), || count(None)), 1);
        assert_eq!(reads.get(), 4);
        // Another key reads its own.
        assert_eq!(read(&slot, "other", paths(), || count(None)), 1);
        assert_eq!(reads.get(), 5);
        // What turns with time alone is read again once that time came.
        assert_eq!(read(&slot, "k", paths(), || count(Some(now_s() - 1))), 1);
        assert_eq!(read(&slot, "k", paths(), || count(None)), 1);
        assert_eq!(reads.get(), 7);
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
