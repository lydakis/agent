//! `/background`: what runs for the project's agents with no one typing,
//! read from files with no model or daemon call: the triggers that wake
//! them, which the system's scheduler keeps (`trigger`), and the lookout's subscriptions
//! for them (`lookout::subs`). From there a trigger is fired now or
//! removed, and a subscription removed; the one Sonny writes for a lead
//! stays, since its project opening writes it again. How many an agent
//! has is the `↻` its `/status` row shows on `m`. What failed comes
//! first, and is the `✗` beside it: a trigger whose fire failed, or whose
//! fire went out and whose turn then failed, as when the machine was
//! offline or a login expired, the daemon saying how the turn ended; and a
//! server the nightly memory cleanup could not read, by the fact it saves.
//! Last, the commands the agents left running, which the daemon is asked
//! for (`commands`): Enter opens a row's agent, and x stops one.
use crate::commands::{self, Commands};
use crate::{kept, lookout, trigger};
use ratatui::crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use ratatui::{
    style::{Color, Stylize},
    text::{Line, Span},
};
use serde_json::{Value, json};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use unicode_width::UnicodeWidthStr;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Kind {
    Trigger,
    Subscription,
    /// A server the nightly memory cleanup could not read: its
    /// `unreadable-SERVER` fact.
    Server,
    /// The nightly memory cleanup Sonny turns on by default, which it
    /// could not schedule ([`trigger::cleanup_unscheduled`]).
    Setup,
}

#[derive(Clone, Debug, PartialEq)]
pub struct Item {
    pub kind: Kind,
    pub name: String,
    /// Whom it is for: the agent a trigger messages, or the subscriber.
    pub bot: String,
    /// Every agent it is for: a trigger's also the one that added it to
    /// start its agent, and the one its answers go to.
    pub bots: Vec<String>,
    /// When it acts: a trigger's time or event.
    pub when: String,
    /// What it does: a trigger's message, a subscription's watch.
    pub what: String,
    /// What came of it: a trigger's sends and last fire; whether a
    /// subscription is Sonny's own.
    pub facts: Vec<String>,
    /// Who gets a trigger's answers, when not the agent it messages.
    pub reply_to: Option<String>,
    /// Sonny's own: a lead's `tasks` subscription. Never counted, never
    /// removed.
    pub own: bool,
    /// Why it failed, when its last fire or the turn it sent did, or the
    /// server could not be read.
    pub failed: Option<String>,
    /// The turn a trigger's last fire sent, whose end `turns` tells.
    pub handle: Option<String>,
    /// When a trigger last fired, in ms: a fire that sent no turn is still
    /// a fire of its own.
    pub fired_ms: Option<u64>,
    /// The sources a subscription hears, by name.
    pub sources: Vec<String>,
    /// What makes its failure the same one when `failed`'s words change:
    /// a broken `--if`'s break, whatever each failed check printed.
    pub episode: Option<String>,
    /// An automation's whole message, which its page shows.
    pub message: String,
    /// A follow as the lookout keeps it, which its page shows whole.
    pub sub: Option<lookout::subs::Sub>,
    /// A question's reminder: Sonny's own automation for an agent's open
    /// question, shown as the question and what happens when, never as
    /// its gate. It goes once the question is answered.
    pub reminder: Option<crate::asks::Reminder>,
    /// An automation's row as Sonny's scheduler lists it, which its page
    /// shows; null for anything else.
    pub row: Value,
}

/// Every trigger made on the daemon whose store is `store_id` and every
/// subscription beside `store` that is for an agent `ours` takes, each
/// trigger's last fire with how its turn ended where `turns` has it.
pub fn read(
    store: &Path,
    store_id: Option<&str>,
    turns: &HashMap<String, Value>,
    ours: impl Fn(&str) -> bool,
) -> Vec<Item> {
    let mut items = Vec::new();
    if let Some(id) = store_id {
        for (mut row, bots) in triggers(id) {
            if bots.iter().any(|bot| ours(bot)) {
                trigger::with_turn(&mut row, turns);
                items.push(trigger_item(&row, bots));
            }
        }
    }
    let down = down(store);
    for (subscriber, subs) in subscriptions(store) {
        if !ours(&subscriber) {
            continue;
        }
        let project = crate::project::lead_project(&subscriber);
        for sub in subs {
            let own = project.is_some_and(|p| sub == lookout::subs::Sub::lead(&subscriber, p));
            let described = sub.describe();
            let mut sources: Vec<String> = (sub.sources.iter())
                .map(|s| {
                    s.split_once(':')
                        .map_or(s.as_str(), |(name, _)| name)
                        .to_owned()
                })
                .collect();
            sources.dedup();
            // A source it hears that is down fails it: nothing from that
            // source reaches its agent until it works again.
            let (failed, episode) = match sources.iter().find_map(|s| Some((s, down.get(s)?))) {
                Some((source, down)) => (
                    Some(format!(
                        "{source} is down since {}: {}",
                        since_of(down.since_ms),
                        down.why.replacen("exit status: ", "exit status ", 1)
                    )),
                    Some(format!("{source} down since {}", down.since_ms)),
                ),
                None => (None, None),
            };
            let what = described
                .strip_prefix(&format!("{} · ", sub.name))
                .unwrap_or(&described)
                .to_owned();
            items.push(Item {
                kind: Kind::Subscription,
                name: sub.name.clone(),
                bot: subscriber.clone(),
                bots: vec![subscriber.clone()],
                when: String::new(),
                what,
                facts: match own {
                    true => vec!["Sonny's own, for the lead's tasks".into()],
                    false => Vec::new(),
                },
                reply_to: None,
                own,
                failed,
                handle: None,
                fired_ms: None,
                episode,
                sources,
                message: String::new(),
                sub: Some(sub.clone()),
                reminder: None,
                row: Value::Null,
            });
        }
    }
    items
}

/// Remove the rows of triggers made on the daemon whose store is
/// `store_id` that have nothing left to say ([`trigger::forget_among`]), found
/// among the rows as last read: only those are read again. How many went.
pub fn forget_done(store_id: &str, turns: &HashMap<String, Value>) -> usize {
    let Ok(places) = trigger::Places::home() else {
        return 0;
    };
    trigger::forget_among(&places, store_id, turns, &triggers(store_id))
}

/// The rows of the triggers made on the daemon whose store is
/// `store_id`, as [`trigger::on_store`] reads them, read again only once
/// the folders they are in change or a row changes with time.
fn triggers(store_id: &str) -> Vec<(Value, Vec<String>)> {
    static KEPT: kept::Slot<Vec<(Value, Vec<String>)>> = kept::Slot::new(None);
    let Ok(places) = trigger::Places::home() else {
        return Vec::new();
    };
    let paths = vec![places.jobs.clone(), places.state.clone()];
    kept::read(&KEPT, store_id, paths, || {
        trigger::on_store_until(&places, store_id)
    })
}

/// Every subscription beside `store`, as [`lookout::subs::all`] reads
/// them, read again only once their folder changes.
fn subscriptions(store: &Path) -> Vec<(String, Vec<lookout::subs::Sub>)> {
    static KEPT: kept::Slot<Vec<(String, Vec<lookout::subs::Sub>)>> = kept::Slot::new(None);
    let key = store.to_string_lossy();
    kept::read(&KEPT, &key, vec![lookout::subs::dir(store)], || {
        (lookout::subs::all(store), None)
    })
}

/// Each source the lookout beside `store` last told is down, as
/// [`lookout::sources::down`] reads them, read again only once their
/// folder changes.
fn down(store: &Path) -> HashMap<String, lookout::sources::Down> {
    static KEPT: kept::Slot<HashMap<String, lookout::sources::Down>> = kept::Slot::new(None);
    let key = store.to_string_lossy();
    let dir = lookout::beside(store, ".sources").join(".health");
    kept::read(&KEPT, &key, vec![dir], || {
        (lookout::sources::down(store), None)
    })
}

/// The newest word each source `sub` names said that it hears, by the
/// name as written (`ci`, `ci:main`), and when: as
/// [`lookout::sources::status`] reads it, or, for a follow with a filter,
/// the newest its filter takes, read from the log's end. Read again only
/// once one of their logs changes, or the follow does.
fn newest(store: &Path, sub: &lookout::subs::Sub) -> HashMap<String, (String, u128)> {
    static KEPT: kept::Slot<HashMap<String, (String, u128)>> = kept::Slot::new(None);
    let dir = lookout::beside(store, ".sources");
    let named: Vec<(&str, Option<&str>)> = (sub.sources.iter())
        .map(|s| lookout::sources::named(s))
        .collect();
    let paths = (named.iter())
        .map(|(name, _)| lookout::sources::recent_file(&dir, name))
        .collect();
    let filter = sub
        .filter
        .as_ref()
        .map(Value::to_string)
        .unwrap_or_default();
    let key = format!("{}\n{}\n{filter}", store.display(), sub.sources.join("\n"));
    kept::read(&KEPT, &key, paths, || {
        let newest = (sub.sources.iter().zip(&named))
            .filter_map(|(written, (name, topic))| {
                let newest = match sub.filter {
                    // What this row's source and topic say that the
                    // filter takes, as the lookout tells it.
                    Some(_) => {
                        let row = lookout::subs::Sub {
                            sources: vec![written.clone()],
                            ..sub.clone()
                        };
                        heard(store, name, |event| row.hears(event))?
                    }
                    None => {
                        let wanted =
                            |said: Option<&str>| topic.is_none_or(|topic| said == Some(topic));
                        let (said, _) = lookout::sources::status(store, name, wanted);
                        (said.into_iter())
                            .map(|(event, _)| event)
                            .max_by_key(|e| e.at_ms)?
                    }
                };
                Some((written.clone(), (newest.text, newest.at_ms)))
            })
            .collect();
        (newest, None)
    })
}

/// The newest word `source`'s log keeps that `hears` takes, read from the
/// log's end: the lookout's own words, and a topic's clearing, left out.
fn heard(
    store: &Path,
    source: &str,
    hears: impl Fn(&lookout::sources::Event) -> bool,
) -> Option<lookout::sources::Event> {
    let file = lookout::sources::recent_file(&lookout::beside(store, ".sources"), source);
    let text = std::fs::read_to_string(file).ok()?;
    (text.lines().rev())
        .filter_map(|line| serde_json::from_str::<Value>(line).ok())
        .filter(|kept| kept["clear"] != true)
        .filter_map(|kept| lookout::sources::Event::from_json(&kept))
        .find(|event| !event.lookout && hears(event))
}

/// The newest word `source` said on each topic, as
/// [`lookout::sources::status`] reads its log, read again only once that
/// log changes.
fn source_said(store: &Path, source: &str) -> Vec<lookout::sources::Event> {
    static KEPT: kept::Slot<Vec<lookout::sources::Event>> = kept::Slot::new(None);
    let file = lookout::sources::recent_file(&lookout::beside(store, ".sources"), source);
    let key = format!("{}\n{source}", store.display());
    kept::read(&KEPT, &key, vec![file], || {
        let (said, _) = lookout::sources::status(store, source, |_| true);
        (said.into_iter().map(|(event, _)| event).collect(), None)
    })
}

/// Every subscriber's follows, as [`lookout::subs::all`] reads them, read
/// again only once a subscriber's file comes, goes or is moved in anew.
fn every_follow(store: &Path) -> Vec<(String, Vec<lookout::subs::Sub>)> {
    static KEPT: kept::Slot<Vec<(String, Vec<lookout::subs::Sub>)>> = kept::Slot::new(None);
    let key = store.to_string_lossy();
    kept::read(&KEPT, &key, vec![lookout::subs::dir(store)], || {
        (lookout::subs::all(store), None)
    })
}

/// When each automation made on the daemon whose store is `store_id`
/// runs next, in seconds, as [`trigger::upcoming`] reads them: read again
/// once their folder changes or the first of those times comes.
fn next_runs(store_id: &str) -> HashMap<String, i64> {
    static KEPT: kept::Slot<HashMap<String, i64>> = kept::Slot::new(None);
    let Ok(places) = trigger::Places::home() else {
        return HashMap::new();
    };
    kept::read(&KEPT, store_id, vec![places.jobs.clone()], || {
        let now = (now_ms() / 1000) as i64;
        let next: HashMap<String, i64> = (trigger::upcoming(&places, store_id, now).into_iter())
            .filter_map(|run| Some((run.name, run.next?)))
            .collect();
        let until = next.values().min().copied();
        (next, until)
    })
}

/// What `subscriber`'s follows told lately, as the lookout last counted
/// it, read again only once its file changes.
fn counted(store: &Path, subscriber: &str) -> lookout::subs::Counts {
    static KEPT: kept::Slot<lookout::subs::Counts> = kept::Slot::new(None);
    let file = lookout::subs::counts_file(store, subscriber);
    kept::read(&KEPT, &file.to_string_lossy(), vec![file.clone()], || {
        (lookout::subs::counts(store, subscriber), None)
    })
}

/// How many notes each of `subscriber`'s follows that lasts so many has
/// told, as [`lookout::runs_told`] reads it, read again only once its
/// file changes.
fn runs_told(store: &Path, subscriber: &str) -> HashMap<String, usize> {
    static KEPT: kept::Slot<HashMap<String, usize>> = kept::Slot::new(None);
    let file = lookout::held_file(store, subscriber);
    kept::read(&KEPT, &file.to_string_lossy(), vec![file.clone()], || {
        (lookout::runs_told(store, subscriber), None)
    })
}

/// When something past began, `ms` since the epoch: the time today, else
/// its date.
fn since_of(ms: u128) -> String {
    let at = (ms / 1000) as i64;
    let now = (now_ms() / 1000) as i64;
    let (day, today) = (trigger::local(at), trigger::local(now));
    match (day.year, day.month, day.day) == (today.year, today.month, today.day) {
        true => format!("{:02}:{:02}", day.hour, day.minute),
        false => trigger::date(at),
    }
}

/// The person's own agents, which no project names, as the one the nightly
/// memory cleanup starts: what fails for them is the person's to know in
/// every window.
pub fn personal(bot: &str) -> bool {
    !bot.contains('.')
}

/// Whether a window of `scope` shows `item`: one of its agents is the
/// scope's, or it failed and is the person's own.
pub fn shown(scope: &Scope, item: &Item) -> bool {
    item.bots.iter().any(|bot| scope.takes(bot))
        || (item.failed.is_some() && item.bots.iter().all(|bot| personal(bot)))
}

/// What a window of `scope` lists: its triggers and subscriptions, the
/// person's own that failed, and the servers the nightly memory cleanup
/// could not read, for the person and the project.
pub fn read_scope(
    store: &Path,
    store_id: Option<&str>,
    turns: &HashMap<String, Value>,
    scope: &Scope,
) -> Vec<Item> {
    let mut items = read_around(store, store_id, turns, scope);
    items.retain(|item| shown(scope, item));
    items
}

/// [`read_scope`] before what it does not show is left out: the person's
/// own triggers too, whose turns must be asked about before it is known
/// whether they failed.
pub fn read_around(
    store: &Path,
    store_id: Option<&str>,
    turns: &HashMap<String, Value>,
    scope: &Scope,
) -> Vec<Item> {
    let mut items = read(store, store_id, turns, |bot| {
        scope.takes(bot) || personal(bot)
    });
    if let Some(home) = std::env::var_os("HOME").map(PathBuf::from) {
        let (view, cleanup) = memory(&home, &scope.project);
        items.extend(servers(&view));
        items.extend(unscheduled(cleanup));
    }
    items
}

/// The memory's facts the person's and `project`'s, and why the nightly
/// cleanup could not be scheduled, read again only once their folders or
/// that file change.
fn memory(home: &Path, project: &str) -> (Value, Option<(String, i64)>) {
    static KEPT: kept::Slot<(Value, Option<(String, i64)>)> = kept::Slot::new(None);
    let root = home.join(".agents/memory");
    let paths = vec![
        root.clone(),
        root.join("projects").join(project),
        trigger::cleanup_unscheduled_path(home),
    ];
    let key = format!("{}\0{project}", home.display());
    kept::read(&KEPT, &key, paths, || {
        let view = agent_client::memory::view(&root, Some(project));
        ((view, trigger::cleanup_unscheduled(home)), None)
    })
}

/// The nightly memory cleanup, when Sonny could not schedule it: the
/// person's to know in every window, since nothing else runs it.
fn unscheduled(cleanup: Option<(String, i64)>) -> Option<Item> {
    let (why, at) = cleanup?;
    let tried = (now_ms() / 1000).saturating_sub(at.max(0) as u64);
    Some(Item {
        kind: Kind::Setup,
        name: "memory-cleanup".into(),
        bot: String::new(),
        bots: Vec::new(),
        when: String::new(),
        what: String::new(),
        facts: vec![
            format!("tried {}", ago(tried)),
            "Sonny tries again at its next start".into(),
        ],
        reply_to: None,
        own: false,
        failed: Some(first_line(&why)),
        episode: None,
        sources: Vec::new(),
        handle: None,
        fired_ms: None,
        message: String::new(),
        sub: None,
        reminder: None,
        row: Value::Null,
    })
}

/// The `unreadable-SERVER` facts the nightly memory cleanup saves, the
/// person's and the project's, as `view` has them: each as the server it
/// names.
fn servers(view: &Value) -> Vec<Item> {
    let scopes =
        std::iter::once(&view["user"]).chain(view["projects"].as_array().into_iter().flatten());
    let mut items = Vec::new();
    for scope in scopes {
        for fact in scope["facts"].as_array().into_iter().flatten() {
            let name = fact["name"].as_str().unwrap_or_default();
            let Some(server) = name.strip_prefix("unreadable-").filter(|s| !s.is_empty()) else {
                continue;
            };
            let why = fact["description"].as_str().unwrap_or_default();
            let mut facts = vec![format!("memory fact {name}")];
            if let Some(project) = scope["name"].as_str() {
                facts[0].push_str(&format!(" in {project}"));
            }
            if let Some(at) = fact["modified"].as_u64().filter(|at| *at > 0) {
                facts.push(format!("saved {}", ago(now_ms().saturating_sub(at) / 1000)));
            }
            facts.push("read again each night".into());
            items.push(Item {
                kind: Kind::Server,
                name: server.to_owned(),
                bot: String::new(),
                bots: Vec::new(),
                when: String::new(),
                what: String::new(),
                facts,
                reply_to: None,
                own: false,
                failed: Some(match why.is_empty() {
                    true => "could not be read".into(),
                    false => first_line(why),
                }),
                handle: None,
                fired_ms: None,
                episode: None,
                sources: Vec::new(),
                message: String::new(),
                sub: None,
                reminder: None,
                row: Value::Null,
            });
        }
    }
    items
}

/// Each failure among `items`, as a key that stays the same while the
/// failure does: what failed, the fire that did (when it fired, the turn
/// it sent), and why. A later fire that fails again is a failure of its
/// own, also one that never sent a turn.
pub fn failures(items: &[Item]) -> Vec<String> {
    (items.iter())
        .filter_map(|item| {
            let why = item.failed.as_ref()?;
            let why = item.episode.as_ref().unwrap_or(why);
            let turn = item.handle.as_deref().unwrap_or("");
            let at = item.fired_ms.unwrap_or(0);
            Some(format!("{:?}/{}/{at}/{turn}/{why}", item.kind, item.name))
        })
        .collect()
}

/// The turns `items`' last fires sent whose end `turns` does not have yet.
pub fn unended(items: &[Item], turns: &HashMap<String, Value>) -> Vec<String> {
    (items.iter())
        .filter_map(|item| item.handle.clone())
        .filter(|handle| !turns.contains_key(handle))
        .collect()
}

fn trigger_item(row: &Value, bots: Vec<String>) -> Item {
    let text = |key: &str| row[key].as_str().unwrap_or_default().to_owned();
    let mut facts = Vec::new();
    let sent = row["sent"].as_u64().unwrap_or(0);
    let last = &row["last"];
    // What its `--if` said, while it has one: one removed since says
    // nothing about it now.
    let checked = match row["if"].is_string() {
        true => &row["checked"],
        false => &Value::Null,
    };
    let mut failed = None;
    let mut episode = None;
    // A last fire that failed because its `--if` could not answer is over
    // once a later check answers: the check, not that fire, is what holds.
    let gate_broke = last["gate"] == "failed";
    let mut fired_ms = last["fired_ms"].as_u64();
    facts.push(match row["runs"].as_u64() {
        Some(runs) => format!("sent {sent} of {runs}"),
        None if sent == 0 && last.is_null() => "not run yet".into(),
        None => format!("sent {sent}"),
    });
    if let Some(until) = row["until"].as_str() {
        facts.push(format!("until {until}"));
    }
    if let Some(outcome) = last["outcome"].as_str().filter(|_| !gate_broke) {
        let ago = last["fired_ms"]
            .as_u64()
            .map(|at| format!(" {}", ago(now_ms().saturating_sub(at) / 1000)))
            .unwrap_or_default();
        let detail = match last["detail"].as_str() {
            Some(detail) if outcome == "failed" => {
                failed = Some(detail_line(detail));
                String::new()
            }
            Some(detail) if outcome != "sent" => format!(": {}", detail_line(detail)),
            _ => String::new(),
        };
        facts.push(format!("last {outcome}{ago}{detail}"));
        match last["turn_status"].as_str() {
            Some("completed") | None => {}
            // A turn the person stopped, or steered into another, ended as
            // they asked.
            Some(status) if status != "failed" => facts.push(format!("its turn {status}")),
            Some(_) => {
                facts.push("its turn failed".into());
                failed = Some(match last["turn_error"].as_str() {
                    Some(error) if !error.is_empty() => detail_line(error),
                    _ => "its turn failed".into(),
                });
            }
        }
        if outcome == "failed" && failed.is_none() {
            failed = Some("failed".into());
        }
        // Its turn went well and its answer did not reach `--reply-to`.
        if last["reply"]["outcome"] == "failed" && failed.is_none() {
            facts.push("its answer was not passed on".into());
            failed = Some(match last["reply"]["detail"].as_str() {
                Some(detail) if !detail.is_empty() => detail_line(detail),
                _ => "its answer was not passed on".into(),
            });
        }
    }
    // What its `--if` said last: checked and nothing new is a healthy
    // quiet; could not check is a failure until a check answers again.
    let checked_ago = |key: &str| {
        checked[key]
            .as_u64()
            .map(|at| format!(" {}", ago(now_ms().saturating_sub(at) / 1000)))
            .unwrap_or_default()
    };
    match checked["gate"].as_str() {
        Some("no") => facts.push(format!("checked{}, nothing new", checked_ago("checked_ms"))),
        Some("failed") => {
            facts.push(format!("could not check since{}", checked_ago("since_ms")));
            failed = Some(match checked["detail"].as_str() {
                Some(detail) if !detail.is_empty() => detail_line(detail),
                _ => "could not check".into(),
            });
            // One break, however many checks it failed and whatever each
            // printed.
            fired_ms = checked["since_ms"].as_u64().or(fired_ms);
            episode = Some("could not check".into());
        }
        _ => {}
    }
    // An `--if` its next fire could not run, said before that fire finds
    // it: a failure until it can, the same one whatever each check said.
    if let Some(why) = row["if_cannot_run"].as_str() {
        facts.push("its --if cannot run".into());
        failed = Some(first_line(why));
        episode = Some("its --if cannot run".into());
    }
    if row["missed"] == true {
        facts.push("missed its time".into());
    }
    if row["ended"] == true {
        facts.push("ended".into());
    }
    if let Some(gate) = row["if"].as_str() {
        facts.push(format!("if {}", first_line(gate)));
    }
    Item {
        kind: Kind::Trigger,
        name: text("name"),
        bot: text("bot"),
        bots,
        when: text("when"),
        what: first_line(&text("message")),
        facts,
        reply_to: row["reply_to"].as_str().map(str::to_owned),
        own: false,
        failed,
        handle: trigger::sent(row),
        fired_ms,
        episode,
        sources: Vec::new(),
        message: text("message"),
        sub: None,
        reminder: crate::asks::reminder(&text("if"), &text("when"), &text("message")),
        row: row.clone(),
    }
}

/// Each source's icon, as `[sonny.sources.NAME] icon` gives it, the
/// commons' too: the settings read again only once they change, so every
/// surface that names a source can mark it as `/status` does.
pub fn source_icons() -> HashMap<String, String> {
    static KEPT: kept::Slot<HashMap<String, String>> = std::sync::Mutex::new(None);
    let load = || {
        let icons = (agent_client::config::load().map(|config| {
            (config.sonny.sources.into_iter())
                .chain(config.sonny.own_sources)
                .filter_map(|source| Some((source.name, source.icon?)))
                .collect()
        }))
        .unwrap_or_default();
        (icons, None)
    };
    match agent_client::config::path() {
        Some(path) => kept::read(&KEPT, "icons", vec![path], load),
        None => load().0,
    }
}

/// The icons a follow is shown by, each once: its sources' when every
/// one has an icon, else none.
fn icons_of(item: &Item, icons: &HashMap<String, String>) -> Option<String> {
    if item.kind != Kind::Subscription || item.sources.is_empty() {
        return None;
    }
    let mut said = String::new();
    for source in &item.sources {
        let icon = icons.get(source)?;
        if !said.contains(icon.as_str()) {
            said.push_str(icon);
        }
    }
    Some(said)
}

/// What `/status` says of `bot`'s: `↻ 2 automations, 1 follow`, or
/// nothing.
pub fn fact(items: &[Item], bot: &str, icons: &HashMap<String, String>) -> String {
    // A follow whose every source has its icon is shown by those.
    let shown = |i: &Item| icons_of(i, icons).is_some();
    let of = |kind: Kind| {
        items
            .iter()
            .filter(|i| i.kind == kind && !i.own && i.bots.iter().any(|b| b == bot))
            .filter(|i| !shown(i))
            .count()
    };
    let said: Vec<String> = [
        (of(Kind::Trigger), "automation"),
        (of(Kind::Subscription), "follow"),
    ]
    .into_iter()
    .filter(|(n, _)| *n > 0)
    .map(|(n, what)| format!("{n} {what}{}", if n == 1 { "" } else { "s" }))
    .collect();
    match said.is_empty() {
        true => String::new(),
        false => format!("↻ {}", said.join(", ")),
    }
}

/// The icons of the sources `bot` follows, as `[sonny.sources.NAME]
/// icon` gives them, each once, in the order of its follows.
pub fn icons(items: &[Item], bot: &str, icons: &HashMap<String, String>) -> String {
    let mut said = String::new();
    let theirs = (items.iter())
        .filter(|i| i.kind == Kind::Subscription && !i.own && i.bots.iter().any(|b| b == bot));
    for source in theirs.flat_map(|i| &i.sources) {
        if let Some(icon) = icons.get(source)
            && !said.contains(icon.as_str())
        {
            said.push_str(icon);
        }
    }
    said
}

/// The agents a window's `/background` covers: the project's, as
/// `/status` lists them, by a glob.
pub struct Scope {
    pub project: String,
    pub lead: String,
    pub glob: String,
}

impl Scope {
    fn takes(&self, bot: &str) -> bool {
        lookout::subs::matches(&self.glob, bot)
    }
    /// A bot's name in the project: `lead`, or the rest after `project.`.
    pub fn short(&self, bot: &str) -> String {
        if bot == self.lead {
            return "lead".into();
        }
        bot.strip_prefix(&format!("{}.", self.project))
            .unwrap_or(bot)
            .to_owned()
    }
}

/// `/background`, open: what it lists, the row picked, and a removal
/// waiting for its second `d`.
pub struct View {
    scope: Scope,
    store: PathBuf,
    store_id: Option<String>,
    /// The window's own agent, whose lookout line heads the card.
    bot: String,
    items: Vec<Item>,
    /// How the turns triggers' last fires sent ended, as the window last
    /// asked the daemon.
    turns: HashMap<String, Value>,
    lookout: String,
    /// Each source's icon, which marks the follows of it.
    icons: HashMap<String, String>,
    selected: usize,
    /// The item the first `d` picked, by kind, agent and name.
    confirm: Option<(Kind, String, String)>,
    /// The commands the agents left running, picked after the items.
    commands: Commands,
    /// The stop `x` asked of a command, by handle and what it names,
    /// waiting for Enter.
    stopping: Option<(String, String)>,
    notice: Option<String>,
    /// The picked row's thing open as a page over the list: an
    /// automation, a follow or a command.
    page: Option<Page>,
    /// The tasks as `/status` last read them, whose agents a follow's
    /// page lists.
    tasks: Arc<Vec<Value>>,
    /// What the open follow's page says beyond its settings.
    follow: Follow,
    /// When the open automation's page says it runs next, in seconds.
    next_run: Option<i64>,
    /// What the open source's page says.
    source: SourcePage,
    /// The open command's newest output, read only while its page is.
    output: Output,
}

/// A command's newest output as the daemon last said it, shared with
/// the read that goes out for it beside the window.
#[derive(Clone, Debug, Default)]
pub struct Output(Arc<Mutex<OutputRead>>);

#[derive(Debug, Default)]
struct OutputRead {
    /// A read is out: no other goes until it comes back.
    asking: bool,
    /// The command it was read for, by its handle.
    handle: String,
    /// What the daemon said (its `folder` and `lines`), none where it
    /// could not say.
    said: Option<Value>,
}

impl Output {
    /// What the read for `handle` came back with.
    pub fn came(&self, handle: &str, said: Option<Value>) {
        let mut read = self.0.lock().unwrap_or_else(|e| e.into_inner());
        read.asking = false;
        read.handle = handle.to_owned();
        read.said = said;
    }

    /// What was last read for `handle`.
    fn of(&self, handle: &str) -> Option<Value> {
        let read = self.0.lock().unwrap_or_else(|e| e.into_inner());
        (read.handle == handle).then(|| read.said.clone()).flatten()
    }
}

/// The newest lines a command's page shows, at most.
const OUTPUT_LINES: usize = 10;

/// A running command's fields on its page, as the daemon said them: the
/// folder it runs in, and its newest lines.
fn output_fields(said: &Value) -> Vec<(&'static str, Vec<Field>)> {
    let mut fields = Vec::new();
    if let Some(folder) = said["folder"].as_str() {
        fields.push(("folder", vec![Field::text(home_path(folder))]));
    }
    let dim = |words: &str| Field {
        spans: vec![Span::raw(words.to_owned()).dim()],
        link: None,
    };
    let output = match said["lines"].as_array() {
        None => vec![dim("not kept")],
        Some(lines) if lines.is_empty() => vec![dim("nothing yet")],
        Some(lines) => (lines.iter())
            .filter_map(Value::as_str)
            .map(|line| Field::text(line.to_owned()))
            .collect(),
    };
    fields.push(("output", output));
    fields
}

/// What a source's page says, read only while it is open, as often as
/// the list is.
#[derive(Clone, Debug, Default)]
struct SourcePage {
    /// As the settings have it: none for one not set up.
    set: Option<agent_client::config::Source>,
    /// Sonny's own, which has no command.
    own: bool,
    down: Option<lookout::sources::Down>,
    /// The newest word on each topic it named, newest first.
    said: Vec<lookout::sources::Event>,
    /// Each follow of it, by its name, and whose.
    followed: Vec<(String, String)>,
}

/// What a follow's page says beyond how it was set, read only while it
/// is open, as often as the list is.
#[derive(Clone, Debug, Default)]
struct Follow {
    /// Each source it names, as written: the newest word it said on what
    /// the follow hears, and when, in ms.
    newest: HashMap<String, (String, u128)>,
    /// Each source the lookout last told is down.
    down: HashMap<String, lookout::sources::Down>,
    /// What it told in the last day.
    lately: Option<lookout::subs::Lately>,
    /// How many notes it told of the ones it lasts.
    runs_told: usize,
}

/// A thing open as a page: the agents it names, linked as `/memory`'s
/// facts link, and the one picked.
#[derive(Default)]
struct Page {
    /// The command the page shows, by its handle: the pick follows it as
    /// the commands are read again.
    handle: Option<String>,
    /// What the command's page names it by, when it ends.
    what: String,
    pick: Option<usize>,
    /// Where each link was last drawn: its row, its columns on that row,
    /// and where it leads.
    links: Vec<(usize, u16, u16, Target)>,
    /// Whether a follow's done agents are listed, not folded to a count.
    done_open: bool,
    /// The source whose page is open over the follow's, by its name, and
    /// the link picked on the follow's page to go back to.
    source: Option<(String, Option<usize>)>,
}

/// One row of a field on a page: its words, and the one that is a link,
/// by its place among them, with where it leads.
struct Field {
    spans: Vec<Span<'static>>,
    link: Option<(usize, Target)>,
}

impl Field {
    fn text(text: String) -> Field {
        Field {
            spans: vec![Span::raw(text)],
            link: None,
        }
    }

    /// A row of words alone, wider than `room`, as rows that fit, broken
    /// after a space or a `/`, so a folder breaks between its parts.
    fn broken(self, room: usize) -> Vec<Field> {
        let [span] = &self.spans[..] else {
            return vec![self];
        };
        if self.link.is_some() || span.width() <= room || room == 0 {
            return vec![self];
        }
        let style = span.style;
        let mut rows: Vec<String> = Vec::new();
        let mut row = String::new();
        // A part a ` · ` apart stays whole where it fits a row.
        let mut words: Vec<String> = Vec::new();
        for part in span.content.split_inclusive(" · ") {
            if part.trim_end().width() <= room {
                words.push(part.to_owned());
                continue;
            }
            let mut word = String::new();
            for c in part.chars() {
                word.push(c);
                if c == ' ' || c == '/' {
                    words.push(std::mem::take(&mut word));
                }
            }
            words.push(word);
        }
        // A row broken at a ` · ` leaves the dot: no row ends in one.
        let ended = |row: &str| -> String {
            let row = row.trim_end();
            row.strip_suffix('·').unwrap_or(row).trim_end().to_owned()
        };
        for word in words.into_iter().filter(|w| !w.is_empty()) {
            if !row.is_empty() && row.width() + ended(&word).width() > room {
                rows.push(ended(&std::mem::take(&mut row)));
            }
            row.push_str(&word);
        }
        rows.push(ended(&row));
        (rows.into_iter())
            .map(|text| Field {
                spans: vec![Span::styled(text, style)],
                link: None,
            })
            .collect()
    }
}

/// `text` broken after its words into rows `room` wide, two at most, the
/// second cut short with an ellipsis where more is left.
fn two_rows(text: &str, room: usize) -> Vec<String> {
    let room = room.max(8);
    let mut rows = vec![String::new()];
    for word in text.split(' ').filter(|w| !w.is_empty()) {
        let full = rows.len() == 2;
        let row = rows.last_mut().expect("a row");
        let wide = row.width() + usize::from(!row.is_empty()) + word.width();
        if row.is_empty() || wide <= room {
            if !row.is_empty() {
                row.push(' ');
            }
            row.push_str(word);
            continue;
        }
        // Past the second row: it is cut where the row ends.
        if full {
            *row = crate::transcript::cut(&format!("{row} {word}"), room);
            return rows;
        }
        rows.push(word.to_owned());
    }
    // A single word wider than a row is cut too.
    for row in &mut rows {
        if row.width() > room {
            *row = crate::transcript::cut(row, room);
        }
    }
    rows
}

/// `text` with every control character a space, for one row.
fn plain(text: &str) -> String {
    (text.chars())
        .map(|c| if c.is_control() { ' ' } else { c })
        .collect()
}

/// `fields` laid under their labels into `body`: a value too wide broken
/// after a space or a `/`, a row with a link kept one row, cut to fit, so
/// the link is where it is drawn; each link's row in `body`, its columns
/// and where it leads in `links`, the one at `pick` among them reversed.
/// How wide the labels are, with the space after them.
fn lay(
    fields: Vec<(&str, Vec<Field>)>,
    width: usize,
    pick: Option<usize>,
    body: &mut Vec<Line<'static>>,
    links: &mut Vec<(usize, u16, u16, Target)>,
) -> usize {
    let label = fields.iter().map(|(l, _)| l.width()).max().unwrap_or(0) + 2;
    let hang = " ".repeat(label);
    for (name, rows) in fields {
        let rows = (rows.into_iter()).flat_map(|row| row.broken(width.saturating_sub(label)));
        for (n, Field { mut spans, link }) in rows.enumerate() {
            let lead = match n {
                0 => Span::raw(format!("{name:<label$}")).dim(),
                _ => Span::raw(hang.clone()),
            };
            let Some((at, target)) = link else {
                body.extend(crate::transcript::wrap(
                    Line::from([vec![lead], spans].concat()),
                    width,
                    Span::raw(""),
                    Span::raw(hang.clone()),
                ));
                continue;
            };
            spans = fit(spans, width.saturating_sub(label));
            if let Some(linked) = spans.get(at).cloned() {
                let start = label + (spans[..at].iter()).map(Span::width).sum::<usize>();
                let end = start + linked.width();
                if pick == Some(links.len()) {
                    spans[at] = linked.add_modifier(ratatui::style::Modifier::REVERSED);
                }
                links.push((body.len(), start as u16, end as u16, target));
            }
            body.push(Line::from([vec![lead], spans].concat()));
        }
    }
    label
}

/// `spans` cut to `room` columns: the span that crosses it cut short with
/// an ellipsis, the rest left out.
fn fit(spans: Vec<Span<'static>>, room: usize) -> Vec<Span<'static>> {
    let mut out = Vec::new();
    let mut used = 0;
    for span in spans {
        let wide = span.width();
        if used + wide <= room {
            used += wide;
            out.push(span);
            continue;
        }
        if room > used + 1 {
            let cut = crate::transcript::cut(&span.content, room - used);
            out.push(Span::styled(cut, span.style));
        }
        break;
    }
    out
}

/// How wide a follow's labels are, with the space after them: its
/// longest, `watches` and `sources`.
const FOLLOW_LABEL: usize = 9;

/// How wide a source's labels are, with the space after them: its
/// longest, `followed`.
const SOURCE_LABEL: usize = 10;
/// The most topics a source's page lists, and how wide a topic is at
/// most there.
const SAID_TOPICS: usize = 20;
const TOPIC_WIDTH: usize = 24;

/// Where a topic of Sonny's own sources leads: the agent the tasks
/// source names, the commons thread, the fact still in memory.
fn topic_target(source: &str, event: &lookout::sources::Event) -> Option<Target> {
    let topic = event.topic.clone().filter(|topic| !topic.is_empty())?;
    match source {
        agent_client::config::TASKS_SOURCE => Some(Target::Agent(topic)),
        agent_client::config::COMMONS_SOURCE => Some(Target::Thread(topic)),
        agent_client::config::MEMORY_SOURCE if event.from.as_deref() != Some("removed") => {
            let project = event.fields["project"].as_str().map(str::to_owned);
            Some(Target::Fact(project, topic))
        }
        _ => None,
    }
}

fn plural(n: u64) -> &'static str {
    if n == 1 { "" } else { "s" }
}

/// Where a page's link leads.
#[derive(Clone, Debug, PartialEq)]
enum Target {
    /// The agent, by its name, opened out of the view.
    Agent(String),
    /// A follow's done agents, folded: the link lists them.
    Done,
    /// A source a follow hears: its page opens over the follow's.
    Source(String),
    /// A fact in memory, by its project (none for the person's) and its
    /// name: opened in `/memory`.
    Fact(Option<String>, String),
    /// A commons thread, by its name: opened in `/commons`.
    Thread(String),
}

/// The view's link to its clicks: a button's address is this, then what
/// was clicked.
pub const BUTTON: &str = "background-page/";

/// What a key did to the view.
#[derive(Debug, PartialEq)]
pub enum Key {
    Stay,
    Close,
    /// A link followed out of the view: an agent a page names.
    Open(String),
    /// Scroll the page one row: up (true) or down.
    Scroll(bool),
    /// Stop this agent's command, by its handle.
    Stop {
        bot: String,
        handle: String,
    },
}

impl View {
    pub fn open(
        scope: Scope,
        store: PathBuf,
        store_id: Option<String>,
        bot: String,
        turns: HashMap<String, Value>,
    ) -> Self {
        let mut view = Self {
            scope,
            store,
            store_id,
            bot,
            items: Vec::new(),
            turns,
            lookout: String::new(),
            icons: HashMap::new(),
            selected: 0,
            confirm: None,
            commands: Commands::default(),
            stopping: None,
            notice: None,
            page: None,
            tasks: Arc::default(),
            follow: Follow::default(),
            next_run: None,
            source: SourcePage::default(),
            output: Output::default(),
        };
        view.reload();
        view
    }

    /// Every row that can be picked: the items, then the commands.
    fn rows(&self) -> usize {
        self.items.len() + self.commands.rows.len()
    }

    /// The command picked, when the pick is past the items.
    fn command(&self) -> Option<&Value> {
        (self.selected.checked_sub(self.items.len())).and_then(|at| self.commands.rows.get(at))
    }

    /// Whether the commands should be read again now.
    pub fn commands_due(&self) -> bool {
        self.commands.due()
    }

    /// The `processes` request for the project's commands.
    pub fn commands_query(&self) -> Value {
        json!({"bot": self.scope.glob})
    }

    /// The `process_output` request for the command whose page is open,
    /// and where its answer goes: none while no command's page is open,
    /// for a call waiting on its approval, or while the last is still out.
    pub fn output_query(&self) -> Option<(Value, Output)> {
        let handle = self.page.as_ref()?.handle.clone()?;
        let row = self.command()?;
        if !matches!(
            row["kind"].as_str(),
            Some("background" | "terminal" | "detached")
        ) {
            return None;
        }
        let mut read = self.output.0.lock().unwrap_or_else(|e| e.into_inner());
        if read.asking {
            return None;
        }
        read.asking = true;
        let query = json!({"bot": row["bot"], "handle": handle, "lines": OUTPUT_LINES});
        Some((query, self.output.clone()))
    }

    /// What the read of the commands came back with.
    pub fn commands_loaded(&mut self, page: Result<Value, String>) {
        if let Some(came) = self.commands.loaded(page) {
            self.notice = Some(came);
        }
        // A command's page stays on it, wherever it is now; one that
        // ended closes.
        if let Some(handle) = self.page.as_ref().and_then(|p| p.handle.clone()) {
            match (self.commands.rows.iter()).position(|row| row["handle"] == *handle) {
                Some(at) => self.selected = self.items.len() + at,
                None => {
                    let what = self.page.take().map(|p| p.what).unwrap_or_default();
                    self.notice.get_or_insert_with(|| format!("{what} ended"));
                }
            }
        }
        self.selected = self.selected.min(self.rows().saturating_sub(1));
        // A stop asked of a command that has gone since is no stop.
        if let Some((handle, _)) = &self.stopping
            && !(self.commands.rows.iter()).any(|row| row["handle"] == *handle)
        {
            self.stopping = None;
        }
    }

    /// A stop of `handle` going out: it says so until it comes to
    /// something, which goes in what this returns.
    pub fn stopping(&mut self, handle: &str) -> Arc<Mutex<Option<String>>> {
        self.notice = Some(format!("stopping {handle}…"));
        self.commands.came()
    }

    /// Read it all again, the pick kept on the same row where it still is.
    pub fn reload(&mut self) {
        let picked = self.items.get(self.selected).map(key_of);
        let mut items = read_scope(
            &self.store,
            self.store_id.as_deref(),
            &self.turns,
            &self.scope,
        );
        items.sort_by_key(|item| (item.failed.is_none(), item.kind != Kind::Trigger));
        self.items = items;
        let at = picked
            .clone()
            .and_then(|k| self.items.iter().position(|i| key_of(i) == k));
        // An item's page closes once the item is gone, as one removed is.
        if at.is_none() && self.page.as_ref().is_some_and(|p| p.handle.is_none()) {
            self.page = None;
        }
        self.selected = at
            .unwrap_or(self.selected)
            .min(self.rows().saturating_sub(1));
        // A command's page stays on its command, wherever the items moved
        // it.
        if let Some(handle) = self.page.as_ref().and_then(|p| p.handle.as_deref())
            && let Some(at) = (self.commands.rows.iter()).position(|row| row["handle"] == *handle)
        {
            self.selected = self.items.len() + at;
        }
        // A removal waits only on the row it was asked for, still picked.
        if self.confirm.is_some() && self.items.get(self.selected).map(key_of) != self.confirm {
            self.confirm = None;
        }
        self.lookout =
            crate::tool_host::with_status(lookout::status(&self.store, &self.bot), &self.store);
        self.icons = source_icons();
        self.read_page();
    }

    /// What the open page says beyond its row, read only while it is open:
    /// a follow's agents, words and counts, an automation's next run.
    fn read_page(&mut self) {
        self.follow = self.read_follow();
        self.next_run = self.read_next_run();
        self.source = self.read_source();
    }

    /// The source whose page is open, by its name.
    fn source_open(&self) -> Option<&str> {
        let (name, _) = self.page.as_ref()?.source.as_ref()?;
        Some(name)
    }

    /// What the open source's page says: nothing read while none is.
    fn read_source(&self) -> SourcePage {
        let Some(name) = self.source_open() else {
            return SourcePage::default();
        };
        let set = (lookout::sources::configured()
            .unwrap_or_default()
            .into_iter())
        .find(|source| source.name == name);
        let own = name == agent_client::config::TASKS_SOURCE
            || set.as_ref().is_some_and(|source| source.command.is_empty());
        let hears = |sub: &lookout::subs::Sub| {
            (sub.sources.iter()).any(|s| lookout::sources::named(s).0 == name)
        };
        let followed = (every_follow(&self.store).into_iter())
            .flat_map(|(bot, subs)| {
                (subs.into_iter())
                    .filter(|sub| hears(sub))
                    .map(move |sub| (sub.name, bot.clone()))
            })
            .collect();
        SourcePage {
            set,
            own,
            down: down(&self.store).remove(name),
            said: source_said(&self.store, name),
            followed,
        }
    }

    /// How deep the view is: its list (0), a thing's page (1), or a
    /// source's page over a follow's (2).
    pub fn depth(&self) -> usize {
        match &self.page {
            None => 0,
            Some(page) => 1 + usize::from(page.source.is_some()),
        }
    }

    /// When the automation whose page is open runs next, as its schedule
    /// says: none for one that waits on a file or a commit.
    fn read_next_run(&self) -> Option<i64> {
        self.page.as_ref().filter(|page| page.handle.is_none())?;
        let item = (self.items.get(self.selected)).filter(|item| item.kind == Kind::Trigger)?;
        let next = next_runs(self.store_id.as_deref()?);
        next.get(&item.name).copied()
    }

    /// The tasks as `/status` read them now.
    pub fn set_tasks(&mut self, tasks: &Arc<Vec<Value>>) {
        self.tasks = tasks.clone();
    }

    /// The follow whose page is open, and whom it is for.
    fn follow_open(&self) -> Option<(&Item, &lookout::subs::Sub)> {
        self.page.as_ref().filter(|page| page.handle.is_none())?;
        let item = self.items.get(self.selected)?;
        Some((item, item.sub.as_ref()?))
    }

    /// What the open follow's page says beyond its settings: nothing
    /// read while no follow's page is open.
    fn read_follow(&self) -> Follow {
        let Some((item, sub)) = self.follow_open() else {
            return Follow::default();
        };
        let runs_told = match sub.runs {
            Some(_) => (runs_told(&self.store, &item.bot).get(&sub.counter()))
                .copied()
                .unwrap_or(0),
            None => 0,
        };
        Follow {
            newest: newest(&self.store, sub),
            down: down(&self.store),
            lately: counted(&self.store, &item.bot).lately(sub, now_ms()),
            runs_told,
        }
    }

    /// How the turns ended, as the window has since asked the daemon.
    pub fn set_turns(&mut self, turns: &HashMap<String, Value>) {
        if *turns != self.turns {
            self.turns = turns.clone();
            self.reload();
        }
    }

    /// The item a removal was asked for is kept, or the command a stop
    /// was: a key went elsewhere.
    pub fn keep(&mut self) {
        if self.confirm.take().is_some() {
            self.notice = Some("not removed".into());
        }
        if self.stopping.take().is_some() {
            self.notice = Some("not stopped".into());
        }
    }

    /// A key, with the rows of the card now in view, for `↑` `↓` on a
    /// page to pick among the links there before scrolling.
    pub fn key(&mut self, key: KeyEvent, in_view: std::ops::Range<usize>) -> Key {
        self.notice = None;
        let plain = !key
            .modifiers
            .intersects(KeyModifiers::CONTROL | KeyModifiers::ALT | KeyModifiers::SUPER);
        let close = key.code == KeyCode::Char('c') && key.modifiers.contains(KeyModifiers::CONTROL);
        // A removal asked for: only Enter, with no modifier, says yes;
        // any other key keeps the row, and says so, and Ctrl+C closes too.
        if let Some(asked) = self.confirm.take() {
            match self.items.iter().find(|item| key_of(item) == asked) {
                Some(item) if key.code == KeyCode::Enter && plain => self.remove(&item.clone()),
                _ => self.notice = Some("not removed".into()),
            }
            return if close { Key::Close } else { Key::Stay };
        }
        // A stop asked for: the same, for the command it would stop.
        if let Some((handle, _)) = self.stopping.take() {
            if key.code != KeyCode::Enter || !plain {
                self.notice = Some("not stopped".into());
                return if close { Key::Close } else { Key::Stay };
            }
            let row = self
                .commands
                .rows
                .iter()
                .find(|row| row["handle"] == *handle);
            return match row.and_then(|row| row["bot"].as_str()) {
                Some(bot) => Key::Stop {
                    bot: bot.to_owned(),
                    handle,
                },
                None => Key::Stay,
            };
        }
        let code = match key.code {
            KeyCode::Char('k') if plain => KeyCode::Up,
            KeyCode::Char('j') if plain => KeyCode::Down,
            KeyCode::Char('h') if plain => KeyCode::Left,
            KeyCode::Char('l') if plain => KeyCode::Right,
            code => code,
        };
        match code {
            KeyCode::Esc => return Key::Close,
            KeyCode::Char('c') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                return Key::Close;
            }
            // A letter with Ctrl or Alt is another shortcut, such as Ctrl+F's
            // find: never a run or a removal.
            KeyCode::Char(_)
                if key.modifiers.intersects(
                    KeyModifiers::CONTROL | KeyModifiers::ALT | KeyModifiers::SUPER,
                ) => {}
            // On a page the arrows pick among its links, as on a fact in
            // `/memory`; `←` goes back to the list.
            KeyCode::Up | KeyCode::Down | KeyCode::Right | KeyCode::Enter | KeyCode::Left
                if self.page.is_some() =>
            {
                return self.page_key(code, in_view);
            }
            // `enter` opens the row's thing as a page, as `→` does: an
            // automation, a follow, a command. The agent it belongs to is
            // the page's first link.
            KeyCode::Enter | KeyCode::Right => self.open_page(),
            KeyCode::Up => self.selected = self.selected.saturating_sub(1),
            KeyCode::Down => {
                self.selected = (self.selected + 1).min(self.rows().saturating_sub(1));
            }
            // A source is the person's to remove, where it is set; and it
            // runs on its own.
            KeyCode::Char('x') | KeyCode::Delete if self.source_open().is_some() => {
                self.notice = Some(match self.source.own {
                    true => "Sonny keeps its own sources".into(),
                    false => "a source is yours to remove, where it is set".into(),
                });
            }
            KeyCode::Char('r') if self.source_open().is_some() => {}
            KeyCode::Char('r') if self.picked_reminder() => {
                self.notice = Some("a question's reminder runs at its time".into());
            }
            KeyCode::Char('r') => self.fire(),
            KeyCode::Char('x') | KeyCode::Delete if self.command().is_some() => {
                let row = self.command().cloned().unwrap_or_default();
                let handle = row["handle"].as_str().unwrap_or_default().to_owned();
                self.stopping = Some((handle, commands::what(&row)));
            }
            KeyCode::Char('x') | KeyCode::Delete => {
                let Some(item) = self.items.get(self.selected).cloned() else {
                    return Key::Stay;
                };
                if item.reminder.is_some() {
                    self.notice = Some("it goes once the question is answered".into());
                } else if item.own {
                    self.notice = Some(
                        "Sonny keeps the lead's tasks follow; the lookout watches its tasks with it"
                            .into(),
                    );
                } else if item.kind == Kind::Server {
                    self.notice = Some(
                        "the nightly cleanup removes it the night the server reads again".into(),
                    );
                } else if item.kind == Kind::Setup {
                    self.notice = Some(
                        "it goes once the cleanup is scheduled: `~/.agent/memory schedule --model PROVIDER/MODEL`"
                            .into(),
                    );
                } else {
                    self.confirm = Some(key_of(&item));
                }
            }
            _ => {}
        }
        Key::Stay
    }

    /// Whether the picked row is a question's reminder.
    fn picked_reminder(&self) -> bool {
        self.command().is_none()
            && (self.items.get(self.selected)).is_some_and(|item| item.reminder.is_some())
    }

    /// Whether the picked row is a thing with a page: an automation, a
    /// follow or a command.
    fn has_page(&self) -> bool {
        match self.items.get(self.selected) {
            Some(item) => matches!(item.kind, Kind::Trigger | Kind::Subscription),
            None => self.command().is_some(),
        }
    }

    /// The picked row's thing opened as a page, its agent's link picked.
    fn open_page(&mut self) {
        if !self.has_page() {
            return;
        }
        let command = self.command();
        self.page = Some(Page {
            handle: command.and_then(|row| row["handle"].as_str().map(str::to_owned)),
            what: command.map(commands::what).unwrap_or_default(),
            pick: Some(0),
            links: Vec::new(),
            done_open: false,
            source: None,
        });
        self.read_page();
    }

    /// Whether a thing is open as a page.
    pub fn on_page(&self) -> bool {
        self.page.is_some()
    }

    /// Whether a link is picked on the open page: none once the arrows
    /// scrolled it out of view.
    pub fn link_picked(&self) -> bool {
        self.page.as_ref().is_some_and(|page| page.pick.is_some())
    }

    /// Back to the list from a page, as `←` there and the title's `‹`
    /// go. Whether there was one to leave.
    pub fn back(&mut self) -> bool {
        // From a source's page back to the follow's, its link picked.
        if let Some(page) = self.page.as_mut()
            && let Some((_, pick)) = page.source.take()
        {
            page.pick = pick;
            page.links.clear();
            self.read_page();
            return true;
        }
        self.page.take().is_some()
    }

    /// Where a page's link leads: the address to follow out of the view,
    /// or none for one that leads within it.
    fn go(&mut self, target: Target) -> Option<String> {
        let button = crate::screen::BUTTON;
        match target {
            Target::Agent(bot) => Some(format!("{}{bot}", crate::links::AGENT)),
            Target::Done => {
                if let Some(page) = self.page.as_mut() {
                    page.done_open = true;
                }
                None
            }
            Target::Source(name) => {
                // Another page: its links are its own, the first picked.
                if let Some(page) = self.page.as_mut() {
                    page.source = Some((name, page.pick));
                    page.pick = Some(0);
                    page.links.clear();
                }
                self.read_page();
                None
            }
            Target::Fact(project, name) => Some(format!(
                "{button}fact/{}/{name}",
                project.unwrap_or_default()
            )),
            Target::Thread(name) => Some(format!("{button}thread/{name}")),
        }
    }

    /// A key on a page: the arrows pick among the links in view, as Lynx
    /// does, and scroll where none is left that way; `→` or `enter`
    /// follows the picked link and `←` goes back to the list.
    fn page_key(&mut self, code: KeyCode, in_view: std::ops::Range<usize>) -> Key {
        let Some(page) = self.page.as_mut() else {
            return Key::Stay;
        };
        match code {
            KeyCode::Up | KeyCode::Down => {
                let down = code == KeyCode::Down;
                let shown: Vec<usize> = (0..page.links.len())
                    .filter(|&at| in_view.contains(&page.links[at].0))
                    .collect();
                let next = match (page.pick, down) {
                    (Some(at), true) => shown.iter().copied().find(|&s| s > at),
                    (Some(at), false) => shown.iter().copied().rev().find(|&s| s < at),
                    (None, true) => shown.first().copied(),
                    (None, false) => shown.last().copied(),
                };
                match next {
                    Some(at) => page.pick = Some(at),
                    None => {
                        // A link scrolled out of view is no longer picked.
                        page.pick = page.pick.filter(|&at| shown.contains(&at));
                        return Key::Scroll(!down);
                    }
                }
            }
            // The done agents open in the fold's place, the first of
            // them picked; a source's page over the follow's.
            KeyCode::Right | KeyCode::Enter => {
                let target = (page.pick)
                    .and_then(|at| page.links.get(at))
                    .map(|link| link.3.clone());
                if let Some(url) = target.and_then(|target| self.go(target)) {
                    return Key::Open(url);
                }
            }
            KeyCode::Left => {
                self.back();
            }
            _ => {}
        }
        Key::Stay
    }

    /// A click on one of a page's links, `what` past [`BUTTON`]: the
    /// address to follow out of the view.
    pub fn click(&mut self, what: &str) -> Option<String> {
        self.confirm = None;
        self.stopping = None;
        let at = what.strip_prefix("link/")?.parse::<usize>().ok()?;
        let page = self.page.as_mut()?;
        let target = page.links.get(at)?.3.clone();
        page.pick = Some(at);
        self.go(target)
    }

    fn fire(&mut self) {
        let Some(item) = self.items.get(self.selected) else {
            if self.command().is_some() {
                self.notice = Some("only an automation runs now".into());
            }
            return;
        };
        if item.kind != Kind::Trigger {
            self.notice = Some("only an automation runs now".into());
            return;
        }
        let name = item.name.clone();
        self.notice = Some(
            match trigger::Places::home()
                .and_then(|places| trigger::fire_now(&places, &name, &trigger::ring))
            {
                Ok(_) => format!("ran {name}"),
                Err(error) => error,
            },
        );
        self.reload();
    }

    fn remove(&mut self, item: &Item) {
        let done = match item.kind {
            Kind::Trigger => {
                trigger::Places::home().and_then(|places| trigger::remove(&places, &item.name))
            }
            Kind::Subscription => {
                lookout::subs::remove(&self.store, &item.bot, &item.name).map(drop)
            }
            Kind::Server | Kind::Setup => return,
        };
        self.notice = Some(match done {
            Ok(()) => format!("removed {}", item.name),
            Err(error) => error,
        });
        self.reload();
    }

    /// The card, and the row the picked item starts on.
    #[cfg(test)]
    pub fn card(&self, width: usize) -> (Vec<Line<'static>>, usize) {
        let (out, at, _) = self.card_rows(width);
        (out, at)
    }

    /// The item at `at` picked, as a click picks it.
    pub fn pick_at(&mut self, at: usize) {
        // Any click takes back a removal or a stop that was asked: only
        // Enter removes or stops.
        self.confirm = None;
        self.stopping = None;
        if at < self.rows() {
            self.selected = at;
        }
    }

    /// [`View::card`], and the row each item starts on, by its place
    /// among the items.
    pub fn card_rows(&self, width: usize) -> (Vec<Line<'static>>, usize, Vec<usize>) {
        self.card_at(width, now_ms() as i64)
    }

    /// [`View::card_rows`] as of `now_ms`, which the commands' ages count
    /// to.
    fn card_at(&self, width: usize, now_ms: i64) -> (Vec<Line<'static>>, usize, Vec<usize>) {
        let mut starts = vec![0; self.rows()];
        let mut out = vec![Line::from(self.scope.project.clone()).bold()];
        if !self.lookout.is_empty() {
            out.push(Line::from(self.lookout.clone()).dim());
        }
        let mut at = 0;
        // Said empty once the commands are read too, a moment after it
        // opens.
        if self.rows() == 0 && !self.commands.read() {
            return (out, 0, Vec::new());
        }
        if self.rows() == 0 && self.commands.error.is_none() {
            out.push(Line::default());
            out.push(Line::from(
                "Nothing runs in the background for these agents.",
            ));
            out.push(
                Line::from(
                    "An agent adds an automation with its automations tool, and a follow with its follows tool.",
                )
                .dim(),
            );
            out.push(
                Line::from("A command an agent leaves running shows here until it ends.").dim(),
            );
            return (out, 0, Vec::new());
        }
        // What failed first, whatever it is; then the rest by kind.
        let section = |item: &Item| match (&item.failed, item.kind) {
            (Some(_), _) => "Failed",
            (None, Kind::Subscription) => "Follows",
            (None, _) => "Automations",
        };
        for heading in ["Failed", "Automations", "Follows"] {
            let takes = |item: &Item| section(item) == heading;
            let n = self.items.iter().filter(|i| takes(i)).count();
            if n == 0 {
                continue;
            }
            out.push(Line::default());
            out.push(Line::from(vec![
                Span::raw(heading).bold(),
                Span::raw(format!(" · {n}")).dim(),
            ]));
            for (index, item) in self.items.iter().enumerate() {
                if !takes(item) {
                    continue;
                }
                if index == self.selected {
                    at = out.len();
                }
                starts[index] = out.len();
                out.extend(self.item_lines(item, index == self.selected, width));
            }
        }
        if let Some(error) = &self.commands.error {
            out.push(Line::default());
            out.push(Line::from(vec![
                Span::raw("✗ ").fg(Color::Red),
                Span::raw(format!("could not read what runs: {error}")),
            ]));
        } else if !self.commands.rows.is_empty() {
            out.push(Line::default());
            out.push(Line::from(vec![
                Span::raw("Commands").bold(),
                Span::raw(format!(" · {}", self.commands.rows.len())).dim(),
            ]));
            let names: Vec<String> = (self.commands.rows.iter())
                .map(|row| self.scope.short(row["bot"].as_str().unwrap_or("?")))
                .collect();
            let name_width = names.iter().map(|n| n.width()).max().unwrap_or(0).min(20);
            for (place, (row, name)) in self.commands.rows.iter().zip(&names).enumerate() {
                let index = self.items.len() + place;
                if index == self.selected {
                    at = out.len();
                }
                starts[index] = out.len();
                let picked = index == self.selected;
                out.push(commands::line(row, name, name_width, width, now_ms, picked));
            }
        }
        (out, at, starts)
    }

    /// The page of the picked row's thing at `width`: its name, then the
    /// agent it belongs to as the page's first link, then everything its
    /// row cut short. The row the picked link is on, and the links'
    /// buttons.
    pub fn page_lines(
        &mut self,
        width: usize,
    ) -> (Vec<Line<'static>>, usize, Vec<(usize, crate::screen::Mark)>) {
        // The link picked, by where it leads: rows that reorder on their
        // own, as a follow's agents do, keep it on the same thing.
        let wanted = (self.page.as_ref())
            .and_then(|page| page.links.get(page.pick?))
            .map(|link| link.3.clone());
        let drawn = self.draw_page(width);
        let (Some(wanted), Some(page)) = (wanted, self.page.as_mut()) else {
            return drawn;
        };
        let now = page.pick.and_then(|at| page.links.get(at));
        if now.is_some_and(|link| link.3 == wanted) {
            return drawn;
        }
        // An agent folded into `N done` since: the fold.
        let found = (page.links.iter())
            .position(|link| link.3 == wanted)
            .or_else(|| {
                matches!(wanted, Target::Agent(_))
                    .then(|| page.links.iter().position(|link| link.3 == Target::Done))
                    .flatten()
            });
        match found {
            Some(at) => {
                page.pick = Some(at);
                self.draw_page(width)
            }
            None => drawn,
        }
    }

    /// [`View::page_lines`] with the pick where it is.
    fn draw_page(
        &mut self,
        width: usize,
    ) -> (Vec<Line<'static>>, usize, Vec<(usize, crate::screen::Mark)>) {
        let mut out = Vec::new();
        // What the line under the name says, in parts a ` · ` apart: an
        // agent in a part is a link to it.
        let mut about: Vec<Vec<(String, Option<String>)>> = Vec::new();
        let mut body = Vec::new();
        // Each link in the body: its row there, its columns, where it
        // leads.
        let mut body_links: Vec<(usize, u16, u16, Target)> = Vec::new();
        let short = |bot: &str| self.scope.short(bot);
        if let Some(name) = self.source_open().map(str::to_owned) {
            // A source's page: how it runs, whether it works and why not,
            // what it said lately, who follows it, and where it is set.
            out.push(Line::from(name.clone()).bold());
            let page = &self.source;
            about.push(vec![("source".into(), None)]);
            match &page.set {
                _ if page.own => about.push(vec![("Sonny's own".into(), None)]),
                Some(set) => {
                    let every = set.every.as_deref().unwrap_or("1m");
                    about.push(vec![(format!("runs every {every}"), None)]);
                }
                None => about.push(vec![("not set up".into(), None)]),
            }
            if let Some(digest) = page.set.as_ref().and_then(|set| set.digest.as_deref()) {
                about.push(vec![(format!("tells at most every {digest}"), None)]);
            }
            if let Some(down) = &page.down {
                let mut why = format!("down since {}", since_of(down.since_ms));
                if !down.why.is_empty() {
                    let said = down.why.replacen("exit status: ", "exit status ", 1);
                    why.push_str(&format!(": {said}"));
                }
                body.extend(crate::transcript::wrap(
                    Line::from(vec![Span::raw("✗ ").fg(Color::Red), Span::raw(why)]),
                    width,
                    Span::raw(""),
                    Span::raw("  "),
                ));
                for line in down.stderr.iter().rev().take(5).rev() {
                    body.extend(crate::transcript::wrap(
                        Line::from(format!("  {}", plain(line))),
                        width,
                        Span::raw(""),
                        Span::raw("  "),
                    ));
                }
                body.push(Line::default());
            }
            let fields = self.source_fields(&name, width);
            let pick = self.page.as_ref().and_then(|page| page.pick);
            lay(fields, width, pick, &mut body, &mut body_links);
            if !self.source.own
                && self.source.set.is_some()
                && let Some(path) = agent_client::config::path()
            {
                body.extend(crate::transcript::wrap(
                    Line::from(format!("set in {}", home_path(&path.to_string_lossy()))).dim(),
                    width,
                    Span::raw(""),
                    Span::raw(""),
                ));
            }
        } else if let Some(row) = self.command().cloned() {
            let bot = row["bot"].as_str().unwrap_or_default().to_owned();
            let command: String = (row["command"].as_str().unwrap_or_default().chars())
                .map(|c| if c.is_control() { ' ' } else { c })
                .collect();
            let command = match command.trim() {
                "" => row["handle"].as_str().unwrap_or_default().to_owned(),
                command => command.to_owned(),
            };
            // Its whole line is its name.
            out.extend(crate::transcript::wrap(
                Line::from(command).bold(),
                width,
                Span::raw(""),
                Span::raw(""),
            ));
            let kind = match row["kind"].as_str().unwrap_or("call") {
                "background" => "background command",
                "terminal" => "terminal",
                "detached" => "detached service",
                _ => "call waiting on its approval",
            };
            about.push(vec![(kind.into(), None)]);
            about.push(vec![
                ("left running by ".into(), None),
                (short(&bot), Some(bot.clone())),
            ]);
            if let Some(started) = row["started_ms"].as_i64() {
                let ran = (now_ms() as i64 - started).max(0) as u64 / 1000;
                about.push(vec![(
                    format!("for {}", ago(ran).trim_end_matches(" ago")),
                    None,
                )]);
            }
            if let Some(pid) = row["pid"].as_i64() {
                about.push(vec![(format!("pid {pid}"), None)]);
            }
            let handle = row["handle"].as_str().unwrap_or_default();
            if let Some(said) = self.output.of(handle) {
                let pick = self.page.as_ref().and_then(|page| page.pick);
                lay(
                    output_fields(&said),
                    width,
                    pick,
                    &mut body,
                    &mut body_links,
                );
            }
        } else if let Some((item, reminder)) = (self.items.get(self.selected))
            .and_then(|item| Some((item.clone(), item.reminder.clone()?)))
        {
            // A question's reminder: whose question, when, what happens
            // then, and the question.
            let now = (now_ms() / 1000) as i64;
            out.push(Line::from("a question's reminder").bold());
            about.push(vec![
                ("question from ".into(), None),
                (short(&reminder.asker), Some(reminder.asker.clone())),
            ]);
            about.push(vec![(crate::asks::clock(reminder.at, now), None)]);
            if let Some(why) = &item.failed {
                body.extend(crate::transcript::wrap(
                    Line::from(vec![Span::raw("✗ ").fg(Color::Red), Span::raw(why.clone())]),
                    width,
                    Span::raw(""),
                    Span::raw("  "),
                ));
            }
            body.extend(crate::transcript::wrap(
                said(&reminder, now),
                width,
                Span::raw(""),
                Span::raw(""),
            ));
            body.push(Line::default());
            body.extend(crate::transcript::wrap(
                Line::from(reminder.question.clone()),
                width,
                Span::raw(""),
                Span::raw(""),
            ));
        } else if let Some(item) = self.items.get(self.selected).cloned() {
            out.push(Line::from(item.name.clone()).bold());
            let start = &item.row["start"];
            match item.kind {
                Kind::Subscription => about.push(vec![
                    ("follow for ".into(), None),
                    (short(&item.bot), Some(item.bot.clone())),
                ]),
                // One that starts its agent each run has none of its own
                // to link.
                _ if start["fresh"] == true => {
                    about.push(vec![(
                        format!("starts {} each run", short(&item.bot)),
                        None,
                    )]);
                    let model = start["model"].as_str().unwrap_or_default();
                    about.push(vec![(crate::models::label(model), None)]);
                }
                // One whose agent its first run starts names it in plain
                // words until it is there.
                _ => {
                    let there = start.is_null() || item.row["bot_id"].is_i64();
                    about.push(vec![
                        ("automation for ".into(), None),
                        (short(&item.bot), there.then(|| item.bot.clone())),
                    ]);
                }
            }
            if let Some(by) = item.row["by"].as_str() {
                about.push(vec![
                    ("added by ".into(), None),
                    (short(by), Some(by.to_owned())),
                ]);
            }
            if item.own {
                about.push(vec![("Sonny's own".into(), None)]);
            }
            if let Some(to) = item.reply_to.as_ref().filter(|to| **to != item.bot) {
                about.push(vec![
                    ("answers go to ".into(), None),
                    (short(to), Some(to.clone())),
                ]);
            }
            if let Some(why) = page_alert(&item) {
                body.extend(crate::transcript::wrap(
                    Line::from(vec![Span::raw("✗ ").fg(Color::Red), Span::raw(why)]),
                    width,
                    Span::raw(""),
                    Span::raw("  "),
                ));
                body.push(Line::default());
            }
            let fields: Vec<(&str, Vec<Field>)> = match &item.sub {
                Some(sub) => self.follow_fields(sub, &item.bot, width),
                None if item.kind == Kind::Trigger => self.automation_fields(&item),
                None => vec![("when", vec![Field::text(item.when.clone())])],
            };
            // The links in the line under the name come first.
            let first = about
                .iter()
                .flatten()
                .filter(|(_, bot)| bot.is_some())
                .count();
            let pick = (self.page.as_ref())
                .and_then(|page| page.pick)
                .and_then(|pick| pick.checked_sub(first));
            let label = lay(fields, width, pick, &mut body, &mut body_links);
            let hang = " ".repeat(label);
            // What a row says after it, whole, where its page has no field
            // for it.
            if !item.facts.is_empty() && item.sub.is_none() && item.kind != Kind::Trigger {
                body.extend(crate::transcript::wrap(
                    Line::from(item.facts.join(" · ")).dim(),
                    width,
                    Span::raw(hang.clone()),
                    Span::raw(hang.clone()),
                ));
            }
            // An automation's whole message, as it is sent.
            if !item.message.trim().is_empty() {
                body.push(Line::default());
                for line in item.message.trim_end().lines() {
                    body.extend(crate::transcript::wrap(
                        Line::from(line.to_owned()),
                        width,
                        Span::raw(""),
                        Span::raw(""),
                    ));
                }
            }
        }
        let Some(page) = self.page.as_mut() else {
            return (out, 0, Vec::new());
        };
        // The parts laid a ` · ` apart, a part that would pass the edge
        // starting the next row; each agent a link, picked as `/memory`'s
        // are.
        page.links.clear();
        let mut marks: Vec<(usize, crate::screen::Mark)> = Vec::new();
        let mut row: Vec<Span<'static>> = Vec::new();
        let mut buttons = Vec::new();
        let mut column = 0;
        let flush = |out: &mut Vec<Line<'static>>,
                     marks: &mut Vec<(usize, crate::screen::Mark)>,
                     row: &mut Vec<Span<'static>>,
                     buttons: &mut crate::screen::Buttons| {
            if !buttons.is_empty() {
                marks.push((
                    out.len(),
                    crate::screen::Mark::Buttons(std::mem::take(buttons)),
                ));
            }
            out.push(Line::from(std::mem::take(row)));
        };
        for part in about {
            let wide: usize = part.iter().map(|(text, _)| text.width()).sum();
            if column > 0 && column + 3 + wide > width {
                flush(&mut out, &mut marks, &mut row, &mut buttons);
                column = 0;
            }
            if column > 0 {
                row.push(Span::raw(" · ").dim());
                column += 3;
            }
            for (text, bot) in part {
                let wide = text.width();
                match bot {
                    Some(bot) => {
                        let at = page.links.len();
                        let mut name = Span::raw(text).fg(Color::Blue);
                        if page.pick == Some(at) {
                            name = name.add_modifier(ratatui::style::Modifier::REVERSED);
                        }
                        row.push(name);
                        let (start, end) = (column as u16, (column + wide) as u16);
                        // A column in from the line between, as the card is
                        // drawn.
                        let button = format!("{}{BUTTON}link/{at}", crate::screen::BUTTON);
                        buttons.push((start + 1, end + 1, button));
                        page.links.push((out.len(), start, end, Target::Agent(bot)));
                    }
                    None => row.push(Span::raw(text).dim()),
                }
                column += wide;
            }
        }
        if !row.is_empty() {
            flush(&mut out, &mut marks, &mut row, &mut buttons);
        }
        if !body.is_empty() {
            out.push(Line::default());
            let top = out.len();
            for (row, start, end, target) in body_links {
                let at = page.links.len();
                let button = format!("{}{BUTTON}link/{at}", crate::screen::BUTTON);
                let buttons = vec![(start + 1, end + 1, button)];
                marks.push((top + row, crate::screen::Mark::Buttons(buttons)));
                page.links.push((top + row, start, end, target));
            }
            out.extend(body);
        }
        let at = (page.pick)
            .and_then(|pick| page.links.get(pick))
            .map_or(0, |link| link.0);
        (out, at, marks)
    }

    /// A source's fields on its page: its command, the newest word on
    /// each topic it named in the last day, and each follow of it.
    fn source_fields(&self, name: &str, width: usize) -> Vec<(&'static str, Vec<Field>)> {
        let page = &self.source;
        let mut fields = Vec::new();
        if let Some(set) = page.set.as_ref().filter(|_| !page.own) {
            fields.push(("command", vec![Field::text(plain(&set.command))]));
        }
        let newest = page.said.iter().map(|event| event.at_ms).max();
        let said = match (&page.down, newest) {
            (Some(_), Some(at)) => vec![Field::text(format!("nothing since {}", since_of(at)))],
            (_, None) => vec![Field {
                spans: vec![Span::raw("nothing yet").dim()],
                link: None,
            }],
            (None, Some(_)) => self.said_rows(name, width),
        };
        fields.push(("said", said));
        let followed = match page.followed.is_empty() {
            true => vec![Field::text("no one follows it".into())],
            false => (page.followed.iter())
                .map(|(follow, bot)| Field {
                    spans: vec![
                        Span::raw(format!("{follow} for ")),
                        Span::raw(self.scope.short(bot)).fg(Color::Blue),
                    ],
                    link: Some((1, Target::Agent(bot.clone()))),
                })
                .collect(),
        };
        fields.push(("followed", followed));
        fields
    }

    /// The newest word on each topic the open source named in the last
    /// day, newest first, or its newest word when it named none then:
    /// `main  main: build 412 passed · 2m ago`. A topic Sonny has a view
    /// for is a link there: an agent, a commons thread, a fact.
    fn said_rows(&self, name: &str, width: usize) -> Vec<Field> {
        let now = now_ms() as u128;
        let day = lookout::sources::RECENT_SPAN.as_millis();
        let mut said: Vec<&lookout::sources::Event> = (self.source.said.iter())
            .filter(|event| now.saturating_sub(event.at_ms) <= day)
            .collect();
        if said.is_empty() {
            said.extend(self.source.said.iter().max_by_key(|event| event.at_ms));
        }
        said.sort_by_key(|event| std::cmp::Reverse(event.at_ms));
        let more = said.len().saturating_sub(SAID_TOPICS);
        said.truncate(SAID_TOPICS);
        let wide = (said.iter())
            .filter_map(|event| event.topic.as_deref())
            .map(|topic| topic.width().min(TOPIC_WIDTH))
            .max();
        let topic_room = wide.map_or(0, |wide| wide + 2);
        let age_of =
            |event: &lookout::sources::Event| ago((now.saturating_sub(event.at_ms) / 1000) as u64);
        let text_of =
            |event: &lookout::sources::Event| plain(event.text.lines().next().unwrap_or_default());
        // A word is never cut beside its topic: when any does not fit
        // there, every word goes under its topic, two rows at most.
        let under = said.iter().any(|event| {
            SOURCE_LABEL + topic_room + text_of(event).width() + 3 + age_of(event).width() > width
        });
        let mut rows = Vec::new();
        for event in said {
            let (text, age) = (text_of(event), age_of(event));
            let mut spans = Vec::new();
            let mut link = None;
            if let Some(wide) = wide {
                let topic = event.topic.as_deref().unwrap_or_default();
                let shown = crate::transcript::cut(&plain(topic), wide);
                let pad = " ".repeat(wide - shown.width() + 2);
                match topic_target(name, event) {
                    Some(target) => {
                        spans.push(Span::raw(shown).fg(Color::Blue));
                        link = Some((0, target));
                    }
                    None => spans.push(Span::raw(shown)),
                }
                spans.push(Span::raw(pad));
            }
            if !under {
                spans.push(Span::raw(text));
                spans.push(Span::raw(format!(" · {age}")).dim());
                rows.push(Field { spans, link });
                continue;
            }
            spans.push(Span::raw(age).dim());
            rows.push(Field { spans, link });
            let room = width.saturating_sub(SOURCE_LABEL + 2);
            rows.extend(two_rows(&text, room).into_iter().map(|row| Field {
                spans: vec![Span::raw(format!("  {row}"))],
                link: None,
            }));
        }
        if more > 0 {
            rows.push(Field {
                spans: vec![Span::raw(format!("{more} more in the last day")).dim()],
                link: None,
            });
        }
        rows
    }

    /// A follow's fields on its page: what it watches and every agent
    /// that matches now, what each source it hears said last, how it was
    /// set, and what it told.
    fn follow_fields(
        &self,
        sub: &lookout::subs::Sub,
        subscriber: &str,
        width: usize,
    ) -> Vec<(&'static str, Vec<Field>)> {
        let now = now_ms();
        let mut fields = Vec::new();
        let watched: Vec<String> = (sub.tasks_of.iter())
            .map(|bot| format!("tasks of {}", self.scope.short(bot)))
            .chain(sub.agents.iter().map(|glob| format!("agents {glob}")))
            .collect();
        if !watched.is_empty() {
            let mut rows = vec![Field::text(watched.join(", "))];
            rows.extend(self.watched(sub, subscriber));
            // Another project's tasks are not among the rows this window
            // reads: said, so the page does not read as "none".
            for root in sub.tasks_of.iter().filter(|root| !self.scope.takes(root)) {
                rows.push(Field {
                    spans: vec![
                        Span::raw(format!("  tasks of {} are not listed here", root)).dim(),
                    ],
                    link: None,
                });
            }
            fields.push(("watches", rows));
        }
        if !sub.sources.is_empty() {
            let wide = sub.sources.iter().map(|s| s.width()).max().unwrap_or(0);
            // Past the label, the mark, the name and the space after it.
            let past = FOLLOW_LABEL + 2 + wide + 2;
            // Each source's newest word on one row, and how long ago.
            let said: Vec<Option<(String, String)>> = (sub.sources.iter())
                .map(|written| {
                    let (text, at) = self.follow.newest.get(written)?;
                    let secs = (u128::from(now).saturating_sub(*at) / 1000) as u64;
                    Some((plain(text.lines().next().unwrap_or_default()), ago(secs)))
                })
                .collect();
            // A word is never cut beside its name: when any does not fit
            // there, every word goes under its name, two rows at most.
            let under = (sub.sources.iter().zip(&said)).any(|(written, said)| {
                let name = lookout::sources::named(written).0;
                !self.follow.down.contains_key(name)
                    && said
                        .as_ref()
                        .is_some_and(|(text, age)| past + text.width() + 3 + age.width() > width)
            });
            let room = width.saturating_sub(FOLLOW_LABEL + 4);
            let mut rows = Vec::new();
            for (written, said) in sub.sources.iter().zip(said) {
                let name = lookout::sources::named(written).0;
                let pad = Span::raw(" ".repeat(wide - written.width() + 2));
                let named = Span::raw(written.clone()).fg(Color::Blue);
                let mut words = Vec::new();
                let spans = match self.follow.down.get(name) {
                    Some(down) => vec![
                        Span::raw("✗ ").fg(Color::Red),
                        named,
                        pad,
                        Span::raw(format!("down since {}", since_of(down.since_ms))),
                    ],
                    None => {
                        let mark = match self.icons.get(name) {
                            Some(icon) => Span::raw(format!("{icon} ")),
                            None => Span::raw("  "),
                        };
                        let said = match said {
                            Some((text, age)) if under => {
                                words = two_rows(&text, room);
                                vec![Span::raw(age).dim()]
                            }
                            Some((text, age)) => {
                                vec![Span::raw(text), Span::raw(format!(" · {age}")).dim()]
                            }
                            None => vec![Span::raw("nothing yet").dim()],
                        };
                        [vec![mark, named, pad], said].concat()
                    }
                };
                // The name is a link to the source's page.
                rows.push(Field {
                    spans,
                    link: Some((1, Target::Source(name.to_owned()))),
                });
                rows.extend(words.into_iter().map(|row| Field {
                    spans: vec![Span::raw(format!("  {row}"))],
                    link: None,
                }));
            }
            fields.push(("sources", rows));
        }
        let pattern = |pattern: &Value| in_words(pattern).unwrap_or_else(|| spaced(pattern));
        if let Some(filter) = &sub.filter {
            fields.push(("where", vec![Field::text(pattern(filter))]));
        }
        if let Some(steer) = &sub.steer {
            fields.push(("steers", vec![Field::text(pattern(steer))]));
        }
        if let Some(digest) = &sub.digest {
            fields.push((
                "tells",
                vec![Field::text(format!("at most every {digest}"))],
            ));
        }
        if let Some(until) = &sub.until {
            let until = sub.ends_at().map_or_else(|| until.clone(), trigger::date);
            fields.push(("until", vec![Field::text(until)]));
        }
        if let Some(runs) = sub.runs {
            let lasts = match self.follow.runs_told as u64 {
                0 => format!("{runs} update{}, none told yet", plural(runs)),
                told => format!("{} of {runs} updates left", runs.saturating_sub(told)),
            };
            fields.push(("lasts", vec![Field::text(lasts)]));
        }
        let told = match &self.follow.lately {
            None => "nothing yet".to_owned(),
            Some(lately) => {
                let day = match lately.notes {
                    0 => "none in the last day".to_owned(),
                    n => format!("{n} update{} in the last day", plural(n)),
                };
                let steered = match lately.steers {
                    0 => String::new(),
                    n => format!(", {n} steered"),
                };
                let secs = now.saturating_sub(lately.last_ms) / 1000;
                format!("{day}{steered} · last {}", ago(secs))
            }
        };
        fields.push(("told", vec![Field::text(told)]));
        fields
    }

    /// An automation's fields on its page: when it runs and when next,
    /// its check and what that last said, its last run and the agent it
    /// ran in, what is left of it, and its budget.
    fn automation_fields(&self, item: &Item) -> Vec<(&'static str, Vec<Field>)> {
        let row = &item.row;
        let now = (now_ms() / 1000) as i64;
        let mut fields = Vec::new();
        // A time at or past its end is no run.
        let until = (row["until"].as_str()).and_then(|until| trigger::moment(until).ok());
        let when_set = row["when"].as_str().unwrap_or(&item.when);
        let mut when = vec![Field::text(match (self.next_run, until) {
            (Some(next), Some(until)) if next >= until => {
                format!("{when_set} · no run before it ends")
            }
            (next, _) => when_words(when_set, next, now),
        })];
        if row["missed"] == true {
            when.push(Field::text("missed its time".into()));
        }
        fields.push(("when", when));
        if let Some(gate) = row["if"].as_str() {
            let mut rows = vec![Field::text(home_path(gate))];
            let checked = &row["checked"];
            let ago_of = |key: &str| {
                let at = checked[key].as_u64().unwrap_or_default();
                ago(now_ms().saturating_sub(at) / 1000)
            };
            match checked["gate"].as_str() {
                Some("no") => rows.push(Field::text(format!(
                    "checked {}, nothing new",
                    ago_of("checked_ms")
                ))),
                Some("go") => rows.push(Field::text(format!(
                    "checked {}, something new",
                    ago_of("checked_ms")
                ))),
                Some("failed") => rows.push(Field::text(format!(
                    "could not check since {}",
                    since_of(u128::from(checked["since_ms"].as_u64().unwrap_or_default()))
                ))),
                _ => {}
            }
            fields.push(("if", rows));
        }
        if let Some(last) = self.last_run(item) {
            fields.push(("last", vec![last]));
        }
        let mut left = Vec::new();
        if let Some(runs) = row["runs"].as_u64() {
            let sent = row["sent"].as_u64().unwrap_or(0);
            left.push(format!("{} of {runs} runs", runs.saturating_sub(sent)));
        }
        if let Some(until) = row["until"].as_str() {
            let until = trigger::moment(until).map_or_else(|_| until.to_owned(), trigger::date);
            left.push(format!("until {until}"));
        }
        if row["ended"] == true {
            left = vec!["ended".into()];
        }
        if !left.is_empty() {
            fields.push(("left", vec![Field::text(left.join(" · "))]));
        }
        if let Some(tokens) = row["turn_budget_tokens"].as_u64() {
            let what = match row["budget_counts"].as_str() {
                Some("work") => " of work",
                _ => "",
            };
            fields.push((
                "budget",
                vec![Field::text(format!(
                    "{} tokens{what} a run",
                    thousands(tokens)
                ))],
            ));
        }
        fields
    }

    /// When an automation last ran and the agent it ran in, a link; how
    /// it ended only when not well, `working` while its turn runs.
    fn last_run(&self, item: &Item) -> Option<Field> {
        let row = &item.row;
        let last = &row["last"];
        let never = || Field::text("not run yet".into());
        let Some(fired) = last["fired_ms"].as_u64() else {
            return Some(never());
        };
        // A check that could not run sent nothing: no run, and its own
        // row and the alert say why.
        if last["gate"] == "failed" {
            return (row["sent"].as_u64().unwrap_or(0) == 0).then(never);
        }
        // The agent its run went to: the run whose turn it sent, where
        // each run starts its own; none where it sent no turn.
        let run = &row["run"];
        // A shadow run went to a copy, gone once its report is passed on:
        // said in words, no link.
        let shadow = last["shadow"].is_string();
        let bot = (last["handle"].as_str().filter(|_| !shadow)).map(|handle| {
            (["running", "finished", "failed"].iter())
                .map(|key| &run[key])
                .find(|run| run["handle"].as_str() == Some(handle))
                .and_then(|run| run["bot"].as_str())
                .unwrap_or(&item.bot)
                .to_owned()
        });
        let ended = match (last["outcome"].as_str(), last["turn_status"].as_str()) {
            (Some("sent"), None) => Some("working".to_owned()),
            (Some("sent"), Some("completed")) => None,
            (Some("sent"), Some("failed")) => Some("its turn failed".into()),
            (Some("sent"), Some(status)) => Some(status.to_owned()),
            (Some("failed"), _) => Some("it could not send".into()),
            (Some(outcome), _) => Some(outcome.to_owned()),
            (None, _) => None,
        };
        let ended = match last["reply"]["outcome"] == "failed" {
            true => Some("its answer was not passed on".to_owned()),
            false => ended,
        };
        let when = ago(now_ms().saturating_sub(fired) / 1000);
        let mut spans = match &bot {
            Some(bot) => vec![
                Span::raw(format!("{when} in ")),
                Span::raw(self.scope.short(bot)).fg(Color::Blue),
            ],
            None if shadow => vec![Span::raw(format!(
                "{when} in a copy of {}",
                self.scope.short(&item.bot)
            ))],
            None => vec![Span::raw(when)],
        };
        if let Some(ended) = ended {
            spans.push(Span::raw(format!(" · {ended}")));
        }
        Some(Field {
            spans,
            link: bot.map(|bot| (1, Target::Agent(bot))),
        })
    }

    /// Every agent `sub` watches now, each a link with its mark and word,
    /// those not done first; two done or more fold to a count, a link
    /// that lists them.
    fn watched(&self, sub: &lookout::subs::Sub, subscriber: &str) -> Vec<Field> {
        let agent = |task: &Value| task["agent"].as_str().unwrap_or_default().to_owned();
        let makers: HashMap<String, String> = (self.tasks.iter())
            .filter_map(|task| Some((agent(task), task["maker"].as_str()?.to_owned())))
            .collect();
        // Made by one it names the tasks of, or by one of theirs, as the
        // lookout follows them: never the one named itself.
        let descends = |name: &str| {
            let mut at = makers.get(name);
            for _ in 0..64 {
                let Some(maker) = at else {
                    return false;
                };
                if sub.tasks_of.contains(maker) {
                    return true;
                }
                at = makers.get(maker);
            }
            false
        };
        // Nor the one it is for, nor a workflow's workers or its row, as
        // the lookout.
        let (done, live): (Vec<&Value>, Vec<&Value>) = (self.tasks.iter())
            .filter(|task| {
                let name = agent(task);
                !name.is_empty()
                    && name != subscriber
                    && !crate::workflow::is_worker(&name)
                    && !name.contains(crate::workflow::view::MARK)
                    && (descends(&name)
                        || (sub.agents.iter()).any(|glob| lookout::subs::matches(glob, &name)))
            })
            .partition(|task| task["group"] == "done");
        let open = self.page.as_ref().is_some_and(|page| page.done_open);
        let fold = done.len() > 1 && !open;
        let shown: Vec<&Value> = match fold {
            true => live,
            false => [live, done.clone()].concat(),
        };
        let name = |task: &Value| task["name"].as_str().unwrap_or_default().to_owned();
        let wide = shown
            .iter()
            .map(|task| name(task).width())
            .max()
            .unwrap_or(0);
        let mut rows: Vec<Field> = (shown.iter())
            .map(|task| {
                let state = match task["group"] == "done" {
                    true => crate::started::DONE,
                    false => task["state"].as_str().unwrap_or_default(),
                };
                let named = name(task);
                let pad = " ".repeat(wide - named.width() + 2);
                Field {
                    spans: vec![
                        Span::raw("  "),
                        crate::status::mark(state),
                        Span::raw(named).fg(Color::Blue),
                        Span::raw(pad),
                        Span::raw(state.to_owned()).dim(),
                    ],
                    link: Some((2, Target::Agent(agent(task)))),
                }
            })
            .collect();
        if fold {
            rows.push(Field {
                spans: vec![
                    Span::raw("  "),
                    crate::status::mark(crate::started::DONE),
                    Span::raw(format!("{} done", done.len())).fg(Color::Blue),
                ],
                link: Some((2, Target::Done)),
            });
        }
        rows
    }

    fn item_lines(&self, item: &Item, picked: bool, width: usize) -> Vec<Line<'static>> {
        let mut out = Vec::new();
        // The pick has a column of its own, as the composer's `› ` does,
        // so it never touches the row's mark; what follows lines up under
        // the name.
        // A column before the mark, as the picker's rows have, so the
        // pick reverses from the mark on.
        let pick = Span::raw("  ");
        // Sonny's one mark for what failed, as `/status` shows a task.
        let mark = match item.kind {
            _ if item.failed.is_some() => Span::raw("✗ ").fg(Color::Red),
            Kind::Trigger => Span::raw("↻ ").dim(),
            // A follow of sources with icons is marked by them, as its
            // agent's `/status` row is.
            Kind::Subscription => match icons_of(item, &self.icons) {
                Some(icons) => Span::raw(format!("{icons} ")).dim(),
                None => Span::raw("◎ ").dim(),
            },
            Kind::Server | Kind::Setup => Span::raw("  "),
        };
        let now = (now_ms() / 1000) as i64;
        if let Some(reminder) = &item.reminder {
            return self.reminder_lines(item, reminder, pick, mark, picked, width, now);
        }
        let name = Span::raw(item.name.clone()).bold();
        let mut spans = vec![pick, mark, name];
        match item.kind {
            Kind::Trigger => {
                spans.push(Span::raw(format!(" {}", item.when)));
                spans.push(Span::raw(format!(" → {}", self.scope.short(&item.bot))).dim());
            }
            Kind::Subscription => {
                spans.push(Span::raw(format!(" for {}", self.scope.short(&item.bot))).dim());
            }
            Kind::Server => {
                spans.push(Span::raw(" could not be read by the nightly memory cleanup").dim());
            }
            Kind::Setup => {
                spans.push(Span::raw(" is not scheduled: nothing cleans up memory").dim());
            }
        }
        let mut first =
            crate::transcript::wrap(Line::from(spans), width, Span::raw(""), Span::raw(INDENT));
        if picked && let Some(row) = first.first_mut() {
            crate::list::pick(row);
        }
        out.extend(first);
        if let Some(why) = &item.failed {
            out.extend(crate::transcript::wrap(
                Line::from(why.clone()),
                width,
                Span::raw(INDENT),
                Span::raw(INDENT),
            ));
        }
        let mut facts = item.facts.clone();
        if let Some(to) = item.reply_to.as_ref().filter(|to| **to != item.bot) {
            facts.push(format!("answers go to {}", self.scope.short(to)));
        }
        if !facts.is_empty() {
            out.extend(crate::transcript::wrap(
                Line::from(facts.join(" · ")).dim(),
                width,
                Span::raw(INDENT),
                Span::raw(INDENT),
            ));
        }
        // A follow in the words its page uses.
        let what = match &item.sub {
            Some(sub) => self.follow_words(sub),
            None => item.what.clone(),
        };
        if !what.is_empty() {
            out.extend(crate::transcript::capped(
                Line::from(what).dim(),
                width,
                Span::raw(format!("{INDENT}└ ")).dim(),
                1,
            ));
        }
        out
    }

    /// What a follow watches and how it was set, in the words its page
    /// uses: `tasks of lead, source ci where text starts with "main:",
    /// until Fri 20 Nov 09:00`.
    fn follow_words(&self, sub: &lookout::subs::Sub) -> String {
        let pattern = |pattern: &Value| in_words(pattern).unwrap_or_else(|| spaced(pattern));
        let mut words: Vec<String> = (sub.tasks_of.iter())
            .map(|bot| format!("tasks of {}", self.scope.short(bot)))
            .chain(sub.agents.iter().map(|glob| format!("agents {glob}")))
            .chain(sub.sources.iter().map(|source| format!("source {source}")))
            .collect();
        if let Some(filter) = &sub.filter {
            let last = words.pop().unwrap_or_default();
            words.push(
                format!("{last} where {}", pattern(filter))
                    .trim_start()
                    .to_owned(),
            );
        }
        if let Some(digest) = &sub.digest {
            words.push(format!("at most every {digest}"));
        }
        if let Some(steer) = &sub.steer {
            words.push(format!("steers when {}", pattern(steer)));
        }
        if let Some(until) = &sub.until {
            let until = sub.ends_at().map_or_else(|| until.clone(), trigger::date);
            words.push(format!("until {until}"));
        }
        if let Some(runs) = sub.runs {
            words.push(format!("for {runs} update{}", plural(runs)));
        }
        words.join(", ")
    }

    /// A question's reminder's rows: whose question and when, then what
    /// happens then in the card's words, then the question.
    #[allow(clippy::too_many_arguments)]
    fn reminder_lines(
        &self,
        item: &Item,
        reminder: &crate::asks::Reminder,
        pick: Span<'static>,
        mark: Span<'static>,
        picked: bool,
        width: usize,
        now: i64,
    ) -> Vec<Line<'static>> {
        let mut out = Vec::new();
        let spans = vec![
            pick,
            mark,
            Span::raw(format!(
                "question from {}",
                self.scope.short(&reminder.asker)
            ))
            .bold(),
            Span::raw(format!("  {}", crate::asks::clock(reminder.at, now))),
            Span::raw(format!(" → {}", self.scope.short(&item.bot))).dim(),
        ];
        let mut first =
            crate::transcript::wrap(Line::from(spans), width, Span::raw(""), Span::raw(INDENT));
        if picked && let Some(row) = first.first_mut() {
            crate::list::pick(row);
        }
        out.extend(first);
        if let Some(why) = &item.failed {
            out.extend(crate::transcript::wrap(
                Line::from(why.clone()),
                width,
                Span::raw(INDENT),
                Span::raw(INDENT),
            ));
        }
        out.extend(crate::transcript::wrap(
            said(reminder, now),
            width,
            Span::raw(INDENT),
            Span::raw(INDENT),
        ));
        out.extend(crate::transcript::capped(
            Line::from(reminder.question.clone()).dim(),
            width,
            Span::raw(format!("{INDENT}└ ")).dim(),
            1,
        ));
        out
    }

    /// The line under the card: what was done, the removal to confirm, or
    /// the keys the picked row takes.
    pub fn footer(&self) -> Option<Line<'static>> {
        if let Some((_, what)) = &self.stopping {
            return Some(Line::from(vec![
                Span::raw(format!("Stop {what} and all it started?")).bold(),
                Span::raw(" It can't be undone.").dim(),
            ]));
        }
        if let Some((_, _, name)) = &self.confirm {
            return Some(Line::from(vec![
                Span::raw(format!("Remove {name}?")).bold(),
                Span::raw(" It can't be undone.").dim(),
            ]));
        }
        self.notice
            .as_ref()
            .map(|notice| Line::from(notice.clone()))
    }

    /// What the keys do, as the pane's title says it: the picked row's,
    /// or the removal's while it waits to be confirmed.
    pub fn keys(&self) -> String {
        use crate::hint::{Esc, Keys};
        if self.confirm.is_some() {
            return (Keys::new().enter(crate::hint::Enter::Confirm("remove")))
                .esc(Esc::Cancel)
                .text();
        }
        if self.stopping.is_some() {
            return (Keys::new().enter(crate::hint::Enter::Confirm("stop")))
                .esc(Esc::Cancel)
                .text();
        }
        let mut keys = Keys::new();
        match &self.page {
            // The pane's title says `← back` first, beside its `‹`.
            Some(page) => {
                if page.links.len() > 1 {
                    keys = keys.pick();
                }
                if page.pick.is_some() {
                    keys = keys.enter(crate::hint::Enter::Open);
                }
            }
            None => {
                if self.rows() > 1 {
                    keys = keys.pick();
                }
                if self.has_page() {
                    keys = keys.enter(crate::hint::Enter::Open);
                }
            }
        }
        // A source's page edits nothing.
        if self.source_open().is_some() {
            return keys.esc(Esc::Close).text();
        }
        if self.command().is_some() {
            return keys.key("x", "stop").esc(Esc::Close).text();
        }
        match self.items.get(self.selected) {
            // A question's reminder goes once the question is answered.
            Some(item) if item.reminder.is_some() => {}
            Some(item) if item.kind == Kind::Trigger => {
                keys = keys.key("r", "run now").key("x", "remove")
            }
            Some(item) if !item.own && !matches!(item.kind, Kind::Server | Kind::Setup) => {
                keys = keys.key("x", "remove")
            }
            _ => {}
        }
        keys.esc(Esc::Close).text()
    }
}

/// What a question's reminder does unless the person answers first, as
/// the card says it: muted, the time at the text's own weight.
fn said(reminder: &crate::asks::Reminder, now: i64) -> Line<'static> {
    Line::from(
        (reminder.says(now).into_iter())
            .map(|(text, weight)| match weight {
                true => Span::raw(text),
                false => Span::raw(text).dim(),
            })
            .collect::<Vec<_>>(),
    )
}

/// A pattern as JSON with a space after each `:` and `,`, so a page
/// wraps it between its parts rather than inside one.
/// When an automation runs, as set, then when it runs next: `every 1h ·
/// next 14:00, in 35m`, `a commit lands in ~/tally`, `~/notes.md changes`.
fn when_words(when: &str, next: Option<i64>, now: i64) -> String {
    if let Some(path) = when.strip_prefix("file ") {
        return format!("{} changes", home_path(path));
    }
    if let Some(path) = when.strip_prefix("commit ") {
        return format!("a commit lands in {}", home_path(path));
    }
    if when.is_empty() || when == "fire" || when == trigger::ON_DEMAND {
        return "only when run".into();
    }
    match (when.strip_prefix("at "), next) {
        (Some(_), Some(next)) => format!("once, {}", coming(next, now)),
        (Some(at), None) => format!(
            "once, {}",
            trigger::moment(at).map_or_else(|_| at.to_owned(), trigger::date)
        ),
        (None, Some(next)) => format!("{when} · next {}", coming(next, now)),
        (None, None) => when.to_owned(),
    }
}

/// A time to come and how far off: `03:00, in 15h` within a day, else
/// its date.
fn coming(at: i64, now: i64) -> String {
    let left = (at - now).max(0);
    let time = match left < 86_400 {
        true => {
            let local = trigger::local(at);
            format!("{:02}:{:02}", local.hour, local.minute)
        }
        false => trigger::date(at),
    };
    // Minutes up to the hour, rounded up; from 60 minutes, hours.
    let minutes = ((left + 59) / 60).max(1);
    let off = match left {
        _ if minutes < 60 => format!("{minutes}m"),
        0..86_400 => format!("{}h", (left / 3600).max(1)),
        _ => format!("{}d", left / 86_400),
    };
    format!("{time}, in {off}")
}

/// A path written from `~` where it is under the home folder, as the
/// footer writes one; a command's paths too.
fn home_path(text: &str) -> String {
    match std::env::var("HOME") {
        Ok(home) if home.len() > 1 => {
            let home = home.trim_end_matches('/');
            let text = text.replace(&format!("{home}/"), "~/");
            match text.strip_suffix(home) {
                Some(rest) if rest.is_empty() || rest.ends_with(' ') => format!("{rest}~"),
                _ => text,
            }
        }
        _ => text.to_owned(),
    }
}

/// `9000` as `9,000`.
fn thousands(n: u64) -> String {
    let digits = n.to_string();
    let mut out = String::new();
    for (i, c) in digits.chars().enumerate() {
        if i > 0 && (digits.len() - i).is_multiple_of(3) {
            out.push(',');
        }
        out.push(c);
    }
    out
}

/// A pattern in words, as a page says it: `text starts with "main:"`,
/// each field's values an `or` apart and the fields an `and`. None for one
/// that nests or has `$or`, which a page shows as written.
fn in_words(pattern: &Value) -> Option<String> {
    let mut said = Vec::new();
    for (field, values) in pattern.as_object()? {
        if field == "$or" {
            return None;
        }
        let words = (values.as_array()?.iter())
            .map(value_words)
            .collect::<Option<Vec<_>>>()?;
        said.push(format!("{field} {}", words.join(" or ")));
    }
    (!said.is_empty()).then(|| said.join(" and "))
}

/// What one value of a pattern's field takes, in words.
fn value_words(value: &Value) -> Option<String> {
    let Value::Object(matcher) = value else {
        return Some(format!("is {value}"));
    };
    let (name, arg) = matcher.iter().next().filter(|_| matcher.len() == 1)?;
    let list = |values: &[Value], and: &str| {
        let words: Vec<String> = (values.iter())
            .map(|v| v.as_str().map_or_else(|| v.to_string(), str::to_owned))
            .collect();
        words.join(and)
    };
    Some(match (name.as_str(), arg) {
        ("prefix", Value::String(_)) => format!("starts with {arg}"),
        ("suffix", Value::String(_)) => format!("ends with {arg}"),
        ("wildcard", Value::String(_)) => format!("matches {arg}"),
        ("equals-ignore-case", Value::String(_)) => format!("is {arg} in any case"),
        ("exists", Value::Bool(true)) => "exists".into(),
        ("exists", Value::Bool(false)) => "is missing".into(),
        // `[">", 0, "<=", 5]`: `is > 0 and <= 5`.
        ("numeric", Value::Array(terms)) => {
            let terms: Vec<String> = terms.chunks(2).map(|term| list(term, " ")).collect();
            format!("is {}", terms.join(" and "))
        }
        ("anything-but", Value::Array(values)) => {
            let values: Vec<String> = values.iter().map(Value::to_string).collect();
            format!("is none of {}", values.join(", "))
        }
        ("anything-but", Value::Object(_)) => {
            let words = value_words(arg)?;
            let (from, to) = [
                ("starts with", "does not start with"),
                ("ends with", "does not end with"),
                ("matches", "does not match"),
                ("is", "is not"),
            ]
            .into_iter()
            .find(|(from, _)| words.starts_with(from))?;
            format!("{to}{}", &words[from.len()..])
        }
        ("anything-but", _) => format!("is not {arg}"),
        _ => return None,
    })
}

fn spaced(value: &Value) -> String {
    match value {
        Value::Array(items) => {
            let items: Vec<String> = items.iter().map(spaced).collect();
            format!("[{}]", items.join(", "))
        }
        Value::Object(fields) => {
            let fields: Vec<String> = (fields.iter())
                .map(|(key, value)| format!("{}: {}", Value::from(key.as_str()), spaced(value)))
                .collect();
            format!("{{{}}}", fields.join(", "))
        }
        value => value.to_string(),
    }
}

/// Where a row's lines after the first start: under its name, past the
/// pick and the mark.
const INDENT: &str = "    ";

fn key_of(item: &Item) -> (Kind, String, String) {
    (item.kind, item.bot.clone(), item.name.clone())
}

fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64
}

/// How long ago, roughly: `40s ago`, `5m ago`, `3h ago`, `2d ago`.
fn ago(seconds: u64) -> String {
    match seconds {
        0..60 => format!("{seconds}s ago"),
        60..3600 => format!("{}m ago", seconds / 60),
        3600..86400 => format!("{}h ago", seconds / 3600),
        _ => format!("{}d ago", seconds / 86400),
    }
}

/// What a run came to, on one line, a daemon's error code at its start
/// in the Rulebook's words: the daemon keeps `bot` inside itself.
/// The page's alert: an automation's check in the page's own words, as
/// its row calls it `if`; anything else as the list says it.
fn page_alert(item: &Item) -> Option<String> {
    let row = &item.row;
    let check = |words: &str, why: &str| {
        let why = why.strip_prefix("--if: ").unwrap_or(why);
        let why = why.replace("exit status: ", "exit status ");
        format!("{words}: {}", first_line(&why))
    };
    // A check it no longer has says nothing, as on the list.
    if item.kind == Kind::Trigger && row["if"].is_string() {
        if let Some(why) = row["if_cannot_run"].as_str() {
            return Some(check("its check cannot run", why));
        }
        let checked = &row["checked"];
        if checked["gate"] == "failed" {
            let why = checked["detail"].as_str().filter(|why| !why.is_empty());
            return Some(match why {
                Some(why) => check("its check could not run", why),
                None => "its check could not run".into(),
            });
        }
    }
    item.failed.clone()
}

fn detail_line(detail: &str) -> String {
    let codes = [("bot_busy", "busy"), ("bot_not_found", "agent not found")];
    let worded = (codes.iter())
        .find_map(|(code, words)| Some(format!("{words}{}", detail.strip_prefix(code)?)));
    first_line(worded.as_deref().unwrap_or(detail))
}

fn first_line(text: &str) -> String {
    let line = text.lines().next().unwrap_or("");
    let mut cut: String = line.chars().take(80).collect();
    if cut.len() < line.len() || text.lines().nth(1).is_some() {
        cut.push('…');
    }
    cut
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn item(kind: Kind, name: &str, bots: &[&str], own: bool) -> Item {
        Item {
            kind,
            name: name.into(),
            bot: bots[0].into(),
            bots: bots.iter().map(|b| b.to_string()).collect(),
            when: "every 1d".into(),
            what: "Read preferences.md".into(),
            facts: vec!["sent 2".into()],
            reply_to: None,
            own,
            failed: None,
            handle: None,
            fired_ms: None,
            episode: None,
            sources: Vec::new(),
            message: "Read preferences.md".into(),
            sub: None,
            reminder: None,
            row: Value::Null,
        }
    }

    #[test]
    fn a_bots_count_leaves_out_sonnys_own() {
        let items = [
            item(
                Kind::Trigger,
                "digest",
                &["calc.digest", "calc.lead"],
                false,
            ),
            item(Kind::Trigger, "nightly", &["calc.lead"], false),
            item(Kind::Subscription, "tasks", &["calc.lead"], true),
            item(Kind::Subscription, "prs", &["calc.lead"], false),
        ];
        let none = HashMap::new();
        assert_eq!(fact(&items, "calc.fix", &none), "");
        assert_eq!(
            fact(&items, "calc.lead", &none),
            "↻ 2 automations, 1 follow"
        );
        assert_eq!(fact(&items, "calc.digest", &none), "↻ 1 automation");
        assert_eq!(fact(&items, "calc.fix", &none), "");
    }

    /// A view of `items` in project `calc`, its first row picked.
    fn view_of(store: PathBuf, items: Vec<Item>) -> View {
        View {
            scope: Scope {
                project: "calc".into(),
                lead: "calc.lead".into(),
                glob: "calc.*".into(),
            },
            store,
            store_id: None,
            bot: "calc.lead".into(),
            items,
            turns: HashMap::new(),
            lookout: String::new(),
            icons: HashMap::from([
                ("mail".to_owned(), "✉".to_owned()),
                ("ci".to_owned(), "⚙".to_owned()),
            ]),
            selected: 0,
            confirm: None,
            commands: Commands::default(),
            stopping: None,
            notice: None,
            page: None,
            tasks: Arc::default(),
            follow: Follow::default(),
            next_run: None,
            source: SourcePage::default(),
            output: Output::default(),
        }
    }

    fn follow_item(sub: lookout::subs::Sub, own: bool) -> Item {
        Item {
            sub: Some(sub.clone()),
            reminder: None,
            facts: match own {
                true => vec!["Sonny's own, for the lead's tasks".into()],
                false => Vec::new(),
            },
            message: String::new(),
            ..item(Kind::Subscription, &sub.name, &["calc.lead"], own)
        }
    }

    fn page_text(view: &mut View, width: usize) -> Vec<String> {
        (view.page_lines(width).0.iter())
            .map(|l| l.to_string().trim_end().to_owned())
            .collect()
    }

    #[test]
    fn a_follows_page_says_what_it_does_now_after_how_it_was_set() {
        let dir = std::env::temp_dir().join(format!("background-follow-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let store = dir.join("state.sqlite");
        let sub = lookout::subs::Sub {
            name: "inbox".into(),
            tasks_of: Vec::new(),
            agents: Vec::new(),
            sources: vec!["mail".into(), "ci:main".into()],
            filter: Some(json!({"text": [{"prefix": "main:"}, {"prefix": "release:"}]})),
            digest: Some("5m".into()),
            steer: Some(json!({"$or": [{"text": [{"suffix": "failed"}]}]})),
            owner: None,
            until: Some("2026-11-20 09:00".into()),
            runs: Some(3),
            made: Some(7),
        };
        let now = now_ms();
        // The lookout's files: mail down, ci's newest word on main, one
        // note told of the three it lasts, two in the last day.
        let sources = lookout::beside(&store, ".sources");
        let since = now - 30 * 60 * 1000;
        let health = lookout::sources::health_file(&sources, "mail");
        std::fs::create_dir_all(health.parent().unwrap()).unwrap();
        let word = json!({"down": true, "since_ms": since, "failed": "exit status: 3"});
        std::fs::write(&health, word.to_string()).unwrap();
        let recent = lookout::sources::recent_file(&sources, "ci");
        std::fs::create_dir_all(recent.parent().unwrap()).unwrap();
        let said = |topic: &str, text: &str, ago_ms: u64, seq: u64| {
            json!({"source": "ci", "topic": topic, "text": text, "at_ms": now - ago_ms, "seq": seq})
                .to_string()
        };
        let lines = [
            said("main", "main: build 411 passed", 3_600_000, 1),
            said(
                "main",
                "main: build 412 passed\nall 1,240 tests",
                120_000,
                2,
            ),
            said("nightly", "nightly: build 9 failed", 60_000, 3),
            // Newer, but not what its filter takes: never its newest.
            said("main", "deploy: started", 30_000, 4),
        ];
        std::fs::write(&recent, lines.join("\n") + "\n").unwrap();
        let mut counts = lookout::subs::Counts::default();
        let of = sub.identity(Some(5));
        counts.told("inbox", &of, "lookout-a", 3, false, now - 3_600_000);
        counts.told("inbox", &of, "lookout-b", 1, true, now - 120_000);
        let file = lookout::subs::counts_file(&store, "calc.lead");
        std::fs::create_dir_all(file.parent().unwrap()).unwrap();
        std::fs::write(&file, counts.to_json().to_string()).unwrap();
        let held = lookout::held_file(&store, "calc.lead");
        std::fs::create_dir_all(held.parent().unwrap()).unwrap();
        std::fs::write(
            &held,
            json!({"runs_told": {"inbox@7": ["lookout-a"]}}).to_string(),
        )
        .unwrap();
        // As the list reads it, failed.
        let item = Item {
            failed: Some(format!(
                "mail is down since {}: exit status 3",
                since_of(u128::from(since))
            )),
            ..follow_item(sub.clone(), false)
        };
        let mut view = view_of(store.clone(), vec![item]);
        view.key(
            KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE),
            0..usize::MAX,
        );
        let until = trigger::date(sub.ends_at().unwrap());
        assert_eq!(
            page_text(&mut view, 60),
            [
                "inbox".to_owned(),
                "follow for lead".into(),
                "".into(),
                format!(
                    "✗ mail is down since {}: exit status 3",
                    since_of(u128::from(since))
                ),
                "".into(),
                format!(
                    "sources  ✗ mail     down since {}",
                    since_of(u128::from(since))
                ),
                "         ⚙ ci:main  main: build 412 passed · 2m ago".into(),
                "where    text starts with \"main:\" or starts with \"release:\"".into(),
                "steers   {\"$or\": [{\"text\": [{\"suffix\": \"failed\"}]}]}".into(),
                "tells    at most every 5m".into(),
                format!("until    {until}"),
                "lasts    2 of 3 updates left".into(),
                "told     2 updates in the last day, 1 steered · last 2m ago".into(),
            ]
        );
        // At 44 columns the newest word is cut to fit its row.
        let narrow = page_text(&mut view, 44);
        // The list's row says it in the page's words.
        let card: Vec<String> = (view.card(150).0.iter()).map(|l| l.to_string()).collect();
        assert!(card.iter().any(|l| l.contains(
            "└ source mail, source ci:main where text starts with \"main:\" or starts with"
        )));
        assert!(
            view.follow_words(&sub)
                .ends_with(&format!("at most every 5m, steers when {{\"$or\": [{{\"text\": [{{\"suffix\": \"failed\"}}]}}]}}, until {until}, for 3 updates")),
        );
        // At 44 columns the newest word goes under its name, whole.
        assert_eq!(
            narrow[5..8],
            [
                format!(
                    "sources  ✗ mail     down since {}",
                    since_of(u128::from(since))
                ),
                "         ⚙ ci:main  2m ago".into(),
                "           main: build 412 passed".into(),
            ]
        );
        // Before any note, it says so.
        std::fs::remove_file(&file).unwrap();
        std::fs::remove_file(&held).unwrap();
        view.follow = view.read_follow();
        let page = page_text(&mut view, 60);
        assert!(page.contains(&"lasts    3 updates, none told yet".to_owned()));
        assert!(page.contains(&"told     nothing yet".to_owned()));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_word_too_long_for_its_row_breaks_after_a_word_onto_two_rows_at_most() {
        assert_eq!(
            two_rows("main: build 417 passed", 33),
            ["main: build 417 passed"]
        );
        assert_eq!(
            two_rows(
                "release: build 417 failed in the integration suite on the arm64 runner after 14 minutes",
                33
            ),
            [
                "release: build 417 failed in the",
                "integration suite on the arm64 r…"
            ]
        );
    }

    #[test]
    fn a_sources_page_opens_from_a_follows_and_says_how_it_runs_and_who_follows_it() {
        let dir = std::env::temp_dir().join(format!("background-source-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let store = dir.join("state.sqlite");
        let follow = |name: &str, sources: &[&str]| lookout::subs::Sub {
            name: name.into(),
            tasks_of: Vec::new(),
            agents: Vec::new(),
            sources: sources.iter().map(|s| s.to_string()).collect(),
            filter: None,
            digest: None,
            steer: None,
            owner: None,
            until: None,
            runs: None,
            made: None,
        };
        let inbox = follow("inbox", &["mail", "ci:main"]);
        lookout::subs::ensure(&store, "calc.lead", inbox.clone()).unwrap();
        lookout::subs::ensure(&store, "calc.fix", follow("builds", &["ci:main"])).unwrap();
        let now = now_ms();
        let sources = lookout::beside(&store, ".sources");
        // mail down, with what its failed run wrote; its last word before.
        let since = now - 30 * 60 * 1000;
        let health = lookout::sources::health_file(&sources, "mail");
        std::fs::create_dir_all(health.parent().unwrap()).unwrap();
        let word = json!({"down": true, "since_ms": since, "failed": "exit status: 3",
            "stderr": ["connecting", "mail-poll: could not sign in"]});
        std::fs::write(&health, word.to_string()).unwrap();
        let said = |source: &str, topic: Option<&str>, text: &str, ago_ms: u64| {
            json!({"source": source, "topic": topic, "text": text, "at_ms": now - ago_ms, "seq": 1})
                .to_string()
        };
        let last = now - 31 * 60 * 1000;
        let recent = lookout::sources::recent_file(&sources, "mail");
        std::fs::create_dir_all(recent.parent().unwrap()).unwrap();
        std::fs::write(
            lookout::sources::recent_file(&sources, "mail"),
            said("mail", None, "1 new", 31 * 60 * 1000) + "\n",
        )
        .unwrap();
        // ci's newest word on each topic, one too old to say.
        let lines = [
            said(
                "ci",
                Some("release"),
                "release: 2.1 shipped",
                2 * 86_400_000,
            ),
            said("ci", Some("main"), "main: build 411 passed", 3_600_000),
            said("ci", Some("main"), "main: build 412 passed", 120_000),
            said("ci", Some("nightly"), "nightly: build 9 failed", 60_000),
        ];
        std::fs::write(
            lookout::sources::recent_file(&sources, "ci"),
            lines.join("\n") + "\n",
        )
        .unwrap();
        let set = |name: &str, command: &str, every: Option<&str>, digest: Option<&str>| {
            Some(agent_client::config::Source {
                name: name.into(),
                command: command.into(),
                every: every.map(str::to_owned),
                digest: digest.map(str::to_owned),
                steer: None,
                watch: Vec::new(),
                icon: None,
                wants: None,
            })
        };
        let mut view = view_of(store.clone(), vec![follow_item(inbox, false)]);
        let key = |view: &mut View, code: KeyCode| {
            view.key(KeyEvent::new(code, KeyModifiers::NONE), 0..usize::MAX)
        };
        key(&mut view, KeyCode::Enter);
        let _ = page_text(&mut view, 60);
        // The follow's sources are links after its agent's: mail's opens
        // its page over the follow's.
        let links: Vec<Target> = (view.page.as_ref().unwrap().links.iter())
            .map(|link| link.3.clone())
            .collect();
        assert_eq!(
            links,
            [
                Target::Agent("calc.lead".into()),
                Target::Source("mail".into()),
                Target::Source("ci".into()),
            ]
        );
        key(&mut view, KeyCode::Down);
        key(&mut view, KeyCode::Enter);
        assert_eq!(view.depth(), 2);
        view.source.set = set("mail", "~/bin/mail-poll --inbox", Some("20s"), None);
        let config = home_path(&agent_client::config::path().unwrap().to_string_lossy());
        assert_eq!(
            page_text(&mut view, 60),
            [
                "mail".to_owned(),
                "source · runs every 20s".into(),
                "".into(),
                format!(
                    "✗ down since {}: exit status 3",
                    since_of(u128::from(since))
                ),
                "  connecting".into(),
                "  mail-poll: could not sign in".into(),
                "".into(),
                "command   ~/bin/mail-poll --inbox".into(),
                format!("said      nothing since {}", since_of(u128::from(last))),
                "followed  inbox for lead".into(),
                format!("set in {config}"),
            ]
        );
        assert_eq!(view.keys(), "enter open · esc close");
        // `x` removes nothing here: a source is the person's.
        key(&mut view, KeyCode::Char('x'));
        assert!(view.confirm.is_none());
        assert_eq!(view.items.len(), 1);
        // `←` goes back to the follow's page, mail's link picked.
        key(&mut view, KeyCode::Left);
        assert_eq!(view.depth(), 1);
        assert_eq!(view.page.as_ref().unwrap().pick, Some(1));
        let _ = page_text(&mut view, 60);
        key(&mut view, KeyCode::Down);
        key(&mut view, KeyCode::Enter);
        view.source.set = set("ci", "ci-watch", None, Some("5m"));
        assert_eq!(
            page_text(&mut view, 60),
            [
                "ci".to_owned(),
                "source · runs every 1m · tells at most every 5m".into(),
                "".into(),
                "command   ci-watch".into(),
                "said      nightly  nightly: build 9 failed · 1m ago".into(),
                "          main     main: build 412 passed · 2m ago".into(),
                "followed  builds for fix".into(),
                "          inbox for lead".into(),
                format!("set in {config}"),
            ]
        );
        // At 44 columns each word goes under its topic, whole.
        assert_eq!(
            page_text(&mut view, 44)[5..9],
            [
                "said      nightly  1m ago",
                "            nightly: build 9 failed",
                "          main     2m ago",
                "            main: build 412 passed",
            ]
        );
        let _ = page_text(&mut view, 60);
        // Each follow's agent is a link; the title's ‹ goes back too.
        let links: Vec<Target> = (view.page.as_ref().unwrap().links.iter())
            .map(|link| link.3.clone())
            .collect();
        assert_eq!(
            links,
            [
                Target::Agent("calc.fix".into()),
                Target::Agent("calc.lead".into()),
            ]
        );
        assert!(view.back());
        assert!(view.back());
        assert!(!view.on_page());
        // One of Sonny's own has no command, nor any place it is set.
        view.source = SourcePage {
            own: true,
            ..SourcePage::default()
        };
        view.page = Some(Page {
            source: Some(("memory".into(), None)),
            ..Page::default()
        });
        assert_eq!(
            page_text(&mut view, 60),
            [
                "memory".to_owned(),
                "source · Sonny's own".into(),
                "".into(),
                "said      nothing yet".into(),
                "followed  no one follows it".into(),
            ]
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_topic_of_sonnys_own_sources_links_where_sonny_shows_it() {
        let event = |source: &str, topic: &str, from: &str, fields: Value| {
            let mut event = lookout::sources::Event::parse(
                source,
                &json!({"text": "x", "topic": topic, "from": from}).to_string(),
                0,
            )
            .unwrap();
            event.fields = Arc::new(fields);
            event
        };
        assert_eq!(
            topic_target("tasks", &event("tasks", "calc.halves", "", json!({}))),
            Some(Target::Agent("calc.halves".into()))
        );
        assert_eq!(
            topic_target("commons", &event("commons", "builds", "", json!({}))),
            Some(Target::Thread("builds".into()))
        );
        assert_eq!(
            topic_target(
                "memory",
                &event(
                    "memory",
                    "no-force-push",
                    "lead saved",
                    json!({"project": "calc"})
                )
            ),
            Some(Target::Fact(Some("calc".into()), "no-force-push".into()))
        );
        assert_eq!(
            topic_target("memory", &event("memory", "gone", "removed", json!({}))),
            None
        );
        assert_eq!(
            topic_target("ci", &event("ci", "main", "", json!({}))),
            None
        );
    }

    #[test]
    fn a_follows_page_lists_the_agents_it_watches_with_the_done_folded() {
        let sub = lookout::subs::Sub {
            name: "tasks".into(),
            tasks_of: vec!["calc.lead".into()],
            agents: vec!["calc.site*".into()],
            sources: Vec::new(),
            filter: None,
            digest: None,
            steer: None,
            owner: None,
            until: None,
            runs: None,
            made: None,
        };
        let task = |name: &str, maker: &str, state: &str, group: &str| {
            json!({"name": name, "agent": format!("calc.{name}"), "state": state,
                "group": group, "maker": maker})
        };
        let mut view = view_of(std::env::temp_dir(), vec![follow_item(sub, true)]);
        view.set_tasks(&Arc::new(vec![
            task("halves", "calc.lead", "needs you", "needs_you"),
            // A task of its task is the lead's too.
            task("tests", "calc.halves", "working", "working"),
            task("sitemap", "", "idle", "idle"),
            task("other", "calc.someone", "working", "working"),
            task("readme", "calc.lead", "idle", "done"),
            task("docs", "calc.lead", "idle", "done"),
            // A workflow's own row is no agent.
            json!({"name": "lead#wf-1a2b", "agent": "calc.lead#wf-1a2b", "state": "working",
                "group": "working", "maker": "calc.lead"}),
        ]));
        view.key(
            KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE),
            0..usize::MAX,
        );
        assert_eq!(
            page_text(&mut view, 60),
            [
                "tasks",
                "follow for lead · Sonny's own",
                "",
                "watches  tasks of lead, agents calc.site*",
                "           ? halves   needs you",
                "           ● tests    working",
                "           ○ sitemap  idle",
                "           ✓ 2 done",
                "told     nothing yet",
            ]
        );
        // The lead first, then each agent, then the fold.
        let targets: Vec<Target> = (view.page.as_ref().unwrap().links.iter())
            .map(|link| link.3.clone())
            .collect();
        assert_eq!(
            targets,
            [
                Target::Agent("calc.lead".into()),
                Target::Agent("calc.halves".into()),
                Target::Agent("calc.tests".into()),
                Target::Agent("calc.sitemap".into()),
                Target::Done,
            ]
        );
        let links = &view.page.as_ref().unwrap().links;
        assert_eq!((links[1].0, links[1].1, links[1].2), (4, 13, 19));
        // `↓` to an agent, which `→` opens.
        let down = KeyEvent::new(KeyCode::Down, KeyModifiers::NONE);
        let right = KeyEvent::new(KeyCode::Right, KeyModifiers::NONE);
        view.key(down, 0..usize::MAX);
        assert_eq!(
            view.key(right, 0..usize::MAX),
            Key::Open(format!("{}calc.halves", crate::links::AGENT))
        );
        // `→` on the fold lists the done agents in its place.
        for _ in 0..3 {
            view.key(down, 0..usize::MAX);
        }
        assert_eq!(view.key(right, 0..usize::MAX), Key::Stay);
        let page = page_text(&mut view, 60);
        assert_eq!(
            page[7..10],
            [
                "           ✓ readme   done",
                "           ✓ docs     done",
                "told     nothing yet",
            ]
        );
        // The pick stays on its agent as the rows reorder, and goes to the
        // fold once the agent folds into it.
        let picked = |view: &View| {
            let page = view.page.as_ref().unwrap();
            page.links[page.pick.unwrap()].3.clone()
        };
        let page = view.page.as_mut().unwrap();
        (page.done_open, page.pick) = (false, Some(1));
        let _ = page_text(&mut view, 60);
        assert_eq!(picked(&view), Target::Agent("calc.halves".into()));
        let mut tasks = vec![
            task("tests", "calc.halves", "working", "working"),
            task("halves", "calc.lead", "idle", "idle"),
            task("readme", "calc.lead", "idle", "done"),
            task("docs", "calc.lead", "idle", "done"),
        ];
        view.set_tasks(&Arc::new(tasks.clone()));
        let _ = page_text(&mut view, 60);
        assert_eq!(picked(&view), Target::Agent("calc.halves".into()));
        tasks[1] = task("halves", "calc.lead", "idle", "done");
        view.set_tasks(&Arc::new(tasks));
        let _ = page_text(&mut view, 60);
        assert_eq!(picked(&view), Target::Done);
    }

    #[test]
    fn a_follow_of_another_projects_tasks_says_they_are_not_listed() {
        let sub = lookout::subs::Sub {
            name: "web".into(),
            tasks_of: vec!["web.lead".into()],
            agents: Vec::new(),
            sources: Vec::new(),
            filter: None,
            digest: None,
            steer: None,
            owner: None,
            until: None,
            runs: None,
            made: None,
        };
        let mut view = view_of(std::env::temp_dir(), vec![follow_item(sub, false)]);
        view.key(
            KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE),
            0..usize::MAX,
        );
        assert_eq!(
            page_text(&mut view, 60)[3..5],
            [
                "watches  tasks of web.lead",
                "           tasks of web.lead are not listed here",
            ]
        );
    }

    #[test]
    fn an_automations_page_says_when_it_runs_next_and_how_its_last_run_went() {
        let now = now_ms();
        let row = json!({"name": "nightly", "when": "cron 0 3 * * *", "bot": "calc.lead",
            "bot_id": 1, "start": null, "by": null, "reply_to": null,
            "if": "git -C /nowhere/calc log --since=1.day --oneline | grep -q .",
            "checked": {"gate": "no", "checked_ms": now - 120_000},
            "runs": 5, "sent": 2, "until": "2026-12-01 09:00", "turn_budget_tokens": 90000,
            "message": "Run the full test suite on main.\nOtherwise say nothing.",
            "last": {"outcome": "sent", "fired_ms": now - 9 * 3_600_000, "handle": "h1",
                "turn_status": "completed"}});
        let item = Item {
            row: row.clone(),
            message: row["message"].as_str().unwrap().into(),
            ..item(Kind::Trigger, "nightly", &["calc.lead"], false)
        };
        let mut view = view_of(std::env::temp_dir(), vec![item]);
        view.key(
            KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE),
            0..usize::MAX,
        );
        let soon = (now_ms() / 1000) as i64 + 15 * 3600 + 30;
        view.next_run = Some(soon);
        let local = trigger::local(soon);
        let until = trigger::date(trigger::moment("2026-12-01 09:00").unwrap());
        assert_eq!(
            page_text(&mut view, 60),
            [
                "nightly".to_owned(),
                "automation for lead".into(),
                "".into(),
                format!(
                    "when    cron 0 3 * * * · next {:02}:{:02}, in 15h",
                    local.hour, local.minute
                ),
                "if      git -C /nowhere/calc log --since=1.day --oneline |".into(),
                "        grep -q .".into(),
                "        checked 2m ago, nothing new".into(),
                "last    9h ago in lead".into(),
                format!("left    3 of 5 runs · until {until}"),
                "budget  90,000 tokens a run".into(),
                "".into(),
                "Run the full test suite on main.".into(),
                "Otherwise say nothing.".into(),
            ]
        );
        // The agent it ran in is a link after the one it is for.
        let links = &view.page.as_ref().unwrap().links;
        assert_eq!(links[1].3, Target::Agent("calc.lead".into()));
        assert_eq!((links[1].0, links[1].1, links[1].2), (7, 18, 22));
        // A folder too wide breaks after a `/`, never inside a name.
        let narrow = page_text(&mut view, 44);
        assert_eq!(narrow[4], "if      git -C /nowhere/calc log");
        assert_eq!(narrow[5], "        --since=1.day --oneline | grep -q .");
        // A part a ` · ` apart stays whole: a date never breaks.
        let narrower = page_text(&mut view, 40);
        let at = narrower.iter().position(|l| l.starts_with("left")).unwrap();
        assert_eq!(narrower[at], "left    3 of 5 runs");
        assert_eq!(narrower[at + 1], format!("        until {until}"));
    }

    #[test]
    fn a_check_that_could_not_run_is_no_run_and_no_time_past_the_end_is_next() {
        let now = now_ms();
        let since = now - 3_000;
        let row = json!({"name": "watch", "when": "cron 0 3 * * *", "bot": "calc.lead",
            "bot_id": 1, "if": "exit 7", "sent": 0, "until": "2026-11-15 23:30",
            "message": "Look.",
            "checked": {"gate": "failed", "since_ms": since, "detail": "--if: exit status: 7"},
            "last": {"outcome": "failed", "gate": "failed", "fired_ms": since,
                "detail": "--if: exit status: 7"}});
        let item = Item {
            row: row.clone(),
            failed: Some("--if: exit status: 7".into()),
            message: "Look.".into(),
            ..item(Kind::Trigger, "watch", &["calc.lead"], false)
        };
        let mut view = view_of(std::env::temp_dir(), vec![item]);
        view.key(
            KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE),
            0..usize::MAX,
        );
        view.next_run = trigger::moment("2026-11-16 03:00").ok();
        let page = page_text(&mut view, 80);
        let clock = since_of(u128::from(since));
        assert_eq!(page[3], "✗ its check could not run: exit status 7");
        assert_eq!(page[5], "when  cron 0 3 * * * · no run before it ends");
        assert_eq!(page[6], "if    exit 7");
        assert_eq!(page[7], format!("      could not check since {clock}"));
        assert_eq!(page[8], "last  not run yet");
        // Nothing went to the agent: no link to it under `last`.
        let links = &view.page.as_ref().unwrap().links;
        assert!(links.iter().all(|link| link.0 != 8), "{links:?}");
        // Replaced with no check, its old check's failure says nothing.
        view.items[0].row["if"] = Value::Null;
        view.items[0].failed = None;
        let page = page_text(&mut view, 80);
        assert!(page.iter().all(|line| !line.contains("check")), "{page:?}");
        // A shadow run went to a copy: said in words, with no link.
        view.items[0].row["sent"] = json!(1);
        view.items[0].row["last"] = json!({"outcome": "sent", "fired_ms": now - 60_000,
            "handle": "calc.lead-shadow-1/1", "shadow": "calc.lead-shadow-1",
            "turn_status": "completed"});
        let page = page_text(&mut view, 80);
        let at = page.iter().position(|l| l.starts_with("last")).unwrap();
        assert_eq!(page[at], "last  1m ago in a copy of lead");
        let links = &view.page.as_ref().unwrap().links;
        assert!(links.iter().all(|link| link.0 != at), "{links:?}");
    }

    #[test]
    fn an_automation_that_starts_its_agent_each_run_names_it_in_words() {
        let now = now_ms();
        let row = json!({"name": "pushcheck", "when": "commit /nowhere/calc", "bot": "calc.pushcheck",
            "bot_id": null, "start": {"model": "demo/scripted", "effort": null, "fresh": true},
            "by": "calc.lead", "reply_to": "calc.lead", "sent": 1,
            "message": "Check what was pushed.",
            "run": {"running": {"n": 1, "bot": "calc.pushcheck-1", "id": 4, "handle": "h"}},
            "last": {"outcome": "sent", "fired_ms": now - 25_000, "handle": "h"}});
        let item = Item {
            row: row.clone(),
            reply_to: Some("calc.lead".into()),
            message: "Check what was pushed.".into(),
            ..item(Kind::Trigger, "pushcheck", &["calc.pushcheck"], false)
        };
        let mut view = view_of(std::env::temp_dir(), vec![item]);
        view.key(
            KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE),
            0..usize::MAX,
        );
        let page = page_text(&mut view, 80);
        assert_eq!(
            page[1],
            "starts pushcheck each run · Scripted · added by lead · answers go to lead"
        );
        assert_eq!(page[3], "when  a commit lands in /nowhere/calc");
        assert_eq!(page[4], "last  25s ago in pushcheck-1 · working");
        let targets: Vec<Target> = (view.page.as_ref().unwrap().links.iter())
            .map(|link| link.3.clone())
            .collect();
        assert_eq!(
            targets,
            [
                Target::Agent("calc.lead".into()),
                Target::Agent("calc.lead".into()),
                Target::Agent("calc.pushcheck-1".into()),
            ]
        );
    }

    #[test]
    fn when_an_automation_runs_reads_in_words() {
        let now = 1_000_000;
        assert_eq!(
            when_words("file /x/notes.md", None, now),
            "/x/notes.md changes"
        );
        assert_eq!(when_words("fire", None, now), "only when run");
        assert_eq!(when_words(trigger::ON_DEMAND, None, now), "only when run");
        // From 60 minutes it is hours.
        let hour = trigger::local(now + 3590);
        assert_eq!(
            when_words("every 1h", Some(now + 3590), now),
            format!("every 1h · next {:02}:{:02}, in 1h", hour.hour, hour.minute)
        );
        assert_eq!(when_words("every 1h", None, now), "every 1h");
        let soon = trigger::local(now + 35 * 60);
        assert_eq!(
            when_words("every 1h", Some(now + 35 * 60), now),
            format!(
                "every 1h · next {:02}:{:02}, in 35m",
                soon.hour, soon.minute
            )
        );
        let far = now + 4 * 86_400 + 60;
        assert_eq!(
            when_words("at 2026-11-20 09:00", Some(far), now),
            format!("once, {}, in 4d", trigger::date(far))
        );
        assert_eq!(thousands(1_234_567), "1,234,567");
        assert_eq!(thousands(900), "900");
    }

    #[test]
    fn a_follow_of_a_tasks_tasks_lists_them_not_it_and_cuts_a_long_name_to_its_row() {
        let sub = lookout::subs::Sub {
            name: "fix".into(),
            tasks_of: vec!["calc.fix".into()],
            agents: Vec::new(),
            sources: Vec::new(),
            filter: None,
            digest: None,
            steer: None,
            owner: None,
            until: None,
            runs: None,
            made: None,
        };
        let task = |name: &str, maker: &str| {
            json!({"name": name, "agent": format!("calc.{name}"), "state": "working",
                "group": "working", "maker": maker})
        };
        let mut view = view_of(std::env::temp_dir(), vec![follow_item(sub, false)]);
        view.set_tasks(&Arc::new(vec![
            task("fix", "calc.lead"),
            task("fix-tests-for-the-rounding-of-half-cents", "calc.fix"),
        ]));
        view.key(
            KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE),
            0..usize::MAX,
        );
        let page = page_text(&mut view, 44);
        // The task it names the tasks of is no agent it hears from.
        assert_eq!(page[3], "watches  tasks of fix");
        assert_eq!(page[4], "           ● fix-tests-for-the-rounding-of-…");
        assert_eq!(page[5], "told     nothing yet");
        // Its link covers the name as drawn, on its own row.
        let links = &view.page.as_ref().unwrap().links;
        assert_eq!(links.len(), 2);
        assert_eq!((links[1].0, links[1].1, links[1].2), (4, 13, 44));
    }

    #[test]
    fn a_pattern_reads_in_words_unless_it_nests() {
        let words = |pattern: Value| in_words(&pattern);
        assert_eq!(
            words(json!({"text": [{"prefix": "main:"}]})).as_deref(),
            Some("text starts with \"main:\"")
        );
        assert_eq!(
            words(json!({"status": ["failed", "cancelled"], "type": [{"suffix": ".ended"}]}))
                .as_deref(),
            Some("status is \"failed\" or is \"cancelled\" and type ends with \".ended\"")
        );
        assert_eq!(
            words(json!({"from": [{"equals-ignore-case": "ana"}], "size": [{"numeric": [">", 0, "<=", 5]}]}))
                .as_deref(),
            Some("from is \"ana\" in any case and size is > 0 and <= 5")
        );
        assert_eq!(
            words(
                json!({"from": [{"anything-but": {"prefix": "bot"}}], "pr": [{"exists": false}]})
            )
            .as_deref(),
            Some("from does not start with \"bot\" and pr is missing")
        );
        assert_eq!(
            words(json!({"from": [{"anything-but": ["a", "b"]}]})).as_deref(),
            Some("from is none of \"a\", \"b\"")
        );
        assert_eq!(words(json!({"meta": {"team": ["web"]}})), None);
        assert_eq!(words(json!({"$or": [{"a": [1]}]})), None);
    }

    #[test]
    fn a_follow_whose_source_is_down_is_failed_until_it_works_again() {
        let dir = std::env::temp_dir().join(format!("background-down-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let store = dir.join("state.sqlite");
        let follow = |name: &str, sources: &[&str]| lookout::subs::Sub {
            name: name.into(),
            tasks_of: Vec::new(),
            agents: Vec::new(),
            sources: sources.iter().map(|s| s.to_string()).collect(),
            filter: None,
            digest: None,
            steer: None,
            owner: None,
            until: None,
            runs: None,
            made: None,
        };
        lookout::subs::ensure(&store, "calc.lead", follow("inbox", &["mail", "ci:main"])).unwrap();
        lookout::subs::ensure(&store, "calc.lead", follow("builds", &["ci"])).unwrap();
        let health = lookout::sources::health_file(&lookout::beside(&store, ".sources"), "mail");
        std::fs::create_dir_all(health.parent().unwrap()).unwrap();
        let since = now_ms() - 60_000;
        let word = json!({"down": true, "since_ms": since, "at_ms": since + 30_000,
            "failed": "exit status: 3", "stderr": ["mail-poll: could not sign in"]});
        std::fs::write(&health, word.to_string()).unwrap();
        let failed = |items: &[Item]| -> Vec<(String, Option<String>)> {
            (items.iter())
                .map(|item| (item.name.clone(), item.failed.clone()))
                .collect()
        };
        let items = read(&store, None, &HashMap::new(), |_| true);
        let clock = since_of(u128::from(since));
        assert_eq!(
            failed(&items),
            [
                ("builds".into(), None),
                (
                    "inbox".into(),
                    Some(format!("mail is down since {clock}: exit status 3"))
                ),
            ]
        );
        // Its failure stays the same one while the source stays down.
        let keys = failures(&items);
        std::thread::sleep(std::time::Duration::from_millis(20));
        assert_eq!(
            failures(&read(&store, None, &HashMap::new(), |_| true)),
            keys
        );
        // Back, it is a follow like any other.
        let back = json!({"down": false, "since_ms": since, "at_ms": since + 50_000});
        std::thread::sleep(std::time::Duration::from_millis(20));
        std::fs::write(health.with_extension("partial"), back.to_string()).unwrap();
        std::fs::rename(health.with_extension("partial"), &health).unwrap();
        let items = read(&store, None, &HashMap::new(), |_| true);
        assert!(items.iter().all(|item| item.failed.is_none()), "{items:?}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_follow_of_sources_with_icons_shows_as_them() {
        let follow = |name: &str, sources: &[&str]| Item {
            sources: sources.iter().map(|s| s.to_string()).collect(),
            ..item(Kind::Subscription, name, &["calc.lead"], false)
        };
        let items = [
            item(Kind::Subscription, "tasks", &["calc.lead"], true),
            follow("prs", &["github"]),
            follow("ci", &["github", "buildkite"]),
            follow("chat", &["slack"]),
        ];
        let icons: HashMap<String, String> = [("github", "▲"), ("slack", "#")]
            .into_iter()
            .map(|(s, i)| (s.to_owned(), i.to_owned()))
            .collect();
        assert_eq!(super::icons(&items, "calc.lead", &icons), "▲#");
        // One with a source that has none is still counted.
        assert_eq!(fact(&items, "calc.lead", &icons), "↻ 1 follow");
        assert_eq!(super::icons(&items, "calc.fix", &icons), "");
        // `/background` marks each follow as `/status` shows it: by its
        // sources' icons, or the follow mark when one has none.
        let view = View {
            scope: Scope {
                project: "calc".into(),
                lead: "calc.lead".into(),
                glob: "calc.*".into(),
            },
            store: std::env::temp_dir().join("background-icons.sqlite"),
            store_id: None,
            bot: "calc.lead".into(),
            items: items.to_vec(),
            turns: HashMap::new(),
            lookout: String::new(),
            icons,
            selected: 0,
            confirm: None,
            commands: Commands::default(),
            stopping: None,
            notice: None,
            page: None,
            tasks: Arc::default(),
            follow: Follow::default(),
            next_run: None,
            source: SourcePage::default(),
            output: Output::default(),
        };
        let rows: Vec<String> = (view.card(100).0.iter())
            .map(|l| l.to_string())
            .filter(|l| l.contains(" for lead"))
            .collect();
        assert_eq!(
            rows,
            [
                // The picked row, padded as the pick draws it.
                "  ◎ tasks for lead ",
                "  ▲ prs for lead",
                "  ◎ ci for lead",
                "  # chat for lead"
            ]
        );
    }

    #[test]
    fn a_trigger_says_what_its_fires_did() {
        let fired = now_ms() - 3 * 3600 * 1000;
        let row = json!({"name": "nightly", "when": "cron 0 8 * * *", "bot": "calc.lead",
            "message": "Read the inbox\nthen post", "sent": 4, "runs": 10,
            "reply_to": "calc.lead", "if": null,
            "last": {"outcome": "failed", "fired_ms": fired, "detail": "bot_busy: it works"}});
        let item = trigger_item(&row, vec!["calc.lead".into()]);
        assert_eq!(item.what, "Read the inbox…");
        assert_eq!(item.facts, ["sent 4 of 10", "last failed 3h ago"]);
        assert_eq!(item.failed.as_deref(), Some("busy: it works"));
        assert_eq!(item.reply_to.as_deref(), Some("calc.lead"));
        // A later fire that failed the same way, sending no turn, is news.
        let mut later = row.clone();
        later["last"]["fired_ms"] = json!(fired + 86_400_000);
        let later = trigger_item(&later, vec!["calc.lead".into()]);
        assert_ne!(failures(std::slice::from_ref(&item)), failures(&[later]));
        let fresh = trigger_item(
            &json!({"name": "new", "when": "fire", "bot": "calc.lead", "sent": 0, "last": null}),
            vec!["calc.lead".into()],
        );
        assert_eq!(fresh.facts, ["not run yet"]);
        assert_eq!(fresh.failed, None);
    }

    #[test]
    fn a_questions_reminder_reads_as_the_question_and_its_card_not_its_gate() {
        // Two hours on, at a whole minute, as Sonny sets a deadline.
        let now = (now_ms() / 1000) as i64;
        let at = (now + 2 * 3600) / 60 * 60;
        let local = crate::trigger::local(at);
        let when = format!(
            "at {}-{:02}-{:02} {:02}:{:02}",
            local.year, local.month, local.day, local.hour, local.minute
        );
        let question = "Round half cents up, or to even?";
        let row = json!({"name": "calc.halves.ask-10-a11", "when": when, "bot": "calc.halves",
            "message": crate::asks::fire(question, crate::asks::Unanswered::Later, Some(0), "Up", false),
            "if": "'/opt/sonny' ask open 'calc.halves' 10 'toolu_a11'", "sent": 0, "last": null});
        let item = trigger_item(&row, vec!["calc.halves".into()]);
        assert!(item.reminder.is_some());
        let mut view = View {
            scope: Scope {
                project: "calc".into(),
                lead: "calc.lead".into(),
                glob: "calc.*".into(),
            },
            store: std::env::temp_dir().join("background-reminder.sqlite"),
            store_id: None,
            bot: "calc.lead".into(),
            items: vec![item],
            turns: HashMap::new(),
            lookout: String::new(),
            icons: HashMap::new(),
            selected: 0,
            confirm: None,
            commands: Commands::default(),
            stopping: None,
            notice: None,
            page: None,
            tasks: Arc::default(),
            follow: Follow::default(),
            next_run: None,
            source: SourcePage::default(),
            output: Output::default(),
        };
        let clock = crate::asks::clock(at, now);
        // The pick reaches a column past the last word.
        let lines: Vec<String> = (view.card(100).0.iter())
            .map(|l| l.to_string().trim_end().to_owned())
            .collect();
        let row = lines
            .iter()
            .position(|l| l.contains("question from"))
            .unwrap();
        assert_eq!(
            lines[row..row + 3],
            [
                format!("  ↻ question from halves  {clock} → halves"),
                format!("    Goes with 1 at {clock} unless you pick."),
                format!("    └ {question}"),
            ]
        );
        // No gate, no install path, no internal name.
        assert!(
            !lines
                .iter()
                .any(|l| l.contains("ask open") || l.contains("ask-10"))
        );
        // It goes once the question is answered: no run, no removal.
        let keys = view.keys();
        assert!(
            !keys.contains("run now") && !keys.contains("remove"),
            "{keys}"
        );
        let key = |view: &mut View, code| {
            view.key(KeyEvent::new(code, KeyModifiers::NONE), 0..usize::MAX)
        };
        key(&mut view, KeyCode::Char('x'));
        assert!(view.confirm.is_none());
        assert_eq!(
            view.notice.as_deref(),
            Some("it goes once the question is answered")
        );
        key(&mut view, KeyCode::Char('r'));
        assert_eq!(
            view.notice.as_deref(),
            Some("a question's reminder runs at its time")
        );
        // `enter` opens its page: whose question, when, what happens, and
        // the question; the agent that asked is the first link.
        key(&mut view, KeyCode::Enter);
        let (page, _, _) = view.page_lines(60);
        let page: Vec<String> = page.iter().map(|l| l.to_string()).collect();
        assert_eq!(page[0], "a question's reminder");
        assert_eq!(page[1], format!("question from halves · {clock}"));
        assert!(page.contains(&format!("Goes with 1 at {clock} unless you pick.")));
        assert!(page.contains(&question.to_owned()));
        assert!(!page.iter().any(|l| l.contains("This is Sonny")));
        assert_eq!(
            view.page.as_ref().unwrap().links[0].3,
            Target::Agent("calc.halves".into())
        );
    }

    #[test]
    fn a_cleanup_sonny_could_not_schedule_is_a_failure_in_every_window() {
        let home = std::env::temp_dir().join(format!("bg-unscheduled-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&home);
        std::fs::create_dir_all(home.join(".agent/sonny")).unwrap();
        assert_eq!(unscheduled(trigger::cleanup_unscheduled(&home)), None);
        let at = now_ms() / 1000 - 120;
        std::fs::write(
            home.join(".agent/sonny/cleanup-not-scheduled.json"),
            json!({"why": "automations_unsupported: no scheduler here", "at": at}).to_string(),
        )
        .unwrap();
        let item = unscheduled(trigger::cleanup_unscheduled(&home)).unwrap();
        assert_eq!(item.kind, Kind::Setup);
        assert_eq!(
            item.failed.as_deref(),
            Some("automations_unsupported: no scheduler here")
        );
        assert_eq!(
            item.facts,
            ["tried 2m ago", "Sonny tries again at its next start"]
        );
        // The person's, so every window shows it.
        let scope = Scope {
            project: "calc".into(),
            lead: "calc.lead".into(),
            glob: "calc.*".into(),
        };
        assert!(shown(&scope, &item));
        assert_eq!(failures(std::slice::from_ref(&item)).len(), 1);
        std::fs::remove_dir_all(home).unwrap();
    }

    #[test]
    fn a_check_that_found_nothing_is_healthy_and_one_that_could_not_is_a_failure() {
        let at = now_ms() - 5 * 60 * 1000;
        let row = |checked: Value, last: Value| {
            json!({"name": "triage", "when": "every 10m", "bot": "p.triage",
                "message": "Run the triage", "sent": 3, "if": "cd /w && ! ./quiet",
                "last": last, "checked": checked})
        };
        let sent = json!({"outcome": "sent", "fired_ms": at - 3_600_000});
        // Checked, nothing new: no failure, and it says when it looked.
        let quiet = trigger_item(
            &row(json!({"gate": "no", "checked_ms": at}), sent.clone()),
            vec!["p.triage".into()],
        );
        assert_eq!(quiet.failed, None);
        assert_eq!(
            quiet.facts,
            [
                "sent 3",
                "last sent 1h ago",
                "checked 5m ago, nothing new",
                "if cd /w && ! ./quiet"
            ]
        );
        // Could not check: a failure, since the first check that could not.
        let broke = json!({"outcome": "failed", "gate": "failed", "fired_ms": at,
            "detail": "--if: exit status: 2: quiet: token expired"});
        // Each failed check settles as the last fire, at its own time.
        let checking = |checked_ms: u64| {
            let mut last = broke.clone();
            last["fired_ms"] = json!(checked_ms);
            trigger_item(
                &row(
                    json!({"gate": "failed", "checked_ms": checked_ms, "since_ms": at - 600_000,
                        "detail": "--if: exit status: 2: quiet: token expired"}),
                    last,
                ),
                vec!["p.triage".into()],
            )
        };
        let unhealthy = checking(at);
        assert_eq!(
            unhealthy.failed.as_deref(),
            Some("--if: exit status: 2: quiet: token expired")
        );
        assert_eq!(
            unhealthy.facts[..2],
            ["sent 3", "could not check since 15m ago"]
        );
        // Each check that fails again is the same break, not news, though
        // what it printed changed.
        let mut again = checking(at + 60_000);
        assert_eq!(
            failures(std::slice::from_ref(&unhealthy)),
            failures(std::slice::from_ref(&again))
        );
        again.failed = Some("--if: exit status: 2: quiet: request 7f3a timed out".into());
        assert_eq!(
            failures(std::slice::from_ref(&unhealthy)),
            failures(&[again])
        );
        // A check that answers again ends it, though no run went since.
        let back = trigger_item(
            &row(json!({"gate": "no", "checked_ms": at + 120_000}), broke),
            vec!["p.triage".into()],
        );
        assert_eq!(back.failed, None);
        assert_eq!(back.facts[..2], ["sent 3", "checked 3m ago, nothing new"]);
        // Its `--if` removed since: what the old one said last is not its
        // health now.
        let mut ungated = row(
            json!({"gate": "failed", "checked_ms": at, "since_ms": at,
                "detail": "--if: exit status: 2"}),
            sent,
        );
        ungated["if"] = Value::Null;
        let ungated = trigger_item(&ungated, vec!["p.triage".into()]);
        assert_eq!(ungated.failed, None);
        assert_eq!(ungated.facts, ["sent 3", "last sent 1h ago"]);
        // An `--if` its next fire could not run is a failure before that
        // fire finds it, one however often its row is read.
        let mut stranded = row(json!({"gate": "no", "checked_ms": at}), Value::Null);
        stranded["if_cannot_run"] = json!("--if: Agent's binary was not found at /gone/agent");
        let stranded = trigger_item(&stranded, vec!["p.triage".into()]);
        assert_eq!(
            stranded.failed.as_deref(),
            Some("--if: Agent's binary was not found at /gone/agent")
        );
        assert!(
            stranded
                .facts
                .iter()
                .any(|fact| fact == "its --if cannot run")
        );
        assert_eq!(failures(std::slice::from_ref(&stranded)).len(), 1);
    }

    #[test]
    fn a_fire_that_went_out_fails_when_its_turn_did() {
        let row = |handle: &str| {
            json!({"name": "nightly", "when": "cron 0 5 * * *", "bot": "memory-cleanup",
                "message": "Clean up memory", "sent": 3,
                "last": {"outcome": "sent", "handle": handle, "fired_ms": now_ms()}})
        };
        let turns = HashMap::from([
            (
                "h1".to_owned(),
                json!({"status": "failed", "error": "provider_failed", "detail": "login expired\nsign in again"}),
            ),
            ("h2".to_owned(), json!({"status": "interrupted"})),
            (
                "h3".to_owned(),
                json!({"status": "completed", "text": "No changes."}),
            ),
        ]);
        let ended = |handle: &str| {
            let mut row = row(handle);
            trigger::with_turn(&mut row, &turns);
            trigger_item(&row, vec!["memory-cleanup".into()])
        };
        let failed = ended("h1");
        assert_eq!(
            failed.failed.as_deref(),
            Some("provider_failed: login expired…")
        );
        assert_eq!(
            failed.facts,
            ["sent 3", "last sent 0s ago", "its turn failed"]
        );
        assert_eq!(failed.handle.as_deref(), Some("h1"));
        // Stopped by the person: how it ended, not a failure.
        let stopped = ended("h2");
        assert_eq!(stopped.failed, None);
        assert_eq!(stopped.facts[2], "its turn interrupted");
        assert_eq!(ended("h3").failed, None);
        // Its answer did not reach the agent it was for.
        let mut lost = row("h3");
        lost["reply_to"] = json!("calc.lead");
        lost["last"]["reply"] = json!({"outcome": "failed", "detail": "bot_not_found: calc.lead"});
        trigger::with_turn(&mut lost, &turns);
        let lost = trigger_item(&lost, vec!["memory-cleanup".into()]);
        assert_eq!(lost.failed.as_deref(), Some("agent not found: calc.lead"));
        assert_eq!(lost.facts[2], "its answer was not passed on");
        // Not asked yet: the window asks the daemon for it.
        let running = ended("h4");
        assert_eq!(running.failed, None);
        assert_eq!(unended(&[running, failed], &turns), ["h4"]);
    }

    #[test]
    fn what_fails_for_the_person_shows_in_every_window_and_a_server_by_its_fact() {
        let scope = Scope {
            project: "calc".into(),
            lead: "calc.lead".into(),
            glob: "calc.*".into(),
        };
        let mut cleanup = item(Kind::Trigger, "memory", &["memory-cleanup"], false);
        assert!(!shown(&scope, &cleanup));
        cleanup.failed = Some("provider_failed".into());
        assert!(shown(&scope, &cleanup));
        let mut other = item(Kind::Trigger, "digest", &["web.lead"], false);
        other.failed = Some("bot_busy".into());
        assert!(!shown(&scope, &other));
        assert!(shown(
            &scope,
            &item(Kind::Trigger, "nightly", &["calc.lead"], false)
        ));

        let root = std::env::temp_dir().join(format!("background-servers-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        let fact = |dir: &Path, name: &str, description: &str| {
            std::fs::create_dir_all(dir).unwrap();
            std::fs::write(
                dir.join(format!("{name}.md")),
                format!("---\nname: {name}\ndescription: {description}\ntype: reference\nsource: memory-cleanup\nverified: 2026-10-30\n---\n\nsince 10-29\n"),
            )
            .unwrap();
        };
        fact(
            &root,
            "unreadable-chat",
            "chat server: auth_expired since 2026-10-29",
        );
        fact(&root, "prefers-short-replies", "George wants short replies");
        fact(
            &root.join("projects/calc"),
            "unreadable-mail",
            "mail server: timeout",
        );
        fact(
            &root.join("projects/web"),
            "unreadable-tracker",
            "tracker: 401",
        );
        let found = servers(&agent_client::memory::view(&root, Some("calc")));
        let names: Vec<&str> = found.iter().map(|i| i.name.as_str()).collect();
        assert_eq!(names, ["chat", "mail"]);
        assert_eq!(
            found[0].failed.as_deref(),
            Some("chat server: auth_expired since 2026-10-29")
        );
        assert_eq!(found[0].facts[0], "memory fact unreadable-chat");
        assert_eq!(found[1].facts[0], "memory fact unreadable-mail in calc");
        assert!(
            found
                .iter()
                .all(|i| i.kind == Kind::Server && i.bots.is_empty())
        );
        assert_eq!(super::fact(&found, "calc.lead", &HashMap::new()), "");
        assert_eq!(failures(&found).len(), 2);
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn the_card_lists_what_failed_first_marked_with_its_error() {
        let dir = std::env::temp_dir().join(format!("background-failed-{}", std::process::id()));
        let mut broken = item(Kind::Trigger, "memory", &["memory-cleanup"], false);
        broken.failed = Some("provider_failed: login expired".into());
        let mut server = item(Kind::Server, "chat", &["calc.lead"], false);
        (server.bot, server.bots, server.what) = (String::new(), Vec::new(), String::new());
        server.facts = vec!["memory fact unreadable-chat".into()];
        server.failed = Some("chat server: auth_expired".into());
        let mut view = View {
            scope: Scope {
                project: "calc".into(),
                lead: "calc.lead".into(),
                glob: "calc.*".into(),
            },
            store: dir.join("state.sqlite"),
            store_id: None,
            bot: "calc.lead".into(),
            items: vec![
                broken,
                server,
                item(Kind::Trigger, "nightly", &["calc.lead"], false),
            ],
            turns: HashMap::new(),
            lookout: String::new(),
            icons: HashMap::new(),
            selected: 1,
            confirm: None,
            commands: Commands::default(),
            stopping: None,
            notice: None,
            page: None,
            tasks: Arc::default(),
            follow: Follow::default(),
            next_run: None,
            source: SourcePage::default(),
            output: Output::default(),
        };
        let (card, at) = view.card(100);
        let lines: Vec<String> = card.iter().map(|l| l.to_string()).collect();
        assert_eq!(lines[2], "Failed · 2");
        assert_eq!(lines[3], "  ✗ memory every 1d → memory-cleanup");
        let mark = card[3]
            .spans
            .iter()
            .find(|s| s.content.starts_with('✗'))
            .unwrap();
        assert_eq!(mark.style.fg, Some(Color::Red));
        assert_eq!(lines[4], "    provider_failed: login expired");
        assert_eq!(
            card[4].style.add_modifier,
            ratatui::style::Modifier::empty()
        );
        assert_eq!(
            lines[7],
            "  ✗ chat could not be read by the nightly memory cleanup "
        );
        assert_eq!(at, 7);
        assert_eq!(lines[8], "    chat server: auth_expired");
        assert_eq!(lines[9], "    memory fact unreadable-chat");
        assert_eq!(lines[11], "Automations · 1");
        assert_eq!(view.keys(), "↑↓ pick · esc close");
        assert_eq!(view.footer(), None);
        let press = |view: &mut View, code| {
            view.key(KeyEvent::new(code, KeyModifiers::NONE), 0..usize::MAX)
        };
        press(&mut view, KeyCode::Char('x'));
        assert_eq!(view.confirm, None);
        assert!(view.notice.as_deref().unwrap().contains("reads again"));
    }

    #[test]
    fn the_card_lists_triggers_then_subscriptions_and_x_asks_once() {
        let dir = std::env::temp_dir().join(format!("background-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let store = dir.join("state.sqlite");
        let mut view = View {
            scope: Scope {
                project: "calc".into(),
                lead: "calc.lead".into(),
                glob: "calc.*".into(),
            },
            store,
            store_id: None,
            bot: "calc.lead".into(),
            items: vec![
                item(Kind::Trigger, "nightly", &["calc.lead"], false),
                item(Kind::Subscription, "tasks", &["calc.lead"], true),
            ],
            turns: HashMap::new(),
            lookout: "Lookout · not running".into(),
            icons: HashMap::new(),
            selected: 0,
            confirm: None,
            commands: Commands::default(),
            stopping: None,
            notice: None,
            page: None,
            tasks: Arc::default(),
            follow: Follow::default(),
            next_run: None,
            source: SourcePage::default(),
            output: Output::default(),
        };
        let text = |view: &View| -> Vec<String> {
            view.card(100)
                .0
                .iter()
                .map(|l| l.spans.iter().map(|s| s.content.as_ref()).collect())
                .collect()
        };
        let lines = text(&view);
        assert_eq!(lines[0], "calc");
        assert_eq!(lines[3], "Automations · 1");
        assert_eq!(lines[4], "  ↻ nightly every 1d → lead ");
        assert_eq!(lines[5], "    sent 2");
        assert_eq!(lines[6], "    └ Read preferences.md");
        assert_eq!(lines[8], "Follows · 1");
        assert_eq!(lines[9], "  ◎ tasks for lead");
        // Enter opens the automation as a page, the agent it wakes its
        // first link, picked; it closes nothing.
        let enter = KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE);
        assert_eq!(
            view.keys(),
            "↑↓ pick · enter open · r run now · x remove · esc close"
        );
        assert_eq!(view.key(enter, 0..usize::MAX), Key::Stay);
        assert!(view.on_page());
        let (page, at, marks) = view.page_lines(100);
        let page: Vec<String> = page.iter().map(|l| l.to_string()).collect();
        assert_eq!(
            page,
            [
                "nightly",
                "automation for lead",
                "",
                "when  every 1d",
                "last  not run yet",
                "",
                "Read preferences.md",
            ]
        );
        assert_eq!((at, marks.len()), (1, 1));
        assert_eq!(view.keys(), "enter open · r run now · x remove · esc close");
        // Enter there goes to the agent, as a click on its name does.
        assert_eq!(
            view.key(enter, 0..usize::MAX),
            Key::Open("sonny:agent/calc.lead".into())
        );
        assert_eq!(view.click("link/0"), Some("sonny:agent/calc.lead".into()));
        // `←` goes back to the list, the pick where it was.
        let left = KeyEvent::new(KeyCode::Left, KeyModifiers::NONE);
        assert_eq!(view.key(left, 0..usize::MAX), Key::Stay);
        assert!(!view.on_page());
        assert_eq!(view.selected, 0);
        let press = |view: &mut View, code| {
            view.key(KeyEvent::new(code, KeyModifiers::NONE), 0..usize::MAX)
        };
        // Ctrl+X is not x.
        view.key(
            KeyEvent::new(KeyCode::Char('x'), KeyModifiers::CONTROL),
            0..usize::MAX,
        );
        assert_eq!(view.confirm, None);
        assert_eq!(press(&mut view, KeyCode::Char('x')), Key::Stay);
        assert_eq!(
            view.confirm,
            Some((Kind::Trigger, "calc.lead".into(), "nightly".into()))
        );
        // It asks once, in words that name it; only Enter says yes.
        assert_eq!(
            view.footer().unwrap().to_string(),
            "Remove nightly? It can't be undone."
        );
        assert_eq!(view.keys(), "enter remove · esc cancel");
        // A key the pane took instead keeps it.
        view.keep();
        assert_eq!(view.confirm, None);
        assert_eq!(press(&mut view, KeyCode::Char('x')), Key::Stay);
        assert!(view.confirm.is_some());
        // A reload that no longer finds it there forgets it.
        view.reload();
        assert_eq!(view.confirm, None);
        view.items = vec![
            item(Kind::Trigger, "nightly", &["calc.lead"], false),
            item(Kind::Subscription, "tasks", &["calc.lead"], true),
        ];
        press(&mut view, KeyCode::Char('x'));
        // Any other key keeps it, x again included, and says so; esc then
        // only cancels.
        assert_eq!(press(&mut view, KeyCode::Char('x')), Key::Stay);
        assert_eq!(view.confirm, None);
        assert_eq!(view.notice.as_deref(), Some("not removed"));
        press(&mut view, KeyCode::Char('x'));
        assert_eq!(press(&mut view, KeyCode::Esc), Key::Stay);
        assert_eq!(view.confirm, None);
        // Enter with a modifier keeps it too; Ctrl+C keeps it and closes.
        for modifier in [KeyModifiers::CONTROL, KeyModifiers::ALT] {
            press(&mut view, KeyCode::Char('x'));
            let enter = KeyEvent::new(KeyCode::Enter, modifier);
            assert_eq!(view.key(enter, 0..usize::MAX), Key::Stay);
            assert_eq!(view.confirm, None);
            assert_eq!(view.items.len(), 2);
            assert_eq!(view.notice.as_deref(), Some("not removed"));
        }
        press(&mut view, KeyCode::Char('x'));
        let close = KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL);
        assert_eq!(view.key(close, 0..usize::MAX), Key::Close);
        assert_eq!(view.confirm, None);
        press(&mut view, KeyCode::Down);
        assert_eq!(view.card(100).1, 9);
        press(&mut view, KeyCode::Char('x'));
        assert_eq!(view.confirm, None);
        assert!(view.notice.as_deref().unwrap().starts_with("Sonny keeps"));
        assert_eq!(press(&mut view, KeyCode::Esc), Key::Close);
        // The commands the agents left running come last, picked after
        // the items: Enter opens a row's agent, x asks once to stop it.
        assert_eq!(view.commands_query(), json!({"bot": "calc.*"}));
        assert!(view.commands_due());
        view.commands_loaded(Ok(json!({"processes": [
            {"handle": "proc:4", "bot": "calc.lead", "kind": "background",
             "command": "cargo build", "started_ms": 1_000, "pid": null},
            {"handle": "proc:9", "bot": "calc.fix", "kind": "detached",
             "command": "npm run dev\nmore", "started_ms": 1_000, "pid": 4242},
        ]})));
        assert!(!view.commands_due());
        let lines: Vec<String> = (view.card_at(80, 1_000 + 125_000).0.iter())
            .map(|l| l.spans.iter().map(|s| s.content.as_ref()).collect())
            .collect();
        let at = lines.iter().position(|l| l == "Commands · 2").unwrap();
        assert_eq!(lines[at - 1], "");
        assert!(lines[at + 1].contains(" 2m lead") && lines[at + 1].contains("cargo build"));
        assert!(lines[at + 2].contains("fix") && lines[at + 2].contains("detached pid 4242"));
        assert_eq!(at + 3, lines.len(), "the commands come last");
        press(&mut view, KeyCode::Down);
        assert_eq!(view.card_at(80, 0).1, at + 1);
        assert_eq!(view.keys(), "↑↓ pick · enter open · x stop · esc close");
        // Enter opens the command as a page: its whole line, and who
        // left it running as its first link.
        press(&mut view, KeyCode::Down);
        assert_eq!(press(&mut view, KeyCode::Enter), Key::Stay);
        let page: Vec<String> = (view.page_lines(44).0.iter())
            .map(|l| l.to_string())
            .collect();
        assert_eq!(page[0], "npm run dev more");
        // Under the name: its kind, who left it running, for how long,
        // its pid.
        assert!(
            page[1].starts_with("detached service · left running by fix"),
            "{page:?}"
        );
        assert!(page[2].ends_with("· pid 4242"), "{page:?}");
        assert_eq!(view.keys(), "enter open · x stop · esc close");
        // Its newest lines are asked for while its page is open, one read
        // out at a time.
        let (query, output) = view.output_query().unwrap();
        assert_eq!(
            query,
            json!({"bot": "calc.fix", "handle": "proc:9", "lines": 10})
        );
        assert!(view.output_query().is_none(), "one read out at a time");
        let home = std::env::var("HOME").unwrap_or_default();
        output.came(
            "proc:9",
            Some(json!({"folder": format!("{home}/web"), "lines": null})),
        );
        let page: Vec<String> = (view.page_lines(44).0.iter())
            .map(|l| l.to_string())
            .collect();
        assert_eq!(page[4], "folder  ~/web", "{page:?}");
        assert_eq!(page[5], "output  not kept", "{page:?}");
        let (_, output) = view.output_query().unwrap();
        output.came("proc:9", Some(json!({"folder": "/srv/web", "lines": []})));
        let page: Vec<String> = (view.page_lines(44).0.iter())
            .map(|l| l.to_string())
            .collect();
        assert_eq!(page[5], "output  nothing yet", "{page:?}");
        let (_, output) = view.output_query().unwrap();
        output.came(
            "proc:9",
            Some(json!({"folder": "/srv/web", "lines": ["ready in 412ms", "GET / 200"]})),
        );
        let page: Vec<String> = (view.page_lines(44).0.iter())
            .map(|l| l.to_string())
            .collect();
        assert_eq!(
            page[4..7],
            [
                "folder  /srv/web",
                "output  ready in 412ms",
                "        GET / 200"
            ]
        );
        // It stays on its command as the commands are read again, and
        // closes once the command ends.
        view.commands_loaded(Ok(json!({"processes": [
            {"handle": "proc:9", "bot": "calc.fix", "kind": "detached",
             "command": "npm run dev\nmore", "started_ms": 1_000, "pid": 4242},
        ]})));
        // The items read again, however many there are now, it stays on
        // its command too.
        let items = view.items.clone();
        view.reload();
        assert_eq!(view.command().unwrap()["handle"], "proc:9");
        view.items = items;
        view.commands_loaded(Ok(json!({"processes": [
            {"handle": "proc:9", "bot": "calc.fix", "kind": "detached",
             "command": "npm run dev\nmore", "started_ms": 1_000, "pid": 4242},
        ]})));
        assert_eq!(
            press(&mut view, KeyCode::Enter),
            Key::Open("sonny:agent/calc.fix".into())
        );
        view.commands_loaded(Ok(json!({"processes": []})));
        assert!(!view.on_page());
        assert_eq!(view.footer().unwrap().to_string(), "npm run dev more ended");
        view.commands_loaded(Ok(json!({"processes": [
            {"handle": "proc:4", "bot": "calc.lead", "kind": "background",
             "command": "cargo build", "started_ms": 1_000, "pid": null},
            {"handle": "proc:9", "bot": "calc.fix", "kind": "detached",
             "command": "npm run dev\nmore", "started_ms": 1_000, "pid": 4242},
        ]})));
        press(&mut view, KeyCode::Down);
        press(&mut view, KeyCode::Down);
        assert_eq!(press(&mut view, KeyCode::Char('x')), Key::Stay);
        assert_eq!(
            view.footer().unwrap().to_string(),
            "Stop npm run dev more and all it started? It can't be undone."
        );
        assert_eq!(view.keys(), "enter stop · esc cancel");
        assert_eq!(press(&mut view, KeyCode::Esc), Key::Stay, "esc cancels");
        assert_eq!(view.footer().unwrap().to_string(), "not stopped");
        // Enter with a modifier keeps it running too; Ctrl+C keeps it and
        // closes.
        for modifier in [KeyModifiers::CONTROL, KeyModifiers::ALT] {
            press(&mut view, KeyCode::Char('x'));
            let enter = KeyEvent::new(KeyCode::Enter, modifier);
            assert_eq!(view.key(enter, 0..usize::MAX), Key::Stay);
            assert_eq!(view.footer().unwrap().to_string(), "not stopped");
        }
        press(&mut view, KeyCode::Char('x'));
        let close = KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL);
        assert_eq!(view.key(close, 0..usize::MAX), Key::Close);
        assert_ne!(view.keys(), "enter stop · esc cancel");
        press(&mut view, KeyCode::Char('x'));
        assert_eq!(
            press(&mut view, KeyCode::Enter),
            Key::Stop {
                bot: "calc.fix".into(),
                handle: "proc:9".into()
            }
        );
        let came = view.stopping("proc:9");
        assert_eq!(view.footer().unwrap().to_string(), "stopping proc:9…");
        *came.lock().unwrap() = Some("stopped proc:9".into());
        assert!(
            view.commands_due(),
            "a stop that came to something reads at once"
        );
        view.commands_loaded(Ok(json!({"processes": []})));
        assert_eq!(view.notice.as_deref(), Some("stopped proc:9"));
        assert_eq!(view.selected, 1, "the pick stays on a row that is there");
        view.commands_loaded(Err("daemon down".into()));
        let lines: Vec<String> = (view.card(80).0.iter())
            .map(|l| l.spans.iter().map(|s| s.content.as_ref()).collect())
            .collect();
        assert_eq!(
            lines.last().unwrap(),
            "✗ could not read what runs: daemon down"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }
}
