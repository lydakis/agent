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
use std::collections::{BTreeSet, HashMap};
use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
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
    bot.split_once('.')
        .is_some_and(|(project, rest)| !project.is_empty() && rest.starts_with(WORKER))
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
    answers: Mutex<HashMap<String, (Value, bool)>>,
    spent: AtomicU64,
    /// Workers whose turn runs, by name, for `stop`.
    live: Mutex<HashMap<String, i64>>,
    /// Workers given a worktree, cleaned up once the run ends.
    trees: Mutex<BTreeSet<String>>,
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

fn lost(error: &agent_client::Error) -> bool {
    matches!(
        error.code.as_str(),
        "daemon_disconnected" | "daemon_unavailable" | "daemon_connect_timeout"
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
    let workspace = PathBuf::from(str_of(&record["workspace"]));
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
        turn: call["turn"].as_i64().unwrap_or(0),
        project,
        roles,
        workspace,
        model: format!(
            "{}/{}",
            str_of(&record["provider"]),
            str_of(&record["model"])
        ),
        effort: record["effort"].as_str().map(str::to_owned),
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
        for (key, (value, _)) in answers_of(from) {
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
fn answers_of(dir: &Path) -> HashMap<String, (Value, bool)> {
    journal_of(dir).0
}

fn journal_of(dir: &Path) -> (HashMap<String, (Value, bool)>, u64) {
    let mut answers = HashMap::new();
    let mut spent = 0;
    let text = std::fs::read_to_string(dir.join("events.jsonl")).unwrap_or_default();
    for line in text.lines() {
        let Ok(mut entry) = serde_json::from_str::<Value>(line) else {
            // A line cut short by a crash is no answer.
            continue;
        };
        if entry["t"] != "agent" {
            continue;
        }
        let earlier = entry["earlier"] == true;
        if !earlier {
            spent += entry["output_tokens"].as_u64().unwrap_or(0);
        }
        answers.insert(str_of(&entry["key"]), (entry["value"].take(), earlier));
    }
    (answers, spent)
}

/// The run kept in `dir`, read to go on.
fn load(dir: &Path, store: &Path, socket: &Path) -> Result<Arc<Run>, String> {
    let plan = read_plan(dir)?;
    let source = read_file(&dir.join("script.js"))?;
    let script = script::read(&source)?;
    let (answers, spent) = journal_of(dir);
    let journal = std::fs::OpenOptions::new()
        .append(true)
        .create(true)
        .open(dir.join("events.jsonl"))
        .ok();
    Ok(Arc::new(Run {
        plan,
        script,
        dir: dir.to_owned(),
        store: store.to_owned(),
        socket: socket.to_owned(),
        stopping: AtomicBool::new(false),
        halt: Notify::new(),
        answers: Mutex::new(answers),
        spent: AtomicU64::new(spent),
        live: Mutex::default(),
        trees: Mutex::default(),
        told: Mutex::default(),
        journal: Mutex::new(journal),
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
                    let socket = socket.clone();
                    tokio::spawn(async move {
                        if tell(&plan, &result, &path, &socket).await {
                            let mut result = result;
                            result["told"] = json!(true);
                            let _ = keep_result(&path, &result);
                        }
                    });
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
    run.stopping.store(true, Ordering::SeqCst);
    run.halt.notify_waiters();
    let stopped = run.interrupt(client).await;
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

    /// Interrupt every worker whose turn runs: how many.
    async fn interrupt(&self, client: &Client) -> usize {
        let live: Vec<(String, i64)> = (self.live.lock().unwrap().iter())
            .map(|(bot, turn)| (bot.clone(), *turn))
            .collect();
        for (bot, turn) in &live {
            let _ = client
                .request("interrupt", json!({"bot": bot, "turn": turn}))
                .await;
        }
        live.len()
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
    /// How many calls of each key came before, this go.
    seen: Mutex<HashMap<String, usize>>,
    calls: AtomicU64,
    nulls: AtomicU64,
    reused: AtomicU64,
    lost: AtomicBool,
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
        if let Some((value, earlier)) = answered {
            if earlier {
                self.reused.fetch_add(1, Ordering::SeqCst);
            }
            if value.is_null() {
                self.nulls.fetch_add(1, Ordering::SeqCst);
            }
            return Ok(value);
        }
        // Checked once a slot is free: calls queued behind it start no
        // worker once the run is stopping or its budget is spent.
        let _slot = self.slots.acquire().await.map_err(|e| e.to_string())?;
        if run.stopping() {
            return Err("the workflow is stopping".into());
        }
        if let Some(total) = run.plan.budget
            && run.spent.load(Ordering::SeqCst) >= total
        {
            return Err(format!(
                "this run's budget of {total} output tokens is spent"
            ));
        }
        let worker = format!("{}.{WORKER}{}-{key}", run.plan.project, run.plan.id);
        match self.work(&worker, &key, &prompt, &asked).await {
            Ok((value, turn, tokens, error)) => {
                run.spent.fetch_add(tokens, Ordering::SeqCst);
                if value.is_null() {
                    self.nulls.fetch_add(1, Ordering::SeqCst);
                }
                run.note(json!({"t": "agent", "key": key, "label": asked.label,
                    "phase": asked.phase, "bot": worker, "turn": turn, "value": value,
                    "output_tokens": tokens, "error": error, "at_ms": now_ms()}));
                run.answers
                    .lock()
                    .unwrap()
                    .insert(key, (value.clone(), false));
                Ok(value)
            }
            Err(Trouble::Thrown(error)) => Err(error),
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

    fn spent(&self) -> u64 {
        self.run.spent.load(Ordering::SeqCst)
    }

    fn load(&self, reference: &Value) -> Result<script::Script, String> {
        let plan = &self.run.plan;
        let path = resolve(reference, &plan.workspace, &plan.roles)?;
        script::read(&read_file(&path)?)
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
                let (here, name) = (plan.workspace.clone(), worker.to_owned());
                let made = tokio::task::spawn_blocking(move || {
                    crate::task::worker_tree(&here, &name, confine)
                })
                .await
                .map_err(|e| Trouble::Thrown(e.to_string()))?;
                match made {
                    Ok(dir) => {
                        self.run.trees.lock().unwrap().insert(worker.to_owned());
                        dir
                    }
                    Err(why) => return failed(format!("no worktree: {why}")),
                }
            }
            false => plan.workspace.clone(),
        };
        let told =
            (self.run.told(&dir, asked.agent_type.as_deref()).await).map_err(Trouble::Thrown)?;
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
        let request = format!("wf-{}-{key}", plan.id);
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
        let schema = asked.schema.as_ref().map(schema::strict);
        let (value, turn, tokens, error) =
            self.ask(worker, &request, prompt, schema.as_ref()).await?;
        let Some(wanted) = &asked.schema else {
            return Ok((value, turn, tokens, error));
        };
        let text = value.as_str().unwrap_or_default();
        match (turn, schema::parse(text, wanted)) {
            (_, Ok(object)) => Ok((object, turn, tokens, None)),
            (None, _) => Ok((Value::Null, turn, tokens, error)),
            (Some(_), Err(why)) => {
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
        let submitted = match self.client.request("submit", submit).await {
            Ok(submitted) => submitted,
            Err(error) if lost(&error) => return Err(Trouble::Lost),
            Err(error) if error.code == "invalid_output_schema" => {
                return Err(Trouble::Thrown(error.to_string()));
            }
            Err(error) => return Ok((Value::Null, None, 0, Some(error.to_string()))),
        };
        let Some(turn) = submitted["turn"].as_i64() else {
            return Ok((Value::Null, None, 0, Some("no turn started".into())));
        };
        self.run
            .live
            .lock()
            .unwrap()
            .insert(worker.to_owned(), turn);
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
        self.run.live.lock().unwrap().remove(worker);
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
        let text = match outcome["text_truncated"] == true {
            true => self.reply(worker).await?,
            false => str_of(&outcome["text"]),
        };
        Ok((Value::String(text), Some(turn), tokens, None))
    }

    /// The whole of `worker`'s last reply, which a wait's outcome cuts:
    /// never the cut one, which would be kept as its value.
    async fn reply(&self, worker: &str) -> Result<String, Trouble> {
        let trouble = |e: agent_client::Error| match lost(&e) {
            true => Trouble::Lost,
            false => Trouble::Thrown(format!("its whole reply could not be read: {e}")),
        };
        let page = self
            .client
            .request("history_nodes", json!({"bot": worker, "limit": 16}))
            .await
            .map_err(trouble)?;
        let nodes: Vec<i64> = (page["nodes"].as_array().into_iter().flatten())
            .filter_map(|node| node["node"].as_i64())
            .collect();
        let read = self
            .client
            .request("history_items", json!({"bot": worker, "nodes": nodes}))
            .await
            .map_err(trouble)?;
        (read["items"].as_array().into_iter().flatten())
            .flat_map(|entry| crate::responses(&entry["item"]))
            .filter(|item| item["role"] == "assistant")
            .map(|item| crate::text(&item))
            .find(|text| !text.is_empty())
            .ok_or_else(|| Trouble::Thrown("its whole reply could not be found".into()))
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
        let attempt = Arc::new(Attempt {
            run: Arc::clone(&run),
            client: client.clone(),
            slots: Semaphore::new(run.plan.at_once),
            seen: Mutex::default(),
            calls: AtomicU64::new(0),
            nulls: AtomicU64::new(0),
            reused: AtomicU64::new(0),
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
    // What still runs is the run's no longer: a call it did not await.
    if let Some(attempt) = attempt {
        run.interrupt(&attempt.client).await;
    }
    let (status, result, error) = match ended {
        script::Ended::Returned(value) => ("completed", value, None),
        script::Ended::Threw(error) => ("failed", Value::Null, Some(error)),
        script::Ended::Halted => ("stopped", Value::Null, None),
    };
    let count =
        |of: fn(&Attempt) -> &AtomicU64| attempt.map_or(0, |a| of(a).load(Ordering::SeqCst));
    let trees: Vec<String> = run.trees.lock().unwrap().iter().cloned().collect();
    let (store, here) = (run.store.clone(), plan.workspace.clone());
    let kept = tokio::task::spawn_blocking(move || {
        trees
            .iter()
            .filter_map(|tree| crate::task::remove(&here, tree, false, (&store, "")).ok())
            .filter(|said| said.contains("is kept"))
            .collect::<Vec<_>>()
    })
    .await
    .unwrap_or_default();
    let mut ended = json!({"status": status, "result": result, "error": error,
        "agents": count(|a| &a.calls), "nulls": count(|a| &a.nulls),
        "reused": count(|a| &a.reused), "output_tokens": run.spent.load(Ordering::SeqCst),
        "kept": kept, "ended_ms": now_ms(), "told": false});
    run.note(json!({"t": "end", "status": status, "error": error, "at_ms": now_ms()}));
    let _ = keep_result(&run.dir, &ended);
    if tell(plan, &ended, &run.dir, &run.socket).await {
        ended["told"] = json!(true);
        let _ = keep_result(&run.dir, &ended);
    }
}

/// Write how the run in `dir` ended, whole or not at all: a host stopped
/// mid-write leaves the run unended, to be run again from its journal.
fn keep_result(dir: &Path, ended: &Value) -> std::io::Result<()> {
    let part = dir.join("result.json.part");
    std::fs::write(&part, ended.to_string())?;
    std::fs::rename(&part, dir.join("result.json"))
}

/// Tell `plan`'s caller how its run ended, once, as a message from the
/// workflow queued for its next turn: whether it was told.
async fn tell(plan: &Plan, ended: &Value, dir: &Path, socket: &Path) -> bool {
    let message = told(plan, ended, dir);
    for _ in 0..30 {
        let Ok((client, _events)) = Client::connect(socket).await else {
            tokio::time::sleep(AGAIN).await;
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
            Ok(_) => return true,
            Err(error) if lost(&error) => tokio::time::sleep(AGAIN).await,
            // The caller is gone: no one to tell, and none to tell again.
            Err(_) => return true,
        }
    }
    false
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
    for kept in ended["kept"].as_array().into_iter().flatten() {
        said.push_str(&format!("{}\n", str_of(kept)));
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
        let mut text: String = lines.iter().map(|l| format!("{l}\n")).collect();
        // A line a crash cut short.
        text.push_str("{\"t\": \"agent\", \"key\": \"k4\"");
        std::fs::write(dir.join("events.jsonl"), text).unwrap();
        let (answers, spent) = journal_of(&dir);
        assert_eq!(spent, 10);
        assert_eq!(answers["k1"], (json!("x"), false));
        assert_eq!(answers["k2"], (Value::Null, false));
        assert_eq!(answers["k3"], (json!({"a": 1}), true));
        assert!(!answers.contains_key("k4"));
        // A run resumed from it starts with what it answered, not its nulls.
        let next = dir.join("next");
        let plan: Plan = serde_json::from_value(json!({"id": "next", "caller": "shop.lead",
            "caller_id": 1, "turn": 1, "project": "shop", "roles": "/", "workspace": "/",
            "model": "openai/m", "effort": null, "mode": "auto", "state": null, "args": null,
            "budget": null, "meta": {}, "resumed_from": "x", "started_ms": 0,
            "max_agents": 1000, "at_once": 4}))
        .unwrap();
        make(&next, &plan, "export const meta = {}", Some(&dir)).unwrap();
        let (answers, spent) = journal_of(&next);
        assert_eq!(spent, 0);
        assert_eq!(answers.len(), 2);
        assert!(
            answers
                .values()
                .all(|(value, earlier)| !value.is_null() && *earlier)
        );
        assert_eq!(read_plan(&next).unwrap().id, "next");
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
    }
}
