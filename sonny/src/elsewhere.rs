//! What needs the person in the other projects: the footer's
//! `? 1 elsewhere`, and where `alt+n` goes once this project has no one.
//!
//! It reads what `/status` reads, the way the sky and `/switch` do, on a
//! connection of its own: the calls held for the person ([`needs::listing`])
//! and each idle agent's question as of its last turn, taken from what any
//! window kept beside the store ([`crate::kept_seen`]) and read only where
//! none did. From then on the daemon's events keep it ([`needs::Needs`]):
//! a call held or let go is an event, and a turn that may have asked or
//! answered something is read once, when it ends. Nothing is read again
//! on a timer.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

use agent_client::Client;
use serde_json::json;
use tokio::sync::{Notify, watch};

use crate::needs::{self, Seen};
use crate::standing;

/// The kinds of event that change what waits on the person, as
/// [`needs::Needs::event`] reads them.
pub const KINDS: [&str; 11] = [
    "approval_requested",
    "approval_asked",
    "step_decided",
    "tool_started",
    "tool_completed",
    "call_lifted",
    "lifted_ended",
    "accepted",
    "steered",
    "turn_finished",
    "deleted",
];

/// Every project but the window's, followed while the window is open.
pub struct Elsewhere {
    /// The agents of other projects that need the person, by name.
    pub waiting: watch::Receiver<Vec<String>>,
    task: tokio::task::JoinHandle<()>,
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
        let (tell, waiting) = watch::channel(Vec::new());
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

/// Whether `bot` is an agent of a project other than `project`: not one of
/// Sonny's own (a leading dot).
pub fn other(bot: &str, project: &str) -> bool {
    !bot.starts_with('.') && bot.split_once('.').is_some_and(|(of, _)| of != project)
}

async fn follow(
    socket: PathBuf,
    store: PathBuf,
    project: String,
    tell: watch::Sender<Vec<String>>,
    woken: Arc<Notify>,
) {
    let Ok((client, mut events)) = Client::connect(&socket).await else {
        return;
    };
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |since| since.as_millis() as u64);
    // From now: where things stand comes from the reads below, and an
    // event that lands while they run is applied after them.
    let asked = json!({"bot": "*.*", "since_ms": now, "kinds": KINDS});
    if client.request("follow", asked).await.is_err() {
        return;
    }
    let mut needs = needs::Needs::default();
    let Ok(held) = needs::listing(&client, None).await else {
        return;
    };
    needs.held(held, |_| true);
    let Ok((listed, _)) = standing::list(&client, "*.*").await else {
        return;
    };
    let order = in_order(&store, &listed);
    let identity = client.identity().map(str::to_owned);
    let kept = crate::kept_seen(&store, identity.as_deref());
    let mut read: HashMap<String, (i64, Seen)> = HashMap::new();
    // Each idle agent's question as of its last turn, as `/status` reads
    // it: what a window kept, else read now.
    for bot in &listed {
        if !other(&bot.name, &project) || bot.head.is_none() || bot.running_turn.is_some() {
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
        needs.asked(&bot.name, seen.open(&store, &bot.name, false));
    }
    if !read.is_empty() {
        crate::keep_seen(&store, ".elsewhere", identity.as_deref(), &read);
    }
    tell_waiting(&needs, &project, &order, &tell, &woken);
    while let Some(event) = events.recv().await {
        let changed = needs.event(&event);
        // A turn that may have asked or answered, read once it ends.
        let mut read = false;
        for (bot, turn) in needs.due() {
            if !other(&bot, &project) {
                needs.sure(&bot);
                continue;
            }
            let stirred = needs.stirred(&bot);
            if let Some(seen) = needs::scan(&client, &bot, turn).await
                && needs.stirred(&bot) == stirred
            {
                needs.asked(&bot, seen.open(&store, &bot, false));
                read = true;
            }
        }
        if changed || read {
            tell_waiting(&needs, &project, &order, &tell, &woken);
        }
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
    tell: &watch::Sender<Vec<String>>,
    woken: &Notify,
) {
    let mut waiting: Vec<String> = (needs.waiting())
        .filter(|bot| other(bot, project))
        .map(str::to_owned)
        .collect();
    let place = |bot: &str| {
        let of = bot.split_once('.').map_or(bot, |(of, _)| of);
        (order.get(of).copied().unwrap_or(usize::MAX), bot.to_owned())
    };
    waiting.sort_by_key(|bot| place(bot));
    if tell.send_if_modified(|was| {
        let moved = *was != waiting;
        *was = std::mem::take(&mut waiting);
        moved
    }) {
        woken.notify_one();
    }
}

/// Where `alt+n` goes from `here`: the next of `waiting`, this project's
/// first as the pane lists them, then the other projects' in the order
/// **Projects** has; around the end, and the first from one not in it.
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
        assert!(other("site.lead", "tally"));
        assert!(other("books.fix", "tally"));
        assert!(!other("tally.readme", "tally"));
        assert!(!other(".sky.lead", "tally"));
    }

    /// An approval held in another project is told the window as the
    /// daemon's event comes, with no read on a timer; one in the window's
    /// own project, and one let go, are not.
    #[tokio::test]
    async fn an_approval_in_another_project_is_told_as_its_event_comes() {
        use serde_json::Value;
        use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
        let dir = std::env::temp_dir().join(format!("elsewhere-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let socket = dir.join("agent.sock");
        let listener = tokio::net::UnixListener::bind(&socket).unwrap();
        let (ops, mut asked_ops) = tokio::sync::mpsc::unbounded_channel::<String>();
        let (go, mut gone) = tokio::sync::mpsc::unbounded_channel::<Value>();
        tokio::spawn(async move {
            let (stream, _) = listener.accept().await.unwrap();
            let (read, mut write) = stream.into_split();
            let ready = json!({"event": "ready", "protocol": agent_client::PROTOCOL});
            write
                .write_all(format!("{ready}\n").as_bytes())
                .await
                .unwrap();
            let mut lines = BufReader::new(read).lines();
            loop {
                tokio::select! {
                    line = lines.next_line() => {
                        let Ok(Some(line)) = line else { return };
                        let asked: Value = serde_json::from_str(&line).unwrap();
                        let op = asked["op"].as_str().unwrap_or("").to_owned();
                        let result = match op.as_str() {
                            "bots" => json!({"bots": [
                                {"name": "tally.lead", "bot_id": 1},
                                {"name": "site.lead", "bot_id": 2}], "cursor": 1}),
                            "approvals" => json!({"approvals": []}),
                            _ => json!({}),
                        };
                        let _ = ops.send(op);
                        let out = json!({"id": asked["id"], "result": result});
                        write.write_all(format!("{out}\n").as_bytes()).await.unwrap();
                    }
                    Some(event) = gone.recv() => {
                        write.write_all(format!("{event}\n").as_bytes()).await.unwrap();
                    }
                }
            }
        });
        let woken = Arc::new(Notify::new());
        let open = Elsewhere::start(
            socket,
            dir.join("state.sqlite"),
            "tally".into(),
            woken.clone(),
        );
        let mut waiting = open.waiting.clone();
        let held = |bot: &str, call: &str| {
            json!({"bot": bot, "turn": 1, "event": "approval_requested",
                "data": {"calls": [{"call_id": call, "gates": ["manual"]}]}})
        };
        // Its own project's approval, then another's.
        let wait = std::time::Duration::from_secs(5);
        while asked_ops.recv().await.as_deref() != Some("bots") {}
        go.send(held("tally.readme", "c1")).unwrap();
        go.send(held("site.fix", "c2")).unwrap();
        tokio::time::timeout(wait, waiting.wait_for(|bots| !bots.is_empty()))
            .await
            .unwrap()
            .unwrap();
        assert_eq!(*waiting.borrow(), ["site.fix"]);
        // Allowed, it starts: no one elsewhere.
        go.send(
            json!({"bot": "site.fix", "turn": 1, "event": "tool_started",
            "data": {"call_id": "c2", "name": "shell"}}),
        )
        .unwrap();
        tokio::time::timeout(wait, waiting.wait_for(|bots| bots.is_empty()))
            .await
            .unwrap()
            .unwrap();
        // The follow, the held calls and the agents read once; nothing again.
        drop(open);
        let mut asked = Vec::new();
        while let Ok(op) = asked_ops.try_recv() {
            asked.push(op);
        }
        assert!(asked.is_empty(), "{asked:?}");
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
}
