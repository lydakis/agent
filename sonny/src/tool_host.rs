//! `sonny tool-host`: the daemon's companion that answers the tools Sonny
//! gives its agents (`served`): starting tasks, memory, the commons,
//! and `exec`, whose MCP servers it keeps up between calls. One a store,
//! apart from the lookout, which only watches and tells: a tool that runs
//! long, crashes or grows does so here, never in the news a lead hears.

use agent_client::Client;
use serde_json::{Value, json};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

use crate::lookout::{beside, hold, identity, replace};
use crate::served::Server;

/// What the daemon and this companion call it.
pub const NAME: &str = "tool-host";

const USAGE: &str = "usage: sonny tool-host [--store PATH] [--socket PATH]";

/// How long the daemon may be gone before the host is too: the client
/// that starts the next daemon starts its host.
const GONE_AFTER: Duration = Duration::from_secs(30);

/// The events that tell the host a call waits, or that a turn whose calls
/// it runs ended. The calls themselves it reads from the daemon.
const KINDS: [&str; 4] = [
    "tool_started",
    "turn_finished",
    "call_lifted",
    "lifted_ended",
];

/// How often the host's place in the stream is saved, when it moved: every
/// tool call of every agent moves it, so saving each would write a file a
/// call. The calls waiting are read from the daemon whenever the host
/// starts, so starting again a moment back only reads a few events twice.
const SAVE_EVERY: Duration = Duration::from_secs(1);

/// `sonny tool-host`: one tool host for the store's daemon.
pub fn run(args: &[String]) -> Result<(), String> {
    let (mut store, mut socket) = (None, None);
    let mut args = args.iter();
    while let Some(arg) = args.next() {
        let mut value = || args.next().cloned().ok_or(USAGE);
        match arg.as_str() {
            "--store" => store = Some(PathBuf::from(value()?)),
            "--socket" => socket = Some(PathBuf::from(value()?)),
            _ => return Err(USAGE.into()),
        }
    }
    let (store, socket) = crate::daemon_paths(store, socket)?;
    let lock = beside(&store, &format!(".{NAME}.lock"));
    let held = match hold(&lock)? {
        Some(held) => Some(held),
        None => replace(&store, NAME)?,
    };
    let Some(_held) = held else {
        println!("{}", json!({"event": "already_serving"}));
        return Ok(());
    };
    let _ = std::fs::write(beside(&store, &format!(".{NAME}.sonny")), identity());
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .map_err(|e| e.to_string())?
        .block_on(serve(store, socket))
}

/// Whether a tool host serves `store`'s daemon: one holds its lock.
pub fn running(store: &Path) -> bool {
    crate::lookout::held(&beside(store, &format!(".{NAME}.lock")))
}

/// The lookout's line on `/background`, told too when no tool host answers
/// the tools Sonny gives its agents, which then wait.
pub fn with_status(lookout: String, store: &Path) -> String {
    match (running(store), lookout.is_empty()) {
        (true, _) => lookout,
        (false, true) => "Tools · not answered".to_owned(),
        (false, false) => format!("{lookout} · tools not answered"),
    }
}

async fn serve(store: PathBuf, socket: PathBuf) -> Result<(), String> {
    println!("{}", json!({"event": "serving", "pid": std::process::id()}));
    // Asked to give way, as by another build's host: the calls it still
    // answers stop first. They were taken, so that one answers that their
    // outcome is unknown and never runs them again.
    if let Ok(mut term) = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
    {
        tokio::spawn(async move {
            term.recv().await;
            crate::served::stop();
            std::process::exit(0);
        });
    }
    let server = Server::start(store.clone(), socket.clone());
    let mut cursor = saved(&store);
    let mut kept = cursor;
    let mut started = true;
    let mut gone: Option<Instant> = None;
    loop {
        if let Ok((client, mut events)) = Client::connect(&socket).await {
            gone = None;
            server.connected(&client);
            // Once a host: a connection dropped and made again changes no
            // bot's tools.
            if started {
                started = false;
                let bots = crate::lookout::all_bots(&client).await;
                tokio::spawn(refresh_tools(client.clone(), bots));
            }
            // For as long as this connection lasts.
            let following = tokio::spawn(follow_config(client.clone()));
            let follow = json!({"bot": "*", "after": cursor, "kinds": KINDS});
            match client.request("follow", follow).await {
                Ok(_) => {
                    let mut tick = tokio::time::interval(SAVE_EVERY);
                    tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
                    loop {
                        tokio::select! {
                            event = events.recv() => {
                                let Some(event) = event else { break };
                                server.event(&event);
                                if let Some(at) = event["cursor"].as_i64()
                                    && at > cursor
                                {
                                    cursor = at;
                                }
                            }
                            _ = tick.tick() => keep(&store, cursor, &mut kept),
                        }
                    }
                    keep(&store, cursor, &mut kept);
                    println!("{}", json!({"event": "disconnected", "cursor": cursor}));
                }
                Err(error) => println!(
                    "{}",
                    json!({"event": "follow_refused", "error": error.code, "after": cursor})
                ),
            }
            server.disconnected();
            following.abort();
        }
        if gone.get_or_insert_with(Instant::now).elapsed() >= GONE_AFTER {
            println!("{}", json!({"event": "daemon_gone"}));
            // As when asked to give way: no call or MCP server goes on
            // behind a host that has left.
            crate::served::stop();
            return Ok(());
        }
        tokio::time::sleep(Duration::from_secs(2)).await;
    }
}

/// Where the host's stream starts again: after the last event it saw, or,
/// the first time, where the lookout's does, so it never reads the whole
/// log. The calls waiting meanwhile it reads from the daemon either way.
fn saved(store: &Path) -> i64 {
    let read = |suffix: &str| -> Option<i64> {
        (std::fs::read_to_string(beside(store, suffix)).ok())?
            .split_whitespace()
            .next()?
            .parse()
            .ok()
    };
    read(&format!(".{NAME}.cursor"))
        .or_else(|| read(".lookout.cursor"))
        .unwrap_or(0)
}

/// Save `cursor` unless it is what was `kept` last.
fn keep(store: &Path, cursor: i64, kept: &mut i64) {
    if cursor != *kept {
        save(store, cursor);
        *kept = cursor;
    }
}

fn save(store: &Path, cursor: i64) {
    let path = beside(store, &format!(".{NAME}.cursor"));
    let partial = path.with_extension(format!("cursor.{}.partial", std::process::id()));
    if std::fs::write(&partial, cursor.to_string()).is_ok() {
        let _ = std::fs::rename(&partial, &path);
    }
}

/// Bring every agent Sonny declared on up to this build's tools, as
/// opening its window would, so one no one opens after an upgrade, such
/// as a task out of sight, gets a tool added since. A busy one is tried
/// again each minute, for an hour.
async fn refresh_tools(client: Arc<Client>, bots: Vec<Value>) {
    let mut left = bots;
    for _ in 0..60 {
        let mut busy = Vec::new();
        for bot in left {
            let Ok(mut record) = client
                .request(
                    "inspect",
                    json!({"bot": bot["name"], "bot_id": bot["bot_id"]}),
                )
                .await
            else {
                continue;
            };
            record["name"] = bot["name"].clone();
            record["bot_id"] = bot["bot_id"].clone();
            let Some(fields) = crate::declared::refresh(&record) else {
                // Said, not skipped in silence: the agent goes on without
                // the tools added since.
                let declared = record["declared"].as_array().cloned().unwrap_or_default();
                let role = crate::declared::declared_role(&declared);
                if let Some(why) = crate::declared::too_many(&record, role) {
                    println!(
                        "{}",
                        json!({"event": "tools_refresh_failed", "bot": bot["name"], "error": "tools_too_large", "why": why})
                    );
                }
                continue;
            };
            match client.request("configure", Value::Object(fields)).await {
                Ok(_) => println!(
                    "{}",
                    json!({"event": "tools_refreshed", "bot": bot["name"]})
                ),
                Err(error) if error.code == "bot_busy" => busy.push(bot),
                Err(error) => println!(
                    "{}",
                    json!({"event": "tools_refresh_failed", "bot": bot["name"], "error": error.code})
                ),
            }
        }
        if busy.is_empty() {
            return;
        }
        left = busy;
        tokio::time::sleep(Duration::from_secs(60)).await;
    }
}

/// How often the host looks at `config.toml`, and asks a busy agent
/// again: an agent takes a change at the first idle point after, and the
/// command rules at its next round.
const CONFIG_EVERY: Duration = Duration::from_secs(5);

/// Keep Sonny's agents on what `config.toml` gives an agent when it is
/// made: each model's fallback and whether senders are named, and on the
/// command rules in `~/.agent/rules`. Read at first and whenever either
/// changes. The daemon takes the rules alone from an agent at work, for
/// its next round; the rest it is asked again until it is idle, since
/// `configure` otherwise waits for the turn to end.
async fn follow_config(client: Arc<Client>) {
    let stamp = || {
        let found = agent_client::config::path().and_then(|path| std::fs::metadata(path).ok());
        found.map(|found| (found.len(), found.modified().ok()))
    };
    let mut seen = None;
    let mut waiting: Vec<Value> = Vec::new();
    let mut live = std::collections::HashSet::new();
    // What the machines linked here lend, which their agents' tools say.
    let mut lent = crate::link::stamp();
    loop {
        let now_lent = crate::link::stamp();
        if now_lent != lent {
            lent = now_lent;
            let bots = crate::lookout::all_bots(&client).await;
            tokio::spawn(refresh_tools(client.clone(), bots));
        }
        let rules = crate::rules::given();
        let now = (stamp(), rules.clone());
        if seen.as_ref() != Some(&now) {
            seen = Some(now);
            waiting = crate::lookout::all_bots(&client).await;
            live = (waiting.iter())
                .filter_map(|bot| bot["bot_id"].as_i64())
                .collect();
        }
        if !waiting.is_empty() {
            match agent_client::config::load() {
                Ok(config) => {
                    let given = Given {
                        config: &config,
                        rules: rules.as_ref(),
                        live: &live,
                    };
                    waiting = apply_config(&client, &given, waiting).await;
                }
                // Its next change is read again.
                Err(error) => {
                    println!("{}", json!({"event": "config_unread", "error": error}));
                    waiting.clear();
                }
            }
        }
        tokio::time::sleep(CONFIG_EVERY).await;
    }
}

/// What Sonny gives an agent it makes, that the agent follows after.
struct Given<'a> {
    config: &'a agent_client::config::Config,
    rules: Option<&'a agent_client::rules::Policy>,
    /// The bots there are, by id: one whose creator is among them goes by
    /// that creator's rules.
    live: &'a std::collections::HashSet<i64>,
}

/// Configure each of `bots` that `given` would make otherwise; those at
/// work, to ask again.
async fn apply_config(client: &Client, given: &Given<'_>, bots: Vec<Value>) -> Vec<Value> {
    let mut busy = Vec::new();
    for bot in bots {
        let Ok(mut record) = client
            .request(
                "inspect",
                json!({"bot": bot["name"], "bot_id": bot["bot_id"]}),
            )
            .await
        else {
            continue;
        };
        record["name"] = bot["name"].clone();
        let Some(mut fields) = configured(&record, given) else {
            continue;
        };
        fields.insert("bot".into(), bot["name"].clone());
        fields.insert("bot_id".into(), bot["bot_id"].clone());
        let rules = (fields.get("settings"))
            .and_then(|settings| settings.get("rules"))
            .cloned();
        match client.request("configure", Value::Object(fields)).await {
            Ok(answer) => println!(
                "{}",
                json!({"event": "config_applied", "bot": bot["name"], "changed": answer["changed"]})
            ),
            Err(error) if error.code == "bot_busy" => {
                // The rules alone reach an agent at work, at its next
                // round; the rest waits for it to be idle.
                if let Some(rules) = rules {
                    let alone = json!({"bot": bot["name"], "bot_id": bot["bot_id"],
                        "settings": {"rules": rules}});
                    if let Ok(answer) = client.request("configure", alone).await {
                        println!(
                            "{}",
                            json!({"event": "config_applied", "bot": bot["name"],
                                "changed": answer["changed"]})
                        );
                    }
                }
                busy.push(bot)
            }
            Err(error) => println!(
                "{}",
                json!({"event": "config_failed", "bot": bot["name"], "error": error.code})
            ),
        }
    }
    busy
}

/// The `configure` fields that bring one of Sonny's agents, as `inspect`
/// gives it, to what `given` gives an agent on its model when it is made
/// ([`crate::made`]); none when it is there, or isn't Sonny's. An agent
/// made by another goes by its creator's rules, which the daemon keeps,
/// while that creator is there.
fn configured(record: &Value, given: &Given<'_>) -> Option<serde_json::Map<String, Value>> {
    // Compared with what the agent will be once the daemon takes what it
    // keeps for its turn's end, so a file that undoes a kept change says so.
    let kept;
    let record = match record["configure_pending"].as_object() {
        Some(pending) => {
            kept = taken(record, pending);
            &kept
        }
        None => record,
    };
    let declared = record["declared"].as_array()?;
    if !declared
        .iter()
        .any(|tool| tool["answered_by"] == crate::served::TAG)
    {
        return None;
    }
    let mut fields = serde_json::Map::new();
    let mut settings = serde_json::Map::new();
    // The daemon gives every agent that names no fallback the person's
    // `[fallbacks]` entry for its model, so none of Sonny's names its own:
    // one an older Sonny copied in goes, and the entry reaches it again.
    if !record["fallback"].is_null() {
        fields.insert("fallback".into(), Value::Null);
    }
    let hidden = record["settings"]["hide_senders"]
        .as_bool()
        .unwrap_or(false);
    if hidden != given.config.hide_senders {
        let hide = given.config.hide_senders.then_some(true);
        settings.insert("hide_senders".into(), json!(hide));
    }
    // As long as the config says, or the daemon's own when it says none.
    let hold = given.config.hold_ms.unwrap_or(agent_client::HOLD_MS);
    // None is the daemon's own: a reset kept for the turn's end, say.
    let held = record["settings"]["hold_ms"].as_u64();
    if held.unwrap_or(agent_client::HOLD_MS) != hold {
        settings.insert("hold_ms".into(), json!(given.config.hold_ms));
    }
    // A trigger's shadow copy keeps the rules it was made with: the
    // person's, without what they let run outside the sandbox.
    let shadow =
        (record["name"].as_str()).is_some_and(|name| crate::trigger::shadow_source(name).is_some());
    // One whose maker is gone goes by the person's rules too, but is let
    // out of its sandbox by none it didn't have from its maker.
    let creator = record["created_by_id"].as_i64();
    let rules = match (shadow, creator) {
        (true, _) => json!(crate::trigger::shadow_rules(given.rules)),
        (false, Some(_)) => json!(orphan_rules(given.rules, &record["settings"]["rules"])),
        (false, None) => json!(given.rules),
    };
    let own = creator.is_none_or(|creator| !given.live.contains(&creator));
    if own && record["settings"]["rules"] != rules {
        settings.insert("rules".into(), rules);
    }
    if !settings.is_empty() {
        fields.insert("settings".into(), Value::Object(settings));
    }
    (!fields.is_empty()).then_some(fields)
}

/// The command rules of an agent another agent made, once that maker is
/// gone: the person's, `given`, with only the `allow` rules it `had`
/// already, as its maker's, by their words, and only the program paths
/// it had pinned. So it never runs outside its sandbox what its maker
/// could not, while the rules that ask or refuse, and any `allow` or path
/// the person takes back, still reach it.
fn orphan_rules(
    given: Option<&agent_client::rules::Policy>,
    had: &Value,
) -> Option<agent_client::rules::Policy> {
    use agent_client::rules::{Decision, Policy};
    let had: Policy = serde_json::from_value(had.clone()).unwrap_or_default();
    let mut rules = given?.clone();
    rules.prefix_rules.retain(|rule| {
        rule.decision != Decision::Allow
            || (had.prefix_rules.iter())
                .any(|old| old.decision == Decision::Allow && old.pattern == rule.pattern)
    });
    // A path pinned since lets the name's `allow` rules out by it, so only
    // the paths it had stay; a name left with none is unpinned, where its
    // rules may still ask or refuse but never allow.
    rules.host_executables.retain(|name, paths| {
        let old = had.host_executables.get(name);
        paths.retain(|path| old.is_some_and(|old| old.contains(path)));
        !paths.is_empty()
    });
    Some(rules).filter(|rules| *rules != Policy::default())
}

/// `record` with the `configure` the daemon keeps for its turn's end laid
/// over it, as the daemon lays it: `settings` key by key, the rest whole.
fn taken(record: &Value, pending: &serde_json::Map<String, Value>) -> Value {
    let mut taken = record.clone();
    for (key, value) in pending {
        match (key.as_str(), value) {
            ("settings", Value::Object(settings)) => {
                for (name, value) in settings {
                    taken["settings"][name] = value.clone();
                }
            }
            ("model", Value::String(model)) => {
                if let Some((provider, model)) = model.split_once('/') {
                    taken["provider"] = json!(provider);
                    taken["model"] = json!(model);
                }
            }
            // What `configure` declares, `inspect` reads as declared.
            ("declare", _) => taken["declared"] = value.clone(),
            _ => taken[key] = value.clone(),
        }
    }
    taken
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashSet;
    use std::sync::LazyLock;

    #[test]
    fn an_agent_follows_what_config_gives_its_model() {
        let mut config = agent_client::config::Config::default();
        let agent = |fallback: Value, hidden: bool| {
            json!({"provider": "anthropic", "model": "claude", "fallback": fallback,
                "settings": {"hide_senders": hidden},
                "declared": [{"name": "start_task", "answered_by": crate::served::TAG}]})
        };
        fn given(config: &agent_client::config::Config) -> Given<'_> {
            static NONE: LazyLock<HashSet<i64>> = LazyLock::new(HashSet::new);
            Given {
                config,
                rules: None,
                live: &NONE,
            }
        }
        assert_eq!(
            configured(&agent(Value::Null, false), &given(&config)),
            None
        );
        // The daemon gives the config's fallback, so the agent keeps none.
        config
            .fallbacks
            .insert("anthropic/claude".into(), "anthropic/other".into());
        config.hide_senders = true;
        assert_eq!(
            configured(&agent(Value::Null, false), &given(&config)).map(Value::Object),
            Some(json!({"settings": {"hide_senders": true}}))
        );
        assert_eq!(configured(&agent(Value::Null, true), &given(&config)), None);
        // One an older Sonny copied in goes, whatever the config says.
        assert_eq!(
            configured(&agent(json!("anthropic/other"), true), &given(&config)).map(Value::Object),
            Some(json!({"fallback": null}))
        );
        let config = agent_client::config::Config::default();
        assert_eq!(
            configured(&agent(json!("anthropic/other"), true), &given(&config)).map(Value::Object),
            Some(json!({"fallback": null, "settings": {"hide_senders": null}}))
        );
        // A change the daemon keeps for the turn's end counts as made: a
        // file that takes it back asks again, and one that matches it
        // asks nothing.
        let mut busy = agent(Value::Null, false);
        busy["configure_pending"] = json!({"settings": {"hide_senders": true}});
        assert_eq!(
            configured(&busy, &given(&config)).map(Value::Object),
            Some(json!({"settings": {"hide_senders": null}}))
        );
        let mut config = agent_client::config::Config {
            hide_senders: true,
            ..Default::default()
        };
        assert_eq!(configured(&busy, &given(&config)), None);
        // A hold reset kept for the turn's end, and then a hold of its own
        // in the file: the file's goes over the reset.
        let mut held = agent(Value::Null, true);
        held["settings"]["hold_ms"] = json!(agent_client::HOLD_MS);
        held["configure_pending"] = json!({"settings": {"hold_ms": null}});
        config.hold_ms = Some(5);
        assert_eq!(
            configured(&held, &given(&config)).map(Value::Object),
            Some(json!({"settings": {"hold_ms": 5}}))
        );
        let config = agent_client::config::Config::default();
        // Another client's bot is its own.
        let mut other = agent(json!("x/y"), true);
        other["declared"] = json!([]);
        assert_eq!(configured(&other, &given(&config)), None);
    }

    #[test]
    fn an_agent_follows_the_rules_and_one_made_by_another_its_creators() {
        let config = agent_client::config::Config::default();
        let policy: agent_client::rules::Policy = serde_json::from_value(json!({
            "prefix_rules": [{"pattern": ["git", "push"], "decision": "prompt",
                "source": "default.rules:1"}]
        }))
        .unwrap();
        let live = HashSet::from([7]);
        let ruled = Given {
            config: &config,
            rules: Some(&policy),
            live: &live,
        };
        let unruled = Given {
            config: &config,
            rules: None,
            live: &live,
        };
        let agent = |rules: Value, creator: Value| {
            json!({"provider": "anthropic", "model": "claude", "fallback": null,
                "settings": {"rules": rules}, "created_by_id": creator,
                "declared": [{"name": "start_task", "answered_by": crate::served::TAG}]})
        };
        assert_eq!(
            configured(&agent(Value::Null, Value::Null), &ruled).map(Value::Object),
            Some(json!({"settings": {"rules": policy}}))
        );
        assert_eq!(configured(&agent(json!(policy), Value::Null), &ruled), None);
        // Taken out of the folder, they go from the agent too.
        assert_eq!(
            configured(&agent(json!(policy), Value::Null), &unruled).map(Value::Object),
            Some(json!({"settings": {"rules": null}}))
        );
        // One made by another takes its creator's, which the daemon keeps,
        // and once that creator is gone, Sonny's.
        assert_eq!(configured(&agent(Value::Null, json!(7)), &ruled), None);
        assert_eq!(
            configured(&agent(Value::Null, json!(8)), &ruled).map(Value::Object),
            Some(json!({"settings": {"rules": policy}}))
        );
        // The person's `allow` rules reach it only if its maker had them:
        // the rules that ask or refuse it takes, the ones that let a
        // command out of its sandbox only as it had them already.
        let allowing: agent_client::rules::Policy = serde_json::from_value(json!({
            "prefix_rules": [
                {"pattern": ["git", "push"], "decision": "prompt", "source": "default.rules:1"},
                {"pattern": ["cargo", "test"], "decision": "allow", "source": "default.rules:2"}
            ]
        }))
        .unwrap();
        let allowed = Given {
            rules: Some(&allowing),
            ..ruled
        };
        assert_eq!(
            configured(&agent(Value::Null, json!(8)), &allowed).map(Value::Object),
            Some(json!({"settings": {"rules": policy}}))
        );
        assert_eq!(
            configured(&agent(json!(allowing), json!(8)), &allowed),
            None
        );
        // A root agent is the person's own, and takes them all.
        assert_eq!(
            configured(&agent(Value::Null, Value::Null), &allowed).map(Value::Object),
            Some(json!({"settings": {"rules": allowing}}))
        );
        // A program path the person pins since lets none of its maker's
        // `allow` rules out by it; one it had pinned still does.
        let pinning: agent_client::rules::Policy = serde_json::from_value(json!({
            "prefix_rules": [{"pattern": ["cargo", "test"], "decision": "allow"}],
            "host_executables": {"cargo": ["/opt/bin/cargo", "/usr/bin/cargo"]}
        }))
        .unwrap();
        let pinned = Given {
            rules: Some(&pinning),
            ..ruled
        };
        let had = json!({
            "prefix_rules": [{"pattern": ["cargo", "test"], "decision": "allow"}],
            "host_executables": {"cargo": ["/usr/bin/cargo"]}
        });
        assert_eq!(configured(&agent(had, json!(8)), &pinned), None);
        let unpinned = json!({
            "prefix_rules": [{"pattern": ["cargo", "test"], "decision": "allow"}]
        });
        assert_eq!(configured(&agent(unpinned, json!(8)), &pinned), None);
    }

    #[test]
    fn the_host_starts_where_it_stopped_or_else_where_the_lookout_did() {
        let dir = std::env::temp_dir().join(format!("tool-host-cursor-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let store = dir.join("state.sqlite");
        assert_eq!(saved(&store), 0);
        std::fs::write(beside(&store, ".lookout.cursor"), "40 12").unwrap();
        assert_eq!(saved(&store), 40);
        save(&store, 90);
        assert_eq!(saved(&store), 90);
        // Saved only when it moved.
        let mut kept = 90;
        std::fs::remove_file(beside(&store, &format!(".{NAME}.cursor"))).unwrap();
        keep(&store, 90, &mut kept);
        assert_eq!(saved(&store), 40);
        keep(&store, 95, &mut kept);
        assert_eq!((saved(&store), kept), (95, 95));
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
