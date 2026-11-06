//! The judge `sonny approver` asks about planned calls: which one, picked
//! in one order, and how it answers. A judge is a `provider/model` the
//! daemon runs, as a fork of an idle base bot that takes one turn, so
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
/// (`--judge`, else the config's `[judge] model`), else the bot's own model. No model is
/// guessed for a provider.
pub fn pick(named: Option<&str>, caller: Option<&str>) -> Option<String> {
    named.or(caller).map(str::to_owned)
}

/// Each id asked, and the judge's answer for it.
pub type Answers = Map<String, Value>;

/// What a judge read and wrote for one check, in tokens, as its provider
/// reported them, and where its time went.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct Usage {
    pub input: u64,
    /// Of `input`, what the provider had cached.
    pub cached: u64,
    pub output: u64,
    pub timing: Timing,
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
    /// turn's start, to its end.
    pub fn of(row: &Value, submitted_ms: i64, entries: &[(i64, bool)]) -> Self {
        let at = |key: &str| row[key].as_i64();
        let (Some(started), Some(finished)) = (at("started_ms"), at("finished_ms")) else {
            return Self::default();
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
        Self {
            queued_ms: span(submitted_ms, started),
            looking_ms: span(started, answered_from) - tools_ms,
            tools_ms,
            verdict_ms: span(answered_from, finished),
            paced_ms: row["paced_ms"].as_u64().unwrap_or(0),
            rounds: row["model_rounds"].as_u64().unwrap_or(0),
            retries: row["retries"].as_u64().unwrap_or(0),
        }
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
/// was written and whether it is a tool's output. Empty when it could not
/// be read: the time then reads as all the verdict's.
async fn entries(client: &Client, bot: &str, turn: i64) -> Vec<(i64, bool)> {
    let Ok(page) = client
        .request("history_nodes", json!({"bot": bot, "limit": 400}))
        .await
    else {
        return Vec::new();
    };
    let written: Map<String, Value> = (page["nodes"].as_array().into_iter().flatten())
        .take_while(|node| node["turn"].as_i64() == Some(turn))
        .filter_map(|node| {
            Some((
                node["node"].as_i64()?.to_string(),
                node["written_ms"].clone(),
            ))
        })
        .collect();
    let nodes: Vec<i64> = written
        .keys()
        .filter_map(|node| node.parse().ok())
        .collect();
    let Ok(items) = client
        .request("history_items", json!({"bot": bot, "nodes": nodes}))
        .await
    else {
        return Vec::new();
    };
    let mut entries: Vec<(i64, i64, bool)> = (items["items"].as_array().into_iter().flatten())
        .filter_map(|entry| {
            let node = entry["node"].as_i64()?;
            let at = written.get(&node.to_string())?.as_i64()?;
            Some((node, at, tool_output(&entry["item"])))
        })
        .collect();
    entries.sort_unstable();
    entries
        .into_iter()
        .map(|(_, at, output)| (at, output))
        .collect()
}

/// Why a judge gave no answers: what failed in this check.
#[derive(Debug, PartialEq)]
pub struct Failed {
    pub why: String,
    /// The judge's reply, when there was one it could not be read as
    /// answers: whole, so whoever logs it redacts it before [`excerpt`]
    /// cuts it, and no secret is cut where redaction would miss it.
    pub reply: Option<String>,
    /// Where the judge's turn spent its time, when it ended: none for one
    /// that never started or was still running when the check gave up.
    pub timing: Option<Timing>,
}

fn failed(why: impl Into<String>) -> Failed {
    Failed {
        why: why.into(),
        reply: None,
        timing: None,
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
                    "judge_name_taken",
                    &format!("{name} is a bot that is not a judge; rename it"),
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
                        "judge_name_taken",
                        &format!("{name} is a bot that is not a judge; rename it"),
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
        forked.map_err(|error| failed(format!("the judge could not start: {}", error.code)))?;
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
        return (Err(failed("the judge's reply schema is not JSON")), None);
    };
    // Queued when every slot is taken: it starts when one frees, within
    // the deadline.
    let submitted_ms = epoch_ms();
    let submitted = match client
        .request(
            "submit",
            // Said by the judge, not the person: clients draw it so.
            json!({"bot": name, "request_id": name, "prompt": prompt, "delivery": "queue",
                "origin": "judge", "output_schema": schema}),
        )
        .await
    {
        Ok(submitted) => submitted,
        Err(error) => {
            let why = format!("the judge's turn was refused: {}", error.code);
            return (Err(failed(why)), None);
        }
    };
    let (Some(turn), Some(handle)) = (submitted["turn"].as_i64(), submitted["handle"].as_str())
    else {
        return (Err(failed("the judge's turn was not started")), None);
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
            let why = format!("the judge could not be waited on: {}", error.code);
            return (Err(failed(why)), Some(turn));
        }
    };
    let outcome = &waited["results"][handle];
    if outcome["pending"] == true {
        return (Err(failed("the check timed out")), Some(turn));
    }
    let usage = usage(client, name, turn, submitted_ms).await;
    if outcome["status"] != "completed" {
        let why = format!(
            "the judge's turn {}: {}",
            outcome["status"].as_str().unwrap_or("ended"),
            outcome["error"].as_str().unwrap_or("no answer")
        );
        let failed = Failed {
            timing: Some(usage.timing),
            ..failed(why)
        };
        return (Err(failed), None);
    }
    let text = outcome["text"].as_str().unwrap_or_default();
    let parsed = match outcome["text_truncated"] == true {
        true => Err("the judge's answer was cut off".to_owned()),
        false => (reply.parse)(text, ids),
    };
    let answers = match parsed {
        Ok(answers) => answers,
        Err(why) => {
            let failed = Failed {
                why,
                reply: Some(text.to_owned()),
                timing: Some(usage.timing),
            };
            return (Err(failed), None);
        }
    };
    (Ok((answers, usage)), None)
}

/// The tokens and time of `bot`'s ended turn `turn`, sent at `submitted_ms`.
async fn usage(client: &Client, bot: &str, turn: i64, submitted_ms: i64) -> Usage {
    let read = client
        .request("turns", json!({"bot": bot, "after": turn - 1, "limit": 1}))
        .await
        .unwrap_or_default();
    let row = &read["turns"][0];
    let tokens = |key: &str| row[key].as_u64().unwrap_or(0);
    // A turn of one round looked at nothing: its history says no more.
    let entries = match tokens("model_rounds") > 1 {
        true => entries(client, bot, turn).await,
        false => Vec::new(),
    };
    Usage {
        input: tokens("input_tokens"),
        cached: tokens("cached_input_tokens"),
        output: tokens("output_tokens"),
        timing: Timing::of(row, submitted_ms, &entries),
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
                json!({"handles": [format!("turn:{name}/{turn}")], "timeout_ms": 5_000}),
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
            Timing {
                queued_ms: 300,
                looking_ms: 2_000,
                tools_ms: 3_000,
                verdict_ms: 3_000,
                paced_ms: 50,
                rounds: 3,
                retries: 1,
            }
        );
        // One round, nothing looked at: all the verdict's.
        let once = Timing::of(&row, 1_000, &[]);
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
        assert_eq!(
            Timing::of(&json!({"started_ms": 1}), 0, &[]),
            Timing::default()
        );
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
