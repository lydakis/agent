//! Tasks marked resolved: the lead calls the `resolve_task` tool Sonny
//! declares on it once nothing is left to do in a task, such as when its
//! pull request merged or the work was dropped, and `/status` files the
//! task out of the way. The lead's Sonny keeps each mark beside the store,
//! read from the lead's history as the calls land, by the lead's turn that
//! made it: turns are numbered across the store, so a task turn numbered
//! after it, new work for the task or another bot under its name, reopens
//! the task without anyone taking the mark off.
use serde_json::{Value, json};
use std::path::{Path, PathBuf};

/// The name of the tool the lead resolves a task with.
pub const TOOL: &str = "resolve_task";

/// The `resolve_task` tool as Sonny declares it on a lead.
pub fn tool() -> Value {
    json!({
        "name": TOOL,
        "description": "Mark one of your tasks resolved once nothing is left to do in it: its pull request \
            merged, its answer passed on, or the person dropped the work. The person's status view files \
            it under Resolved, out of their way, and its window closes. Sending the task new work reopens \
            it. A task that is still working, or waits on the person, is not resolved.",
        "parameters": {
            "type": "object",
            "properties": {
                "task": {"type": "string", "description": "The task's name, as you started it."}
            },
            "required": ["task"],
            "additionalProperties": false
        },
        "result": "Resolved."
    })
}

/// The task a `resolve_task` call with `arguments` names, in full, for
/// the lead of `project`.
pub fn call(arguments: &Value, project: &str) -> Option<String> {
    let task = arguments["task"].as_str()?.trim();
    if task.is_empty() || task.contains('/') {
        return None;
    }
    Some(match task.starts_with(&format!("{project}.")) {
        true => task.to_owned(),
        false => format!("{project}.{task}"),
    })
}

/// Where the marks are kept: beside the store, one file per task.
fn file(store: &Path, task: &str) -> PathBuf {
    let mut dir = store.as_os_str().to_owned();
    dir.push(".resolved");
    PathBuf::from(dir).join(format!("{task}.json"))
}

/// The lead's turn that resolved `task`, if one did.
pub fn read(store: &Path, task: &str) -> Option<i64> {
    let value: Value = serde_json::from_slice(&std::fs::read(file(store, task)).ok()?).ok()?;
    value["turn"].as_i64()
}

/// Whether a task resolved in the lead's turn `mark` still is: none of its
/// turns, newest `newest`, came after, and none runs.
pub fn holds(mark: Option<i64>, newest: Option<i64>, running: bool) -> bool {
    mark.is_some_and(|mark| !running && newest.is_none_or(|turn| turn < mark))
}

/// Note `task` resolved in the lead's turn `turn`, unless a later turn of
/// the lead's resolved it already.
pub fn keep(store: &Path, task: &str, turn: i64) -> Result<(), String> {
    if read(store, task).is_some_and(|kept| kept >= turn) {
        return Ok(());
    }
    let path = file(store, task);
    let failed = |e: std::io::Error| format!("{}: {e}", path.display());
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir).map_err(failed)?;
    }
    // Its own, so two windows never write one file together.
    let next = path.with_extension(format!("json.{}.next", std::process::id()));
    std::fs::write(&next, json!({"turn": turn}).to_string()).map_err(failed)?;
    std::fs::rename(&next, &path).map_err(failed)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_task_is_named_in_full_for_its_lead() {
        let named = |task: &str| call(&json!({"task": task}), "calc");
        assert_eq!(named("fix"), Some("calc.fix".into()));
        assert_eq!(named(" calc.fix "), Some("calc.fix".into()));
        assert_eq!(named(""), None);
        assert_eq!(named("../x"), None);
        assert_eq!(call(&json!({}), "calc"), None);
    }

    #[test]
    fn a_mark_holds_until_the_task_has_a_turn_after_it() {
        assert!(holds(Some(10), Some(7), false));
        assert!(holds(Some(10), None, false));
        // New work since, running or ended.
        assert!(!holds(Some(10), Some(12), false));
        assert!(!holds(Some(10), Some(7), true));
        assert!(!holds(None, Some(7), false));
    }

    #[test]
    fn marks_are_kept_beside_the_store_and_only_move_on() {
        let dir = std::env::temp_dir().join(format!("sonny-resolved-{}", std::process::id()));
        let store = dir.join("state.sqlite");
        keep(&store, "calc.fix", 9).unwrap();
        assert_eq!(read(&store, "calc.fix"), Some(9));
        assert!(dir.join("state.sqlite.resolved/calc.fix.json").exists());
        // A replay of an older resolve leaves the newer one.
        keep(&store, "calc.fix", 4).unwrap();
        assert_eq!(read(&store, "calc.fix"), Some(9));
        keep(&store, "calc.fix", 15).unwrap();
        assert_eq!(read(&store, "calc.fix"), Some(15));
        assert_eq!(read(&store, "calc.other"), None);
        let _ = std::fs::remove_dir_all(dir);
    }
}
