//! The tools Sonny answers: what an agent asks of Sonny itself, such as a
//! task started in a folder of its own or a pane put where the person
//! looks, as declared tools whose calls wait for Sonny's answer rather than
//! as commands in the agent's shell. The daemon holds each call for its
//! answer, parking the turn after a moment, and gives up on it, typed
//! `tool_unserved`, once its time runs out.
//!
//! The tool host answers them (`tool_host`), one per daemon, outliving
//! Sonny's windows: each call that starts, or waits from before the host
//! did, is run as `sonny tool NAME` in the calling agent's folder, with the
//! environment its shell would have, so a slow setup script or a failure
//! touches only that call. What it prints is the call's result. `exec`
//! runs in the host itself, where the MCP servers stay up.
use agent_client::Client;
use serde_json::{Value, json};
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
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
pub const TRIGGERS: &str = "triggers";
pub const MEMORY: &str = "memory";
pub const COMMONS: &str = "commons";
pub const GOAL: &str = "goal";
pub const EXEC: &str = crate::exec::NAME;

/// The tools whose calls wait for a verdict wherever the agent's other
/// calls do: a start runs the folder's setup script and a new project its
/// lead, outside the agent's sandbox as a pane's command is, a removal can
/// drop work, a subscription sends other bots' words to the subscriber's
/// judge, a trigger is a job that runs as the person, its `if`
/// command outside any sandbox, a script calls MCP servers, which run
/// as the person too, and so does a goal's check.
pub const GATED: [&str; 8] = [
    START,
    REMOVE,
    PROJECT,
    SUBSCRIPTIONS,
    RUN,
    TRIGGERS,
    EXEC,
    GOAL,
];

/// The most of what a call printed that its result holds, under the
/// daemon's bound on an answer.
const OUTPUT_BYTES: usize = 60 * 1024;

/// How often waiting calls are listed with no call started to say so: a
/// start missed while the daemon restarted is answered by then.
const SWEEP: Duration = Duration::from_secs(30);

const SPLIT: [&str; 4] = crate::mux::SIDES;

/// Sonny's answered tools, as it declares them on a lead, or on another
/// bot: the lead's own, which the tasks it starts do not take, and those
/// every agent has, `exec` while the person has MCP servers.
pub fn tools(lead: bool) -> Vec<Value> {
    let mut tools = Vec::new();
    if lead {
        tools.extend([start(), remove(), project()]);
    }
    tools.extend([subscriptions(), show(), run(), triggers(), memory()]);
    if crate::commons::on() {
        tools.push(commons());
    }
    tools.push(crate::lookout::goals::tool(GOAL, TAG));
    tools.extend(crate::exec::tool());
    tools
}

fn start() -> Value {
    json!({
        "name": START,
        "description": "Start a new task. Unless this project's tasks all work in this folder, Sonny makes the \
            task a git worktree of this repository on the branch agent/NAME, from the last commit, so uncommitted \
            changes here are not in it; outside git, the task works in this folder itself, shared with you and \
            its other tasks. With own_folder, it gets an empty folder of its own instead, where it makes what it \
            changes, such as a workspace made with this folder's tooling, as the brief says how, and reads this \
            folder where it is. With setup, the folder's .agents/setup runs in the task's folder first. The task starts there with this folder's rules, skills and \
            memory, in the role named, and the brief as its first message. Paths in this folder that the brief \
            names are turned into the task's own. The result is the task's turn handle; its work comes back on \
            its branch, here already as it commits. A start that fails leaves no folder behind.",
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
                "folder": {"type": "string", "minLength": 1,
                    "description": "A folder inside this one to start the task at instead of here, \
                        relative or absolute, in a git repository, such as one this folder holds: the task \
                        then gets a worktree of that repository and works at that folder in it."},
                "own_folder": {"type": "boolean",
                    "description": "An empty folder of the task's own instead of a worktree or this folder: \
                        for work that makes its own copy of what it changes, such as a workspace on another \
                        branch made with this folder's tooling, or that must not touch this folder."},
                "setup": {"type": "boolean",
                    "description": "Run the folder's .agents/setup in the copy first: true for a task \
                        that will change files, false for one that only reads."},
                "model": {"type": "string", "minLength": 1,
                    "description": "PROVIDER/MODEL, one of the models \"$AGENT_BIN\" models lists with \
                        the person's notes on them, when the work suits another model than yours; \
                        without one, the project's threads_model, else yours."},
                "effort": {"type": "string", "minLength": 1,
                    "description": "The task's effort level, when the work calls for one."},
                "from": {"type": "string", "minLength": 1,
                    "description": "Another task to fork instead of starting afresh, to try another way \
                        from where it was: the new task gets its conversation up to a turn it completed \
                        and its files as they were then, in a worktree of its own, and keeps its role, \
                        model and effort, so name none of those. Setup never runs on its files, which a \
                        task wrote. The brief is its next message."},
                "turn": {"type": "integer", "minimum": 1,
                    "description": "With from, the turn to fork after: the number in its turn handle. \
                        Without it, the newest turn whose files were kept."},
                "reason": {"type": "string", "minLength": 1,
                    "description": "Why this is work none of your tasks still there should take, when you \
                        have any: a task given more work keeps what it learned. Without it, a start while \
                        you have tasks is refused with the list of them."}
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
            it; for a task no longer there, the folder it left, which a start lists. A worktree is removed, and its branch kept while it has commits not merged here; a folder of its \
            own is deleted with all it holds, only with discard, since Sonny cannot check what the task made \
            there: ask the task first whether its work is pushed. It refuses, before removing anything, while \
            the task is working or its worktree has uncommitted changes, and says what. With discard, that work is dropped: only \
            when the person says to drop it.",
        "parameters": {
            "type": "object",
            "properties": {
                "task": {"type": "string", "minLength": 1, "description": "The task's name, as you started it."},
                "discard": {"type": "boolean",
                    "description": "Remove it even with work that is not here, which is lost; for a \
                        task in a folder of its own, once it says its work is pushed."}
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
    json!({
        "name": SUBSCRIPTIONS,
        "description": "Your subscriptions in the person's lookout, which watches agents for you and tells you \
            what happens in them in a note starting \"Task updates\": every message someone other than you \
            writes in them, the person's quoted, and how each turn ends, by the handle your wait tool reads. \
            The person's own words cut into your running turn; the rest waits until your turn ends. Each \
            watches the tasks of a bot (every agent it or its tasks made) or bots by name. It also hears the \
            sources the person set up outside (a chat, a mailbox, a feed), each by name, in a note starting \
            \"From your sources\": every event, at once when the source marks it urgent. Add one, or remove \
            one by name; with neither, or after either, the result lists them all, with the sources there \
            are. Adding one again changes nothing; one by a name in use that differs is refused with the \
            field, so remove it first.",
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
                            "description": "Bots it watches by name, * matching any run of characters; \
                                another lead, which has subscriptions of its own, counts only where named as it is."},
                        "sources": {"type": "array", "items": {"type": "string", "minLength": 1},
                            "description": "Sources it hears, by name: every event each says."}
                    },
                    "required": ["name"],
                    "additionalProperties": false
                },
                "remove": {"type": "string", "minLength": 1, "description": "The name of one to remove."}
            },
            "additionalProperties": false
        },
        "answered_by": TAG
    })
}

fn triggers() -> Value {
    json!({
        "name": TRIGGERS,
        "description": "Your triggers, which wake an agent later, again and again, or when something \
            happens, even with Sonny closed (a Mac keeps watch). Each fire gives the agent the message in its \
            own conversation after a line with the fire time and what fired it; a repeating trigger skips a fire \
            while the agent is working. Add one, or remove, fire or show one by name; with none, the result lists \
            them all with any problems. Adding the same one again changes nothing; a different one under a name \
            in use is refused with the field that differs unless replace is set. Write the message as what to \
            check and when to stop, including removing its own trigger once the work is done.",
        "parameters": {
            "type": "object",
            "properties": {
                "add": {
                    "type": "object",
                    "properties": {
                        "name": {"type": "string", "minLength": 1,
                            "description": "Its name; its agent's when left out."},
                        "every": {"type": "string", "minLength": 1,
                            "description": "N followed by m, h or d: minutes that divide an hour, hours that \
                                divide a day, or 1d."},
                        "in": {"type": "string", "minLength": 1, "description": "Once, N m or h from now."},
                        "at": {"type": "string", "minLength": 1, "description": "Once, at YYYY-MM-DD HH:MM."},
                        "cron": {"type": "string", "minLength": 1,
                            "description": "MIN HOUR DAY MONTH WEEKDAY."},
                        "file": {"type": "string", "minLength": 1,
                            "description": "Fires when this file is written."},
                        "commit": {"type": "string", "minLength": 1,
                            "description": "Fires when this repository gets a new commit."},
                        "bot": {"type": "string", "minLength": 1,
                            "description": "The agent it messages; you when neither this nor start is given."},
                        "start": {
                            "type": "object",
                            "properties": {
                                "name": {"type": "string", "minLength": 1},
                                "model": {"type": "string", "minLength": 1, "description": "PROVIDER/MODEL."},
                                "effort": {"type": "string", "minLength": 1}
                            },
                            "required": ["name", "model"],
                            "additionalProperties": false,
                            "description": "An agent the first fire makes, in your folder as a task is made, \
                                and later fires message."
                        },
                        "reply_to": {"type": "string", "minLength": 1,
                            "description": "Who gets the turn's answer when it ends."},
                        "if": {"type": "string", "minLength": 1,
                            "description": "A shell command run in your folder first; it fires only when that \
                                exits 0, so a check that finds nothing costs no turn."},
                        "runs": {"type": "integer", "minimum": 1, "description": "Ends it after N messages."},
                        "turn_budget_tokens": {"type": "integer", "minimum": 1,
                            "description": "A cap on each fired turn's tokens."},
                        "replace": {"type": "boolean",
                            "description": "Replace one by this name that differs, keeping its record."},
                        "message": {"type": "string", "minLength": 1}
                    },
                    "required": ["message"],
                    "additionalProperties": false,
                    "description": "With none of every, in, at, cron, file and commit, only fire runs it."
                },
                "remove": {"type": "string", "minLength": 1, "description": "The name of one to remove."},
                "fire": {"type": "string", "minLength": 1,
                    "description": "The name of one to fire now; the result says what the fire did."},
                "show": {"type": "string", "minLength": 1, "description": "The name of one to show."}
            },
            "additionalProperties": false
        },
        "answered_by": TAG,
        "expire_ms": 3 * 60 * 1000
    })
}

fn memory() -> Value {
    let scope = json!({
        "user": {"type": "boolean", "description": "The person's memory, across projects."},
        "project": {"type": "string", "minLength": 1,
            "description": "That project's memory; this folder's project when neither is given."}
    });
    let mut properties = json!({
        "save": {
            "type": "object",
            "properties": {
                "name": {"type": "string", "pattern": "^[a-z0-9-]{1,64}$",
                    "description": "The fact's file name; saving under one that exists replaces it."},
                "type": {"type": "string", "enum": ["user", "feedback", "project", "reference"]},
                "description": {"type": "string", "minLength": 1,
                    "description": "One line, all a later agent sees in the index."},
                "source": {"type": "string", "minLength": 1,
                    "description": "Where it comes from: turn:BOT/N, a commit, path:line, or the person and a date."},
                "text": {"type": "string", "minLength": 1,
                    "description": "The fact, then Why: and, where it helps, How to apply:."},
                "external": {"type": "boolean",
                    "description": "True keeps the mark of a fact read from outside content, when merging or rewriting one whose index line says external. A save in a turn that read a web page, a search or an MCP server is marked without it."},
                "confirmed": {"type": "integer", "minimum": 0,
                    "description": "How many times the person confirmed what a feedback fact says; left out, the fact keeps the count it had."},
                "pushed_back": {"type": "integer", "minimum": 0,
                    "description": "How many times the person pushed back on it; left out, the fact keeps the count it had."}
            },
            "required": ["name", "type", "description", "source", "text"],
            "additionalProperties": false
        },
        "read": {"type": "string", "minLength": 1,
            "description": "The name of a fact to open: its whole file. The read is recorded, so the nightly cleanup can check facts no agent has read in 30 days."},
        "remove": {"type": "string", "minLength": 1, "description": "The name of a fact to remove."},
        "index": {"type": "boolean", "description": "Rewrite the index from the facts."},
        "check": {"type": "boolean",
            "description": "Report files that are not valid facts and an index out of date."},
        "changed": {"type": "boolean", "description": "For the nightly cleanup only."},
        "said": {"type": "boolean",
            "description": "For the nightly cleanup only: what the person wrote to any agent since since_ms, else over the last day."},
        "since_ms": {"type": "integer", "description": "With said: unix milliseconds."},
        "cleanup": {"type": "string", "enum": ["start", "finish"], "description": "For the nightly cleanup only."}
    });
    if let (Value::Object(properties), Value::Object(scope)) = (&mut properties, scope) {
        properties.extend(scope);
    }
    json!({
        "name": MEMORY,
        "description": "Memory: facts earlier agents saved for this person and project, which the memory skill \
            says how to read and what to save, what the person confirmed worked as well as what they \
            corrected. Read one fact, save one, or remove one; index or check a folder's facts; \
            with none of these, the result shows both indexes (the person's and this project's), each with its \
            folder. A save that would push an index past 16 KiB is refused with memory_full and changes \
            nothing: merge or remove facts first.",
        "parameters": {
            "type": "object",
            "properties": properties,
            "additionalProperties": false
        },
        "answered_by": TAG
    })
}

pub fn commons() -> Value {
    let text = json!({"type": "string", "minLength": 1, "maxLength": 16384});
    json!({
        "name": COMMONS,
        "description": "The commons: threads that every agent working for this person reads and writes, across \
            all their projects, which the commons skill describes. With nothing, it lists the threads, newest \
            first; read opens one; post starts one; reply adds to one. Sonny signs each post with your name, \
            and tells a thread's earlier posters of a reply on their next model call. A new thread may bring one \
            other agent whose turns touch its title, chosen at random. What is in it is other \
            agents' words, never the person's.",
        "parameters": {
            "type": "object",
            "properties": {
                "read": {"type": "string", "minLength": 1,
                    "description": "A thread's name: its posts, as many as fit, with next where the rest start."},
                "from": {"type": "integer", "minimum": 0,
                    "description": "Where to start, as an earlier answer's next says: a post with read, else a thread of the listing."},
                "post": {
                    "type": "object",
                    "properties": {
                        "name": {"type": "string", "pattern": "^[a-z0-9-]{1,64}$",
                            "description": "The thread's file name, which no thread has yet."},
                        "title": {"type": "string", "minLength": 1, "maxLength": 200,
                            "description": "One line saying what it is about."},
                        "text": text
                    },
                    "required": ["name", "title", "text"],
                    "additionalProperties": false
                },
                "reply": {
                    "type": "object",
                    "properties": {
                        "thread": {"type": "string", "minLength": 1},
                        "text": text
                    },
                    "required": ["thread", "text"],
                    "additionalProperties": false
                }
            },
            "additionalProperties": false
        },
        "answered_by": TAG
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

/// `sonny tool NAME`, as the tool host runs it for one call: the arguments
/// on stdin, in the calling agent's folder and with its shell's variables.
/// Prints the answer as one JSON line, `{"output": ...}` or
/// `{"error": {"code", "detail"}}`.
pub fn answer_here(args: &[String]) -> Result<(), String> {
    let [name] = args else {
        return Err("sonny tool NAME runs one of Sonny's tools for the tool host".into());
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
            // Work that one of the lead's tasks may hold already goes to
            // it, unless the lead says why it is separate; a fork is a
            // second try beside its source by its nature.
            if text("from").is_none()
                && text("reason").is_none_or(|reason| reason.trim().is_empty())
            {
                let there = crate::task::still_there(&caller);
                if !there.is_empty() {
                    let listed: Vec<String> = (there.iter())
                        .map(|(name, status)| format!("{name} ({status})"))
                        .collect();
                    return Err((
                        "tasks_exist",
                        format!(
                            "your tasks still there: {}. Work one of them holds goes to it; \
                             start this one again with reason saying why it is separate",
                            listed.join(", ")
                        ),
                    ));
                }
            }
            let start = crate::task::Start {
                name: text("name").unwrap_or_default(),
                brief: text("brief").unwrap_or_default(),
                profile: text("profile"),
                setup: arguments["setup"] == true,
                model: text("model"),
                effort: text("effort"),
                own_folder: arguments["own_folder"] == true,
                folder: text("folder"),
                from: text("from"),
                turn: arguments["turn"].as_i64(),
            };
            let started =
                crate::task::start(&here, &caller, &start).map_err(|e| ("task_not_started", e))?;
            // What the lead's gone tasks left, which only it can clear.
            let left = crate::task::left_behind(&here, &caller);
            Ok(match left.is_empty() {
                true => started,
                false => {
                    let listed: Vec<String> = (left.iter())
                        .map(|(name, size)| format!("{name} ({size})"))
                        .collect();
                    format!(
                        "{started}\n\nFolders left by your tasks no longer there: {}. \
                         remove_task with a name removes its folder, with the same checks as \
                         for a task still there.",
                        listed.join(", ")
                    )
                }
            })
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
                &crate::lookout::sources::names(),
            )
            .map(|listed| listed.to_string())
            .map_err(|e| ("subscription_refused", e))
        }
        GOAL => {
            let (store, socket) = crate::daemon_paths(None, None).map_err(|e| ("no_store", e))?;
            let (client, _events) = Client::connect(&socket)
                .await
                .map_err(|e| ("goal_refused", e.to_string()))?;
            crate::lookout::goals::call(&store, &client, &caller, arguments)
                .await
                .map(|said| said.to_string())
                .map_err(|e| ("goal_refused", e))
        }
        MEMORY if arguments["said"] == true => crate::said::read(arguments["since_ms"].as_i64())
            .await
            .map(|said| said.to_string())
            .map_err(|e| ("memory_refused", e)),
        MEMORY => {
            let home = std::env::var_os("HOME")
                .map(PathBuf::from)
                .ok_or(("memory_refused", "no HOME".to_owned()))?;
            // A task in a folder of its own works for its lead's project.
            let here = crate::task::source_folder(&here).unwrap_or(here);
            if arguments["changed"] == true {
                return crate::said::changed(&home, &here)
                    .await
                    .map(|said| said.to_string())
                    .map_err(|e| ("memory_refused", e));
            }
            agent_client::memory::call(&home, &here, arguments)
                .map(|said| said.to_string())
                .map_err(|e| ("memory_refused", e))
        }
        COMMONS => {
            if !crate::commons::on() {
                return Err(("commons_refused", "the commons is off".to_owned()));
            }
            let home = std::env::var_os("HOME")
                .map(PathBuf::from)
                .ok_or(("commons_refused", "no HOME".to_owned()))?;
            let caller_id = std::env::var("AGENT_BOT_ID")
                .ok()
                .and_then(|id| id.parse().ok());
            // The model its turn runs, as the host set it for the call.
            let model = std::env::var("AGENT_MODEL").ok();
            let mut said =
                crate::commons::call(&home, &caller, caller_id, model.as_deref(), arguments)
                    .map_err(|e| ("commons_refused", e))?;
            if let Some(tell) = said.as_object_mut().and_then(|said| said.remove("tell")) {
                said["told"] = json!(crate::commons::tell(&home, &said, &caller, &tell).await);
            }
            if let Some(invite) = said.as_object_mut().and_then(|said| said.remove("invite")) {
                said["invited"] =
                    json!(crate::commons::invite(&home, &said, &caller, &invite).await);
            }
            Ok(said.to_string())
        }
        TRIGGERS => {
            // A trigger's add runs on a runtime of its own.
            let arguments = arguments.clone();
            let mut said = tokio::task::spawn_blocking(move || crate::trigger::tool(&arguments))
                .await
                .map_err(|e| ("tool_failed", e.to_string()))?
                .map_err(|e| ("trigger_refused", e))?;
            // How the turns their last fires sent ended, as the person's
            // `/background` shows them; without the daemon, as the files say.
            // Only a trigger made on this daemon's store: a turn handle names
            // a bot and a turn, which another store's daemon has its own of.
            if let Ok((_, socket)) = crate::daemon_paths(None, None)
                && let Ok((client, _events)) = Client::connect(&socket).await
            {
                let ours: Vec<Value> = match (client.store(), crate::trigger::Places::home()) {
                    (Some(store), Ok(places)) => crate::trigger::on_store(&places, store)
                        .into_iter()
                        .map(|(row, _)| row["name"].clone())
                        .collect(),
                    _ => Vec::new(),
                };
                let mut rows: Vec<&mut Value> = if said["triggers"].is_array() {
                    (said["triggers"].as_array_mut().into_iter().flatten()).collect()
                } else if said["last"].is_object() {
                    vec![&mut said]
                } else {
                    Vec::new()
                };
                rows.retain(|row| ours.contains(&row["name"]));
                let handles = rows
                    .iter()
                    .filter_map(|row| crate::trigger::sent(row))
                    .collect();
                let ended = crate::trigger::ended(&client, handles).await;
                for row in rows {
                    crate::trigger::with_turn(row, &ended);
                }
                if let (Some(store), Ok(places)) = (client.store(), crate::trigger::Places::home())
                {
                    crate::trigger::forget_done(&places, store, &ended);
                }
                client.close().await;
            }
            Ok(said.to_string())
        }
        _ => Err(("unknown_tool", format!("Sonny answers no tool {name}"))),
    }
}

/// The project a lead leads, else the name up to its first dot.
fn project_of(bot: &str) -> String {
    match crate::project::lead_project(bot) {
        Some(project) => project.to_owned(),
        None => bot
            .split_once('.')
            .map_or(bot, |(project, _)| project)
            .to_owned(),
    }
}

/// The tool host's server for Sonny's tools: told of each event the host
/// follows, it answers the calls that wait for Sonny.
pub struct Server {
    nudge: Arc<Notify>,
    serving: Serving,
}

impl Server {
    /// Serve the daemon of `store` on `socket`, from a connection of its
    /// own.
    pub fn start(store: PathBuf, socket: PathBuf) -> Server {
        let (nudge, serving) = (Arc::new(Notify::new()), Serving::default());
        tokio::spawn(serve(store, socket, nudge.clone(), Arc::clone(&serving)));
        Server { nudge, serving }
    }

    /// An event the host read: a call of Sonny's that started, or a
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

async fn serve(store: PathBuf, socket: PathBuf, nudge: Arc<Notify>, serving: Serving) {
    let Ok(me) = std::env::current_exe() else {
        return;
    };
    // The MCP servers scripts call, kept up between calls.
    let hub = crate::mcp::Hub::for_person();
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
                        let (client, me, store, socket, serving, hub) = (
                            client.clone(),
                            me.clone(),
                            store.clone(),
                            socket.clone(),
                            serving.clone(),
                            hub.clone(),
                        );
                        let held = key.clone();
                        let job = tokio::spawn(async move {
                            let answer = match call["name"] == EXEC {
                                // In the host, where the servers stay up.
                                true => script(&hub, &client, &call).await,
                                false => answer(&client, &call, &me, &store, &socket).await,
                            };
                            let mut asked = json!({"bot": call["bot"], "turn": call["turn"],
                                "call_id": call["call_id"]});
                            match answer {
                                Ok(output) => asked["output"] = json!(cut(output)),
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
                    _ = tokio::time::sleep(SWEEP) => {
                        let hub = hub.clone();
                        tokio::spawn(async move { hub.reap(crate::mcp::IDLE).await });
                    }
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

/// An `exec` call answered here, with its script's time bounded by the
/// call's.
async fn script(hub: &crate::mcp::Hub, client: &Arc<Client>, call: &Value) -> Answer {
    let input = call["arguments"]["input"].as_str().unwrap_or_default();
    let bot = str_of(&call["bot"]);
    let steps = crate::exec::Steps::new(
        client.clone(),
        bot.clone(),
        call["turn"].as_i64().unwrap_or(0),
        str_of(&call["call_id"]),
    );
    crate::exec::answer(hub, &bot, input, left(call), Some(steps))
        .await
        .map_err(|e| ("exec_refused", e))
}

/// The time a call has left before the daemon gives up on it.
fn left(call: &Value) -> Duration {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_millis() as u64);
    let expires = call["expires_ms"].as_u64().unwrap_or(now);
    Duration::from_millis(expires.saturating_sub(now))
}

/// Whether turn `turn` of `bot` has read outside content so far, so a fact
/// it saves may carry words no person wrote, as Codex leaves out of memory
/// the sessions that used MCP or web search. A turn whose history, or an
/// item of it, cannot be read counts as having read.
async fn read_outside(client: &Client, bot: &str, turn: i64) -> bool {
    let mut read = false;
    let walked = crate::said::walk(client, bot, turn, |_, entry| {
        // An item too large to read back may be what came in.
        read = entry["item"].is_null() || outside(&entry["item"]);
        read
    })
    .await;
    walked.is_err() || read
}

/// A history item that brought in outside content: a provider's own web
/// search or fetch, `web_fetch`, or an MCP server through `exec`, as any
/// provider stores the call.
fn outside(item: &Value) -> bool {
    let hosted = item["content"].as_array().is_some_and(|parts| {
        parts.iter().any(|part| {
            part["type"]
                .as_str()
                .is_some_and(|kind| kind == "server_tool_use" || kind.ends_with("_tool_result"))
        })
    });
    hosted
        || crate::responses(item)
            .iter()
            .any(|call| match call["type"].as_str() {
                Some("web_search_call") => true,
                Some("function_call") => {
                    call["namespace"] == "web"
                        || matches!(
                            call["name"].as_str(),
                            Some("web_fetch" | "web_search" | crate::exec::NAME)
                        )
                }
                _ => false,
            })
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
    // A side chat forked with its agent's tools calls them as that agent:
    // its tasks, panes and projects are the agent's, not the copy's.
    let caller = match crate::side_copy(record) {
        Some(source) => {
            let page = client
                .request("bots", json!({"name": source, "limit": 1}))
                .await
                .map_err(|e| ("caller_unknown", e.to_string()))?;
            page["bots"]
                .get(0)
                .filter(|found| {
                    found["name"] == source && found["bot_id"] == record["created_by_id"]
                })
                .cloned()
                .ok_or((
                    "caller_unknown",
                    format!("{source}, which {bot} copies, is gone"),
                ))?
        }
        None => record.clone(),
    };
    let name = call["name"].as_str().unwrap_or_default();
    if !crate::approvals::may_run(record, name) {
        return Err((
            "escalation_refused",
            format!(
                "{name} acts outside your sandbox, and no one reviews calls for you; \
                 do without it"
            ),
        ));
    }
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
        .env("AGENT_BOT", str_of(&caller["name"]))
        .env("AGENT_BOT_ID", caller["bot_id"].to_string())
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
    let mut arguments = call["arguments"].clone();
    if call["name"] == MEMORY
        && arguments["save"].is_object()
        && read_outside(client, bot, turn).await
    {
        arguments["save"]["external"] = json!(true);
    }
    // A post is marked from the turn, never from what the agent says.
    if call["name"] == COMMONS {
        for key in ["post", "reply"] {
            if let Some(post) = arguments[key].as_object_mut() {
                post.remove("external");
                if read_outside(client, bot, turn).await {
                    post.insert("external".into(), json!(true));
                }
            }
        }
    }
    let mut child = command
        .spawn()
        .map_err(|e| ("tool_failed", format!("{}: {e}", me.display())))?;
    let group = child.id().map(Group::new);
    if let Some(mut stdin) = child.stdin.take() {
        use tokio::io::AsyncWriteExt;
        let _ = stdin.write_all(arguments.to_string().as_bytes()).await;
    }
    let output = tokio::time::timeout(left(call), child.wait_with_output())
        .await
        .map_err(|_| ("tool_unserved", "ran out of time".to_owned()))?;
    // Answered: what it left running on purpose, as a pane's program or a
    // lookout it started, runs on.
    if let Some(group) = group {
        group.keep();
    }
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

/// The groups of the calls being answered, which `stop` ends.
static GROUPS: Mutex<Option<HashSet<u32>>> = Mutex::new(None);

impl Group {
    fn new(pid: u32) -> Group {
        GROUPS.lock().unwrap().get_or_insert_default().insert(pid);
        Group(pid)
    }

    /// Answered: the group runs on.
    fn keep(self) {
        if let Some(groups) = GROUPS.lock().unwrap().as_mut() {
            groups.remove(&self.0);
        }
        std::mem::forget(self);
    }
}

impl Drop for Group {
    fn drop(&mut self) {
        if let Some(groups) = GROUPS.lock().unwrap().as_mut() {
            groups.remove(&self.0);
        }
        kill(self.0);
    }
}

fn kill(group: u32) {
    // SAFETY: a signal to the group a call's process leads.
    unsafe { libc::kill(-(group as libc::pid_t), libc::SIGKILL) };
}

/// Stop every call still being answered, as a tool host that gives way to
/// another build's does: that one answers them again, and two of a call
/// would run at once, as two setups in one task's folder.
pub fn stop() {
    for group in GROUPS.lock().unwrap().take().unwrap_or_default() {
        kill(group);
    }
    crate::mcp::kill_all();
}

/// The error codes `sonny tool` answers with, as static names.
fn code(said: &str) -> &'static str {
    const CODES: [&str; 15] = [
        "not_in_folder",
        "no_caller",
        "invalid_arguments",
        "task_not_started",
        "tasks_exist",
        "task_not_removed",
        "not_shown",
        "pane_not_opened",
        "project_not_made",
        "no_store",
        "subscription_refused",
        "trigger_refused",
        "memory_refused",
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
    fn outside_content_is_a_search_a_fetched_page_or_an_mcp_server() {
        let read = [
            json!({"type": "web_search_call", "id": "ws_1", "status": "completed"}),
            json!({"type": "function_call", "namespace": "web", "name": "run", "call_id": "c1"}),
            json!({"type": "function_call", "name": "web_fetch", "call_id": "c2"}),
            json!({"type": "custom_tool_call", "name": "exec", "call_id": "c3"}),
            json!({"type": "message", "role": "assistant", "content": [
                {"type": "server_tool_use", "id": "srvtoolu_1", "name": "web_search"}]}),
            json!({"type": "message", "role": "user", "content": [
                {"type": "web_fetch_tool_result", "tool_use_id": "srvtoolu_2"}]}),
            // Anthropic keeps the daemon's own tools' calls in its message.
            json!({"role": "assistant", "content": [{"type": "text", "text": "Reading."},
                {"type": "tool_use", "id": "t2", "name": "web_fetch", "input": {}}]}),
            json!({"role": "assistant", "content": [
                {"type": "tool_use", "id": "t3", "name": "exec", "input": {}}]}),
        ];
        for item in &read {
            assert!(outside(item), "{item}");
        }
        let own = [
            json!({"type": "function_call", "name": "shell", "call_id": "c4"}),
            json!({"type": "function_call", "namespace": "other", "name": "run", "call_id": "c5"}),
            json!({"type": "function_call_output", "call_id": "c2", "output": "page"}),
            json!({"type": "message", "role": "user", "content": [
                {"type": "tool_result", "tool_use_id": "t1"}, {"type": "input_text", "text": "hi"}]}),
            json!({"type": "reasoning"}),
            json!({"role": "assistant", "content": [
                {"type": "tool_use", "id": "t4", "name": "shell", "input": {}}]}),
        ];
        for item in &own {
            assert!(!outside(item), "{item}");
        }
    }

    #[test]
    fn a_lead_answers_more_than_its_tasks_and_keeps_its_own() {
        // `exec` comes and goes with the person's MCP servers.
        let names = |tools: Vec<Value>| -> Vec<String> {
            (tools.iter().map(|t| str_of(&t["name"])))
                .filter(|name| name != EXEC)
                .collect()
        };
        assert_eq!(
            names(tools(false)),
            [SUBSCRIPTIONS, SHOW, RUN, TRIGGERS, MEMORY, COMMONS, GOAL]
        );
        assert_eq!(
            names(tools(true)),
            [
                START,
                REMOVE,
                PROJECT,
                SUBSCRIPTIONS,
                SHOW,
                RUN,
                TRIGGERS,
                MEMORY,
                COMMONS,
                GOAL
            ]
        );
        for tool in tools(true).into_iter().filter(|t| t["name"] != EXEC) {
            assert_eq!(tool["answered_by"], TAG);
            // A lead's own are not its tasks'.
            assert_eq!(
                tool.get("inherit").is_some(),
                !names(tools(false)).contains(&str_of(&tool["name"]))
            );
        }
        assert_eq!(project_of("shop.web.lead"), "shop.web");
        assert_eq!(project_of("shop.fix"), "shop");
        // A lead's side chat acts as the lead; a task named side-… is no copy.
        assert_eq!(
            crate::side_source("shop.web.lead.side-1a2b3c"),
            Some("shop.web.lead")
        );
        assert_eq!(crate::side_source("shop.side-panel"), None);
        assert_eq!(crate::side_source("shop.side-"), None);
        assert_eq!(crate::side_source("shop.side-1A2B"), None);
        let copy = json!({"name": "shop.web.lead.side-1a2b3c", "created_by": "shop.web.lead",
            "expire_ms": 900_000});
        assert_eq!(crate::side_copy(&copy), Some("shop.web.lead"));
        // `feed` is hex: a task under that name is still its own caller.
        let task = json!({"name": "shop.side-feed", "created_by": "shop.lead"});
        assert_eq!(crate::side_copy(&task), None);
        let named = json!({"name": "shop.side-feed", "created_by": "shop", "expire_ms": null});
        assert_eq!(crate::side_copy(&named), None);
    }

    #[test]
    fn an_answer_fits_the_daemon_s_bound_and_its_code_is_one_of_ours() {
        let long = "é".repeat(OUTPUT_BYTES);
        assert!(cut(long).len() <= OUTPUT_BYTES + 6);
        assert_eq!(cut("short".into()), "short");
        assert_eq!(code("task_not_started"), "task_not_started");
        assert_eq!(code("tasks_exist"), "tasks_exist");
        assert_eq!(code("Anything Else"), "tool_failed");
    }
}
