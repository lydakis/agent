//! `agent approver`: serve a gate tag and have a judge model decide every
//! call waiting on it, in one request a round. The policy (what the judge is
//! told, the verdicts, what it sees, and redaction) is
//! `agent_client::approver` and the judge's transport `agent_client::judge`;
//! this is the loop.
//! As in Codex, a denial goes back to the model at once, and a round the
//! judge never assessed is denied as a review failure; only a call too long
//! to show is handed to a person, where one can be asked.
use agent_client::{
    Client,
    approver::{
        self as policy, Allowed, By, Intent, Planned, Redactor, Refused, State, Transcript,
        Verdict, Words,
    },
    judge::{self, Failed, Usage},
};
use agent_runtime::{Error, Result};
use serde_json::{Value, json};
use std::{
    collections::{HashMap, HashSet},
    path::{Path, PathBuf},
    sync::{Arc, Mutex},
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};
use tokio::sync::{Notify, Semaphore};

pub struct Settings {
    pub socket: PathBuf,
    pub tag: String,
    /// The judge `--judge` or `AGENT_JUDGE` names, if either does.
    pub judge: Option<String>,
    /// The model the bots being judged run, `AGENT_MODEL`, when no judge
    /// is named: the last in the order a judge is picked in.
    pub caller: Option<String>,
    /// A model judge's effort.
    pub effort: Option<String>,
    /// For the Decisions API: where, and its key.
    pub url: Option<String>,
    pub key: Option<String>,
    /// The environment note the judge always sees.
    pub note: Option<String>,
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
/// the words above are left out, and the judge is told.
const CHAIN: usize = 8;
const DELEGATING: usize = 64;
/// How much of a file written this turn a judged call that names it is
/// shown: a larger one only by its size, and the call is not allowed on the
/// judge's word alone.
const FILE_BYTES: u64 = 48 * 1024;
/// The most history nodes read for the transcript, newest first, and how
/// many bodies one read asks for. Codex reads the whole conversation; past
/// this the rest is left out, and the judge is told.
const HISTORY_NODES: usize = 2_000;
const HISTORY_PAGE: usize = 50;
/// A judge fork is `approver.TAG` and the fork's suffix, and every bot
/// name fits 128 bytes.
const JUDGE_NAME_ROOM: usize = 128 - "approver.".len() - judge::FORK_SUFFIX;

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
    /// An assessment for every call, by its id.
    Answers {
        answers: Value,
        usage: Usage,
    },
    /// No slot, or the judge's rate limit, before the round's deadline.
    Overloaded,
    Failed(String),
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
    Decisions(judge::Decisions),
    Model(judge::Base),
}

/// The judge picked, before the tag's lease is held: a model judge's base
/// bot is made only after.
enum Picked<'a> {
    Decisions(judge::Decisions),
    Model(&'a str),
}

impl Judge {
    async fn ask(&self, client: &Client, state: &Value, ids: &[&str], deadline: Instant) -> Judged {
        let answered = match &self.via {
            // One predicate a call, its probability read as an outcome.
            Via::Decisions(decisions) => {
                let text = policy::state_text(state);
                let questions = policy::questions(ids);
                decisions
                    .ask(policy::DECISIONS_INSTRUCTIONS, &text, &questions, deadline)
                    .await
                    .map(|(answers, usage)| {
                        let assessed = answers
                            .into_iter()
                            .map(|(id, p)| (id, policy::decided(p.as_f64().unwrap_or(1.0))))
                            .collect();
                        (assessed, usage)
                    })
            }
            Via::Model(base) => {
                let prompt = policy::model_prompt(state, ids);
                base.ask_with(client, &prompt, ids, policy::assessments, deadline)
                    .await
            }
        };
        match answered {
            Ok((answers, usage)) => Judged::Answers {
                answers: Value::Object(answers),
                usage,
            },
            Err(Failed::Overloaded) => Judged::Overloaded,
            Err(Failed::Quota) => Judged::Failed("the judge's account is out of quota".into()),
            Err(Failed::Check(why)) => Judged::Failed(why),
        }
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
    let (client, mut events) = Client::connect(&settings.socket)
        .await
        .map_err(Error::from)?;
    let named =
        judge::pick(settings.judge.as_deref(), settings.caller.as_deref()).ok_or_else(|| {
            Error::with(
                "judge_required",
                format!(
                    "auto needs a judge: pass --judge PROVIDER/MODEL, or set {} or AGENT_MODEL",
                    judge::VAR
                ),
            )
        })?;
    // An approver the daemon refuses the tag never replaces the base of
    // the one serving it.
    let picked = match judge::spec(&named) {
        None => {
            return Err(Error::with(
                "invalid_judge",
                format!("{named} is not PROVIDER/MODEL or openai-decisions/MODEL"),
            ));
        }
        Some(judge::Spec::Decisions(model)) => {
            let key = settings.key.ok_or_else(|| {
                Error::with(
                    "judge_key_required",
                    format!("set {} for the Decisions API", judge::DECISIONS_KEY),
                )
            })?;
            Picked::Decisions(judge::Decisions::new(model, settings.url.as_deref(), key)?)
        }
        Some(judge::Spec::Model(_)) if tag.len() > JUDGE_NAME_ROOM => {
            return Err(Error::with(
                "invalid_tag",
                format!(
                    "a model judge's tag is at most {JUDGE_NAME_ROOM} bytes, to name its forks"
                ),
            ));
        }
        Some(judge::Spec::Model(model)) => Picked::Model(model),
    };
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
    let via = match picked {
        Picked::Decisions(decisions) => Via::Decisions(decisions),
        Picked::Model(model) => Via::Model(
            judge::Base::make(
                &client,
                &format!("approver.{tag}"),
                model,
                settings.effort.as_deref(),
                policy::JUDGE_INSTRUCTIONS,
            )
            .await?,
        ),
    };
    let shared = Arc::new(Shared {
        client,
        judge: Judge {
            via,
            slots: Semaphore::new(IN_FLIGHT),
            deadline_ms: policy::DEADLINE_MS,
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
    report(
        json!({"event":"serving","tag":tag,"lease":lease,"judge":named,
        "pid":std::process::id()}),
    );
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
            // Handed to a person already, by this tag's last holder.
            .filter(|call| {
                !call["asked"]
                    .as_array()
                    .into_iter()
                    .flatten()
                    .any(|asked| asked["tag"] == shared.tag.as_str())
            })
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
    let (in_row, recent) = (
        counts["in_row"].as_u64().unwrap_or(0),
        counts["recent"].as_u64().unwrap_or(0),
    );
    let key = (bot.clone(), turn);
    let mut usage = Usage::default();
    let verdicts: Vec<Verdict> = if shared.tripped.lock().unwrap().contains(&key) {
        // The turn kept going after it was told to stop: end it now.
        interrupt(&shared, &bot, turn).await;
        vec![policy::tripped(in_row.max(policy::IN_ROW), recent).unwrap(); calls.len()]
    } else if let Some(stop) = policy::tripped(in_row, recent) {
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
        vec![policy::timed_out(); calls.len()]
    } else {
        // A check that failed is tried again while the round has time, up
        // to Codex's attempts: a failure is the approver's, not the call's.
        let mut attempt = 1;
        loop {
            match judge(&shared, &bot, turn, &calls, deadline).await {
                Ok((verdicts, used)) => {
                    usage = used;
                    break verdicts;
                }
                Err(Unreviewed::Failed(_))
                    if attempt < policy::ATTEMPTS && Instant::now() < deadline =>
                {
                    attempt += 1
                }
                Err(Unreviewed::Failed(_)) if Instant::now() >= deadline => {
                    break vec![policy::timed_out(); calls.len()];
                }
                Err(Unreviewed::Failed(why)) => {
                    break vec![policy::not_reviewed(&why); calls.len()];
                }
                Err(Unreviewed::Not(verdict)) => break vec![verdict; calls.len()],
            }
        }
    };
    let judge_ms = started.elapsed().as_millis() as u64;
    let mut answered = Vec::with_capacity(calls.len());
    for (call, verdict) in calls.iter().zip(&verdicts) {
        let (mut decision, reason) = match verdict {
            Verdict::Allow => ("allow", None),
            Verdict::Deny(reason) | Verdict::Unreviewed(reason) => ("deny", Some(reason.as_str())),
            Verdict::Doubt(reason) => ("ask", Some(reason.as_str())),
        };
        // A round nobody assessed is denied without counting toward the
        // breaker, as in Codex.
        let unreviewed = matches!(verdict, Verdict::Unreviewed(_));
        let answer = |decision: &str| {
            let mut answer = json!({"bot":bot,"turn":turn,"call_id":call["call_id"],
                "request":call["request"],"tag":shared.tag,"decision":decision,"reason":reason,
                "by":"approver","lease":shared.lease});
            if unreviewed {
                answer["unreviewed"] = json!(true);
            }
            shared.client.request("answer", answer)
        };
        let mut result = answer(decision).await;
        // A gate that lapses has nobody to wait on: the doubt is a denial.
        if decision == "ask"
            && result
                .as_ref()
                .is_err_and(|error| error.code == "approval_ask_expiring")
        {
            decision = "deny";
            result = answer(decision).await;
        }
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

/// Why a round has no verdicts from the judge.
enum Unreviewed {
    /// Reading the turn or asking the judge failed, and why: worth another
    /// try.
    Failed(String),
    /// It cannot be judged now: this verdict for every call.
    Not(Verdict),
}

/// One judge request for the round, and a verdict for each call.
async fn judge(
    shared: &Shared,
    bot: &str,
    turn: i64,
    calls: &[Value],
    deadline: Instant,
) -> std::result::Result<(Vec<Verdict>, Usage), Unreviewed> {
    let late = || Unreviewed::Not(policy::timed_out());
    let long = |why: &str| Unreviewed::Not(policy::too_long(why));
    let Ok(Ok(_slot)) =
        tokio::time::timeout_at(deadline.into(), shared.judge.slots.acquire()).await
    else {
        return Err(late());
    };
    let intent =
        match tokio::time::timeout_at(deadline.into(), intent(shared, bot, turn, calls)).await {
            Err(_) => return Err(late()),
            Ok(Err(Unjudged::TooLong)) => return Err(long("the call is too long to show")),
            Ok(Err(Unjudged::Failed)) => {
                return Err(Unreviewed::Failed("the turn could not be read".into()));
            }
            Ok(Ok(intent)) => intent,
        };
    let state = match policy::build(&intent, &shared.redactor) {
        State::TooLong => return Err(long("the calls are too long to show")),
        State::Built { state, .. } => state,
    };
    let ids: Vec<&str> = intent.planned.iter().map(|call| call.id.as_str()).collect();
    match shared
        .judge
        .ask(&shared.client, &state, &ids, deadline)
        .await
    {
        Judged::Overloaded => Err(late()),
        Judged::Failed(detail) => {
            report(json!({"event":"judge_failed","bot":bot,"turn":turn,"detail":detail}));
            Err(Unreviewed::Failed(detail))
        }
        Judged::Answers { answers, usage } => {
            let verdicts = intent
                .planned
                .iter()
                .map(|call| {
                    let verdict = policy::verdict(&answers[&call.id])?;
                    Some(match call.unseen {
                        true => policy::unseen(verdict),
                        false => verdict,
                    })
                })
                .collect::<Option<Vec<_>>>()
                .ok_or_else(|| Unreviewed::Failed("the judge's answer did not parse".into()))?;
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

/// A file the turn wrote, as a judged call that names it would run it.
enum Written {
    Whole(String),
    /// Past `FILE_BYTES`, by its size: a part of it shown could not be
    /// redacted with what names a secret, nor tell what the rest does.
    TooLong(u64),
}

/// A file the turn wrote: only a regular file, opened without waiting on a
/// FIFO or device, whole within `FILE_BYTES`, read no further whatever size
/// it reported.
fn written_file(file: &Path) -> Option<Written> {
    use std::io::{Read, Seek, SeekFrom};
    use std::os::unix::fs::OpenOptionsExt;
    let mut opened = std::fs::OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NONBLOCK)
        .open(file)
        .ok()?;
    if !opened.metadata().is_ok_and(|meta| meta.is_file()) {
        return None;
    }
    let mut head = Vec::new();
    (&mut opened)
        .take(FILE_BYTES + 1)
        .read_to_end(&mut head)
        .ok()?;
    if head.len() as u64 <= FILE_BYTES {
        return Some(Written::Whole(String::from_utf8_lossy(&head).into_owned()));
    }
    Some(Written::TooLong(opened.seek(SeekFrom::End(0)).ok()?))
}

enum Unjudged {
    TooLong,
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
    // The read keeps the earliest words of the turn, so steers it left out
    // or cut may hold the person's latest word: authorization is unreadable then.
    let mut left_out = Left {
        newest: read["prompts_more"] == true,
        ..Left::default()
    };
    let mut request = Vec::new();
    let mut visited = HashSet::from([(bot.to_owned(), turn)]);
    let prompts = with_replies(read["prompts"].as_array().cloned().unwrap_or_default());
    let mut budget = PROMPT_BYTES.saturating_sub(prompt_bytes(&prompts));
    for prompt in &prompts {
        left_out.cut(prompt);
        let text = prompt_text(prompt);
        match prompt.get("from") {
            None => request.push(Words {
                by: by_origin(prompt),
                text,
                refused: refused(&prompt["refused"]),
            }),
            Some(from) => {
                persons_above(
                    client,
                    from,
                    &mut visited,
                    &mut budget,
                    &mut request,
                    &mut left_out,
                )
                .await?;
                request.push(Words {
                    by: By::Model,
                    text,
                    refused: refused(&prompt["refused"]),
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
        let mut arguments = match whole {
            true => call["arguments"].clone(),
            false => {
                let item = node_item(client, bot, &call["node"]).await?;
                policy::call_arguments(&item, call["call_id"].as_str().unwrap_or_default())
                    .ok_or(Unjudged::Failed)?
            }
        };
        let tool = call["name"].as_str().unwrap_or_default().to_owned();
        let mut unseen = false;
        if matches!(tool.as_str(), "write" | "edit") {
            (arguments, unseen) = policy::shown_change(&arguments, &shared.redactor);
        }
        size += arguments.to_string().len();
        if size > PROMPT_BYTES {
            return Err(Unjudged::TooLong);
        }
        planned.push(Planned {
            id: format!("c{}", index + 1),
            tool,
            arguments,
            files: Vec::new(),
            unseen,
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
                    json!(whole_words(preview))
                }
                Some(node) => match node_item(client, bot, &json!(node)).await {
                    Ok(item) => {
                        policy::call_arguments(&item, call["call_id"].as_str().unwrap_or_default())
                            .ok_or(Unjudged::Failed)?
                    }
                    // Too large to read back: as a call with no node.
                    Err(Unjudged::TooLong) if writes => {
                        sized = false;
                        json!({"path":preview_path(preview).ok_or(Unjudged::TooLong)?})
                    }
                    Err(Unjudged::TooLong) => {
                        allowed_cut = true;
                        json!(whole_words(preview))
                    }
                    Err(error) => return Err(error),
                },
                // Its path is what a later call needs from it; without it,
                // no file this turn wrote could be matched.
                None if writes => {
                    sized = false;
                    json!({"path":preview_path(preview).ok_or(Unjudged::TooLong)?})
                }
                None => {
                    allowed_cut = true;
                    json!(whole_words(preview))
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
    // whole, as it is on disk now: what the call would run, or by its size. One that an
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
            match read.await.map_err(|_| Unjudged::Failed)? {
                Some(Written::Whole(content)) => call.files.push((path.clone(), content)),
                Some(Written::TooLong(bytes)) => {
                    call.files
                        .push((path.clone(), format!("[{bytes} bytes, too long to show]")));
                    call.unseen = true;
                }
                None => {}
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
            text: prompt_text(prompt),
            refused: refused(&prompt["refused"]),
        })
        .collect();
    let judged: Vec<&str> = calls
        .iter()
        .filter_map(|call| call["call_id"].as_str())
        .collect();
    let transcript = transcript(client, &shared.redactor, bot, &judged).await?;
    let instructions = match &workspace {
        Some(workspace) => {
            let bot = client.request("inspect", json!({"bot":bot})).await?;
            let given = bot["instructions"].as_str().unwrap_or_default().to_owned();
            let workspace = workspace.clone();
            tokio::task::spawn_blocking(move || {
                agent_client::policy::given_agents_files(&workspace, &given)
            })
            .await
            .map_err(|_| Unjudged::Failed)?
            .into_iter()
            .map(|(path, text)| (path.to_string_lossy().into_owned(), text))
            .collect()
        }
        None => Vec::new(),
    };
    Ok(Intent {
        note: shared.note.clone(),
        workspace: workspace.map(|w| w.to_string_lossy().into_owned()),
        request,
        planned,
        allowed,
        allowed_cut,
        request_cut: left_out.any || left_out.newest,
        newest_cut: left_out.newest,
        earlier,
        transcript,
        instructions,
    })
}

/// The bot's own history the judge is shown, read newest first until the
/// transcript is full: its messages, calls, and their outputs. The calls
/// being judged are left to the state's own entries.
async fn transcript(
    client: &Client,
    redactor: &Redactor,
    bot: &str,
    judged: &[&str],
) -> std::result::Result<Transcript, Unjudged> {
    let mut transcript = Transcript::default();
    let mut from = Value::Null;
    let mut read = 0;
    'read: while read < HISTORY_NODES {
        let page = client
            .request(
                "history_nodes",
                json!({"bot":bot,"from":from,"limit":(HISTORY_NODES - read).min(400)}),
            )
            .await?;
        let nodes: Vec<i64> = page["nodes"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(|node| node["node"].as_i64())
            .collect();
        read += nodes.len();
        let mut next = 0;
        while next < nodes.len() {
            let wanted = &nodes[next..(next + HISTORY_PAGE).min(nodes.len())];
            let items = client
                .request("history_items", json!({"bot":bot,"nodes":wanted}))
                .await?;
            let items = items["items"].as_array().cloned().unwrap_or_default();
            // A read returns at least one item; fewer than asked when large.
            if items.is_empty() {
                return Err(Unjudged::Failed);
            }
            next += items.len();
            for entry in &items {
                if entry.get("error").is_some() {
                    transcript.cut = true;
                    continue;
                }
                // An item's entries newest first, as the transcript is read.
                for said in policy::entries(&entry["item"], judged).into_iter().rev() {
                    if !transcript.offer(said, redactor) {
                        break 'read;
                    }
                }
            }
        }
        from = page["next_from"].clone();
        if from.is_null() {
            return Ok(transcript.finish());
        }
    }
    transcript.cut = true;
    Ok(transcript.finish())
}

/// A `prompts` read's words with the bot's message before each, its
/// `reply`, as its model's words: what a short "yes" answers.
fn with_replies(prompts: Vec<Value>) -> Vec<Value> {
    let mut words = Vec::with_capacity(prompts.len());
    for prompt in prompts {
        if let Some(text) = prompt["reply"]["text"].as_str() {
            words.push(json!({"text":text,"origin":"reply"}));
        }
        words.push(prompt);
    }
    words
}

/// The calls a `prompts` read lists as refused after a prompt or steer.
fn refused(calls: &Value) -> Vec<Refused> {
    calls
        .as_array()
        .into_iter()
        .flatten()
        .map(|call| Refused {
            tool: call["name"].as_str().unwrap_or_default().to_owned(),
            arguments: match call["arguments_truncated"] == true {
                true => whole_words(call["arguments"].as_str().unwrap_or_default()).to_owned(),
                false => call["arguments"].as_str().unwrap_or_default().to_owned(),
            },
        })
        .collect()
}

/// Text the daemon cut at its byte budget ends without the part-word it was
/// cut in, so the redactor never sees a secret without what marks it: the
/// `@host` past a URL's password, say.
fn whole_words(text: &str) -> &str {
    text.rfind(char::is_whitespace).map_or("", |at| &text[..at])
}

/// A prompt's text as the judge is shown it.
fn prompt_text(prompt: &Value) -> String {
    let text = prompt["text"].as_str().unwrap_or_default();
    match prompt["truncated"] == true {
        true => whole_words(text).to_owned(),
        false => text.to_owned(),
    }
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
/// above it, so nothing there consents. Words left out go in `left_out`:
/// a delegating turn past the chain's limits, or a prompt or steer a read
/// cut or left out.
async fn persons_above(
    client: &Client,
    from: &Value,
    visited: &mut HashSet<(String, i64)>,
    budget: &mut usize,
    request: &mut Vec<Words>,
    left_out: &mut Left,
) -> std::result::Result<(), Unjudged> {
    // Each delegating turn's prompts in order; a model-written one is
    // replaced by the prompts above it, depth first.
    let mut levels: Vec<std::vec::IntoIter<Value>> = Vec::new();
    let mut next = Some(from.clone());
    loop {
        if let Some(from) = next.take() {
            // A turn not read at all may hold a later word too.
            if levels.len() >= CHAIN {
                left_out.newest = true;
            } else {
                match delegated(client, &from, visited, budget).await? {
                    Above::Prompts { prompts, more } => {
                        left_out.newest |= more;
                        levels.push(prompts.into_iter());
                    }
                    Above::None => {}
                    Above::Cut => left_out.newest = true,
                }
            }
        }
        let Some(level) = levels.last_mut() else {
            return Ok(());
        };
        let Some(prompt) = level.next() else {
            levels.pop();
            continue;
        };
        left_out.cut(&prompt);
        match prompt.get("from") {
            Some(above) => next = Some(above.clone()),
            None => request.push(Words {
                by: by_origin(&prompt),
                text: prompt_text(&prompt),
                refused: refused(&prompt["refused"]),
            }),
        }
    }
}

/// What a round's `prompts` reads left out of the words.
#[derive(Default)]
struct Left {
    /// Some text was cut.
    any: bool,
    /// A steer was cut or left out, or a turn was not read: a later word,
    /// which may withdraw what an earlier one asked.
    newest: bool,
}

impl Left {
    /// Note a prompt or steer as a read returned it. A turn's prompt cut
    /// at its end loses only its own tail; a steer cut came later than it.
    fn cut(&mut self, prompt: &Value) {
        if prompt["truncated"] == true {
            self.any = true;
            self.newest |= prompt["steer"] == true;
        }
    }
}

/// What a delegating turn adds above a model-written prompt.
enum Above {
    /// Its prompts, and whether steers were left out of them.
    Prompts { prompts: Vec<Value>, more: bool },
    /// Read already, or gone.
    None,
    /// Past the round's limits: left out.
    Cut,
}

/// The prompts of the turn `from` names, unless it was read already or is
/// gone, read within what is left of the round's `budget`.
async fn delegated(
    client: &Client,
    from: &Value,
    visited: &mut HashSet<(String, i64)>,
    budget: &mut usize,
) -> std::result::Result<Above, Unjudged> {
    let (Some(bot), Some(turn)) = (from["bot"].as_str(), from["turn"].as_i64()) else {
        return Ok(Above::None);
    };
    if !visited.insert((bot.to_owned(), turn)) {
        return Ok(Above::None);
    }
    if visited.len() > DELEGATING || *budget == 0 {
        return Ok(Above::Cut);
    }
    match client
        .request("prompts", json!({"bot":bot,"turn":turn,"bytes":*budget}))
        .await
    {
        Ok(mut read) => Ok(match read["prompts"].take() {
            Value::Array(prompts) => {
                let prompts = with_replies(prompts);
                *budget = budget.saturating_sub(prompt_bytes(&prompts));
                // Steers left out may hold that person's latest word.
                Above::Prompts {
                    prompts,
                    more: read["prompts_more"] == true,
                }
            }
            _ => Above::None,
        }),
        Err(error) if matches!(error.code.as_str(), "bot_not_found" | "turn_not_found") => {
            Ok(Above::None)
        }
        Err(error) => Err(error.into()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_cut_steer_is_a_later_word_left_out_and_a_cut_prompt_is_not() {
        let mut left = Left::default();
        left.cut(&json!({"text":"deploy","truncated":true}));
        assert!(left.any && !left.newest);
        left.cut(&json!({"text":"","truncated":true,"steer":true}));
        assert!(left.newest);
        let mut whole = Left::default();
        whole.cut(&json!({"text":"don't","steer":true}));
        assert!(!whole.any && !whole.newest);
    }

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
    fn text_the_daemon_cut_ends_on_a_whole_word() {
        let password = ["Zq8vK2", "mX9pL4", "tR7wY3nB"].concat();
        // Cut before the `@host` that marks a URL's password.
        let url = ["https://ci", ":", &password].concat();
        let cut = json!({"text":format!("deploy with {url}"),"truncated":true});
        assert_eq!(prompt_text(&cut), "deploy with");
        let whole = json!({"text":"deploy now"});
        assert_eq!(prompt_text(&whole), "deploy now");
        assert_eq!(whole_words("unbroken"), "");
    }

    #[test]
    fn a_written_file_is_read_only_if_regular_and_whole_within_the_limit() {
        let dir = std::env::temp_dir().join(format!("agent-written-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let small = dir.join("run.sh");
        std::fs::write(&small, "echo hi\n").unwrap();
        assert!(matches!(written_file(&small), Some(Written::Whole(text)) if text == "echo hi\n"));
        // A larger one only by its size.
        let large = dir.join("large.sh");
        std::fs::write(&large, "a".repeat(FILE_BYTES as usize + 100)).unwrap();
        assert!(matches!(
            written_file(&large),
            Some(Written::TooLong(bytes)) if bytes == FILE_BYTES + 100
        ));
        // A FIFO with no writer is skipped, not waited on.
        let fifo = dir.join("fifo");
        let path = std::ffi::CString::new(fifo.as_os_str().as_encoded_bytes()).unwrap();
        // SAFETY: path is a NUL-terminated string that outlives the call.
        assert_eq!(unsafe { libc::mkfifo(path.as_ptr(), 0o600) }, 0);
        assert!(written_file(&fifo).is_none());
        assert!(written_file(&dir.join("gone")).is_none());
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
