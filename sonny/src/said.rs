//! What the person wrote to any agent lately, for the nightly memory
//! cleanup to find what no agent saved, as Codex mines finished sessions
//! and OpenClaw's dreaming reads the day's notes. The person's words only:
//! a prompt another agent or a named client sent, and a tool's result, are
//! left out, so the cleanup learns nothing from text no person wrote.
use agent_client::Client;
use serde_json::{Value, json};

/// The most of the person's words one call returns; the newest are kept.
const BYTES: usize = 96 * 1024;
/// The most of one message returned; the rest is cut.
const MESSAGE_BYTES: usize = 4 * 1024;
/// How far back the cleanup looks when no earlier one finished.
pub const DAY_MS: i64 = 24 * 60 * 60 * 1000;

/// Every message the person wrote to an agent in a turn that ran at or
/// after `since_ms`, newest first, each with its agent, turn, and the
/// project whose memory the agent reads (null for none).
pub async fn said(client: &Client, since_ms: i64) -> Result<Value, String> {
    let failed = |e: agent_client::Error| format!("said_unread: {e}");
    let mut said = Vec::new();
    let mut after = Value::Null;
    loop {
        let page = client
            .request("bots", json!({"after": after, "limit": 64}))
            .await
            .map_err(failed)?;
        for bot in page["bots"].as_array().into_iter().flatten() {
            let name = bot["name"].as_str().unwrap_or_default();
            // The project whose memory its folder reads, as memory finds it.
            let project = bot["workspace"]
                .as_str()
                .and_then(|dir| agent_client::policy::project(std::path::Path::new(dir)).ok())
                .flatten();
            if let Some(first) = first_turn(client, name, since_ms).await.map_err(failed)? {
                let mut found = words(client, name, first).await.map_err(failed)?;
                for (_, entry) in &mut found {
                    entry["project"] = json!(project);
                }
                said.extend(found);
            }
        }
        after = page["next_after"].clone();
        if after.is_null() {
            break;
        }
    }
    // Node ids grow as history is written, across agents too.
    said.sort_by_key(|(node, _)| std::cmp::Reverse(*node));
    let mut kept = Vec::new();
    let mut bytes = 0;
    let mut cut = false;
    for (_, entry) in said {
        bytes += entry.to_string().len();
        if bytes > BYTES {
            cut = true;
            break;
        }
        kept.push(entry);
    }
    Ok(json!({"since_ms": since_ms, "said": kept, "cut": cut}))
}

/// The oldest turn of `bot` that was running at or after `since_ms`.
async fn first_turn(
    client: &Client,
    bot: &str,
    since_ms: i64,
) -> Result<Option<i64>, agent_client::Error> {
    let page = client
        .request("turns", json!({"bot": bot, "newest": true, "limit": 256}))
        .await?;
    Ok(page["turns"]
        .as_array()
        .into_iter()
        .flatten()
        .take_while(|turn| turn["finished_ms"].as_i64().is_none_or(|ms| ms >= since_ms))
        .filter_map(|turn| turn["turn"].as_i64())
        .min())
}

/// The person's messages to `bot` in its turns from `first` on, each with
/// its node, newest first.
async fn words(
    client: &Client,
    bot: &str,
    first: i64,
) -> Result<Vec<(i64, Value)>, agent_client::Error> {
    let mut found = Vec::new();
    let mut from = Value::Null;
    loop {
        let page = client
            .request(
                "history_nodes",
                json!({"bot": bot, "from": from, "limit": 200}),
            )
            .await?;
        let nodes: Vec<(i64, i64)> = (page["nodes"].as_array().into_iter().flatten())
            .filter_map(|n| Some((n["node"].as_i64()?, n["turn"].as_i64()?)))
            .collect();
        let older = nodes.iter().any(|(_, turn)| *turn < first);
        let wanted: Vec<(i64, i64)> = nodes
            .into_iter()
            .take_while(|(_, turn)| *turn >= first)
            .collect();
        let mut next = 0;
        while next < wanted.len() {
            let chunk: Vec<i64> = wanted[next..(next + 100).min(wanted.len())]
                .iter()
                .map(|(node, _)| *node)
                .collect();
            let items = client
                .request("history_items", json!({"bot": bot, "nodes": chunk}))
                .await?;
            // Fewer than asked when they are large; at least one.
            let items = items["items"].as_array().cloned().unwrap_or_default();
            if items.is_empty() {
                return Ok(found);
            }
            next += items.len();
            for entry in &items {
                let Some(node) = crate::lookout::said_by_person(entry) else {
                    continue;
                };
                let turn = wanted
                    .iter()
                    .find(|(id, _)| *id == node)
                    .map(|(_, turn)| *turn);
                found.push((
                    node,
                    json!({"bot": bot, "turn": turn, "text": cut(crate::text(&entry["item"]))}),
                ));
            }
        }
        from = page["next_from"].clone();
        if older || wanted.is_empty() || from.is_null() {
            return Ok(found);
        }
    }
}

fn cut(mut text: String) -> String {
    if text.len() > MESSAGE_BYTES {
        let mut end = MESSAGE_BYTES;
        while !text.is_char_boundary(end) {
            end -= 1;
        }
        text.truncate(end);
        text.push_str(" [cut]");
    }
    text
}

/// The person's words since `since_ms`, else over the last day, from the
/// daemon this shell reaches.
pub async fn read(since_ms: Option<i64>) -> Result<Value, String> {
    let (_, socket) = crate::daemon_paths(None, None).map_err(|e| format!("said_unread: {e}"))?;
    let client = Client::connect(&socket)
        .await
        .map_err(|e| format!("said_unread: {e}"))?
        .0;
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_millis() as i64);
    said(&client, since_ms.unwrap_or(now - DAY_MS)).await
}

/// `memory said [SINCE_MS]`, as `~/.agent/memory` runs it: one JSON line
/// on stdout, or `{"error", "detail"}` on stderr and exit 1.
pub fn cli(args: &[String]) -> i32 {
    let since = match args {
        [] => Ok(None),
        [ms] => ms.parse::<i64>().map(Some).map_err(|_| ()),
        _ => Err(()),
    };
    let done = match since {
        Err(()) => Err("usage: memory said [SINCE_MS]".to_owned()),
        Ok(since) => tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .map_err(|e| format!("said_unread: {e}"))
            .and_then(|runtime| runtime.block_on(read(since))),
    };
    match done {
        Ok(value) => {
            println!("{value}");
            0
        }
        Err(error) => {
            let (code, detail) = error.split_once(": ").unwrap_or(("usage", error.as_str()));
            eprintln!("{}", json!({"error": code, "detail": detail}));
            1
        }
    }
}
