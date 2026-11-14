//! The reviewer model `sonny reviewer` asks about planned calls: which
//! one, picked in one order, and how it answers. It is a `provider/model`
//! the daemon runs, as a fork of an idle base bot that takes one turn, so
//! checks run at once and none sees another. Its reply follows a JSON
//! Schema the provider enforces, and is checked whole before it counts.
use crate::{Client, Error};
use serde_json::{Map, Value, json};
use std::{
    sync::atomic::{AtomicU64, Ordering},
    time::Instant,
};

/// Whether a judge's name is `provider/model`, a model the daemon may run.
pub fn is_model(name: &str) -> bool {
    name.split_once('/')
        .is_some_and(|(provider, model)| !provider.is_empty() && !model.is_empty())
}

/// Who judges for a bot running `caller`: the judge `named` by the caller
/// (`--model`, else the config's `[reviewer] model`), else the bot's own model. No model is
/// guessed for a provider.
pub fn pick(named: Option<&str>, caller: Option<&str>) -> Option<String> {
    named.or(caller).map(str::to_owned)
}

/// Each id asked, and the reviewer's answer for it.
pub type Answers = Map<String, Value>;

/// What a judge read and wrote for one check, in tokens, as its provider
/// reported them, and where its time went.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct Usage {
    pub input: u64,
    /// Of `input`, what the provider had cached.
    pub cached: u64,
    pub output: u64,
    /// None when the turn's times or history could not be read whole.
    pub timing: Option<Timing>,
}

/// Where a check's turn spent its time, in milliseconds, as the daemon
/// recorded it: waiting for a slot to start, the model's rounds before
/// the one that answered (deciding what to look at), the tools it ran to
/// look, and the round that wrote the verdict. The model's rounds and the
/// provider's retries are counts.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct Timing {
    pub queued_ms: u64,
    pub looking_ms: u64,
    pub tools_ms: u64,
    pub verdict_ms: u64,
    /// Of the rounds, the time the daemon held back calls to keep under
    /// the provider's limits.
    pub paced_ms: u64,
    pub rounds: u64,
    pub retries: u64,
}

impl Timing {
    /// Add `other`'s times and counts to these: one check's attempts.
    pub fn add(&mut self, other: &Timing) {
        self.queued_ms += other.queued_ms;
        self.looking_ms += other.looking_ms;
        self.tools_ms += other.tools_ms;
        self.verdict_ms += other.verdict_ms;
        self.paced_ms += other.paced_ms;
        self.rounds += other.rounds;
        self.retries += other.retries;
    }

    /// The time of a judge's turn, from its row in `turns`, the time it
    /// was `submitted`, and each of its history's entries in order, as
    /// when it was written and whether it is a tool's output. A tool's
    /// time runs from the round that called it to the last output of
    /// those it called; the verdict's from the last output, or the
    /// turn's start, to its end. None for a turn not ended.
    pub fn of(row: &Value, submitted_ms: i64, entries: &[(i64, bool)]) -> Option<Self> {
        let at = |key: &str| row[key].as_i64();
        let (Some(started), Some(finished)) = (at("started_ms"), at("finished_ms")) else {
            return None;
        };
        let (mut tools, mut called, mut looked) = (0, None, None);
        for &(written, output) in entries {
            match (output, called) {
                (true, Some(round)) => looked = Some(written.max(round)),
                (true, None) => {}
                (false, _) => {
                    if let (Some(round), Some(last)) = (called, looked) {
                        tools += last - round;
                    }
                    (called, looked) = (Some(written), None);
                }
            }
        }
        if let (Some(round), Some(last)) = (called, looked) {
            tools += last - round;
        }
        let answered_from = (entries.iter().rev())
            .find(|(_, output)| *output)
            .map_or(started, |(written, _)| (*written).clamp(started, finished));
        let span = |from: i64, to: i64| (to - from).max(0) as u64;
        let tools_ms = (tools.max(0) as u64).min(span(started, answered_from));
        Some(Self {
            queued_ms: span(submitted_ms, started),
            looking_ms: span(started, answered_from) - tools_ms,
            tools_ms,
            verdict_ms: span(answered_from, finished),
            paced_ms: row["paced_ms"].as_u64().unwrap_or(0),
            rounds: row["model_rounds"].as_u64().unwrap_or(0),
            retries: row["retries"].as_u64().unwrap_or(0),
        })
    }
}

/// Milliseconds since the epoch, as the daemon stamps a turn's times.
fn epoch_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |since| since.as_millis() as i64)
}

/// Whether a history item is a tool's output: a `function_call_output`,
/// or a message of `tool_result` blocks, as the Anthropic family keeps them.
fn tool_output(item: &Value) -> bool {
    item["type"] == "function_call_output"
        || (item["role"] == "user"
            && (item["content"].as_array().into_iter().flatten())
                .any(|block| block["type"] == "tool_result"))
}

/// Each history entry of `bot`'s turn `turn`, oldest first, as when it
/// was written and whether it is a tool's output. None when any of it
/// could not be read, so no split is made from part of it.
async fn entries(client: &Client, bot: &str, turn: i64) -> Option<Vec<(i64, bool)>> {
    // Pages run newest first: they are read until one reaches an earlier
    // turn, or the history's start.
    let mut written = Map::new();
    let mut from: Option<i64> = None;
    loop {
        let Ok(page) = client
            .request(
                "history_nodes",
                json!({"bot": bot, "limit": HISTORY_PAGE, "from": from}),
            )
            .await
        else {
            return None;
        };
        match turn_nodes(&page, turn, &mut written) {
            Some(next) => from = Some(next),
            None => break,
        }
    }
    let mut left: Vec<i64> = written
        .keys()
        .filter_map(|node| node.parse().ok())
        .collect();
    // A read takes at most a page of nodes and stops at a size, its items
    // a prefix of those asked: the rest are asked for again until every
    // one is read, or the split is left unsaid rather than given wrong.
    let mut items = Vec::new();
    while !left.is_empty() {
        let asked = left.len().min(HISTORY_PAGE);
        let Ok(page) = client
            .request(
                "history_items",
                json!({"bot": bot, "nodes": &left[..asked]}),
            )
            .await
        else {
            return None;
        };
        let page = page["items"].as_array().cloned().unwrap_or_default();
        let mut rest = rest(&left[..asked], &page)?;
        rest.extend_from_slice(&left[asked..]);
        left = rest;
        items.extend(page);
    }
    // An item too large to send can't say whether it was a tool's output.
    let mut entries: Vec<(i64, i64, bool)> = Vec::with_capacity(items.len());
    for entry in &items {
        if !entry["error"].is_null() {
            return None;
        }
        let node = entry["node"].as_i64()?;
        let at = written.get(&node.to_string())?.as_i64()?;
        entries.push((node, at, tool_output(&entry["item"])));
    }
    entries.sort_unstable();
    Some(
        entries
            .into_iter()
            .map(|(_, at, output)| (at, output))
            .collect(),
    )
}

/// The most nodes one history read takes, as the daemon allows.
const HISTORY_PAGE: usize = 400;

/// Add the nodes of turn `turn` on a newest-first `page` to `written`, by
/// node, as when each was written. Gives where the next page starts while
/// the turn may go on past this one.
fn turn_nodes(page: &Value, turn: i64, written: &mut Map<String, Value>) -> Option<i64> {
    let nodes = page["nodes"]
        .as_array()
        .map(Vec::as_slice)
        .unwrap_or_default();
    for node in nodes {
        if node["turn"].as_i64() != Some(turn) {
            return None;
        }
        if let Some(id) = node["node"].as_i64() {
            written.insert(id.to_string(), node["written_ms"].clone());
        }
    }
    page["next_from"].as_i64()
}

/// The nodes of `wanted` a read's `page` left out, to ask for next; none
/// when it read none of them, as a read that makes no progress.
fn rest(wanted: &[i64], page: &[Value]) -> Option<Vec<i64>> {
    let read: std::collections::HashSet<i64> = page
        .iter()
        .filter_map(|entry| entry["node"].as_i64())
        .collect();
    let rest: Vec<i64> = (wanted.iter().copied())
        .filter(|node| !read.contains(node))
        .collect();
    (rest.len() < wanted.len()).then_some(rest)
}

/// Why a judge gave no answers: what failed in this check.
#[derive(Debug, PartialEq)]
pub struct Failed {
    pub why: String,
    /// The judge's reply, when there was one it could not be read as
    /// answers: whole, so whoever logs it redacts it before [`excerpt`]
    /// cuts it, and no secret is cut where redaction would miss it.
    pub reply: Option<String>,
    /// Where the reviewer's turn spent its time, when it ended: none for one
    /// that never started or was still running when the check gave up.
    pub timing: Option<Timing>,
    /// Where a turn still running when the check gave up was, read before
    /// its fork is removed: its status, rounds, retries and time held back,
    /// when it started, and how long since it last wrote anything.
    pub hung: Option<Value>,
}

fn failed(why: impl Into<String>) -> Failed {
    Failed {
        why: why.into(),
        reply: None,
        timing: None,
        hung: None,
    }
}

/// How much of a reply that could not be read a log keeps.
const KEPT_REPLY: usize = 2048;

/// The start of `text` a log keeps of a reply that could not be read, cut
/// at a character. Redact `text` first.
pub fn excerpt(text: &str) -> &str {
    let mut cut = text.len().min(KEPT_REPLY);
    while !text.is_char_boundary(cut) {
        cut -= 1;
    }
    &text[..cut]
}

/// How long a judge's fork may sit idle before the daemon deletes it. Each
/// is deleted once it answers; this is for a judge that stopped mid-check.
const FORK_EXPIRE_MS: u64 = 60_000;

/// The tools of a judge that may look before it answers: read-only, as
/// Codex's guardian runs, in a read-only sandbox with no network.
pub const LOOKING: [&str; 2] = ["shell", "read"];

/// A model the daemon runs, as an idle base bot holding the judge's model,
/// instructions, and tools: none, or `LOOKING`. Each check forks it, takes
/// one turn, and deletes the fork once it answers.
pub struct Base {
    pub name: String,
    /// The base's identity, which each fork names as its creator.
    pub id: i64,
    next: AtomicU64,
}

/// What a fork of a base is named past the base's own name, at most:
/// `.ID.PID.N`.
pub const FORK_SUFFIX: usize = ".9223372036854775807.4294967295.18446744073709551615".len();

impl Base {
    /// The base `name` running `model` at `effort` with `instructions`
    /// and `tools`, `LOOKING` ones run in a read-only sandbox: kept when it
    /// already is that judge, made anew when it is another or an older
    /// version of it, so a changed judge takes effect. A bot of that name
    /// that is not a judge at all is refused, never deleted.
    pub async fn make(
        client: &Client,
        name: &str,
        model: &str,
        effort: Option<&str>,
        instructions: &str,
        tools: &[&str],
    ) -> crate::Result<Self> {
        let judge = |bot: &Value| is_judge(bot, instructions);
        let same = |bot: &Value| {
            format!(
                "{}/{}",
                bot["provider"].as_str().unwrap_or_default(),
                bot["model"].as_str().unwrap_or_default()
            ) == model
                && bot["effort"].as_str() == effort
                && bot["instructions"] == instructions
                && bot["tools"] == json!(tools)
        };
        let mut made = json!({"bot": name, "workspace": "/", "model": model, "effort": effort,
            "instructions": instructions, "tools": tools});
        if !tools.is_empty() {
            made["settings"] = json!({"sandbox": "read-only"});
        }
        let base = |bot: &Value| {
            Ok(Self {
                name: name.to_owned(),
                id: bot["bot_id"]
                    .as_i64()
                    .ok_or(Error::new("daemon_protocol_mismatch"))?,
                next: AtomicU64::new(0),
            })
        };
        match client.request("inspect", json!({"bot": name})).await {
            Ok(bot) if !judge(&bot) => {
                return Err(Error::with(
                    "reviewer_name_taken",
                    &format!("{name} is a bot that is not a reviewer; rename it"),
                ));
            }
            Ok(bot) if same(&bot) => {
                // A sandbox can go after the judge was made: asking for it
                // again is refused where none runs now.
                if !tools.is_empty() {
                    client
                        .request(
                            "configure",
                            json!({"bot": name, "settings": {"sandbox": "read-only"}}),
                        )
                        .await?;
                }
                return base(&bot);
            }
            Ok(_) => match client.request("delete", json!({"bot": name})).await {
                Err(error) if error.code != "bot_not_found" => return Err(error),
                _ => {}
            },
            Err(error) if error.code == "bot_not_found" => {}
            Err(error) => return Err(error),
        }
        let made = client.request("create", made).await;
        match made {
            Ok(made) => base(&made),
            // Another judge made it first.
            Err(error) if error.code == "bot_exists" => {
                let bot = client.request("inspect", json!({"bot": name})).await?;
                match judge(&bot) {
                    true => base(&bot),
                    false => Err(Error::with(
                        "reviewer_name_taken",
                        &format!("{name} is a bot that is not a reviewer; rename it"),
                    )),
                }
            }
            Err(error) => Err(error),
        }
    }

    /// One check whose reply follows `reply`'s schema, which the provider
    /// enforces, and is read by its parse as answers to every id. Its fork
    /// starts in `workspace` (else `/`), where a judge with tools looks. A
    /// workspace since removed starts it in `/`.
    pub async fn ask_with(
        &self,
        client: &Client,
        prompt: &str,
        ids: &[&str],
        reply: &Reply,
        workspace: Option<&str>,
        deadline: Instant,
    ) -> Result<(Answers, Usage), Failed> {
        let name = format!(
            "{}.{}.{}.{}",
            self.name,
            self.id,
            std::process::id(),
            self.next.fetch_add(1, Ordering::Relaxed)
        );
        let fork = |workspace: &str| {
            client.request(
                "fork",
                json!({"source": self.name, "bot": name, "workspace": workspace,
                    "created_by": self.name, "created_by_id": self.id,
                    "expire_ms": FORK_EXPIRE_MS}),
            )
        };
        let forked = match fork(workspace.unwrap_or("/")).await {
            Err(error)
                if workspace.is_some() && error.code == "workspace_must_exist_and_be_absolute" =>
            {
                fork("/").await
            }
            forked => forked,
        };
        forked.map_err(|error| failed(format!("the reviewer could not start: {}", error.code)))?;
        let (answered, running) = turn_with(client, &name, prompt, ids, reply, deadline).await;
        remove(client, &name, running).await;
        answered
    }
}

/// How a judge's reply is read as answers to the ids asked, or why it
/// cannot be: a failed check.
pub type Parse = fn(&str, &[&str]) -> Result<Answers, String>;

/// What a judge replies: the JSON Schema its turn asks the provider to
/// hold the reply to, and how the reply is then read.
pub struct Reply {
    pub schema: &'static str,
    pub parse: Parse,
}

/// Whether a bot record is a judge given `instructions`, or an older
/// version of them: their first sentence, which names what the judge
/// does, and no tools but `LOOKING` ones, those in a read-only sandbox.
pub fn is_judge(bot: &Value, instructions: &str) -> bool {
    let first = |text: &str| {
        text.split_once(". ")
            .map_or(text, |(first, _)| first)
            .to_owned()
    };
    let Some(tools) = bot["tools"].as_array() else {
        return false;
    };
    bot["instructions"]
        .as_str()
        .is_some_and(|said| first(said) == first(instructions))
        && tools
            .iter()
            .all(|tool| LOOKING.iter().any(|looking| tool == looking))
        && (tools.is_empty() || bot["settings"]["sandbox"] == "read-only")
}

/// One turn of a judge's fork `name`, its reply held to `reply`'s schema
/// and read by its parse, by `deadline`; and the turn if it is still
/// running.
pub async fn turn_with(
    client: &Client,
    name: &str,
    prompt: &str,
    ids: &[&str],
    reply: &Reply,
    deadline: Instant,
) -> (Result<(Answers, Usage), Failed>, Option<i64>) {
    let Ok(schema) = serde_json::from_str::<Value>(reply.schema) else {
        return (Err(failed("the reviewer's reply schema is not JSON")), None);
    };
    // Queued when every slot is taken: it starts when one frees, within
    // the deadline.
    let submitted_ms = epoch_ms();
    let submitted = match client
        .request(
            "submit",
            // Said by the judge, not the person: clients draw it so.
            json!({"bot": name, "request_id": name, "prompt": prompt, "delivery": "queue",
                "origin": "reviewer", "output_schema": schema}),
        )
        .await
    {
        Ok(submitted) => submitted,
        Err(error) => {
            let why = format!("the reviewer's turn was refused: {}", error.code);
            return (Err(failed(why)), None);
        }
    };
    let (Some(turn), Some(handle)) = (submitted["message"].as_i64(), submitted["handle"].as_str())
    else {
        return (Err(failed("the reviewer's turn was not started")), None);
    };
    let left = deadline.saturating_duration_since(Instant::now());
    let waited = match client
        .request(
            "wait",
            json!({"handles": [handle], "timeout_ms": left.as_millis() as u64}),
        )
        .await
    {
        Ok(waited) => waited,
        Err(error) => {
            let why = format!("the reviewer could not be waited on: {}", error.code);
            return (Err(failed(why)), Some(turn));
        }
    };
    let outcome = &waited["results"][handle];
    if outcome["pending"] == true {
        let failed = Failed {
            hung: Some(hung(client, name, turn, submitted_ms).await),
            ..failed("the check timed out")
        };
        return (Err(failed), Some(turn));
    }
    let usage = usage(client, name, turn, submitted_ms).await;
    if outcome["status"] != "completed" {
        let why = format!(
            "the reviewer's turn {}: {}",
            outcome["status"].as_str().unwrap_or("ended"),
            outcome["error"].as_str().unwrap_or("no answer")
        );
        let failed = Failed {
            timing: usage.timing,
            ..failed(why)
        };
        return (Err(failed), None);
    }
    let text = outcome["text"].as_str().unwrap_or_default();
    let parsed = match outcome["text_truncated"] == true {
        true => Err("the reviewer's answer was cut off".to_owned()),
        false => (reply.parse)(text, ids),
    };
    let answers = match parsed {
        Ok(answers) => answers,
        Err(why) => {
            let failed = Failed {
                why,
                reply: Some(text.to_owned()),
                timing: usage.timing,
                hung: None,
            };
            return (Err(failed), None);
        }
    };
    (Ok((answers, usage)), None)
}

/// Where `bot`'s turn `turn`, sent at `submitted_ms` and still running,
/// is: what its row says, and how long since it last wrote, from its
/// history's newest entry. What can't be read is left out.
async fn hung(client: &Client, bot: &str, turn: i64, submitted_ms: i64) -> Value {
    let now = epoch_ms();
    let read = client
        .request("turns", json!({"bot": bot, "after": turn - 1, "limit": 1}))
        .await
        .unwrap_or_default();
    let row = &read["turns"][0];
    let mut state = json!({"status": row["status"], "model_rounds": row["model_rounds"],
        "retries": row["retries"], "paced_ms": row["paced_ms"],
        "submitted_ms_ago": now - submitted_ms});
    if let Some(started) = row["started_ms"].as_i64() {
        state["started_ms_ago"] = json!(now - started);
    }
    if let Some(last) = (entries(client, bot, turn).await)
        .and_then(|entries| entries.iter().map(|(at, _)| *at).max())
    {
        state["entries_ms_ago"] = json!(now - last);
    }
    state
}

/// The tokens and time of `bot`'s ended turn `turn`, sent at `submitted_ms`.
async fn usage(client: &Client, bot: &str, turn: i64, submitted_ms: i64) -> Usage {
    let read = client
        .request("turns", json!({"bot": bot, "after": turn - 1, "limit": 1}))
        .await
        .unwrap_or_default();
    let row = &read["turns"][0];
    let tokens = |key: &str| row[key].as_u64().unwrap_or(0);
    // A turn that completed in one round looked at nothing: its history
    // says no more. One that failed may have run tools its first round
    // called, with no round after to count them.
    let entries = match row["status"] != "completed" || tokens("model_rounds") > 1 {
        true => entries(client, bot, turn).await,
        false => Some(Vec::new()),
    };
    Usage {
        input: tokens("input_tokens"),
        cached: tokens("cached_input_tokens"),
        output: tokens("output_tokens"),
        timing: entries.and_then(|entries| Timing::of(row, submitted_ms, &entries)),
    }
}

/// Delete a judge's fork, ending its turn first if it still runs. One that
/// stays expires on its own.
pub async fn remove(client: &Client, name: &str, running: Option<i64>) {
    if let Some(turn) = running {
        let _ = client
            .request("interrupt", json!({"bot": name, "turn": turn}))
            .await;
        let _ = client
            .request(
                "wait",
                json!({"handles": [format!("{name}/{turn}")], "timeout_ms": 5_000}),
            )
            .await;
    }
    let _ = client.request("delete", json!({"bot": name})).await;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_checks_time_splits_into_waiting_looking_tools_and_the_verdict() {
        let row = json!({"started_ms": 1_000, "finished_ms": 9_000, "paced_ms": 50,
            "model_rounds": 3, "retries": 1});
        // Prompt; a round calling two tools, their outputs; a round
        // calling one, its output; the verdict.
        let entries = [
            (1_000, false),
            (2_000, false),
            (2_000, false),
            (2_500, true),
            (3_000, true),
            (4_000, false),
            (6_000, true),
            (9_000, false),
        ];
        let timing = Timing::of(&row, 700, &entries);
        assert_eq!(
            timing,
            Some(Timing {
                queued_ms: 300,
                looking_ms: 2_000,
                tools_ms: 3_000,
                verdict_ms: 3_000,
                paced_ms: 50,
                rounds: 3,
                retries: 1,
            })
        );
        // One round, nothing looked at: all the verdict's.
        let once = Timing::of(&row, 1_000, &[]).unwrap();
        assert_eq!(
            (once.looking_ms, once.tools_ms, once.verdict_ms),
            (0, 0, 8_000)
        );
        // Either family's tool results are outputs; words are not.
        assert!(tool_output(
            &json!({"type": "function_call_output", "call_id": "c1"})
        ));
        assert!(tool_output(&json!({"role": "user", "content": [
            {"type": "tool_result", "tool_use_id": "t1", "content": "ok"}]})));
        assert!(!tool_output(&json!({"role": "user", "content": [
            {"type": "input_text", "text": "look"}]})));
        // A turn not yet ended has no time to split.
        assert_eq!(Timing::of(&json!({"started_ms": 1}), 0, &[]), None);
    }

    #[test]
    fn a_turns_nodes_are_paged_until_an_earlier_turn() {
        let mut written = Map::new();
        let page = json!({"nodes": [
            {"node": 9, "turn": 5, "written_ms": 90},
            {"node": 8, "turn": 5, "written_ms": 80},
        ], "next_from": 7});
        // Every node is the turn's: it may go on in the next page.
        assert_eq!(turn_nodes(&page, 5, &mut written), Some(7));
        let page = json!({"nodes": [
            {"node": 7, "turn": 5, "written_ms": 70},
            {"node": 6, "turn": 4, "written_ms": 60},
        ], "next_from": 5});
        assert_eq!(turn_nodes(&page, 5, &mut written), None);
        assert_eq!(
            written,
            json!({"7": 70, "8": 80, "9": 90})
                .as_object()
                .cloned()
                .unwrap()
        );
        // The history's start ends it too.
        let page = json!({"nodes": [{"node": 1, "turn": 1}], "next_from": null});
        assert_eq!(turn_nodes(&page, 1, &mut written), None);
    }

    #[test]
    fn a_turn_read_in_parts_is_read_whole_or_not_at_all() {
        let page = |nodes: &[i64]| -> Vec<Value> {
            nodes
                .iter()
                .map(|node| json!({"node": node, "item": {}}))
                .collect()
        };
        // Cut at its size: the rest is asked for next.
        assert_eq!(rest(&[4, 5, 6], &page(&[4])), Some(vec![5, 6]));
        assert_eq!(rest(&[5, 6], &page(&[5, 6])), Some(vec![]));
        // A read that reads none of them ends the reading.
        assert_eq!(rest(&[5, 6], &[]), None);
    }

    #[test]
    fn a_judge_is_named_provider_slash_model() {
        assert!(is_model("bedrock/claude-haiku"));
        assert!(!is_model("claude"));
        assert!(!is_model("/claude"));
        assert!(!is_model("openai/"));
    }

    #[test]
    fn with_no_judge_named_the_bot_s_own_model_judges() {
        assert_eq!(
            pick(Some("openai/gpt-6-luna"), Some("chatgpt/gpt-6-sol")).as_deref(),
            Some("openai/gpt-6-luna")
        );
        assert_eq!(
            pick(None, Some("chatgpt/gpt-6-sol")).as_deref(),
            Some("chatgpt/gpt-6-sol")
        );
        assert_eq!(pick(None, None), None);
    }

    #[test]
    fn a_judge_has_its_first_sentence_and_only_read_only_tools() {
        let said = "Answer yes or no. Reply in JSON.";
        assert!(is_judge(&json!({"tools": [], "instructions": said}), said));
        // An older version of the same judge is still one, to be remade.
        let older = json!({"tools": [], "instructions": "Answer yes or no. Reply in YAML."});
        assert!(is_judge(&older, said));
        let looking = json!({"tools": ["shell", "read"], "instructions": said,
            "settings": {"sandbox": "read-only"}});
        assert!(is_judge(&looking, said));
        // Tools outside a read-only sandbox, or ones that write, are not a
        // judge's.
        assert!(!is_judge(
            &json!({"tools": ["shell"], "instructions": said}),
            said
        ));
        assert!(!is_judge(
            &json!({"tools": ["shell"], "instructions": said,
                "settings": {"sandbox": "workspace-write"}}),
            said
        ));
        assert!(!is_judge(
            &json!({"tools": ["write"], "instructions": said,
                "settings": {"sandbox": "read-only"}}),
            said
        ));
        assert!(!is_judge(
            &json!({"tools": [], "instructions": "Help with the repo."}),
            said
        ));
        assert!(!is_judge(
            &json!({"tools": [], "instructions": "Answer yes"}),
            said
        ));
    }
}
