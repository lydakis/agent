//! What the person wrote to any agent lately, for the nightly memory
//! cleanup to find what no agent saved, as Codex mines finished sessions
//! and OpenClaw's dreaming reads the day's notes. The person's words only:
//! a prompt another agent or a named client sent, and a tool's result, are
//! left out, so the cleanup learns nothing from text no person wrote.
use agent_client::Client;
use serde_json::{Value, json};
use std::collections::{BTreeMap, HashMap};
use std::path::{Path, PathBuf};

/// The most of the person's words one call returns, the answer whole; the
/// newest are kept. Within the 60 KiB a served tool's answer may take.
const BYTES: usize = 56 * 1024;
/// The most of one message returned; the rest is cut.
const MESSAGE_BYTES: usize = 4 * 1024;
/// How far back the cleanup looks when no earlier one finished.
pub const DAY_MS: i64 = 24 * 60 * 60 * 1000;

/// Every message the person wrote to an agent in a turn that ran at or
/// after `since_ms`, newest first, each with its agent, turn, and the
/// project whose memory the turn's folder reads (null for none).
pub async fn said(client: &Client, since_ms: i64) -> Result<Value, String> {
    let failed = |e: agent_client::Error| format!("said_unread: {e}");
    let mut said = Newest::default();
    let mut after = Value::Null;
    loop {
        let page = client
            .request("bots", json!({"after": after, "limit": 64}))
            .await
            .map_err(failed)?;
        for bot in page["bots"].as_array().into_iter().flatten() {
            let name = bot["name"].as_str().unwrap_or_default();
            let projects = turns_since(client, name, since_ms).await.map_err(failed)?;
            let Some(first) = projects.keys().min().copied() else {
                continue;
            };
            walk(client, name, first, |turn, entry| {
                if let Some(node) = crate::lookout::said_by_person(entry) {
                    let text = cut(crate::text(&entry["item"]));
                    let project = projects.get(&turn).cloned().flatten();
                    said.add(
                        node,
                        json!({"bot": name, "turn": turn, "text": text, "project": project}),
                    );
                }
                false
            })
            .await
            .map_err(failed)?;
        }
        after = page["next_after"].clone();
        if after.is_null() {
            break;
        }
    }
    let kept: Vec<Value> = said
        .kept
        .into_values()
        .rev()
        .map(|(_, entry)| entry)
        .collect();
    Ok(json!({"since_ms": since_ms, "said": kept, "cut": said.left_out}))
}

/// The newest messages that fit in `BYTES`, by node: node ids grow as
/// history is written, across agents too. Older ones are dropped as they
/// come, so a long interval holds no more than the answer.
#[derive(Default)]
struct Newest {
    kept: BTreeMap<i64, (usize, Value)>,
    bytes: usize,
    /// The newest node dropped; no older one is kept after it.
    floor: Option<i64>,
    left_out: bool,
}

impl Newest {
    /// What the answer takes besides its messages.
    const FRAME: usize = 128;

    fn add(&mut self, node: i64, entry: Value) {
        if self.floor.is_some_and(|floor| node <= floor) {
            self.left_out = true;
            return;
        }
        // Its bytes and the comma before it.
        let size = entry.to_string().len() + 1;
        self.bytes += size;
        self.kept.insert(node, (size, entry));
        while self.bytes + Self::FRAME > BYTES {
            let Some((oldest, (size, _))) = self.kept.pop_first() else {
                break;
            };
            self.bytes -= size;
            self.floor = self.floor.max(Some(oldest));
            self.left_out = true;
        }
    }
}

/// The turns of `bot` that were running at or after `since_ms`, each with
/// the project whose memory its folder reads, as memory finds it: a bot
/// that moved between projects keeps each turn's own.
async fn turns_since(
    client: &Client,
    bot: &str,
    since_ms: i64,
) -> Result<HashMap<i64, Option<String>>, agent_client::Error> {
    let mut found = HashMap::new();
    let mut folders: HashMap<String, Option<String>> = HashMap::new();
    let mut before = Value::Null;
    loop {
        let page = client
            .request(
                "turns",
                json!({"bot": bot, "newest": true, "limit": 256, "before": before}),
            )
            .await?;
        let turns = page["turns"].as_array().cloned().unwrap_or_default();
        // A bot runs one turn at a time, so once a turn that ran ended
        // before `since_ms`, every older one that ran did too. A queued turn
        // ended without running says nothing about older ones.
        let mut ended = false;
        for turn in &turns {
            if turn["finished_ms"].as_i64().is_some_and(|ms| ms < since_ms) {
                ended = !turn["started_ms"].is_null();
                if ended {
                    break;
                }
                continue;
            }
            let Some(id) = turn["turn"].as_i64() else {
                continue;
            };
            let project = match turn["workspace"].as_str() {
                Some(dir) => folders
                    .entry(dir.to_owned())
                    .or_insert_with(|| {
                        // A task in a folder of its own works for its
                        // lead's project.
                        let dir = crate::task::source_folder(Path::new(dir))
                            .unwrap_or_else(|| PathBuf::from(dir));
                        agent_client::policy::project(&dir).ok().flatten()
                    })
                    .clone(),
                None => None,
            };
            found.insert(id, project);
        }
        before = page["next_before"].clone();
        if ended || before.is_null() {
            return Ok(found);
        }
    }
}

/// Each history entry of `bot` in its turns from `first` on, newest first,
/// with its turn, until `visit` says to stop.
pub async fn walk(
    client: &Client,
    bot: &str,
    first: i64,
    mut visit: impl FnMut(i64, &Value) -> bool,
) -> Result<(), agent_client::Error> {
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
        let turns: HashMap<i64, i64> = wanted.iter().copied().collect();
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
                return Ok(());
            }
            next += items.len();
            for entry in &items {
                let turn = entry["node"].as_i64().and_then(|node| turns.get(&node));
                if let Some(turn) = turn
                    && visit(*turn, entry)
                {
                    return Ok(());
                }
            }
        }
        from = page["next_from"].clone();
        if older || wanted.is_empty() || from.is_null() {
            return Ok(());
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_the_newest_words_that_fit_are_held_while_collecting() {
        let entry = |text: &str| json!({"text": text});
        let mut said = Newest::default();
        // Four of these are just over what fits.
        let quarter = "x".repeat(BYTES / 4);
        said.add(5, entry(&quarter));
        said.add(9, entry(&quarter));
        said.add(7, entry(&quarter));
        assert!(!said.left_out);
        // A fourth drops the oldest, and nothing older is kept after it,
        // though a small one would fit.
        said.add(8, entry(&quarter));
        said.add(2, entry("small"));
        assert!(said.left_out);
        assert_eq!(said.kept.keys().copied().collect::<Vec<_>>(), [7, 8, 9]);
        assert!(said.bytes + Newest::FRAME <= BYTES);
    }
}
