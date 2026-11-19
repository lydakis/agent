//! What needs the person in the other projects: the footer's
//! `? 1 elsewhere`, and where `alt+n` goes once this project has no one.
//!
//! It reads what `/status` reads, the way the sky and `/switch` do, on a
//! connection of its own: the calls held for the person ([`needs::listing`])
//! and each idle agent's question as of its last turn, taken from what any
//! window kept beside the store ([`crate::kept_seen`]) and read only where
//! none did. From then on the daemon's events keep it ([`needs::Needs`]),
//! and nothing is read again on a timer.
//!
//! Every agent is followed for the events that can start a wait on the
//! person ([`WIDE`]): a call held for a verdict, a call starting (a
//! question is the ask tool's call), a turn's start and end. A call's end,
//! as many again, is followed only for an agent with a call held for the
//! person or a question asked ([`NARROW`]), from that event on, so nothing
//! is missed between. The daemon has no unfollow: once none of those
//! waits any more, the connection is made again from a new read.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

use agent_client::Client;
use serde_json::{Value, json};
use tokio::sync::{Notify, watch};

use crate::needs::{self, Seen};
use crate::standing;

/// The kinds of event every agent is followed for: those that can start
/// a wait on the person, and a turn's few, which [`needs::Needs::event`]
/// reads.
pub const WIDE: [&str; 10] = [
    "approval_requested",
    "approval_asked",
    "tool_started",
    "step_decided",
    "call_lifted",
    "lifted_ended",
    "accepted",
    "steered",
    "turn_finished",
    "deleted",
];

/// The kind of event an agent that waits on the person is followed for as
/// well: its calls ending, which lets a held call go when it was denied,
/// and takes a question's answer.
pub const NARROW: [&str; 1] = ["tool_completed"];

/// Every project but the window's, followed while the window is open.
pub struct Elsewhere {
    /// The agents of other projects that need the person.
    pub waiting: watch::Receiver<Waiting>,
    task: tokio::task::JoinHandle<()>,
}

/// The agents of other projects that need the person, in the order
/// **Projects** lists their projects.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Waiting {
    /// The place of the window's project in that order.
    pub here: usize,
    /// Each agent, with its project's place.
    pub agents: Vec<(usize, String)>,
}

impl Drop for Elsewhere {
    fn drop(&mut self) {
        self.task.abort();
    }
}

impl Elsewhere {
    /// Follows what needs the person outside `project`, waking the window
    /// (`woken`) when it changes.
    pub fn start(socket: PathBuf, store: PathBuf, project: String, woken: Arc<Notify>) -> Self {
        let (tell, waiting) = watch::channel(Waiting::default());
        let task = tokio::spawn(async move {
            follow(socket, store, project, tell, woken).await;
        });
        Elsewhere { waiting, task }
    }

    /// Whether it stopped, as when the daemon went: to start again.
    pub fn ended(&self) -> bool {
        self.task.is_finished()
    }
}

/// Whether `bot` is an agent of a project other than `project`, of those
/// in `projects` (each by name): not one of Sonny's own (a leading dot),
/// nor a side chat, which `/status` leaves out too.
pub fn other(bot: &str, project: &str, projects: &HashMap<String, usize>) -> bool {
    !bot.starts_with('.')
        && bot.contains('.')
        && crate::side_source(bot).is_none()
        && project_in(bot, projects) != project
}

/// The project `bot` is an agent of: the longest start of its name that
/// names one of `projects`, as a window finds its own, so `shop.web.page`
/// is `shop.web`'s where both projects are; else up to its first dot.
pub fn project_in<'a>(bot: &'a str, projects: &HashMap<String, usize>) -> &'a str {
    (bot.match_indices('.').map(|(at, _)| &bot[..at]).rev())
        .find(|start| projects.contains_key(*start))
        .unwrap_or_else(|| bot.split_once('.').map_or(bot, |(of, _)| of))
}

async fn follow(
    socket: PathBuf,
    store: PathBuf,
    project: String,
    tell: watch::Sender<Waiting>,
    woken: Arc<Notify>,
) {
    // Made again each time none of the agents followed narrowly waits.
    while follow_once(&socket, &store, &project, &tell, &woken).await {}
}

/// One connection's follow: whether to make another, once none of the
/// agents it follows narrowly waits on the person any more.
async fn follow_once(
    socket: &std::path::Path,
    store: &std::path::Path,
    project: &str,
    tell: &watch::Sender<Waiting>,
    woken: &Notify,
) -> bool {
    let Ok((client, mut events)) = Client::connect(socket).await else {
        return false;
    };
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |since| since.as_millis() as u64);
    // From now: where things stand comes from the reads below, and an
    // event that lands while they run is applied after them.
    let asked = json!({"bot": "*.*", "since_ms": now, "kinds": WIDE});
    if client.request("follow", asked).await.is_err() {
        return false;
    }
    let mut needs = needs::Needs::default();
    let Ok(held) = needs::listing(&client, None).await else {
        return false;
    };
    let Ok((listed, _)) = standing::list(&client, "*.*").await else {
        return false;
    };
    let order = in_order(store, &listed);
    let theirs = |bot: &str| other(bot, project, &order);
    needs.held(held, theirs);
    // What the history of each agent with a question open said, kept to
    // read again when a lead marks it done.
    let mut seen_of: HashMap<String, Seen> = HashMap::new();
    let identity = client.identity().map(str::to_owned);
    let kept = crate::kept_seen(store, identity.as_deref());
    let mut read: HashMap<String, (i64, Seen)> = HashMap::new();
    // Each idle agent's question as of its last turn, as `/status` reads
    // it: what a window kept, else read now.
    for bot in &listed {
        if !theirs(&bot.name) || bot.head.is_none() {
            continue;
        }
        // Its turn runs: what it asked, read once the turn ends.
        if bot.running_turn.is_some() {
            needs.read_at_end(&bot.name);
            continue;
        }
        let Some(turn) = bot.last.as_ref().map(|last| last.turn) else {
            continue;
        };
        let was = kept.get(&bot.name);
        let seen = match was.filter(|(at, _)| *at == turn) {
            Some((_, seen)) => seen.clone(),
            None => {
                let seen = match was {
                    Some((_, was)) => needs::scan_since(&client, &bot.name, Some(turn), was).await,
                    None => needs::scan(&client, &bot.name, Some(turn)).await,
                };
                let Some(seen) = seen else {
                    continue;
                };
                read.insert(bot.name.clone(), (turn, seen.clone()));
                seen
            }
        };
        let open = seen.clone().open(store, &bot.name, false);
        if open.is_some() {
            seen_of.insert(bot.name.clone(), seen);
        }
        needs.asked(&bot.name, open);
    }
    if !read.is_empty() {
        crate::keep_seen(store, ".elsewhere", identity.as_deref(), &read);
    }
    // The agents followed narrowly, and whether each has waited since.
    let mut narrow: HashMap<String, bool> = HashMap::new();
    let waiting: Vec<String> = needs.waiting().map(str::to_owned).collect();
    for bot in waiting {
        let asked = json!({"bot": bot, "since_ms": now, "kinds": NARROW});
        if client.request("follow", asked).await.is_err() {
            return false;
        }
        narrow.insert(bot, true);
    }
    // The agents a lead's turn marked done, by the lead: read again once
    // the turn ends, when the mark is beside the store.
    let mut marked: HashMap<String, Vec<String>> = HashMap::new();
    tell_waiting(&needs, project, &order, tell, woken);
    while let Some(event) = events.recv().await {
        let Some(bot) = event["bot"].as_str().filter(|bot| theirs(bot)) else {
            continue;
        };
        let bot = bot.to_owned();
        if let Some(done) = marks(&event, &bot, &order) {
            marked.entry(bot.clone()).or_default().push(done);
        }
        // A call held for the person, or a question asked: its calls from
        // here on, to see it let go or answered.
        if !narrow.contains_key(&bot) && waits_from(&event) {
            let asked = json!({"bot": bot, "after": event["cursor"], "kinds": NARROW});
            if client.request("follow", asked).await.is_err() {
                return false;
            }
            narrow.insert(bot.clone(), false);
        }
        let changed = needs.event(&event);
        // A turn that may have asked or answered, read once it ends.
        let mut read = false;
        for (due, turn) in needs.due() {
            let stirred = needs.stirred(&due);
            if let Some(seen) = needs::scan(&client, &due, turn).await
                && needs.stirred(&due) == stirred
            {
                let open = seen.clone().open(store, &due, false);
                match open.is_some() {
                    true => seen_of.insert(due.clone(), seen),
                    false => seen_of.remove(&due),
                };
                needs.asked(&due, open);
                read = true;
            }
        }
        // A lead's turn that marked agents done: their questions close.
        if event["event"] == "turn_finished" {
            for done in marked.remove(&bot).unwrap_or_default() {
                if let Some(seen) = seen_of.get(&done) {
                    let open = seen.clone().open(store, &done, false);
                    if open.is_none() {
                        seen_of.remove(&done);
                    }
                    needs.asked(&done, open);
                    read = true;
                }
            }
        }
        if changed || read {
            tell_waiting(&needs, project, &order, tell, woken);
        }
        if let Some(waited) = narrow.get_mut(&bot) {
            *waited |= needs.waits(&bot) || event["event"] == "turn_finished";
        }
        // None followed narrowly waits any more: their calls are no longer
        // wanted, and the daemon has no unfollow.
        if !narrow.is_empty()
            && narrow
                .iter()
                .all(|(bot, waited)| *waited && !needs.waits(bot))
        {
            return true;
        }
    }
    false
}

/// The agent a `mark_done` call starting in `event`, by `bot`, marks done:
/// `bot` itself, or a task of its project by name.
fn marks(event: &Value, bot: &str, projects: &HashMap<String, usize>) -> Option<String> {
    let data = &event["data"];
    if event["event"] != "tool_started" || data["name"] != crate::resolved::TOOL {
        return None;
    }
    let arguments: Value = (data["arguments"].as_str())
        .and_then(|arguments| serde_json::from_str(arguments).ok())
        .unwrap_or(Value::Null);
    match crate::resolved::itself(&arguments, bot) {
        true => Some(bot.to_owned()),
        false => crate::resolved::call(&arguments, project_in(bot, projects)),
    }
}

/// Whether `event` holds a call for the person, or asks a question (the
/// ask tool's call starting): after it, the agent's calls ending say when
/// it no longer waits.
fn waits_from(event: &Value) -> bool {
    match event["event"].as_str() {
        Some("approval_asked") => true,
        Some("approval_requested") => {
            (event["data"]["calls"].as_array().into_iter().flatten()).any(needs::person_call)
        }
        Some("tool_started") => event["data"]["name"] == crate::asks::TOOL,
        _ => false,
    }
}

/// Each project's place in the order **Projects** lists them, by its
/// name, from its lead's listing and title.
fn in_order(store: &std::path::Path, listed: &[standing::Agent]) -> HashMap<String, usize> {
    let mut titles = crate::title::Titles::read(store);
    let projects = (listed.iter())
        .filter(|bot| {
            crate::project::is_lead(&bot.name)
                && bot.maker().is_empty()
                && bot.expire_ms.is_none()
                && !crate::project::is_internal(&bot.name)
        })
        .map(|lead| crate::projects::Project {
            lead: lead.name.clone(),
            name: crate::project::lead_project(&lead.name)
                .unwrap_or("")
                .to_owned(),
            title: (titles.named(&lead.name, lead.bot_id, lead.workspace.as_deref()))
                .unwrap_or_default(),
            folder: lead.workspace.clone().unwrap_or_default(),
            ..Default::default()
        })
        .collect();
    let mut order = HashMap::new();
    for lead in crate::projects::order(projects, 0) {
        if let Some(name) = crate::project::lead_project(&lead) {
            let next = order.len();
            order.entry(name.to_owned()).or_insert(next);
        }
    }
    order
}

/// The agents of other projects waiting on the person, in the order
/// **Projects** lists their projects (one made since, after), told to the
/// window when they changed.
fn tell_waiting(
    needs: &needs::Needs,
    project: &str,
    order: &HashMap<String, usize>,
    tell: &watch::Sender<Waiting>,
    woken: &Notify,
) {
    let place = |bot: &str| (order.get(project_in(bot, order)).copied()).unwrap_or(usize::MAX);
    let mut agents: Vec<(usize, String)> = (needs.waiting())
        .filter(|bot| other(bot, project, order))
        .map(|bot| (place(bot), bot.to_owned()))
        .collect();
    agents.sort();
    let mut waiting = Waiting {
        here: order.get(project).copied().unwrap_or(0),
        agents,
    };
    if tell.send_if_modified(|was| {
        let moved = *was != waiting;
        *was = std::mem::take(&mut waiting);
        moved
    }) {
        woken.notify_one();
    }
}

/// Every agent that needs the person, in the order `alt+n` goes round
/// them from the window's project: `local`, its own as the pane lists
/// them, then `elsewhere`'s by their projects' places in **Projects**,
/// from the one after the window's around to the one before it. The same
/// round from every project, so `alt+n` reaches each.
pub fn round<'a>(local: &'a [String], elsewhere: &'a Waiting) -> Vec<&'a str> {
    let (after, before): (Vec<_>, Vec<_>) =
        (elsewhere.agents.iter()).partition(|(place, _)| *place > elsewhere.here);
    (local.iter().map(String::as_str))
        .chain(after.into_iter().map(|(_, bot)| bot.as_str()))
        .chain(before.into_iter().map(|(_, bot)| bot.as_str()))
        .collect()
}

/// Where `alt+n` goes from `here`: the next of `waiting` ([`round`]),
/// around the end, and the first from one not in it.
pub fn next<'a>(waiting: &[&'a str], here: &str) -> Option<&'a str> {
    let first = waiting.first()?;
    Some(match waiting.iter().position(|bot| *bot == here) {
        Some(at) => waiting[(at + 1) % waiting.len()],
        None => first,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn other_projects_are_every_project_but_this_one_and_not_sonnys_own() {
        let projects: HashMap<String, usize> =
            [("tally", 0), ("site", 1), ("shop", 2), ("shop.web", 3)]
                .map(|(name, at)| (name.to_owned(), at))
                .into();
        let other = |bot, project| other(bot, project, &projects);
        assert!(other("site.lead", "tally"));
        assert!(other("books.fix", "tally"));
        assert!(!other("tally.readme", "tally"));
        assert!(!other(".sky.lead", "tally"));
        // A side chat is left out, as `/status` leaves it.
        assert!(!other("site.lead.side-1a2b", "tally"));
        // A project named with a dot is its own, not the one it starts with.
        assert!(other("shop.web.lead", "shop"));
        assert!(other("shop.web.page", "shop"));
        assert!(!other("shop.webby", "shop"));
        assert!(other("shop.fix", "shop.web"));
        assert_eq!(project_in("shop.web.page", &projects), "shop.web");
        assert_eq!(project_in("new.lead", &projects), "new");
    }

    #[test]
    fn a_lead_marking_a_task_done_names_it_in_full() {
        let projects: HashMap<String, usize> = [("shop".to_owned(), 0)].into();
        let started = |arguments: &str| json!({"event": "tool_started", "data": {"name": "mark_done", "arguments": arguments}});
        let task = started(r#"{"task":"fix"}"#);
        assert_eq!(
            marks(&task, "shop.lead", &projects).as_deref(),
            Some("shop.fix")
        );
        let own = started("{}");
        assert_eq!(
            marks(&own, "shop.fix", &projects).as_deref(),
            Some("shop.fix")
        );
        let other = json!({"event": "tool_started", "data": {"name": "shell"}});
        assert_eq!(marks(&other, "shop.lead", &projects), None);
    }

    /// An approval held in another project is told the window as the
    /// daemon's event comes, with no read on a timer; one in the window's
    /// own project is not. Every agent is followed for the few events of
    /// a turn; the held one's calls only from its approval on, and once it
    /// no longer waits, the connection is made again.
    #[tokio::test]
    async fn an_approval_in_another_project_is_told_as_its_event_comes() {
        use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
        let dir = std::env::temp_dir().join(format!("elsewhere-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let socket = dir.join("agent.sock");
        let listener = tokio::net::UnixListener::bind(&socket).unwrap();
        // Each request as (connection, request); events sent to the newest.
        let (asked, mut requests) = tokio::sync::mpsc::unbounded_channel::<(usize, Value)>();
        let (go, gone) = tokio::sync::mpsc::unbounded_channel::<Value>();
        let gone = Arc::new(tokio::sync::Mutex::new(gone));
        tokio::spawn(async move {
            let mut at = 0;
            loop {
                let (stream, _) = listener.accept().await.unwrap();
                at += 1;
                let (asked, gone) = (asked.clone(), gone.clone());
                tokio::spawn(async move {
                    let (read, mut write) = stream.into_split();
                    let ready = json!({"event": "ready", "protocol": agent_client::PROTOCOL});
                    write
                        .write_all(format!("{ready}\n").as_bytes())
                        .await
                        .unwrap();
                    let mut lines = BufReader::new(read).lines();
                    let mut gone = gone.lock().await;
                    loop {
                        tokio::select! {
                            line = lines.next_line() => {
                                let Ok(Some(line)) = line else { return };
                                let request: Value = serde_json::from_str(&line).unwrap();
                                let result = match request["op"].as_str() {
                                    Some("bots") => json!({"bots": [
                                        {"name": "tally.lead", "bot_id": 1},
                                        {"name": "site.lead", "bot_id": 2}], "cursor": 1}),
                                    Some("approvals") => json!({"approvals": []}),
                                    _ => json!({}),
                                };
                                let _ = asked.send((at, request.clone()));
                                let out = json!({"id": request["id"], "result": result});
                                write.write_all(format!("{out}\n").as_bytes()).await.unwrap();
                            }
                            Some(event) = gone.recv() => {
                                write.write_all(format!("{event}\n").as_bytes()).await.unwrap();
                            }
                        }
                    }
                });
            }
        });
        let woken = Arc::new(Notify::new());
        let open = Elsewhere::start(socket, dir.join("state.sqlite"), "tally".into(), woken);
        let mut waiting = open.waiting.clone();
        let held = |bot: &str, call: &str, cursor: i64| {
            json!({"cursor": cursor, "bot": bot, "turn": 1, "event": "approval_requested",
                "data": {"calls": [{"call_id": call, "gates": ["manual"], "name": "shell"}]}})
        };
        let wait = std::time::Duration::from_secs(5);
        let op = |request: &Value| request["op"].as_str().unwrap_or("").to_owned();
        // Every agent, for the few events of a turn; the held calls and
        // the agents read once.
        let (_, first) = requests.recv().await.unwrap();
        assert_eq!(
            (op(&first), first["bot"].clone()),
            ("follow".into(), json!("*.*"))
        );
        assert_eq!(first["kinds"], json!(WIDE));
        while op(&requests.recv().await.unwrap().1) != "bots" {}
        // Its own project's approval, then another's.
        go.send(held("tally.readme", "c1", 7)).unwrap();
        go.send(held("site.fix", "c2", 8)).unwrap();
        tokio::time::timeout(wait, waiting.wait_for(|bots| !bots.agents.is_empty()))
            .await
            .unwrap()
            .unwrap();
        let names = |waiting: &Waiting| -> Vec<String> {
            waiting.agents.iter().map(|(_, bot)| bot.clone()).collect()
        };
        assert_eq!(names(&waiting.borrow()), ["site.fix"]);
        // The held agent's calls, from its approval on; not the other's.
        let (_, narrow) = requests.recv().await.unwrap();
        assert_eq!(
            narrow,
            json!({"id": narrow["id"], "op": "follow", "bot": "site.fix",
            "after": 8, "kinds": NARROW})
        );
        // Allowed, it starts: no one elsewhere, and a new connection.
        go.send(
            json!({"cursor": 9, "bot": "site.fix", "turn": 1, "event": "tool_started",
            "data": {"call_id": "c2", "name": "shell"}}),
        )
        .unwrap();
        tokio::time::timeout(wait, waiting.wait_for(|bots| bots.agents.is_empty()))
            .await
            .unwrap()
            .unwrap();
        let (again, follow) = tokio::time::timeout(wait, requests.recv())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(
            (again, op(&follow), follow["bot"].clone()),
            (2, "follow".into(), json!("*.*"))
        );
        drop(open);
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn alt_n_goes_to_the_next_that_needs_you_around_the_end() {
        let waiting = ["tally.readme", "site.lead", "books.fix"];
        assert_eq!(next(&waiting, "tally.lead"), Some("tally.readme"));
        assert_eq!(next(&waiting, "tally.readme"), Some("site.lead"));
        assert_eq!(next(&waiting, "books.fix"), Some("tally.readme"));
        assert_eq!(next(&[], "tally.lead"), None);
    }

    /// One waiting in each of three projects, A, B and C as **Projects**
    /// lists them: from each, `alt+n` goes to the next, and so to all.
    #[test]
    fn alt_n_reaches_every_project_from_any() {
        let places = [("a.fix", 0), ("b.fix", 1), ("c.fix", 2)];
        let from = |here: usize| {
            let elsewhere = Waiting {
                here,
                agents: (places.iter())
                    .filter(|(_, at)| *at != here)
                    .map(|(bot, at)| (*at, (*bot).to_owned()))
                    .collect(),
            };
            let local = vec![places[here].0.to_owned()];
            let round: Vec<String> = round(&local, &elsewhere)
                .into_iter()
                .map(str::to_owned)
                .collect();
            let round: Vec<&str> = round.iter().map(String::as_str).collect();
            next(&round, places[here].0).map(str::to_owned)
        };
        assert_eq!(from(0).as_deref(), Some("b.fix"));
        assert_eq!(from(1).as_deref(), Some("c.fix"));
        assert_eq!(from(2).as_deref(), Some("a.fix"));
    }
}
