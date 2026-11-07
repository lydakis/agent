//! Each lead's state folder: where it, its tasks and the agents they make
//! keep files that outlive a turn or a task, such as a monitor's
//! bookmarks or a tracker's lock. `~/.local/state/sonny/agents/NAME`,
//! under `$XDG_STATE_HOME` when that is set, as the XDG base directories
//! put state. Sonny makes it and adds it to the folders the lead's sandbox
//! and its tasks' sandboxes write, so it works in every row of
//! `/permissions`, while `~/.agents`, the person's skills, roles and
//! memory, stays read-only to agents, as Codex keeps `.agents`.
use std::path::PathBuf;

use serde_json::Value;

/// Where the state folders are, unmade.
fn root() -> Option<PathBuf> {
    base().map(|base| base.join("sonny/agents"))
}

/// The XDG state home: `$XDG_STATE_HOME` when it is an absolute path, as
/// the specification ignores a relative one, else `~/.local/state`.
#[cfg(not(test))]
fn base() -> Option<PathBuf> {
    let given = std::env::var_os("XDG_STATE_HOME").map(PathBuf::from);
    match given.filter(|path| path.is_absolute()) {
        Some(path) => Some(path),
        None => Some(PathBuf::from(std::env::var_os("HOME")?).join(".local/state")),
    }
}

/// Tests keep their state apart from the person's, and from each other's
/// runs.
#[cfg(test)]
fn base() -> Option<PathBuf> {
    Some(std::env::temp_dir().join(format!("sonny-state-{}", std::process::id())))
}

/// The state folder of the agent `name`, made when missing, readable by
/// the person alone, by its real path, as a sandbox writes it. None for
/// a name that is not one folder's, or when it can't be made.
pub fn folder(name: &str) -> Option<String> {
    use std::os::unix::fs::DirBuilderExt;
    // Bot names are 1 to 128 of `A-Z a-z 0-9 . _ -`: one folder's name,
    // unless it is only dots.
    if name.is_empty() || name.contains('/') || name.bytes().all(|b| b == b'.') {
        return None;
    }
    let folder = root()?.join(name);
    std::fs::DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(&folder)
        .ok()?;
    let real = std::fs::canonicalize(&folder).ok()?;
    real.to_str().map(str::to_owned)
}

/// Whether `path`, a real path as a bot's writable folders hold them, is
/// an agent's state folder.
pub fn within(path: &str) -> bool {
    inside(&roots(), path)
}

/// The state root as written and by its real path, where it differs, as
/// `~/.local` may be a link.
fn roots() -> Vec<PathBuf> {
    let Some(root) = root() else {
        return Vec::new();
    };
    let real = std::fs::canonicalize(&root)
        .ok()
        .filter(|real| *real != root);
    [Some(root), real].into_iter().flatten().collect()
}

/// Whether `path` is a folder right inside one of `roots`.
fn inside(roots: &[PathBuf], path: &str) -> bool {
    let path = std::path::Path::new(path);
    (roots.iter()).any(|root| {
        path.strip_prefix(root)
            .is_ok_and(|rest| rest.components().count() == 1)
    })
}

/// The state folder `record`, a bot as `inspect` gives it, writes now: the
/// first of its writable folders that is one. An agent keeps the folder it
/// was made with, its lead's for a task.
pub fn kept(record: &Value) -> Option<String> {
    let roots = roots();
    (record["settings"]["writable"]
        .as_array()
        .into_iter()
        .flatten())
    .filter_map(Value::as_str)
    .find(|path| inside(&roots, path))
    .map(str::to_owned)
}

/// The state folder of the agent `name` that `record`, as `inspect` gives
/// it, describes: the one it has, else that of the agent that made it,
/// its lead for a task, else its own; made when missing.
pub fn of(record: &Value, name: &str) -> Option<String> {
    kept(record).or_else(|| folder(record["created_by"].as_str().unwrap_or(name)))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn a_lead_gets_its_own_folder_made_for_it_alone() {
        use std::os::unix::fs::PermissionsExt;
        let made = folder("shop.lead").unwrap();
        assert!(made.ends_with("/sonny/agents/shop.lead"));
        let meta = std::fs::metadata(&made).unwrap();
        assert!(meta.is_dir());
        assert_eq!(meta.permissions().mode() & 0o777, 0o700);
        assert!(within(&made));
        // Made again, the same folder, as it was.
        assert_eq!(folder("shop.lead").unwrap(), made);
    }

    #[test]
    fn a_name_that_is_not_one_folder_gets_none() {
        for name in ["", ".", "..", "a/b"] {
            assert_eq!(folder(name), None, "{name}");
        }
        assert!(!within("/tmp"));
        let made = folder("deep.lead").unwrap();
        // Only a folder right inside the root is a state folder.
        assert!(!within(&format!("{made}/jobs")));
        assert!(!within(
            std::path::Path::new(&made)
                .parent()
                .unwrap()
                .to_str()
                .unwrap()
        ));
    }

    #[test]
    fn an_agent_keeps_the_folder_it_was_made_with() {
        let lead = folder("calc.lead").unwrap();
        let task = json!({"settings": {"writable": ["/repo/.git", lead]}});
        assert_eq!(kept(&task).as_deref(), Some(lead.as_str()));
        assert_eq!(of(&task, "calc.fix").as_deref(), Some(lead.as_str()));
        // One made before state folders takes its maker's, and a lead its
        // own.
        let older = json!({"settings": {"writable": ["/repo/.git"]}, "created_by": "calc.lead"});
        assert_eq!(kept(&older), None);
        assert_eq!(of(&older, "calc.fix").as_deref(), Some(lead.as_str()));
        let alone = json!({"settings": {"writable": []}});
        assert_eq!(of(&alone, "calc.lead").as_deref(), Some(lead.as_str()));
    }
}
