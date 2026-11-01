//! `sonny approver`: serve a gate tag and have a judge model decide every
//! call waiting on it, in one request a round. The policy (what the judge is
//! told, the verdicts, what it sees, and redaction) is
//! `agent_client::approver` and the judge's transport `agent_client::judge`;
//! this is the loop. Sonny keeps it as the daemon's companion; the daemon
//! holds only the gates it serves.
//! As in Codex, a denial goes back to the model at once, and a round the
//! judge never assessed is denied as a review failure; only a call too long
//! to show is handed to a person, where one can be asked.
use agent_client::Error;
use agent_client::{
    Client,
    approver::{
        self as policy, Allowed, By, Intent, Planned, Redactor, Refused, State, Transcript,
        Verdict, Words,
    },
    judge::{self, Failed, Usage},
};
use serde_json::{Value, json};
use std::{
    collections::{HashMap, HashSet},
    path::{Path, PathBuf},
    sync::{Arc, Mutex},
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};
use tokio::sync::Notify;

pub struct Settings {
    pub socket: PathBuf,
    pub tag: String,
    /// The judge `--judge` or the config's `[judge] model` names, if either
    /// does.
    pub judge: Option<String>,
    /// The model the bots being judged run, `AGENT_MODEL`, when no judge
    /// is named: the last in the order a judge is picked in.
    pub caller: Option<String>,
    /// A model judge's effort.
    pub effort: Option<String>,
    /// The environment note the judge always sees.
    pub note: Option<String>,
}

/// The lease a tag is served under, renewed well inside it.
const LEASE_MS: u64 = 5_000;
const RENEW: Duration = Duration::from_secs(1);
/// Requests in flight at the daemon at once. Rounds past these queue for
/// room one request at a time, never for a whole round, so a slow judge
/// holds nothing; renewals skip the queue.
const IN_FLIGHT: usize = 64;
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

type Result<T> = std::result::Result<T, Error>;

/// What the daemon counts for each prompt a `prompts` read returns, past
/// its text, against the read's `bytes`.
const PROMPTS_ENTRY: usize = 64;

fn epoch_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_millis() as i64)
}

const USAGE: &str = "usage: sonny approver [--tag TAG] [--judge PROVIDER/MODEL] [--effort LEVEL] [--note FILE] [--store PATH] [--socket PATH]";

/// `sonny approver`: the flags over the person's `[judge]` in
/// ~/.agent/config.toml, then `AGENT_MODEL` for the judge, and
/// `AGENT_APPROVER_NOTE` for the note.
pub fn run(args: &[String]) -> std::result::Result<(), String> {
    let env = |name: &str| std::env::var(name).ok().filter(|v| !v.is_empty());
    let (mut store, mut socket, mut tag, mut judge, mut effort) = (None, None, None, None, None);
    let mut note = env("AGENT_APPROVER_NOTE").map(PathBuf::from);
    let mut args = args.iter();
    while let Some(arg) = args.next() {
        let (flag, inline) = match arg.split_once('=') {
            Some((flag, value)) => (flag, Some(value.to_owned())),
            None => (arg.as_str(), None),
        };
        let mut value = || inline.clone().or_else(|| args.next().cloned()).ok_or(USAGE);
        match flag {
            "--store" => store = Some(PathBuf::from(value()?)),
            "--socket" => socket = Some(PathBuf::from(value()?)),
            "--tag" => tag = Some(value()?),
            "--judge" => judge = Some(value()?),
            "--effort" => effort = Some(value()?),
            "--note" => note = Some(PathBuf::from(value()?)),
            _ => return Err(USAGE.into()),
        }
    }
    let (_, socket) = crate::daemon_paths(store, socket)?;
    let config = agent_client::config::load()?;
    let note = note.as_deref().map(read_note).transpose()?;
    let settings = Settings {
        socket,
        tag: tag.unwrap_or_else(|| "auto".into()),
        judge: judge.or(config.judge.model),
        caller: env("AGENT_MODEL"),
        effort: effort.or(config.judge.effort),
        note,
    };
    main(settings).map(drop).map_err(|error| {
        serde_json::json!({"error": error.code, "detail": error.detail}).to_string()
    })
}

/// The note file, which the judge sees whole: refused past what it reads.
fn read_note(path: &Path) -> std::result::Result<String, String> {
    use std::io::Read;
    use std::os::unix::fs::OpenOptionsExt;
    let unreadable = || format!("cannot read {}", path.display());
    let file = std::fs::OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NONBLOCK)
        .open(path)
        .map_err(|_| unreadable())?;
    if !file.metadata().is_ok_and(|meta| meta.is_file()) {
        return Err(unreadable());
    }
    let limit = policy::STATE_TOKENS * 3;
    let mut note = Vec::new();
    file.take(limit as u64 + 1)
        .read_to_end(&mut note)
        .map_err(|_| unreadable())?;
    if note.len() > limit {
        return Err(format!(
            "{} is over {limit} bytes, more than a judge sees",
            path.display()
        ));
    }
    String::from_utf8(note).map_err(|_| unreadable())
}

pub fn main(settings: Settings) -> Result<i32> {
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .map_err(|e| Error::with("io", &e.to_string()))?
        .block_on(serve(settings))
}

/// What a judge request came back with.
enum Judged {
    /// An assessment for every call, by its id.
    Answers {
        answers: Value,
        usage: Usage,
    },
    Failed(String),
}

struct Judge {
    base: judge::Base,
    /// How long a round may wait for its verdict, from its announcement.
    deadline_ms: u64,
}

impl Judge {
    /// The judge's answers for a round of a bot working in `workspace`,
    /// where a model judge with tools starts to look.
    async fn ask(
        &self,
        client: &Client,
        state: &Value,
        ids: &[&str],
        workspace: Option<&str>,
        deadline: Instant,
    ) -> Judged {
        let prompt = policy::model_prompt(state, ids);
        let answered = self
            .base
            .ask_with(
                client,
                &prompt,
                ids,
                policy::assessments,
                workspace,
                deadline,
            )
            .await;
        match answered {
            Ok((answers, usage)) => Judged::Answers {
                answers: Value::Object(answers),
                usage,
            },
            Err(Failed(why)) => Judged::Failed(why),
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
    let (client, mut events) = Client::connect(&settings.socket).await?;
    client.bound(IN_FLIGHT);
    let named =
        judge::pick(settings.judge.as_deref(), settings.caller.as_deref()).ok_or_else(|| {
            Error::with(
                "judge_required",
                "auto needs a judge: pass --judge PROVIDER/MODEL, or set [judge] model in ~/.agent/config.toml or AGENT_MODEL",
            )
        })?;
    // An approver the daemon refuses the tag never replaces the base of
    // the one serving it.
    if !judge::is_model(&named) {
        return Err(Error::with(
            "invalid_judge",
            &format!("{named} is not PROVIDER/MODEL"),
        ));
    }
    if tag.len() > JUDGE_NAME_ROOM {
        return Err(Error::with(
            "invalid_tag",
            &format!("a judge's tag is at most {JUDGE_NAME_ROOM} bytes, to name its forks"),
        ));
    }
    let served = client
        .request(
            "serve_approvals",
            json!({"tag":tag,"lease_ms":LEASE_MS,"limit":256}),
        )
        .await?;
    let (Some(lease), Some(through)) = (served["lease"].as_u64(), served["through"].as_i64())
    else {
        return Err(Error::new("daemon_protocol_mismatch"));
    };
    // It looks before it answers only where a sandbox keeps it read-only,
    // as Codex's guardian does: the daemon refuses the sandbox where none
    // runs.
    let name = format!("approver.{tag}");
    let make = |tools: &'static [&'static str]| {
        judge::Base::make(
            &client,
            &name,
            &named,
            settings.effort.as_deref(),
            policy::JUDGE_INSTRUCTIONS,
            tools,
        )
    };
    let base = match make(&judge::LOOKING).await {
        Err(error) if error.code == "sandbox_unavailable" => make(&[]).await?,
        made => made?,
    };
    let shared = Arc::new(Shared {
        client,
        judge: Judge {
            base,
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
            .await?;
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
    let intent =
        match tokio::time::timeout_at(deadline.into(), intent(shared, bot, turn, calls)).await {
            Err(_) => return Err(late()),
            Ok(Err(Unjudged::TooLong)) => return Err(long("the call is too long to show")),
            Ok(Err(Unjudged::Unnamed)) => {
                return Err(long(
                    "an earlier call this turn changed files it no longer names",
                ));
            }
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
        .ask(
            &shared.client,
            &state,
            &ids,
            intent.workspace.as_deref(),
            deadline,
        )
        .await
    {
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
                    let verdict = match call.unseen {
                        true => policy::unseen(verdict),
                        false => verdict,
                    };
                    Some(match call.own_home {
                        true => policy::own_home(verdict),
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
        .map(|prompt| prompt["text"].as_str().map_or(0, str::len) + PROMPTS_ENTRY)
        .sum()
}

/// Whether a tool changes files: its calls are judged by the files they
/// change, not their contents.
fn changes_files(tool: &str) -> bool {
    matches!(tool, "write" | "edit" | "apply_patch")
}

/// The files a call changes: a write's or edit's `path`, or every file a
/// patch adds, deletes, updates or moves to, or the `paths` read from a
/// cut patch.
fn changed_paths(tool: &str, arguments: &Value) -> Vec<String> {
    match tool {
        "write" | "edit" => arguments["path"]
            .as_str()
            .map(str::to_owned)
            .into_iter()
            .collect(),
        "apply_patch" => match arguments["paths"].as_array() {
            Some(paths) => paths
                .iter()
                .filter_map(Value::as_str)
                .map(str::to_owned)
                .collect(),
            None => patch_paths(arguments["input"].as_str().unwrap_or_default()),
        },
        _ => Vec::new(),
    }
}

/// The agent's home as a bot's calls may reach it, given the folders it may
/// write (its own and the writable ones, as `inspect` has them); none
/// without a `$HOME`. The home and every folder given in it are kept both
/// as `$HOME` spells them and by their real paths, so a path a command
/// spells either way is matched against the same spelling.
fn own_home(given: &[String]) -> Option<policy::Home> {
    Some(own_home_in(PathBuf::from(std::env::var_os("HOME")?), given))
}

fn own_home_in(home: PathBuf, given: &[String]) -> policy::Home {
    let agent = home.join(".agent");
    let real = agent_client::files::real(&agent).filter(|real| *real != agent);
    let mut folders = Vec::new();
    for folder in given {
        let folder = PathBuf::from(folder);
        let resolved = agent_client::files::real(&folder).unwrap_or_else(|| folder.clone());
        // A folder in the home's real place, as the home's own name spells it.
        if let Some(real) = &real
            && let Ok(rest) = resolved.strip_prefix(real)
        {
            folders.push(agent.join(rest));
        }
        folders.push(resolved);
        folders.push(folder);
    }
    folders.sort();
    folders.dedup();
    policy::Home {
        home,
        agent: std::iter::once(agent).chain(real).collect(),
        given: folders,
    }
}

/// Whether a planned call reaches the agent's home: a write, edit or patch
/// of a file there, by the path it resolves to, or a command that would
/// run outside the sandbox and names one. `folder` is the turn's, which a
/// relative path is read from; `confined` says the bot has a sandbox, in
/// which a command that does not escalate runs.
fn reaches_home(
    home: &policy::Home,
    tool: &str,
    arguments: &Value,
    changed: &[String],
    folder: Option<&Path>,
    confined: bool,
) -> bool {
    let absolute = |path: &str| -> Option<PathBuf> {
        let path = match path {
            "~" | "$HOME" | "${HOME}" => home.home.clone(),
            path => match ["~/", "$HOME/", "${HOME}/"]
                .iter()
                .find_map(|prefix| path.strip_prefix(prefix))
            {
                Some(rest) => home.home.join(rest),
                None => PathBuf::from(path),
            },
        };
        match path.is_absolute() {
            true => Some(path),
            false => Some(folder?.join(path)),
        }
    };
    match tool {
        "write" | "edit" | "apply_patch" => changed.iter().any(|path| {
            // An unknown folder is as near as it could be.
            let Some(path) = absolute(path) else {
                return true;
            };
            home.reaches(&path)
                || agent_client::files::real(&path).is_none_or(|real| home.reaches(&real))
        }),
        "shell" => {
            if confined && arguments["sandbox_permissions"] != "require_escalated" {
                return false;
            }
            let workdir = arguments["workdir"].as_str().and_then(absolute);
            let mut text = String::new();
            strings(arguments, &mut text);
            home.named_by(&text, workdir.as_deref().or(folder))
        }
        _ => false,
    }
}

/// The files a patch names on its file lines. Also read from a cut
/// arguments preview, where the patch is still JSON-escaped, so a literal
/// `\n` ends a line too.
fn patch_paths(text: &str) -> Vec<String> {
    const MARKERS: [&str; 4] = [
        "*** Add File: ",
        "*** Delete File: ",
        "*** Update File: ",
        "*** Move to: ",
    ];
    let mut paths: Vec<String> = Vec::new();
    let mut seen = std::collections::HashSet::new();
    for line in text.split('\n').flat_map(|line| line.split("\\n")) {
        let line = line.trim();
        if let Some(path) = MARKERS.iter().find_map(|marker| line.strip_prefix(marker))
            && !path.is_empty()
            && seen.insert(path)
        {
            paths.push(path.to_owned());
        }
    }
    paths
}

/// The files a file-changing call too large to read back changes: as the
/// daemon named them whole, else as its preview names them.
fn cut_files(tool: &str, call: &Value, preview: &str) -> std::result::Result<Value, Unjudged> {
    match (tool, call["paths"].as_array().map(Vec::as_slice)) {
        ("apply_patch", Some(paths)) => Ok(json!({"paths":paths})),
        (_, Some([path])) => Ok(json!({"path":path})),
        _ => cut_change(tool, preview),
    }
}

/// What is shown of a file-changing call too large to read back: the
/// files it changes, when its preview names them.
fn cut_change(tool: &str, preview: &str) -> std::result::Result<Value, Unjudged> {
    match tool {
        "apply_patch" => {
            // The line the cut falls in may hold part of a path: only
            // lines it ended are read.
            let ended = preview.rfind("\\n").map_or("", |end| &preview[..end]);
            let paths = patch_paths(ended);
            if paths.is_empty() {
                return Err(Unjudged::TooLong);
            }
            Ok(json!({"paths":paths}))
        }
        _ => Ok(json!({"path":preview_path(preview).ok_or(Unjudged::TooLong)?})),
    }
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
    /// An earlier call of the turn changed files that cannot be read back,
    /// any of which the calls may run.
    Unnamed,
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
    // The files each call changes, read before its contents are clipped
    // for showing, which can leave out a patch's file lines.
    let mut changes = Vec::with_capacity(calls.len());
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
        changes.push(changed_paths(&tool, &arguments));
        if changes_files(&tool) {
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
            own_home: false,
        });
    }
    let mut allowed = Vec::new();
    let mut allowed_cut = read["calls_more"] == true;
    let mut written = Vec::new();
    // Whether an earlier call changed files not all of which are named: a
    // later call may run one unseen.
    let mut written_cut = false;
    // Arguments read whole for calls whose preview was cut; past the text
    // limit only a write's or edit's path is still read, and kept small.
    let mut expanded = 0;
    for call in read["calls"].as_array().into_iter().flatten() {
        let tool = call["name"].as_str().unwrap_or_default().to_owned();
        let preview = call["arguments"].as_str().unwrap_or_default();
        let writes = changes_files(&tool);
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
                        cut_files(&tool, call, preview).map_err(|_| Unjudged::Unnamed)?
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
                    cut_files(&tool, call, preview).map_err(|_| Unjudged::Unnamed)?
                }
                None => {
                    allowed_cut = true;
                    json!(whole_words(preview))
                }
            },
        };
        // A file's content is not what consent is about: its path and size.
        if writes && let Value::Object(fields) = &arguments {
            let paths = changed_paths(&tool, &arguments);
            // A failed write changed nothing this turn: its file is not shown.
            if call["failed"] != true {
                written.extend(paths.iter().cloned());
                // A patch read from its cut preview alone may name more.
                written_cut |= call["paths_cut"] == true
                    || (tool == "apply_patch" && !sized && !call["paths"].is_array());
            }
            let bytes: usize = fields
                .iter()
                .filter(|(name, _)| !matches!(name.as_str(), "path" | "paths"))
                .map(|(_, value)| value.as_str().map_or(0, str::len))
                .sum();
            arguments = match tool.as_str() {
                "apply_patch" => json!({"paths":paths,"bytes":sized.then_some(bytes)}),
                _ => json!({"path":fields.get("path"),"bytes":sized.then_some(bytes)}),
            };
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
    let changed_by_call = changes.clone();
    for (call, changed) in planned.iter_mut().zip(changes) {
        call.unseen |= written_cut;
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
        for path in changed {
            changing.push((path, call.id.clone()));
        }
    }
    let earlier = read["earlier"]
        .as_array()
        .into_iter()
        .flatten()
        .flat_map(|prompt| {
            let quotes = quoted(prompt["quotes"].clone(), prompt);
            std::iter::once(prompt.clone()).chain(quotes)
        })
        .map(|prompt| Words {
            by: match prompt.get("from") {
                Some(_) => By::Model,
                None => by_origin(&prompt),
            },
            text: prompt_text(&prompt),
            refused: refused(&prompt["refused"]),
        })
        .collect();
    let judged: Vec<&str> = calls
        .iter()
        .filter_map(|call| call["call_id"].as_str())
        .collect();
    let transcript = transcript(client, &shared.redactor, bot, &judged).await?;
    let inspected = client.request("inspect", json!({"bot":bot})).await?;
    let settings = &inspected["settings"];
    let sandbox = matches!(
        settings["sandbox"].as_str(),
        Some("read-only" | "workspace-write")
    )
    .then(|| {
        json!({"mode":settings["sandbox"],"writable":settings["writable"],
            "network":settings["network"]})
    });
    // Only a person may let a call reach the agent's home. Off the runtime
    // thread: resolving a path may wait on a slow mount, and a stall here
    // must not hold up the lease's renewal.
    {
        let folder = workspace.clone();
        let confined = sandbox.is_some();
        let given: Vec<String> = (inspected["workspace"].as_str().into_iter())
            .chain(
                (inspected["settings"]["writable"]
                    .as_array()
                    .into_iter()
                    .flatten())
                .filter_map(Value::as_str),
            )
            .map(str::to_owned)
            .collect();
        let calls: Vec<(String, Value)> = (planned.iter())
            .map(|call| (call.tool.clone(), call.arguments.clone()))
            .collect();
        let flags = tokio::task::spawn_blocking(move || {
            let Some(home) = own_home(&given) else {
                return vec![false; calls.len()];
            };
            (calls.iter().zip(changed_by_call))
                .map(|((tool, arguments), changed)| {
                    reaches_home(
                        &home,
                        tool,
                        arguments,
                        &changed,
                        folder.as_deref(),
                        confined,
                    )
                })
                .collect::<Vec<bool>>()
        })
        .await
        .map_err(|_| Unjudged::Failed)?;
        for (call, flag) in planned.iter_mut().zip(flags) {
            call.own_home = flag;
        }
    }
    let instructions = match &workspace {
        Some(workspace) => {
            let given = inspected["instructions"]
                .as_str()
                .unwrap_or_default()
                .to_owned();
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
        sandbox,
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
                for said in policy::entries(&entry["item"], judged, redactor)
                    .into_iter()
                    .rev()
                {
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
/// `reply`, as its model's words: what a short "yes" answers; and the
/// words each quotes after it, by whoever first wrote them.
fn with_replies(prompts: Vec<Value>) -> Vec<Value> {
    let mut words = Vec::with_capacity(prompts.len());
    for mut prompt in prompts {
        if let Some(text) = prompt["reply"]["text"].as_str() {
            words.push(json!({"text":text,"origin":"reply"}));
        }
        let quotes = prompt.as_object_mut().and_then(|p| p.remove("quotes"));
        let parent = prompt.clone();
        words.push(prompt);
        words.extend((quotes.into_iter()).flat_map(|quotes| quoted(quotes, &parent)));
    }
    words
}

/// `parent`'s quotes as words of their own: the person's own words as
/// theirs, the daemon having copied them from the person's turn, when the
/// person sent `parent`, so chose them; any other's, an agent's or a client's,
/// as asking for nothing, with no turn above them to read, since quoting
/// is not delegating. The person's words another picked are theirs but
/// not their word on this: an agent could pick an old "yes" for a new ask.
/// Quotes of a steer are a steer's words: cut, they may be a later word.
fn quoted(quotes: Value, parent: &Value) -> Vec<Value> {
    let chosen = by_person(parent);
    let Value::Array(quotes) = quotes else {
        return Vec::new();
    };
    quotes
        .into_iter()
        .map(|quote| {
            let mut words = json!({"text":quote["text"]});
            if quote["truncated"] == true {
                words["truncated"] = json!(true);
            }
            // Only the quotes the person picked themselves: one that came
            // along inside the turn they quoted was picked by its writer.
            if !(chosen && by_person(&quote) && quote["nested"] != true) {
                words["origin"] = json!("quote");
            }
            if parent["steer"] == true {
                words["steer"] = json!(true);
            }
            words
        })
        .collect()
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

/// Whether the person wrote it: no bot's turn, client or other person.
fn by_person(words: &Value) -> bool {
    words["from"].is_null() && words["origin"].is_null() && words["person"].is_null()
}

/// Who wrote a prompt no bot's turn wrote: the person, unless a client sent
/// it on its own (`origin`) or for another person it names (`person`),
/// whose words ask for nothing either.
fn by_origin(prompt: &Value) -> By {
    if prompt.get("origin").is_some() || prompt.get("person").is_some() {
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
    fn only_a_call_that_would_leave_its_sandbox_for_the_agent_s_home_waits_for_the_person() {
        let dir = std::env::temp_dir().join(format!("sonny-own-home-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let root = std::fs::canonicalize(&dir).unwrap();
        let agent = root.join(".agent");
        let worktree = agent.join("worktrees/shop-fix");
        std::fs::create_dir_all(&worktree).unwrap();
        let home = policy::Home {
            home: root.clone(),
            agent: vec![agent.clone()],
            given: vec![root.join("code"), worktree.clone()],
        };
        let folder = root.join("code");
        let reaches = |tool: &str, arguments: Value, confined: bool| {
            let changed = changed_paths(tool, &arguments);
            reaches_home(&home, tool, &arguments, &changed, Some(&folder), confined)
        };
        let job = "~/.agent/triggers/jobs/x.json";
        // In its sandbox a command can't write there: nothing to hold.
        assert!(!reaches(
            "shell",
            json!({"command": format!("touch {job}")}),
            true
        ));
        let escalated = json!({"command": format!("touch {job}"),
            "sandbox_permissions": "require_escalated"});
        assert!(reaches("shell", escalated.clone(), true));
        // Unconfined, every command runs as the person.
        assert!(reaches(
            "shell",
            json!({"command": format!("touch {job}")}),
            false
        ));
        // Named from a workdir in the home.
        assert!(reaches(
            "shell",
            json!({"command": "rm -rf triggers/../.agent",
            "workdir": "~", "sandbox_permissions": "require_escalated"}),
            true
        ));
        assert!(!reaches(
            "shell",
            json!({"command": "git push",
            "workdir": worktree.to_str().unwrap(),
            "sandbox_permissions": "require_escalated"}),
            true
        ));
        // A write by its path, relative or through a link.
        assert!(reaches(
            "write",
            json!({"path": "../.agent/config.toml", "content": "x"}),
            true
        ));
        assert!(reaches("write", json!({"path": job, "content": "x"}), true));
        std::os::unix::fs::symlink(&agent, root.join("code-link")).unwrap();
        assert!(reaches(
            "write",
            json!({"path": root.join("code-link/env"), "content": "x"}),
            true
        ));
        assert!(!reaches(
            "write",
            json!({"path": worktree.join("a.rs"), "content": "x"}),
            true
        ));
        assert!(!reaches(
            "write",
            json!({"path": root.join("notes.md"), "content": "x"}),
            true
        ));
        let patch = "*** Begin Patch\n*** Add File: ../.agent/rules/x.rules\n+x\n*** End Patch";
        assert!(reaches("apply_patch", json!({"input": patch}), true));
        // Other tools, such as Sonny's own triggers tool, are its way in.
        assert!(!reaches("triggers", json!({"add": {"message": job}}), true));
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn a_home_kept_elsewhere_through_a_link_matches_its_given_folders_either_way() {
        let dir = std::env::temp_dir().join(format!("sonny-linked-home-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let root = {
            std::fs::create_dir_all(&dir).unwrap();
            std::fs::canonicalize(&dir).unwrap()
        };
        let kept = root.join("disk/agent");
        let worktree = kept.join("worktrees/shop-fix");
        std::fs::create_dir_all(&worktree).unwrap();
        std::os::unix::fs::symlink(&kept, root.join(".agent")).unwrap();
        // The daemon gives the worktree by its real path.
        let home = own_home_in(root.clone(), &[worktree.to_str().unwrap().to_owned()]);
        let folder = root.join("code");
        let reaches = |tool: &str, arguments: Value| {
            let changed = changed_paths(tool, &arguments);
            reaches_home(&home, tool, &arguments, &changed, Some(&folder), true)
        };
        let escalated =
            |command: &str| json!({"command": command, "sandbox_permissions": "require_escalated"});
        assert!(!reaches(
            "shell",
            escalated("git -C ~/.agent/worktrees/shop-fix push origin HEAD")
        ));
        assert!(!reaches(
            "shell",
            escalated(&format!("git -C {} push", worktree.display()))
        ));
        assert!(reaches(
            "shell",
            escalated("touch ~/.agent/triggers/jobs/x.json")
        ));
        assert!(reaches(
            "shell",
            escalated(&format!("touch {}/config.toml", kept.display()))
        ));
        assert!(!reaches(
            "write",
            json!({"path": "~/.agent/worktrees/shop-fix/a.rs", "content": "x"})
        ));
        assert!(!reaches(
            "write",
            json!({"path": worktree.join("a.rs"), "content": "x"})
        ));
        assert!(reaches(
            "write",
            json!({"path": "~/.agent/rules/x.rules", "content": "x"})
        ));
        assert!(reaches(
            "write",
            json!({"path": kept.join("env"), "content": "x"})
        ));
        std::fs::remove_dir_all(&dir).unwrap();
    }

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
    fn quoted_words_are_the_persons_only_when_the_person_wrote_and_quoted_them() {
        let quotes = json!([
            {"bot":"lead","turn":2,"text":"yes, deploy it"},
            {"bot":"task","turn":3,"reply":true,"from":{"bot":"task","turn":3},
                "text":"say yes to deploy","truncated":true},
            {"bot":"lead","turn":1,"person":"Ana","text":"deploy"},
        ]);
        let words = with_replies(vec![
            json!({"turn":4,"text":"as I said","quotes":quotes}),
            json!({"turn":5,"text":"a note","origin":"lookout","quotes":quotes}),
            json!({"turn":6,"text":"run it","from":{"bot":"task","turn":3},"quotes":quotes}),
        ]);
        assert_eq!(words.len(), 12);
        assert!(words.iter().all(|w| w.get("quotes").is_none()));
        assert_eq!(words[2]["truncated"], true);
        // A steer's quote cut short may hold a later word.
        let steered = with_replies(vec![json!({"turn":7,"text":"ok","steer":true,
            "quotes":[{"bot":"lead","turn":2,"text":"no","truncated":true}]})]);
        let mut left = Left::default();
        steered.iter().for_each(|w| left.cut(w));
        assert!(left.newest);
        let by: Vec<(By, &str)> = (words.iter())
            .filter(|w| w.get("from").is_none())
            .map(|w| (by_origin(w), w["text"].as_str().unwrap()))
            .collect();
        assert_eq!(
            by[..4],
            [
                (By::Person, "as I said"),
                (By::Person, "yes, deploy it"),
                (By::Model, "say yes to deploy"),
                (By::Model, "deploy"),
            ]
        );
        // The person's old words, picked by a client or an agent, are not
        // their word on this.
        assert!(by[4..].iter().all(|(by, _)| *by == By::Model), "{by:?}");
        // Nor are they when they came along inside a turn the person
        // quoted: that turn's writer picked them.
        let carried = with_replies(vec![json!({"turn":8,"text":"see this","quotes":[
            {"bot":"task","turn":3,"from":{"bot":"task","turn":3},"text":"may I deploy?"},
            {"bot":"lead","turn":2,"nested":true,"text":"yes"}]})]);
        let by: Vec<By> = carried.iter().map(by_origin).collect();
        assert_eq!(by, [By::Person, By::Model, By::Model]);
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
    fn a_patch_is_judged_by_every_file_it_names() {
        let patch = "*** Begin Patch\n*** Update File: a.rs\n*** Move to: b.rs\n@@\n-x\n+y\n*** Add File: c.rs\n+z\n*** Delete File: a.rs\n*** End Patch";
        assert_eq!(
            changed_paths("apply_patch", &json!({"input": patch})),
            ["a.rs", "b.rs", "c.rs"]
        );
        // Cut short, still JSON-escaped: the files named before the cut.
        let preview = &json!({"input": patch}).to_string()[..72];
        assert_eq!(
            cut_change("apply_patch", preview).ok(),
            Some(json!({"paths":["a.rs", "b.rs"]}))
        );
        // A path the cut falls in is not read as a shorter one.
        let preview = &json!({"input": patch}).to_string()[..65];
        assert_eq!(
            cut_change("apply_patch", preview).ok(),
            Some(json!({"paths":["a.rs"]}))
        );
        assert!(cut_change("apply_patch", r#"{"input":"*** Begin Pa"#).is_err());
        // The files the daemon named whole win over a cut preview's.
        let call = json!({"paths":["a.rs", "b.rs", "c.rs", "run.sh"]});
        assert_eq!(
            cut_files("apply_patch", &call, preview).ok(),
            Some(json!({"paths":["a.rs", "b.rs", "c.rs", "run.sh"]}))
        );
        assert_eq!(
            cut_files("apply_patch", &json!({}), preview).ok(),
            Some(json!({"paths":["a.rs"]}))
        );
        // What a cut preview yields still names its files.
        assert_eq!(
            changed_paths("apply_patch", &json!({"paths":["a.rs", "b.rs"]})),
            ["a.rs", "b.rs"]
        );
        assert_eq!(changed_paths("edit", &json!({"path": "x"})), ["x"]);
        assert!(changed_paths("shell", &json!({"command": "*** Add File: x"})).is_empty());
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
