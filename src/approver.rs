//! `agent approver`: serve a gate tag and have a judge model decide every
//! call waiting on it, in one request a round. The policy (the questions,
//! the verdicts, what the judge sees, and redaction) is
//! `agent_client::approver`; this is the loop and the judge's transport.
//! It never pages a person: a round it cannot judge in time is denied with
//! the reason, and the model can ask its caller instead.
use agent_client::{
    Client,
    approver::{self as policy, Allowed, By, Intent, Planned, Redactor, State, Verdict, Words},
};
use agent_runtime::{Error, Result};
use serde_json::{Value, json};
use std::{
    collections::{HashMap, HashSet},
    path::{Path, PathBuf},
    sync::{
        Arc, Mutex,
        atomic::{AtomicU64, Ordering},
    },
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};
use tokio::sync::{Notify, Semaphore};

pub struct Settings {
    pub socket: PathBuf,
    pub tag: String,
    pub judge: JudgeSpec,
    /// The environment note the judge always sees.
    pub note: Option<String>,
}

/// Who judges: Jev over its own API, or any model the daemon runs.
pub enum JudgeSpec {
    Jev {
        url: String,
        model: String,
        key: String,
    },
    /// A `provider/model` the daemon serves, with an optional effort.
    Model {
        model: String,
        reasoning: Option<String>,
    },
}

/// The lease a tag is served under, renewed well inside it.
const LEASE_MS: u64 = 5_000;
const RENEW: Duration = Duration::from_secs(1);
/// Judge requests in flight at once; a round waits for a slot within its
/// deadline, so the queue holds what the deadline allows and no more.
const IN_FLIGHT: usize = 32;
/// Text one `prompts` read returns: past the judge's whole state limit.
/// The prompts of a round's delegating turns share it too.
const PROMPT_BYTES: usize = 128 * 1024;
/// How many delegations deep the approver follows up to the person's
/// words, and how many delegating turns it reads for one round; past either
/// the round is too long to judge, rather than judged without them.
const CHAIN: usize = 8;
const DELEGATING: usize = 64;
/// The largest file written this turn that a judged call may run: a call
/// that runs a larger one is refused rather than judged on its first part.
const FILE_BYTES: u64 = 48 * 1024;
/// The largest answer Jev may send; a larger one is a failed check.
const REPLY_BYTES: usize = 64 * 1024;
/// First wait after a 429 or 529 without Retry-After, doubled each time.
const BACKOFF: Duration = Duration::from_millis(250);
const BACKOFF_MAX: Duration = Duration::from_secs(4);
/// The longest Retry-After honored, past any round's deadline anyway.
const RETRY_MAX_SECS: f64 = 60.0;
/// A judge fork is `approver.TAG.PID.N`, and every bot name fits 128 bytes.
const JUDGE_NAME_ROOM: usize = 128 - "approver.".len() - ".4294967295.18446744073709551615".len();

fn epoch_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_millis() as i64)
}

pub fn main(settings: Settings) -> Result<i32> {
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()?
        .block_on(serve(settings))
}

/// What a judge request came back with.
enum Judged {
    Answers {
        answers: Value,
        usage: Usage,
    },
    /// No slot, or the judge's rate limit, before the round's deadline.
    Overloaded,
    Failed(String),
}

/// Tokens a judge request used, as its provider reported them.
#[derive(Default)]
struct Usage {
    input: u64,
    cached: u64,
    output: u64,
}

struct Judge {
    via: Via,
    /// Taken before a round reads anything, so rounds past these wait
    /// holding only their calls.
    slots: Semaphore,
    /// How long a round may wait for its verdict, from its announcement.
    deadline_ms: u64,
}

enum Via {
    Jev(Jev),
    Model(Model),
}

impl Judge {
    async fn ask(
        &self,
        client: &Arc<Client>,
        state: &Value,
        questions: &Value,
        deadline: Instant,
    ) -> Judged {
        match &self.via {
            Via::Jev(jev) => jev.ask(state, questions, deadline).await,
            Via::Model(model) => model.ask(client, state, questions, deadline).await,
        }
    }
}

struct Jev {
    http: reqwest::Client,
    url: String,
    model: String,
    key: String,
    /// Shared by every round: after a 429 or 529 nobody asks before this.
    backoff: Mutex<(Option<Instant>, Duration)>,
}

impl Jev {
    async fn ask(&self, state: &Value, questions: &Value, deadline: Instant) -> Judged {
        let body = match serde_json::to_vec(
            &json!({"model":self.model,"state":state,"questions":questions}),
        ) {
            Ok(body) => body,
            Err(_) => return Judged::Failed("the request could not be encoded".into()),
        };
        loop {
            let until = self.backoff.lock().unwrap().0;
            if let Some(until) = until.filter(|until| *until > Instant::now()) {
                if until >= deadline {
                    return Judged::Overloaded;
                }
                tokio::time::sleep_until(until.into()).await;
            }
            let Some(left) = deadline.checked_duration_since(Instant::now()) else {
                return Judged::Overloaded;
            };
            let sent = self
                .http
                .post(format!("{}/v1/systemone", self.url))
                .header(
                    reqwest::header::AUTHORIZATION,
                    format!("Bearer {}", self.key),
                )
                .header(reqwest::header::CONTENT_TYPE, "application/json")
                .body(body.clone())
                .timeout(left)
                .send()
                .await;
            let response = match sent {
                Ok(response) => response,
                Err(error) if error.is_timeout() => {
                    return Judged::Failed("the check timed out".into());
                }
                Err(_) => return Judged::Failed("the check failed".into()),
            };
            let status = response.status().as_u16();
            if matches!(status, 429 | 529) {
                let retry = response
                    .headers()
                    .get(reqwest::header::RETRY_AFTER)
                    .and_then(|value| value.to_str().ok()?.parse::<f64>().ok())
                    .filter(|seconds| seconds.is_finite() && *seconds >= 0.0)
                    .map(|seconds| Duration::from_secs_f64(seconds.min(RETRY_MAX_SECS)));
                let mut backoff = self.backoff.lock().unwrap();
                // A server's shorter wait, zero included, still paces.
                let wait = retry.map_or(backoff.1, |retry| retry.max(backoff.1));
                backoff.1 = (backoff.1 * 2).min(BACKOFF_MAX);
                backoff.0 = Some(Instant::now() + wait);
                continue;
            }
            if !(200..300).contains(&status) {
                return Judged::Failed(format!("the judge answered {status}"));
            }
            {
                // An answer to a request sent before another was throttled
                // leaves that newer wait in place.
                let mut backoff = self.backoff.lock().unwrap();
                if backoff.0.is_none_or(|until| until <= Instant::now()) {
                    *backoff = (None, BACKOFF);
                }
            }
            let mut response = response;
            let mut bytes = Vec::new();
            loop {
                match response.chunk().await {
                    Ok(Some(chunk)) if bytes.len() + chunk.len() <= REPLY_BYTES => {
                        bytes.extend_from_slice(&chunk);
                    }
                    Ok(Some(_)) => {
                        return Judged::Failed("the judge's answer was too large".into());
                    }
                    Ok(None) => break,
                    Err(_) => return Judged::Failed("the check failed".into()),
                }
            }
            let Ok(reply) = serde_json::from_slice::<Value>(&bytes) else {
                return Judged::Failed("the judge's answer was not JSON".into());
            };
            return Judged::Answers {
                usage: Usage {
                    input: reply["usage"]["input_tokens"].as_u64().unwrap_or(0),
                    ..Usage::default()
                },
                answers: reply["answers"].clone(),
            };
        }
    }
}

/// A general model judges through the daemon, with the providers and logins
/// bots use. Each round forks an idle base bot that holds the judge's
/// instructions and no tools, so rounds run at once and none sees another,
/// and the fork is deleted once the round is answered.
struct Model {
    base: String,
    /// The base's identity, which each fork names as its creator.
    base_id: i64,
    next: AtomicU64,
}

impl Model {
    /// A fresh base, so a changed judge takes effect, and no forks left by
    /// an approver that stopped mid-round. Only the tag's holder does this,
    /// and it removes only judges: a bot of the same name that is not one
    /// is refused, not deleted.
    async fn prepare(
        client: &Arc<Client>,
        base: String,
        model: &str,
        reasoning: Option<&str>,
    ) -> Result<Model> {
        match client.request("resume", json!({"bot":base})).await {
            Ok(bot) if !judge_bot(&bot) => {
                return Err(Error::with(
                    "approver_name_taken",
                    format!("{base} is a bot that is not a judge; rename it to serve this tag"),
                ));
            }
            Err(error) if error.code != "bot_not_found" => return Err(error.into()),
            _ => {}
        }
        // Bots are listed in name order, so the judge's forks are the run
        // of names right after its prefix: only those pages are read.
        let prefix = format!("{base}.");
        let mut after = json!(prefix);
        'pages: loop {
            let page = client
                .request("bots", json!({"after":after,"limit":64}))
                .await
                .map_err(Error::from)?;
            for bot in page["bots"].as_array().into_iter().flatten() {
                let Some(name) = bot["name"]
                    .as_str()
                    .filter(|name| name.starts_with(&prefix))
                else {
                    break 'pages;
                };
                if bot["created_by"] == base.as_str()
                    && bot["tools"].as_array().is_some_and(Vec::is_empty)
                {
                    // Aside, so a turn still ending does not hold up serving.
                    let (client, name) = (client.clone(), name.to_owned());
                    let running = bot["running_turn"].as_i64();
                    tokio::spawn(async move { remove(&client, &name, running).await });
                }
            }
            match &page["next_after"] {
                Value::Null => break,
                next => after = next.clone(),
            }
        }
        match client.request("delete", json!({"bot":base})).await {
            Err(error) if error.code != "bot_not_found" => return Err(error.into()),
            _ => {}
        }
        let created = client
            .request(
                "create",
                json!({"bot":base,"workspace":"/","model":model,"reasoning":reasoning,
                    "instructions":policy::JUDGE_INSTRUCTIONS,"tools":[]}),
            )
            .await
            .map_err(Error::from)?;
        Ok(Model {
            base_id: created["id"]
                .as_i64()
                .ok_or(Error::new("daemon_protocol_mismatch"))?,
            base,
            next: AtomicU64::new(0),
        })
    }

    async fn ask(
        &self,
        client: &Arc<Client>,
        state: &Value,
        questions: &Value,
        deadline: Instant,
    ) -> Judged {
        let name = format!(
            "{}.{}.{}",
            self.base,
            std::process::id(),
            self.next.fetch_add(1, Ordering::Relaxed)
        );
        if let Err(error) = client
            .request(
                "fork",
                json!({"source":self.base,"bot":name,"workspace":"/","created_by":self.base,
                    "created_by_id":self.base_id}),
            )
            .await
        {
            return Judged::Failed(format!("the judge could not start: {}", error.code));
        }
        let (judged, running) = turn(client, &name, state, questions, deadline).await;
        let client = client.clone();
        tokio::spawn(async move { remove(&client, &name, running).await });
        judged
    }
}

/// Whether a bot record is a judge an approver made: no tools, and the
/// judge's instructions exactly.
fn judge_bot(bot: &Value) -> bool {
    bot["tools"].as_array().is_some_and(Vec::is_empty)
        && bot["instructions"] == policy::JUDGE_INSTRUCTIONS
}

/// The judge's one turn, and the turn if it is still running.
async fn turn(
    client: &Client,
    name: &str,
    state: &Value,
    questions: &Value,
    deadline: Instant,
) -> (Judged, Option<i64>) {
    // Queued when every slot is taken, often by the turn being judged
    // until it parks: it starts when one frees, within the deadline.
    let submitted = match client
        .request(
            "submit",
            json!({"bot":name,"request_id":name,"prompt":policy::model_prompt(state, questions),
                "delivery":"queue"}),
        )
        .await
    {
        Ok(submitted) => submitted,
        Err(error) => {
            return (
                Judged::Failed(format!("the judge's turn was refused: {}", error.code)),
                None,
            );
        }
    };
    let (Some(turn), Some(handle)) = (submitted["turn"].as_i64(), submitted["handle"].as_str())
    else {
        return (
            Judged::Failed("the judge's turn was not started".into()),
            None,
        );
    };
    let left = deadline.saturating_duration_since(Instant::now());
    let waited = match client
        .request(
            "wait",
            json!({"handles":[handle],"timeout_ms":left.as_millis() as u64}),
        )
        .await
    {
        Ok(waited) => waited,
        Err(error) => {
            return (
                Judged::Failed(format!("the judge could not be waited on: {}", error.code)),
                Some(turn),
            );
        }
    };
    let outcome = &waited["results"][handle];
    if outcome["pending"] == true {
        return (Judged::Failed("the check timed out".into()), Some(turn));
    }
    if outcome["status"] != "completed" {
        return (
            Judged::Failed(format!(
                "the judge's turn {}: {}",
                outcome["status"].as_str().unwrap_or("ended"),
                outcome["error"].as_str().unwrap_or("no answer")
            )),
            None,
        );
    }
    let Some(answers) = (outcome["text_truncated"] != true)
        .then(|| policy::model_answers(outcome["text"].as_str().unwrap_or_default(), questions))
        .flatten()
    else {
        return (
            Judged::Failed("the judge's answer did not parse".into()),
            None,
        );
    };
    let read = client
        .request("turns", json!({"bot":name,"after":turn - 1,"limit":1}))
        .await
        .unwrap_or_default();
    let row = &read["turns"][0];
    let tokens = |key: &str| row[key].as_u64().unwrap_or(0);
    (
        Judged::Answers {
            answers,
            usage: Usage {
                input: tokens("input_tokens"),
                cached: tokens("cached_input_tokens"),
                output: tokens("output_tokens"),
            },
        },
        None,
    )
}

/// Delete a judge's fork, ending its turn first if it still runs.
async fn remove(client: &Client, name: &str, running: Option<i64>) {
    if let Some(turn) = running {
        let _ = client
            .request("interrupt", json!({"bot":name,"turn":turn}))
            .await;
        let _ = client
            .request(
                "wait",
                json!({"handles":[format!("turn:{name}/{turn}")],"timeout_ms":5_000}),
            )
            .await;
    }
    if let Err(error) = client.request("delete", json!({"bot":name})).await
        && error.code != "bot_not_found"
    {
        report(json!({"event":"judge_not_removed","bot":name,"error":error.code}));
    }
}

/// A bot's latest turn and the requests of it already judged.
type Seen = (i64, HashSet<(String, i64)>);

struct Shared {
    client: Arc<Client>,
    judge: Judge,
    tag: String,
    lease: u64,
    note: Option<String>,
    redactor: Redactor,
    /// Requests already judged, so none is judged twice: per bot, only
    /// those of its latest turn, since a bot's turns run one at a time.
    seen: Mutex<HashMap<String, Seen>>,
    /// Turns the breaker stopped, still to be interrupted.
    tripped: Mutex<HashSet<(String, i64)>>,
    /// Set when an answer finds the lease gone.
    lost: Notify,
}

async fn serve(settings: Settings) -> Result<i32> {
    let tag = settings.tag.clone();
    if matches!(settings.judge, JudgeSpec::Model { .. }) && tag.len() > JUDGE_NAME_ROOM {
        return Err(Error::with(
            "invalid_tag",
            format!("a model judge's tag is at most {JUDGE_NAME_ROOM} bytes, to name its forks"),
        ));
    }
    let (client, mut events) = Client::connect(&settings.socket)
        .await
        .map_err(Error::from)?;
    let served = client
        .request(
            "serve_approvals",
            json!({"tag":tag,"lease_ms":LEASE_MS,"limit":256}),
        )
        .await
        .map_err(Error::from)?;
    let (Some(lease), Some(through)) = (served["lease"].as_u64(), served["through"].as_i64())
    else {
        return Err(Error::new("daemon_protocol_mismatch"));
    };
    let (via, deadline_ms) = match settings.judge {
        JudgeSpec::Jev { url, model, key } => (
            Via::Jev(Jev {
                http: reqwest::Client::builder()
                    .user_agent(concat!("agent-runtime/", env!("CARGO_PKG_VERSION")))
                    .no_proxy()
                    .redirect(reqwest::redirect::Policy::none())
                    .connect_timeout(Duration::from_secs(5))
                    .build()
                    .map_err(|_| Error::new("http_client_init"))?,
                url: url.trim_end_matches('/').to_owned(),
                model,
                key,
                backoff: Mutex::new((None, BACKOFF)),
            }),
            policy::DEADLINE_MS,
        ),
        JudgeSpec::Model { model, reasoning } => {
            let base = format!("approver.{tag}");
            let judge = Model::prepare(&client, base, &model, reasoning.as_deref()).await?;
            (Via::Model(judge), policy::MODEL_DEADLINE_MS)
        }
    };
    let shared = Arc::new(Shared {
        client,
        judge: Judge {
            via,
            slots: Semaphore::new(IN_FLIGHT),
            deadline_ms,
        },
        tag: tag.clone(),
        lease,
        note: settings.note,
        redactor: Redactor::from_env(std::env::vars()),
        seen: Mutex::default(),
        tripped: Mutex::default(),
        lost: Notify::new(),
    });
    // The CLI that started this process waits for this line: the judge is
    // ready, not just the lease held.
    report(json!({"event":"serving","tag":tag,"lease":lease,"pid":std::process::id()}));
    let mut rounds = tokio::task::JoinSet::new();
    // The calls already waiting, page by page up to where serving began;
    // later ones are pushed. Every page first, so a round that spans two
    // is judged whole.
    let mut waiting = Vec::new();
    let mut page = served;
    loop {
        if let Value::Array(calls) = page["approvals"].take() {
            waiting.extend(calls);
        }
        let Some(after) = page["next_after"].as_i64() else {
            break;
        };
        page = shared
            .client
            .request(
                "approvals",
                json!({"tag":tag,"after":after,"through":through,"limit":256}),
            )
            .await
            .map_err(Error::from)?;
    }
    for (bot, turn, calls) in group(waiting) {
        rounds.spawn(round(shared.clone(), bot, turn, calls));
    }
    // The parts of a pushed round so far, until its last part arrives.
    let mut parts: HashMap<(String, i64, i64), Vec<Value>> = HashMap::new();
    let mut renew = tokio::time::interval(RENEW);
    renew.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    loop {
        tokio::select! {
            event = events.recv() => {
                let Some(event) = event else {
                    report(json!({"event":"daemon_gone","tag":tag}));
                    return Ok(1);
                };
                let ours = event["tag"] == tag.as_str();
                if ours && event["event"] == "approval_requested" {
                    if let Some(calls) = whole_round(&mut parts, event) {
                        for (bot, turn, calls) in group(calls) {
                            rounds.spawn(round(shared.clone(), bot, turn, calls));
                        }
                    }
                    continue;
                }
                match event["event"].as_str() {
                    Some("approvals_lost") if ours => {
                        report(json!({"event":"approvals_lost","tag":tag}));
                        return Ok(0);
                    }
                    // The daemon let this session go: nothing more arrives.
                    Some("follow_lagged") => {
                        report(json!({"event":"lagged","tag":tag}));
                        return Ok(1);
                    }
                    _ => {}
                }
            }
            _ = renew.tick() => {
                if let Err(error) = shared
                    .client
                    .request("renew_approvals", json!({"tag":tag,"lease":lease}))
                    .await
                {
                    report(json!({"event":error.code,"tag":tag}));
                    return Ok(if error.code == "approvals_lost" { 0 } else { 1 });
                }
            }
            _ = shared.lost.notified() => {
                report(json!({"event":"approvals_lost","tag":tag}));
                return Ok(0);
            }
            Some(_) = rounds.join_next() => {}
        }
    }
}

fn report(line: Value) {
    println!("{line}");
}

/// A pushed round's calls once its last part is in: the daemon numbers
/// the parts of a round too large for one message, and sends them in order.
fn whole_round(
    parts: &mut HashMap<(String, i64, i64), Vec<Value>>,
    mut event: Value,
) -> Option<Vec<Value>> {
    let Value::Array(calls) = event["data"]["calls"].take() else {
        return None;
    };
    let (part, count) = (
        event["data"]["part"].as_u64().unwrap_or(1),
        event["data"]["parts"].as_u64().unwrap_or(1),
    );
    if count <= 1 {
        return Some(calls);
    }
    let key = (
        event["bot"].as_str().unwrap_or_default().to_owned(),
        event["turn"].as_i64().unwrap_or(0),
        event["cursor"].as_i64().unwrap_or(0),
    );
    let held = parts.entry(key.clone()).or_default();
    held.extend(calls);
    (part >= count).then(|| parts.remove(&key).unwrap_or_default())
}

/// Calls by round, in the order they came: a pushed round is one, the
/// listing may hold several.
fn group(calls: Vec<Value>) -> Vec<(String, i64, Vec<Value>)> {
    let mut rounds: Vec<(String, i64, Vec<Value>)> = Vec::new();
    let mut index: HashMap<(String, i64), usize> = HashMap::new();
    for call in calls {
        let (Some(bot), Some(turn)) = (call["bot"].as_str(), call["turn"].as_i64()) else {
            continue;
        };
        match index.get(&(bot.to_owned(), turn)) {
            Some(&at) => rounds[at].2.push(call),
            None => {
                index.insert((bot.to_owned(), turn), rounds.len());
                rounds.push((bot.to_owned(), turn, vec![call]));
            }
        }
    }
    rounds
}

async fn round(shared: Arc<Shared>, bot: String, turn: i64, calls: Vec<Value>) {
    let calls: Vec<Value> = {
        let mut seen = shared.seen.lock().unwrap();
        let (latest, requests) = seen
            .entry(bot.clone())
            .or_insert_with(|| (turn, HashSet::new()));
        if *latest < turn {
            *latest = turn;
            requests.clear();
        }
        calls
            .into_iter()
            .filter(|call| {
                *latest > turn
                    || requests.insert((
                        call["call_id"].as_str().unwrap_or_default().to_owned(),
                        call["request"].as_i64().unwrap_or(0),
                    ))
            })
            .collect()
    };
    if calls.is_empty() {
        return;
    }
    let started = Instant::now();
    let announced = calls
        .iter()
        .filter_map(|call| call["announced_ms"].as_i64())
        .min()
        .unwrap_or_else(epoch_ms);
    let left = (announced + shared.judge.deadline_ms as i64 - epoch_ms()).max(0) as u64;
    let deadline = started + Duration::from_millis(left);
    let counts = &calls[0]["denials"][&shared.tag];
    let (in_row, in_turn) = (
        counts["in_row"].as_u64().unwrap_or(0),
        counts["in_turn"].as_u64().unwrap_or(0),
    );
    let key = (bot.clone(), turn);
    let mut usage = Usage::default();
    let verdicts: Vec<Verdict> = if shared.tripped.lock().unwrap().contains(&key) {
        // The turn kept going after it was told to stop: end it now.
        interrupt(&shared, &bot, turn).await;
        vec![policy::tripped(in_row.max(policy::IN_ROW), in_turn).unwrap(); calls.len()]
    } else if let Some(stop) = policy::tripped(in_row, in_turn) {
        shared.tripped.lock().unwrap().insert(key);
        let later = shared.clone();
        let (b, t) = (bot.clone(), turn);
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(policy::INTERRUPT_AFTER_MS)).await;
            if later.tripped.lock().unwrap().contains(&(b.clone(), t)) {
                interrupt(&later, &b, t).await;
            }
        });
        vec![stop; calls.len()]
    } else if left == 0 {
        vec![policy::not_reviewed("the approver is overloaded"); calls.len()]
    } else {
        match judge(&shared, &bot, turn, &calls, deadline).await {
            Ok((verdicts, used)) => {
                usage = used;
                verdicts
            }
            Err(verdict) => vec![verdict; calls.len()],
        }
    };
    let judge_ms = started.elapsed().as_millis() as u64;
    let mut answered = Vec::with_capacity(calls.len());
    for (call, verdict) in calls.iter().zip(&verdicts) {
        let (decision, reason) = match verdict {
            Verdict::Allow => ("allow", None),
            Verdict::Deny(reason) => ("deny", Some(reason.as_str())),
        };
        let result = shared
            .client
            .request(
                "answer",
                json!({"bot":bot,"turn":turn,"call_id":call["call_id"],"request":call["request"],
                    "tag":shared.tag,"decision":decision,"reason":reason,"by":"approver",
                    "lease":shared.lease}),
            )
            .await;
        let error = result.err().map(|error| {
            if error.code == "approvals_lost" {
                shared.lost.notify_one();
            }
            error.code
        });
        answered.push(json!({"call_id":call["call_id"],"request":call["request"],
            "decision":decision,"reason":reason,"error":error}));
    }
    report(
        json!({"event":"judged","bot":bot,"turn":turn,"calls":answered,
        "judge_ms":judge_ms,"input_tokens":usage.input,"cached_tokens":usage.cached,
        "output_tokens":usage.output}),
    );
}

async fn interrupt(shared: &Shared, bot: &str, turn: i64) {
    shared
        .tripped
        .lock()
        .unwrap()
        .remove(&(bot.to_owned(), turn));
    // A turn that already ended answers stale_turn: nothing to do.
    let _ = shared
        .client
        .request("interrupt", json!({"bot":bot,"turn":turn}))
        .await;
}

/// One judge request for the round, and a verdict for each call.
async fn judge(
    shared: &Shared,
    bot: &str,
    turn: i64,
    calls: &[Value],
    deadline: Instant,
) -> std::result::Result<(Vec<Verdict>, Usage), Verdict> {
    let Ok(Ok(_slot)) =
        tokio::time::timeout_at(deadline.into(), shared.judge.slots.acquire()).await
    else {
        return Err(policy::not_reviewed("the approver is overloaded"));
    };
    let intent =
        match tokio::time::timeout_at(deadline.into(), intent(shared, bot, turn, calls)).await {
            Err(_) => return Err(policy::not_reviewed("the approver is overloaded")),
            Ok(Err(Unjudged::TooLong)) => return Err(policy::not_reviewed("intent too long")),
            Ok(Err(Unjudged::FileTooLong)) => {
                return Err(policy::not_reviewed("it runs a file too large to show"));
            }
            Ok(Err(Unjudged::Failed)) => return Err(policy::not_reviewed("the check failed")),
            Ok(Ok(intent)) => intent,
        };
    let (state, consent) = match policy::build(&intent, &shared.redactor) {
        State::TooLong => return Err(policy::not_reviewed("intent too long")),
        State::Built { state, consent } => (state, consent),
    };
    let ids: Vec<&str> = intent.planned.iter().map(|call| call.id.as_str()).collect();
    let questions = policy::questions(&ids, consent);
    match shared
        .judge
        .ask(&shared.client, &state, &questions, deadline)
        .await
    {
        Judged::Overloaded => Err(policy::not_reviewed("the approver is overloaded")),
        Judged::Failed(detail) => {
            report(json!({"event":"judge_failed","bot":bot,"turn":turn,"detail":detail}));
            Err(policy::not_reviewed("the check failed"))
        }
        Judged::Answers { answers, usage } => {
            let verdicts = ids
                .iter()
                .map(|id| policy::verdict(id, &answers, consent))
                .collect::<Option<Vec<_>>>()
                .ok_or_else(|| policy::not_reviewed("the check failed"))?;
            Ok((verdicts, usage))
        }
    }
}

/// Every string in a call's arguments, one per line, as a command sees them.
fn strings(value: &Value, out: &mut String) {
    match value {
        Value::String(text) => {
            out.push_str(text);
            out.push('\n');
        }
        Value::Array(items) => items.iter().for_each(|item| strings(item, out)),
        Value::Object(fields) => fields.values().for_each(|item| strings(item, out)),
        _ => {}
    }
}

/// Whether `text` names `path` as a whole: not as part of a longer name,
/// so a file `config` is not named by `./configure`.
fn mentions(text: &str, path: &str) -> bool {
    let part = |c: Option<char>| c.is_some_and(|c| c.is_ascii_alphanumeric() || "_-.".contains(c));
    text.match_indices(path).any(|(at, _)| {
        !part(text[..at].chars().next_back()) && !part(text[at + path.len()..].chars().next())
    })
}

/// What a `prompts` reply's prompts take from a round's shared budget:
/// their text, and `PROMPTS_ENTRY` each.
fn prompt_bytes(prompts: &[Value]) -> usize {
    prompts
        .iter()
        .map(|prompt| {
            prompt["text"].as_str().map_or(0, str::len)
                + agent_runtime::store::Database::PROMPTS_ENTRY
        })
        .sum()
}

/// The `path` of a call whose arguments preview was cut, when the preview
/// holds it whole: the fields before the cut parse, the one it falls in
/// does not.
fn preview_path(preview: &str) -> Option<String> {
    let mut rest = preview.trim_start().strip_prefix('{')?;
    loop {
        let mut keys = serde_json::Deserializer::from_str(rest).into_iter::<String>();
        let key = keys.next()?.ok()?;
        rest = rest[keys.byte_offset()..].trim_start().strip_prefix(':')?;
        let mut values = serde_json::Deserializer::from_str(rest).into_iter::<Value>();
        let value = values.next()?.ok()?;
        if key == "path" {
            return value.as_str().map(str::to_owned);
        }
        rest = rest[values.byte_offset()..]
            .trim_start()
            .strip_prefix(',')?;
    }
}

/// A file the turn wrote, as a judged call that names it would run it:
/// only a regular file, opened without waiting on a FIFO or device, and
/// read no further than `FILE_BYTES`, whatever size it reported.
fn written_file(file: &Path) -> std::result::Result<Option<Vec<u8>>, Unjudged> {
    use std::io::Read;
    use std::os::unix::fs::OpenOptionsExt;
    let Ok(opened) = std::fs::OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NONBLOCK)
        .open(file)
    else {
        return Ok(None);
    };
    if !opened.metadata().is_ok_and(|meta| meta.is_file()) {
        return Ok(None);
    }
    let mut content = Vec::new();
    if opened
        .take(FILE_BYTES + 1)
        .read_to_end(&mut content)
        .is_err()
    {
        return Ok(None);
    }
    if content.len() as u64 > FILE_BYTES {
        return Err(Unjudged::FileTooLong);
    }
    Ok(Some(content))
}

enum Unjudged {
    TooLong,
    FileTooLong,
    Failed,
}

impl From<agent_client::Error> for Unjudged {
    fn from(_: agent_client::Error) -> Self {
        Unjudged::Failed
    }
}

/// The item a planned call's node holds. One too large to read whole is too
/// long to judge.
async fn node_item(
    client: &Client,
    bot: &str,
    node: &Value,
) -> std::result::Result<Value, Unjudged> {
    let read = client
        .request("history_items", json!({"bot":bot,"nodes":[node]}))
        .await?;
    match &read["items"][0] {
        entry if entry["error"] == "item_too_large" => Err(Unjudged::TooLong),
        entry => Ok(entry["item"].clone()),
    }
}

/// What the judge is shown for a round, read from the daemon: the words the
/// turn answers to, followed up delegations to the person's own; the calls
/// being judged, whole; the calls the turn already ran; earlier prompts.
async fn intent(
    shared: &Shared,
    bot: &str,
    turn: i64,
    calls: &[Value],
) -> std::result::Result<Intent, Unjudged> {
    let client = &shared.client;
    let read = client
        .request(
            "prompts",
            json!({"bot":bot,"turn":turn,"bytes":PROMPT_BYTES}),
        )
        .await?;
    // Steers left out may hold the person's latest word.
    if read["prompts_more"] == true {
        return Err(Unjudged::TooLong);
    }
    let mut request = Vec::new();
    let mut visited = HashSet::from([(bot.to_owned(), turn)]);
    let prompts = read["prompts"].as_array().map_or(&[][..], Vec::as_slice);
    let mut budget = PROMPT_BYTES.saturating_sub(prompt_bytes(prompts));
    for prompt in prompts {
        if prompt["truncated"] == true {
            return Err(Unjudged::TooLong);
        }
        let text = prompt["text"].as_str().unwrap_or_default().to_owned();
        match prompt.get("from") {
            None => request.push(Words {
                by: by_origin(prompt),
                text,
            }),
            Some(from) => {
                persons_above(client, from, &mut visited, &mut budget, &mut request).await?;
                request.push(Words {
                    by: By::Model,
                    text,
                });
            }
        }
    }
    let mut planned = Vec::with_capacity(calls.len());
    // Past this the state is too long to judge anyway: stop reading.
    let mut size = 0;
    for (index, call) in calls.iter().enumerate() {
        let whole = call["arguments"].is_object()
            && call["arguments_cut"].as_array().is_none_or(Vec::is_empty)
            && call["arguments_omitted"].as_u64().unwrap_or(0) == 0;
        let arguments = match whole {
            true => call["arguments"].clone(),
            false => {
                let item = node_item(client, bot, &call["node"]).await?;
                policy::call_arguments(&item, call["call_id"].as_str().unwrap_or_default())
                    .ok_or(Unjudged::Failed)?
            }
        };
        size += arguments.to_string().len();
        if size > PROMPT_BYTES {
            return Err(Unjudged::TooLong);
        }
        planned.push(Planned {
            id: format!("c{}", index + 1),
            tool: call["name"].as_str().unwrap_or_default().to_owned(),
            arguments,
            files: Vec::new(),
        });
    }
    let mut allowed = Vec::new();
    let mut allowed_cut = read["calls_more"] == true;
    let mut written = Vec::new();
    // Arguments read whole for calls whose preview was cut; past the text
    // limit only a write's or edit's path is still read, and kept small.
    let mut expanded = 0;
    for call in read["calls"].as_array().into_iter().flatten() {
        let tool = call["name"].as_str().unwrap_or_default().to_owned();
        let preview = call["arguments"].as_str().unwrap_or_default();
        let writes = matches!(tool.as_str(), "write" | "edit");
        let cut = call["arguments_truncated"] == true;
        // Whether a write's size is known: an ungated call has no planning
        // node to read its whole arguments from.
        let mut sized = true;
        let mut arguments = match cut {
            false => serde_json::from_str(preview).unwrap_or_else(|_| json!(preview)),
            true => match call["node"].as_i64() {
                Some(_) if expanded >= PROMPT_BYTES && !writes => {
                    allowed_cut = true;
                    json!(preview)
                }
                Some(node) => {
                    let item = node_item(client, bot, &json!(node)).await?;
                    policy::call_arguments(&item, call["call_id"].as_str().unwrap_or_default())
                        .ok_or(Unjudged::Failed)?
                }
                // Its path is what a later call needs from it; without it,
                // no file this turn wrote could be matched.
                None if writes => {
                    sized = false;
                    json!({"path":preview_path(preview).ok_or(Unjudged::TooLong)?})
                }
                None => {
                    allowed_cut = true;
                    json!(preview)
                }
            },
        };
        // A file's content is not what consent is about: its path and size.
        if writes && let Value::Object(fields) = &arguments {
            // A failed write changed nothing this turn: its file is not shown.
            if let Some(path) = fields.get("path").and_then(Value::as_str)
                && call["failed"] != true
            {
                written.push(path.to_owned());
            }
            let bytes: usize = fields
                .iter()
                .filter(|(name, _)| name.as_str() != "path")
                .map(|(_, value)| value.as_str().map_or(0, str::len))
                .sum();
            arguments = json!({"path":fields.get("path"),"bytes":sized.then_some(bytes)});
        }
        if cut {
            expanded += arguments.to_string().len();
        }
        allowed.push(Allowed {
            tool,
            arguments,
            status: match (call["done"] == true, call["failed"] == true) {
                (_, true) => "failed",
                (true, false) => "succeeded",
                (false, false) => "running",
            },
        });
    }
    // A call that names a file this turn wrote is judged with that file
    // whole, as it is on disk now: what the call would run. One that an
    // earlier call of this round writes or edits is not on disk yet as the
    // call will find it, so it points to that call instead.
    let workspace = read["workspace"].as_str().map(PathBuf::from);
    let mut changing: Vec<(String, String)> = Vec::new();
    for call in &mut planned {
        let mut text = String::new();
        strings(&call.arguments, &mut text);
        let names = |path: &str| {
            let name = std::path::Path::new(path)
                .file_name()
                .and_then(|name| name.to_str())
                .unwrap_or(path);
            mentions(&text, path) || (name.len() >= 3 && mentions(&text, name))
        };
        for (path, id) in &changing {
            if names(path) {
                call.files.push((
                    path.clone(),
                    format!("[{id} in this round changes it before this call runs; see {id}]"),
                ));
            }
        }
        for path in &written {
            if !names(path) || changing.iter().any(|(changed, _)| changed == path) {
                continue;
            }
            let file = workspace
                .as_deref()
                .map_or_else(|| PathBuf::from(path), |w| w.join(path));
            // Off the runtime thread: a slow mount must not hold up renewal.
            let read = tokio::task::spawn_blocking(move || written_file(&file));
            if let Some(content) = read.await.map_err(|_| Unjudged::Failed)?? {
                call.files
                    .push((path.clone(), String::from_utf8_lossy(&content).into_owned()));
            }
        }
        if matches!(call.tool.as_str(), "write" | "edit")
            && let Some(path) = call.arguments["path"].as_str()
        {
            changing.push((path.to_owned(), call.id.clone()));
        }
    }
    let earlier = read["earlier"]
        .as_array()
        .into_iter()
        .flatten()
        .map(|prompt| Words {
            by: match prompt.get("from") {
                Some(_) => By::Model,
                None => by_origin(prompt),
            },
            text: prompt["text"].as_str().unwrap_or_default().to_owned(),
        })
        .collect();
    Ok(Intent {
        note: shared.note.clone(),
        workspace: workspace.map(|w| w.to_string_lossy().into_owned()),
        request,
        planned,
        allowed,
        allowed_cut,
        earlier,
    })
}

/// Who wrote a prompt no bot's turn wrote: a person, unless a client sent it
/// on its own (`origin`), whose words ask for nothing either.
fn by_origin(prompt: &Value) -> By {
    if prompt.get("origin").is_some() {
        By::Model
    } else {
        By::Person
    }
}

/// The person-written prompts above a model-written one: the turn that
/// wrote it, and so on up to `CHAIN` delegations, in the order they were
/// written, so a later word from the person still reads as the later one.
/// A turn that can no longer be read (its bot was deleted) has no person
/// above it, so nothing there consents.
async fn persons_above(
    client: &Client,
    from: &Value,
    visited: &mut HashSet<(String, i64)>,
    budget: &mut usize,
    request: &mut Vec<Words>,
) -> std::result::Result<(), Unjudged> {
    // Each delegating turn's prompts in order; a model-written one is
    // replaced by the prompts above it, depth first.
    let mut levels: Vec<std::vec::IntoIter<Value>> = Vec::new();
    let mut next = Some(from.clone());
    loop {
        if let Some(from) = next.take() {
            if levels.len() >= CHAIN {
                return Err(Unjudged::TooLong);
            }
            if let Some(prompts) = delegated(client, &from, visited, budget).await? {
                levels.push(prompts.into_iter());
            }
        }
        let Some(level) = levels.last_mut() else {
            return Ok(());
        };
        let Some(prompt) = level.next() else {
            levels.pop();
            continue;
        };
        if prompt["truncated"] == true {
            return Err(Unjudged::TooLong);
        }
        match prompt.get("from") {
            Some(above) => next = Some(above.clone()),
            None => request.push(Words {
                by: by_origin(&prompt),
                text: prompt["text"].as_str().unwrap_or_default().to_owned(),
            }),
        }
    }
}

/// The prompts of the turn `from` names, unless it was read already or is
/// gone, read within what is left of the round's `budget`.
async fn delegated(
    client: &Client,
    from: &Value,
    visited: &mut HashSet<(String, i64)>,
    budget: &mut usize,
) -> std::result::Result<Option<Vec<Value>>, Unjudged> {
    let (Some(bot), Some(turn)) = (from["bot"].as_str(), from["turn"].as_i64()) else {
        return Ok(None);
    };
    if !visited.insert((bot.to_owned(), turn)) {
        return Ok(None);
    }
    if visited.len() > DELEGATING || *budget == 0 {
        return Err(Unjudged::TooLong);
    }
    match client
        .request("prompts", json!({"bot":bot,"turn":turn,"bytes":*budget}))
        .await
    {
        // Steers left out may hold that person's latest word.
        Ok(read) if read["prompts_more"] == true => Err(Unjudged::TooLong),
        Ok(mut read) => Ok(match read["prompts"].take() {
            Value::Array(prompts) => {
                *budget = budget.saturating_sub(prompt_bytes(&prompts));
                Some(prompts)
            }
            _ => None,
        }),
        Err(error) if matches!(error.code.as_str(), "bot_not_found" | "turn_not_found") => Ok(None),
        Err(error) => Err(error.into()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rounds_keep_the_order_their_first_calls_arrived_in() {
        let call = |bot: &str, turn: i64, id: &str| json!({"bot":bot,"turn":turn,"call_id":id});
        let rounds = group(vec![
            call("Bob", 3, "a"),
            call("Ann", 1, "b"),
            call("Bob", 3, "c"),
            call("Bob", 4, "d"),
        ]);
        let shape: Vec<(&str, i64, Vec<&str>)> = rounds
            .iter()
            .map(|(bot, turn, calls)| {
                let ids = calls.iter().map(|c| c["call_id"].as_str().unwrap());
                (bot.as_str(), *turn, ids.collect())
            })
            .collect();
        assert_eq!(
            shape,
            [
                ("Bob", 3, vec!["a", "c"]),
                ("Ann", 1, vec!["b"]),
                ("Bob", 4, vec!["d"])
            ]
        );
    }

    #[test]
    fn a_cut_preview_gives_a_path_only_when_it_holds_it_whole() {
        let cut = |text: &str| preview_path(text);
        assert_eq!(
            cut(r#"{"path":"run.sh","content":"echo one\necho tw"#).as_deref(),
            Some("run.sh")
        );
        assert_eq!(
            cut(r#"{ "mode" : 7, "path" : "a \"b\".txt" , "content":"x"#).as_deref(),
            Some(r#"a "b".txt"#)
        );
        assert_eq!(cut(r#"{"content":"echo one","path":"run"#), None);
        assert_eq!(cut(r#"{"content":"echo one\necho tw"#), None);
        assert_eq!(cut(r#"{"path":7,"content":"x"#), None);
        assert_eq!(cut("echo"), None);
    }

    #[test]
    fn a_file_is_named_only_as_a_whole_word() {
        let mut text = String::new();
        strings(
            &json!({"command":"./configure && sh ./run.sh\ncat notes/config.bak","env":["x"]}),
            &mut text,
        );
        assert!(mentions(&text, "run.sh"));
        assert!(!mentions(&text, "config"));
        assert!(!mentions(&text, "notes/config"));
        assert!(mentions(&text, "notes/config.bak"));
        assert!(mentions(&text, "x"));
    }

    #[test]
    fn only_a_tool_less_bot_with_the_judges_instructions_is_a_judge() {
        let judge = json!({"tools":[],"instructions":policy::JUDGE_INSTRUCTIONS});
        assert!(judge_bot(&judge));
        let tools = json!({"tools":["shell"],"instructions":policy::JUDGE_INSTRUCTIONS});
        assert!(!judge_bot(&tools));
        assert!(!judge_bot(
            &json!({"tools":[],"instructions":"Help with the repo."})
        ));
        let opening = format!("{} And more.", &policy::JUDGE_INSTRUCTIONS[..60]);
        assert!(!judge_bot(&json!({"tools":[],"instructions":opening})));
    }

    #[test]
    fn a_written_file_is_read_only_if_regular_and_within_the_limit() {
        let dir = std::env::temp_dir().join(format!("agent-written-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let small = dir.join("run.sh");
        std::fs::write(&small, "echo hi\n").unwrap();
        assert!(matches!(written_file(&small), Ok(Some(content)) if content == b"echo hi\n"));
        let large = dir.join("large.sh");
        std::fs::write(&large, vec![b'#'; FILE_BYTES as usize + 1]).unwrap();
        assert!(matches!(written_file(&large), Err(Unjudged::FileTooLong)));
        // A FIFO with no writer is skipped, not waited on.
        let fifo = dir.join("fifo");
        let path = std::ffi::CString::new(fifo.as_os_str().as_encoded_bytes()).unwrap();
        assert_eq!(unsafe { libc::mkfifo(path.as_ptr(), 0o600) }, 0);
        assert!(matches!(written_file(&fifo), Ok(None)));
        assert!(matches!(written_file(&dir.join("gone")), Ok(None)));
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn a_round_in_parts_is_judged_once_its_last_part_is_in() {
        let part = |part: u64, parts: u64, calls: &[&str]| {
            let calls: Vec<Value> = calls
                .iter()
                .map(|id| json!({"bot":"Bob","turn":3,"call_id":id}))
                .collect();
            json!({"bot":"Bob","turn":3,"cursor":9,"tag":"auto",
                "data":{"calls":calls,"part":part,"parts":parts}})
        };
        let mut parts = HashMap::new();
        let whole = whole_round(&mut parts, part(1, 1, &["a"])).unwrap();
        assert_eq!(whole.len(), 1);
        assert!(whole_round(&mut parts, part(1, 2, &["a", "b"])).is_none());
        let whole = whole_round(&mut parts, part(2, 2, &["c"])).unwrap();
        let rounds = group(whole);
        let [(bot, turn, calls)] = &rounds[..] else {
            panic!("{rounds:?}")
        };
        assert_eq!((bot.as_str(), *turn), ("Bob", 3));
        let ids: Vec<&str> = calls
            .iter()
            .map(|c| c["call_id"].as_str().unwrap())
            .collect();
        assert_eq!(ids, ["a", "b", "c"]);
        assert!(parts.is_empty());
    }
}
