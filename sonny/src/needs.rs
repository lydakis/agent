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
//! the person's words answered one it only suggests, so the agent is read
//! again then ([`Needs::due`]): words that answer none leave every
//! question open, as only a pick or the agent's close settles one. A restart,
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

/// A call waiting on the person: its bot, turn and id, the call it is a
/// step of, and whether it waits past its turn, as a call a message lifted
/// (and its steps) does.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct Held {
    pub bot: String,
    pub turn: i64,
    pub call: String,
    pub step_of: Option<String>,
    pub lifted: bool,
}

impl Held {
    pub fn new(bot: &str, turn: i64, call: &str) -> Self {
        Held {
            bot: bot.to_owned(),
            turn,
            call: call.to_owned(),
            step_of: None,
            lifted: false,
        }
    }

    /// `bot`'s call as the daemon lists it.
    pub fn listed(bot: &str, call: &Value) -> Option<Self> {
        Some(Held {
            step_of: call["step_of"].as_str().map(str::to_owned),
            lifted: call["handle"].is_string(),
            ..Held::new(bot, call["turn"].as_i64()?, call["call_id"].as_str()?)
        })
    }
}

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
            if let Some(listed) = call["bot"].as_str().and_then(|bot| Held::listed(bot, call)) {
                held.push(listed);
            }
        }
        after = page["next_after"].clone();
        if after.is_null() {
            return Ok(held);
        }
    }
}

/// What an agent's history says: the question it has open, whole,
/// whether its last turn resolved it, and that turn, the newest its
/// history holds.
#[derive(Clone, Debug, Default, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct Seen {
    pub ask: Option<crate::asks::Ask>,
    pub itself: bool,
    pub turn: Option<i64>,
    /// The newest node read, from which a later read goes on
    /// ([`scan_since`]).
    pub through: Option<i64>,
}

impl Seen {
    /// The question still open, as every surface reads it: none for an
    /// agent done, closed by its own call in the turn read, or `closed`
    /// as the daemon lists it while nothing `running`. A lead closed is
    /// done as a task is.
    pub fn open(self, closed: bool, running: bool) -> Option<crate::asks::Ask> {
        if self.itself || crate::resolved::done(closed, running) {
            return None;
        }
        self.ask
    }
}

/// How this build reads a history for its question: a hash of the files
/// that read one, so a build that changed none of them takes what the one
/// before read and kept ([`crate::seen_stamp`]), and one that changed any
/// reads every agent again. A change elsewhere to what a read finds bumps
/// [`READ_AS`].
pub const READS: u64 = {
    let mut hash = fnv(0xcbf2_9ce4_8422_2325, &READ_AS.to_le_bytes());
    hash = fnv(hash, include_bytes!("needs.rs"));
    hash = fnv(hash, include_bytes!("asks.rs"));
    fnv(hash, include_bytes!("resolved.rs"))
};

/// Bumped when what a question read finds changes outside the files
/// [`READS`] hashes.
const READ_AS: u32 = 2;

/// FNV-1a over `bytes`, going on from `hash`.
const fn fnv(mut hash: u64, bytes: &[u8]) -> u64 {
    let mut at = 0;
    while at < bytes.len() {
        hash ^= bytes[at] as u64;
        hash = hash.wrapping_mul(0x0100_0000_01b3);
        at += 1;
    }
    hash
}

/// How many of an agent's newest nodes are read for its open question.
const SCANNED: usize = 400;

/// How many nodes one `history_items` read takes.
const READ: usize = 100;

/// Read `bot`'s newest history for the question it has open, and whether
/// its turn `last`, or with none its newest, resolved it: newest first, as
/// far back as [`SCANNED`] nodes. None unless every read it needed came
/// back whole: a history that could not be listed, or items a read gave up
/// on, may hold the open question or the mark, so what was read is no
/// answer, and nobody keeps it as one.
pub async fn scan(client: &Client, bot: &str, last: Option<i64>) -> Option<Seen> {
    let page = client
        .request("history_nodes", json!({"bot": bot, "limit": SCANNED}))
        .await
        .ok()?;
    let listed = page["nodes"].as_array().cloned().unwrap_or_default();
    let nodes: Vec<i64> = listed.iter().filter_map(|n| n["node"].as_i64()).collect();
    // Each node's turn, which names an ask with its call.
    let turns: HashMap<i64, i64> = (listed.iter())
        .filter_map(|n| Some((n["node"].as_i64()?, n["turn"].as_i64()?)))
        .collect();
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
            if let Some(&at) = turns.get(&node) {
                for item in &mut kept {
                    item["turn"] = json!(at);
                }
            }
            if in_last.contains(&node) {
                turn.extend(kept.iter().cloned());
            }
            fresh.extend(kept.into_iter().rev());
        }
        if short {
            return None;
        }
        items.extend(fresh);
    }
    Some(Seen {
        ask: crate::asks::open(&items),
        itself: resolved_itself(&turn, bot),
        turn: last,
        through: nodes.iter().copied().max(),
    })
}

/// [`scan`], going on from what was read before (`was`): only the nodes
/// written since are read, and while none of them asks, closes or answers
/// a question, the question is the one read before and the newest turn
/// says whether it resolved it. Anything
/// else, or more nodes than one scan reads, is read whole again.
pub async fn scan_since(client: &Client, bot: &str, last: Option<i64>, was: &Seen) -> Option<Seen> {
    let Some(through) = was.through else {
        return scan(client, bot, last).await;
    };
    let page = client
        .request(
            "history_nodes",
            json!({"bot": bot, "limit": SCANNED, "min_node": through + 1}),
        )
        .await
        .ok()?;
    if !page["next_from"].is_null() {
        return scan(client, bot, last).await;
    }
    let listed = page["nodes"].as_array().cloned().unwrap_or_default();
    let nodes: Vec<i64> = listed.iter().filter_map(|n| n["node"].as_i64()).collect();
    let newest = listed.iter().filter_map(|n| n["turn"].as_i64()).max();
    let last = last.or(newest).or(was.turn);
    // The turn that says whether it resolved its question: read before, or
    // whole among the nodes since.
    let in_last: HashSet<i64> = (listed.iter())
        .filter(|n| last.is_some() && n["turn"].as_i64() == last)
        .filter_map(|n| n["node"].as_i64())
        .collect();
    if in_last.is_empty() && last != was.turn {
        return scan(client, bot, last).await;
    }
    let (read, short) = items(client, bot, &nodes).await;
    if short {
        return None;
    }
    let mut turn = Vec::new();
    // The call that asked what was open, whose output, should it come
    // late, may say it failed.
    let open = (was.ask.as_ref()).and_then(|ask| ask.id.rsplit_once('/').map(|(_, call)| call));
    for (node, item) in read {
        let of_open = item["type"] == "function_call_output"
            && open.is_some_and(|call| item["call_id"] == call);
        if answers(&item) || asks(&item) || of_open {
            return scan(client, bot, last).await;
        }
        if in_last.contains(&node) {
            turn.push(item);
        }
    }
    Some(Seen {
        ask: was.ask.clone(),
        itself: match in_last.is_empty() {
            true => was.itself,
            false => resolved_itself(&turn, bot),
        },
        turn: last,
        through: nodes.iter().copied().max().or(Some(through)),
    })
}

/// Whether `item` answers a question: the person's pick.
fn answers(item: &Value) -> bool {
    item["role"] == "user"
        && crate::asks::answer_of(&crate::text(item), item["from"].as_str()).is_some()
}

/// Whether `item` asks a question or closes one.
fn asks(item: &Value) -> bool {
    item["type"] == "function_call"
        && (item["name"] == crate::asks::TOOL || item["name"] == crate::asks::CLOSE)
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
                        // An agent sent it: its name opens it.
                        if entry["from"]["bot"].is_string() {
                            item["agent"] = json!(true);
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
                    // When a message joined the history, which the window
                    // shows on hover, and when a call's output did, which
                    // dates the margin note it put up.
                    if (item["role"].is_string() || item["type"] == "function_call_output")
                        && entry["written_ms"].is_i64()
                    {
                        item["written_ms"] = entry["written_ms"].clone();
                    }
                    // The message it holds, by its handle, which the window
                    // shows beside that time.
                    if item["role"] == "user" && entry["handle"].is_string() {
                        item["handle"] = entry["handle"].clone();
                    }
                    (node, item)
                })
        }));
    }
    (items, false)
}

/// Whether `items`, one turn of `bot`'s, hold a `close_agent` call that
/// closed `bot` itself and was not refused.
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
    /// The calls a message lifted out of their turn, by agent: they, and
    /// the steps their clients ask for, wait on past its end, until their
    /// verdict.
    lifted: HashMap<String, HashSet<(i64, String)>>,
    /// Of `calls`, the steps, by agent, each with the call it is a step
    /// of: an event for one agent touches no other's.
    steps: HashMap<String, HashMap<(i64, String), String>>,
    /// Each agent with a question open.
    asks: HashMap<String, Open>,
    /// Agents whose questions an event left unsure, to read again once
    /// their turn ends.
    unsure: HashSet<String>,
    /// Those whose turn ended since, with the turn.
    due: Vec<(String, Option<i64>)>,
    /// For each agent, how many events changed its question: a read begun
    /// before one of them is older than what the events said.
    stirred: HashMap<String, u64>,
    /// Counts every change to the questions open, so a window showing
    /// them reads them again only when they changed.
    version: u64,
}

/// A question open: the call that asked it, when an event said, and the
/// question whole, once read, as a window that answers it shows it.
#[derive(Clone, Debug, Default)]
struct Open {
    call: Option<String>,
    ask: Option<crate::asks::Ask>,
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
        self.steps.retain(|bot, _| !listed(bot));
        for held in held.into_iter().filter(|held| listed(&held.bot)) {
            let key = (held.turn, held.call.clone());
            if held.lifted {
                let call = held.step_of.clone().unwrap_or_else(|| held.call.clone());
                (self.lifted.entry(held.bot.clone()).or_default()).insert((held.turn, call));
            }
            if let Some(of) = held.step_of {
                visit(1);
                (self.steps.entry(held.bot.clone()).or_default()).insert(key, of);
            }
            (self.calls.entry(held.bot).or_default()).insert((held.turn, held.call));
        }
        self.calls.retain(|_, calls| !calls.is_empty());
    }

    /// Which of `bot`'s calls wait on the person's approval, by turn and
    /// id: none for most, so a copy to compare costs nothing then.
    pub fn held_by(&self, bot: &str) -> Option<BTreeSet<(i64, String)>> {
        self.calls
            .get(bot)
            .filter(|calls| !calls.is_empty())
            .cloned()
    }

    /// Every call waiting on the person's approval: its agent, turn and id.
    pub fn calls(&self) -> impl Iterator<Item = (&str, i64, &str)> {
        (self.calls.iter()).flat_map(|(bot, calls)| {
            (calls.iter()).map(move |(turn, call)| (bot.as_str(), *turn, call.as_str()))
        })
    }

    /// Every agent that waits on the person, once each.
    pub fn waiting(&self) -> impl Iterator<Item = &str> {
        let asking = (self.asks.keys()).filter(|bot| !self.holds(bot));
        (self.calls.iter())
            .filter(|(_, calls)| !calls.is_empty())
            .map(|(bot, _)| bot)
            .chain(asking)
            .map(String::as_str)
    }

    /// The question each agent has open, whole, where it was read.
    pub fn asks(&self) -> impl Iterator<Item = (&str, &crate::asks::Ask)> {
        (self.asks.iter()).filter_map(|(bot, open)| Some((bot.as_str(), open.ask.as_ref()?)))
    }

    /// What [`Needs::asks`] says changes whenever this does.
    pub fn version(&self) -> u64 {
        self.version
    }

    /// `bot`'s open question, as its history reads now.
    pub fn asked(&mut self, bot: &str, question: Option<crate::asks::Ask>) {
        self.version += 1;
        match question {
            Some(ask) => {
                self.asks.entry(bot.to_owned()).or_default().ask = Some(ask);
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
            match other.lifted.get(bot) {
                Some(lifted) => self.lifted.insert(bot.clone(), lifted.clone()),
                None => self.lifted.remove(bot),
            };
            match other.steps.get(bot) {
                Some(steps) => {
                    visit(steps.len());
                    self.steps.insert(bot.clone(), steps.clone())
                }
                None => self.steps.remove(bot),
            };
        }
        for bot in asks {
            self.version += 1;
            match other.asks.get(bot) {
                Some(asked) => self.asks.insert(bot.clone(), asked.clone()),
                None => self.asks.remove(bot),
            };
        }
    }

    /// `bot` is gone, or resolved: nothing of its waits now.
    pub fn forget(&mut self, bot: &str) {
        self.calls.remove(bot);
        self.lifted.remove(bot);
        self.steps.remove(bot);
        if self.asks.remove(bot).is_some() {
            self.version += 1;
        }
        self.unsure.remove(bot);
    }

    /// Whether an event left `bot`'s questions unsure, to be read again
    /// once its turn ends.
    pub fn unsure(&self, bot: &str) -> bool {
        self.unsure.contains(bot)
    }

    /// `bot`'s turn runs, its start unseen: its history, read once the
    /// turn ends, says what is open.
    pub fn read_at_end(&mut self, bot: &str) {
        self.unsure.insert(bot.to_owned());
    }

    /// `bot`'s turn is over, its end unseen: its history, read next, says
    /// what is open.
    pub fn sure(&mut self, bot: &str) {
        self.unsure.remove(bot);
    }

    /// How many events changed `bot`'s question: the same before and after
    /// a read of it, none came between.
    pub fn stirred(&self, bot: &str) -> u64 {
        self.stirred.get(bot).copied().unwrap_or(0)
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
        let kind = event["event"].as_str().unwrap_or("");
        match kind {
            "approval_requested" => {
                for call in data["calls"].as_array().into_iter().flatten() {
                    if person_call(call)
                        && let Some(id) = call["call_id"].as_str()
                    {
                        self.wait(bot, turn, id, call["step_of"].as_str());
                    }
                }
            }
            // A reviewer handed the call to the person, a step with the
            // call it is a step of.
            "approval_asked" => {
                if let Some(id) = data["call_id"].as_str() {
                    self.wait(bot, turn, id, data["step_of"].as_str());
                }
            }
            // A call allowed starts; a question is asked as it starts.
            "tool_started" if data["name"] != crate::asks::TOOL => {
                if let Some(id) = data["call_id"].as_str() {
                    self.let_go(bot, turn, id);
                }
                // An agent closing a question of its own: which one, the
                // history says once the turn ends.
                if data["name"] == crate::asks::CLOSE && self.asks.contains_key(bot) {
                    self.unsure.insert(bot.to_owned());
                    self.stir(bot);
                }
            }
            "tool_started" => {
                let call = data["call_id"].as_str().map(str::to_owned);
                self.asks.insert(bot.to_owned(), Open { call, ask: None });
                self.version += 1;
                self.unsure.insert(bot.to_owned());
                self.stir(bot);
            }
            // A message lifted the call out of its turn: it waits on, and
            // so do the steps its client asks for, then or later.
            "call_lifted" => {
                if let Some(id) = data["call_id"].as_str() {
                    let lifted = self.lifted.entry(bot.to_owned()).or_default();
                    lifted.insert((turn, id.to_owned()));
                }
            }
            // Allowed, one its client answers waits on for that answer: no
            // longer for the person, but lifted still, as its steps are.
            "lifted_ended" if data["waits_for"] == "answer" => {
                if let Some(id) = data["call_id"].as_str() {
                    self.let_go(bot, turn, id);
                }
            }
            // Its verdict, or its agent, ended a lifted call, and its steps.
            "lifted_ended" => {
                if let Some(id) = data["call_id"].as_str() {
                    let steps = self.steps.get(bot);
                    let ended: Vec<String> = (self.turn_calls(bot, turn))
                        .filter(|call| {
                            *call == id
                                || steps
                                    .and_then(|steps| {
                                        visit(1);
                                        steps.get(&(turn, (*call).to_owned()))
                                    })
                                    .is_some_and(|of| of == id)
                        })
                        .map(str::to_owned)
                        .collect();
                    for call in ended {
                        self.let_go(bot, turn, &call);
                    }
                    if let Some(lifted) = self.lifted.get_mut(bot) {
                        lifted.remove(&(turn, id.to_owned()));
                        if lifted.is_empty() {
                            self.lifted.remove(bot);
                        }
                    }
                }
            }
            // Each answer lets go of its call alone, but a lifted call's
            // turn going on is no answer, nor is a parked turn resuming:
            // a message wakes it too. A question refused was never asked.
            "step_decided" | "tool_completed" => {
                if let Some(id) = data["call_id"].as_str() {
                    let lifted =
                        kind != "step_decided" && lifted(&self.lifted, &self.steps, bot, turn, id);
                    if !lifted {
                        self.let_go(bot, turn, id);
                    }
                    let refused = data["failed"] == true || data["denied"] == true;
                    let asking =
                        (self.asks.get(bot)).is_some_and(|a| a.call.as_deref() == Some(id));
                    if refused && asking {
                        self.asks.remove(bot);
                        self.version += 1;
                        self.stir(bot);
                    }
                    // Taken, the question is in its history whole, with what
                    // Sonny took it as: read now, not when the turn ends,
                    // as a turn may go on meanwhile.
                    if asking && !refused && kind == "tool_completed" {
                        self.due.push((bot.to_owned(), event["turn"].as_i64()));
                    }
                }
            }
            // The person's words may answer its question, or be words of
            // their own that leave it open: their history, which holds them
            // as the turn takes them, says which, read at once.
            "accepted" | "steered" if from_person(data) => {
                if self.asks.contains_key(bot) {
                    self.due.push((bot.to_owned(), event["turn"].as_i64()));
                    self.stir(bot);
                }
            }
            // Only a turn's end lets go of its calls; a message that ended
            // in line ends as a message.
            "turn_finished" => {
                let (kept, steps) = (&self.lifted, &self.steps);
                let ended: Vec<String> = (self.turn_calls(bot, turn))
                    .filter(|call| !lifted(kept, steps, bot, turn, call))
                    .map(str::to_owned)
                    .collect();
                for call in ended {
                    self.let_go(bot, turn, &call);
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

    /// `bot`'s call `id` of `turn` waits on the person, a step of `of`
    /// where it is one.
    fn wait(&mut self, bot: &str, turn: i64, id: &str, of: Option<&str>) {
        let calls = self.calls.entry(bot.to_owned()).or_default();
        calls.insert((turn, id.to_owned()));
        if let Some(of) = of {
            visit(1);
            let steps = self.steps.entry(bot.to_owned()).or_default();
            steps.insert((turn, id.to_owned()), of.to_owned());
        }
    }

    /// `bot`'s call `id` of `turn` waits on the person no more, nor as a
    /// step.
    fn let_go(&mut self, bot: &str, turn: i64, id: &str) {
        let key = (turn, id.to_owned());
        if let Some(calls) = self.calls.get_mut(bot) {
            calls.remove(&key);
        }
        if let Some(steps) = self.steps.get_mut(bot) {
            visit(1);
            steps.remove(&key);
            if steps.is_empty() {
                self.steps.remove(bot);
            }
        }
    }

    /// The calls of `bot`'s `turn` that wait on the person.
    /// Whether `bot`'s turn `turn` ending lets go a step held for the
    /// person whose call no event said was lifted: one lifted before these
    /// events were followed waits on past the turn, as the daemon says.
    pub fn ends_steps_of_unseen(&self, bot: &str, turn: i64) -> bool {
        let Some(steps) = self.steps.get(bot) else {
            return false;
        };
        (self.turn_calls(bot, turn)).any(|call| {
            steps.contains_key(&(turn, call.to_owned()))
                && !lifted(&self.lifted, &self.steps, bot, turn, call)
        })
    }

    fn turn_calls(&self, bot: &str, turn: i64) -> impl Iterator<Item = &str> {
        (self.calls.get(bot).into_iter())
            .flat_map(move |calls| calls.range((turn, String::new())..))
            .take_while(move |(of, _)| *of == turn)
            .map(|(_, call)| call.as_str())
    }

    fn stir(&mut self, bot: &str) {
        *self.stirred.entry(bot.to_owned()).or_default() += 1;
    }
}

// Counts the steps an event looks at, in tests: an event for one agent
// looks at none of another's.
#[cfg(test)]
thread_local!(static VISITED: std::cell::Cell<usize> = const { std::cell::Cell::new(0) });

/// `entries` of the steps looked at.
fn visit(entries: usize) {
    #[cfg(test)]
    VISITED.with(|visited| visited.set(visited.get() + entries));
    #[cfg(not(test))]
    let _ = entries;
}

/// Whether `bot`'s call `id` of `turn` waits past its turn: a message
/// lifted it, or the call it is a step of.
fn lifted(
    lifted: &HashMap<String, HashSet<(i64, String)>>,
    steps: &HashMap<String, HashMap<(i64, String), String>>,
    bot: &str,
    turn: i64,
    id: &str,
) -> bool {
    let Some(lifted) = lifted.get(bot) else {
        return false;
    };
    let key = (turn, id.to_owned());
    lifted.contains(&key)
        || (steps.get(bot))
            .and_then(|steps| {
                visit(1);
                steps.get(&key)
            })
            .is_some_and(|of| lifted.contains(&(turn, of.clone())))
}

/// Whether a turn's prompt, as `accepted` or `steered` tells it, is the
/// person's: no agent sent it, nor a client from outside, nor another
/// person.
pub fn from_person(data: &Value) -> bool {
    data["from"]["bot"].is_null()
        && data["origin"].as_str().is_none_or(str::is_empty)
        && data["person"].is_null()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn event(bot: &str, turn: i64, kind: &str, data: Value) -> Value {
        json!({"bot": bot, "turn": turn, "event": kind, "data": data})
    }

    fn whole(question: &str) -> crate::asks::Ask {
        let arguments = json!({"question": question, "options": [{"label": "a"}]});
        crate::asks::call(&crate::asks::id(1, "c"), &arguments).unwrap()
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
        needs.held(vec![Held::new("p.fix", 4, "c1")], |_| true);
        assert!(needs.waits("p.fix"));
        // The turn picks up again with no call's verdict.
        needs.event(&event("p.fix", 4, "turn_resumed", json!({})));
        assert!(needs.waits("p.fix"));
        // A message ends in line; the running turn goes on waiting.
        needs.event(&event(
            "p.fix",
            5,
            "message_ended",
            json!({"status": "withdrawn"}),
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
            vec![Held::new("a.x", 1, "c"), Held::new("b.y", 1, "c")],
            |_| true,
        );
        needs.held(Vec::new(), |bot| bot == "a.x");
        assert!(!needs.waits("a.x"));
        assert!(needs.waits("b.y"));
    }

    #[test]
    fn a_lifted_call_waits_past_its_turn_until_its_verdict() {
        let mut needs = Needs::default();
        let manual = json!({"calls": [{"call_id": "c1", "gates": ["manual"]}]});
        needs.event(&event("p.fix", 3, "approval_requested", manual.clone()));
        // A message lifts it: its turn goes on and ends, and it still waits.
        for (kind, data) in [
            ("turn_resumed", json!({"call_id": "c1"})),
            ("call_lifted", json!({"call_id": "c1", "handle": "proc:1"})),
            ("tool_completed", json!({"call_id": "c1"})),
            ("turn_finished", json!({"status": "completed"})),
        ] {
            needs.event(&event("p.fix", 3, kind, data));
        }
        assert_eq!(needs.calls().collect::<Vec<_>>(), [("p.fix", 3, "c1")]);
        // Its verdict ends it.
        needs.event(&event(
            "p.fix",
            3,
            "lifted_ended",
            json!({"call_id": "c1", "outcome": "denied"}),
        ));
        assert!(!needs.waits("p.fix"));
        // Allowed, it starts as its command.
        needs.event(&event("p.fix", 4, "approval_requested", manual));
        needs.event(&event("p.fix", 4, "call_lifted", json!({"call_id": "c1"})));
        needs.event(&event(
            "p.fix",
            4,
            "turn_finished",
            json!({"status": "completed"}),
        ));
        assert!(needs.waits("p.fix"));
        needs.event(&event(
            "p.fix",
            4,
            "lifted_ended",
            json!({"call_id": "c1", "handle": "proc:2", "outcome": "allowed"}),
        ));
        assert!(!needs.waits("p.fix"));
        assert!(needs.lifted.is_empty());
    }

    #[test]
    fn a_step_of_a_call_lifted_unseen_is_read_again_at_its_turns_end() {
        let mut needs = Needs::default();
        let step = json!({"calls": [{"call_id": "c1/1", "step_of": "c1", "gates": ["manual"]}]});
        needs.event(&event("p.fix", 3, "approval_requested", step));
        // The lift came before these events: its turn's end would let the
        // step go, so what the daemon holds is read again.
        assert!(needs.ends_steps_of_unseen("p.fix", 3));
        assert!(!needs.ends_steps_of_unseen("p.fix", 4));
        needs.event(&event("p.fix", 3, "call_lifted", json!({"call_id": "c1"})));
        assert!(!needs.ends_steps_of_unseen("p.fix", 3));
        // A call of its own, not a step, ends with its turn.
        let manual = json!({"calls": [{"call_id": "c2", "gates": ["manual"]}]});
        needs.event(&event("p.ask", 5, "approval_requested", manual));
        assert!(!needs.ends_steps_of_unseen("p.ask", 5));
        // Listed lifted, the step waits on past the turn.
        let mut lifted = Held::new("p.new", 6, "c3/1");
        (lifted.step_of, lifted.lifted) = (Some("c3".into()), true);
        let step = json!({"calls": [{"call_id": "c3/1", "step_of": "c3", "gates": ["manual"]}]});
        needs.event(&event("p.new", 6, "approval_requested", step));
        needs.held(vec![lifted], |bot| bot == "p.new");
        let end = event("p.new", 6, "turn_finished", json!({"status": "completed"}));
        needs.event(&end);
        assert!(needs.waits("p.new"));
    }

    #[test]
    fn a_lifted_answers_steps_wait_past_its_turn_and_end_with_it() {
        let mut needs = Needs::default();
        let step = |n: u32| json!({"calls": [{"call_id": format!("c1/{n}"), "step_of": "c1", "gates": ["manual"]}]});
        // One step waits as the message lifts its call; another comes after.
        needs.event(&event("p.fix", 3, "approval_requested", step(1)));
        needs.event(&event("p.fix", 3, "call_lifted", json!({"call_id": "c1"})));
        needs.event(&event(
            "p.fix",
            3,
            "turn_finished",
            json!({"status": "completed"}),
        ));
        needs.event(&event("p.fix", 3, "approval_requested", step(2)));
        assert_eq!(
            needs.calls().collect::<Vec<_>>(),
            [("p.fix", 3, "c1/1"), ("p.fix", 3, "c1/2")]
        );
        // A later turn's call of the same id is its own.
        let manual = json!({"calls": [{"call_id": "c1/1", "gates": ["manual"]}]});
        needs.event(&event("p.fix", 4, "approval_requested", manual));
        needs.event(&event(
            "p.fix",
            4,
            "tool_started",
            json!({"call_id": "c1/1"}),
        ));
        needs.event(&event(
            "p.fix",
            4,
            "tool_completed",
            json!({"call_id": "c1/1"}),
        ));
        assert_eq!(
            needs.calls().collect::<Vec<_>>(),
            [("p.fix", 3, "c1/1"), ("p.fix", 3, "c1/2")]
        );
        // Its call's end is theirs.
        needs.event(&event(
            "p.fix",
            3,
            "lifted_ended",
            json!({"call_id": "c1", "outcome": "answered"}),
        ));
        assert!(!needs.waits("p.fix"));
        assert!(needs.steps.is_empty() && needs.lifted.is_empty());
        // Listed, a lifted call's step waits past its turn too.
        let listed = Held {
            step_of: Some("c2".into()),
            lifted: true,
            ..Held::new("p.fix", 5, "c2/1")
        };
        needs.held(vec![listed], |_| true);
        needs.event(&event(
            "p.fix",
            5,
            "turn_finished",
            json!({"status": "completed"}),
        ));
        assert_eq!(needs.calls().collect::<Vec<_>>(), [("p.fix", 5, "c2/1")]);
    }

    #[test]
    fn an_allowed_lifted_answers_steps_wait_until_its_answer() {
        let mut needs = Needs::default();
        let manual = json!({"calls": [{"call_id": "c1", "gates": ["manual"]}]});
        needs.event(&event("p.fix", 3, "approval_requested", manual));
        needs.event(&event("p.fix", 3, "call_lifted", json!({"call_id": "c1"})));
        // Allowed, it waits for its client's answer, not the person.
        needs.event(&event(
            "p.fix",
            3,
            "lifted_ended",
            json!({"call_id": "c1", "outcome": "allowed", "waits_for": "answer"}),
        ));
        assert!(!needs.waits("p.fix"));
        // A step its client asks for then outlives the turn.
        let step = json!({"calls": [{"call_id": "c1/1", "step_of": "c1", "gates": ["manual"]}]});
        needs.event(&event("p.fix", 3, "approval_requested", step));
        needs.event(&event(
            "p.fix",
            3,
            "turn_finished",
            json!({"status": "completed"}),
        ));
        assert_eq!(needs.calls().collect::<Vec<_>>(), [("p.fix", 3, "c1/1")]);
        // Its answer ends both.
        needs.event(&event(
            "p.fix",
            3,
            "lifted_ended",
            json!({"call_id": "c1", "outcome": "answered"}),
        ));
        assert!(!needs.waits("p.fix"));
        assert!(needs.steps.is_empty() && needs.lifted.is_empty());
    }

    #[test]
    fn a_step_a_reviewer_hands_over_waits_with_its_lifted_call() {
        let mut needs = Needs::default();
        needs.event(&event("p.fix", 3, "call_lifted", json!({"call_id": "c1"})));
        // Announced for the reviewer only, then handed to the person.
        let auto = json!({"calls": [{"call_id": "c1/1", "step_of": "c1", "gates": ["auto"]}]});
        needs.event(&event("p.fix", 3, "approval_requested", auto));
        assert!(!needs.waits("p.fix"));
        needs.event(&event(
            "p.fix",
            3,
            "approval_asked",
            json!({"call_id": "c1/1", "step_of": "c1", "tag": "auto"}),
        ));
        needs.event(&event(
            "p.fix",
            3,
            "turn_finished",
            json!({"status": "completed"}),
        ));
        assert_eq!(needs.calls().collect::<Vec<_>>(), [("p.fix", 3, "c1/1")]);
    }

    #[test]
    fn an_event_for_one_agent_looks_at_none_of_another_agents_steps() {
        let step = || json!({"call_id": "c1/1", "step_of": "c1"});
        // Every one of `agents` with a step waiting on the person: the
        // steps looked at to hand them over, then for one more agent's
        // step handed over, decided and its turn ended.
        let looked = |agents: usize| {
            let mut needs = Needs::default();
            VISITED.with(|visited| visited.set(0));
            for n in 0..agents {
                needs.event(&event(&format!("b{n}"), 1, "approval_asked", step()));
            }
            let all = VISITED.with(|visited| visited.replace(0));
            needs.event(&event("a", 1, "approval_asked", step()));
            needs.event(&event("a", 1, "step_decided", json!({"call_id": "c1/1"})));
            needs.event(&event("a", 1, "turn_finished", json!({})));
            assert!(!needs.waits("a") && needs.waits("b0"));
            (all, VISITED.with(|visited| visited.get()))
        };
        let (all, one) = looked(10_000);
        assert_eq!(all, 10_000, "one look a step handed over");
        assert_eq!(one, looked(1).1);
        assert!(one <= 3, "{one}");
    }

    #[test]
    fn a_question_waits_through_its_turns_end_and_is_read_again_then() {
        let mut needs = Needs::default();
        needs.event(&event("p.fix", 2, "tool_started", ask("q1", "Ship it?")));
        assert!(needs.waits("p.fix"));
        assert_eq!(needs.asks["p.fix"].call.as_deref(), Some("q1"));
        // Not whole yet: its history says it, with what Sonny took it as,
        // once the call is taken.
        assert!(needs.asks().next().is_none());
        needs.event(&event(
            "p.fix",
            2,
            "tool_completed",
            json!({"call_id": "q1"}),
        ));
        // Taken, it is read at once, its turn going on or not.
        assert_eq!(needs.due(), vec![("p.fix".to_owned(), Some(2))]);
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
        needs.asked("p.fix", Some(whole("Ship it?")));
        assert!(needs.waits("p.fix"));
        let read: Vec<_> = needs
            .asks()
            .map(|(bot, a)| (bot, a.question.as_str()))
            .collect();
        assert_eq!(read, [("p.fix", "Ship it?")]);
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
        needs.asked("p.fix", Some(whole("Ship it?")));
        needs.event(&event(
            "p.fix",
            3,
            "accepted",
            json!({"from": {"bot": "p.lead"}}),
        ));
        needs.event(&event("p.fix", 3, "accepted", json!({"origin": "trigger"})));
        needs.event(&event("p.fix", 3, "accepted", json!({"person": "dana"})));
        assert!(needs.waits("p.fix"), "only the person answers it");
        assert!(needs.due().is_empty());
        // The person's words: whether they answer it, its history, read
        // at once, says. Until then it waits on them still.
        assert!(!needs.event(&event("p.fix", 4, "accepted", json!({}))));
        assert!(needs.waits("p.fix"));
        assert_eq!(needs.due(), vec![("p.fix".to_owned(), Some(4))]);
        needs.event(&event(
            "p.fix",
            4,
            "turn_finished",
            json!({"status": "completed"}),
        ));
        assert!(needs.due().is_empty(), "read already");
        // Words of their own answer nothing: the read keeps it open.
        needs.asked("p.fix", Some(whole("Ship it?")));
        assert!(needs.waits("p.fix"));
        // Their answer: the read lets it go.
        needs.event(&event("p.fix", 5, "steered", json!({})));
        assert_eq!(needs.due(), vec![("p.fix".to_owned(), Some(5))]);
        needs.asked("p.fix", None);
        assert!(!needs.waits("p.fix"));
    }

    #[test]
    fn what_another_read_replaces_only_the_agents_it_read() {
        let mut kept = Needs::default();
        kept.asked("p.a", Some(whole("A?")));
        kept.asked("p.b", Some(whole("B?")));
        let mut read = Needs::default();
        read.held(vec![Held::new("p.c", 1, "c")], |_| true);
        let bots = ["p.a".to_owned(), "p.c".to_owned()];
        kept.adopt(&read, &bots, &bots);
        assert!(!kept.waits("p.a"));
        assert!(kept.waits("p.b"));
        assert!(kept.waits("p.c"));
        kept.forget("p.b");
        assert!(!kept.waits("p.b"));
    }
}
