//! Workflows: a way an agent works, as Claude Code's dynamic workflows
//! are. The agent writes a script (`script`) that runs many fresh agents
//! in code, and hears back once, when it ends. The `workflow` tool starts
//! it; the tool host runs it, on a connection of its own, and the daemon
//! sees only bots, submits and waits.
//!
//! Each `agent()` call makes a worker: a bot the caller made, in its
//! folder (or a worktree of it), under its permissions row and gates, told
//! its folder's rules with what a workflow's worker is told, and given
//! none of the tools that start agents, so a run's fan-out stays where its
//! script says. Workers are no tasks: no tab, no `/status` row, no note to
//! the lead per worker; the run's one message stands in for them.
//!
//! A run is kept in a folder beside the store: `run.json`, the script, an
//! append-only journal and, once it ends, `result.json`. The journal is
//! the run's truth. A worker is named and asked by its call's key, the
//! hash of its prompt and options with how many such calls came before,
//! so asking again, after the host or its connection went, gets the same
//! bot and turn, and a run is picked up by running its script again with
//! each call it recorded answered from the journal. A run resumed from
//! another starts with that one's answers, so only the calls that changed
//! cost model calls.
pub mod schema;
pub mod script;

use agent_client::Client;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::collections::{BTreeSet, HashMap, HashSet};
use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, LazyLock, Mutex};
use std::time::Duration;
use tokio::sync::{Notify, Semaphore};

pub const NAME: &str = "workflow";

/// What starts the part of a worker's name after its project's: no
/// task's, which `start_task` refuses to start under it.
pub const WORKER: &str = "wf-";

/// How long a run waits to try the daemon again.
const AGAIN: Duration = Duration::from_secs(2);

/// The most of a run's result its message holds; the rest is in its
/// `result.json`.
const TOLD_BYTES: usize = 8 * 1024;

/// A worker's turn in `Run::live` while its submission has no answer.
const SUBMITTED: i64 = -1;

/// The statuses of a turn that has ended.
const ENDED: [&str; 4] = ["completed", "failed", "interrupted", "steered"];

/// What every worker is told after its folder's rules.
const TOLD: &str = "You are one agent in a workflow: a script another agent wrote runs you, and \
passes your final reply to its next step in code. Your final reply is that step's value, not a \
message to a person: give exactly what the prompt asks for, as data, with no greeting, summary \
of your process or sign-off. No one reads your conversation or answers questions while you work, \
so decide for yourself and say in the reply what you assumed. Do what the prompt asks and \
nothing more; change files only when it says to.";

/// Whether agents have the `workflow` tool: `[sonny.workflows] enabled`.
pub fn on() -> bool {
    agent_client::config::load().map_or(true, |config| config.sonny.workflows.enabled)
}

/// Whether `bot` is a workflow's worker, by its name.
pub fn is_worker(bot: &str) -> bool {
    worker_project(bot).is_some()
}

/// The project of worker `bot`, named `{project}.wf-{run}-{key}`: all
/// before its last dot, as a project's name may hold dots.
pub fn worker_project(bot: &str) -> Option<&str> {
    bot.rsplit_once('.')
        .filter(|(project, rest)| !project.is_empty() && rest.starts_with(WORKER))
        .map(|(project, _)| project)
}

/// The `workflow` tool, while workflows are on.
pub fn tool() -> Option<Value> {
    on().then(|| {
        json!({
            "name": NAME,
            "answered_by": crate::served::TAG,
            "description": "Run a workflow: a JavaScript plan that runs many fresh agents in code \
                and tells you once, when it ends. For work in many independent pieces (an audit \
                across files, find-then-verify, a migration, research over many sources) or when \
                the person asks for one; scout the work list first. Read the workflow skill \
                before writing one. Agents work in your folder with your permissions, each \
                costing tokens. Returns at once with the run's id.",
            "parameters": {
                "type": "object",
                "properties": {
                    "script": {"type": "string", "minLength": 1,
                        "description": "The script, starting with export const meta."},
                    "script_path": {"type": "string", "minLength": 1,
                        "description": "A script file to run instead."},
                    "name": {"type": "string", "minLength": 1,
                        "description": "A saved workflow, .agents/workflows/NAME.js, to run \
                            instead."},
                    "args": {"description": "Any JSON value: the script's args."},
                    "resume_from_run_id": {"type": "string", "minLength": 1,
                        "description": "An earlier run whose answers to unchanged agent() \
                            calls this run reuses."},
                    "budget_tokens": {"type": "integer", "minimum": 1,
                        "description": "Only when the person names a token budget; by default a run has none. The run's budget.total, in output tokens."},
                    "stop": {"type": "string", "minLength": 1,
                        "description": "A run's id to end now, given alone."}
                },
                "additionalProperties": false
            }
        })
    })
}

/// Where the runs of the daemon of `store` are kept.
pub fn runs(store: &Path) -> PathBuf {
    let store = agent_client::socket::canonical_store(store).unwrap_or_else(|_| store.to_owned());
    let mut dir = store.into_os_string();
    dir.push(".workflows");
    PathBuf::from(dir)
}

/// What a run was started with, which a host that picks it up again reads.
#[derive(Clone, Debug, Serialize, Deserialize)]
struct Plan {
    id: String,
    /// The agent that started it, by name and id, and its turn that did.
    caller: String,
    caller_id: i64,
    turn: i64,
    /// The project its workers are named in, and its lead's folder, whose
    /// roles they are told.
    project: String,
    roles: PathBuf,
    /// The caller's folder, where its workers work.
    workspace: PathBuf,
    model: String,
    effort: Option<String>,
    /// The caller's permissions row, whose sandbox its workers get.
    mode: String,
    state: Option<String>,
    args: Value,
    budget: Option<u64>,
    meta: Value,
    resumed_from: Option<String>,
    started_ms: u64,
    max_agents: u64,
    at_once: usize,
}

/// A run going on in this host.
struct Run {
    plan: Plan,
    script: script::Script,
    dir: PathBuf,
    store: PathBuf,
    socket: PathBuf,
    /// Set by `stop`: the script is interrupted and the run ends stopped.
    stopping: AtomicBool,
    /// Woken when the run is to stop, or its connection is lost.
    halt: Notify,
    /// The recorded answers, by call key, and whether each came from the
    /// run it was resumed from.
    answers: Mutex<HashMap<String, Answer>>,
    spent: AtomicU64,
    /// Workers whose turn runs, by name, for `stop`.
    live: Mutex<HashMap<String, i64>>,
    /// Workers given a worktree, cleaned up once the run ends.
    trees: Mutex<BTreeSet<String>>,
    /// Held while a worktree is made, which a cancelled call does not
    /// stop: the run's end waits for every one.
    making: Arc<tokio::sync::RwLock<()>>,
    /// Instructions composed, by folder and profile: once a run.
    told: Mutex<HashMap<(PathBuf, Option<String>), Told>>,
    journal: Mutex<Option<std::fs::File>>,
}

/// What a worker in a folder is told, and its profile's model and tools.
#[derive(Clone)]
struct Told {
    instructions: String,
    model: Option<String>,
    tools: Option<Vec<String>>,
}

/// The runs going on in this host, by id.
static RUNS: LazyLock<Mutex<HashMap<String, Arc<Run>>>> = LazyLock::new(Mutex::default);

fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_millis() as u64)
}

fn str_of(value: &Value) -> String {
    value.as_str().unwrap_or_default().to_owned()
}

/// Whether `error` is the connection's, not the request's: a request that
/// could not be written (`io`) is lost as one whose answer never came.
fn lost(error: &agent_client::Error) -> bool {
    matches!(
        error.code.as_str(),
        "daemon_disconnected" | "daemon_unavailable" | "daemon_connect_timeout" | "io"
    )
}

/// A `workflow` call: a run started, or stopped.
pub async fn call(
    client: &Arc<Client>,
    call: &Value,
    store: &Path,
    socket: &Path,
) -> Result<Value, (&'static str, String)> {
    let refused = |detail: String| ("workflow_refused", detail);
    let arguments = &call["arguments"];
    let bot = str_of(&call["bot"]);
    let record = client
        .request("inspect", json!({"bot": bot}))
        .await
        .map_err(|e| ("caller_unknown", e.to_string()))?;
    if crate::side_copy(&record).is_some() {
        return Err(refused(
            "a side chat runs no workflows; ask in its agent's own conversation".into(),
        ));
    }
    if let Some(id) = arguments["stop"].as_str() {
        return stop(client, &bot, id)
            .await
            .map(Value::String)
            .map_err(refused);
    }
    if !on() {
        return Err(refused(
            "workflows are off: [sonny.workflows] enabled = false in ~/.agent/config.toml".into(),
        ));
    }
    if is_worker(&bot) {
        return Err(refused(
            "a workflow's agent starts no workflows; return what you found".into(),
        ));
    }
    if !crate::approvals::may_run(&record, NAME) {
        return Err((
            "escalation_refused",
            format!(
                "{NAME} acts outside your sandbox, and no one reviews calls for you; do without it"
            ),
        ));
    }
    // A worker is one generation below the caller: refused as a start
    // would be, before anything is made.
    let depth = record["depth"].as_u64().unwrap_or(0);
    if let Some(most) = client.max_depth().filter(|most| depth >= *most) {
        return Err(refused(format!(
            "a workflow's agents would be {} generations below the agent that starts your line, \
             and the daemon's max_depth is {most}: do this work yourself",
            depth + 1
        )));
    }
    let config = agent_client::config::load()
        .map_err(refused)?
        .sonny
        .workflows;
    // A place among the runs is held from here, so calls at once never
    // start more than max_runs between them.
    let _seat = {
        let runs = RUNS.lock().unwrap();
        let going = runs.len() as u64 + STARTING.load(Ordering::SeqCst);
        if going >= config.max_runs {
            return Err(refused(format!(
                "{going} workflows are running, the most at once ([sonny.workflows] \
                 max_runs); wait for one to end"
            )));
        }
        STARTING.fetch_add(1, Ordering::SeqCst);
        Seat
    };
    // As the calling turn runs: its folder, model and effort, which a
    // choice for that turn alone may make other than the bot's own.
    let turn = call["turn"].as_i64().unwrap_or(0);
    let running = crate::served::running_turn(client, &bot, turn)
        .await
        .map_err(|e| ("caller_unknown", e))?;
    let (workspace, model, effort) = crate::served::as_running(&running, &record);
    let workspace = PathBuf::from(workspace);
    let (project, roles) = line_of(client, &record).await;
    let source = source_of(arguments, &workspace, &roles).map_err(refused)?;
    let script = script::read(&source).map_err(refused)?;
    let budget = match &arguments["budget_tokens"] {
        Value::Null => None,
        value => Some(
            value
                .as_u64()
                .filter(|n| *n > 0)
                .ok_or_else(|| refused("budget_tokens is a whole number, 1 or more".into()))?,
        ),
    };
    let dir_of = runs(store);
    let resumed_from = match arguments["resume_from_run_id"].as_str() {
        Some(id) => {
            let earlier = read_plan(&dir_of.join(valid_id(id).map_err(refused)?))
                .map_err(|e| refused(format!("no run {id} to resume: {e}")))?;
            if earlier.caller != bot && earlier.project != project {
                return Err(refused(format!(
                    "run {id} is {}'s, of another project",
                    earlier.caller
                )));
            }
            Some(earlier.id)
        }
        None => None,
    };
    let id = fresh_id(&dir_of);
    let plan = Plan {
        id: id.clone(),
        caller: bot.clone(),
        caller_id: record["bot_id"].as_i64().unwrap_or(0),
        turn,
        project,
        roles,
        workspace,
        model,
        effort,
        mode: crate::approvals::mode(&record)
            .unwrap_or_else(crate::approvals::default)
            .to_owned(),
        state: crate::state::of(&record, &bot),
        args: arguments.get("args").cloned().unwrap_or(Value::Null),
        budget,
        meta: script.meta.clone(),
        resumed_from,
        started_ms: now_ms(),
        max_agents: config.max_agents,
        at_once: config.at_once(),
    };
    let dir = dir_of.join(&id);
    let made = tokio::task::spawn_blocking({
        let (dir, plan, source, from) = (
            dir.clone(),
            plan.clone(),
            source.clone(),
            plan.resumed_from.clone().map(|from| dir_of.join(from)),
        );
        move || make(&dir, &plan, &source, from.as_deref())
    })
    .await
    .map_err(|e| refused(e.to_string()))?;
    made.map_err(|e| refused(format!("{}: {e}", dir.display())))?;
    let run = load(&dir, store, socket).map_err(refused)?;
    let name = run.script.name().to_owned();
    start(run);
    let reused = match &plan.resumed_from {
        Some(from) => {
            format!(" Calls unchanged since run {from} are answered from it, without a model call.")
        }
        None => String::new(),
    };
    Ok(Value::String(format!(
        "Started workflow {name} as run {id}.{reused} You hear back once, when it ends, in a \
         message from the workflow; carry on meanwhile. Its script is saved at {}: to change a \
         step and run it again, edit that file and call workflow with script_path and \
         resume_from_run_id \"{id}\". To end it early, call workflow with stop \"{id}\".",
        dir.join("script.js").display()
    )))
}

/// The project `record`'s line is in, and its lead's folder: the lead's
/// own for a lead, else that of the nearest lead that made it; with no
/// lead, its name's first part and its own folder.
async fn line_of(client: &Client, record: &Value) -> (String, PathBuf) {
    let own = PathBuf::from(str_of(&record["workspace"]));
    let mut at = record.clone();
    for _ in 0..256 {
        let name = str_of(&at["name"]);
        if let Some(project) = crate::project::lead_project(&name) {
            let folder = at["workspace"].as_str().map(PathBuf::from).unwrap_or(own);
            return (project.to_owned(), folder);
        }
        let Some(by) = at["created_by"].as_str() else {
            break;
        };
        match client.request("inspect", json!({"bot": by})).await {
            Ok(maker)
                if at["created_by_id"].is_null() || maker["bot_id"] == at["created_by_id"] =>
            {
                at = maker
            }
            _ => break,
        }
    }
    let name = str_of(&record["name"]);
    let project = name.split_once('.').map_or(name.as_str(), |(p, _)| p);
    (project.to_owned(), own)
}

/// The script a call names: given inline, by its file, or saved by name.
fn source_of(arguments: &Value, workspace: &Path, roles: &Path) -> Result<String, String> {
    let given = ["script", "script_path", "name"]
        .iter()
        .filter(|key| arguments[**key].is_string())
        .count();
    if given != 1 {
        return Err("give one of script, script_path or name".into());
    }
    if let Some(script) = arguments["script"].as_str() {
        return Ok(script.to_owned());
    }
    let reference = match arguments["script_path"].as_str() {
        Some(path) => json!({"scriptPath": path}),
        None => arguments["name"].clone(),
    };
    resolve(&reference, workspace, roles).and_then(|path| read_file(&path))
}

/// The file a workflow reference names: a saved workflow's name, found in
/// the project's `.agents/workflows`, the caller's folder's, then the
/// person's; or `{scriptPath}` (`{script_path}` too), from the caller's
/// folder.
fn resolve(reference: &Value, workspace: &Path, roles: &Path) -> Result<PathBuf, String> {
    if let Some(path) = (reference["scriptPath"].as_str()).or(reference["script_path"].as_str()) {
        let path = Path::new(path);
        return Ok(match path.is_absolute() {
            true => path.to_owned(),
            false => workspace.join(path),
        });
    }
    let Some(name) = reference.as_str() else {
        return Err("workflow() takes a saved workflow's name or {scriptPath}".into());
    };
    if !agent_client::config::named(name) {
        return Err(format!(
            "{name:?} is no workflow's name: 1 to 64 letters, digits, _ or -"
        ));
    }
    let home = std::env::var_os("HOME").map(PathBuf::from);
    let places: Vec<PathBuf> = [Some(roles), Some(workspace), home.as_deref()]
        .into_iter()
        .flatten()
        .map(|dir| dir.join(".agents/workflows").join(format!("{name}.js")))
        .collect();
    places
        .iter()
        .find(|path| path.is_file())
        .cloned()
        .ok_or_else(|| {
            format!(
                "no saved workflow {name}: looked for {}",
                places
                    .iter()
                    .map(|p| p.display().to_string())
                    .collect::<Vec<_>>()
                    .join(", ")
            )
        })
}

fn read_file(path: &Path) -> Result<String, String> {
    let size = std::fs::metadata(path)
        .map_err(|e| format!("{}: {e}", path.display()))?
        .len();
    if size > script::SCRIPT_BYTES {
        return Err(format!(
            "{} is {size} bytes, past the {} a script may be",
            path.display(),
            script::SCRIPT_BYTES
        ));
    }
    std::fs::read_to_string(path).map_err(|e| format!("{}: {e}", path.display()))
}

/// `id` when it can name a run's folder.
fn valid_id(id: &str) -> Result<&str, String> {
    match agent_client::config::named(id) {
        true => Ok(id),
        false => Err(format!("{id:?} is no run's id")),
    }
}

/// A run id no run in `dir` has: eight letters and digits.
fn fresh_id(dir: &Path) -> String {
    use sha2::Digest;
    static MADE: AtomicU64 = AtomicU64::new(0);
    loop {
        let seed = format!(
            "{}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map_or(0, |d| d.as_nanos()),
            MADE.fetch_add(1, Ordering::Relaxed)
        );
        let hash = sha2::Sha256::digest(seed.as_bytes());
        let id: String = (hash.iter().take(5))
            .flat_map(|byte| [byte >> 4, byte & 15])
            .take(8)
            .map(|n| char::from_digit(u32::from(n), 16).unwrap_or('0'))
            .collect();
        if !dir.join(&id).exists() {
            return id;
        }
    }
}

/// A new run's folder: its plan, its script, and a journal holding the
/// answers of the run it resumes, `from`.
fn make(dir: &Path, plan: &Plan, source: &str, from: Option<&Path>) -> std::io::Result<()> {
    std::fs::create_dir_all(dir)?;
    std::fs::write(dir.join("script.js"), source)?;
    let mut journal = String::new();
    if let Some(from) = from {
        for (key, (value, ..)) in answers_of(from) {
            // A worker that failed then runs again.
            if !value.is_null() {
                journal.push_str(
                    &json!({"t": "agent", "key": key, "value": value, "earlier": true}).to_string(),
                );
                journal.push('\n');
            }
        }
    }
    std::fs::write(dir.join("events.jsonl"), journal)?;
    // The plan last: a folder without one is no run, for a host that starts
    // while this one is being made.
    let text = serde_json::to_vec_pretty(plan).map_err(std::io::Error::other)?;
    std::fs::write(dir.join("run.json"), text)
}

fn read_plan(dir: &Path) -> Result<Plan, String> {
    let text = std::fs::read(dir.join("run.json")).map_err(|e| e.to_string())?;
    serde_json::from_slice(&text).map_err(|e| e.to_string())
}

/// The answers the journal in `dir` records, by call key, each with
/// whether it came from an earlier run, and the output tokens this run's
/// own agents spent.
fn answers_of(dir: &Path) -> HashMap<String, Answer> {
    journal_of(dir).answers
}

/// A call's recorded value, whether an earlier run gave it, and the
/// output tokens it cost this run.
type Answer = (Value, bool, u64);

/// What the journal of a run records.
#[derive(Default)]
struct Journal {
    answers: HashMap<String, Answer>,
    /// Output tokens the run's own agents spent.
    spent: u64,
    /// Whether its last line was cut short, so the next line written
    /// starts on a line of its own.
    cut: bool,
    /// Whether the run was told to stop.
    stopped: bool,
    /// The workers it submitted to.
    submitted: HashSet<String>,
}

fn journal_of(dir: &Path) -> Journal {
    let mut journal = Journal::default();
    // Each line is read on its own, from bytes: a line a crash cut short,
    // even inside a character, costs only that line.
    let bytes = std::fs::read(dir.join("events.jsonl")).unwrap_or_default();
    journal.cut = bytes.last().is_some_and(|last| *last != b'\n');
    for line in bytes.split(|byte| *byte == b'\n') {
        let Ok(mut entry) = serde_json::from_slice::<Value>(line) else {
            continue;
        };
        match entry["t"].as_str() {
            Some("stop") => journal.stopped = true,
            Some("submit") => {
                journal.submitted.insert(str_of(&entry["worker"]));
            }
            _ => {}
        }
        if entry["t"] != "agent" {
            continue;
        }
        let earlier = entry["earlier"] == true;
        let tokens = match earlier {
            true => 0,
            false => entry["output_tokens"].as_u64().unwrap_or(0),
        };
        journal.spent += tokens;
        journal.answers.insert(
            str_of(&entry["key"]),
            (entry["value"].take(), earlier, tokens),
        );
    }
    journal
}

/// The run kept in `dir`, read to go on.
fn load(dir: &Path, store: &Path, socket: &Path) -> Result<Arc<Run>, String> {
    let plan = read_plan(dir)?;
    let source = read_file(&dir.join("script.js"))?;
    let script = script::read(&source)?;
    let Journal {
        answers,
        spent,
        cut,
        stopped,
        submitted,
    } = journal_of(dir);
    // Workers it submitted to and has no answer from may still run: known,
    // so a stop or end interrupts them and a replay takes them up again.
    let worker = |key: &String| format!("{}.{WORKER}{}-{key}", plan.project, plan.id);
    let answered: HashSet<String> = answers.keys().map(worker).collect();
    let live: HashMap<String, i64> = (submitted.into_iter())
        .filter(|bot| !answered.contains(bot))
        .map(|bot| (bot, SUBMITTED))
        .collect();
    let mut journal = std::fs::OpenOptions::new()
        .append(true)
        .create(true)
        .open(dir.join("events.jsonl"))
        .ok();
    if cut && let Some(file) = journal.as_mut() {
        let _ = file.write_all(b"\n");
    }
    Ok(Arc::new(Run {
        script,
        dir: dir.to_owned(),
        store: store.to_owned(),
        socket: socket.to_owned(),
        // A stop the run was told of before the host went away still holds.
        stopping: AtomicBool::new(stopped),
        halt: Notify::new(),
        answers: Mutex::new(answers),
        spent: AtomicU64::new(spent),
        live: Mutex::new(live),
        // Its workers' worktrees, which its answers no longer make.
        trees: Mutex::new(crate::task::worker_trees(
            &plan.workspace,
            &format!("{}.{WORKER}{}-", plan.project, plan.id),
        )),
        making: Arc::default(),
        told: Mutex::default(),
        journal: Mutex::new(journal),
        plan,
    }))
}

/// Go on with the runs of the daemon of `store` that had not ended when
/// the host last stopped, and tell each caller whose run ended but was not
/// told: called once, as the host starts.
pub fn resume(store: PathBuf, socket: PathBuf) {
    let dir = runs(&store);
    let Ok(entries) = std::fs::read_dir(&dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        let ended = std::fs::read(path.join("result.json"))
            .ok()
            .and_then(|text| serde_json::from_slice::<Value>(&text).ok())
            .filter(Value::is_object);
        match ended {
            Some(result) => {
                if result["told"] == false
                    && let Ok(plan) = read_plan(&path)
                {
                    deliver(plan, result, path, socket.clone());
                }
            }
            // Unended, or its ending never written whole: run again.
            None => {
                if let Ok(run) = load(&path, &store, &socket) {
                    run.note(json!({"t": "replay", "at_ms": now_ms()}));
                    start(run);
                }
            }
        }
    }
}

/// Calls past the check on `max_runs` whose run has not started yet.
static STARTING: AtomicU64 = AtomicU64::new(0);

/// One of [`STARTING`], given back however its call ends.
struct Seat;

impl Drop for Seat {
    fn drop(&mut self) {
        STARTING.fetch_sub(1, Ordering::SeqCst);
    }
}

/// Run `run` to its end, as one of this host's.
fn start(run: Arc<Run>) {
    RUNS.lock()
        .unwrap()
        .insert(run.plan.id.clone(), Arc::clone(&run));
    tokio::spawn(drive(run));
}

/// `stop`: the run `id` ends as stopped, its workers' turns interrupted.
/// Only its caller stops it, or the lead of its project.
async fn stop(client: &Client, caller: &str, id: &str) -> Result<String, String> {
    let run = RUNS.lock().unwrap().get(id).cloned();
    let Some(run) = run else {
        return Err(format!("no workflow {id} is running"));
    };
    let lead = crate::project::lead_project(caller) == Some(run.plan.project.as_str());
    if run.plan.caller != caller && !lead {
        return Err(format!("run {id} is {}'s to stop", run.plan.caller));
    }
    // Kept before it is acknowledged, so a host that goes away before
    // the run ends still ends it stopped.
    run.note(json!({"t": "stop", "at_ms": now_ms()}));
    run.stopping.store(true, Ordering::SeqCst);
    run.halt.notify_waiters();
    let (stopped, _) = run.interrupt(client).await;
    Ok(format!(
        "Stopping workflow {} (run {id}): {stopped} running agent{} interrupted. You hear back \
         once it has ended, with what finished.",
        run.script.name(),
        if stopped == 1 { "" } else { "s" }
    ))
}

impl Run {
    fn note(&self, line: Value) {
        if let Some(file) = self.journal.lock().unwrap().as_mut() {
            let mut text = line.to_string();
            text.push('\n');
            let _ = file.write_all(text.as_bytes());
        }
    }

    fn stopping(&self) -> bool {
        self.stopping.load(Ordering::SeqCst)
    }

    /// Interrupt every worker whose turn runs: how many there were, and
    /// whether the daemon heard about each one.
    async fn interrupt(&self, client: &Client) -> (usize, bool) {
        let live: Vec<(String, i64)> = (self.live.lock().unwrap().iter())
            .map(|(bot, turn)| (bot.clone(), *turn))
            .collect();
        let mut heard = true;
        for (bot, turn) in &live {
            // A submission whose answer never came: each of its worker's
            // turns not yet ended, running or still queued.
            let turns = match *turn {
                SUBMITTED => match client
                    .request("turns", json!({"bot": bot, "newest": true, "limit": 8}))
                    .await
                {
                    Ok(page) => (page["turns"].as_array().into_iter().flatten())
                        .filter(|turn| !ENDED.contains(&turn["status"].as_str().unwrap_or("")))
                        .filter_map(|turn| turn["turn"].as_i64())
                        .collect(),
                    Err(error) => {
                        heard &= !lost(&error);
                        Vec::new()
                    }
                },
                turn => vec![turn],
            };
            for turn in turns {
                if let Err(error) = client
                    .request("interrupt", json!({"bot": bot, "turn": turn}))
                    .await
                {
                    heard &= !lost(&error);
                }
            }
        }
        (live.len(), heard)
    }

    /// The output tokens of every turn of the workers whose calls were
    /// never answered, which no answer has counted: once interrupted, what
    /// each turn spent. None when the connection is lost on the way.
    async fn unanswered(&self, client: &Client) -> Option<u64> {
        let workers: Vec<String> = self.live.lock().unwrap().keys().cloned().collect();
        let mut spent = 0;
        for bot in workers {
            match client
                .request("turns", json!({"bot": bot, "newest": true, "limit": 16}))
                .await
            {
                Ok(page) => {
                    for turn in page["turns"].as_array().into_iter().flatten() {
                        // Interrupted, a turn may still be counting what it
                        // spent: read once it has ended.
                        let ended = ENDED.contains(&turn["status"].as_str().unwrap_or(""));
                        let tokens = match (ended, turn["handle"].as_str()) {
                            (false, Some(handle)) => match Self::ended(client, handle).await {
                                Ok(turn) => turn["output_tokens"].as_u64(),
                                Err(error) if lost(&error) => return None,
                                Err(_) => turn["output_tokens"].as_u64(),
                            },
                            _ => turn["output_tokens"].as_u64(),
                        };
                        spent += tokens.unwrap_or(0);
                    }
                }
                Err(error) if lost(&error) => return None,
                Err(_) => {}
            }
        }
        Some(spent)
    }

    /// The turn `handle` names, once it has ended.
    async fn ended(client: &Client, handle: &str) -> Result<Value, agent_client::Error> {
        loop {
            let mut waited = client.request("wait", json!({"handles": [handle]})).await?;
            if waited["results"][handle]["pending"] != true {
                return Ok(waited["results"][handle].take());
            }
        }
    }

    /// What a worker in `dir`, as profile `role`, is told: composed once a
    /// run for each.
    async fn told(&self, dir: &Path, role: Option<&str>) -> Result<Told, String> {
        let key = (dir.to_owned(), role.map(str::to_owned));
        if let Some(told) = self.told.lock().unwrap().get(&key) {
            return Ok(told.clone());
        }
        let (dir, roles, role) = (
            dir.to_owned(),
            self.plan.roles.clone(),
            role.map(str::to_owned),
        );
        let told = tokio::task::spawn_blocking(move || compose(&dir, &roles, role.as_deref()))
            .await
            .map_err(|e| e.to_string())??;
        self.told.lock().unwrap().insert(key, told.clone());
        Ok(told)
    }
}

/// What a worker in `dir` is told: the folder's rules and skills, the
/// memory of the project whose lead's folder is `roles`, the profile
/// `role` from there when given, and [`TOLD`].
fn compose(dir: &Path, roles: &Path, role: Option<&str>) -> Result<Told, String> {
    let memory = agent_client::policy::memory_indexes(roles).map_err(|e| e.to_string())?;
    let role = role
        .map(|name| crate::project::profile(roles, name))
        .transpose()?;
    let composed = agent_client::policy::instructions(dir, role.as_ref(), &memory)
        .map_err(|e| format!("{}: {e}", e.code()))?;
    Ok(Told {
        instructions: format!("{}\n\n{TOLD}", composed.text),
        model: role.as_ref().and_then(|role| role.model.clone()),
        tools: role.and_then(|role| role.tools),
    })
}

/// One go at running a run's script, on one connection: a lost connection
/// ends it, and the run is run again on the next.
struct Attempt {
    run: Arc<Run>,
    client: Arc<Client>,
    slots: Semaphore,
    /// Workers a lost connection left running, each holding one of
    /// `slots` until its call reaches it again.
    survivors: Mutex<HashSet<String>>,
    /// The slots held back for survivors, given back as their calls end.
    owed: AtomicUsize,
    /// How many calls of each key came before, this go.
    seen: Mutex<HashMap<String, usize>>,
    calls: AtomicU64,
    nulls: AtomicU64,
    reused: AtomicU64,
    /// The output tokens `budget.spent()` shows.
    shown: AtomicU64,
    lost: AtomicBool,
}

/// A call's place among a run's `at_once`.
enum Slot<'a> {
    Taken(#[allow(dead_code)] tokio::sync::SemaphorePermit<'a>),
    /// A survivor's, given back to the run when its call ends if the run
    /// held one back for it.
    Held(&'a Semaphore, bool),
}

impl Drop for Slot<'_> {
    fn drop(&mut self) {
        if let Slot::Held(slots, true) = self {
            slots.add_permits(1);
        }
    }
}

/// The options an `agent()` call takes, Claude Code's; others are left
/// as it leaves them, though they are part of the call's key.
#[derive(Default)]
struct Opts {
    label: Option<String>,
    phase: Option<String>,
    schema: Option<Value>,
    model: Option<String>,
    effort: Option<String>,
    worktree: bool,
    agent_type: Option<String>,
}

impl Opts {
    fn of(opts: &Value) -> Result<Opts, String> {
        let text = |key: &str| -> Result<Option<String>, String> {
            match &opts[key] {
                Value::Null => Ok(None),
                Value::String(text) if !text.trim().is_empty() => Ok(Some(text.clone())),
                _ => Err(format!("agent()'s {key} is a string")),
            }
        };
        let worktree = match &opts["isolation"] {
            Value::Null => false,
            value if value == "worktree" => true,
            _ => return Err("agent()'s isolation is 'worktree' or left out".into()),
        };
        let schema = match &opts["schema"] {
            Value::Null => None,
            schema => {
                schema::check(schema)?;
                Some(schema.clone())
            }
        };
        Ok(Opts {
            label: text("label")?,
            phase: text("phase")?,
            schema,
            model: text("model")?,
            effort: text("effort")?,
            worktree,
            agent_type: text("agentType")?,
        })
    }
}

/// The key of an `agent()` call: a hash of its prompt and its options but
/// the label and phase, which only say where it shows.
fn key_of(prompt: &str, opts: &Value) -> String {
    use sha2::Digest;
    let mut kept = opts.clone();
    if let Value::Object(fields) = &mut kept {
        fields.remove("label");
        fields.remove("phase");
    }
    let mut hash = sha2::Sha256::new();
    hash.update(prompt.as_bytes());
    hash.update([0]);
    hash.update(canonical(&kept).as_bytes());
    hash.finalize()
        .iter()
        .take(6)
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

/// `value` as JSON with each object's keys in order.
fn canonical(value: &Value) -> String {
    match value {
        Value::Object(fields) => {
            let mut keys: Vec<&String> = fields.keys().collect();
            keys.sort();
            let inner: Vec<String> = keys
                .into_iter()
                .map(|key| format!("{}:{}", json!(key), canonical(&fields[key])))
                .collect();
            format!("{{{}}}", inner.join(","))
        }
        Value::Array(items) => {
            format!(
                "[{}]",
                items.iter().map(canonical).collect::<Vec<_>>().join(",")
            )
        }
        other => other.to_string(),
    }
}

/// Why a worker gave no value.
enum Trouble {
    /// The connection went: the run is picked up on the next.
    Lost,
    /// What `agent()` throws.
    Thrown(String),
}

impl script::Host for Attempt {
    async fn agent(self: Arc<Self>, prompt: String, opts: Value) -> Result<Value, String> {
        let run = Arc::clone(&self.run);
        let asked = Opts::of(&opts)?;
        let key = key_of(&prompt, &opts);
        let key = {
            let mut seen = self.seen.lock().unwrap();
            let before = seen.entry(key.clone()).or_default();
            *before += 1;
            match *before {
                1 => key,
                n => format!("{key}-{}", n - 1),
            }
        };
        let calls = self.calls.fetch_add(1, Ordering::SeqCst) + 1;
        if calls > run.plan.max_agents {
            return Err(format!(
                "this run has started {} agents, the most one run starts ([sonny.workflows] \
                 max_agents)",
                run.plan.max_agents
            ));
        }
        let answered = run.answers.lock().unwrap().get(&key).cloned();
        if let Some((value, earlier, tokens)) = answered {
            // What the script sees spent grows as its calls are answered
            // again, as it did the first time.
            self.shown.fetch_add(tokens, Ordering::SeqCst);
            if earlier {
                self.reused.fetch_add(1, Ordering::SeqCst);
            }
            if value.is_null() {
                self.nulls.fetch_add(1, Ordering::SeqCst);
            }
            return Ok(value);
        }
        let worker = format!("{}.{WORKER}{}-{key}", run.plan.project, run.plan.id);
        // Checked once a slot is free: calls queued behind it start no
        // worker once the run is stopping or its budget is spent. A worker
        // still running from before has its slot already.
        let survived = self.survivors.lock().unwrap().remove(&worker);
        let _slot = match survived {
            // Never more given back than were held back: at most `at_once`.
            true => Slot::Held(
                &self.slots,
                (self
                    .owed
                    .try_update(Ordering::SeqCst, Ordering::SeqCst, |n| n.checked_sub(1)))
                .is_ok(),
            ),
            false => Slot::Taken(self.slots.acquire().await.map_err(|e| e.to_string())?),
        };
        if run.stopping() {
            return Err("the workflow is stopping".into());
        }
        // A worker the daemon has a turn for is taken up again, spent or
        // not: no new work starts for it. One whose submission never
        // reached the daemon is new work.
        if let Some(total) = run.plan.budget
            && run.spent.load(Ordering::SeqCst) >= total
        {
            let started = match survived {
                false => false,
                true => match (self.client)
                    .request("turns", json!({"bot": worker, "limit": 1}))
                    .await
                {
                    Ok(page) => page["turns"].as_array().is_some_and(|t| !t.is_empty()),
                    Err(error) if lost(&error) => {
                        self.lost.store(true, Ordering::SeqCst);
                        run.halt.notify_waiters();
                        return Err(
                            "the daemon's connection was lost; the run picks up where it was"
                                .into(),
                        );
                    }
                    Err(_) => false,
                },
            };
            if !started {
                return Err(format!(
                    "this run's budget of {total} output tokens is spent"
                ));
            }
        }
        let done = || run.live.lock().unwrap().remove(&worker);
        match self.work(&worker, &key, &prompt, &asked).await {
            Ok((value, turn, tokens, error)) => {
                run.spent.fetch_add(tokens, Ordering::SeqCst);
                self.shown.fetch_add(tokens, Ordering::SeqCst);
                if value.is_null() {
                    self.nulls.fetch_add(1, Ordering::SeqCst);
                }
                run.note(json!({"t": "agent", "key": key, "label": asked.label,
                    "phase": asked.phase, "bot": worker, "turn": turn, "value": value,
                    "output_tokens": tokens, "error": error, "at_ms": now_ms()}));
                run.answers
                    .lock()
                    .unwrap()
                    .insert(key, (value.clone(), false, tokens));
                done();
                Ok(value)
            }
            Err(Trouble::Thrown(error)) => {
                done();
                Err(error)
            }
            Err(Trouble::Lost) => {
                self.lost.store(true, Ordering::SeqCst);
                run.halt.notify_waiters();
                Err("the daemon's connection was lost; the run picks up where it was".into())
            }
        }
    }

    fn phase(&self, title: &str) {
        self.run
            .note(json!({"t": "phase", "title": title, "at_ms": now_ms()}));
    }

    fn log(&self, text: &str) {
        self.run
            .note(json!({"t": "log", "text": text, "at_ms": now_ms()}));
    }

    /// What this go's calls cost so far, the ones answered from the
    /// journal as they cost then: so a replay takes the same branches.
    /// Admission goes by what the run truly spent.
    fn spent(&self) -> u64 {
        self.shown.load(Ordering::SeqCst)
    }

    fn load(&self, reference: &Value) -> Result<script::Script, String> {
        let plan = &self.run.plan;
        child(&self.run.dir, reference, &plan.workspace, &plan.roles)
    }

    fn halted(&self) -> bool {
        self.run.stopping() || self.lost.load(Ordering::SeqCst)
    }

    async fn stopped(&self) {
        loop {
            let woken = self.run.halt.notified();
            tokio::pin!(woken);
            woken.as_mut().enable();
            if script::Host::halted(self) {
                return;
            }
            woken.await;
        }
    }
}

impl Attempt {
    /// Make worker `worker` for the call `key`, ask it, and wait for its
    /// value: with the turn that gave it, the output tokens it spent and,
    /// for a null, why.
    async fn work(
        &self,
        worker: &str,
        key: &str,
        prompt: &str,
        asked: &Opts,
    ) -> Result<(Value, Option<i64>, u64, Option<String>), Trouble> {
        let plan = &self.run.plan;
        let failed = |why: String| Ok((Value::Null, None, 0, Some(why)));
        let confine = crate::approvals::sandbox(&plan.mode, &plan.workspace, None)
            .get("sandbox")
            .and_then(Value::as_str)
            != Some("danger-full-access");
        let dir = match asked.worktree {
            true => {
                // Known before it is made, and made to its end, so a call
                // cancelled meanwhile still leaves it to the run's end.
                self.run.trees.lock().unwrap().insert(worker.to_owned());
                let making = Arc::clone(&self.run.making).read_owned().await;
                let (here, name) = (plan.workspace.clone(), worker.to_owned());
                let made = tokio::task::spawn_blocking(move || {
                    let made = crate::task::worker_tree(&here, &name, confine);
                    drop(making);
                    made
                })
                .await
                .map_err(|e| Trouble::Thrown(e.to_string()))?;
                match made {
                    Ok(dir) => dir,
                    Err(why) => {
                        self.run.trees.lock().unwrap().remove(worker);
                        return failed(format!("no worktree: {why}"));
                    }
                }
            }
            false => plan.workspace.clone(),
        };
        // Told its source folder's rules, as a task in a worktree is, while
        // it works in its own worktree.
        let told = match (self.run)
            .told(&plan.workspace, asked.agent_type.as_deref())
            .await
        {
            Ok(told) => Some(told),
            // A worker this call made before the host restarted needs
            // nothing its profile says now: recover it, as it is.
            Err(why) => match self.client.request("inspect", json!({"bot": worker})).await {
                Ok(made) if made["created_by_id"] == plan.caller_id => None,
                Err(again) if lost(&again) => return Err(Trouble::Lost),
                _ => return Err(Trouble::Thrown(why)),
            },
        };
        let request = format!("wf-{}-{key}", plan.id);
        if let Some(told) = told {
            let model = (asked.model.clone())
                .or(told.model.clone())
                .unwrap_or_else(|| plan.model.clone());
            let effort = asked
                .effort
                .clone()
                .or_else(|| plan.effort.clone().filter(|_| model == plan.model));
            let config = agent_client::config::load().unwrap_or_default();
            let tools = told.tools.clone().unwrap_or_else(|| {
                agent_client::default_tools(
                    self.client.providers(),
                    &model,
                    config.web_search,
                    &config.web_search_off,
                )
                .into_iter()
                .map(str::to_owned)
                .collect()
            });
            // Confined as the caller's row confines it, in its own folder; the
            // rest of its settings, rules and environment, its maker's.
            let mut settings = crate::approvals::sandbox(&plan.mode, &dir, plan.state.as_deref());
            if let Some(ttl) = agent_client::policy::cache_ttl(&model) {
                settings.insert("cache_ttl".into(), json!(ttl));
            }
            if let Some(outputs) = agent_client::policy::outputs_beside(&self.run.store) {
                settings.insert("outputs".into(), json!(outputs));
            }
            if config.hide_senders {
                settings.insert("hide_senders".into(), json!(true));
            }
            if let Some(ms) = config.hold_ms {
                settings.insert("hold_ms".into(), json!(ms));
            }
            let create = json!({"bot": worker, "workspace": dir, "model": model,
                "instructions": told.instructions, "effort": effort, "tools": tools,
                "declare": crate::declared::tools(crate::served::Role::WORKER),
                "created_by": plan.caller, "created_by_id": plan.caller_id,
                "compaction_instructions": agent_client::policy::DEFAULT_COMPACTION_INSTRUCTIONS,
                "fallback": config.fallbacks.get(&model), "settings": settings,
                "request_id": request});
            if let Err(error) = self.client.request("create", create).await {
                if lost(&error) {
                    return Err(Trouble::Lost);
                }
                // Made by this call before, told what its folder said then.
                match self.client.request("inspect", json!({"bot": worker})).await {
                    Ok(made) if made["created_by_id"] == plan.caller_id => {}
                    Err(again) if lost(&again) => return Err(Trouble::Lost),
                    _ => return failed(format!("{worker} was not made: {error}")),
                }
            }
        }
        let schema = asked.schema.as_ref().map(schema::strict);
        let (value, turn, tokens, error) =
            self.ask(worker, &request, prompt, schema.as_ref()).await?;
        let Some(wanted) = &asked.schema else {
            return Ok((value, turn, tokens, error));
        };
        // A turn that did not complete is the call's end, null with why:
        // only a completed reply that misses the schema is asked again.
        if turn.is_none() || error.is_some() {
            return Ok((Value::Null, turn, tokens, error));
        }
        let text = value.as_str().unwrap_or_default();
        match schema::parse(text, wanted) {
            Ok(object) => Ok((object, turn, tokens, None)),
            Err(why) => {
                // Once more, on the same worker, as Claude Code's
                // StructuredOutput answers a mismatch.
                let again = format!(
                    "Your reply was not an object that follows the schema ({why}). Reply again \
                     with only that JSON object."
                );
                let (value, turn, more, error) = self
                    .ask(worker, &format!("{request}-again"), &again, schema.as_ref())
                    .await?;
                let text = value.as_str().unwrap_or_default();
                Ok(match schema::parse(text, wanted) {
                    Ok(object) => (object, turn, tokens + more, None),
                    Err(why) => (
                        Value::Null,
                        turn,
                        tokens + more,
                        error.or(Some(format!("its reply did not follow the schema: {why}"))),
                    ),
                })
            }
        }
    }

    /// Submit `prompt` to `worker` and wait for its turn: its text, the
    /// turn, its output tokens and, for a turn that did not complete, why.
    async fn ask(
        &self,
        worker: &str,
        request: &str,
        prompt: &str,
        schema: Option<&Value>,
    ) -> Result<(Value, Option<i64>, u64, Option<String>), Trouble> {
        let plan = &self.run.plan;
        let submit = json!({"bot": worker, "request_id": request, "prompt": prompt,
            "delivery": "queue", "from": {"bot": plan.caller, "turn": plan.turn},
            "output_schema": schema});
        // Known before it is sent: a call cancelled while the daemon takes
        // it, or whose connection is lost, still leaves its turn to stop.
        let live = |turn: Option<i64>| {
            let mut live = self.run.live.lock().unwrap();
            match turn {
                Some(turn) => live.insert(worker.to_owned(), turn),
                None => live.remove(worker),
            }
        };
        live(Some(SUBMITTED));
        self.run
            .note(json!({"t": "submit", "worker": worker, "at_ms": now_ms()}));
        let submitted = match self.client.request("submit", submit).await {
            Ok(submitted) => submitted,
            Err(error) if lost(&error) => return Err(Trouble::Lost),
            Err(error) => {
                live(None);
                return match error.code == "invalid_output_schema" {
                    true => Err(Trouble::Thrown(error.to_string())),
                    false => Ok((Value::Null, None, 0, Some(error.to_string()))),
                };
            }
        };
        let Some(turn) = submitted["turn"].as_i64() else {
            live(None);
            return Ok((Value::Null, None, 0, Some("no turn started".into())));
        };
        live(Some(turn));
        let handle = format!("turn:{worker}/{turn}");
        let waited = loop {
            match self
                .client
                .request("wait", json!({"handles": [handle]}))
                .await
            {
                Ok(mut waited) if waited["results"][&handle]["pending"] != true => {
                    break Ok(waited["results"][&handle].take());
                }
                Ok(_) => continue,
                Err(error) => break Err(error),
            }
        };
        // The worker stays known until its call's answer is recorded: a
        // turn whose wait the connection lost may still run, and one whose
        // whole reply is still to be read is taken up again on replay.
        let outcome = waited.map_err(|e| match lost(&e) {
            true => Trouble::Lost,
            false => Trouble::Thrown(e.to_string()),
        })?;
        let tokens = outcome["output_tokens"].as_u64().unwrap_or(0);
        if outcome["status"] != "completed" {
            let why = match outcome["detail"].as_str().or(outcome["error"].as_str()) {
                Some(detail) => format!("its turn {}: {detail}", str_of(&outcome["status"])),
                None => format!("its turn {}", str_of(&outcome["status"])),
            };
            return Ok((Value::Null, Some(turn), tokens, Some(why)));
        }
        // A whole reply that cannot be read leaves the call null, with
        // what its turn spent counted.
        let text = match outcome["text_truncated"] == true {
            true => match self.reply(worker, turn).await {
                Ok(text) => text,
                Err(Trouble::Thrown(why)) => {
                    return Ok((Value::Null, Some(turn), tokens, Some(why)));
                }
                Err(Trouble::Lost) => return Err(Trouble::Lost),
            },
            false => str_of(&outcome["text"]),
        };
        Ok((Value::String(text), Some(turn), tokens, None))
    }

    /// The whole of `worker`'s last reply in `turn`, which a wait's
    /// outcome cuts: never the cut one, which would be kept as its value,
    /// nor one from a later turn.
    async fn reply(&self, worker: &str, turn: i64) -> Result<String, Trouble> {
        let trouble = |e: agent_client::Error| match lost(&e) {
            true => Trouble::Lost,
            false => Trouble::Thrown(format!("its whole reply could not be read: {e}")),
        };
        let page = self
            .client
            .request("history_nodes", json!({"bot": worker, "limit": 400}))
            .await
            .map_err(trouble)?;
        let nodes: Vec<i64> = (page["nodes"].as_array().into_iter().flatten())
            .filter(|node| node["turn"] == turn)
            .filter_map(|node| node["node"].as_i64())
            .take(16)
            .collect();
        let read = self
            .client
            .request("history_items", json!({"bot": worker, "nodes": nodes}))
            .await
            .map_err(trouble)?;
        // Newest first: an item that could not be read before the reply
        // is found may be the reply, so none older stands in for it.
        for entry in read["items"].as_array().into_iter().flatten() {
            if let Some(error) = entry["error"].as_str() {
                return Err(Trouble::Thrown(format!(
                    "its whole reply could not be read: {error}"
                )));
            }
            let found = (crate::responses(&entry["item"]).into_iter())
                .filter(|item| item["role"] == "assistant")
                .map(|item| crate::text(&item))
                .find(|text| !text.is_empty());
            if let Some(text) = found {
                return Ok(text);
            }
        }
        Err(Trouble::Thrown("its whole reply could not be found".into()))
    }
}

/// Run `run`'s script to its end, again on a new connection whenever one
/// is lost, then record and tell how it ended.
async fn drive(run: Arc<Run>) {
    let (ended, attempt) = loop {
        let connected = tokio::select! {
            connected = Client::connect(&run.socket) => connected.ok(),
            () = halted(&run) => None,
        };
        let Some((client, mut events)) = connected else {
            if run.stopping() {
                break (script::Ended::Halted, None);
            }
            tokio::time::sleep(AGAIN).await;
            continue;
        };
        // Read, so the connection is never closed for a full queue.
        tokio::spawn(async move { while events.recv().await.is_some() {} });
        let survivors: HashSet<String> = run.live.lock().unwrap().keys().cloned().collect();
        let attempt = Arc::new(Attempt {
            run: Arc::clone(&run),
            client: client.clone(),
            slots: Semaphore::new(run.plan.at_once.saturating_sub(survivors.len())),
            owed: AtomicUsize::new(survivors.len().min(run.plan.at_once)),
            survivors: Mutex::new(survivors),
            seen: Mutex::default(),
            calls: AtomicU64::new(0),
            nulls: AtomicU64::new(0),
            reused: AtomicU64::new(0),
            shown: AtomicU64::new(0),
            lost: AtomicBool::new(false),
        });
        let ended = script::run(
            Arc::clone(&attempt),
            &run.script,
            run.plan.args.clone(),
            run.plan.budget,
        )
        .await;
        if attempt.lost.load(Ordering::SeqCst) && !run.stopping() {
            client.close().await;
            run.note(json!({"t": "replay", "at_ms": now_ms()}));
            tokio::time::sleep(AGAIN).await;
            continue;
        }
        break (ended, Some(attempt));
    };
    finish(&run, ended, attempt.as_deref()).await;
    RUNS.lock().unwrap().remove(&run.plan.id);
}

/// Resolves once `run` is to stop.
async fn halted(run: &Run) {
    loop {
        let woken = run.halt.notified();
        tokio::pin!(woken);
        woken.as_mut().enable();
        if run.stopping() {
            return;
        }
        woken.await;
    }
}

/// Record how `run` ended, take away the worktrees it left clean, and tell
/// its caller.
async fn finish(run: &Run, ended: script::Ended, attempt: Option<&Attempt>) {
    let plan = &run.plan;
    // Every worktree being made is made, and known, before any is taken.
    drop(run.making.write().await);
    // What still runs is the run's no longer: a call it did not await,
    // or one whose wait a lost connection left. The run is not recorded
    // ended until the daemon has heard every interrupt, on a new
    // connection if it must; a host gone meanwhile finds them again in the
    // journal.
    let mut heard = match attempt {
        Some(attempt) => run.interrupt(&attempt.client).await.1,
        None => run.live.lock().unwrap().is_empty(),
    };
    let mut wait = AGAIN;
    let mut spoke = None;
    while !heard {
        if let Ok((client, _events)) = Client::connect(&run.socket).await {
            heard = run.interrupt(&client).await.1;
            spoke = Some(client);
            if heard {
                break;
            }
        }
        tokio::time::sleep(wait).await;
        wait = (wait * 2).min(Duration::from_secs(60));
    }
    // What the calls never answered spent all the same, read on the
    // connection that last worked, or a new one, until it is read.
    let mut client = spoke.or_else(|| attempt.map(|a| Arc::clone(&a.client)));
    let mut wait = AGAIN;
    while !run.live.lock().unwrap().is_empty() {
        if let Some(client) = &client
            && let Some(unanswered) = run.unanswered(client).await
        {
            run.spent.fetch_add(unanswered, Ordering::SeqCst);
            break;
        }
        tokio::time::sleep(wait).await;
        wait = (wait * 2).min(Duration::from_secs(60));
        client = Client::connect(&run.socket)
            .await
            .ok()
            .map(|(client, _)| client);
    }
    let (status, result, error) = match ended {
        script::Ended::Returned(value) => ("completed", value, None),
        script::Ended::Threw(error) => ("failed", Value::Null, Some(error)),
        script::Ended::Halted => ("stopped", Value::Null, None),
    };
    let count =
        |of: fn(&Attempt) -> &AtomicU64| attempt.map_or(0, |a| of(a).load(Ordering::SeqCst));
    let trees: Vec<String> = run.trees.lock().unwrap().iter().cloned().collect();
    let (store, socket, here) = (
        run.store.clone(),
        run.socket.clone(),
        plan.workspace.clone(),
    );
    let kept = tokio::task::spawn_blocking(move || {
        // What stays: a branch with work on it, or a worktree not taken
        // away, and why.
        (trees.iter())
            .filter_map(|tree| {
                match crate::task::remove(&here, tree, false, (&store, &socket, "")) {
                    Ok(said) => said.contains("is kept").then_some(said),
                    Err(why) => Some(match crate::task::worktree_of(&here, tree) {
                        Some(path) => format!("{why} (at {})", path.display()),
                        None => why,
                    }),
                }
            })
            .collect::<Vec<_>>()
    })
    .await
    .unwrap_or_default();
    let ended = json!({"status": status, "result": result, "error": error,
        "agents": count(|a| &a.calls), "nulls": count(|a| &a.nulls),
        "reused": count(|a| &a.reused), "output_tokens": run.spent.load(Ordering::SeqCst),
        "kept": kept, "ended_ms": now_ms(), "told": false});
    run.note(json!({"t": "end", "status": status, "error": error, "at_ms": now_ms()}));
    let _ = keep_result(&run.dir, &ended);
    deliver(plan.clone(), ended, run.dir.clone(), run.socket.clone());
}

/// Tell `plan`'s caller how its run ended, trying until the daemon takes
/// it, then mark it told.
fn deliver(plan: Plan, mut ended: Value, dir: PathBuf, socket: PathBuf) {
    tokio::spawn(async move {
        tell(&plan, &ended, &dir, &socket).await;
        ended["told"] = json!(true);
        let _ = keep_result(&dir, &ended);
    });
}

/// The workflow `reference` names, as the run in `dir` first read it: its
/// source is kept in the run's folder, so a replay runs the same steps
/// whatever has happened to the file since.
fn child(
    dir: &Path,
    reference: &Value,
    workspace: &Path,
    roles: &Path,
) -> Result<script::Script, String> {
    let kept = (dir.join("children")).join(format!(
        "{}.js",
        key_of(&canonical(reference), &Value::Null)
    ));
    if let Ok(source) = std::fs::read_to_string(&kept) {
        return script::read(&source);
    }
    let source = read_file(&resolve(reference, workspace, roles)?)?;
    let script = script::read(&source)?;
    keep(&kept, &source).map_err(|e| format!("{}: {e}", kept.display()))?;
    Ok(script)
}

/// Write how the run in `dir` ended, whole or not at all: a host stopped
/// mid-write leaves the run unended, to be run again from its journal.
fn keep_result(dir: &Path, ended: &Value) -> std::io::Result<()> {
    keep(&dir.join("result.json"), &ended.to_string())
}

/// Write `text` to `path` whole or not at all.
fn keep(path: &Path, text: &str) -> std::io::Result<()> {
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let mut part = path.as_os_str().to_owned();
    part.push(".part");
    std::fs::write(&part, text)?;
    std::fs::rename(&part, path)
}

/// Tell `plan`'s caller how its run ended, once, as a message from the
/// workflow queued for its next turn, waiting out a daemon that is away.
async fn tell(plan: &Plan, ended: &Value, dir: &Path, socket: &Path) {
    let message = told(plan, ended, dir);
    let mut wait = AGAIN;
    loop {
        let Ok((client, _events)) = Client::connect(socket).await else {
            tokio::time::sleep(wait).await;
            wait = (wait * 2).min(Duration::from_secs(60));
            continue;
        };
        let sent = client
            .request(
                "submit",
                json!({"bot": plan.caller, "bot_id": plan.caller_id,
                    "request_id": format!("wf-end-{}", plan.id), "prompt": message,
                    "delivery": "queue", "origin": "workflow"}),
            )
            .await;
        client.close().await;
        match sent {
            Err(error) if lost(&error) => {
                tokio::time::sleep(wait).await;
                wait = (wait * 2).min(Duration::from_secs(60));
            }
            // Told, or the caller is gone: no one to tell again.
            _ => return,
        }
    }
}

/// The message a run's caller is sent at its end.
fn told(plan: &Plan, ended: &Value, dir: &Path) -> String {
    let name = plan.meta["name"].as_str().unwrap_or("workflow");
    let took = ended["ended_ms"]
        .as_u64()
        .unwrap_or(plan.started_ms)
        .saturating_sub(plan.started_ms)
        / 1000;
    let took = match took {
        0..60 => format!("{took}s"),
        60..3600 => format!("{}m {}s", took / 60, took % 60),
        _ => format!("{}h {}m", took / 3600, took % 3600 / 60),
    };
    let (agents, nulls, reused) = (
        ended["agents"].as_u64().unwrap_or(0),
        ended["nulls"].as_u64().unwrap_or(0),
        ended["reused"].as_u64().unwrap_or(0),
    );
    let mut counts = format!("{agents} agent{}", if agents == 1 { "" } else { "s" });
    if nulls > 0 {
        counts.push_str(&format!(", {nulls} null"));
    }
    if reused > 0 {
        counts.push_str(&format!(
            ", {reused} answered from run {}",
            plan.resumed_from.as_deref().unwrap_or("an earlier run")
        ));
    }
    let tokens = ended["output_tokens"].as_u64().unwrap_or(0);
    let id = &plan.id;
    let mut said = match ended["status"].as_str() {
        Some("completed") => format!("Workflow {name} (run {id}) completed in {took}"),
        Some("stopped") => format!("Workflow {name} (run {id}) was stopped after {took}"),
        _ => format!("Workflow {name} (run {id}) failed after {took}"),
    };
    said.push_str(&format!(": {counts}; {tokens} output tokens.\n\n"));
    if let Some(error) = ended["error"].as_str() {
        said.push_str(&format!("Error:\n{}\n\n", cut(error, TOLD_BYTES / 2)));
    }
    if ended["status"] == "completed" {
        let result = match &ended["result"] {
            Value::String(text) => text.clone(),
            other => serde_json::to_string_pretty(other).unwrap_or_default(),
        };
        said.push_str(&format!("Result:\n{}\n\n", cut(&result, TOLD_BYTES)));
    }
    // What stays, within its share; the rest is in result.json.
    let kept: Vec<&Value> = ended["kept"].as_array().into_iter().flatten().collect();
    let mut room = TOLD_BYTES / 2;
    for (shown, each) in kept.iter().enumerate() {
        let line = format!("{}\n", str_of(each));
        if line.len() > room {
            said.push_str(&format!(
                "{} more kept; see \"kept\" in result.json.\n",
                kept.len() - shown
            ));
            break;
        }
        room -= line.len();
        said.push_str(&line);
    }
    said.push_str(&format!(
        "The whole result is in {}, each agent's value in {}; to run it again with some steps \
         changed, edit {} and call workflow with script_path and resume_from_run_id \"{id}\".",
        dir.join("result.json").display(),
        dir.join("events.jsonl").display(),
        dir.join("script.js").display()
    ));
    said
}

/// `text` within `most` bytes: its start and its end, with how much was
/// left out between them.
fn cut(text: &str, most: usize) -> String {
    if text.len() <= most {
        return text.to_owned();
    }
    let floor = |mut at: usize| {
        while !text.is_char_boundary(at) {
            at -= 1;
        }
        at
    };
    let head = floor(most * 3 / 4);
    let tail = floor(text.len() - most / 4);
    format!(
        "{}\n[… {} bytes left out …]\n{}",
        &text[..head],
        tail - head,
        &text[tail..]
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_worker_is_known_by_its_name_in_a_dotted_project_too() {
        assert_eq!(worker_project("shop.wf-r1-ab12"), Some("shop"));
        assert_eq!(worker_project("shop.web.wf-r1-ab12"), Some("shop.web"));
        assert!(!is_worker("shop.web.lead"));
        assert!(!is_worker(".wf-r1-ab12"));
        assert!(!is_worker("shop.task"));
    }

    #[test]
    fn a_worker_is_known_by_its_name() {
        assert!(is_worker("shop.wf-1a2b3c4d-0a1b2c3d4e5f"));
        assert!(!is_worker("shop.fix"));
        assert!(!is_worker("shop.lead"));
        assert!(!is_worker("wf-x"));
        assert!(!is_worker(".wf-x"));
    }

    #[test]
    fn a_call_s_key_is_its_prompt_and_options_but_where_it_shows() {
        let opts = json!({"label": "a", "phase": "Find", "effort": "low"});
        let other = json!({"effort": "low", "label": "b"});
        assert_eq!(key_of("p", &opts), key_of("p", &other));
        assert_ne!(key_of("p", &opts), key_of("q", &opts));
        assert_ne!(key_of("p", &opts), key_of("p", &json!({})));
        // Key order is no change.
        assert_eq!(
            key_of(
                "p",
                &json!({"schema": {"type": "object", "properties": {}}, "model": "m"})
            ),
            key_of(
                "p",
                &json!({"model": "m", "schema": {"properties": {}, "type": "object"}})
            )
        );
        assert_eq!(key_of("p", &opts).len(), 12);
    }

    #[test]
    fn options_are_claude_code_s() {
        assert!(
            Opts::of(&json!({"isolation": "worktree"}))
                .unwrap()
                .worktree
        );
        assert!(Opts::of(&json!({"isolation": "copy"})).is_err());
        assert!(Opts::of(&json!({"schema": {"type": "array"}})).is_err());
        let opts = Opts::of(&json!({"agentType": "reviewer", "model": "openai/gpt"})).unwrap();
        assert_eq!(opts.agent_type.as_deref(), Some("reviewer"));
    }

    #[test]
    fn a_long_result_keeps_its_start_and_end() {
        let text = format!("{}{}", "a".repeat(10_000), "z".repeat(10_000));
        let cut = cut(&text, 1000);
        assert!(cut.starts_with("aaa") && cut.ends_with("zzz"), "{cut}");
        assert!(cut.len() < 1100);
        assert_eq!(super::cut("short", 1000), "short");
    }

    #[test]
    fn a_nested_workflow_replays_as_the_run_first_read_it() {
        let dir = std::env::temp_dir().join(format!("sonny-workflow-child-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let file = dir.join("child.js");
        let first = "export const meta = {name: 'child', description: 'first'}\nreturn 1\n";
        std::fs::write(&file, first).unwrap();
        let reference = json!({"scriptPath": "child.js"});
        let run = dir.join("run");
        let name = |script: script::Script| script.name().to_owned();
        assert_eq!(name(child(&run, &reference, &dir, &dir).unwrap()), "child");
        std::fs::write(
            &file,
            "export const meta = {name: 'edited', description: 'x'}\n",
        )
        .unwrap();
        assert_eq!(name(child(&run, &reference, &dir, &dir).unwrap()), "child");
        std::fs::remove_file(&file).unwrap();
        assert_eq!(name(child(&run, &reference, &dir, &dir).unwrap()), "child");
        // Another run reads the file as it is now.
        assert!(child(&dir.join("other"), &reference, &dir, &dir).is_err());
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn a_run_s_journal_is_its_answers() {
        let dir = std::env::temp_dir().join(format!("sonny-workflow-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let lines = [
            json!({"t": "phase", "title": "Find"}),
            json!({"t": "agent", "key": "k1", "value": "x", "output_tokens": 7}),
            json!({"t": "agent", "key": "k2", "value": null, "output_tokens": 3}),
            json!({"t": "agent", "key": "k3", "value": {"a": 1}, "earlier": true, "output_tokens": 9}),
        ];
        let text: String = lines.iter().map(|l| format!("{l}\n")).collect();
        // A line a crash cut short inside a character.
        let mut bytes = text.into_bytes();
        bytes.extend_from_slice("{\"t\": \"agent\", \"key\": \"k4\", \"value\": \"é".as_bytes());
        bytes.pop();
        std::fs::write(dir.join("events.jsonl"), bytes).unwrap();
        let Journal {
            answers,
            spent,
            cut,
            stopped,
            ..
        } = journal_of(&dir);
        assert!(cut && !stopped);
        assert_eq!(spent, 10);
        assert_eq!(answers["k1"], (json!("x"), false, 7));
        assert_eq!(answers["k2"], (Value::Null, false, 3));
        assert_eq!(answers["k3"], (json!({"a": 1}), true, 0));
        assert!(!answers.contains_key("k4"));
        // A run resumed from it starts with what it answered, not its nulls.
        let next = dir.join("next");
        let plan: Plan = serde_json::from_value(json!({"id": "next", "caller": "shop.lead",
            "caller_id": 1, "turn": 1, "project": "shop", "roles": "/", "workspace": "/",
            "model": "openai/m", "effort": null, "mode": "auto", "state": null, "args": null,
            "budget": null, "meta": {}, "resumed_from": "x", "started_ms": 0,
            "max_agents": 1000, "at_once": 4}))
        .unwrap();
        let source = "export const meta = {name: 'next', description: 'x'}\nreturn 1\n";
        make(&next, &plan, source, Some(&dir)).unwrap();
        let Journal { answers, spent, .. } = journal_of(&next);
        assert_eq!(spent, 0);
        assert_eq!(answers.len(), 2);
        assert!(
            answers
                .values()
                .all(|(value, earlier, tokens)| !value.is_null() && *earlier && *tokens == 0)
        );
        assert_eq!(read_plan(&next).unwrap().id, "next");
        // A stop it was told of holds when it is read again; a run resumed
        // from it starts unstopped.
        let mut file = std::fs::OpenOptions::new()
            .append(true)
            .open(dir.join("events.jsonl"))
            .unwrap();
        file.write_all(b"\n{\"t\": \"stop\"}\n").unwrap();
        assert!(journal_of(&dir).stopped);
        assert!(!journal_of(&next).stopped);
        // Loaded again, a stopped run is stopping, and knows the workers it
        // submitted to that have not answered, to interrupt them.
        let mut file = std::fs::OpenOptions::new()
            .append(true)
            .open(next.join("events.jsonl"))
            .unwrap();
        let more = [
            json!({"t": "submit", "worker": "shop.wf-next-k1"}),
            json!({"t": "submit", "worker": "shop.wf-next-k9"}),
            json!({"t": "stop"}),
        ];
        for line in more {
            file.write_all(format!("{line}\n").as_bytes()).unwrap();
        }
        let run = load(&next, &dir, &dir.join("socket")).unwrap();
        assert!(run.stopping());
        assert_eq!(
            *run.live.lock().unwrap(),
            HashMap::from([("shop.wf-next-k9".to_owned(), SUBMITTED)])
        );
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn the_caller_hears_how_it_ended_with_where_the_rest_is() {
        let plan: Plan = serde_json::from_value(json!({"id": "r1", "caller": "shop.lead",
            "caller_id": 1, "turn": 1, "project": "shop", "roles": "/", "workspace": "/",
            "model": "openai/m", "effort": null, "mode": "auto", "state": null, "args": null,
            "budget": null, "meta": {"name": "audit"}, "resumed_from": "r0", "started_ms": 0,
            "max_agents": 1000, "at_once": 4}))
        .unwrap();
        let ended = json!({"status": "completed", "result": {"bugs": 2}, "agents": 12,
            "nulls": 1, "reused": 4, "output_tokens": 5000, "ended_ms": 125_000, "kept": []});
        let said = told(&plan, &ended, Path::new("/runs/r1"));
        assert!(
            said.starts_with("Workflow audit (run r1) completed in 2m 5s: 12 agents, 1 null, 4 answered from run r0; 5000 output tokens."),
            "{said}"
        );
        assert!(said.contains("\"bugs\": 2"), "{said}");
        assert!(said.contains("/runs/r1/result.json"), "{said}");
        let failed = json!({"status": "failed", "error": "TypeError: x", "agents": 1,
            "ended_ms": 5000});
        let said = told(&plan, &failed, Path::new("/runs/r1"));
        assert!(
            said.contains("failed after 5s") && said.contains("TypeError: x"),
            "{said}"
        );
        assert!(!said.contains("Result:"), "{said}");
        let kept: Vec<String> = (0..1000)
            .map(|n| format!("shop.wf-r1-{n:040}'s worktree has uncommitted changes"))
            .collect();
        let many = json!({"status": "completed", "result": 1, "ended_ms": 0, "kept": kept});
        let said = told(&plan, &many, Path::new("/runs/r1"));
        assert!(said.len() < 2 * TOLD_BYTES, "{}", said.len());
        assert!(
            said.contains(" more kept; see \"kept\" in result.json."),
            "{said}"
        );
    }
}
