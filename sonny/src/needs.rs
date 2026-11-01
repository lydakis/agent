//! What waits on the person, said one way for every surface that counts
//! it: the footer, a task's tab, `/status`, the started list and the sky.
//! An agent needs the person while a call of its waits on their approval
//! (gated `manual`, or handed to them by a reviewer, [`person_call`]) or
//! a question it put to them with the ask tool is open, as
//! [`crate::asks::open`] reads its history.
//!
//! The daemon keeps both. A surface reads them whole when it starts or
//! comes back ([`listing`], [`scan`]) into a [`Needs`], which its events
//! keep current after ([`Needs::event`]). An event says for sure when a
//! call waits and when its wait ends, and when a question is asked; that
//! the person answered one, or wrote past it, it only suggests, so the
//! agent is read again once that turn ends ([`Needs::due`]). A restart,
//! a reconnect or a lost event is mended at the next read, never carried.
use std::collections::{BTreeSet, HashMap, HashSet};

use agent_client::Client;
use serde_json::{Value, json};

/// Whether a call the daemon lists as waiting on a gate waits on the
/// person: one gated `manual`, or one a reviewer handed over (`asked`).
/// A call a reviewer still weighs waits on a program.
pub fn person_call(call: &Value) -> bool {
    !call["asked"].is_null()
        || (call["gates"].as_array()).is_some_and(|gates| gates.iter().any(|g| g == "manual"))
}

/// A call waiting on the person: its bot, turn and id.
pub type Held = (String, i64, String);

/// Every call in the daemon, or only `bot`'s, that waits on the person
/// now, every page of them.
pub async fn listing(client: &Client, bot: Option<&str>) -> agent_client::Result<Vec<Held>> {
    let mut held = Vec::new();
    let mut after = Value::Null;
    loop {
        let mut asked = json!({"limit": 256});
        if let Some(bot) = bot {
            asked["bot"] = json!(bot);
        }
        if !after.is_null() {
            asked["after"] = after;
        }
        let page = client.request("approvals", asked).await?;
        for call in (page["approvals"].as_array().into_iter().flatten()).filter(|c| person_call(c))
        {
            if let (Some(bot), Some(id)) = (call["bot"].as_str(), call["call_id"].as_str()) {
                held.push((
                    bot.to_owned(),
                    call["turn"].as_i64().unwrap_or(-1),
                    id.to_owned(),
                ));
            }
        }
        after = page["next_after"].clone();
        if after.is_null() {
            return Ok(held);
        }
    }
}

/// What an agent's history says: the question it has open, whether its
/// last turn resolved it, and that turn, the newest its history holds.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Seen {
    pub ask: Option<String>,
    pub itself: bool,
    pub turn: Option<i64>,
}

/// How many of an agent's newest nodes are read for its open question.
const SCANNED: usize = 400;

/// How many nodes one `history_items` read takes.
const READ: usize = 100;

/// Read `bot`'s newest history for the question it has open, and whether
/// its turn `last`, or with none its newest, resolved it; none when the history could not be listed,
/// what a later read gave up on read as far as it came. Newest first, and
/// no further than the person's last words of their own: past those every
/// question before is set aside.
pub async fn scan(client: &Client, bot: &str, last: Option<i64>) -> Option<Seen> {
    let page = client
        .request("history_nodes", json!({"bot": bot, "limit": SCANNED}))
        .await
        .ok()?;
    let listed = page["nodes"].as_array().cloned().unwrap_or_default();
    let nodes: Vec<i64> = listed.iter().filter_map(|n| n["node"].as_i64()).collect();
    let newest = listed.iter().filter_map(|n| n["turn"].as_i64()).max();
    let last = last.or(newest);
    let in_last: HashSet<i64> = (listed.iter())
        .filter(|n| last.is_some() && n["turn"].as_i64() == last)
        .filter_map(|n| n["node"].as_i64())
        .collect();
    let (mut items, mut turn) = (Vec::new(), Vec::new());
    for chunk in nodes.chunks(READ) {
        let (read, short) = self::items(client, bot, chunk).await;
        // Newest first, as the nodes were asked for; each node's items
        // newest first too.
        let mut read = read.into_iter().peekable();
        let mut fresh = Vec::new();
        while let Some((node, item)) = read.next() {
            let mut kept = vec![item];
            while let Some((_, item)) = read.next_if(|(next, _)| *next == node) {
                kept.push(item);
            }
            if in_last.contains(&node) {
                turn.extend(kept.iter().cloned());
            }
            fresh.extend(kept.into_iter().rev());
        }
        let past = fresh.iter().any(moves_on);
        items.extend(fresh);
        if short {
            break;
        }
        // The person wrote past every question older than this, and the
        // last turn, newer, is read.
        if past
            && in_last
                .iter()
                .all(|node| node > chunk.last().unwrap_or(&i64::MAX))
        {
            break;
        }
    }
    Some(Seen {
        ask: crate::asks::open(&items),
        itself: resolved_itself(&turn, bot),
        turn: last,
    })
}

/// Whether `item` is the person's own words, past which their questions
/// before are set aside.
fn moves_on(item: &Value) -> bool {
    item["role"] == "user" && crate::asks::moves_on(&crate::text(item), item["from"].as_str())
}

/// `bot`'s items kept at `nodes`, each with its node, as the transcript
/// reads them: who sent a prompt marked on it. Whether the read came back
/// short too, the daemon gone or the nodes pruned.
pub async fn items(client: &Client, bot: &str, nodes: &[i64]) -> (Vec<(i64, Value)>, bool) {
    let mut items = Vec::new();
    let mut rest = nodes;
    while !rest.is_empty() {
        // The daemon reads at most `HISTORY_NODES` nodes a call.
        let ask = &rest[..rest.len().min(crate::HISTORY_NODES)];
        let Ok(read) = client
            .request("history_items", json!({"bot": bot, "nodes": ask}))
            .await
        else {
            return (items, true);
        };
        let entries = read["items"].as_array().cloned().unwrap_or_default();
        if entries.is_empty() {
            return (items, true);
        }
        rest = &rest[entries.len().min(rest.len())..];
        items.extend(entries.iter().flat_map(|entry| {
            let node = entry["node"].as_i64().unwrap_or(0);
            // Who sent a prompt, when another bot did, a client from
            // outside (the commons, an automation) or another person.
            let from = crate::transcript::sender(
                entry["from"]["bot"].as_str(),
                entry["origin"].as_str(),
                entry["person"].as_str(),
            );
            let quotes = entry.get("quotes").cloned();
            crate::responses(&entry["item"])
                .into_iter()
                .map(move |mut item| {
                    if let Some(from) = from.as_ref().filter(|_| item["role"] == "user") {
                        item["from"] = json!(from);
                        // A person, not an agent or a client, wrote it.
                        if entry["person"].is_string() && entry["from"].is_null() {
                            item["person"] = json!(true);
                        }
                    }
                    // It rode along on a call rather than coming as a message.
                    if entry["attached"] == true && item["role"] == "user" {
                        item["attached"] = json!(true);
                    }
                    // Where its quoted words are among its parts, and who
                    // wrote them.
                    if let Some(quotes) = quotes.as_ref().filter(|_| item["role"] == "user") {
                        item["quotes"] = quotes.clone();
                    }
                    (node, item)
                })
        }));
    }
    (items, false)
}

/// Whether `items`, one turn of `bot`'s, hold a `resolve_task` call of its
/// own that resolved itself and was not refused.
pub fn resolved_itself(items: &[Value], bot: &str) -> bool {
    items.iter().any(|call| {
        call["type"] == "function_call"
            && call["name"] == crate::resolved::TOOL
            && crate::resolved::itself(
                &call["arguments"]
                    .as_str()
                    .and_then(|a| serde_json::from_str(a).ok())
                    .unwrap_or(Value::Null),
                bot,
            )
            && items.iter().any(|done| {
                done["type"] == "function_call_output"
                    && done["call_id"] == call["call_id"]
                    && !crate::output(done).1
                    && !crate::reactions::refused(done)
            })
    })
}

/// What waits on the person, by agent: the calls waiting on their approval
/// and the question each has open.
#[derive(Clone, Debug, Default)]
pub struct Needs {
    calls: HashMap<String, BTreeSet<(i64, String)>>,
    /// Each agent with a question open, and the call that asked it when
    /// an event said.
    asks: HashMap<String, Option<String>>,
    /// Agents whose questions an event left unsure, to read again once
    /// their turn ends.
    unsure: HashSet<String>,
    /// Those whose turn ended since, with the turn.
    due: Vec<(String, Option<i64>)>,
}

impl Needs {
    /// Whether `bot` waits on the person.
    pub fn waits(&self, bot: &str) -> bool {
        self.holds(bot) || self.asks.contains_key(bot)
    }

    /// Whether a call of `bot`'s waits on the person's approval.
    pub fn holds(&self, bot: &str) -> bool {
        self.calls.get(bot).is_some_and(|calls| !calls.is_empty())
    }

    /// The calls waiting on the person as the daemon lists them now, in
    /// place of every one kept for the agents `listed` covers.
    pub fn held(&mut self, held: Vec<Held>, listed: impl Fn(&str) -> bool) {
        self.calls.retain(|bot, _| !listed(bot));
        for (bot, turn, call) in held.into_iter().filter(|(bot, ..)| listed(bot)) {
            self.calls.entry(bot).or_default().insert((turn, call));
        }
        self.calls.retain(|_, calls| !calls.is_empty());
    }

    /// `bot`'s open question, as its history reads now.
    pub fn asked(&mut self, bot: &str, question: Option<String>) {
        match question {
            Some(_) => {
                self.asks.entry(bot.to_owned()).or_default();
            }
            None => {
                self.asks.remove(bot);
            }
        }
        self.unsure.remove(bot);
    }

    /// What `other` read, newer, in place of what this kept: the calls of
    /// the agents `calls` names and the questions of those `asks` does.
    pub fn adopt(&mut self, other: &Needs, calls: &[String], asks: &[String]) {
        for bot in calls {
            match other.calls.get(bot) {
                Some(calls) => self.calls.insert(bot.clone(), calls.clone()),
                None => self.calls.remove(bot),
            };
        }
        for bot in asks {
            match other.asks.get(bot) {
                Some(asked) => self.asks.insert(bot.clone(), asked.clone()),
                None => self.asks.remove(bot),
            };
        }
    }

    /// `bot` is gone, or resolved: nothing of its waits now.
    pub fn forget(&mut self, bot: &str) {
        self.calls.remove(bot);
        self.asks.remove(bot);
        self.unsure.remove(bot);
    }

    /// Whether an event left `bot`'s questions unsure, to be read again
    /// once its turn ends.
    pub fn unsure(&self, bot: &str) -> bool {
        self.unsure.contains(bot)
    }

    /// The agents to read again, their turn over, with that turn.
    pub fn due(&mut self) -> Vec<(String, Option<i64>)> {
        std::mem::take(&mut self.due)
    }

    /// One of the daemon's events, `{bot, turn, event, data}`: whether it
    /// changed what waits on the person.
    pub fn event(&mut self, event: &Value) -> bool {
        let Some(bot) = event["bot"].as_str() else {
            return false;
        };
        let was = self.waits(bot);
        let data = &event["data"];
        let turn = event["turn"].as_i64().unwrap_or(-1);
        match event["event"].as_str().unwrap_or("") {
            "approval_requested" => {
                for call in data["calls"].as_array().into_iter().flatten() {
                    if person_call(call)
                        && let Some(id) = call["call_id"].as_str()
                    {
                        let calls = self.calls.entry(bot.to_owned()).or_default();
                        calls.insert((turn, id.to_owned()));
                    }
                }
            }
            // A reviewer handed the call to the person.
            "approval_asked" => {
                if let Some(id) = data["call_id"].as_str() {
                    let calls = self.calls.entry(bot.to_owned()).or_default();
                    calls.insert((turn, id.to_owned()));
                }
            }
            // A call allowed starts; a question is asked as it starts.
            "tool_started" if data["name"] != crate::asks::TOOL => {
                if let (Some(calls), Some(id)) = (self.calls.get_mut(bot), data["call_id"].as_str())
                {
                    calls.retain(|(_, call)| call != id);
                }
            }
            "tool_started" => {
                let call = data["call_id"].as_str().map(str::to_owned);
                self.asks.insert(bot.to_owned(), call);
                self.unsure.insert(bot.to_owned());
            }
            // Each answer lets go of its call alone; a question refused
            // was never asked.
            "step_decided" | "turn_resumed" | "tool_completed" => {
                if let Some(id) = data["call_id"].as_str() {
                    if let Some(calls) = self.calls.get_mut(bot) {
                        calls.retain(|(_, call)| call != id);
                    }
                    let refused = data["failed"] == true || data["denied"] == true;
                    if refused && (self.asks.get(bot)).is_some_and(|a| a.as_deref() == Some(id)) {
                        self.asks.remove(bot);
                    }
                }
            }
            // The person's own words answer a question or go past it; which
            // one, and whether they were words of their own (a reaction is
            // not), the history says once the turn ends.
            "accepted" | "steered" if from_person(data) => {
                if self.asks.remove(bot).is_some() {
                    self.unsure.insert(bot.to_owned());
                }
            }
            // A steer the running turn took ends as a turn of its own: only
            // the running turn's end lets go of its calls.
            "turn_finished" if !matches!(data["status"].as_str(), Some("steered" | "attached")) => {
                if let Some(calls) = self.calls.get_mut(bot) {
                    calls.retain(|(of, _)| *of != turn);
                }
                if self.unsure.remove(bot) {
                    self.due.push((bot.to_owned(), event["turn"].as_i64()));
                }
            }
            "deleted" => self.forget(bot),
            _ => {}
        }
        if self.calls.get(bot).is_some_and(BTreeSet::is_empty) {
            self.calls.remove(bot);
        }
        was != self.waits(bot)
    }
}

/// Whether a turn's prompt, as `accepted` or `steered` tells it, is the
/// person's: no agent sent it, nor a client from outside.
fn from_person(data: &Value) -> bool {
    data["from"]["bot"].is_null() && data["origin"].as_str().is_none_or(str::is_empty)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn event(bot: &str, turn: i64, kind: &str, data: Value) -> Value {
        json!({"bot": bot, "turn": turn, "event": kind, "data": data})
    }

    fn ask(call: &str, question: &str) -> Value {
        let arguments = json!({"question": question, "why": "w", "options": [{"label": "a"}]});
        json!({"name": "ask", "call_id": call, "arguments": arguments.to_string()})
    }

    #[test]
    fn a_call_waits_on_the_person_only_when_gated_for_them_or_handed_over() {
        assert!(person_call(&json!({"gates": ["manual"], "asked": null})));
        assert!(person_call(
            &json!({"gates": ["auto"], "asked": {"tag": "auto"}})
        ));
        assert!(!person_call(&json!({"gates": ["auto"], "asked": null})));
        let mut needs = Needs::default();
        let gated = |call: &str, gate: &str| json!({"call_id": call, "gates": [gate]});
        needs.event(&event(
            "p.fix",
            3,
            "approval_requested",
            json!({"calls": [gated("c1", "auto"), gated("c2", "manual")]}),
        ));
        assert!(needs.waits("p.fix"));
        needs.event(&event("p.fix", 3, "step_decided", json!({"call_id": "c2"})));
        assert!(
            !needs.waits("p.fix"),
            "the reviewer's own call waits on no one"
        );
        needs.event(&event(
            "p.fix",
            3,
            "approval_asked",
            json!({"call_id": "c1", "tag": "auto"}),
        ));
        assert!(needs.waits("p.fix"));
        needs.event(&event(
            "p.fix",
            3,
            "tool_started",
            json!({"name": "shell", "call_id": "c1"}),
        ));
        assert!(!needs.waits("p.fix"), "a call allowed starts");
    }

    #[test]
    fn a_parked_turn_resuming_after_a_restart_still_waits_on_its_call() {
        let mut needs = Needs::default();
        needs.held(vec![("p.fix".into(), 4, "c1".into())], |_| true);
        assert!(needs.waits("p.fix"));
        // The turn picks up again with no call's verdict.
        needs.event(&event("p.fix", 4, "turn_resumed", json!({})));
        assert!(needs.waits("p.fix"));
        // A steer's own turn ends; the running one goes on waiting.
        needs.event(&event(
            "p.fix",
            5,
            "turn_finished",
            json!({"status": "steered"}),
        ));
        assert!(needs.waits("p.fix"));
        needs.event(&event(
            "p.fix",
            4,
            "turn_finished",
            json!({"status": "failed"}),
        ));
        assert!(!needs.waits("p.fix"));
    }

    #[test]
    fn a_listing_replaces_only_what_it_covers() {
        let mut needs = Needs::default();
        needs.held(
            vec![("a.x".into(), 1, "c".into()), ("b.y".into(), 1, "c".into())],
            |_| true,
        );
        needs.held(Vec::new(), |bot| bot == "a.x");
        assert!(!needs.waits("a.x"));
        assert!(needs.waits("b.y"));
    }

    #[test]
    fn a_question_waits_through_its_turns_end_and_is_read_again_then() {
        let mut needs = Needs::default();
        needs.event(&event("p.fix", 2, "tool_started", ask("q1", "Ship it?")));
        assert!(needs.waits("p.fix"));
        assert_eq!(needs.asks["p.fix"].as_deref(), Some("q1"));
        needs.event(&event(
            "p.fix",
            2,
            "tool_completed",
            json!({"call_id": "q1"}),
        ));
        assert!(needs.due().is_empty(), "read again once the turn ends");
        needs.event(&event(
            "p.fix",
            2,
            "turn_finished",
            json!({"status": "completed"}),
        ));
        assert!(
            needs.waits("p.fix"),
            "a task that ended on a question needs you"
        );
        assert_eq!(needs.due(), vec![("p.fix".to_owned(), Some(2))]);
        needs.asked("p.fix", Some("Ship it?".into()));
        assert!(needs.waits("p.fix"));
    }

    #[test]
    fn a_refused_question_was_never_asked() {
        let mut needs = Needs::default();
        needs.event(&event("p.fix", 2, "tool_started", ask("q1", "Ship it?")));
        needs.event(&event(
            "p.fix",
            2,
            "tool_completed",
            json!({"call_id": "q1", "denied": true}),
        ));
        assert!(!needs.waits("p.fix"));
    }

    #[test]
    fn the_persons_words_answer_and_agents_words_do_not() {
        let mut needs = Needs::default();
        needs.asked("p.fix", Some("Ship it?".into()));
        needs.event(&event(
            "p.fix",
            3,
            "accepted",
            json!({"from": {"bot": "p.lead"}}),
        ));
        needs.event(&event("p.fix", 3, "accepted", json!({"origin": "trigger"})));
        assert!(needs.waits("p.fix"), "only the person answers it");
        assert!(needs.event(&event("p.fix", 4, "accepted", json!({}))));
        assert!(!needs.waits("p.fix"));
        needs.event(&event(
            "p.fix",
            4,
            "turn_finished",
            json!({"status": "completed"}),
        ));
        assert_eq!(needs.due(), vec![("p.fix".to_owned(), Some(4))]);
        // A reaction is not words of their own: the read puts it back.
        needs.asked("p.fix", Some("Ship it?".into()));
        assert!(needs.waits("p.fix"));
    }

    #[test]
    fn what_another_read_replaces_only_the_agents_it_read() {
        let mut kept = Needs::default();
        kept.asked("p.a", Some("A?".into()));
        kept.asked("p.b", Some("B?".into()));
        let mut read = Needs::default();
        read.held(vec![("p.c".into(), 1, "c".into())], |_| true);
        let bots = ["p.a".to_owned(), "p.c".to_owned()];
        kept.adopt(&read, &bots, &bots);
        assert!(!kept.waits("p.a"));
        assert!(kept.waits("p.b"));
        assert!(kept.waits("p.c"));
        kept.forget("p.b");
        assert!(!kept.waits("p.b"));
    }
}
