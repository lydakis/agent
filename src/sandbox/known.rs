//! The daemons the person runs on this machine, as each lists itself in
//! one folder (`~/.agent/run/daemons`, which no sandbox may write): its
//! store and its socket, a file each. A daemon keeps its own store
//! read-only and its own socket out of reach of every command it
//! sandboxes; with this list it does the same for every other daemon's, so
//! a second daemon is no way out of the first one's sandbox, nor the first
//! out of the second's. A daemon lists itself as it opens its store, and
//! the entry stays while the store does, so an idle store is kept too;
//! entries whose store is gone are dropped by the next daemon to list
//! itself. A daemon that cannot write the folder, as one started inside a
//! sandbox cannot, is not listed: it can do nothing its sandbox does not
//! allow already.

use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime};

/// One daemon: its store and, when it listens on one, its socket, each by
/// its real path.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Known {
    pub store: PathBuf,
    #[serde(default)]
    pub socket: Option<PathBuf>,
}

/// The most an entry may hold: a store's path and a socket's.
const ENTRY_BYTES: u64 = 16 * 1024;

/// A folder changed this recently may change again within its timestamp's
/// resolution, so its list is read again rather than trusted, as git
/// distrusts an index written in the same second as a file.
const SETTLE: Duration = Duration::from_secs(2);

/// List `known` in `folder`, dropping the entries whose store is gone.
pub fn record(folder: &Path, known: &Known) -> std::io::Result<()> {
    use std::io::Write;
    use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt};
    std::fs::DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(folder)?;
    for entry in std::fs::read_dir(folder)?.flatten() {
        let path = entry.path();
        if path
            .extension()
            .is_some_and(|extension| extension == "json")
            && let Some(listed) = read(&path)
            && std::fs::symlink_metadata(&listed.store).is_err()
        {
            let _ = std::fs::remove_file(&path);
        }
    }
    let name = format!(
        "{:016x}.json",
        fnv(known.store.as_os_str().as_encoded_bytes())
    );
    let partial = folder.join(format!(".{name}.{}", std::process::id()));
    let mut file = std::fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .mode(0o600)
        .open(&partial)?;
    file.write_all(&serde_json::to_vec(known).map_err(std::io::Error::other)?)?;
    file.sync_all()?;
    std::fs::rename(&partial, folder.join(name))
}

/// The daemons listed in `folder`, its own (`store`) left out: read again
/// only when the folder changed, so each command's sandbox costs one
/// `stat` while no daemon starts.
pub fn others(folder: &Path, store: Option<&Path>) -> Arc<Vec<Known>> {
    type Cache = Option<(PathBuf, SystemTime, Arc<Vec<Known>>)>;
    static CACHE: Mutex<Cache> = Mutex::new(None);
    let Ok(changed) = std::fs::metadata(folder).and_then(|meta| meta.modified()) else {
        return Arc::default();
    };
    let settled = SystemTime::now()
        .duration_since(changed)
        .is_ok_and(|age| age >= SETTLE);
    let mut cache = CACHE.lock().unwrap_or_else(|e| e.into_inner());
    let listed = match &*cache {
        Some((cached, at, listed)) if settled && cached == folder && *at == changed => {
            listed.clone()
        }
        _ => {
            let listed = Arc::new(list(folder));
            *cache = Some((folder.to_owned(), changed, listed.clone()));
            listed
        }
    };
    drop(cache);
    match store {
        Some(store) if listed.iter().any(|known| known.store == store) => Arc::new(
            (listed.iter())
                .filter(|known| known.store != store)
                .cloned()
                .collect(),
        ),
        _ => listed,
    }
}

fn list(folder: &Path) -> Vec<Known> {
    let mut listed: Vec<Known> = std::fs::read_dir(folder)
        .into_iter()
        .flatten()
        .flatten()
        .map(|entry| entry.path())
        .filter(|path| {
            path.extension()
                .is_some_and(|extension| extension == "json")
        })
        .filter_map(|path| read(&path))
        .collect();
    listed.sort_by(|a, b| a.store.cmp(&b.store));
    listed
}

/// An entry, if it is one: a regular file of at most [`ENTRY_BYTES`]
/// naming absolute paths.
fn read(path: &Path) -> Option<Known> {
    use std::io::Read;
    let file = std::fs::File::open(path).ok()?;
    if !file.metadata().ok()?.is_file() {
        return None;
    }
    let mut text = Vec::new();
    file.take(ENTRY_BYTES).read_to_end(&mut text).ok()?;
    let known: Known = serde_json::from_slice(&text).ok()?;
    (known.store.is_absolute() && known.socket.as_deref().is_none_or(Path::is_absolute))
        .then_some(known)
}

/// FNV-1a, 64 bits: an entry's name from its store's path, the same in
/// every daemon, so a daemon started again on a store replaces its entry.
fn fnv(bytes: &[u8]) -> u64 {
    let mut hash = 0xcbf29ce484222325u64;
    for byte in bytes {
        hash ^= u64::from(*byte);
        hash = hash.wrapping_mul(0x100000001b3);
    }
    hash
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_daemon_lists_itself_once_and_reads_only_the_others() {
        let base = std::env::temp_dir().join(format!("agent-known-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&base);
        std::fs::create_dir_all(&base).unwrap();
        let folder = base.join("daemons");
        let (one, two) = (base.join("one.sqlite"), base.join("two.sqlite"));
        std::fs::write(&one, "").unwrap();
        std::fs::write(&two, "").unwrap();
        let first = Known {
            store: one.clone(),
            socket: Some(base.join("one.sock")),
        };
        let second = Known {
            store: two.clone(),
            socket: None,
        };
        record(&folder, &first).unwrap();
        record(&folder, &first).unwrap();
        record(&folder, &second).unwrap();
        assert_eq!(list(&folder), [first.clone(), second.clone()]);
        assert_eq!(*others(&folder, Some(&one)), std::slice::from_ref(&second));
        assert_eq!(*others(&folder, Some(&two)), std::slice::from_ref(&first));
        // A store that is gone is dropped by the next daemon to list itself.
        std::fs::remove_file(&two).unwrap();
        record(&folder, &first).unwrap();
        assert_eq!(list(&folder), [first]);
        // What is no entry is skipped: another file, a relative path.
        std::fs::write(folder.join("x.json"), r#"{"store": "rel.sqlite"}"#).unwrap();
        std::fs::write(folder.join("note.txt"), "{}").unwrap();
        assert_eq!(list(&folder).len(), 1);
        // No folder, no others.
        assert!(others(&base.join("none"), None).is_empty());
        std::fs::remove_dir_all(base).unwrap();
    }
}
