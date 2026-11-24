//! Done is closed. An agent is done once it is closed, as the daemon
//! closes it: by itself, naming its own name alone to `close_agent`, which
//! lets the turn closing it run on to its end; by the agent that made it,
//! a task by its lead; by `remove_task`; or by the person. Its history
//! stays, and a message that opens a turn of its own opens it again. Every
//! surface reads done from the daemon (a row's `closed_ms`), so it holds
//! with no window open and one word means it everywhere. `/status` files a
//! closed agent under Done, out of the way, a done lead's automations
//! pause until a message opens it, and closing ends its follows, so news
//! does not open it again unasked.
//!
//! The `mark_done` tool Sonny once declared is gone: its calls still read
//! in old histories ([`OLD`]), and the marks it kept beside the store were
//! made closes once ([`marks_close`]), the marks left where they were.
use serde_json::{Value, json};
use std::collections::HashSet;
use std::path::{Path, PathBuf};

/// The daemon's tool an agent closes itself or another with.
pub const TOOL: &str = "close_agent";

/// Whether a `close_agent` call with `arguments`, made by `bot`, closes
/// `bot` itself: its own name, whole, alone, as the daemon closes it.
pub fn itself(arguments: &Value, bot: &str) -> bool {
    match (arguments["name"].as_str(), arguments["names"].as_array()) {
        (Some(name), None) => name == bot,
        (None, Some(names)) => names.len() == 1 && names[0] == bot,
        _ => false,
    }
}

/// The agents a `close_agent` call's `output` says it closed, each by its
/// name: those it refused stay open.
pub fn closed_by(output: &str) -> Vec<String> {
    let output: Value = serde_json::from_str(output).unwrap_or_default();
    (output["closed"].as_array().into_iter().flatten())
        .filter_map(|closed| Some(closed["bot"].as_str()?.to_owned()))
        .collect()
}

/// Whether an agent is done: closed, and no turn of its runs, as the turn
/// that closed it runs on to its end.
pub fn done(closed: bool, running: bool) -> bool {
    closed && !running
}

/// Whether the daemon's record or row `record` says the agent is closed.
pub fn closed(record: &Value) -> bool {
    !record["closed_ms"].is_null()
}

/// Agents' turns that closed themselves, followed in the daemon's events:
/// a `close_agent` call naming its own agent alone that did not fail.
#[derive(Default)]
pub struct Own {
    /// Calls under way, by bot and call.
    calls: HashSet<(String, String)>,
    /// Turns that closed their bot, by bot and turn.
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

    /// Whether `bot`'s turn `turn` closed it, asked once it ended.
    pub fn ended(&mut self, bot: &str, turn: i64) -> bool {
        self.turns.remove(&(bot.to_owned(), turn))
    }
}

/// Why a lead's automation does not run now: it is closed, by itself or
/// by whoever made it, and no turn of its since opened it again. None for
/// any other agent, or a lead that is open.
pub fn paused(lead: &str, closed: bool) -> Option<String> {
    (crate::project::is_lead(lead) && closed)
        .then(|| format!("{lead} is done; its automations run again once a message reopens it"))
}

/// The marks of `mark_done` became closes (after 0.10.5).
pub const MARKS_CLOSE: &str = "done-marks-close";

/// Where `mark_done` kept its marks: beside the store, one file per task.
fn file(store: &Path, task: &str) -> PathBuf {
    let mut dir = store.as_os_str().to_owned();
    dir.push(".resolved");
    PathBuf::from(dir).join(format!("{task}.json"))
}

/// Whether any task has a mark kept beside `store`: one look at the
/// folder, so a store with none closes nothing and lists nothing for it.
pub fn any_marked(store: &Path) -> bool {
    !marked(store).is_empty()
}

/// The tasks with a mark kept beside `store`, by name.
fn marked(store: &Path) -> HashSet<String> {
    let folder = file(store, "x");
    let entries = folder.parent().and_then(|dir| std::fs::read_dir(dir).ok());
    (entries.into_iter().flatten().flatten())
        .filter_map(|entry| {
            let name = entry.file_name().into_string().ok()?;
            Some(name.strip_suffix(".json")?.to_owned())
        })
        .collect()
}

/// The newest turn of `task` its mark covered, if it was marked.
fn read(store: &Path, task: &str) -> Option<i64> {
    let kept: Value = serde_json::from_slice(&std::fs::read(file(store, task)).ok()?).ok()?;
    kept["turn"].as_i64()
}

/// The agents among `bots`, as the daemon lists them (`name`,
/// `running_turn`, `last`, `closed_ms`), whose mark beside `store`
/// still held: open, running no turn, and none of their turns came after
/// the turn it covered. Each mark is left as it was, so a Sonny from
/// before reads it as ever.
pub fn still_marked(store: &Path, bots: &[Value]) -> Vec<String> {
    let marked = marked(store);
    if marked.is_empty() {
        return Vec::new();
    }
    (bots.iter())
        .filter(|bot| !closed(bot) && bot["running_turn"].is_null())
        .filter_map(|bot| {
            let name = bot["name"].as_str()?;
            let mark = marked.contains(name).then(|| read(store, name)).flatten()?;
            let newest = bot["last"]["turn"].as_i64();
            newest
                .is_none_or(|turn| turn <= mark)
                .then(|| name.to_owned())
        })
        .collect()
}

/// A pattern that names `name` alone: its last character as a set of one,
/// as names hold no character a pattern reads otherwise.
fn alone(name: &str) -> String {
    match name.char_indices().last() {
        Some((at, last)) => format!("{}[{last}]", &name[..at]),
        None => String::new(),
    }
}

/// Close each agent [`still_marked`] finds among `bots`, listed at
/// `listed_ms`, as the person closes it: only if it runs no turn and ran
/// none since it was listed, which the daemon checks as it closes, so work
/// that came meanwhile goes on. Whether every close was answered: one that
/// was not is tried again at the next window's start.
pub async fn marks_close(
    client: &agent_client::Client,
    store: &Path,
    bots: &[Value],
    listed_ms: i64,
) -> bool {
    let mut whole = true;
    for name in still_marked(store, bots) {
        let asked = json!({"bot": alone(&name), "no_turn_since_ms": listed_ms, "limit": 1});
        if client.request("close", asked).await.is_err() {
            whole = false;
        }
    }
    whole
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_close_names_its_own_agent_only_alone_and_whole() {
        let by = |arguments: Value| itself(&arguments, "calc.fix");
        assert!(by(json!({"name": "calc.fix"})));
        assert!(by(json!({"names": ["calc.fix"]})));
        assert!(!by(json!({"name": "fix"})));
        assert!(!by(json!({"names": ["calc.fix", "calc.docs"]})));
        assert!(!by(json!({"name": "calc.*"})));
        assert!(!by(json!({})));
        // What a call closed is what its output says, not what it named.
        let output = json!({"closed": [{"bot": "calc.fix", "was_open": true}],
            "refused": [{"bot": "calc.fix.help", "error": "not_permitted"}]});
        assert_eq!(closed_by(&output.to_string()), ["calc.fix"]);
        assert!(closed_by("not json").is_empty());
    }

    #[test]
    fn a_closed_lead_s_automations_pause_until_a_turn_reopens_it() {
        assert!(
            paused("web.lead", true)
                .unwrap()
                .contains("web.lead is done")
        );
        assert_eq!(paused("web.lead", false), None);
        // A closed task is messaged by its automations as ever.
        assert_eq!(paused("web.fix", true), None);
    }

    #[test]
    fn done_is_closed_once_its_turn_ends() {
        assert!(done(true, false));
        assert!(!done(true, true));
        assert!(!done(false, false));
        assert!(closed(&json!({"closed_ms": 1})));
        assert!(!closed(&json!({"closed_ms": null})));
        assert!(!closed(&json!({})));
    }

    #[test]
    fn an_agent_turn_that_closed_itself_is_followed_in_events() {
        let mut own = Own::default();
        let ev = |bot: &str, turn: i64, kind: &str, data: Value| json!({"bot": bot, "turn": turn, "event": kind, "data": data});
        let started = |call: &str, arguments: &str| json!({"name": TOOL, "call_id": call, "arguments": arguments});
        for (bot, turn, call, arguments, failed) in [
            ("calc.fix", 5, "c1", "{\"name\":\"calc.fix\"}", false),
            ("calc.docs", 6, "c1", "{\"name\":\"calc.fix\"}", false),
            ("calc.bench", 7, "c1", "{\"name\":\"calc.bench\"}", true),
            ("calc.lead", 8, "c1", "{\"name\":\"calc.lead\"}", false),
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
        // Another agent named, a failed call, the lead itself: none.
        assert!(!own.ended("calc.docs", 6));
        assert!(!own.ended("calc.bench", 7));
        assert!(!own.ended("calc.lead", 8));
    }

    #[test]
    fn a_mark_that_still_held_is_one_to_close() {
        let dir = std::env::temp_dir().join(format!("sonny-marks-close-{}", std::process::id()));
        let store = dir.join("state.sqlite");
        let folder = dir.join("state.sqlite.resolved");
        std::fs::create_dir_all(&folder).unwrap();
        for (task, turn) in [
            ("calc.fix", 9),
            ("calc.docs", 4),
            ("calc.busy", 9),
            ("calc.shut", 9),
            ("calc.new", 0),
        ] {
            std::fs::write(
                folder.join(format!("{task}.json")),
                json!({"turn": turn, "call": "c"}).to_string(),
            )
            .unwrap();
        }
        let bots = [
            json!({"name": "calc.fix", "bot_id": 1, "last": {"turn": 9}}),
            // A turn of its came after the mark: it reopened.
            json!({"name": "calc.docs", "bot_id": 2, "last": {"turn": 7}}),
            json!({"name": "calc.busy", "bot_id": 3, "running_turn": 12, "last": {"turn": 9}}),
            json!({"name": "calc.shut", "bot_id": 4, "closed_ms": 5, "last": {"turn": 9}}),
            json!({"name": "calc.new", "bot_id": 5}),
            json!({"name": "calc.open", "bot_id": 6, "last": {"turn": 3}}),
        ];
        assert_eq!(still_marked(&store, &bots), ["calc.fix", "calc.new"]);
        // Closed by a pattern that names it alone.
        assert_eq!(alone("calc.fix"), "calc.fi[x]");
        // The marks stay where they were.
        assert!(folder.join("calc.fix.json").exists());
        assert!(still_marked(&dir.join("other.sqlite"), &bots).is_empty());
        let _ = std::fs::remove_dir_all(dir);
    }
}
