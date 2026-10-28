//! The judge that `agent approver` and Sonny's lookout ask yes/no questions
//! of: which one, picked in one order, and its two ways of answering. A
//! judge named `openai-decisions/MODEL` is OpenAI's Decisions API, asked
//! over HTTP; any other `provider/model` runs in the daemon, as a fork of
//! an idle base bot that takes one turn, so checks run at once and none
//! sees another. Either answers with each question's probability of yes.
use crate::{Client, Error};
use serde_json::{Map, Value, json};
use std::{
    sync::{
        Mutex,
        atomic::{AtomicU64, Ordering},
    },
    time::{Duration, Instant},
};

/// The key OpenAI's Decisions API takes.
pub const DECISIONS_KEY: &str = "OPENAI_API_KEY";
/// Where OpenAI's Decisions API is, unless the caller points elsewhere.
pub const DECISIONS_URL: &str = "https://api.openai.com";
const DECISIONS: &str = "openai-decisions/";

/// How a judge's name says it answers.
#[derive(Debug, PartialEq)]
pub enum Spec<'a> {
    /// OpenAI's Decisions API, with this model.
    Decisions(&'a str),
    /// A `provider/model` the daemon runs, with the providers and logins
    /// bots use.
    Model(&'a str),
}

/// What a judge's name says it is: `None` when it is not `provider/model`.
pub fn spec(name: &str) -> Option<Spec<'_>> {
    let (provider, model) = name.split_once('/')?;
    if provider.is_empty() || model.is_empty() {
        return None;
    }
    Some(match name.strip_prefix(DECISIONS) {
        Some(model) => Spec::Decisions(model),
        None => Spec::Model(name),
    })
}

/// Who judges for a bot running `caller`: the judge `named` by the caller
/// (`--judge`, else the config's `[judge] model`), else the bot's own model. No model is
/// guessed for a provider.
pub fn pick(named: Option<&str>, caller: Option<&str>) -> Option<String> {
    named.or(caller).map(str::to_owned)
}

/// Each question's id, and the probability, from 0 to 1, that its answer
/// is yes.
pub type Answers = Map<String, Value>;

/// What a judge read and wrote for one check, in tokens, as its provider
/// reported them.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct Usage {
    pub input: u64,
    /// Of `input`, what the provider had cached.
    pub cached: u64,
    pub output: u64,
}

/// Why a judge gave no answers.
#[derive(Debug, PartialEq)]
pub enum Failed {
    /// Its rate limit lasted past the deadline.
    Overloaded,
    /// The account is out of credit, which no wait fixes.
    Quota,
    /// This check failed, and why.
    Check(String),
}

fn failed(why: impl Into<String>) -> Failed {
    Failed::Check(why.into())
}

/// A check as a `POST /v1/decisions` body: `instructions` as the developer
/// message, `text` as the user's, and each question a predicate named by
/// its id.
pub fn decisions_body(
    model: &str,
    instructions: &str,
    text: &str,
    questions: &[(String, String)],
) -> Value {
    json!({
        "model": model,
        "input": [
            {"role": "developer", "content": [{"type": "input_text", "text": instructions}]},
            {"role": "user", "content": [{"type": "input_text", "text": text}]},
        ],
        "questions": questions.iter()
            .map(|(id, text)| json!({"type": "predicate", "name": id, "instructions": text}))
            .collect::<Vec<_>>(),
    })
}

/// A Decisions reply's answers, or why it is not one: a refusal of any
/// question, by its id and what the API said, or a question missing or out
/// of range.
pub fn decisions_answers(reply: &Value, questions: &[(String, String)]) -> Result<Answers, String> {
    let given = reply["answers"]
        .as_array()
        .ok_or("the judge's answer had no answers")?;
    let mut answers = Map::new();
    for answer in given {
        let Some(name) = answer["name"].as_str() else {
            continue;
        };
        match answer["type"].as_str() {
            Some("refusal") => {
                let mut said = answer.clone();
                if let Some(said) = said.as_object_mut() {
                    said.remove("name");
                    said.remove("type");
                }
                return Err(format!("the judge refused {name}: {said}"));
            }
            Some("predicate") => {
                if let Some(p) = answer["probability"]
                    .as_f64()
                    .filter(|p| (0.0..=1.0).contains(p))
                {
                    answers.insert(name.to_owned(), json!(p));
                }
            }
            _ => {}
        }
    }
    match questions.iter().all(|(id, _)| answers.contains_key(id)) {
        true => Ok(answers),
        false => Err("the judge's answer missed a question".into()),
    }
}

/// A general model's reply as answers: its JSON object, found in the
/// reply, with a probability for every id asked. `None` when any is
/// missing or out of range, a failed check.
pub fn model_answers<'a>(reply: &str, ids: impl IntoIterator<Item = &'a str>) -> Option<Answers> {
    let start = reply.find('{')?;
    let end = reply.rfind('}')?;
    let given: Map<String, Value> = serde_json::from_str(reply.get(start..=end)?).ok()?;
    let mut answers = Map::new();
    for id in ids {
        let p = given
            .get(id)?
            .as_f64()
            .filter(|p| (0.0..=1.0).contains(p))?;
        answers.insert(id.to_owned(), json!(p));
    }
    Some(answers)
}

/// The largest answer the Decisions API may send; a larger one is a failed check.
const REPLY_BYTES: usize = 64 * 1024;
/// First wait after a 429 or 529 without Retry-After, doubled each time.
const BACKOFF: Duration = Duration::from_millis(250);
const BACKOFF_MAX: Duration = Duration::from_secs(4);
/// The longest Retry-After honored, past any check's deadline anyway.
const RETRY_MAX_SECS: f64 = 60.0;

/// OpenAI's Decisions API, which answers a check's questions with
/// probabilities and bills input only.
pub struct Decisions {
    http: reqwest::Client,
    url: String,
    model: String,
    key: String,
    /// Shared by every check: after a 429 or 529 nobody asks before this.
    backoff: Mutex<(Option<Instant>, Duration)>,
}

impl Decisions {
    /// `model` at `url`, `DECISIONS_URL` when `None`, with `key`.
    pub fn new(model: &str, url: Option<&str>, key: String) -> crate::Result<Self> {
        let http = reqwest::Client::builder()
            .user_agent(concat!("agent-client/", env!("CARGO_PKG_VERSION")))
            .redirect(reqwest::redirect::Policy::none())
            .connect_timeout(Duration::from_secs(5))
            .build()
            .map_err(|_| Error::new("http_client_init"))?;
        Ok(Self {
            http,
            url: url
                .unwrap_or(DECISIONS_URL)
                .trim_end_matches('/')
                .to_owned(),
            model: model.to_owned(),
            key,
            backoff: Mutex::new((None, BACKOFF)),
        })
    }

    /// The body this judge sends for a check.
    pub fn body(&self, instructions: &str, text: &str, questions: &[(String, String)]) -> Value {
        decisions_body(&self.model, instructions, text, questions)
    }

    /// Each question's probability, by `deadline`, waiting out the API's
    /// rate limit within it.
    pub async fn ask(
        &self,
        instructions: &str,
        text: &str,
        questions: &[(String, String)],
        deadline: Instant,
    ) -> Result<(Answers, Usage), Failed> {
        let body = self.body(instructions, text, questions).to_string();
        loop {
            let until = self.backoff.lock().unwrap().0;
            if let Some(until) = until.filter(|until| *until > Instant::now()) {
                if until >= deadline {
                    return Err(Failed::Overloaded);
                }
                tokio::time::sleep_until(until.into()).await;
            }
            let Some(left) = deadline.checked_duration_since(Instant::now()) else {
                return Err(Failed::Overloaded);
            };
            let sent = self
                .http
                .post(format!("{}/v1/decisions", self.url))
                .bearer_auth(&self.key)
                .header(reqwest::header::CONTENT_TYPE, "application/json")
                .body(body.clone())
                .timeout(left)
                .send()
                .await;
            let response = match sent {
                Ok(response) => response,
                Err(error) if error.is_timeout() => return Err(failed("the check timed out")),
                Err(_) => return Err(failed("the check failed")),
            };
            let status = response.status().as_u16();
            if matches!(status, 429 | 529) {
                let retry = response
                    .headers()
                    .get(reqwest::header::RETRY_AFTER)
                    .and_then(|value| value.to_str().ok()?.parse::<f64>().ok())
                    .filter(|seconds| seconds.is_finite() && *seconds >= 0.0)
                    .map(|seconds| Duration::from_secs_f64(seconds.min(RETRY_MAX_SECS)));
                // A 429 also means the account is out of credit, which no
                // wait fixes; its body names that by a published code.
                let error: Value = read_reply(response)
                    .await
                    .ok()
                    .and_then(|bytes| serde_json::from_slice(&bytes).ok())
                    .unwrap_or_default();
                if error["error"]["code"] == "insufficient_quota" {
                    return Err(Failed::Quota);
                }
                let mut backoff = self.backoff.lock().unwrap();
                // A server's shorter wait, zero included, still paces.
                let wait = retry.map_or(backoff.1, |retry| retry.max(backoff.1));
                backoff.1 = (backoff.1 * 2).min(BACKOFF_MAX);
                backoff.0 = Some(Instant::now() + wait);
                continue;
            }
            if !(200..300).contains(&status) {
                return Err(failed(format!("the judge answered {status}")));
            }
            {
                // An answer to a request sent before another was throttled
                // leaves that newer wait in place.
                let mut backoff = self.backoff.lock().unwrap();
                if backoff.0.is_none_or(|until| until <= Instant::now()) {
                    *backoff = (None, BACKOFF);
                }
            }
            let bytes = read_reply(response).await.map_err(failed)?;
            let reply: Value = serde_json::from_slice(&bytes)
                .map_err(|_| failed("the judge's answer was not JSON"))?;
            let usage = Usage {
                input: reply["usage"]["input_tokens"].as_u64().unwrap_or(0),
                cached: reply["usage"]["input_tokens_details"]["cached_tokens"]
                    .as_u64()
                    .unwrap_or(0),
                ..Usage::default()
            };
            return decisions_answers(&reply, questions)
                .map(|answers| (answers, usage))
                .map_err(Failed::Check);
        }
    }
}

/// A response's body, up to the largest answer the API may send.
async fn read_reply(mut response: reqwest::Response) -> Result<Vec<u8>, &'static str> {
    let mut bytes = Vec::new();
    loop {
        match response.chunk().await {
            Ok(Some(chunk)) if bytes.len() + chunk.len() <= REPLY_BYTES => {
                bytes.extend_from_slice(&chunk);
            }
            Ok(Some(_)) => return Err("the judge's answer was too large"),
            Ok(None) => return Ok(bytes),
            Err(_) => return Err("the check failed"),
        }
    }
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
            Ok(bot) if same(&bot) => return base(&bot),
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

    /// One check: a fork reads `prompt` and answers every id with a
    /// probability by `deadline`.
    pub async fn ask(
        &self,
        client: &Client,
        prompt: &str,
        ids: &[&str],
        deadline: Instant,
    ) -> Result<(Answers, Usage), Failed> {
        self.ask_with(client, prompt, ids, probabilities, None, deadline)
            .await
    }

    /// One check whose reply `parse` reads as answers to every id, its
    /// fork started in `workspace` (else `/`), where a judge with tools
    /// looks. A workspace since removed starts it in `/`.
    pub async fn ask_with(
        &self,
        client: &Client,
        prompt: &str,
        ids: &[&str],
        parse: Parse,
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
        let (answered, running) = turn_with(client, &name, prompt, ids, parse, deadline).await;
        remove(client, &name, running).await;
        answered
    }
}

/// How a judge's reply is read as answers to the ids asked: `None` when it
/// does not answer them all, a failed check.
pub type Parse = fn(&str, &[&str]) -> Option<Answers>;

/// A reply read as a probability for every id.
pub fn probabilities(reply: &str, ids: &[&str]) -> Option<Answers> {
    model_answers(reply, ids.iter().copied())
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

/// One turn of a judge's fork `name`: its probabilities for every id by
/// `deadline`, and the turn if it is still running.
pub async fn turn(
    client: &Client,
    name: &str,
    prompt: &str,
    ids: &[&str],
    deadline: Instant,
) -> (Result<(Answers, Usage), Failed>, Option<i64>) {
    turn_with(client, name, prompt, ids, probabilities, deadline).await
}

/// One turn of a judge's fork, its reply read by `parse`.
pub async fn turn_with(
    client: &Client,
    name: &str,
    prompt: &str,
    ids: &[&str],
    parse: Parse,
    deadline: Instant,
) -> (Result<(Answers, Usage), Failed>, Option<i64>) {
    // Queued when every slot is taken: it starts when one frees, within
    // the deadline.
    let submitted = match client
        .request(
            "submit",
            json!({"bot": name, "request_id": name, "prompt": prompt, "delivery": "queue"}),
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
    if outcome["status"] != "completed" {
        let why = format!(
            "the judge's turn {}: {}",
            outcome["status"].as_str().unwrap_or("ended"),
            outcome["error"].as_str().unwrap_or("no answer")
        );
        return (Err(failed(why)), None);
    }
    let Some(answers) = (outcome["text_truncated"] != true)
        .then(|| parse(outcome["text"].as_str().unwrap_or_default(), ids))
        .flatten()
    else {
        return (Err(failed("the judge's answer did not parse")), None);
    };
    let read = client
        .request("turns", json!({"bot": name, "after": turn - 1, "limit": 1}))
        .await
        .unwrap_or_default();
    let row = &read["turns"][0];
    let tokens = |key: &str| row[key].as_u64().unwrap_or(0);
    let usage = Usage {
        input: tokens("input_tokens"),
        cached: tokens("cached_input_tokens"),
        output: tokens("output_tokens"),
    };
    (Ok((answers, usage)), None)
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
    fn a_judge_is_named_by_how_it_answers() {
        assert_eq!(
            spec("openai-decisions/gpt-6-luna"),
            Some(Spec::Decisions("gpt-6-luna"))
        );
        assert_eq!(
            spec("bedrock/claude-haiku"),
            Some(Spec::Model("bedrock/claude-haiku"))
        );
        assert_eq!(spec("claude"), None);
        assert_eq!(spec("/claude"), None);
        assert_eq!(spec("openai-decisions/"), None);
    }

    #[test]
    fn with_no_judge_named_the_bot_s_own_model_judges() {
        assert_eq!(
            pick(
                Some("openai-decisions/gpt-6-luna"),
                Some("chatgpt/gpt-6-sol")
            )
            .as_deref(),
            Some("openai-decisions/gpt-6-luna")
        );
        assert_eq!(
            pick(None, Some("chatgpt/gpt-6-sol")).as_deref(),
            Some("chatgpt/gpt-6-sol")
        );
        assert_eq!(pick(None, None), None);
    }

    #[test]
    fn decisions_asks_each_question_as_a_predicate() {
        let asked = vec![
            ("a".to_owned(), "Is it a?".to_owned()),
            ("b".to_owned(), "Is it b?".to_owned()),
        ];
        let body = decisions_body("gpt-6-luna", "Read it.", "{}", &asked);
        assert_eq!(body["model"], "gpt-6-luna");
        assert_eq!(body["input"][0]["role"], "developer");
        assert_eq!(body["input"][0]["content"][0]["text"], "Read it.");
        assert_eq!(body["input"][1]["content"][0]["text"], "{}");
        assert_eq!(
            body["questions"][1],
            json!({"type": "predicate", "name": "b", "instructions": "Is it b?"})
        );
        let every = json!([
            {"type": "predicate", "name": "a", "probability": 0.1},
            {"type": "predicate", "name": "b", "probability": 0.9},
        ]);
        let answers = decisions_answers(&json!({"answers": every}), &asked).unwrap();
        assert_eq!(answers["b"], json!(0.9));
        let refused = json!([
            {"type": "predicate", "name": "a", "probability": 0.1},
            {"type": "refusal", "name": "b", "refusal": "I can't help with that."},
        ]);
        assert_eq!(
            decisions_answers(&json!({"answers": refused}), &asked),
            Err(r#"the judge refused b: {"refusal":"I can't help with that."}"#.into())
        );
        let wild = json!([
            {"type": "predicate", "name": "a", "probability": 1.5},
            {"type": "predicate", "name": "b", "probability": 0.9},
        ]);
        assert!(decisions_answers(&json!({"answers": wild}), &asked).is_err());
        assert!(decisions_answers(&json!({"answers": [every[0]]}), &asked).is_err());
        assert!(decisions_answers(&json!({}), &asked).is_err());
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

    #[test]
    fn a_general_model_answers_in_one_json_object() {
        let reply = "Here you go:\n```json\n{\"a\": 0.1, \"b\": 0.9}\n```";
        let answers = model_answers(reply, ["a", "b"]).unwrap();
        assert_eq!(answers["a"], json!(0.1));
        assert_eq!(model_answers(reply, ["a", "c"]), None);
        assert_eq!(model_answers("{\"a\": 1.5}", ["a"]), None);
        assert_eq!(model_answers("no json here", ["a"]), None);
    }
}
