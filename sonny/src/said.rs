//! What the person wrote to any agent lately, for the nightly memory
//! cleanup to find what no agent saved, as Codex mines finished sessions
//! and OpenClaw's dreaming reads the day's notes. The person's words only:
//! a prompt another agent or a named client sent, and a tool's result, are
//! left out, so the cleanup learns nothing from text no person wrote. Their
//! reactions to a bot's replies are in too, each with the first line of the
//! reply it answers, as ChatGPT Pulse keeps feedback with the card it was
//! on, so the cleanup can tell what they welcomed.
use crate::reactions::Reactions;
use agent_client::Client;
use serde_json::{Value, json};
use std::borrow::Borrow;
use std::collections::{BTreeMap, HashMap};
use std::path::{Path, PathBuf};

/// The most of the person's words one call returns, the answer whole; the
/// newest are kept. Within the 60 KiB a served tool's answer may take.
const BYTES: usize = 56 * 1024;
/// The most of one message returned; the rest is cut.
const MESSAGE_BYTES: usize = 4 * 1024;
/// The most of a reply's first line a reaction carries.
const REPLY_CHARS: usize = 200;
/// How far back the cleanup looks when no earlier one finished.
pub const DAY_MS: i64 = 24 * 60 * 60 * 1000;

/// Every message the person wrote to an agent at or after `since_ms`,
/// newest first, each with its agent, turn, and the project whose memory
/// the turn's folder reads (null for none). The one that started a turn
/// before `since_ms` is older, though the turn still ran after. With the
/// `home` and canonical store path the windows keep reactions under, a
/// reaction the person took back since is left out.
pub async fn said(
    client: &Client,
    kept: Option<(&Path, &Path)>,
    since_ms: i64,
) -> Result<Value, String> {
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
            let turns = turns_since(client, name, since_ms).await.map_err(failed)?;
            let Some(first) = turns.keys().min().copied() else {
                continue;
            };
            // History has no times, so a message is dated by its turn: a
            // turn's first entry is what started it, written when the turn
            // started, and what the person sent into it later came while it
            // ran. Newest first, and a turn's entries together, its oldest
            // message is held until an older entry shows it came later or
            // the turn ends, when it counts only if the turn started since.
            let opened = |turn: i64| {
                (turns.get(&turn).and_then(|t| t.started_ms)).is_none_or(|ms| ms >= since_ms)
            };
            let mut held: Option<(i64, i64, Value)> = None;
            // What the bot's replies are reacted with now, by node; none
            // when no window kept a file for it.
            let on = kept
                .zip(bot["bot_id"].as_i64())
                .and_then(|((home, store), id)| {
                    Reactions::kept(home, &crate::reactions::key(store, id))
                });
            // Reactions, kept as they are read and each waiting for the
            // reply it answers, which comes later walking back: the next
            // reply for one to the bot's last reply, else a reply it
            // quotes.
            let mut waiting: Vec<Waiting> = Vec::new();
            // The oldest node walked.
            let mut oldest = i64::MAX;
            // Answers to the bot's asks, each waiting for the ask it
            // answers, which comes later walking back, for what it
            // recommended.
            let mut answers: Vec<(String, i64)> = Vec::new();
            walk(client, name, first, |turn, entry| {
                if held.as_ref().is_some_and(|(at, ..)| *at != turn)
                    && let Some((at, node, message)) = held.take()
                    && opened(at)
                {
                    admit(&mut said, &mut answers, node, message);
                }
                let project = turns.get(&turn).and_then(|t| t.project.clone());
                let later = match crate::lookout::said_by_person(entry) {
                    Some(node) => {
                        let text = cut(crate::text(&entry["item"]));
                        let message =
                            json!({"bot": name, "turn": turn, "text": text, "project": project});
                        held.replace((turn, node, message))
                    }
                    None => held.take(),
                };
                if let Some((_, node, message)) = later {
                    admit(&mut said, &mut answers, node, message);
                }
                if let Some((node, quote, emoji)) = reaction(entry)
                    // One no reply holds now was taken off: it takes no room.
                    && (on.as_ref()).is_none_or(|on| on.before(i64::MAX, &emoji))
                {
                    said.add(
                        node,
                        json!({"bot": name, "turn": turn, "text": emoji,
                        "reacted_to": quote, "project": project}),
                    );
                    waiting.retain(|waiting| said.kept.contains_key(&waiting.node));
                    waiting.push(Waiting { node, quote, emoji });
                }
                for (question, recommended) in asked(entry) {
                    if let Some(at) = answers.iter().position(|(q, _)| *q == question) {
                        let (_, node) = answers.remove(at);
                        said.amend(node, "recommended", json!(recommended));
                    }
                }
                if let Some(reply) = reply(entry)
                    && let Some(at) = entry["node"].as_i64()
                {
                    let quoted = crate::transcript::cut(&reply, crate::QUOTED);
                    let has = |emoji: &str| {
                        on.as_ref()
                            .is_none_or(|on| on.on(at).iter().any(|e| e == emoji))
                    };
                    // Each emoji once per reply, the newest kept.
                    let mut counted: Vec<String> = Vec::new();
                    waiting.retain_mut(|waiting| {
                        // The newest reply it can answer that holds it: a
                        // reply of the bot's may come between the reply and
                        // a reaction that waited for its call, and two
                        // replies may begin alike.
                        let can = (waiting.quote.as_ref()).is_none_or(|quote| *quote == quoted);
                        if !(can && has(&waiting.emoji)) {
                            return true;
                        }
                        if counted.contains(&waiting.emoji) {
                            // Put on again.
                            said.remove(waiting.node);
                        } else {
                            counted.push(waiting.emoji.clone());
                            said.amend(waiting.node, "reacted_to", json!(cut_reply(&reply)));
                        }
                        false
                    });
                }
                oldest = oldest.min(entry["node"].as_i64().unwrap_or(i64::MAX));
                false
            })
            .await
            .map_err(failed)?;
            if let Some((at, node, message)) = held
                && opened(at)
            {
                admit(&mut said, &mut answers, node, message);
            }
            // A reaction no reply in reach holds answers one out of reach,
            // when what is on those says so, else it was taken off. Each
            // counts once.
            let mut counted: Vec<(Option<String>, String)> = Vec::new();
            for waiting in waiting {
                let out_of_reach = (on.as_ref()).is_none_or(|on| on.before(oldest, &waiting.emoji));
                let key = (waiting.quote, waiting.emoji);
                if !out_of_reach || counted.contains(&key) {
                    said.remove(waiting.node);
                } else {
                    counted.push(key);
                }
            }
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

/// A message of the person's into the answer; an answer to an ask also
/// waits for the ask, for what it recommended.
fn admit(said: &mut Newest, answers: &mut Vec<(String, i64)>, node: i64, message: Value) {
    let question = crate::asks::answered(message["text"].as_str().unwrap_or_default());
    let question = question.map(str::to_owned);
    said.add(node, message);
    if let Some(question) = question {
        answers.retain(|(_, node)| said.kept.contains_key(node));
        answers.push((question, node));
    }
}

/// A reaction of the person's waiting for the reply it answers.
struct Waiting {
    node: i64,
    quote: Option<String>,
    emoji: String,
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
        self.trim();
    }

    /// Sets `field` on the message kept at `node`, if it still is.
    fn amend(&mut self, node: i64, field: &str, value: Value) {
        let Some((size, entry)) = self.kept.get_mut(&node) else {
            return;
        };
        entry[field] = value;
        let grown = entry.to_string().len() + 1;
        self.bytes = self.bytes - *size + grown;
        *size = grown;
        self.trim();
    }

    /// Drops the message kept at `node`, if it still is.
    fn remove(&mut self, node: i64) {
        if let Some((size, _)) = self.kept.remove(&node) {
            self.bytes -= size;
        }
    }

    /// Drops the oldest messages until the answer fits.
    fn trim(&mut self) {
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

/// A turn that ran at or after `since_ms`: the project whose memory its
/// folder reads, as memory finds it, so a bot that moved between projects
/// keeps each turn's own, and when it started (none while queued).
struct Ran {
    project: Option<String>,
    started_ms: Option<i64>,
}

/// The turns of `bot` that were running at or after `since_ms`.
async fn turns_since(
    client: &Client,
    bot: &str,
    since_ms: i64,
) -> Result<HashMap<i64, Ran>, agent_client::Error> {
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
            let started_ms = turn["started_ms"].as_i64();
            found.insert(
                id,
                Ran {
                    project,
                    started_ms,
                },
            );
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

/// A reaction of the person's to one of the bot's replies, which is
/// feedback on that reply: its node, the first line of the reply when the
/// reaction quotes it, and the emoji.
fn reaction(entry: &Value) -> Option<(i64, Option<String>, String)> {
    if !(entry["from"].is_null() && entry["origin"].is_null() && entry["item"]["role"] == "user") {
        return None;
    }
    let text = crate::text(&entry["item"]);
    let (quote, emoji) = crate::reactions::parse(&text)?;
    Some((
        entry["node"].as_i64()?,
        quote.map(str::to_owned),
        emoji.to_owned(),
    ))
}

/// The questions the bot's `ask` calls in an entry put, each with the
/// label of the option it recommended, if any. An Anthropic message holds
/// its calls as `tool_use` blocks, any number of them.
fn asked(entry: &Value) -> Vec<(String, Option<String>)> {
    (crate::responses(&entry["item"]).iter())
        .filter_map(|item| {
            if item["type"] != "function_call" || item["name"] != crate::asks::TOOL {
                return None;
            }
            let arguments = serde_json::from_str::<Value>(item["arguments"].as_str()?).ok()?;
            let ask = crate::asks::call("", &arguments)?;
            let recommended = (ask.recommended).map(|at| ask.options[at].label.clone());
            Some((ask.question, recommended))
        })
        .collect()
}

/// The first line of a reply of the bot's, trimmed, as a window quotes it.
fn reply(entry: &Value) -> Option<String> {
    if entry["item"]["role"] != "assistant" {
        return None;
    }
    let text = crate::text(&entry["item"]);
    let first = text.lines().map(str::trim).find(|line| !line.is_empty())?;
    Some(first.to_owned())
}

/// The first line of a reply as a reaction to it carries.
fn cut_reply(first: &str) -> String {
    first.chars().take(REPLY_CHARS).collect()
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
    let client = connect().await?;
    // Where the windows keep reactions: the store as the CLI finds it.
    let home = std::env::var_os("HOME").map(std::path::PathBuf::from);
    let store = crate::trigger::Daemon::current()
        .ok()
        .and_then(|daemon| daemon.store);
    let store = store.map(|store| std::fs::canonicalize(&store).unwrap_or(store));
    let kept = home.as_deref().zip(store.as_deref());
    said(&client, kept, since_ms.unwrap_or(now_ms() - DAY_MS)).await
}

/// Whether the nightly cleanup has work, for its trigger's `--if`: what
/// memory's own `changed` says, or, when nothing was saved or removed,
/// whether the person wrote to any agent since the last finished cleanup
/// started, so the night they say something is the night it is read, as
/// Generative Agents and sleep-time agents reflect on the day's events and
/// not only on what was stored.
pub async fn changed(home: &Path, cwd: &Path) -> Result<Value, String> {
    changed_on(home, cwd, connect()).await
}

/// [`changed`], reaching the daemon with `client` only when memory alone
/// says nothing changed.
pub async fn changed_on(
    home: &Path,
    cwd: &Path,
    client: impl std::future::Future<Output = Result<impl Borrow<Client>, String>>,
) -> Result<Value, String> {
    let unchanged = match agent_client::memory::call(home, cwd, &json!({"changed": true})) {
        Err(error) if error.starts_with("unchanged:") => error,
        done => return done,
    };
    let root = home.join(".agents/memory");
    let since = agent_client::memory::said_since(&root).unwrap_or(now_ms() - DAY_MS);
    let said = said(client.await?.borrow(), None, since).await?;
    match said["said"].as_array().map_or(0, Vec::len) {
        0 => Err(format!("{unchanged}, and the person wrote nothing since")),
        _ => Ok(json!({"changed": true, "said": true})),
    }
}

/// The daemon this shell reaches, started as a trigger's fire starts it
/// when it is not running: the cleanup's `--if` runs before its fire
/// would start one, and a stopped daemon still holds what the person wrote.
async fn connect() -> Result<std::sync::Arc<Client>, String> {
    let failed = |e: String| format!("said_unread: {e}");
    let daemon = crate::trigger::Daemon::current().map_err(failed)?;
    let socket = daemon.socket().map_err(failed)?;
    crate::trigger::connect(&socket, &daemon)
        .await
        .map_err(failed)
}

fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_millis() as i64)
}

/// `memory said [SINCE_MS]`, as `~/.agent/memory` runs it: one JSON line
/// on stdout, or `{"error", "detail"}` on stderr and exit 1.
pub fn cli(args: &[String]) -> i32 {
    let since = match args {
        [] => Ok(None),
        [ms] => ms.parse::<i64>().map(Some).map_err(|_| ()),
        _ => Err(()),
    };
    printed(match since {
        Err(()) => Err("usage: memory said [SINCE_MS]".to_owned()),
        Ok(since) => block_on(read(since)),
    })
}

/// `memory changed`, as `~/.agent/memory` runs it for the cleanup's
/// trigger: [`changed`], printed as [`cli`] prints.
pub fn changed_cli() -> i32 {
    let home = std::env::var_os("HOME").map(std::path::PathBuf::from);
    let cwd = std::env::current_dir().ok();
    printed(match (home, cwd) {
        (Some(home), Some(cwd)) => block_on(changed(&home, &cwd)),
        _ => Err("memory_failed: no HOME or working folder".into()),
    })
}

fn block_on(
    done: impl std::future::Future<Output = Result<Value, String>>,
) -> Result<Value, String> {
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .map_err(|e| format!("said_unread: {e}"))
        .and_then(|runtime| runtime.block_on(done))
}

fn printed(done: Result<Value, String>) -> i32 {
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
