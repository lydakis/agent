//! The tools Sonny answers: what an agent asks of Sonny itself, such as a
//! task started in a folder of its own or a pane put where the person
//! looks, as declared tools whose calls wait for Sonny's answer rather than
//! as commands in the agent's shell. The daemon holds each call for its
//! answer, parking the turn after a moment, and gives up on it, typed
//! `tool_unserved`, once its time runs out.
//!
//! The lookout answers them, one per daemon, outliving Sonny's windows:
//! each call that starts, or waits from before the lookout did, is run as
//! `sonny tool NAME` in the calling agent's folder, with the environment
//! its shell would have, so a slow setup script or a failure touches only
//! that call. What it prints is the call's result.
use agent_client::Client;
use serde_json::{Value, json};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::sync::Notify;

/// The tag the daemon gives the calls Sonny answers.
pub const TAG: &str = "sonny";

pub const START: &str = "start_task";
pub const REMOVE: &str = "remove_task";
pub const SHOW: &str = "show_agent";
pub const RUN: &str = "run_in_pane";
pub const PROJECT: &str = "new_project";
pub const SUBSCRIPTIONS: &str = "subscriptions";

/// The tools whose calls wait for a verdict wherever the agent's other
/// calls do: a start runs the folder's setup script and a new project its
/// lead, outside the agent's sandbox as a pane's command is, a removal can
/// drop work, and a subscription sends other bots' words to the
/// subscriber's judge.
pub const GATED: [&str; 5] = [START, REMOVE, PROJECT, SUBSCRIPTIONS, RUN];

/// The most of what a call printed that its result holds, under the
/// daemon's bound on an answer.
const OUTPUT_BYTES: usize = 60 * 1024;

/// How often waiting calls are listed with no call started to say so: a
/// start missed while the daemon restarted is answered by then.
const SWEEP: Duration = Duration::from_secs(30);

const SPLIT: [&str; 4] = crate::mux::SIDES;

/// Sonny's answered tools, as it declares them on a lead, or on another
/// bot: the lead's own, which the tasks it starts do not take, and those
/// every agent has.
pub fn tools(lead: bool) -> Vec<Value> {
    let mut tools = Vec::new();
    if lead {
        tools.extend([start(), remove(), project(), subscriptions()]);
    }
    tools.extend([show(), run()]);
    tools
}

fn start() -> Value {
    json!({
        "name": START,
        "description": "Start a new task. Sonny makes the task's folder the way the person set Sonny up, unless \
            this project's tasks all work in this folder: a git worktree of this repository on the branch \
            agent/NAME, from the last commit, or a sandfs copy, which takes uncommitted changes along and works \
            outside git too. With setup, the folder's .agents/setup runs in the copy first. The task starts there \
            with this folder's rules, skills and memory, in the role named, and the brief as its first message. \
            Paths in this folder that the brief names are turned into the task's own. The result is the task's \
            turn handle, and how its work comes back here: on its branch, here already for a worktree; with \
            the git push it names for a sandfs copy of a repository; or for a sandfs copy of another folder, with \
            `task changes`, which lists what the task changed and with --apply writes it here once you have checked \
            it. Take a task's work back only that way: never copy its files here yourself (cp, a redirect, tee), \
            which can write zeros over the files here. A start that fails leaves no folder behind.",
        "parameters": {
            "type": "object",
            "properties": {
                "name": {"type": "string", "minLength": 1, "maxLength": 100,
                    "description": "A short name for the task, valid as a git branch name, that no agent of \
                        this project has yet; Sonny puts your project's name and a dot before it."},
                "brief": {"type": "string", "minLength": 1,
                    "description": "Everything the task needs, since it never sees your conversation."},
                "profile": {"type": "string", "minLength": 1,
                    "description": "A role listed under Profiles that fits the task; without one, it \
                        starts in no role."},
                "setup": {"type": "boolean",
                    "description": "Run the folder's .agents/setup in the copy first: true for a task \
                        that will change files, false for one that only reads."},
                "model": {"type": "string", "minLength": 1,
                    "description": "PROVIDER/MODEL, when the person asked for one; else the one this \
                        project's task rule names, else yours."},
                "effort": {"type": "string", "minLength": 1,
                    "description": "The task's effort level."}
            },
            "required": ["name", "brief"],
            "additionalProperties": false
        },
        "answered_by": TAG,
        // A setup script may build for minutes.
        "expire_ms": 20 * 60 * 1000,
        "inherit": false
    })
}

fn remove() -> Value {
    json!({
        "name": REMOVE,
        "description": "Remove a task once the person says it is done: the agent, then the folder Sonny made \
            it. A worktree is removed, and its branch kept while it has commits not merged here; a sandfs copy \
            is unmounted and removed. It refuses, before removing anything, while the task is working or its \
            folder holds work that is not here yet, and says what. With discard, that work is dropped: only \
            when the person says to drop it.",
        "parameters": {
            "type": "object",
            "properties": {
                "task": {"type": "string", "minLength": 1, "description": "The task's name, as you started it."},
                "discard": {"type": "boolean",
                    "description": "Remove it even with work that is not here, which is lost."}
            },
            "required": ["task"],
            "additionalProperties": false
        },
        "answered_by": TAG,
        "expire_ms": 5 * 60 * 1000,
        "inherit": false
    })
}

fn project() -> Value {
    json!({
        "name": PROJECT,
        "description": "Make a new project in a folder, when the person asks for one, or for work in a \
            subfolder that is its own repository but not a project yet: its .agents/project.toml and its lead, \
            which opens in a home of its own beside yours. The result names the lead, which never sees this \
            conversation, so brief it as you would a task. A folder that is a project already keeps its lead \
            and its settings.",
        "parameters": {
            "type": "object",
            "properties": {
                "folder": {"type": "string", "minLength": 1, "description": "The project's folder, absolute."},
                "model": {"type": "string", "minLength": 1,
                    "description": "The lead's PROVIDER/MODEL, as the person picked; else yours."},
                "effort": {"type": "string", "minLength": 1, "description": "The lead's effort level."},
                "threads_model": {"type": "string", "minLength": 1,
                    "description": "PROVIDER/MODEL for the project's tasks, when the person picked one."},
                "threads_effort": {"type": "string", "minLength": 1,
                    "description": "The tasks' effort level."},
                "threads_in": {"type": "string", "enum": ["worktree", "project"],
                    "description": "project when the person wants every task to work in the project's \
                        folder itself."}
            },
            "required": ["folder"],
            "additionalProperties": false
        },
        "answered_by": TAG,
        "expire_ms": 2 * 60 * 1000,
        "inherit": false
    })
}

fn subscriptions() -> Value {
    let questions: Vec<&str> = crate::lookout::QUESTIONS.iter().map(|q| q.id).collect();
    json!({
        "name": SUBSCRIPTIONS,
        "description": "Your subscriptions in the person's lookout, which watches agents for you and wakes you \
            with a note starting \"Task updates\" when one needs you. Each watches the tasks of a bot (every \
            agent it or its tasks made) or bots by name, and asks a judge model the lookout's built-in \
            questions about what they say and how their turns end, or what you look for in your own words, \
            with what you do on a yes. A note that names a subscription means do what it says. Add one, or \
            remove one by name; with neither, or after either, the result lists them all. Adding one again \
            changes nothing; one by a name in use that differs is refused with the field, so remove it first.",
        "parameters": {
            "type": "object",
            "properties": {
                "add": {
                    "type": "object",
                    "properties": {
                        "name": {"type": "string", "pattern": "^[A-Za-z0-9_-][A-Za-z0-9._-]{0,63}$"},
                        "tasks_of": {"type": "array", "items": {"type": "string", "minLength": 1},
                            "description": "Bots whose tasks it watches."},
                        "bots": {"type": "array", "items": {"type": "string", "minLength": 1},
                            "description": "Bots it watches by name, * matching any run of characters."},
                        "on": {"type": "array", "items": {"type": "string", "enum": ["said", "ended"]},
                            "description": "What it watches: what the bots say, how their turns end; both \
                                when left out."},
                        "ask": {"type": "array", "items": {"type": "string", "enum": questions},
                            "description": "Which built-in questions it asks; every one that fits when left \
                                out, unless it looks for something of its own."},
                        "look_for": {"type": "array", "items": {
                            "type": "object",
                            "properties": {
                                "text": {"type": "string", "minLength": 1, "description": "What to look for."},
                                "then": {"type": "string", "minLength": 1, "description": "What you do on a yes."},
                                "urgent": {"type": "boolean", "description": "A yes cuts into your running turn."}
                            },
                            "required": ["text", "then"],
                            "additionalProperties": false
                        }},
                        "context": {"type": "string", "minLength": 1,
                            "description": "A line every check reads: what you care about."},
                        "updates": {"type": "boolean",
                            "description": "Also hear how the turns of the bots you made ended, unjudged."}
                    },
                    "required": ["name"],
                    "additionalProperties": false
                },
                "remove": {"type": "string", "minLength": 1, "description": "The name of one to remove."}
            },
            "additionalProperties": false
        },
        "answered_by": TAG,
        "inherit": false
    })
}

fn show() -> Value {
    json!({
        "name": SHOW,
        "description": "Put an agent's window where the person asks, or where they should watch it now: a tab \
            of its own, the default, or a split on one side of yours, or of the agent beside names. Another \
            project's lead (PROJECT.lead) opens in its project's own home instead, unless it is open there \
            already. Every agent you start gets a tab of its own when its turn starts, without this.",
        "parameters": {
            "type": "object",
            "properties": {
                "agent": {"type": "string", "minLength": 1, "description": "The agent's full name."},
                "split": {"type": "string", "enum": SPLIT,
                    "description": "Beside you, or beside the agent beside names, on this side."},
                "beside": {"type": "string", "minLength": 1, "description": "The agent to split beside."}
            },
            "required": ["agent"],
            "additionalProperties": false
        },
        "answered_by": TAG
    })
}

fn run() -> Value {
    json!({
        "name": RUN,
        "description": "Run a command the person can watch in a pane of its own, labelled YOUR-NAME/NAME: one \
            that runs longer than the shell tool allows, keeps running (a dev server, a watcher), or that the \
            person wants to watch. It runs in your folder, or cwd, in the person's shell, which stays open after \
            the command exits so its output can be read. Running the same name again replaces its pane. A tab \
            is the default; a split puts it beside you, for something the person should see now. The result \
            holds the pane's ID as block.",
        "parameters": {
            "type": "object",
            "properties": {
                "name": {"type": "string", "pattern": "^[A-Za-z0-9][A-Za-z0-9._-]{0,63}$",
                    "description": "What it is, such as dev-server or tests."},
                "command": {"type": "string", "minLength": 1,
                    "description": "The command line, as sh -c runs it. End it with a line you can wait \
                        for, such as `cargo test; echo \"exited $?\"`, to know when it ends."},
                "cwd": {"type": "string", "minLength": 1, "description": "Where it runs, absolute."},
                "split": {"type": "string", "enum": SPLIT, "description": "Beside you, on this side."}
            },
            "required": ["name", "command"],
            "additionalProperties": false
        },
        "answered_by": TAG
    })
}

/// An answer: the call's result, or an error as the daemon carries it.
type Answer = Result<String, (&'static str, String)>;

/// `sonny tool NAME`, as the lookout runs it for one call: the arguments
/// on stdin, in the calling agent's folder and with its shell's variables.
/// Prints the answer as one JSON line, `{"output": ...}` or
/// `{"error": {"code", "detail"}}`.
pub fn answer_here(args: &[String]) -> Result<(), String> {
    let [name] = args else {
        return Err("sonny tool NAME runs one of Sonny's tools for the lookout".into());
    };
    let mut input = String::new();
    std::io::Read::read_to_string(&mut std::io::stdin(), &mut input).map_err(|e| e.to_string())?;
    let arguments: Value = serde_json::from_str(&input).map_err(|e| e.to_string())?;
    let answer = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .map_err(|e| e.to_string())?
        .block_on(called(name, &arguments));
    let line = match answer {
        Ok(output) => json!({"output": output}),
        Err((code, detail)) => json!({"error": {"code": code, "detail": detail}}),
    };
    println!("{line}");
    Ok(())
}

async fn called(name: &str, arguments: &Value) -> Answer {
    let here = std::env::current_dir()
        .and_then(std::fs::canonicalize)
        .map_err(|e| ("not_in_folder", e.to_string()))?;
    let text = |key: &str| arguments[key].as_str().map(str::to_owned);
    let caller =
        std::env::var("AGENT_BOT").map_err(|_| ("no_caller", "AGENT_BOT is not set".into()))?;
    match name {
        START => {
            let start = crate::task::Start {
                name: text("name").unwrap_or_default(),
                brief: text("brief").unwrap_or_default(),
                profile: text("profile"),
                setup: arguments["setup"] == true,
                model: text("model"),
                effort: text("effort"),
            };
            crate::task::start(&here, &caller, &start).map_err(|e| ("task_not_started", e))
        }
        REMOVE => {
            let task = text("task").unwrap_or_default();
            let task = crate::resolved::call(&json!({"task": task}), &project_of(&caller))
                .ok_or(("invalid_arguments", "a task's name, without /".to_owned()))?;
            crate::task::remove(&here, &task, arguments["discard"] == true)
                .map_err(|e| ("task_not_removed", e))
        }
        SHOW => crate::show(
            &caller,
            &text("agent").unwrap_or_default(),
            text("split"),
            text("beside"),
        )
        .await
        .map(|shown| shown.to_string())
        .map_err(|e| ("not_shown", e)),
        RUN => {
            let cwd = text("cwd").map_or(here, PathBuf::from);
            let command = text("command").unwrap_or_default();
            crate::run_in_pane(
                &caller,
                &text("name").unwrap_or_default(),
                &cwd,
                &command,
                text("split"),
            )
            .await
            .map(|opened| opened.to_string())
            .map_err(|e| ("pane_not_opened", e))
        }
        PROJECT => crate::project::add(&caller, arguments)
            .await
            .map(|made| made.to_string())
            .map_err(|e| ("project_not_made", e)),
        SUBSCRIPTIONS => {
            let (store, _) = crate::daemon_paths(None, None).map_err(|e| ("no_store", e))?;
            crate::lookout::subs::change(
                &store,
                &caller,
                arguments.get("add"),
                arguments["remove"].as_str(),
            )
            .map(|listed| listed.to_string())
            .map_err(|e| ("subscription_refused", e))
        }
        _ => Err(("unknown_tool", format!("Sonny answers no tool {name}"))),
    }
}

/// The project a lead leads: its name less `.lead`, else up to its first dot.
fn project_of(bot: &str) -> String {
    match bot.strip_suffix(".lead") {
        Some(project) => project.to_owned(),
        None => bot
            .split_once('.')
            .map_or(bot, |(project, _)| project)
            .to_owned(),
    }
}

/// The lookout's server for Sonny's tools: told of each event the lookout
/// follows, it answers the calls that wait for Sonny.
pub struct Server {
    nudge: Arc<Notify>,
    subscribed: Arc<AtomicBool>,
    serving: Serving,
}

impl Server {
    /// Serve the daemon of `store` on `socket`, from a connection of its
    /// own.
    pub fn start(store: PathBuf, socket: PathBuf) -> Server {
        let (nudge, subscribed, serving) =
            (Arc::new(Notify::new()), Arc::default(), Serving::default());
        tokio::spawn(serve(
            store,
            socket,
            nudge.clone(),
            Arc::clone(&subscribed),
            Arc::clone(&serving),
        ));
        Server {
            nudge,
            subscribed,
            serving,
        }
    }

    /// Whether a `subscriptions` call changed a file since last asked, so
    /// the lookout reads them now rather than at its next reread.
    pub fn subscribed(&self) -> bool {
        self.subscribed.swap(false, Ordering::Relaxed)
    }

    /// An event the lookout read: a call of Sonny's that started, or a
    /// stream live again, which may follow calls started while it was not.
    /// A turn that ended, as one the person interrupted, stops what its
    /// calls still run, so a slow setup starts no task after all.
    pub fn event(&self, event: &Value) {
        let started = event["event"] == "tool_started" && event["data"]["answered_by"] == TAG;
        if started || event["event"] == "follow_live" {
            self.nudge.notify_one();
        }
        if event["event"] == "turn_finished"
            && let Some(turn) = event["turn"].as_i64()
        {
            self.serving.lock().unwrap().retain(|key, job| {
                if key.0 == turn {
                    job.abort();
                }
                key.0 != turn
            });
        }
    }
}

/// The calls being answered, by their turn and ID.
type Serving = Arc<Mutex<HashMap<(i64, String), tokio::task::AbortHandle>>>;

async fn serve(
    store: PathBuf,
    socket: PathBuf,
    nudge: Arc<Notify>,
    subscribed: Arc<AtomicBool>,
    serving: Serving,
) {
    let Ok(me) = std::env::current_exe() else {
        return;
    };
    loop {
        if let Ok((client, _events)) = Client::connect(&socket).await {
            loop {
                let Ok(calls) = waiting(&client).await else {
                    break;
                };
                for call in calls {
                    let key = (call["turn"].as_i64().unwrap_or(0), str_of(&call["call_id"]));
                    let mut jobs = serving.lock().unwrap();
                    if !jobs.contains_key(&key) {
                        let (client, me, store, socket, serving, subscribed) = (
                            client.clone(),
                            me.clone(),
                            store.clone(),
                            socket.clone(),
                            serving.clone(),
                            subscribed.clone(),
                        );
                        let held = key.clone();
                        let job = tokio::spawn(async move {
                            let answer = answer(&client, &call, &me, &store, &socket).await;
                            let mut asked = json!({"bot": call["bot"], "turn": call["turn"],
                                "call_id": call["call_id"]});
                            match answer {
                                Ok(output) => {
                                    if call["name"] == SUBSCRIPTIONS {
                                        subscribed.store(true, Ordering::Relaxed);
                                    }
                                    asked["output"] = json!(cut(output))
                                }
                                Err((code, detail)) => {
                                    asked["error"] = json!({"code": code, "detail": cut(detail)})
                                }
                            }
                            // One out of time, interrupted or answered already
                            // waits for nothing more.
                            let _ = client.request("tool_result", asked).await;
                            serving.lock().unwrap().remove(&key);
                        });
                        jobs.insert(held, job.abort_handle());
                    }
                }
                tokio::select! {
                    _ = nudge.notified() => {}
                    _ = tokio::time::sleep(SWEEP) => {}
                }
            }
        }
        tokio::time::sleep(Duration::from_secs(2)).await;
    }
}

fn str_of(value: &Value) -> String {
    value.as_str().unwrap_or_default().to_owned()
}

/// Every call waiting for Sonny's answer, page by page.
async fn waiting(client: &Client) -> Result<Vec<Value>, agent_client::Error> {
    let mut calls = Vec::new();
    let mut after = 0;
    loop {
        let page = client
            .request(
                "tool_calls",
                json!({"tag": TAG, "after": after, "limit": 256}),
            )
            .await?;
        calls.extend(page["calls"].as_array().cloned().unwrap_or_default());
        match page["next_after"].as_i64() {
            Some(next) => after = next,
            None => return Ok(calls),
        }
    }
}

/// `text` within an answer's bound, its end cut on a character.
fn cut(mut text: String) -> String {
    if text.len() > OUTPUT_BYTES {
        let mut end = OUTPUT_BYTES;
        while !text.is_char_boundary(end) {
            end -= 1;
        }
        text.truncate(end);
        text.push_str("\n[cut]");
    }
    text
}

/// One call answered by `sonny tool NAME`, run as the calling agent's shell
/// would run it, and stopped when the call's time is up.
async fn answer(client: &Client, call: &Value, me: &Path, store: &Path, socket: &Path) -> Answer {
    let bot = call["bot"].as_str().unwrap_or_default();
    let page = client
        .request("bots", json!({"name": bot, "limit": 1}))
        .await
        .map_err(|e| ("caller_unknown", e.to_string()))?;
    let record = page["bots"]
        .get(0)
        .filter(|record| record["name"] == bot)
        .ok_or(("caller_unknown", format!("{bot} is gone")))?;
    // As the calling turn runs, which keeps its folder and model when the
    // bot's own change for a turn after it.
    let turn = call["turn"].as_i64().unwrap_or(0);
    let page = client
        .request(
            "turns",
            json!({"bot": bot, "after": turn - 1, "before": turn + 1, "limit": 1}),
        )
        .await
        .map_err(|e| ("caller_unknown", e.to_string()))?;
    let running = page["turns"].get(0).filter(|t| t["turn"] == turn);
    let of = |key: &str| running.map_or(&Value::Null, |t| &t[key]);
    let workspace = of("workspace")
        .as_str()
        .or(record["workspace"].as_str())
        .unwrap_or("/");
    let model = match of("model").as_str() {
        Some(model) => model.to_owned(),
        None => format!(
            "{}/{}",
            str_of(&record["provider"]),
            str_of(&record["model"])
        ),
    };
    let effort = match running {
        Some(_) => of("effort").as_str(),
        None => record["effort"].as_str(),
    };
    let mut command = tokio::process::Command::new(me);
    command
        .arg("tool")
        .arg(call["name"].as_str().unwrap_or_default())
        .current_dir(workspace)
        .env("PWD", workspace)
        .env("AGENT_BOT", bot)
        .env("AGENT_BOT_ID", record["bot_id"].to_string())
        .env("AGENT_TURN", call["turn"].to_string())
        .env("AGENT_STORE", store)
        .env("AGENT_SOCKET", socket)
        .env("AGENT_MODEL", model)
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        // A group of its own, so what it started stops with it.
        .process_group(0)
        .kill_on_drop(true);
    match effort {
        Some(effort) => command.env("AGENT_EFFORT", effort),
        None => command.env_remove("AGENT_EFFORT"),
    };
    if let Some(bin) = crate::agent_bin() {
        command.env("AGENT_BIN", bin);
    }
    let mut child = command
        .spawn()
        .map_err(|e| ("tool_failed", format!("{}: {e}", me.display())))?;
    let group = child.id().map(Group);
    if let Some(mut stdin) = child.stdin.take() {
        use tokio::io::AsyncWriteExt;
        let _ = stdin
            .write_all(call["arguments"].to_string().as_bytes())
            .await;
    }
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_millis() as u64);
    let left = call["expires_ms"]
        .as_u64()
        .unwrap_or(now)
        .saturating_sub(now);
    let output = tokio::time::timeout(Duration::from_millis(left), child.wait_with_output())
        .await
        .map_err(|_| ("tool_unserved", "ran out of time".to_owned()))?;
    // Answered: what it left running on purpose, as a pane's program or a
    // lookout it started, runs on.
    std::mem::forget(group);
    let output = output.map_err(|e| ("tool_failed", e.to_string()))?;
    let stdout = String::from_utf8_lossy(&output.stdout);
    let said: Option<Value> = stdout
        .lines()
        .last()
        .and_then(|line| serde_json::from_str(line).ok());
    match said {
        Some(said) if said["output"].is_string() => Ok(str_of(&said["output"])),
        Some(said) if said["error"]["code"].is_string() => Err((
            code(said["error"]["code"].as_str().unwrap_or_default()),
            str_of(&said["error"]["detail"]),
        )),
        _ => Err((
            "tool_failed",
            String::from_utf8_lossy(&output.stderr).trim().to_owned(),
        )),
    }
}

/// A call's process group, stopped when its answer is given up: out of
/// time, or its turn over, as the person interrupting a slow setup.
struct Group(u32);

impl Drop for Group {
    fn drop(&mut self) {
        // SAFETY: a signal to the group this call's process leads.
        unsafe { libc::kill(-(self.0 as libc::pid_t), libc::SIGKILL) };
    }
}

/// The error codes `sonny tool` answers with, as static names.
fn code(said: &str) -> &'static str {
    const CODES: [&str; 12] = [
        "not_in_folder",
        "no_caller",
        "invalid_arguments",
        "task_not_started",
        "task_not_removed",
        "not_shown",
        "pane_not_opened",
        "project_not_made",
        "no_store",
        "subscription_refused",
        "unknown_tool",
        "tool_failed",
    ];
    CODES
        .into_iter()
        .find(|code| *code == said)
        .unwrap_or("tool_failed")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_lead_answers_more_than_its_tasks_and_keeps_its_own() {
        let names = |tools: Vec<Value>| -> Vec<String> {
            tools.iter().map(|t| str_of(&t["name"])).collect()
        };
        assert_eq!(names(tools(false)), [SHOW, RUN]);
        assert_eq!(
            names(tools(true)),
            [START, REMOVE, PROJECT, SUBSCRIPTIONS, SHOW, RUN]
        );
        for tool in tools(true) {
            assert_eq!(tool["answered_by"], TAG);
            // A lead's own are not its tasks'.
            assert_eq!(
                tool.get("inherit").is_some(),
                !names(tools(false)).contains(&str_of(&tool["name"]))
            );
        }
        assert_eq!(project_of("shop.web.lead"), "shop.web");
        assert_eq!(project_of("shop.fix"), "shop");
    }

    #[test]
    fn an_answer_fits_the_daemon_s_bound_and_its_code_is_one_of_ours() {
        let long = "é".repeat(OUTPUT_BYTES);
        assert!(cut(long).len() <= OUTPUT_BYTES + 6);
        assert_eq!(cut("short".into()), "short");
        assert_eq!(code("task_not_started"), "task_not_started");
        assert_eq!(code("Anything Else"), "tool_failed");
    }
}
