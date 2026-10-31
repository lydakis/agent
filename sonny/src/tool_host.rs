//! `sonny tool-host`: the daemon's companion that answers the tools Sonny
//! gives its agents (`served`): starting tasks, memory, goals, the commons,
//! and `exec`, whose MCP servers it keeps up between calls. One a store,
//! apart from the lookout, which only watches and tells: a tool that runs
//! long, crashes or grows does so here, never in the news a lead hears.

use agent_client::Client;
use serde_json::{Value, json};
use std::collections::HashMap;
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
const KINDS: [&str; 2] = ["tool_started", "turn_finished"];

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

/// The lookout's line on `/status`, told too when no tool host answers
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
    // answers stop first, since that one answers them again.
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
    let mut started = true;
    let mut gone: Option<Instant> = None;
    loop {
        if let Ok((client, mut events)) = Client::connect(&socket).await {
            gone = None;
            // Once a host: a connection dropped and made again changes no
            // bot's tools.
            if started {
                started = false;
                let bots = crate::lookout::all_bots(&client).await;
                tokio::spawn(refresh_tools(client.clone(), bots));
            }
            let follow = json!({"bot": "*", "after": cursor, "kinds": KINDS});
            match client.request("follow", follow).await {
                Ok(_) => {
                    while let Some(event) = events.recv().await {
                        server.event(&event);
                        if let Some(at) = event["cursor"].as_i64()
                            && at > cursor
                        {
                            cursor = at;
                            save(&store, cursor);
                        }
                    }
                    println!("{}", json!({"event": "disconnected", "cursor": cursor}));
                }
                Err(error) => println!(
                    "{}",
                    json!({"event": "follow_refused", "error": error.code, "after": cursor})
                ),
            }
        }
        if gone.get_or_insert_with(Instant::now).elapsed() >= GONE_AFTER {
            println!("{}", json!({"event": "daemon_gone"}));
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
    // Where each stands in its lead's line, from the listing.
    let limit = crate::served::depth_limit();
    let by_id: HashMap<i64, &Value> = (bots.iter())
        .filter_map(|bot| Some((bot["bot_id"].as_i64()?, bot)))
        .collect();
    let depths: HashMap<i64, u64> = (bots.iter())
        .filter_map(|bot| {
            let (depth, _) = crate::declared::line_by(bot, limit, |at| {
                by_id
                    .get(&at["created_by_id"].as_i64()?)
                    .map(|&by| by.clone())
            });
            Some((bot["bot_id"].as_i64()?, depth))
        })
        .collect();
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
            let depth = (bot["bot_id"].as_i64())
                .and_then(|id| depths.get(&id).copied())
                .unwrap_or(1);
            let Some(fields) = crate::declared::refresh(&record, depth) else {
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

#[cfg(test)]
mod tests {
    use super::*;

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
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
