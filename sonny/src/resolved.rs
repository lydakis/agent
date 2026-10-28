//! Tasks marked resolved, as threads are: a task resolves itself once
//! nothing is left to do in it, and its lead can resolve any of its tasks,
//! both with the `resolve_task` tool Sonny declares, and `/status` files
//! the task out of the way. A task's own call is read from its history: its
//! newest turn resolved it. The lead's Sonny keeps a mark for each of the
//! lead's calls beside the store, read from the lead's history as the
//! calls land: the task's newest turn finished by then, so one still
//! running is not resolved. Turns are numbered across the store, so a task
//! turn numbered after it, new work for the task or another bot under its
//! name, reopens the task without anyone taking the mark off.
use serde_json::{Value, json};
use std::collections::HashSet;
use std::path::{Path, PathBuf};

/// The name of the tool a task or its lead resolves the task with.
pub const TOOL: &str = "resolve_task";

/// The `resolve_task` tool as Sonny declares it on a lead, which the
/// tasks it starts take too, and on a task.
pub fn tool() -> Value {
    json!({
        "name": TOOL,
        "description": "Mark a task resolved once nothing is left to do in it: its pull request merged, \
            its answer passed on, or the person dropped the work. As a task, resolve yourself, naming no \
            task, in the turn that finishes the work; as a lead, name one of your tasks. The person's status \
            view files it under Resolved, out of their way, and its window closes once its turn ends. New \
            work sent to the task reopens it. A task that waits on the person, or on work still to come, is \
            not resolved.",
        "parameters": {
            "type": "object",
            "properties": {
                "task": {"type": "string", "description": "The task's name, as it was started; none to resolve yourself."}
            },
            "additionalProperties": false
        },
        "result": "Resolved."
    })
}

/// Whether a `resolve_task` call with `arguments`, made by `bot`, resolves
/// `bot` itself: it names no task, or its own name, whole or past its
/// project's, which may hold dots too.
pub fn itself(arguments: &Value, bot: &str) -> bool {
    match arguments["task"].as_str().map(str::trim) {
        None | Some("") => true,
        Some(task) => (bot.strip_suffix(task))
            .is_some_and(|project| project.is_empty() || project.ends_with('.')),
    }
}

/// Tasks' turns that resolved the task, followed in the daemon's events:
/// a call that names no task or its own and did not fail.
#[derive(Default)]
pub struct Own {
    /// Calls under way, by bot and call.
    calls: HashSet<(String, String)>,
    /// Turns that resolved their bot, by bot and turn.
    turns: HashSet<(String, i64)>,
}

impl Own {
    /// Follow `event`, of any bot but `lead`.
    pub fn event(&mut self, event: &Value, lead: &str) {
        let (Some(bot), Some(turn)) = (event["bot"].as_str(), event["turn"].as_i64()) else {
            return;
        };
        let data = &event["data"];
        let call = (
            bot.to_owned(),
            data["call_id"].as_str().unwrap_or("").to_owned(),
        );
        match event["event"].as_str() {
            Some("tool_started") if data["name"] == TOOL && bot != lead => {
                let arguments = data["arguments"]
                    .as_str()
                    .and_then(|a| serde_json::from_str(a).ok())
                    .unwrap_or(Value::Null);
                if itself(&arguments, bot) {
                    self.calls.insert(call);
                }
            }
            Some("tool_completed") if self.calls.remove(&call) && data["failed"] != true => {
                self.turns.insert((bot.to_owned(), turn));
            }
            _ => {}
        }
    }

    /// Whether `bot`'s turn `turn` resolved it, asked once it ended.
    pub fn ended(&mut self, bot: &str, turn: i64) -> bool {
        self.turns.remove(&(bot.to_owned(), turn))
    }
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

/// The newest turn of `task` its resolving covered, if it was resolved.
pub fn read(store: &Path, task: &str) -> Option<i64> {
    kept(store, task)?["turn"].as_i64()
}

fn kept(store: &Path, task: &str) -> Option<Value> {
    serde_json::from_slice(&std::fs::read(file(store, task)).ok()?).ok()
}

/// Whether the mark kept for `task` is the one the lead's call `call`
/// made, which a replay of that call leaves as it is.
pub fn made_by(store: &Path, task: &str, call: &str) -> bool {
    kept(store, task).is_some_and(|mark| mark["call"] == call)
}

/// Whether a task resolved up to its turn `mark` still is: none of its
/// turns, newest `newest`, came after, and none runs.
pub fn holds(mark: Option<i64>, newest: Option<i64>, running: bool) -> bool {
    mark.is_some_and(|mark| !running && newest.is_none_or(|turn| turn <= mark))
}

/// The newest of a task's `turns`, as the daemon lists them, finished by
/// `at` (ms): what resolving it then covers. A task with none covers 0.
pub fn upto(turns: &[Value], at: i64) -> i64 {
    turns
        .iter()
        .filter(|turn| {
            turn["finished_ms"]
                .as_i64()
                .is_some_and(|ended| ended <= at)
        })
        .filter_map(|turn| turn["turn"].as_i64())
        .max()
        .unwrap_or(0)
}

/// Note `task` resolved up to its turn `turn` by the lead's call `call`,
/// unless it was resolved past that already.
pub fn keep(store: &Path, task: &str, turn: i64, call: &str) -> Result<(), String> {
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
    std::fs::write(&next, json!({"turn": turn, "call": call}).to_string()).map_err(failed)?;
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
    fn a_task_resolves_itself_by_naming_no_task_or_its_own() {
        let by = |arguments: Value| itself(&arguments, "calc.fix");
        assert!(by(json!({})));
        assert!(by(json!({"task": " "})));
        assert!(by(json!({"task": "fix"})));
        assert!(by(json!({"task": "calc.fix"})));
        assert!(!by(json!({"task": "docs"})));
        assert!(!by(json!({"task": "ix"})));
        // A project's name may hold dots.
        assert!(itself(&json!({"task": "fix"}), "shop.web.fix"));
    }

    #[test]
    fn a_task_turn_that_resolved_itself_is_followed_in_events() {
        let mut own = Own::default();
        let ev = |bot: &str, turn: i64, kind: &str, data: Value| json!({"bot": bot, "turn": turn, "event": kind, "data": data});
        let started = |call: &str, arguments: &str| json!({"name": TOOL, "call_id": call, "arguments": arguments});
        for (bot, turn, call, arguments, failed) in [
            ("calc.fix", 5, "c1", "{}", false),
            ("calc.docs", 6, "c1", "{\"task\":\"fix\"}", false),
            ("calc.bench", 7, "c1", "{}", true),
            ("calc.lead", 8, "c1", "{}", false),
        ] {
            own.event(
                &ev(bot, turn, "tool_started", started(call, arguments)),
                "calc.lead",
            );
            own.event(
                &ev(
                    bot,
                    turn,
                    "tool_completed",
                    json!({"call_id": call, "failed": failed}),
                ),
                "calc.lead",
            );
        }
        assert!(own.ended("calc.fix", 5));
        assert!(!own.ended("calc.fix", 5));
        // Another task named, a failed call, the lead itself: none.
        assert!(!own.ended("calc.docs", 6));
        assert!(!own.ended("calc.bench", 7));
        assert!(!own.ended("calc.lead", 8));
    }

    #[test]
    fn a_mark_holds_until_the_task_has_a_turn_after_it() {
        assert!(holds(Some(10), Some(10), false));
        assert!(holds(Some(0), None, false));
        // New work since, running or ended.
        assert!(!holds(Some(10), Some(12), false));
        assert!(!holds(Some(10), Some(10), true));
        assert!(!holds(None, Some(7), false));
    }

    #[test]
    fn resolving_covers_the_turns_finished_by_then() {
        // Newest first; a task the lead waited on in the resolving turn
        // has a turn numbered after the lead's, finished before the call.
        let turns = [
            json!({"turn": 16, "finished_ms": null}),
            json!({"turn": 14, "finished_ms": 900}),
            json!({"turn": 9, "finished_ms": 500}),
        ];
        assert_eq!(upto(&turns, 950), 14);
        assert_eq!(upto(&turns, 600), 9);
        assert_eq!(upto(&turns, 100), 0);
        assert_eq!(upto(&[], 100), 0);
    }

    #[test]
    fn marks_are_kept_beside_the_store_and_only_move_on() {
        let dir = std::env::temp_dir().join(format!("sonny-resolved-{}", std::process::id()));
        let store = dir.join("state.sqlite");
        keep(&store, "calc.fix", 9, "r1").unwrap();
        assert_eq!(read(&store, "calc.fix"), Some(9));
        assert!(made_by(&store, "calc.fix", "r1"));
        assert!(dir.join("state.sqlite.resolved/calc.fix.json").exists());
        // A replay of an older resolve leaves the newer one.
        keep(&store, "calc.fix", 4, "r0").unwrap();
        assert_eq!(read(&store, "calc.fix"), Some(9));
        assert!(!made_by(&store, "calc.fix", "r0"));
        keep(&store, "calc.fix", 15, "r2").unwrap();
        assert_eq!(read(&store, "calc.fix"), Some(15));
        assert_eq!(read(&store, "calc.other"), None);
        let _ = std::fs::remove_dir_all(dir);
    }
}
