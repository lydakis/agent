//! What needs the person in the other projects: the footer's `← ? 1`,
//! and where `alt+n` goes once this project has no one.
//!
//! One follow for every window of a store: the first window to open takes
//! a lock beside the store and follows; the others hear what it found on
//! a socket beside the store, one line each time it changes, and each
//! leaves out its own project. When that window closes, another takes the
//! lock and follows on. So a hundred windows cost the daemon and the
//! machine what one does.
//!
//! It reads what `/status` reads, the way the sky and `/switch` do, on a
//! connection of its own: the calls held for the person ([`needs::listing`])
//! and each idle agent's question as of its last turn, taken from what any
//! window kept beside the store ([`crate::kept_seen`]) and read only where
//! none did. From then on the daemon's events keep it ([`needs::Needs`]),
//! and nothing is read again on a timer.
//!
//! Every agent is followed only for the events that can start a wait on
//! the person ([`WIDE`]): a call held for a verdict, and a call starting
//! (a question is the ask tool's call; `mark_done` marks a task done). The
//! rest of an agent's events, its calls ending and its turn's end, are
//! followed only while it waits, runs a turn begun before the follow, or
//! marks a task done ([`NARROW`]), from that event on, so nothing is missed
//! between. The daemon has no unfollow: once none of those does any more,
//! a new connection follows every agent on from the last event, with
//! nothing read again.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use agent_client::Client;
use serde_json::{Value, json};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::{UnixListener, UnixStream};
use tokio::sync::{Notify, watch};

use crate::needs::{self, Seen};
use crate::standing;

/// The kinds of event every agent is followed for: those that can start
/// a wait on the person, a call lifted out of its turn and its end (a
/// step of it held for the person waits past the turn), and an agent's
/// closing or deletion.
pub const WIDE: [&str; 7] = [
    "approval_requested",
    "approval_asked",
    "tool_started",
    "call_lifted",
    "lifted_ended",
    "closed",
    "deleted",
];

/// The kinds of event an agent that waits on the person is followed for as
/// well, which [`needs::Needs::event`] reads: its calls ending or decided,
/// which let a held call go or take a question's answer, the person's
/// words, and its turn's end.
pub const NARROW: [&str; 5] = [
    "tool_completed",
    "step_decided",
    "accepted",
    "steered",
    "turn_finished",
];

/// Every project's agents that need the person, followed while the window
/// is open: by this window, or heard from the one that follows.
pub struct Elsewhere {
    /// What needs the person in every project.
    pub waiting: watch::Receiver<Shared>,
    task: tokio::task::JoinHandle<()>,
}

/// What the window that follows tells the others: every project's agents
/// that need the person, in the order **Projects** lists their projects.
#[derive(Clone, Debug, Default, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct Shared {
    /// Each project's place in **Projects**, by name.
    pub order: HashMap<String, usize>,
    /// Each agent with its project's place and its project.
    pub agents: Vec<(usize, String, String)>,
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

impl Shared {
    /// What needs the person outside `project`.
    pub fn outside(&self, project: &str) -> Waiting {
        Waiting {
            here: self.order.get(project).copied().unwrap_or(0),
            agents: (self.agents.iter())
                .filter(|(_, of, _)| of != project)
                .map(|(place, _, bot)| (*place, bot.clone()))
                .collect(),
        }
    }
}

impl Drop for Elsewhere {
    fn drop(&mut self) {
        self.task.abort();
    }
}

impl Elsewhere {
    /// Follows what needs the person in every project, or hears it from
    /// the window that does, waking the window (`woken`) when it changes.
    pub fn start(socket: PathBuf, store: PathBuf, woken: Arc<Notify>) -> Self {
        let (tell, waiting) = watch::channel(Shared::default());
        let task = tokio::spawn(async move {
            share(&socket, &store, &tell, &woken).await;
        });
        Elsewhere { waiting, task }
    }

    /// Whether it stopped, as when the daemon went: to start again.
    pub fn ended(&self) -> bool {
        self.task.is_finished()
    }
}

/// The lock the window that follows holds, and the socket it tells the
/// others on: beside `store`, or, where that path is too long for a Unix
/// socket, the short one the client finds for a daemon's
/// ([`agent_client::socket::default_socket`]).
fn beside(store: &Path) -> (PathBuf, PathBuf) {
    let store = agent_client::socket::canonical_store(store).unwrap_or_else(|_| store.into());
    let at = |end: &str| {
        let mut path = store.as_os_str().to_owned();
        path.push(end);
        PathBuf::from(path)
    };
    let lock = at(".elsewhere.lock");
    let told = agent_client::socket::default_socket(&at(".elsewhere"))
        .unwrap_or_else(|_| at(".elsewhere.sock"));
    (lock, told)
}

/// Follow, where no window of the store does; else hear the one that does,
/// and follow once it is gone. Ends when the daemon goes.
async fn share(socket: &Path, store: &Path, tell: &watch::Sender<Shared>, woken: &Notify) {
    let (lock, told) = beside(store);
    let mut missed = 0;
    loop {
        if let Ok(Some(held)) = crate::scheduler::locked(&lock) {
            lead(socket, store, &told, tell, woken).await;
            drop(held);
            return;
        }
        match UnixStream::connect(&told).await {
            Ok(stream) => {
                missed = 0;
                hear(stream, tell, woken).await;
            }
            // The window that took the lock is still making its socket.
            Err(_) => {
                missed = (missed + 1).min(7);
                tokio::time::sleep(Duration::from_millis(40 << missed)).await;
            }
        }
    }
}

/// The socket the window that follows tells the others on, gone with it.
struct Told(PathBuf);

impl Drop for Told {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.0);
    }
}

/// Follow every project, and tell each window that connects what needs
/// the person, at once and each time it changes.
async fn lead(
    socket: &Path,
    store: &Path,
    told: &Path,
    tell: &watch::Sender<Shared>,
    woken: &Notify,
) {
    let _ = std::fs::remove_file(told);
    let listener = UnixListener::bind(told).ok();
    let _told = listener.as_ref().map(|_| Told(told.to_owned()));
    let mut hearing = tokio::task::JoinSet::new();
    let following = follow(socket, store, tell, woken);
    tokio::pin!(following);
    loop {
        tokio::select! {
            () = &mut following => return,
            Ok((stream, _)) = async { listener.as_ref().unwrap().accept().await }, if listener.is_some() => {
                hearing.spawn(tell_on(stream, tell.subscribe()));
            }
            Some(_) = hearing.join_next(), if !hearing.is_empty() => {}
        }
    }
}

/// What needs the person, to a window that connected: now, and again each
/// time it changes, one line each.
async fn tell_on(stream: UnixStream, mut shared: watch::Receiver<Shared>) {
    let (mut read, mut write) = stream.into_split();
    shared.mark_changed();
    let mut gone = [0u8; 1];
    loop {
        let line = serde_json::to_string(&*shared.borrow_and_update()).unwrap_or_default();
        if write
            .write_all(format!("{line}\n").as_bytes())
            .await
            .is_err()
        {
            return;
        }
        tokio::select! {
            changed = shared.changed() => if changed.is_err() { return },
            // The window closed.
            _ = tokio::io::AsyncReadExt::read(&mut read, &mut gone) => return,
        }
    }
}

/// What the window that follows tells, until it goes.
async fn hear(stream: UnixStream, tell: &watch::Sender<Shared>, woken: &Notify) {
    let mut lines = BufReader::new(stream).lines();
    while let Ok(Some(line)) = lines.next_line().await {
        let Ok(mut shared) = serde_json::from_str::<Shared>(&line) else {
            continue;
        };
        if tell.send_if_modified(|was| {
            let moved = *was != shared;
            *was = std::mem::take(&mut shared);
            moved
        }) {
            woken.notify_one();
        }
    }
}

/// Whether `bot` is an agent of a project: not one of Sonny's own (a
/// leading dot), nor a side chat, which `/status` leaves out too. A side
/// chat is named as `/side` names one, but for the tasks in
/// `named_as_sides`, listed with such a name though no side chat's record.
pub fn other(bot: &str, named_as_sides: &HashSet<String>) -> bool {
    !bot.starts_with('.')
        && bot.contains('.')
        && (crate::side_source(bot).is_none() || named_as_sides.contains(bot))
}

/// The project `bot` is an agent of: the longest start of its name that
/// names one of `projects`, as a window finds its own, so `shop.web.page`
/// is `shop.web`'s where both projects are; else up to its first dot.
pub fn project_in<'a>(bot: &'a str, projects: &HashMap<String, usize>) -> &'a str {
    (bot.match_indices('.').map(|(at, _)| &bot[..at]).rev())
        .find(|start| projects.contains_key(*start))
        .unwrap_or_else(|| bot.split_once('.').map_or(bot, |(of, _)| of))
}

/// Follow every project, telling what needs the person in `tell`.
async fn follow(socket: &Path, store: &Path, tell: &watch::Sender<Shared>, woken: &Notify) {
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |since| since.as_millis() as u64);
    // From now: where things stand comes from the reads below, and an
    // event that lands while they run is applied after them.
    let Some((mut client, mut events)) = follow_wide(socket, json!({"since_ms": now})).await else {
        return;
    };
    let Some(seeded) = seed(&client, store).await else {
        return;
    };
    let Seeded {
        mut needs,
        mut order,
        named_as_sides,
        mut seen_of,
        running,
    } = seeded;
    let theirs = |bot: &str| other(bot, &named_as_sides);
    // The agents followed narrowly, and whether each has waited since:
    // those that wait now, and those whose turn runs, to its end.
    let mut narrow: HashMap<String, bool> = HashMap::new();
    let waiting: Vec<String> = needs.waiting().map(str::to_owned).collect();
    for (bot, waited) in (waiting.into_iter().map(|bot| (bot, true)))
        .chain(running.into_iter().map(|bot| (bot, false)))
    {
        if narrow.contains_key(&bot) {
            continue;
        }
        let asked = json!({"bot": bot, "since_ms": now, "kinds": NARROW});
        if client.request("follow", asked).await.is_err() {
            return;
        }
        narrow.insert(bot, waited);
    }
    // The agents a lead's turn marked done, by the lead: read again once
    // the turn ends, when the mark is beside the store.
    let mut marked: HashMap<String, Vec<String>> = HashMap::new();
    tell_waiting(&needs, &order, tell, woken);
    loop {
        let Some(event) = events.recv().await else {
            return;
        };
        let cursor = event["cursor"].clone();
        let Some(bot) = event["bot"].as_str() else {
            continue;
        };
        let bot = bot.to_owned();
        let ours = theirs(&bot);
        // A side chat needs no one, but it may mark its source done.
        let side = (!ours).then(|| crate::side_source(&bot)).flatten();
        let done = marks(&event, &bot, &order).map(|done| match side {
            Some(source) if done == bot => source.to_owned(),
            _ => done,
        });
        if !ours && done.is_none() && !marked.contains_key(&bot) {
            continue;
        }
        // A lead the start did not list: its project is placed after the
        // others, so its agents are told as its own.
        if ours
            && crate::project::is_lead(&bot)
            && let Some(project) = crate::project::lead_project(&bot)
            && !order.contains_key(project)
        {
            order.insert(project.to_owned(), order.len());
        }
        // A call held for the person, a question asked, or agents marked
        // done: its calls and its turn's end from here on, to see it let
        // go, answered or over.
        if !narrow.contains_key(&bot) && (ours && waits_from(&event) || done.is_some()) {
            let asked = json!({"bot": bot, "after": cursor, "kinds": NARROW});
            if client.request("follow", asked).await.is_err() {
                return;
            }
            narrow.insert(bot.clone(), false);
        }
        if let Some(done) = done {
            marked.entry(bot.clone()).or_default().push(done);
        }
        let mut changed = ours && needs.event(&event);
        // Closed, it needs no one, as `/status` lists it no more.
        if ours && event["event"] == "closed" {
            changed |= needs.waits(&bot);
            needs.forget(&bot);
            seen_of.remove(&bot);
        }
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
            tell_waiting(&needs, &order, tell, woken);
        }
        if let Some(waited) = narrow.get_mut(&bot) {
            *waited |= needs.waits(&bot) || event["event"] == "turn_finished";
        }
        // None followed narrowly waits, runs, marks any more or is to be
        // read at its turn's end: their events are no longer wanted, and
        // the daemon has no unfollow. A new connection follows every agent
        // on from this event, with nothing read again.
        if done_with(&narrow, &needs, &marked) {
            drop((client, events));
            let Some(again) = follow_wide(socket, json!({"after": cursor})).await else {
                return;
            };
            (client, events) = again;
            narrow.clear();
        }
    }
}

/// Whether none of the agents followed narrowly is wanted any more: each
/// has waited or run to its turn's end, and none waits now, is to be read
/// when its turn ends (the person's words went past its question), or
/// marked another done.
fn done_with(
    narrow: &HashMap<String, bool>,
    needs: &needs::Needs,
    marked: &HashMap<String, Vec<String>>,
) -> bool {
    !narrow.is_empty()
        && (narrow.iter()).all(|(bot, waited)| *waited && !needs.waits(bot) && !needs.unsure(bot))
        && marked.is_empty()
}

/// A connection of its own following every agent for [`WIDE`], from
/// `from` (`since_ms` or `after`).
async fn follow_wide(
    socket: &std::path::Path,
    from: Value,
) -> Option<(Arc<Client>, agent_client::Events)> {
    let (client, events) = Client::connect(socket).await.ok()?;
    let mut asked = json!({"bot": "*.*", "kinds": WIDE});
    if let (Some(asked), Some(from)) = (asked.as_object_mut(), from.as_object()) {
        asked.extend(from.clone());
    }
    client.request("follow", asked).await.ok()?;
    Some((client, events))
}

/// Where things stood when the follow began.
struct Seeded {
    needs: needs::Needs,
    /// Each project's place in **Projects**, by name.
    order: HashMap<String, usize>,
    /// The tasks listed with a side chat's name that are not side chats.
    named_as_sides: HashSet<String>,
    /// What the history of each agent with a question open said, kept to
    /// read again when a lead marks it done.
    seen_of: HashMap<String, Seen>,
    /// The agents whose turn ran, read once it ends.
    running: Vec<String>,
}

/// The calls held for the person, and each idle agent's question as of
/// its last turn, as `/status` reads it: what a window kept, else read
/// now. The listing of every agent goes once this is read.
async fn seed(client: &Client, store: &Path) -> Option<Seeded> {
    let mut needs = needs::Needs::default();
    let held = needs::listing(client, None).await.ok()?;
    let (listed, _) = standing::list(client, "*.*").await.ok()?;
    let order = in_order(store, &listed);
    let named_as_sides: HashSet<String> = (listed.iter())
        .filter(|bot| {
            crate::side_source(&bot.name)
                .is_some_and(|source| bot.maker() != source || bot.expire_ms.is_none())
        })
        .map(|bot| bot.name.clone())
        .collect();
    let theirs = |bot: &str| other(bot, &named_as_sides);
    needs.held(held, theirs);
    let mut seen_of: HashMap<String, Seen> = HashMap::new();
    let mut running = Vec::new();
    let identity = client.identity().map(str::to_owned);
    let kept = crate::kept_seen(store, identity.as_deref());
    let mut read: HashMap<String, (i64, Seen)> = HashMap::new();
    for bot in &listed {
        if !theirs(&bot.name) || bot.head.is_none() {
            continue;
        }
        // Its turn runs: what it asked, read once the turn ends.
        if bot.running_turn.is_some() {
            needs.read_at_end(&bot.name);
            running.push(bot.name.clone());
            continue;
        }
        // A fork that has run no turn of its own yet keeps its source's
        // history, and any question open in it: read it whole.
        let Some(turn) = bot.last.as_ref().map(|last| last.turn) else {
            let Some(seen) = needs::scan(client, &bot.name, None).await else {
                continue;
            };
            let open = seen.clone().open(store, &bot.name, false);
            if open.is_some() {
                seen_of.insert(bot.name.clone(), seen);
            }
            needs.asked(&bot.name, open);
            continue;
        };
        let was = kept.get(&bot.name);
        let seen = match was.filter(|(at, _)| *at == turn) {
            Some((_, seen)) => seen.clone(),
            None => {
                let seen = match was {
                    Some((_, was)) => needs::scan_since(client, &bot.name, Some(turn), was).await,
                    None => needs::scan(client, &bot.name, Some(turn)).await,
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
    Some(Seeded {
        needs,
        order,
        named_as_sides,
        seen_of,
        running,
    })
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

/// Every project's agents waiting on the person, in the order
/// **Projects** lists their projects (one made since, after), told to the
/// windows when they changed.
fn tell_waiting(
    needs: &needs::Needs,
    order: &HashMap<String, usize>,
    tell: &watch::Sender<Shared>,
    woken: &Notify,
) {
    let mut agents: Vec<(usize, String, String)> = (needs.waiting())
        .map(|bot| {
            let project = project_in(bot, order);
            let place = order.get(project).copied().unwrap_or(usize::MAX);
            (place, project.to_owned(), bot.to_owned())
        })
        .collect();
    agents.sort();
    if tell.borrow().agents == agents && tell.borrow().order == *order {
        return;
    }
    tell.send_replace(Shared {
        order: order.clone(),
        agents,
    });
    woken.notify_one();
}

/// Every agent that needs the person, in the order `alt+n` goes round
/// them from the window's project: `local`, its own, then `elsewhere`'s by
/// their projects' places in **Projects**, from the one after the
/// window's around to the one before it; in each project its lead first,
/// then by name. The same round from every agent of every project, so
/// `alt+n` reaches each.
pub fn round<'a>(local: &'a [String], elsewhere: &'a Waiting) -> Vec<&'a str> {
    let here = elsewhere.here;
    let mut all: Vec<(bool, usize, bool, &str)> = (local.iter())
        .map(|bot| (here, bot.as_str()))
        .chain((elsewhere.agents.iter()).map(|(place, bot)| (*place, bot.as_str())))
        .map(|(place, bot)| (place < here, place, !crate::project::is_lead(bot), bot))
        .collect();
    all.sort();
    let mut seen = HashSet::new();
    (all.into_iter())
        .map(|(_, _, _, bot)| bot)
        .filter(|bot| seen.insert(*bot))
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
        let sides: HashSet<String> = ["site.side-feed".to_owned()].into();
        // Every window's: what its own project holds is left out below.
        let other = |bot| other(bot, &sides);
        assert!(other("site.lead"));
        assert!(other("books.fix"));
        assert!(!other(".sky.lead"));
        // A side chat is left out, as `/status` leaves it.
        assert!(!other("site.lead.side-1a2b"));
        // A task named like one, which its record says it is not.
        assert!(other("site.side-feed"));
        // A project named with a dot is its own, not the one it starts with.
        let waiting = [
            "tally.readme",
            "shop.web.lead",
            "shop.web.page",
            "shop.webby",
            "shop.fix",
        ];
        let shared = Shared {
            order: projects.clone(),
            agents: (waiting.iter())
                .map(|bot| {
                    let project = project_in(bot, &projects);
                    (projects[project], project.to_owned(), (*bot).to_owned())
                })
                .collect(),
        };
        let outside = |project: &str| -> Vec<String> {
            (shared.outside(project).agents.into_iter())
                .map(|(_, bot)| bot)
                .collect()
        };
        assert_eq!(
            outside("shop"),
            ["tally.readme", "shop.web.lead", "shop.web.page"]
        );
        assert_eq!(
            outside("tally"),
            ["shop.web.lead", "shop.web.page", "shop.webby", "shop.fix"]
        );
        assert_eq!(
            outside("shop.web"),
            ["tally.readme", "shop.webby", "shop.fix"]
        );
        assert_eq!(shared.outside("shop.web").here, 3);
        assert_eq!(project_in("shop.web.page", &projects), "shop.web");
        assert_eq!(project_in("new.lead", &projects), "new");
    }

    /// The socket the others hear on fits a Unix socket's address, however
    /// long the store's path.
    #[test]
    fn the_shared_socket_fits_beside_a_long_store() {
        let short = std::env::temp_dir().join("s/state.sqlite");
        let (lock, told) = beside(&short);
        assert!(
            lock.to_string_lossy()
                .ends_with("state.sqlite.elsewhere.lock")
        );
        assert!(
            told.to_string_lossy()
                .ends_with("state.sqlite.elsewhere.sock")
        );
        let long = std::env::temp_dir()
            .join("a".repeat(120))
            .join("state.sqlite");
        let (lock, told) = beside(&long);
        assert!(
            lock.to_string_lossy()
                .ends_with("state.sqlite.elsewhere.lock")
        );
        assert!(
            std::os::unix::net::SocketAddr::from_pathname(&told).is_ok(),
            "{told:?}"
        );
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
    /// no longer waits, the connection is made again. One window of the
    /// store follows, the others hear it, and one of them follows on once
    /// it closes.
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
        let store = dir.join("state.sqlite");
        let open = Elsewhere::start(socket.clone(), store.clone(), woken.clone());
        let mut waiting = open.waiting.clone();
        let held = |bot: &str, call: &str, cursor: i64| {
            json!({"cursor": cursor, "bot": bot, "turn": 1, "event": "approval_requested",
                "data": {"calls": [{"call_id": call, "gates": ["manual"], "name": "shell"}]}})
        };
        let started = |bot: &str, call: &str, cursor: i64| {
            json!({"cursor": cursor, "bot": bot, "turn": 1, "event": "tool_started",
                "data": {"call_id": call, "name": "shell"}})
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
        // A second window of the store hears the first: it follows nothing.
        let second = Elsewhere::start(socket, store, woken);
        let mut heard = second.waiting.clone();
        // An approval in each project.
        go.send(held("tally.readme", "c1", 7)).unwrap();
        go.send(held("site.fix", "c2", 8)).unwrap();
        let names = |waiting: &Waiting| -> Vec<String> {
            waiting.agents.iter().map(|(_, bot)| bot.clone()).collect()
        };
        for shared in [&mut waiting, &mut heard] {
            tokio::time::timeout(wait, shared.wait_for(|shared| shared.agents.len() == 2))
                .await
                .unwrap()
                .unwrap();
        }
        // Each window leaves out its own project's.
        assert_eq!(names(&waiting.borrow().outside("tally")), ["site.fix"]);
        assert_eq!(names(&heard.borrow().outside("site")), ["tally.readme"]);
        // The held agents' calls, each from its approval on.
        for (bot, after) in [("tally.readme", 7), ("site.fix", 8)] {
            let (at, narrow) = requests.recv().await.unwrap();
            assert_eq!(at, 1);
            assert_eq!(
                narrow,
                json!({"id": narrow["id"], "op": "follow", "bot": bot,
                "after": after, "kinds": NARROW})
            );
        }
        // Allowed, they start: no one waits, and a new connection.
        go.send(started("tally.readme", "c1", 9)).unwrap();
        go.send(started("site.fix", "c2", 10)).unwrap();
        tokio::time::timeout(wait, heard.wait_for(|shared| shared.agents.is_empty()))
            .await
            .unwrap()
            .unwrap();
        let (again, follow) = tokio::time::timeout(wait, requests.recv())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(
            (
                again,
                op(&follow),
                follow["bot"].clone(),
                follow["after"].clone()
            ),
            (2, "follow".into(), json!("*.*"), json!(10))
        );
        // The first window closes: the second follows on, on its own.
        drop(open);
        let (at, follow) = tokio::time::timeout(wait, requests.recv())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(
            (at, op(&follow), follow["bot"].clone()),
            (3, "follow".into(), json!("*.*"))
        );
        go.send(held("tally.readme", "c3", 11)).unwrap();
        tokio::time::timeout(wait, heard.wait_for(|shared| shared.agents.len() == 1))
            .await
            .unwrap()
            .unwrap();
        assert_eq!(names(&heard.borrow().outside("site")), ["tally.readme"]);
        drop(second);
        std::fs::remove_dir_all(dir).unwrap();
    }

    /// A question the person's words went past is read again at its
    /// turn's end, so its agent's turn's end is followed till then.
    #[test]
    fn an_agent_whose_question_is_unsure_is_followed_to_its_turn_end() {
        let event = |kind: &str, data: Value| json!({"bot": "site.fix", "turn": 1, "event": kind, "data": data});
        let mut needs = needs::Needs::default();
        let asked = json!({"call_id": "c1", "name": crate::asks::TOOL});
        needs.event(&event("tool_started", asked));
        let narrow: HashMap<String, bool> = [("site.fix".to_owned(), true)].into();
        let marked = HashMap::new();
        assert!(!done_with(&narrow, &needs, &marked));
        // A reaction or words of the person's: answered or not, the
        // history says once the turn ends.
        needs.event(&event("accepted", json!({"text": "ok"})));
        assert!(!needs.waits("site.fix"));
        assert!(!done_with(&narrow, &needs, &marked));
        needs.event(&event("turn_finished", json!({})));
        assert_eq!(needs.due().len(), 1);
        needs.asked("site.fix", None);
        assert!(done_with(&narrow, &needs, &marked));
    }

    #[test]
    fn alt_n_goes_to_the_next_that_needs_you_around_the_end() {
        let waiting = ["tally.readme", "site.lead", "books.fix"];
        assert_eq!(next(&waiting, "tally.lead"), Some("tally.readme"));
        assert_eq!(next(&waiting, "tally.readme"), Some("site.lead"));
        assert_eq!(next(&waiting, "books.fix"), Some("tally.readme"));
        assert_eq!(next(&[], "tally.lead"), None);
    }

    /// Agents waiting in three projects, A, B and C as **Projects** lists
    /// them, two in B: from each agent's window, `alt+n` goes to the next,
    /// and so to all, whatever order a window's own are in.
    #[test]
    fn alt_n_reaches_every_project_from_any() {
        let places = [("a.lead", 0), ("b.ask", 1), ("b.lead", 1), ("c.fix", 2)];
        let from = |at: &str| {
            let here = places.iter().find(|(bot, _)| *bot == at).unwrap().1;
            let elsewhere = Waiting {
                here,
                agents: (places.iter())
                    .filter(|(_, place)| *place != here)
                    .map(|(bot, place)| (*place, (*bot).to_owned()))
                    .collect(),
            };
            let local: Vec<String> = (places.iter())
                .filter(|(_, place)| *place == here)
                .map(|(bot, _)| (*bot).to_owned())
                .collect();
            let round: Vec<&str> = round(&local, &elsewhere);
            next(&round, at).map(str::to_owned)
        };
        let mut at = "a.lead".to_owned();
        let mut went = Vec::new();
        for _ in 0..places.len() {
            at = from(&at).unwrap();
            went.push(at.clone());
        }
        assert_eq!(went, ["b.lead", "b.ask", "c.fix", "a.lead"]);
    }
}
