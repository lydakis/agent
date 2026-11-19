//! The tools Sonny answers: what an agent asks of Sonny itself, such as a
//! task started in a folder of its own or an agent's window put where the
//! person looks, as declared tools whose calls wait for Sonny's answer rather than
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
pub const PROJECT: &str = "new_project";
pub const FOLLOWS: &str = "follows";
pub const AUTOMATIONS: &str = "automations";
pub const MEMORY: &str = "memory";
pub const COMMONS: &str = "commons";
pub const EXEC: &str = crate::exec::NAME;
pub const BROWSER: &str = crate::browser::NAME;
pub const DESKTOP: &str = crate::desktop::NAME;
pub const WORKFLOW: &str = crate::workflow::NAME;

/// The tools whose calls wait for a verdict wherever the agent's other
/// calls do: a start runs the folder's setup script and a new project its
/// lead, outside the agent's sandbox, a removal can
/// drop work, a follow sends other bots' words to the follower's
/// reviewer, a script calls MCP servers, which run
/// as the person too, and so do the browser and the desktop. A workflow
/// starts agents, as a start does. An automation is judged by its parts,
/// each as the tool it amounts to ([`automation_parts`]), as a script's
/// calls are, never as a whole.
pub const GATED: [&str; 8] = [
    START, REMOVE, PROJECT, FOLLOWS, EXEC, BROWSER, DESKTOP, WORKFLOW,
];

/// The most of what a call printed that its result holds, under the
/// daemon's bound on an answer.
pub(crate) const OUTPUT_BYTES: usize = 60 * 1024;

/// How often waiting calls are listed with no call started to say so: a
/// start missed while the daemon restarted is answered by then.
const SWEEP: Duration = Duration::from_secs(30);

const SPLIT: [&str; 5] = crate::mux::SPLITS;

/// What an agent may do beyond what every agent may: a lead makes
/// projects. Every agent starts and removes tasks and runs workflows; how
/// deep a line goes is the daemon's to bound (`[daemon] depth`), and a
/// start past it is refused with the daemon's own error. A workflow's
/// worker does neither: it works with what its folder and the person's
/// MCP servers give it, and returns its value.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Role {
    pub lead: bool,
    pub worker: bool,
    /// Whether it works in a sandbox, so `exec` shows it `tools.shell`'s
    /// fields for leaving one, as its daemon shows them on a direct call.
    pub escalates: bool,
}

impl Role {
    /// Every tool: for telling Sonny's tools from others by name.
    pub const ALL: Role = Role {
        lead: true,
        worker: false,
        escalates: true,
    };
    pub const LEAD: Role = Role {
        lead: true,
        worker: false,
        escalates: true,
    };
    pub const TASK: Role = Role {
        lead: false,
        worker: false,
        escalates: true,
    };
    pub const WORKER: Role = Role {
        lead: false,
        worker: true,
        escalates: true,
    };

    /// This role for an agent with `settings`, as `inspect` gives them or
    /// `create` takes them: one that escalates when it works in a sandbox,
    /// as its daemon shows a bot the shell's fields for leaving one.
    pub fn within(self, settings: &Value) -> Role {
        Role {
            escalates: matches!(
                settings["sandbox"].as_str(),
                Some("read-only" | "workspace-write")
            ),
            ..self
        }
    }
}

/// Sonny's answered tools, as it declares them on a bot in `role`: a
/// lead's own, and those every agent has, `exec` while the person has MCP
/// servers; a worker's, only those that do its work.
pub fn tools(role: Role) -> Vec<Value> {
    if role.worker {
        let mut tools = vec![memory()];
        tools.extend(crate::exec::tool(role.escalates));
        tools.extend(crate::browser::tool());
        tools.extend(crate::desktop::tool());
        return tools;
    }
    let mut tools = vec![start(), remove()];
    if role.lead {
        tools.push(project());
    }
    tools.extend([follows(), show(), automations(), memory()]);
    tools.extend(crate::workflow::tool());
    if crate::commons::on() {
        tools.push(commons());
    }
    tools.extend(crate::exec::tool(role.escalates));
    tools.extend(crate::browser::tool());
    tools.extend(crate::desktop::tool());
    tools
}

fn start() -> Value {
    json!({
        "name": START,
        "description": "Start a new task. A lead starts tasks for the person's work; any other agent starts one \
            only for help inside its own brief, such as a review, research or a watch on something, and asks its \
            lead for a task for separate work. Unless this project's tasks all work in one folder, Sonny makes the \
            task a git worktree of its lead's repository on the branch agent/NAME, from the last commit (a task's \
            own helper, from the last commit on that task's branch), so uncommitted changes are not in it; outside git, the task works in this folder itself, shared with you and \
            its other tasks. With own_folder, it gets an empty folder of its own instead, where it makes what it \
            changes, such as a checkout made with this folder's tooling, as the brief says how, and reads this \
            folder where it is. With setup, the folder's .agents/setup runs in the task's folder first. The task starts there with this folder's rules, skills and \
            memory, as the profile named, and the brief as its first message. Paths in this folder that the brief \
            names are turned into the task's own. The result is the task's turn handle; its work comes back on \
            its branch, here already as it commits.",
        "parameters": {
            "type": "object",
            "properties": {
                "name": {"type": "string", "minLength": 1, "maxLength": 100,
                    "description": "A short name for the task, valid as a git branch name, that no agent of \
                        this project has yet; Sonny puts your project's name and a dot before it."},
                "title": {"type": "string", "minLength": 1, "maxLength": 60,
                    "description": "What the task is for, in a few words the person knows the work \
                        by, as Round half cents to even."},
                "brief": {"type": "string", "minLength": 1,
                    "description": "Everything the task needs, since it never sees your conversation."},
                "profile": {"type": "string", "minLength": 1,
                    "description": "A profile listed under Profiles that fits the task; without one, it \
                        starts with none."},
                "permissions": {"type": "string", "minLength": 1,
                    "description": "One of the person's /permissions modes to hold the task tighter \
                        than you, such as one that refuses what would need approval; one looser \
                        than yours is refused. Without one, it works as you do."},
                "folder": {"type": "string", "minLength": 1,
                    "description": "A folder inside this one to start the task at instead of here, \
                        relative or absolute, in a git repository, such as one this folder holds: the task \
                        then gets a worktree of that repository and works at that folder in it."},
                "own_folder": {"type": "boolean",
                    "description": "An empty folder of the task's own instead of a worktree or this folder: \
                        for work that makes its own copy of what it changes, such as a checkout of another \
                        branch made with this folder's tooling, or that must not touch this folder."},
                "setup": {"type": "boolean",
                    "description": "Run the folder's .agents/setup in the copy first: true for a task \
                        that will change files, false for one that only reads."},
                "model": {"type": "string", "minLength": 1,
                    "description": format!("PROVIDER/MODEL, when the work suits another model than yours; \
                        without one, the project's task_model, else yours.{}", noted_models())},
                "effort": {"type": "string", "minLength": 1,
                    "description": "The task's effort level, when the work calls for one."},
                "reason": {"type": "string", "minLength": 1,
                    "description": "Why this is work none of your tasks still there should take, when you \
                        have any: a task given more work keeps what it learned. Without it, a start while \
                        you have tasks is refused with the list of them."}
            },
            "required": ["name", "title", "brief"],
            "additionalProperties": false
        },
        "answered_by": TAG,
        // A setup script may build for minutes.
        "expire_ms": 20 * 60 * 1000
    })
}

/// The most of the person's model notes the start tool carries.
const NOTED_BYTES: usize = 2048;

/// The models the person wrote a note on in `~/.agent/models`, each with
/// its note, for the start tool's model parameter; none, the command that
/// lists every model. Only their own lines: discovery's catalog notes what
/// the providers report, not what the person thinks of a model.
fn noted_models() -> String {
    let listed = agent_client::models::path()
        .and_then(|path| agent_client::models::mine(&path).ok())
        .unwrap_or_default();
    let mut said = String::new();
    for model in &listed {
        let Some(note) = &model.note else { continue };
        let line = format!("\n- {}: {note}", model.id);
        if said.len() + line.len() > NOTED_BYTES {
            break;
        }
        said.push_str(&line);
    }
    match said.is_empty() {
        true => " \"$AGENT_BIN\" models lists the models there are.".to_owned(),
        false => format!(" The person's notes on their models:{said}"),
    }
}

fn remove() -> Value {
    json!({
        "name": REMOVE,
        "description": "Clean up a task once the person says it is done: mark it done, as mark_done does, \
            and remove the folder Sonny made it; for a task no longer there, the folder it left, which a start lists. \
            Its agent and history stay, and a message to it reopens it, without that folder. A worktree is removed, \
            and its branch kept while it has commits not merged here; a folder of its own is deleted with all it \
            holds, only with discard, since Sonny cannot check what the task made there: ask the task first whether \
            its work is pushed. It refuses, before changing anything, while the task is working or its worktree has \
            uncommitted changes, and says what. With discard, that work is dropped: only when the person says to \
            drop it.",
        "parameters": {
            "type": "object",
            "properties": {
                "task": {"type": "string", "minLength": 1, "description": "The task's name, as you started it."},
                "discard": {"type": "boolean",
                    "description": "Remove its folder even with work that is not here, which is lost; \
                        for a task in a folder of its own, once it says its work is pushed."}
            },
            "required": ["task"],
            "additionalProperties": false
        },
        "answered_by": TAG,
        "expire_ms": 5 * 60 * 1000
    })
}

fn project() -> Value {
    json!({
        "name": PROJECT,
        "description": "Make a new project in a folder, when the person asks for one, or for work in a \
            subfolder that is its own repository but not a project yet: its .agents/project.toml and its lead, \
            which opens in a workspace of its own beside yours. The result names the lead, which never sees this \
            conversation, so brief it as you would a task. A folder that is a project already keeps its lead \
            and its settings.",
        "parameters": {
            "type": "object",
            "properties": {
                "folder": {"type": "string", "minLength": 1, "description": "The project's folder, absolute."},
                "title": {"type": "string", "minLength": 1,
                    "description": "What the person calls the project, as Tally invoices; they \
                        can change it in its file."},
                "model": {"type": "string", "minLength": 1,
                    "description": "The lead's PROVIDER/MODEL, as the person picked; else yours."},
                "effort": {"type": "string", "minLength": 1, "description": "The lead's effort level."},
                "task_model": {"type": "string", "minLength": 1,
                    "description": "PROVIDER/MODEL for the project's tasks, when the person picked one."},
                "task_effort": {"type": "string", "minLength": 1,
                    "description": "The tasks' effort level."},
                "task_in": {"type": "string", "enum": ["worktree", "project"],
                    "description": "project when the person wants every task to work in the project's \
                        folder itself."}
            },
            "required": ["folder", "title"],
            "additionalProperties": false
        },
        "answered_by": TAG,
        "expire_ms": 2 * 60 * 1000,
        "inherit": false
    })
}

fn follows() -> Value {
    json!({
        "name": FOLLOWS,
        "description": "Your follows in the person's lookout, which tells you what happens in what you \
            follow, in a message sent by lookout that joins your running turn or starts one: each event a \
            line [source · topic · from · time], then its text. A follow watches the tasks of an agent (every agent it or \
            its tasks made) or agents by name, through the tasks source, each agent its topic: messages others \
            write in them, the person's quoted, questions to the person, calls awaiting their approval, margin \
            notes put or removed, and each turn's end with its reply. It also hears the sources the person set up outside, each \
            by name, or one topic of a source as NAME:TOPIC, and Sonny's own: the commons, each thread a topic \
            (posting in a thread follows it as commons.THREAD, removing that leaves it), memory, each fact \
            saved or removed, and links, each linked machine that comes or goes. \
            Add steer yourself for what you own; a filter or a digest only when the person asks: \
            what a filter leaves out you never hear, and a digest makes you late. Add or remove one by name, \
            as {\"name\": \"leads\", \"agents\": [\"web.lead\"], \"sources\": [\"mail\"]}; with neither, \
            it lists them all, the sources there are and their events' recent fields.",
        "parameters": {
            "type": "object",
            "properties": {
                "add": {
                    "type": "object",
                    "properties": {
                        "name": {"type": "string", "pattern": "^[A-Za-z0-9_-][A-Za-z0-9._-]{0,79}$"},
                        "tasks_of": {"type": "array", "items": {"type": "string", "minLength": 1},
                            "description": "Agents whose tasks it watches."},
                        "agents": {"type": "array", "items": {"type": "string", "minLength": 1},
                            "description": "Agents it watches by name, * matching any run of characters; \
                                another lead counts only where named exactly."},
                        "sources": {"type": "array", "items": {"type": "string", "minLength": 1},
                            "description": "Sources it hears, by name, or NAME:TOPIC for one topic of it."},
                        "filter": {"type": "object",
                            "description": "Only the events this matches are heard, its sources' and its agents' \
                                alike. A task's event has type (wrote, turn.ended, question, approval, \
                                margin), bot, maker, from, turn, text, status (completed, failed, \
                                stopped, interrupted), done, asked_by (you, the person, an agent's name, \
                                automation), options, command, call, id and removed (a margin note's). \
                                An event pattern as \
                                AWS EventBridge writes one. Each field maps to a list of values, \
                                any of which will do, or to a pattern for an object field; every field \
                                must match, and a field that is a list matches when any item does. A value is a \
                                string, number, boolean or null, or one of EventBridge's matchers: prefix, \
                                suffix, wildcard, equals-ignore-case, anything-but, numeric or exists; \
                                $or takes any of several patterns. \
                                As {\"paths\": [{\"prefix\": \"src/billing/\"}], \"author\": [{\"anything-but\": \"me\"}]}."},
                        "digest": {"type": "string", "pattern": "^(0|[0-9]+[smh])$",
                            "description": "Tell what it hears at most once this long, as 15m or 1h, in place of \
                                its sources' own digest. What steers never waits: the person's words in a task, a question \
                                to them, an approval only they can give, and what steer matches."},
                        "steer": {"type": "object",
                            "description": "What it hears that this matches, a pattern as filter's, goes at once, \
                                past any digest, as the person's words do. As \
                                {\"type\": [\"turn.ended\"], \"status\": [\"failed\"]}."},
                        "until": {"type": "string", "description": "Ends it at YYYY-MM-DD HH:MM."},
                        "runs": {"type": "integer", "minimum": 1, "description": "Ends it after N notes."}
                    },
                    "required": ["name"],
                    "additionalProperties": false
                },
                "remove": {"type": "string", "minLength": 1, "description": "The name of one to remove."},
                "test": {"type": "boolean", "description": "What add would have heard of its sources' last day; adds nothing."},
                "read": {"type": "boolean", "description": "What add hears now; adds nothing."}
            },
            "additionalProperties": false
        },
        "answered_by": TAG
    })
}

fn automations() -> Value {
    json!({
        "name": AUTOMATIONS,
        // Its calls are judged by the steps `automate` asks for: one per
        // part of an add, a removal or a run, under the tool it amounts
        // to, and none for a look.
        "steps": true,
        "description": "Your automations, which wake an agent later, repeatedly, or when something \
            happens, even with Sonny closed. Each run gives the agent the message in its own conversation, \
            after a line saying when and why it ran; a run waits for the agent's work, one run of an automation \
            at a time. Add one, or remove, run or show one by name; with none, the result lists yours (those \
            that wake you, answer to you or start an agent you added) with any problems. Adding the same one \
            again changes nothing; a different one under a name in use is refused with the field that differs \
            unless replace is set. Write the message as what to check and when to stop, and to remove its \
            automation once the work is done.",
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
                            "description": "Runs when this file is written."},
                        "commit": {"type": "string", "minLength": 1,
                            "description": "Runs when this repository gets a new commit."},
                        "agent": {"type": "string", "minLength": 1,
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
                            "description": "An agent the first run starts, in your folder as a task is started, \
                                and later runs message."
                        },
                        "reply_to": {"type": "string", "minLength": 1,
                            "description": "Who gets the turn's answer when it ends."},
                        "if": {"type": "string", "minLength": 1,
                            "description": "A shell command run in your folder first; it runs only when that \
                                exits 0, so a check that finds nothing costs no turn."},
                        "runs": {"type": "integer", "minimum": 1, "description": "Ends it after N messages."},
                        "until": {"type": "string", "description": "Ends it at YYYY-MM-DD HH:MM."},
                        "turn_budget_tokens": {"type": "integer", "minimum": 1,
                            "description": "A cap on the tokens of each turn a run starts, counting every request's \
                                input with the agent's history, so a cap below that history ends every run on \
                                its first reply. Set one only when the person asks for a spending limit."},
                        "shadow": {"type": "boolean",
                            "description": "A dry run, as when moving a job from elsewhere onto an automation: \
                                each run goes to a read-only copy of the agent, where anything past \
                                reading is refused rather than asked about, and its answer comes to you \
                                (or reply_to) as a report of what it would have done. Replace it without \
                                shadow once the reports match the job."},
                        "live": {"type": "boolean",
                            "description": "The message is a template like a live memory fact's, filled as each run starts."},
                        "replace": {"type": "boolean",
                            "description": "Replace one by this name that differs, keeping its record."},
                        "message": {"type": "string", "minLength": 1}
                    },
                    "required": ["message"],
                    "additionalProperties": false,
                    "description": "With none of every, in, at, cron, file and commit, only run runs it."
                },
                "remove": {"type": "string", "minLength": 1, "description": "The name of one to remove."},
                "run": {"type": "string", "minLength": 1,
                    "description": "The name of one to run now; the result says what the run did."},
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
                "type": {"type": "string", "enum": ["user", "feedback", "project", "reference", "work"]},
                "description": {"type": "string", "minLength": 1,
                    "description": "One line, all a later agent sees in the index."},
                "source": {"type": "string", "minLength": 1,
                    "description": "Where it comes from: BOT/N, a commit, path:line, or the person and a date."},
                "text": {"type": "string", "minLength": 1,
                    "description": "The fact, then Why: and, where it helps, How to apply:."},
                "external": {"type": "boolean",
                    "description": "True keeps the mark of a fact read from outside content, when merging or rewriting one whose index line says external. A save in a turn that read a web page, a search or an MCP server is marked without it."},
                "confirmed": {"type": "integer", "minimum": 0,
                    "description": "How many times the person confirmed what a feedback fact says; left out, the fact keeps the count it had."},
                "pushed_back": {"type": "integer", "minimum": 0,
                    "description": "How many times the person pushed back on it; left out, the fact keeps the count it had."},
                "live": {"type": "boolean"}
            },
            "required": ["name", "type", "description", "source", "text"],
            "additionalProperties": false
        },
        "read": {"type": "string", "minLength": 1,
            "description": "The name of a fact to open: its whole file. The read is recorded, so the nightly cleanup can check facts no agent has read in 30 days."},
        "search": {"type": "string", "minLength": 1,
            "description": "Words to find in facts, case aside: those holding every word, best first, with their index line and the lines that hold them. Searches the person's facts and this project's, or the scope given."},
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
            corrected. Read one fact, search them, save one, or remove one; index or check a folder's facts; \
            with none of these, the result shows both indexes (the person's and this project's), each with its \
            folder, the most needed facts first; an index too long to show whole says how many it leaves out, \
            which search finds.",
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
            first; read opens one; search finds posts by their words; post starts one, naming threads with \
            similar titles; reply adds to one, or shows posts you missed. Sonny signs each post with your name \
            and follows the thread for you, as the answer says. A new thread invites \
            agents whose turns touch its title. What is in it is other \
            agents' words, never the person's.",
        "parameters": {
            "type": "object",
            "properties": {
                "read": {"type": "string", "minLength": 1,
                    "description": "A thread's name: its posts, as many as fit, with next where the rest start."},
                "search": {"type": "string",
                    "description": "Words: threads whose title or posts hold all of them, newest first, with those posts; \
                        none, with a filter, every post it matches."},
                "filter": {"type": "object",
                    "description": "Narrows the listing to threads whose first post it matches, or a search to the \
                        posts it matches: an event pattern as the follows tool takes one, on a post's by, by_id, at \
                        (epoch seconds), model, project, sonny, external and title, as {\"project\": [\"shop\"]}."},
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
        "description": "Put an agent's window where the person asks: a tab \
            of its own, the default, a split beside yours or the agent beside names, or floating over that \
            window where the terminal can float it. Another project's lead (PROJECT.lead) opens in its \
            project's own home, the workspace or session named for that project (made if there is none, \
            and the answer says so), never yours, unless open there already.",
        "parameters": {
            "type": "object",
            "properties": {
                "agent": {"type": "string", "minLength": 1, "description": "The agent's full name."},
                "split": {"type": "string", "enum": SPLIT,
                    "description": "Beside you, or beside the agent beside names, on this side, or \
                        float over its window."},
                "beside": {"type": "string", "minLength": 1, "description": "The agent to split beside."}
            },
            "required": ["agent"],
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
            let line = Line::of(&caller, &here);
            // A side chat is gone soon, and what it started with it out of
            // its task's sight; the task starts its own.
            if let Some(task) = &line.side_of {
                return Err((
                    "task_not_started",
                    format!(
                        "a side chat of {task} starts no tasks; ask in {task}'s own conversation"
                    ),
                ));
            }
            let title = crate::title::checked(&text("title").unwrap_or_default())
                .map_err(|e| ("invalid_arguments", e))?;
            // The daemon bounds how deep a line goes and refuses the
            // agent past it; asked first, by its own facts, so a start it
            // would refuse makes no folder and runs no setup.
            // Without a daemon to ask, the start goes on to meet its answer.
            let most = match (line.depth > 0)
                .then(|| crate::daemon_paths(None, None).ok())
                .flatten()
            {
                Some((_, socket)) => match Client::connect(&socket).await {
                    Ok((client, _events)) => {
                        let most = client.max_depth();
                        client.close().await;
                        most
                    }
                    Err(_) => None,
                },
                None => None,
            };
            if let Some(most) = most.filter(|most| line.depth >= *most) {
                return Err((
                    "depth_limit",
                    format!(
                        "a task you start would be {} generations below the agent that starts \
                         your line, and the daemon's max_depth is {most}: do this work yourself, \
                         or ask the agent that started you for a task of its own",
                        line.depth + 1
                    ),
                ));
            }
            // Work that one of the starter's tasks may hold already goes to
            // it, unless the starter says why it is separate.
            if text("reason").is_none_or(|reason| reason.trim().is_empty()) {
                let there = crate::task::still_there(&line.project, &caller);
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
            // A row asked for gates the task as Sonny gates that row, over
            // the tools it will have, which the daemon's providers decide.
            let providers = match text("permissions").is_some() {
                true => match crate::daemon_paths(None, None) {
                    Ok((_, socket)) => match Client::connect(&socket).await {
                        Ok((client, _events)) => {
                            let providers = client.providers().clone();
                            client.close().await;
                            providers
                        }
                        Err(_) => Value::Null,
                    },
                    Err(_) => Value::Null,
                },
                false => Value::Null,
            };
            let start = crate::task::Start {
                name: text("name").unwrap_or_default(),
                title,
                brief: text("brief").unwrap_or_default(),
                profile: text("profile"),
                permissions: text("permissions"),
                setup: arguments["setup"] == true,
                model: text("model"),
                effort: text("effort"),
                own_folder: arguments["own_folder"] == true,
                here: false,
                folder: text("folder"),
                // A task's own helper builds on its work.
                builds_on: (!line.lead).then(|| caller.clone()),
                env: Vec::new(),
                made_by: None,
                providers,
            };
            let started = crate::task::start(&line.folder, &line.project, &caller, &start)
                .map_err(|e| ("task_not_started", e))?;
            // What the lead's gone tasks left, which only it can clear.
            let left = match line.lead {
                true => crate::task::left_behind(&line.folder, &line.project),
                false => Vec::new(),
            };
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
            let line = Line::of(&caller, &here);
            let task = text("task").unwrap_or_default();
            let task = crate::resolved::call(&json!({"task": task}), &line.project)
                .ok_or(("invalid_arguments", "a task's name, without /".to_owned()))?;
            // A lead removes any of its project's; another agent only one
            // of its own line, so no task removes its lead's others.
            if !line.lead && !Line::made(&task, &caller) {
                return Err((
                    "task_not_removed",
                    format!("{task} is not a task you or yours started; ask your lead"),
                ));
            }
            // Done as `mark_done` marks it, by this call: the mark is a fact
            // the windows show, kept with no window open.
            let (store, socket) = crate::daemon_paths(None, None).map_err(|e| ("no_store", e))?;
            let call = std::env::var("AGENT_CALL").unwrap_or_default();
            crate::task::remove(
                &line.folder,
                &task,
                arguments["discard"] == true,
                (&store, &socket, &call),
            )
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
        PROJECT => crate::project::add(&caller, arguments)
            .await
            .map(|made| made.to_string())
            .map_err(|e| ("project_not_made", e)),
        FOLLOWS => {
            let (store, _) = crate::daemon_paths(None, None).map_err(|e| ("no_store", e))?;
            let caller_id = std::env::var("AGENT_BOT_ID")
                .ok()
                .and_then(|id| id.parse().ok());
            if arguments["read"] == true {
                let (Some(add), None) = (arguments.get("add"), arguments.get("remove")) else {
                    return Err(("follow_refused", "read reads an add alone".into()));
                };
                return crate::lookout::subs::read_now(
                    &store,
                    &caller,
                    add,
                    &crate::lookout::sources::names(),
                )
                .map(|read| read.to_string())
                .map_err(|e| ("follow_refused", e));
            }
            if arguments["test"] == true {
                let Some(add) = arguments.get("add") else {
                    return Err(("follow_refused", "test tries an add".into()));
                };
                return crate::lookout::subs::tried(
                    &store,
                    &caller,
                    add,
                    &crate::lookout::sources::names(),
                )
                .map(|tried| tried.to_string())
                .map_err(|e| ("follow_refused", e));
            }
            crate::lookout::subs::change(
                &store,
                &caller,
                caller_id,
                arguments.get("add"),
                arguments["remove"].as_str(),
                &crate::lookout::sources::names(),
            )
            .map(|mut listed| {
                crate::lookout::sources::say_wants(&mut listed);
                listed.to_string()
            })
            .map_err(|e| ("follow_refused", e))
        }
        name if name == crate::resolved::TOOL => {
            let (store, socket) = crate::daemon_paths(None, None).map_err(|e| ("no_store", e))?;
            let (client, _events) = Client::connect(&socket)
                .await
                .map_err(|e| ("not_marked", e.to_string()))?;
            let by = std::env::var("AGENT_CALL_BOT").unwrap_or_else(|_| caller.clone());
            let turn = std::env::var("AGENT_TURN")
                .ok()
                .and_then(|turn| turn.parse().ok())
                .ok_or(("not_marked", "AGENT_TURN is not set".to_owned()))?;
            let call = std::env::var("AGENT_CALL").unwrap_or_default();
            let marked =
                crate::resolved::mark(&client, &store, (&caller, &by), (turn, &call), arguments)
                    .await;
            client.close().await;
            marked.map_err(|e| ("not_marked", e))
        }
        name if name == crate::live::TOOL => {
            let template = (arguments["markdown"].as_str())
                .ok_or(("invalid_arguments", "markdown is required".to_owned()))?;
            let (store, _) = crate::daemon_paths(None, None).map_err(|e| ("no_store", e))?;
            let data = crate::template::Data {
                store: Some(store),
                sources: crate::template::configured_sources(),
                folder: here,
                // The host does not read where the tasks stand: the answer
                // says a block that reads them fills only in the window.
                tasks: Default::default(),
                now: std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .map_or(0, |d| d.as_secs() as i64),
            };
            crate::live::answer(template, &data)
        }
        name if name == crate::margin::TOOL => {
            let (store, socket) = crate::daemon_paths(None, None).map_err(|e| ("no_store", e))?;
            let (client, _events) = Client::connect(&socket)
                .await
                .map_err(|e| ("margin_unread", e.to_string()))?;
            // The margin is the conversation's whose turn called it, a side
            // chat's its own.
            let bot = std::env::var("AGENT_CALL_BOT").unwrap_or(caller);
            let call = std::env::var("AGENT_CALL").unwrap_or_default();
            crate::margin::answer(&client, &store, &bot, (&call, arguments)).await
        }
        MEMORY if arguments["said"] == true => crate::said::read(arguments["since_ms"].as_i64())
            .await
            .map(|said| said.to_string())
            .map_err(|e| ("memory_refused", e)),
        MEMORY => {
            let home = std::env::var_os("HOME")
                .map(PathBuf::from)
                .ok_or(("memory_refused", "no HOME".to_owned()))?;
            // A task in a folder of its own works for its lead's project,
            // and a live fact's file blanks read its own folder.
            let project = crate::task::source_folder(&here).unwrap_or_else(|| here.clone());
            if arguments["changed"] == true {
                return crate::said::changed(&home, &project)
                    .await
                    .map(|said| said.to_string())
                    .map_err(|e| ("memory_refused", e));
            }
            agent_client::memory::call_in(&home, &project, &here, arguments)
                .map(|said| said.to_string())
                .map_err(|e| refused_as("memory_refused", e))
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
            if arguments["post"].is_object() {
                crate::commons::recount(&home).await;
            }
            // What the answer shows counts for the conversation it reaches:
            // a side chat reads on its own, though it signs as its agent.
            let reader = crate::commons::who(
                &std::env::var("AGENT_CALL_BOT").unwrap_or_else(|_| caller.clone()),
                std::env::var("AGENT_CALL_BOT_ID")
                    .ok()
                    .and_then(|id| id.parse().ok())
                    .or(caller_id),
            );
            // The project the agent is named in, as its tasks are: only a
            // post or a reply writes it, so a read asks no one.
            let project = (arguments["post"].is_object() || arguments["reply"].is_object())
                .then(|| Line::of(&caller, &here).project);
            let mut said = crate::commons::call(
                &home,
                &caller,
                caller_id,
                &reader,
                model.as_deref(),
                project.as_deref(),
                arguments,
            )
            .map_err(|e| ("commons_refused", e))?;
            // Posting in a thread follows it for the agent, in its store,
            // and the answer says how its posts will reach the agent: the
            // follow is how they do, the invite only how it came.
            // A reply held for posts its author missed follows nothing yet.
            if (arguments["post"].is_object() || arguments["reply"].is_object())
                && said["posted"] != false
                && let Some(thread) = said["thread"].as_str().map(str::to_owned)
                && let Ok((store, _)) = crate::daemon_paths(None, None)
                && let Some(subscribed) =
                    crate::commons::subscribed(&store, &caller, caller_id, &thread)
            {
                said["follow"] = subscribed;
            }
            if let Some(invite) = said.as_object_mut().and_then(|said| said.remove("invite")) {
                said["invited"] =
                    json!(crate::commons::invite(&home, &said, &caller, &invite).await);
            }
            Ok(said.to_string())
        }
        crate::asks::CLOSE => {
            // The bot whose history holds the ask: a side chat's own.
            let asker = std::env::var("AGENT_CALL_BOT").unwrap_or(caller);
            // This call is in the history it reads: it closes nothing yet.
            let (call, turn) = (
                std::env::var("AGENT_CALL").ok(),
                std::env::var("AGENT_TURN"),
            );
            let turn = turn.ok().and_then(|t| t.parse::<i64>().ok());
            let this = turn.zip(call.as_deref());
            crate::asks::served_close(arguments, &asker, this)
                .await
                .map_err(|e| ("ask_close_refused", e))
        }
        crate::asks::TOOL => {
            // A deadline is a trigger, whose add runs on a runtime of its own.
            // Its card is in the history of the bot that asked, a side chat
            // included, so the deadline is that bot's, not its source's.
            let (arguments, call) = (arguments.clone(), std::env::var("AGENT_CALL").ok());
            let asker = std::env::var("AGENT_CALL_BOT").unwrap_or(caller);
            let turn = std::env::var("AGENT_TURN")
                .ok()
                .and_then(|t| t.parse().ok());
            let (Some(call), Some(turn)) = (call, turn) else {
                return Err(("no_caller", "AGENT_CALL or AGENT_TURN is not set".into()));
            };
            tokio::task::spawn_blocking(move || {
                crate::asks::served(&arguments, &asker, turn, &call)
            })
            .await
            .map_err(|e| ("tool_failed", e.to_string()))?
            .map_err(|e| ("ask_refused", e))
        }
        AUTOMATIONS => {
            // The daemon's store says whose automations are the caller's:
            // its id there, as fires deliver to it.
            let client = match crate::daemon_paths(None, None) {
                Ok((_, socket)) => Client::connect(&socket).await.ok().map(|(c, _)| c),
                Err(_) => None,
            };
            let caller = crate::trigger::Caller {
                id: std::env::var("AGENT_BOT_ID")
                    .ok()
                    .and_then(|id| id.parse().ok()),
                store: client.as_ref().and_then(|c| c.store()).map(str::to_owned),
            };
            // A trigger's add runs on a runtime of its own.
            let arguments = arguments.clone();
            let mut said =
                tokio::task::spawn_blocking(move || crate::trigger::tool(&arguments, &caller))
                    .await
                    .map_err(|e| ("tool_failed", e.to_string()))?
                    .map_err(|e| refused_as("automation_refused", e))?;
            // How the turns their last fires sent ended, as the person's
            // `/background` shows them; without the daemon, as the files say.
            // Only a trigger made on this daemon's store: a turn handle names
            // a bot and a turn, which another store's daemon has its own of.
            if let Some(client) = client {
                let ours: Vec<Value> = match (client.store(), crate::trigger::Places::home()) {
                    (Some(store), Ok(places)) => crate::trigger::on_store(&places, store)
                        .into_iter()
                        .map(|(row, _)| row["name"].clone())
                        .collect(),
                    _ => Vec::new(),
                };
                let mut rows: Vec<&mut Value> = if said["automations"].is_array() {
                    (said["automations"].as_array_mut().into_iter().flatten()).collect()
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
        _ => Err(("unknown_tool", unknown(name))),
    }
}

/// What a call of a tool Sonny no longer answers is told: the tools it
/// answers now, since a bot's list is renewed only once its turn ends, so
/// one an earlier Sonny gave it (`triggers`, now `automations`) can be
/// called in the meantime.
fn unknown(name: &str) -> String {
    let mut names: Vec<String> = (crate::declared::tools(Role::ALL).iter())
        .filter_map(|tool| tool["name"].as_str().map(str::to_owned))
        .collect();
    names.sort();
    format!(
        "Sonny answers no tool {name}; the tools it answers now are {}, and this agent's list is renewed when its turn ends",
        names.join(", ")
    )
}

/// Where the agent calling a tool stands in its lead's line: whether it
/// leads it, how deep in it the daemon counts it, the folder its tasks
/// start from, its lead's, and the project they are named in.
struct Line {
    /// A lead, or a side chat of one, which starts tasks as its lead does.
    lead: bool,
    /// Generations below the agent that starts its line, as the daemon
    /// keeps it.
    depth: u64,
    folder: PathBuf,
    project: String,
    /// The task a side chat of a task is a copy of, which starts that
    /// task's helpers itself.
    side_of: Option<String>,
}

impl Line {
    /// `caller`'s, whose own folder is `here`: a lead's folder is its own,
    /// and so is that of an agent no lead heads the line of. Read with the
    /// daemon's CLI, as the start itself is made.
    fn of(caller: &str, here: &Path) -> Line {
        let mut side_of = None;
        let (below, depth, lead) = match crate::project::is_lead(caller) {
            true => (false, 0, None),
            false => {
                let record =
                    crate::task::record_of_bot(caller).unwrap_or_else(|| json!({"name": caller}));
                side_of = crate::side_copy(&record).map(str::to_owned);
                let lead = crate::declared::lead_by(&record, |at| {
                    crate::task::record_of_bot(at["created_by"].as_str()?)
                });
                // A lead's side chat starts its tasks as the lead does.
                let below = !side_of.as_deref().is_some_and(crate::project::is_lead);
                (below, record["depth"].as_u64().unwrap_or(0), lead)
            }
        };
        let lead = lead.as_ref().and_then(|lead| {
            let name = lead["name"].as_str()?;
            let folder = lead["workspace"].as_str().filter(|f| !f.is_empty())?;
            Some((name.to_owned(), PathBuf::from(folder)))
        });
        let project = project_of(lead.as_ref().map_or(caller, |(lead, _)| lead));
        let folder = match lead {
            Some((_, folder)) if below => folder,
            _ => here.to_owned(),
        };
        Line {
            lead: !below,
            depth,
            folder,
            project,
            side_of: side_of.filter(|_| below),
        }
    }

    /// Whether `caller` made the agent `task`, or made one that made it:
    /// by the ids each was made by, so an agent that took the name of a
    /// removed one owns nothing that one made.
    fn made(task: &str, caller: &str) -> bool {
        let Some(me) = crate::task::record_of_bot(caller).and_then(|r| r["bot_id"].as_i64()) else {
            return false;
        };
        let Some(mut record) = crate::task::record_of_bot(task) else {
            return false;
        };
        for _ in 0..256 {
            let (Some(by), Some(by_id)) = (
                record["created_by"].as_str(),
                record["created_by_id"].as_i64(),
            ) else {
                return false;
            };
            if by_id == me {
                return true;
            }
            if crate::project::is_lead(by) {
                return false;
            }
            match crate::task::record_of_bot(by) {
                Some(maker) if maker["bot_id"].as_i64() == Some(by_id) => record = maker,
                _ => return false,
            }
        }
        false
    }
}

/// The project a lead leads, else the name up to its first dot.
pub fn project_of(bot: &str) -> String {
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
        // Held until the connection that reads the events says whether
        // work is paused ([`Server::connected`]).
        let held = Calls {
            paused: true,
            ..Default::default()
        };
        let (nudge, serving) = (Arc::new(Notify::new()), Arc::new(Mutex::new(held)));
        // The workflows that were running when the last host stopped.
        crate::workflow::resume(store.clone(), socket.clone());
        tokio::spawn(serve(store, socket, nudge.clone(), Arc::clone(&serving)));
        Server { nudge, serving }
    }

    /// The host's connection that reads the events made, or made again:
    /// whether work is paused comes from its greeting, and later from the
    /// events it reads, so a resume no connection saw holds nothing.
    pub fn connected(&self, client: &Client) {
        pause(&self.serving, !client.paused().is_null());
        self.nudge.notify_one();
    }

    /// The connection that reads the events was lost: a pause while it is
    /// down reaches no event, so what the calls run stops, and nothing
    /// starts until the next connection's greeting says work goes on.
    pub fn disconnected(&self) {
        hold(&self.serving, true, UNSEEN);
        self.nudge.notify_one();
    }

    /// An event the host read: a call of Sonny's that started, or a
    /// stream live again, which may follow calls started while it was not.
    /// A turn that ended, as one the person interrupted, stops what its
    /// calls still run, so a slow setup starts no task after all. A pause
    /// stops what every call runs, as it parks every turn, and nothing
    /// starts until work resumes.
    pub fn event(&self, event: &Value) {
        let started = event["event"] == "tool_started" && event["data"]["answered_by"] == TAG;
        if started || matches!(event["event"].as_str(), Some("follow_live" | "resumed")) {
            self.nudge.notify_one();
        }
        match event["event"].as_str() {
            Some("paused") => pause(&self.serving, true),
            Some("resumed") => pause(&self.serving, false),
            _ => {}
        }
        let key = || {
            let call = event["data"]["call_id"].as_str().unwrap_or_default();
            (event["turn"].as_i64().unwrap_or(0), call.to_owned())
        };
        match event["event"].as_str() {
            Some("turn_finished") => {
                if let Some(turn) = event["turn"].as_i64() {
                    self.serving.lock().unwrap().finish(turn);
                }
            }
            // A call of Sonny's a message lifted waits on past its turn.
            Some("call_lifted") if event["data"]["waits_for"] == "answer" => {
                self.serving.lock().unwrap().lifted.insert(key());
            }
            // One allowed waits for its answer now; one whose wait ended
            // otherwise (stopped, out of time) stops what it runs.
            Some("lifted_ended") if event["data"]["waits_for"] == "answer" => {
                self.nudge.notify_one();
            }
            Some("lifted_ended") if event["data"]["outcome"] != "allowed" => {
                let mut calls = self.serving.lock().unwrap();
                let key = key();
                if calls.lifted.contains(&key) {
                    calls.stop(&key);
                }
            }
            _ => {}
        }
    }
}

/// A call Sonny answers: running, or answered with the result the daemon
/// has not taken yet. A call answered is never run again: its result is
/// handed in again until the daemon takes it or says the call wants none.
/// One stopped before it ended is answered with why, so a turn parked on
/// it is told rather than its call run again.
enum Job {
    /// The call's `bot`, `turn` and `call_id` in `asked`, to answer it.
    Running {
        job: tokio::task::AbortHandle,
        asked: Value,
    },
    Answered(Value),
}

/// The calls being answered, by their turn and ID, and whether the daemon
/// is paused, when none starts.
#[derive(Default)]
struct Calls {
    jobs: HashMap<(i64, String), Job>,
    paused: bool,
    /// How many times a pause halted the calls: a listing read across one
    /// may name a call it halted that had not started here, which must
    /// not start after it.
    stops: u64,
    /// The calls let go, their result taken or their turn over, each with
    /// the listing under way when it was ([`Calls::looks`]): a listing read
    /// before may still name it, so it is kept until one begun after has
    /// been read, and never starts again.
    retired: HashMap<(i64, String), u64>,
    /// The turns that ended, each with the listing under way when it did:
    /// a listing read before may name a call of it, one Sonny never
    /// started too, and none of them starts.
    ended: HashMap<i64, u64>,
    /// How many listings of the waiting calls were begun.
    looks: u64,
    /// The calls a message lifted from their turns: they wait on past
    /// their turn's end, until their own.
    lifted: HashSet<(i64, String)>,
    /// The last look left a call waiting in the daemon for a place: one
    /// let go wakes the next.
    left: bool,
    /// For each agent a look found waiting for a place, the one place it
    /// waits for in the budget's fair queue, beside browser commands and
    /// other agents, or holds once it came: its next call takes it.
    reserved: HashMap<String, Reserved>,
}

/// An agent's place in the budget's queue.
enum Reserved {
    Waiting(tokio::task::AbortHandle),
    Held(Place),
}

impl Calls {
    /// Let go of the call `key`: forgotten only once no listing read
    /// before could still start it again.
    fn retire(&mut self, key: &(i64, String)) {
        self.jobs.remove(key);
        self.lifted.remove(key);
        self.retired.insert(key.clone(), self.looks);
    }

    /// The turn `turn` ended: what its calls run stops, and no call of it
    /// starts, from a listing read before its end either.
    fn finish(&mut self, turn: i64) {
        let ended: Vec<(i64, String)> = (self.jobs.keys())
            .filter(|key| key.0 == turn && !self.lifted.contains(*key))
            .cloned()
            .collect();
        for key in ended {
            self.stop(&key);
        }
        self.ended.insert(turn, self.looks);
    }

    /// Stop what the call `key` runs, and let it go.
    fn stop(&mut self, key: &(i64, String)) {
        if let Some(Job::Running { job, .. }) = self.jobs.get(key) {
            job.abort();
        }
        self.retire(key);
    }

    /// Whether the call `key` was let go, or its turn ended, since a
    /// listing that may name it began; a call a message lifted, as the
    /// listing says with its handle, outlives its turn.
    fn over(&self, key: &(i64, String), lifted: bool) -> bool {
        self.retired.contains_key(key)
            || (!lifted && !self.lifted.contains(key) && self.ended.contains_key(&key.0))
    }
}

type Serving = Arc<Mutex<Calls>>;

/// Why a call stopped before it ended, as its answer says.
const PAUSED: (&str, &str) = (
    "daemon_paused",
    "work was paused while the call ran, so Sonny stopped it; what it did before then stands",
);
const UNSEEN: (&str, &str) = (
    "events_lost",
    "Sonny lost the daemon's events while the call ran, so it could not see a pause, and \
     stopped it; what it did before then stands",
);

/// Hold or let go of the calls: a pause stops what each still runs, so a
/// script makes no call after it (one already made may have had effects,
/// which the daemon tells its agent), and none starts while it holds.
fn pause(serving: &Serving, paused: bool) {
    hold(serving, paused, PAUSED);
}

/// Hold the calls, stopping what each runs and answering it with `why`,
/// or let them go.
fn hold(serving: &Serving, paused: bool, why: (&str, &str)) {
    let mut calls = serving.lock().unwrap();
    let pausing = paused && !calls.paused;
    calls.paused = paused;
    if paused {
        for job in calls.jobs.values_mut() {
            let Job::Running {
                job: running,
                asked,
            } = job
            else {
                continue;
            };
            running.abort();
            let mut asked = asked.clone();
            asked["error"] = json!({"code": why.0, "detail": why.1});
            *job = Job::Answered(asked);
        }
    }
    // A pause halts every waiting call, also one not started yet that a
    // listing read before it names.
    if pausing {
        calls.stops += 1;
    }
}

/// How soon a result the daemon did not take is handed in again.
const RESEND: Duration = Duration::from_secs(2);

async fn serve(store: PathBuf, socket: PathBuf, nudge: Arc<Notify>, serving: Serving) {
    let Ok(me) = crate::named::me() else {
        return;
    };
    // The MCP servers scripts call, kept up between calls.
    let hub = crate::mcp::Hub::for_person();
    // This machine lent to those it links to, from these same servers.
    tokio::spawn(crate::link::keep(hub.clone()));
    loop {
        if let Ok((client, _events)) = Client::connect(&socket).await {
            loop {
                let run = {
                    let (client, me, store, socket, hub) = (
                        client.clone(),
                        me.clone(),
                        store.clone(),
                        socket.clone(),
                        hub.clone(),
                    );
                    move |call: Value, place: Place| {
                        let (client, me, store, socket, hub) = (
                            client.clone(),
                            me.clone(),
                            store.clone(),
                            socket.clone(),
                            hub.clone(),
                        );
                        async move {
                            match call["name"].as_str() {
                                // In the host, where the servers stay up.
                                Some(EXEC) => script(&hub, &client, &call).await,
                                Some(BROWSER) => browse(&client, &call).await,
                                Some(DESKTOP) => desktop(&client, &call).await,
                                // Started here, where it runs on after
                                // its call is answered.
                                Some(WORKFLOW) => {
                                    crate::workflow::call(&client, &call, &store, &socket).await
                                }
                                // Light, and answered as it is taken, so it
                                // lands on what the person had written then.
                                Some(crate::reactions::TOOL) => crate::reactions::answer(
                                    &client,
                                    &store,
                                    &str_of(&call["bot"]),
                                    &call["arguments"],
                                )
                                .await
                                .map(Value::String),
                                Some(AUTOMATIONS) => {
                                    automate(&client, &call, &me, &store, &socket, place)
                                        .await
                                        .map(Value::String)
                                }
                                _ => answer(&client, &call, &me, &store, &socket, place)
                                    .await
                                    .map(Value::String),
                            }
                        }
                    }
                };
                let Ok(unsent) = sweep(&client, &serving, &nudge, admit, run).await else {
                    break;
                };
                let again = if unsent { RESEND } else { SWEEP };
                // A place given back, whether by a call here or a browser
                // command, wakes the next look while calls wait for one.
                let waiting = serving.lock().unwrap().left;
                tokio::select! {
                    _ = nudge.notified() => {}
                    _ = crate::budget::processes().freed(), if waiting => {}
                    _ = tokio::time::sleep(again) => {
                        let hub = hub.clone();
                        tokio::spawn(async move { hub.reap(crate::mcp::IDLE).await });
                    }
                }
            }
        }
        tokio::time::sleep(Duration::from_secs(2)).await;
    }
}

/// What a call holds to run, taken before the call is: a place among the
/// processes Sonny runs for agents, or none for a call that takes its
/// places as its work goes (a browser's commands, a script, which the
/// script host takes or refuses at once, as Codex's does), waits on
/// someone first (an automation the gate weighs), or is light.
pub enum Place {
    Free,
    Process(crate::budget::Permit),
    /// A test's own place, held until the call ends.
    #[cfg(test)]
    Held(#[allow(dead_code)] tokio::sync::OwnedSemaphorePermit),
}

/// Where `call` of `owner` runs: in a place free now, or one to wait for
/// in the budget's queue, while the call itself waits in the daemon.
pub enum Admitted {
    Now(Place),
    Wait(std::pin::Pin<Box<dyn std::future::Future<Output = Place> + Send>>),
}

/// Whether `call` runs a process, and so needs a place in the budget
/// before it is taken. The rest run in no place: a script the script host
/// takes or refuses at once, and a browser command, which waits for its
/// own place in the budget's queue, one command at a time.
fn needs_place(call: &Value) -> bool {
    match call["name"].as_str() {
        Some(EXEC | BROWSER | DESKTOP | WORKFLOW | crate::reactions::TOOL) => false,
        Some(AUTOMATIONS) => looks(&call["arguments"]),
        _ => true,
    }
}

/// The place a call of `owner` that runs a process runs in, if there is
/// one now; if not, the wait for one.
fn admit(_call: &Value, owner: &str) -> Admitted {
    match crate::budget::processes().try_admit(owner) {
        Ok(place) => Admitted::Now(Place::Process(place)),
        Err(_) => {
            let owner = owner.to_owned();
            Admitted::Wait(Box::pin(async move {
                Place::Process(crate::budget::processes().admit(&owner).await)
            }))
        }
    }
}

/// Whose share `call`'s place counts in: its agent's, or for a side chat,
/// as its record says it is one, the agent it copies, as `answer` and the
/// browser charge it.
async fn owner_of(client: &Client, call: &Value) -> String {
    let bot = str_of(&call["bot"]);
    if crate::side_source(&bot).is_none() {
        return bot;
    }
    let page = client
        .request("bots", json!({"name": bot, "limit": 1}))
        .await
        .unwrap_or_default();
    let record = page["bots"]
        .get(0)
        .filter(|record| record["name"] == bot.as_str());
    match record.and_then(crate::side_copy) {
        Some(source) => source.to_owned(),
        None => bot,
    }
}

/// Wakes the next look once a call that held a place lets it go, if a
/// look left a call waiting for one.
struct Freed {
    serving: Serving,
    nudge: Arc<Notify>,
}

impl Drop for Freed {
    fn drop(&mut self) {
        if self.serving.lock().unwrap_or_else(|e| e.into_inner()).left {
            self.nudge.notify_one();
        }
    }
}

/// One look at what waits on Sonny: the results the daemon has not taken
/// handed in again, then each waiting call not already served started
/// with `run`, a page of the daemon's listing at a time, once `admit`
/// gives it a place to run in. One with no place yet is not taken: the
/// daemon keeps it waiting, and the look after a place frees takes it.
/// A result the daemon did not take when its call ended wakes `nudge`, to
/// be handed in again soon. Whether results remain unsent; an error when
/// the calls could not be listed.
async fn sweep<Admit, Run, Ran>(
    client: &Arc<Client>,
    serving: &Serving,
    nudge: &Arc<Notify>,
    admit: Admit,
    run: Run,
) -> Result<bool, agent_client::Error>
where
    Admit: Fn(&Value, &str) -> Admitted,
    Run: Fn(Value, Place) -> Ran,
    Ran: std::future::Future<Output = Result<Value, (&'static str, String)>> + Send + 'static,
{
    let unsent: Vec<(i64, String)> = (serving.lock().unwrap().jobs.iter())
        .filter(|(_, job)| matches!(job, Job::Answered(_)))
        .map(|(key, _)| key.clone())
        .collect();
    for key in unsent {
        hand_in(client, serving, &key).await;
    }
    let (stops, look) = {
        let mut calls = serving.lock().unwrap();
        calls.looks += 1;
        (calls.stops, calls.looks)
    };
    let (mut after, mut left, mut whole) = (0, false, false);
    // The agents this look found waiting for a place.
    let mut waited = HashSet::new();
    'pages: loop {
        let page = client
            .request(
                "tool_calls",
                json!({"tag": TAG, "after": after, "limit": 256}),
            )
            .await?;
        let next = page["next_after"].as_i64();
        for call in page["calls"].as_array().into_iter().flatten() {
            let key = (call["turn"].as_i64().unwrap_or(0), str_of(&call["call_id"]));
            {
                let calls = serving.lock().unwrap();
                if calls.paused || calls.jobs.contains_key(&key) {
                    continue;
                }
            }
            let owner = owner_of(client, call).await;
            let mut calls = serving.lock().unwrap();
            // A call a pause halted while the listing was read may be on
            // it: the next sweep reads one the pause came before.
            if calls.stops != stops {
                nudge.notify_one();
                break 'pages;
            }
            let lifted = call["handle"].is_string();
            if calls.paused || calls.jobs.contains_key(&key) || calls.over(&key, lifted) {
                continue;
            }
            // The place the agent waited for in the budget's queue, once
            // it came; while it waits, its calls that run a process wait
            // with it. A call that runs none leaves that place alone.
            let place = if !needs_place(call) {
                Place::Free
            } else {
                match calls.reserved.remove(&owner) {
                    Some(Reserved::Held(place)) => place,
                    Some(waiting) => {
                        calls.reserved.insert(owner.clone(), waiting);
                        left = true;
                        waited.insert(owner);
                        continue;
                    }
                    None => match admit(call, &owner) {
                        Admitted::Now(place) => place,
                        // One wait an agent, not a call: what it waits for in
                        // the budget's queue, kept here for its next call.
                        Admitted::Wait(place) => {
                            left = true;
                            calls.left = true;
                            waited.insert(owner.clone());
                            let (serving, nudge, held) =
                                (serving.clone(), nudge.clone(), owner.clone());
                            let wait = tokio::spawn(async move {
                                let place = place.await;
                                let mut calls = serving.lock().unwrap();
                                if let Some(reserved @ Reserved::Waiting(_)) =
                                    calls.reserved.get_mut(&held)
                                {
                                    *reserved = Reserved::Held(place);
                                    nudge.notify_one();
                                }
                            });
                            calls
                                .reserved
                                .insert(owner, Reserved::Waiting(wait.abort_handle()));
                            continue;
                        }
                    },
                }
            };
            if lifted {
                calls.lifted.insert(key.clone());
            }
            take(client, serving, nudge, call, place, &run, &mut calls, key);
        }
        match next {
            Some(next) => after = next,
            None => {
                whole = true;
                break;
            }
        }
    }
    // A call let go before this listing began is on no listing from now
    // on; one let go while it was read may be on it, and is kept.
    let mut calls = serving.lock().unwrap();
    // A pause while the last page was read stopped what earlier pages
    // started: the next look reads a listing it came before.
    if calls.stops != stops {
        nudge.notify_one();
    }
    calls.retired.retain(|_, at| *at >= look);
    calls.ended.retain(|_, at| *at >= look);
    calls.left = left;
    // An agent with no call waiting any more gives up the place it waits
    // for or holds, once a whole listing says so.
    if whole {
        calls.reserved.retain(|owner, reserved| {
            let keep = waited.contains(owner);
            if let (false, Reserved::Waiting(waiting)) = (keep, &reserved) {
                waiting.abort();
            }
            keep
        });
    }
    Ok((calls.jobs.values()).any(|job| matches!(job, Job::Answered(_))))
}

/// Take `call` and run it in `place`, as the job `key` of `calls`.
#[allow(clippy::too_many_arguments)]
fn take<Run, Ran>(
    client: &Arc<Client>,
    serving: &Serving,
    nudge: &Arc<Notify>,
    call: &Value,
    place: Place,
    run: &Run,
    calls: &mut Calls,
    key: (i64, String),
) where
    Run: Fn(Value, Place) -> Ran,
    Ran: std::future::Future<Output = Result<Value, (&'static str, String)>> + Send + 'static,
{
    {
        let call = call.clone();
        let (client, serving, held) = (client.clone(), serving.clone(), key.clone());
        let nudge = nudge.clone();
        let freed = Freed {
            serving: serving.clone(),
            nudge: nudge.clone(),
        };
        let ran = run(call.clone(), place);
        let base = json!({"bot": call["bot"], "turn": call["turn"], "call_id": call["call_id"]});
        let take = base.clone();
        let job = tokio::spawn(async move {
            // Its place goes with the job, whichever way it ends, and only
            // then is the next look woken: locals drop in reverse.
            let _freed = freed;
            let ran = ran;
            // Taken before it runs, so it runs at most once: a call another
            // host took, or this one before it restarted, may have done its
            // work already, and is answered that its outcome is unknown.
            let taken = client.request("take_call", take).await;
            let answer = match taken {
                Ok(_) => ran.await,
                Err(error) if error.code == "call_taken" => Err((
                    "tool_outcome_unknown",
                    "this call may have started before Sonny's tool host restarted or was \
                     upgraded, and was not run again; whether it did its work is not known"
                        .to_string(),
                )),
                // One that wants no answer is let go; one not known to be
                // taken is looked at again at the next sweep.
                Err(error) => {
                    let mut calls = serving.lock().unwrap();
                    if wants_none(&error.code) {
                        calls.retire(&key);
                    } else if matches!(calls.jobs.get(&key), Some(Job::Running { .. })) {
                        calls.jobs.remove(&key);
                    }
                    return;
                }
            };
            let mut asked = json!({"bot": call["bot"], "turn": call["turn"],
                "call_id": call["call_id"]});
            match answer {
                Ok(output) => asked["output"] = cut_parts(output),
                Err((code, detail)) => {
                    asked["error"] = json!({"code": code, "detail": cut(detail)})
                }
            }
            // Kept before it is handed in: a call that ran never runs again
            // because recording its result failed. One whose turn ended
            // meanwhile is let go, and one stopped keeps the answer saying so.
            {
                let mut calls = serving.lock().unwrap();
                if !matches!(calls.jobs.get(&key), Some(Job::Running { .. })) {
                    return;
                }
                calls.jobs.insert(key.clone(), Job::Answered(asked));
            }
            if !hand_in(&client, &serving, &key).await {
                nudge.notify_one();
            }
        });
        let job = job.abort_handle();
        calls.jobs.insert(held, Job::Running { job, asked: base });
    }
}

/// Hand the daemon the result kept for `key`, and let it go once the
/// daemon has it or says the call wants none: answered already, out of
/// time, its turn over. Anything else, such as a lost connection or a
/// store that could not write, keeps it to be handed in again. Whether
/// it is let go.
async fn hand_in(client: &Client, serving: &Serving, key: &(i64, String)) -> bool {
    let asked = match serving.lock().unwrap().jobs.get(key) {
        Some(Job::Answered(asked)) => asked.clone(),
        _ => return true,
    };
    let taken = match client.request("tool_result", asked).await {
        Ok(_) => true,
        Err(error) => wants_none(&error.code),
    };
    if taken {
        serving.lock().unwrap().retire(key);
    }
    taken
}

/// Whether the daemon's refusal of a result says the call wants none, so
/// handing it in again would change nothing.
fn wants_none(code: &str) -> bool {
    matches!(
        code,
        "no_pending_call"
            | "call_already_answered"
            | "turn_not_found"
            | "bot_not_found"
            | "tool_output_limit"
    ) || code.starts_with("invalid_")
}

fn str_of(value: &Value) -> String {
    value.as_str().unwrap_or_default().to_owned()
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

/// An answer of text alone or of parts, its text held to `OUTPUT_BYTES`
/// together, as `cut` holds text alone; its pictures the script bounded.
fn cut_parts(output: Value) -> Value {
    let Value::Array(parts) = output else {
        return json!(cut(output.as_str().unwrap_or_default().to_owned()));
    };
    let mut left = OUTPUT_BYTES;
    let parts = parts.into_iter().filter_map(|mut part| {
        let Some(text) = part["text"].as_str() else {
            return Some(part);
        };
        if left == 0 {
            return None;
        }
        let mut end = text.len().min(left);
        while !text.is_char_boundary(end) {
            end -= 1;
        }
        left -= end;
        let kept = match end < text.len() {
            true => format!("{}\n[cut]", &text[..end]),
            false => text.to_owned(),
        };
        part["text"] = json!(kept);
        Some(part)
    });
    Value::Array(parts.collect())
}

/// An `exec` call answered here, with its script's time bounded by the
/// call's: its text, and the pictures it showed.
async fn script(
    hub: &crate::mcp::Hub,
    client: &Arc<Client>,
    call: &Value,
) -> Result<Value, (&'static str, String)> {
    let input = call["arguments"]["input"].as_str().unwrap_or_default();
    let bot = str_of(&call["bot"]);
    unreviewed(client, &bot, &Value::Null, EXEC).await?;
    let steps = crate::exec::Steps::new(
        client.clone(),
        bot.clone(),
        call["turn"].as_i64().unwrap_or(0),
        str_of(&call["call_id"]),
    );
    // On the machine its options line names, against that machine's
    // servers, with this agent's gate asked what they ask first.
    if let Ok(crate::exec::Script {
        machine: Some(machine),
        ..
    }) = crate::exec::parse(input)
    {
        // What the output's budget cuts comes back whole, saved here, where
        // the agent reads it.
        let whole = crate::exec::whole_file();
        let lent = json!({"input": input, "bot": bot, "timeout_ms": running(call).as_millis() as u64,
            "whole": whole, "store": client.store().unwrap_or_default()});
        let answer = crate::link::call(&machine, EXEC, lent, Some(&steps)).await?;
        // Up to megabytes, so off the runtime; its owner's alone, as a
        // file saved here is.
        if let (Some(file), Some(text)) = (whole, answer["whole"].as_str()) {
            let text = text.to_owned();
            let _ =
                tokio::task::spawn_blocking(move || crate::exec::save_whole(&file, &text)).await;
        }
        return Ok(answer["answer"].clone());
    }
    crate::exec::answer(hub, &bot, input, running(call), Some(steps))
        .await
        .map_err(|e| ("exec_refused", e))
}

/// Answer a `browser` call in the host, whose browsers stay up between
/// calls, with the agent's gate judging what agent-browser asks to confirm.
async fn browse(client: &Arc<Client>, call: &Value) -> Result<Value, (&'static str, String)> {
    let bot = str_of(&call["bot"]);
    unreviewed(client, &bot, &Value::Null, BROWSER).await?;
    let steps = crate::exec::Steps::new(
        client.clone(),
        bot,
        call["turn"].as_i64().unwrap_or(0),
        str_of(&call["call_id"]),
    );
    let left = left(call).min(crate::browser::TIMEOUT);
    match tokio::time::timeout(left, crate::browser::answer(client, call, steps)).await {
        Ok(answer) => answer,
        Err(_) => Err(("timed_out", "the browser's commands ran out of time".into())),
    }
}

/// Whether an `automations` call only looks: it lists them or shows one,
/// and adds, removes and runs none.
fn looks(arguments: &Value) -> bool {
    ["add", "remove", "run"]
        .iter()
        .all(|key| arguments.get(key).is_none_or(Value::is_null))
}

/// Answer an `automations` call. An add, a removal or a run is judged by
/// its parts, each a step of the call under the tool it amounts to,
/// decided by the agent's daemon as that tool's call from the model would
/// be: any step refused refuses the call, with nothing added. A part that
/// touches only the agent itself, as waking it, is no step, and a look
/// runs unjudged, as reads do.
async fn automate(
    client: &Arc<Client>,
    call: &Value,
    me: &Path,
    store: &Path,
    socket: &Path,
    place: Place,
) -> Answer {
    let bot = str_of(&call["bot"]);
    let (parts, reviewed) = match looks(&call["arguments"]) {
        true => (Vec::new(), None),
        false => automation_parts(client, &bot, &call["arguments"]).await?,
    };
    // What it removes, runs or replaces is the definition its parts were
    // read from, or nothing: the tool refuses one that changed since.
    let mut call = call.clone();
    match reviewed {
        Some(reviewed) => call["arguments"][crate::trigger::REVIEWED] = json!(reviewed),
        None => {
            if let Some(arguments) = call["arguments"].as_object_mut() {
                arguments.remove(crate::trigger::REVIEWED);
            }
        }
    }
    let call = &call;
    if parts.is_empty() {
        return answer(client, call, me, store, socket, place).await;
    }
    // An agent it starts is started as start_task starts one: refused to
    // a confined agent no one reviews.
    if parts.iter().any(|part| part.tool == START) {
        unreviewed(client, &bot, &Value::Null, START).await?;
    }
    let steps = crate::exec::Steps::new(
        client.clone(),
        bot,
        call["turn"].as_i64().unwrap_or(0),
        str_of(&call["call_id"]),
    );
    let asked = std::time::Instant::now();
    let mut allowed = Vec::with_capacity(parts.len());
    for part in parts {
        let called = format!("{} ({})", part.what, part.tool);
        let facts = crate::exec::Facts {
            kind: part.kind,
            place: None,
            access: None,
        };
        match (steps.ask(part.tool, &called, part.arguments, facts, true)).await {
            Ok(step) => allowed.push(step),
            Err(refused) => {
                // Nothing of it was added, so the parts allowed before
                // never happened.
                for step in allowed {
                    steps.ran(step, true).await;
                }
                return Err(("automation_refused", refused));
            }
        }
    }
    // The daemon's clock for the call stood still while the gates weighed
    // it, so this one does too.
    let call = later(call, asked.elapsed());
    let answered = answer(client, &call, me, store, socket, place).await;
    for step in allowed {
        steps.ran(step, answered.is_err()).await;
    }
    answered
}

/// One part of an automation that reaches past the agent asking: the step
/// it amounts to, under that tool's name and arguments.
#[derive(Debug, PartialEq)]
struct Part {
    tool: &'static str,
    /// What the part is, for a refusal to name.
    what: &'static str,
    /// Its kind in the audit trail.
    kind: &'static str,
    arguments: Value,
}

impl Part {
    /// Its `if` command, run where the automation runs it: in the agent's
    /// sandbox, or outside any for one the agent did not write.
    fn check(command: &str, folder: Option<&Path>, outside: bool) -> Part {
        let mut arguments = json!({"command": command});
        if let Some(folder) = folder {
            arguments["workdir"] = json!(folder);
        }
        if outside {
            arguments["sandbox_permissions"] = json!("require_escalated");
        }
        Part {
            tool: "shell",
            what: "its if command",
            kind: "shell",
            arguments,
        }
    }

    /// A message to another agent: each run's to the agent it wakes, or
    /// each run's answer to the agent it answers.
    fn message(to: &str, what: &'static str, message: &str) -> Part {
        Part {
            tool: "send_message",
            what,
            kind: "agents",
            arguments: json!({"to": to, "message": message}),
        }
    }

    /// The agent its first run starts.
    fn start(name: &str, model: &str, effort: Option<&str>, brief: &str) -> Part {
        let mut arguments = json!({"name": name, "brief": brief, "model": model});
        if let Some(effort) = effort {
            arguments["effort"] = json!(effort);
        }
        Part {
            tool: START,
            what: "the agent it starts",
            kind: "agents",
            arguments,
        }
    }
}

/// What a run's answer is said to be, to the agent it goes to.
const ANSWERS: &str = "each run's answer";

/// The parts of an `add` by the agent `me`: none for one that only wakes
/// it, else its `if` command as a `shell` call in its own sandbox, where it
/// runs at each fire, a message to another agent as `send_message`, and an
/// agent it starts as `start_task`.
fn added_parts(me: &str, add: &Value) -> Vec<Part> {
    let text = |key: &str| add[key].as_str().filter(|value| !value.is_empty());
    let message = text("message").unwrap_or_default();
    let mut parts = Vec::new();
    if let Some(command) = text("if") {
        parts.push(Part::check(command, None, false));
    }
    if let Some(name) = add["start"]["name"].as_str() {
        let model = add["start"]["model"].as_str().unwrap_or_default();
        parts.push(Part::start(
            name,
            model,
            add["start"]["effort"].as_str(),
            message,
        ));
    }
    if let Some(to) = text("agent").filter(|to| *to != me) {
        parts.push(Part::message(to, "the agent it wakes", message));
    }
    if let Some(to) = text("reply_to").filter(|to| *to != me) {
        parts.push(Part::message(to, "the agent it answers", ANSWERS));
    }
    parts
}

/// The parts of the automation `trigger` stands as, for a removal or run
/// of it by `caller`, as its add would have asked them: an `if` command
/// the caller did not write runs where its writer's do, outside the
/// caller's sandbox. The caller is who the automation pins, by id in its
/// store, not by name: another store sharing this HOME can have an agent
/// of the same name.
fn standing_parts(caller: &crate::trigger::Caller, trigger: &crate::trigger::Trigger) -> Vec<Part> {
    use crate::trigger::{Target, Writer};
    let ours = caller.store.as_deref() == Some(trigger.store_id.as_str());
    let mine = |id: i64| ours && caller.id == Some(id);
    let mut parts = Vec::new();
    if let Some(command) = &trigger.gate {
        let written = matches!(&trigger.gate_by, Some(Writer::Agent(_, by)) if mine(*by));
        parts.push(Part::check(command, trigger.dir.as_deref(), !written));
    }
    match &trigger.target {
        Target::Start {
            name,
            model,
            effort,
            ..
        } => parts.push(Part::start(
            name,
            model,
            effort.as_deref(),
            &trigger.message,
        )),
        Target::Bot { name, id } if !mine(*id) => {
            parts.push(Part::message(name, "the agent it wakes", &trigger.message))
        }
        Target::Bot { .. } => {}
    }
    if let Some((to, _)) = trigger.reply_to.as_ref().filter(|(_, id)| !mine(*id)) {
        parts.push(Part::message(to, "the agent it answers", ANSWERS));
    }
    parts
}

/// The parts of an `automations` call by the agent `bot` that adds,
/// removes or runs one, each once: an add's own, and when it replaces one
/// that is not the agent's, that one's too; a removal of one that is not
/// the agent's, its parts; a run, the parts of the one it runs. Only a
/// removal, a run or a replacement reads the daemon, for who the agent is,
/// and with the parts comes the definition they were read from
/// ([`crate::trigger::Reviewed`]), which the call then acts on only as it
/// stands: one replaced, or added, while its parts were asked about is
/// not what was asked about.
async fn automation_parts(
    client: &Client,
    bot: &str,
    arguments: &Value,
) -> Result<(Vec<Part>, Option<crate::trigger::Reviewed>), (&'static str, String)> {
    let named = |key: &str| arguments[key].as_str().filter(|name| !name.is_empty());
    let add = &arguments["add"];
    // The name an add replaces: its own, its agent's, or, run from the
    // agent's shell, the agent's that shell runs as.
    let replaced = |me: &[String]| {
        (add["name"].as_str())
            .or(add["start"]["name"].as_str())
            .or(add["agent"].as_str())
            .or(me.last().map(String::as_str))
            .unwrap_or(bot)
            .to_owned()
    };
    let (mut parts, acted, run) = match (add.is_object(), named("remove"), named("run")) {
        (true, ..) => (
            added_parts(bot, add),
            (add["replace"] == true).then_some(None),
            false,
        ),
        (false, Some(name), _) => (Vec::new(), Some(Some(name.to_owned())), false),
        (false, None, Some(name)) => (Vec::new(), Some(Some(name.to_owned())), true),
        _ => (Vec::new(), None, false),
    };
    let mut reviewed = None;
    if let Some(name) = acted {
        let (me, id) = automation_caller(client, bot).await?;
        let name = name.unwrap_or_else(|| replaced(&me));
        let caller = crate::trigger::Caller {
            id,
            store: client.store().map(str::to_owned),
        };
        let places = crate::trigger::Places::home().map_err(|e| ("automation_refused", e))?;
        let standing = crate::trigger::standing(&places, &name, &caller);
        if let Some((trigger, mine)) = &standing
            && (run || !mine)
        {
            parts.extend(standing_parts(&caller, trigger));
        }
        reviewed = Some(crate::trigger::Reviewed {
            generation: standing.map(|(trigger, _)| trigger.generation),
            name,
        });
    }
    let mut once: Vec<Part> = Vec::with_capacity(parts.len());
    for part in parts {
        if !once.contains(&part) {
            once.push(part);
        }
    }
    Ok((once, reviewed))
}

/// The names the agent `bot` acts as, its own and that of the agent a side
/// chat copies, and the id its automations know it by, as the tool's
/// answer reads them.
async fn automation_caller(
    client: &Client,
    bot: &str,
) -> Result<(Vec<String>, Option<i64>), (&'static str, String)> {
    let page = client
        .request("bots", json!({"name": bot, "limit": 1}))
        .await
        .map_err(|e| ("caller_unknown", e.to_string()))?;
    let record = page["bots"]
        .get(0)
        .filter(|record| record["name"] == bot)
        .ok_or(("caller_unknown", format!("{bot} is gone")))?;
    Ok(match crate::side_copy(record) {
        Some(source) => (
            vec![bot.to_owned(), source.to_owned()],
            record["created_by_id"].as_i64(),
        ),
        None => (vec![bot.to_owned()], record["bot_id"].as_i64()),
    })
}

/// Answer a `desktop` call in the host, with the agent's gate judging what
/// acts on the desktop.
async fn desktop(client: &Arc<Client>, call: &Value) -> Result<Value, (&'static str, String)> {
    let bot = str_of(&call["bot"]);
    unreviewed(client, &bot, &Value::Null, DESKTOP).await?;
    let steps = crate::exec::Steps::new(
        client.clone(),
        bot,
        call["turn"].as_i64().unwrap_or(0),
        str_of(&call["call_id"]),
    );
    let left = left(call).min(crate::desktop::TIMEOUT);
    match tokio::time::timeout(left, crate::desktop::answer(call, steps)).await {
        Ok(answer) => answer,
        Err(_) => Err(("timed_out", "the desktop's commands ran out of time".into())),
    }
}

/// Refuses `tool`, one Sonny gates, to `bot` when it acts outside a
/// sandbox no one reviews calls from. The sandbox is read from `inspect`,
/// which a `bots` listing leaves out; `bot_id`, when given, pins the bot.
async fn unreviewed(
    client: &Client,
    bot: &str,
    bot_id: &Value,
    tool: &str,
) -> Result<(), (&'static str, String)> {
    if !GATED.contains(&tool) {
        return Ok(());
    }
    let inspected = client
        .request("inspect", json!({"bot": bot, "bot_id": bot_id}))
        .await
        .map_err(|e| ("caller_unknown", e.to_string()))?;
    match crate::approvals::may_run(&inspected, tool) {
        true => Ok(()),
        false => Err((
            "escalation_refused",
            format!(
                "{tool} acts outside your sandbox, and no one reviews calls for you; \
                 do without it"
            ),
        )),
    }
}

/// `call` with its time to answer moved `by` later.
fn later(call: &Value, by: Duration) -> Value {
    let mut call = call.clone();
    if let Some(expires) = call["expires_ms"].as_u64() {
        call["expires_ms"] = json!(expires.saturating_add(by.as_millis() as u64));
    }
    call
}

/// The time a call has left before the daemon gives up on it.
fn left(call: &Value) -> Duration {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_millis() as u64);
    let expires = call["expires_ms"].as_u64().unwrap_or(now);
    Duration::from_millis(expires.saturating_sub(now))
}

/// The time an `exec` script may run: the call's, less [`ANSWERING`] (at
/// most half of it), so a script run to its time limit answers with what
/// it did while the daemon still takes the answer.
fn running(call: &Value) -> Duration {
    let left = left(call);
    left - ANSWERING.min(left / 2)
}

/// The time kept back from a script's for its answer to reach the daemon.
const ANSWERING: Duration = Duration::from_secs(2);

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
                            Some(
                                "web_fetch" | "web_search" | crate::exec::NAME | BROWSER | DESKTOP
                            )
                        )
                }
                _ => false,
            })
}

/// One call answered by `sonny tool NAME`, run as the calling agent's shell
/// would run it, and stopped when the call's time is up.
/// `bot`'s turn `turn`, as the daemon lists it, or null when it has none.
pub async fn running_turn(client: &Client, bot: &str, turn: i64) -> Result<Value, String> {
    let page = client
        .request(
            "turns",
            json!({"bot": bot, "after": turn - 1, "before": turn + 1, "limit": 1}),
        )
        .await
        .map_err(|e| e.to_string())?;
    Ok((page["turns"].get(0).filter(|t| t["turn"] == turn))
        .cloned()
        .unwrap_or(Value::Null))
}

/// The folder, model and effort the turn `running` runs with: as it
/// started, which a bot's own change for a later turn, or the turn's own
/// choice, makes other than `record`'s. Without the turn, `record`'s.
pub fn as_running(running: &Value, record: &Value) -> (String, String, Option<String>) {
    let workspace = (running["workspace"].as_str())
        .or(record["workspace"].as_str())
        .unwrap_or("/");
    let model = match running["model"].as_str() {
        Some(model) => model.to_owned(),
        None => format!(
            "{}/{}",
            str_of(&record["provider"]),
            str_of(&record["model"])
        ),
    };
    let effort = match running.is_null() {
        false => running["effort"].as_str(),
        true => record["effort"].as_str(),
    };
    (workspace.to_owned(), model, effort.map(str::to_owned))
}

/// In `place`, a process's taken before the call was, or one waited for
/// here.
async fn answer(
    client: &Client,
    call: &Value,
    me: &Path,
    store: &Path,
    socket: &Path,
    place: Place,
) -> Answer {
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
    // its tasks, windows and projects are the agent's, not the copy's.
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
    // That bot, by its id, not one given its name since. `automate` has
    // seen to an automations call: a look needs no one to review it.
    if name != AUTOMATIONS {
        unreviewed(client, bot, &record["bot_id"], name).await?;
    }
    let turn = call["turn"].as_i64().unwrap_or(0);
    // As the calling turn runs, which keeps its folder and model when the
    // bot's own change for a turn after it.
    let running = running_turn(client, bot, turn)
        .await
        .map_err(|e| ("caller_unknown", e))?;
    let (workspace, model, effort) = as_running(&running, record);
    let (workspace, effort) = (workspace.as_str(), effort.as_deref());
    let mut command = tokio::process::Command::new(me);
    command
        .arg("tool")
        .arg(call["name"].as_str().unwrap_or_default())
        .current_dir(workspace)
        .env("PWD", workspace)
        .env("AGENT_BOT", str_of(&caller["name"]))
        .env("AGENT_BOT_ID", caller["bot_id"].to_string())
        .env("AGENT_TURN", call["turn"].to_string())
        .env("AGENT_CALL", str_of(&call["call_id"]))
        // The bot whose turn made the call, which a side chat's AGENT_BOT
        // is not: an ask's deadline looks for the call in its history.
        .env("AGENT_CALL_BOT", bot)
        .env("AGENT_CALL_BOT_ID", record["bot_id"].to_string())
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
    // A run of an automation works for the automation: an open memory
    // cleanup is the automation's, whichever run's agent started it, so a
    // run that carries on one cut short changes it as its own.
    if call["name"] == MEMORY
        && running["request_id"]
            .as_str()
            .is_some_and(|r| r.starts_with(crate::trigger::SENT_PREFIX))
        && let (Some(store_id), Ok(places), Some(id)) = (
            client.store(),
            crate::trigger::Places::home(),
            caller["bot_id"].as_i64(),
        )
        && let Some(name) =
            crate::trigger::run_of(&places, Some(store_id), &str_of(&caller["name"]), id)
    {
        command.env("AGENT_AUTOMATION", name);
    } else {
        command.env_remove("AGENT_AUTOMATION");
    }
    // A task's pointer comes from here, where the commons' words are kept
    // between tasks, never read whole again by each start's own process.
    command.env_remove(crate::commons::POINTER_ENV);
    if name == START
        && let (Some(home), Some(brief)) = (
            std::env::var_os("HOME"),
            call["arguments"]["brief"].as_str(),
        )
    {
        let brief = brief.to_owned();
        let line = tokio::task::spawn_blocking(move || {
            crate::commons::pointer_line(Path::new(&home), &brief)
        })
        .await
        .unwrap_or_default();
        command.env(crate::commons::POINTER_ENV, line);
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
    // A place among the processes Sonny runs for agents, within the call's
    // time: held until its process is done.
    let owner = str_of(&caller["name"]);
    let _place = match place {
        Place::Process(place) => place,
        _ => tokio::time::timeout(left(call), crate::budget::processes().admit(&owner))
            .await
            .map_err(|_| {
                (
                    "tool_unserved",
                    "ran out of time waiting for one of Sonny's processes (sonny.tool_processes)"
                        .to_owned(),
                )
            })?,
    };
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
    // Answered: what it left running on purpose, as a lookout it started,
    // runs on.
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
/// another build's does, so none goes on behind it. Each was taken, so
/// that host answers that its outcome is unknown and never runs it again.
pub fn stop() {
    for group in GROUPS.lock().unwrap().take().unwrap_or_default() {
        kill(group);
    }
    crate::mcp::kill_all();
}

/// A tool's refusal `said`, coded `code`, unless it refuses a live
/// template, which is coded as one wherever it is written.
fn refused_as(code: &'static str, said: String) -> (&'static str, String) {
    let live = format!("{}: ", crate::template::REFUSED);
    match said.strip_prefix(&live) {
        Some(why) => (crate::template::REFUSED, why.to_owned()),
        None => (code, said),
    }
}

/// The error codes `sonny tool` answers with, as static names.
fn code(said: &str) -> &'static str {
    const CODES: [&str; 24] = [
        "not_in_folder",
        "no_caller",
        "invalid_arguments",
        "task_not_started",
        "depth_limit",
        "tasks_exist",
        "task_not_removed",
        "not_shown",
        "project_not_made",
        "no_store",
        "follow_refused",
        "automation_refused",
        "memory_refused",
        "margin_full",
        "margin_kind",
        "margin_note_too_long",
        "margin_unread",
        "live_template",
        "not_marked",
        "commons_refused",
        "ask_refused",
        "ask_close_refused",
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

    /// Every call has a place at once.
    fn free(_: &Value, _: &str) -> Admitted {
        Admitted::Now(Place::Free)
    }

    #[test]
    fn a_call_ends_before_the_daemon_gives_up_on_it() {
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_millis() as u64;
        let call = |expires: u64| json!({"expires_ms": expires});
        let script = running(&call(now + 600_000));
        assert!(script <= Duration::from_secs(598) && script > Duration::from_secs(590));
        // A short call keeps half of its time for the answer.
        let script = running(&call(now + 2_000));
        assert!(script <= Duration::from_secs(1) && script > Duration::from_millis(900));
        assert_eq!(running(&json!({})), Duration::ZERO);
        // Every other wait is the call's whole time.
        assert!(left(&call(now + 600_000)) > Duration::from_secs(598));
    }

    #[test]
    fn a_tool_sonny_no_longer_answers_is_told_the_ones_it_does() {
        let said = unknown("triggers");
        assert!(said.starts_with("Sonny answers no tool triggers; the tools it answers now are "));
        assert!(said.contains("automations"), "{said}");
        assert!(!said.contains(" triggers,"), "{said}");
    }

    #[tokio::test]
    async fn a_call_that_ran_is_never_run_again_when_its_result_was_not_recorded() {
        use std::sync::atomic::{AtomicUsize, Ordering};
        let handed = Arc::new(AtomicUsize::new(0));
        let tries = handed.clone();
        // The daemon lists the call until it takes a result; the first two
        // results are lost to a store that could not write.
        let client = crate::tests::daemon_answering(move |asked| match asked["op"].as_str() {
            Some("tool_calls") if tries.load(Ordering::SeqCst) < 3 => json!({
                "calls": [{"bot": "b", "turn": 1, "call_id": "c1", "name": SHOW}],
                "next_after": null}),
            Some("tool_calls") => json!({"calls": [], "next_after": null}),
            Some("tool_result") if tries.fetch_add(1, Ordering::SeqCst) < 2 => {
                json!({"error": "storage", "detail": "disk I/O error"})
            }
            _ => json!({}),
        })
        .await;
        let serving: Serving = Default::default();
        let effects = Arc::new(AtomicUsize::new(0));
        let ran = effects.clone();
        let run = move |_call: Value, _: Place| {
            let ran = ran.clone();
            async move {
                ran.fetch_add(1, Ordering::SeqCst);
                Ok(json!("written"))
            }
        };
        let nudge = Arc::new(Notify::new());
        sweep(&client, &serving, &nudge, free, run.clone())
            .await
            .unwrap();
        // It runs and its result is handed in, and lost: the host is woken
        // to hand it in again soon, not at its next sweep.
        tokio::time::timeout(Duration::from_secs(5), nudge.notified())
            .await
            .expect("woken for the lost result");
        assert_eq!(handed.load(Ordering::SeqCst), 1);
        assert!(matches!(
            serving.lock().unwrap().jobs.values().next(),
            Some(Job::Answered(_))
        ));
        // Recording failed again: the result is still kept, and the call,
        // listed again, is not run again.
        assert!(
            sweep(&client, &serving, &nudge, free, run.clone())
                .await
                .unwrap()
        );
        assert_eq!(handed.load(Ordering::SeqCst), 2);
        assert_eq!(effects.load(Ordering::SeqCst), 1);
        // Taken at last, it is let go: one effect in all.
        assert!(!sweep(&client, &serving, &nudge, free, run).await.unwrap());
        assert_eq!(handed.load(Ordering::SeqCst), 3);
        assert!(serving.lock().unwrap().jobs.is_empty());
        assert_eq!(effects.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn calls_wait_in_the_daemon_until_a_place_frees_and_each_runs_once() {
        use std::sync::atomic::{AtomicUsize, Ordering};
        // A thousand calls wait, more than four pages of the listing; two
        // places to run them in, and each call slow until let go.
        const CALLS: i64 = 1_000;
        let answered = Arc::new(Mutex::new(HashSet::<String>::new()));
        let (taken, most_listed) = (Arc::new(AtomicUsize::new(0)), Arc::new(AtomicUsize::new(0)));
        let client = {
            let (answered, taken, most_listed) =
                (answered.clone(), taken.clone(), most_listed.clone());
            crate::tests::daemon_answering(move |asked| match asked["op"].as_str() {
                Some("tool_calls") => {
                    let after = asked["after"].as_i64().unwrap_or(0);
                    let limit = asked["limit"].as_i64().unwrap_or(256);
                    let answered = answered.lock().unwrap();
                    let calls: Vec<Value> = (after + 1..=CALLS)
                        .filter(|n| !answered.contains(&format!("c{n}")))
                        .take(limit as usize)
                        .map(|n| json!({"bot": "b", "turn": 1, "call_id": format!("c{n}"),
                            "name": "memory", "id": n}))
                        .collect();
                    most_listed.fetch_max(calls.len(), Ordering::SeqCst);
                    let last = calls.last().and_then(|call| call["id"].as_i64());
                    let more = last.is_some_and(|last| {
                        (last + 1..=CALLS).any(|n| !answered.contains(&format!("c{n}")))
                    });
                    json!({"calls": calls, "next_after": if more { json!(last) } else { Value::Null }})
                }
                Some("take_call") => {
                    taken.fetch_add(1, Ordering::SeqCst);
                    json!({})
                }
                // A result handed in again while the first is on its way,
                // as a look that starts meanwhile does, is refused as the
                // daemon refuses it; each call still runs once (`ran`).
                Some("tool_result") => {
                    match (answered.lock().unwrap()).insert(str_of(&asked["call_id"])) {
                        true => json!({}),
                        false => json!({"error": "call_already_answered"}),
                    }
                }
                _ => json!({}),
            })
            .await
        };
        let places = Arc::new(tokio::sync::Semaphore::new(2));
        let admit = {
            let places = places.clone();
            move |_: &Value, _: &str| match places.clone().try_acquire_owned() {
                Ok(place) => Admitted::Now(Place::Held(place)),
                Err(_) => {
                    let places = places.clone();
                    Admitted::Wait(Box::pin(async move {
                        Place::Held(places.acquire_owned().await.unwrap())
                    }))
                }
            }
        };
        let go = Arc::new(tokio::sync::Semaphore::new(0));
        let ran = Arc::new(AtomicUsize::new(0));
        // The calls running at once, and the most there ever were.
        let (running, most_running) =
            (Arc::new(AtomicUsize::new(0)), Arc::new(AtomicUsize::new(0)));
        let run = {
            let (go, ran) = (go.clone(), ran.clone());
            let (running, most_running) = (running.clone(), most_running.clone());
            move |_call: Value, place: Place| {
                let (go, ran) = (go.clone(), ran.clone());
                let (running, most_running) = (running.clone(), most_running.clone());
                async move {
                    let _place = place;
                    most_running
                        .fetch_max(running.fetch_add(1, Ordering::SeqCst) + 1, Ordering::SeqCst);
                    go.acquire().await.unwrap().forget();
                    ran.fetch_add(1, Ordering::SeqCst);
                    running.fetch_sub(1, Ordering::SeqCst);
                    Ok(json!("noted"))
                }
            }
        };
        let serving: Serving = Default::default();
        let nudge = Arc::new(Notify::new());
        sweep(&client, &serving, &nudge, admit.clone(), run.clone())
            .await
            .unwrap();
        loop {
            // Only what has a place was taken; the rest wait in the daemon.
            let done = answered.lock().unwrap().len();
            // A call that ran and gave back its place may still be
            // handing in its result: the places, not the jobs, are two.
            assert!(taken.load(Ordering::SeqCst) <= ran.load(Ordering::SeqCst) + 2);
            assert!(most_running.load(Ordering::SeqCst) <= 2);
            if done == CALLS as usize {
                break;
            }
            let left = serving.lock().unwrap().left;
            go.add_permits(1);
            if left {
                // A place let go wakes the next look, while calls wait.
                tokio::time::timeout(Duration::from_secs(5), nudge.notified())
                    .await
                    .expect("a freed place woke no look");
            } else {
                // None waits: the last ones end on their own.
                while answered.lock().unwrap().len() == done {
                    tokio::time::sleep(Duration::from_millis(5)).await;
                }
            }
            sweep(&client, &serving, &nudge, admit.clone(), run.clone())
                .await
                .unwrap();
        }
        // Every call ran once, none was lost, and no look held more than a
        // page of the listing.
        assert_eq!(ran.load(Ordering::SeqCst), CALLS as usize);
        assert_eq!(taken.load(Ordering::SeqCst), CALLS as usize);
        assert!(most_listed.load(Ordering::SeqCst) <= 256);
    }

    #[tokio::test]
    async fn an_agent_found_waiting_for_a_place_takes_the_next_before_earlier_backlogs() {
        use std::sync::atomic::{AtomicUsize, Ordering};
        // Two places, a share of one each: a's and b's backlogs are listed
        // first, c's one call last.
        let listed: Vec<Value> = (1..=20)
            .map(|n| ("a", n))
            .chain((1..=20).map(|n| ("b", n)))
            .chain([("c", 1)])
            .enumerate()
            .map(|(id, (bot, n))| {
                json!({"bot": bot, "turn": 1, "call_id": format!("{bot}{n}"),
                    "name": "memory", "id": id + 1})
            })
            .collect();
        let taken = Arc::new(Mutex::new(Vec::<String>::new()));
        let answered = Arc::new(Mutex::new(HashSet::<String>::new()));
        let client = {
            let (taken, answered) = (taken.clone(), answered.clone());
            crate::tests::daemon_answering(move |asked| match asked["op"].as_str() {
                Some("tool_calls") => {
                    let answered = answered.lock().unwrap();
                    let calls: Vec<&Value> = (listed.iter())
                        .filter(|call| !answered.contains(&str_of(&call["call_id"])))
                        .collect();
                    json!({"calls": calls, "next_after": null})
                }
                Some("take_call") => {
                    taken.lock().unwrap().push(str_of(&asked["call_id"]));
                    json!({})
                }
                Some("tool_result") => {
                    answered.lock().unwrap().insert(str_of(&asked["call_id"]));
                    json!({})
                }
                _ => json!({}),
            })
            .await
        };
        let budget: &'static crate::budget::Budget =
            Box::leak(Box::new(crate::budget::Budget::new(2)));
        let admit = move |_: &Value, owner: &str| match budget.try_admit(owner) {
            Ok(place) => Admitted::Now(Place::Process(place)),
            Err(_) => {
                let owner = owner.to_owned();
                Admitted::Wait(Box::pin(async move {
                    Place::Process(budget.admit(&owner).await)
                }))
            }
        };
        let go = Arc::new(tokio::sync::Semaphore::new(0));
        let run = {
            let go = go.clone();
            move |_call: Value, place: Place| {
                let go = go.clone();
                async move {
                    let _place = place;
                    go.acquire().await.unwrap().forget();
                    Ok(json!("noted"))
                }
            }
        };
        let serving: Serving = Default::default();
        let nudge = Arc::new(Notify::new());
        let took = |count: usize| {
            let taken = taken.clone();
            async move {
                while taken.lock().unwrap().len() < count {
                    tokio::time::sleep(Duration::from_millis(5)).await;
                }
            }
        };
        sweep(&client, &serving, &nudge, admit, run.clone())
            .await
            .unwrap();
        // a and b hold the places; c waits behind them, first in line.
        took(2).await;
        tokio::time::sleep(Duration::from_millis(50)).await;
        assert_eq!(*taken.lock().unwrap(), ["a1", "b1"]);
        // A browser command waits for a place after c did.
        let browsed = Arc::new(AtomicUsize::new(0));
        let browser = {
            let browsed = browsed.clone();
            tokio::spawn(async move {
                let place = budget.admit("x").await;
                browsed.fetch_add(1, Ordering::SeqCst);
                place
            })
        };
        tokio::time::sleep(Duration::from_millis(50)).await;
        go.add_permits(1);
        tokio::time::timeout(Duration::from_secs(5), async {
            loop {
                nudge.notified().await;
                sweep(&client, &serving, &nudge, admit, run.clone())
                    .await
                    .unwrap();
                if tokio::time::timeout(Duration::from_millis(200), took(3))
                    .await
                    .is_ok()
                {
                    break;
                }
            }
        })
        .await
        .expect("a place given back goes to the next call");
        // The first place let go went to c, ahead of a's and b's backlogs
        // and of the browser command that came after it.
        assert_eq!(
            taken.lock().unwrap()[2],
            "c1",
            "{:?}",
            taken.lock().unwrap()
        );
        assert_eq!(browsed.load(Ordering::SeqCst), 0);
        browser.abort();
    }

    #[tokio::test]
    async fn a_call_that_runs_no_process_is_taken_while_its_agent_waits_for_a_place() {
        // Two places, held by a and b. c waits for one; its browser call,
        // which waits for its own place one command at a time, is taken now.
        let listed = json!([
            {"bot": "a", "turn": 1, "call_id": "a1", "name": "memory", "id": 1},
            {"bot": "b", "turn": 1, "call_id": "b1", "name": "memory", "id": 2},
            {"bot": "c", "turn": 1, "call_id": "c1", "name": "memory", "id": 3},
            {"bot": "c", "turn": 1, "call_id": "c2", "name": BROWSER, "id": 4},
        ]);
        let taken = Arc::new(Mutex::new(Vec::<String>::new()));
        let client = {
            let taken = taken.clone();
            crate::tests::daemon_answering(move |asked| match asked["op"].as_str() {
                Some("tool_calls") => json!({"calls": listed, "next_after": null}),
                Some("take_call") => {
                    taken.lock().unwrap().push(str_of(&asked["call_id"]));
                    json!({})
                }
                _ => json!({}),
            })
            .await
        };
        let budget: &'static crate::budget::Budget =
            Box::leak(Box::new(crate::budget::Budget::new(2)));
        let admit = move |_: &Value, owner: &str| match budget.try_admit(owner) {
            Ok(place) => Admitted::Now(Place::Process(place)),
            Err(_) => {
                let owner = owner.to_owned();
                Admitted::Wait(Box::pin(async move {
                    Place::Process(budget.admit(&owner).await)
                }))
            }
        };
        let run = |_call: Value, place: Place| async move {
            let _place = place;
            std::future::pending::<()>().await;
            Ok(json!("done"))
        };
        let serving: Serving = Default::default();
        let nudge = Arc::new(Notify::new());
        sweep(&client, &serving, &nudge, admit, run).await.unwrap();
        tokio::time::timeout(Duration::from_secs(5), async {
            while taken.lock().unwrap().len() < 3 {
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
        })
        .await
        .expect("the browser call is taken");
        assert_eq!(*taken.lock().unwrap(), ["a1", "b1", "c2"]);
        // c still waits for the place its process call needs.
        assert!(matches!(
            serving.lock().unwrap().reserved.get("c"),
            Some(Reserved::Waiting(_))
        ));
    }

    #[tokio::test]
    async fn a_call_counts_in_the_share_of_the_agent_its_record_says_it_copies() {
        let client = crate::tests::daemon_answering(|asked| match asked["name"].as_str() {
            // A side chat of shop, made by it, expiring.
            Some("shop.side-ab12") => json!({"bots": [{"name": "shop.side-ab12",
                "created_by": "shop", "expire_ms": 1}]}),
            // A task only named like one.
            Some("shop.side-feed1") => json!({"bots": [{"name": "shop.side-feed1",
                "created_by": "lead"}]}),
            _ => json!({"bots": []}),
        })
        .await;
        assert_eq!(
            owner_of(&client, &json!({"bot": "shop.side-ab12"})).await,
            "shop"
        );
        assert_eq!(
            owner_of(&client, &json!({"bot": "shop.side-feed1"})).await,
            "shop.side-feed1"
        );
        assert_eq!(owner_of(&client, &json!({"bot": "shop"})).await, "shop");
    }

    #[tokio::test]
    async fn a_call_taken_before_a_restart_is_answered_unknown_and_never_run_again() {
        use std::sync::atomic::{AtomicUsize, Ordering};
        // The daemon lists two calls: c1 a host took before it restarted,
        // c2 none took. It records what each is answered.
        let answers = Arc::new(Mutex::new(Vec::<Value>::new()));
        let answered = answers.clone();
        let client = crate::tests::daemon_answering(move |asked| match asked["op"].as_str() {
            Some("tool_calls") if answered.lock().unwrap().len() < 2 => json!({"calls": [
                {"bot": "b", "turn": 1, "call_id": "c1", "name": EXEC},
                {"bot": "b", "turn": 1, "call_id": "c2", "name": EXEC}],
                "next_after": null}),
            Some("tool_calls") => json!({"calls": [], "next_after": null}),
            Some("take_call") if asked["call_id"] == "c1" => {
                json!({"error": "call_taken", "detail": "taken"})
            }
            Some("tool_result") => {
                answered.lock().unwrap().push(asked.clone());
                json!({})
            }
            _ => json!({}),
        })
        .await;
        let serving: Serving = Default::default();
        let effects = Arc::new(Mutex::new(Vec::<String>::new()));
        let ran = effects.clone();
        let run = move |call: Value, _: Place| {
            let ran = ran.clone();
            async move {
                ran.lock().unwrap().push(str_of(&call["call_id"]));
                Ok(json!("written"))
            }
        };
        let nudge = Arc::new(Notify::new());
        let swept = Arc::new(AtomicUsize::new(0));
        while answers.lock().unwrap().len() < 2 && swept.fetch_add(1, Ordering::SeqCst) < 50 {
            sweep(&client, &serving, &nudge, free, run.clone())
                .await
                .unwrap();
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        // c2 ran once; c1 never ran, and is answered that its outcome is
        // unknown.
        assert_eq!(*effects.lock().unwrap(), ["c2"]);
        let answers = answers.lock().unwrap();
        let of = |id: &str| {
            answers
                .iter()
                .find(|a| a["call_id"] == id)
                .cloned()
                .unwrap()
        };
        assert_eq!(of("c1")["error"]["code"], "tool_outcome_unknown");
        assert_eq!(of("c2")["output"], "written");
    }

    #[tokio::test]
    async fn a_call_whose_result_was_taken_while_the_calls_were_listed_does_not_run_again() {
        use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
        let took = Arc::new(AtomicBool::new(false));
        let (taken, listing) = (took.clone(), Arc::new(AtomicUsize::new(0)));
        let finish = Arc::new(Notify::new());
        let release = finish.clone();
        // The daemon lists the call until its result is taken. The second
        // sweep's listing names it on its first page and lets it finish;
        // its later pages come only once the result is taken.
        let client = crate::tests::daemon_answering(move |asked| match asked["op"].as_str() {
            Some("tool_calls") if asked["after"] == 0 && !taken.load(Ordering::SeqCst) => {
                if listing.fetch_add(1, Ordering::SeqCst) == 1 {
                    release.notify_one();
                    return json!({"calls": [{"bot": "b", "turn": 1, "call_id": "c1",
                        "name": EXEC}], "next_after": 1});
                }
                json!({"calls": [{"bot": "b", "turn": 1, "call_id": "c1", "name": EXEC}],
                    "next_after": null})
            }
            Some("tool_calls") if asked["after"] != 0 && !taken.load(Ordering::SeqCst) => {
                std::thread::sleep(Duration::from_millis(1));
                json!({"calls": [], "next_after": 1})
            }
            Some("tool_calls") => json!({"calls": [], "next_after": null}),
            Some("tool_result") => {
                taken.store(true, Ordering::SeqCst);
                json!({})
            }
            _ => json!({}),
        })
        .await;
        let serving: Serving = Default::default();
        let effects = Arc::new(AtomicUsize::new(0));
        let (ran, finished) = (effects.clone(), finish.clone());
        let run = move |_call: Value, _: Place| {
            let (ran, finished) = (ran.clone(), finished.clone());
            async move {
                // The write happens as the call starts; the first waits to
                // end until the listing that crosses it is under way.
                if ran.fetch_add(1, Ordering::SeqCst) == 0 {
                    finished.notified().await;
                }
                Ok(json!("written"))
            }
        };
        let nudge = Arc::new(Notify::new());
        sweep(&client, &serving, &nudge, free, run.clone())
            .await
            .unwrap();
        // Read across the call's end: its first page names it, and its
        // result is taken before the listing is read to its end.
        sweep(&client, &serving, &nudge, free, run.clone())
            .await
            .unwrap();
        assert!(took.load(Ordering::SeqCst));
        assert!(serving.lock().unwrap().jobs.is_empty());
        sweep(&client, &serving, &nudge, free, run).await.unwrap();
        tokio::time::sleep(Duration::from_millis(50)).await;
        assert_eq!(effects.load(Ordering::SeqCst), 1);
        // Once a listing begun after it has been read, it is forgotten.
        assert!(serving.lock().unwrap().retired.is_empty());
    }

    #[tokio::test]
    async fn a_call_of_a_turn_that_ended_while_the_calls_were_listed_does_not_start() {
        use std::sync::atomic::{AtomicUsize, Ordering};
        let serving: Serving = Default::default();
        let listings = Arc::new(AtomicUsize::new(0));
        let (held, listed) = (serving.clone(), listings.clone());
        // The turn's first call runs here; while the next listing is read,
        // the turn ends, and the listing names its next call, which the
        // daemon had moved on to.
        let client = crate::tests::daemon_answering(move |asked| match asked["op"].as_str() {
            Some("tool_calls") => match listed.fetch_add(1, Ordering::SeqCst) {
                0 => json!({"calls": [{"bot": "b", "turn": 1, "call_id": "c1", "name": EXEC}],
                    "next_after": null}),
                1 => {
                    held.lock().unwrap().finish(1);
                    json!({"calls": [{"bot": "b", "turn": 1, "call_id": "c2", "name": EXEC}],
                        "next_after": null})
                }
                _ => json!({"calls": [], "next_after": null}),
            },
            _ => json!({}),
        })
        .await;
        let started = Arc::new(Mutex::new(Vec::<String>::new()));
        let starts = started.clone();
        let run = move |call: Value, _: Place| {
            starts.lock().unwrap().push(str_of(&call["call_id"]));
            async move {
                std::future::pending::<()>().await;
                Ok(json!("never"))
            }
        };
        let nudge = Arc::new(Notify::new());
        sweep(&client, &serving, &nudge, free, run.clone())
            .await
            .unwrap();
        sweep(&client, &serving, &nudge, free, run.clone())
            .await
            .unwrap();
        assert_eq!(*started.lock().unwrap(), ["c1"]);
        assert!(serving.lock().unwrap().jobs.is_empty());
        // A listing begun after the end lets the turn go.
        sweep(&client, &serving, &nudge, free, run).await.unwrap();
        assert!(serving.lock().unwrap().ended.is_empty());
    }

    #[tokio::test]
    async fn a_pause_stops_what_calls_run_and_starts_none_until_work_resumes() {
        use std::sync::atomic::{AtomicUsize, Ordering};
        let client = crate::tests::daemon_answering(|asked| match asked["op"].as_str() {
            Some("tool_calls") => json!({
                "calls": [{"bot": "b", "turn": 1, "call_id": "c1", "name": EXEC}],
                "next_after": null}),
            _ => json!({}),
        })
        .await;
        let server = Server {
            nudge: Arc::new(Notify::new()),
            serving: Default::default(),
        };
        // A script that reads slowly, then writes.
        let (read, written) = (Arc::new(Notify::new()), Arc::new(AtomicUsize::new(0)));
        let (reading, writes) = (read.clone(), written.clone());
        let run = move |_call: Value, _: Place| {
            let (reading, writes) = (reading.clone(), writes.clone());
            async move {
                reading.notify_one();
                tokio::time::sleep(Duration::from_millis(200)).await;
                writes.fetch_add(1, Ordering::SeqCst);
                Ok(json!("written"))
            }
        };
        sweep(&client, &server.serving, &server.nudge, free, run.clone())
            .await
            .unwrap();
        // Paused during the read: the write never comes.
        read.notified().await;
        server.event(&json!({"event": "paused", "pause": {"reason": "stop"}}));
        tokio::time::sleep(Duration::from_millis(400)).await;
        assert_eq!(written.load(Ordering::SeqCst), 0);
        // While it holds, a waiting call doesn't start.
        sweep(&client, &server.serving, &server.nudge, free, run.clone())
            .await
            .unwrap();
        assert!(server.serving.lock().unwrap().jobs.is_empty());
        // Once work resumes it does.
        server.event(&json!({"event": "resumed", "pause": {"reason": "stop"}}));
        sweep(&client, &server.serving, &server.nudge, free, run.clone())
            .await
            .unwrap();
        assert_eq!(server.serving.lock().unwrap().jobs.len(), 1);
        // A resume the events never carried, as while their connection was
        // down, is read from that connection's greeting when it is made
        // again.
        server.event(&json!({"event": "paused", "pause": {"reason": "stop"}}));
        sweep(&client, &server.serving, &server.nudge, free, run.clone())
            .await
            .unwrap();
        assert!(server.serving.lock().unwrap().jobs.is_empty());
        server.connected(&client);
        sweep(&client, &server.serving, &server.nudge, free, run)
            .await
            .unwrap();
        assert_eq!(server.serving.lock().unwrap().jobs.len(), 1);
    }

    #[tokio::test]
    async fn a_pause_stops_a_script_s_tool_calls_even_one_answered_right_after_it() {
        use agent_client::config::McpServer;
        let dir = std::env::temp_dir().join(format!("sonny-pause-exec-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let (log, gate) = (dir.join("calls"), dir.join("go"));
        let reached = || std::fs::read_to_string(&log).unwrap_or_default();
        // A server that notes each call as it arrives; its slow read
        // answers once the test lets it.
        let server = McpServer {
            command: Some("python3".into()),
            args: vec![concat!(env!("CARGO_MANIFEST_DIR"), "/../tests/mcp_server.py").into()],
            env: [("MCP_CALLS".to_owned(), log.display().to_string())].into(),
            enabled: true,
            ..McpServer::default()
        };
        let hub = crate::mcp::Hub::new(Some(dir.join("mcp")));
        hub.configure(&[("test".to_owned(), server)].into()).await;
        // A script that reads slowly, then keeps a note and writes, each
        // tried whatever the one before said.
        let input = format!(
            "await tools.mcp__test__slow({{until: {}}});\n\
             try {{ store('note', 1); }} catch (e) {{}}\n\
             await tools.mcp__test__echo({{text: 'written'}});\ntext('done');",
            serde_json::to_string(&gate.display().to_string()).unwrap()
        );
        let client = crate::tests::daemon_answering(|asked| match asked["op"].as_str() {
            Some("tool_calls") => json!({
                "calls": [{"bot": "b", "turn": 1, "call_id": "c1", "name": EXEC}],
                "next_after": null}),
            _ => json!({}),
        })
        .await;
        let server = Server {
            nudge: Arc::new(Notify::new()),
            serving: Default::default(),
        };
        let run = {
            let (hub, dir, input) = (hub.clone(), dir.clone(), input.clone());
            move |_call: Value, _: Place| {
                let (hub, dir, input) = (hub.clone(), dir.clone(), input.clone());
                async move {
                    crate::exec::run(&hub, Some(&dir), "b", &input, Duration::from_secs(30), None)
                        .await
                        .map_err(|e| ("exec_refused", e))
                }
            }
        };
        sweep(&client, &server.serving, &server.nudge, free, run.clone())
            .await
            .unwrap();
        // The read reaches the server, and the script waits on it.
        let started = std::time::Instant::now();
        while reached() != "slow\n" {
            assert!(started.elapsed() < Duration::from_secs(20), "{}", reached());
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        // Paused during the read, which answers the moment after.
        server.event(&json!({"event": "paused", "pause": {"reason": "stop"}}));
        std::fs::write(&gate, "").unwrap();
        // Long enough for the read's answer to come back and a script that
        // went on to write.
        tokio::time::sleep(Duration::from_millis(1500)).await;
        assert_eq!(reached(), "slow\n");
        assert!(!dir.join("store").join("b.json").exists());
        {
            let calls = server.serving.lock().unwrap();
            let key = (1, "c1".to_owned());
            let Some(Job::Answered(answered)) = calls.jobs.get(&key) else {
                panic!("the call is answered");
            };
            assert_eq!(answered["error"]["code"], PAUSED.0);
        }
        // While the pause holds, the call listed again starts nothing.
        sweep(&client, &server.serving, &server.nudge, free, run)
            .await
            .unwrap();
        tokio::time::sleep(Duration::from_millis(300)).await;
        assert_eq!(reached(), "slow\n");
        hub.stop().await;
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn a_call_stopped_while_the_calls_were_listed_does_not_start_again() {
        use std::sync::atomic::{AtomicUsize, Ordering};
        let serving: Serving = Default::default();
        let listings = Arc::new(AtomicUsize::new(0));
        let (held, listed) = (serving.clone(), listings.clone());
        // A pause, and a resume, while the listing is read: it names a call
        // the pause stopped.
        let client = crate::tests::daemon_answering(move |asked| match asked["op"].as_str() {
            Some("tool_calls") => {
                if listed.fetch_add(1, Ordering::SeqCst) == 1 {
                    pause(&held, true);
                    pause(&held, false);
                }
                json!({"calls": [{"bot": "b", "turn": 1, "call_id": "c1", "name": EXEC}],
                    "next_after": null})
            }
            _ => json!({}),
        })
        .await;
        let ran = Arc::new(AtomicUsize::new(0));
        let runs = ran.clone();
        let run = move |_call: Value, _: Place| {
            let runs = runs.clone();
            async move {
                runs.fetch_add(1, Ordering::SeqCst);
                tokio::time::sleep(Duration::from_secs(5)).await;
                Ok(json!("written"))
            }
        };
        let nudge = Arc::new(Notify::new());
        sweep(&client, &serving, &nudge, free, run.clone())
            .await
            .unwrap();
        while ran.load(Ordering::SeqCst) == 0 {
            tokio::task::yield_now().await;
        }
        // The pause stops it; the listing read across the pause starts
        // nothing, and the host looks again soon.
        sweep(&client, &serving, &nudge, free, run).await.unwrap();
        assert!(matches!(
            serving.lock().unwrap().jobs.values().next(),
            None | Some(Job::Answered(_))
        ));
        tokio::time::timeout(Duration::from_secs(1), nudge.notified())
            .await
            .expect("woken to look again");
        tokio::time::sleep(Duration::from_millis(50)).await;
        assert_eq!(ran.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn a_call_a_pause_stops_is_answered_so_it_does_not_run_again_after() {
        use std::sync::atomic::{AtomicUsize, Ordering};
        // A turn parked on the call: the daemon lists it until it is
        // answered, and a pause does not answer it.
        let answered = Arc::new(Mutex::new(None::<Value>));
        let took = answered.clone();
        let client = crate::tests::daemon_answering(move |asked| match asked["op"].as_str() {
            Some("tool_calls") if took.lock().unwrap().is_none() => json!({
                "calls": [{"bot": "b", "turn": 1, "call_id": "c1", "name": EXEC}],
                "next_after": null}),
            Some("tool_calls") => json!({"calls": [], "next_after": null}),
            Some("tool_result") => {
                *took.lock().unwrap() = Some(asked.clone());
                json!({})
            }
            _ => json!({}),
        })
        .await;
        let server = Server {
            nudge: Arc::new(Notify::new()),
            serving: Default::default(),
        };
        let (ran, runs) = (Arc::new(AtomicUsize::new(0)), Arc::new(Notify::new()));
        let (count, started) = (ran.clone(), runs.clone());
        let run = move |_call: Value, _: Place| {
            let (count, started) = (count.clone(), started.clone());
            async move {
                count.fetch_add(1, Ordering::SeqCst);
                started.notify_one();
                tokio::time::sleep(Duration::from_secs(5)).await;
                Ok(json!("written"))
            }
        };
        sweep(&client, &server.serving, &server.nudge, free, run.clone())
            .await
            .unwrap();
        runs.notified().await;
        server.event(&json!({"event": "paused", "pause": {"reason": "stop"}}));
        server.event(&json!({"event": "resumed", "pause": {"reason": "stop"}}));
        // The sweep after the resume hands in why it stopped, and does not
        // run it again.
        sweep(&client, &server.serving, &server.nudge, free, run.clone())
            .await
            .unwrap();
        sweep(&client, &server.serving, &server.nudge, free, run)
            .await
            .unwrap();
        assert_eq!(ran.load(Ordering::SeqCst), 1);
        let answer = answered.lock().unwrap().clone().expect("answered");
        assert_eq!(answer["error"]["code"], "daemon_paused");
        assert_eq!(answer["call_id"], "c1");
        assert!(server.serving.lock().unwrap().jobs.is_empty());
    }

    #[tokio::test]
    async fn losing_the_events_stops_what_calls_run_until_a_greeting_says_go_on() {
        use std::sync::atomic::{AtomicUsize, Ordering};
        let answered = Arc::new(Mutex::new(Vec::<Value>::new()));
        let took = answered.clone();
        let client = crate::tests::daemon_answering(move |asked| match asked["op"].as_str() {
            Some("tool_calls") => json!({
                "calls": [{"bot": "b", "turn": 1, "call_id": "c1", "name": EXEC}],
                "next_after": null}),
            Some("tool_result") => {
                took.lock().unwrap().push(asked.clone());
                json!({})
            }
            _ => json!({}),
        })
        .await;
        let server = Server {
            nudge: Arc::new(Notify::new()),
            serving: Default::default(),
        };
        let (written, wrote) = (Arc::new(AtomicUsize::new(0)), Arc::new(Notify::new()));
        let (writes, reading) = (written.clone(), wrote.clone());
        let run = move |_call: Value, _: Place| {
            let (writes, reading) = (writes.clone(), reading.clone());
            async move {
                reading.notify_one();
                tokio::time::sleep(Duration::from_millis(200)).await;
                writes.fetch_add(1, Ordering::SeqCst);
                Ok(json!("written"))
            }
        };
        sweep(&client, &server.serving, &server.nudge, free, run.clone())
            .await
            .unwrap();
        wrote.notified().await;
        // The events' connection drops: a pause then would reach no event.
        server.disconnected();
        tokio::time::sleep(Duration::from_millis(400)).await;
        assert_eq!(written.load(Ordering::SeqCst), 0);
        sweep(&client, &server.serving, &server.nudge, free, run.clone())
            .await
            .unwrap();
        // Why it stopped is handed in, and nothing starts while it holds.
        let said = answered.lock().unwrap().clone();
        assert_eq!(said.len(), 1);
        assert_eq!(said[0]["error"]["code"], "events_lost");
        assert!(server.serving.lock().unwrap().jobs.is_empty());
        // The next connection's greeting says work goes on.
        server.connected(&client);
        sweep(&client, &server.serving, &server.nudge, free, run)
            .await
            .unwrap();
        assert_eq!(server.serving.lock().unwrap().jobs.len(), 1);
    }

    #[tokio::test]
    async fn a_call_listed_across_a_pause_is_stopped_by_it_and_never_runs_after() {
        use std::sync::atomic::{AtomicUsize, Ordering};
        let serving: Serving = Default::default();
        let held = serving.clone();
        // The first page names a call, taken as that page is read; a pause
        // and a resume come before the second.
        let client = crate::tests::daemon_answering(move |asked| match asked["op"].as_str() {
            Some("tool_calls") if asked["after"] == 0 => {
                json!({"calls": [{"bot": "b", "turn": 1, "call_id": "c1", "name": EXEC}],
                    "next_after": 1})
            }
            Some("tool_calls") => {
                pause(&held, true);
                pause(&held, false);
                json!({"calls": [], "next_after": null})
            }
            _ => json!({}),
        })
        .await;
        let ran = Arc::new(AtomicUsize::new(0));
        let runs = ran.clone();
        let run = move |_call: Value, _: Place| {
            let runs = runs.clone();
            async move {
                runs.fetch_add(1, Ordering::SeqCst);
                Ok(json!("written"))
            }
        };
        let nudge = Arc::new(Notify::new());
        sweep(&client, &serving, &nudge, free, run.clone())
            .await
            .unwrap();
        tokio::time::timeout(Duration::from_secs(1), nudge.notified())
            .await
            .expect("woken to look again");
        tokio::time::sleep(Duration::from_millis(50)).await;
        // The pause stopped it as it stops any call running: why is handed
        // in, and the look that does so, listing it again, starts nothing.
        let before = ran.load(Ordering::SeqCst);
        assert!(before <= 1);
        sweep(&client, &serving, &nudge, free, run).await.unwrap();
        tokio::time::sleep(Duration::from_millis(50)).await;
        assert_eq!(ran.load(Ordering::SeqCst), before);
        assert!(serving.lock().unwrap().jobs.is_empty());
    }

    #[test]
    fn a_result_is_handed_in_again_unless_the_call_wants_none() {
        for code in ["no_pending_call", "call_already_answered", "turn_not_found"] {
            assert!(wants_none(code), "{code}");
        }
        for code in ["io", "storage", "store_busy", "overloaded"] {
            assert!(!wants_none(code), "{code}");
        }
    }

    /// A stand-in daemon that allows every step but those whose tool is in
    /// `refused`, and the requests it was sent, in order.
    async fn steps_daemon(
        refused: &'static [&'static str],
    ) -> (Arc<Client>, Arc<std::sync::Mutex<Vec<Value>>>) {
        let asked = Arc::new(std::sync::Mutex::new(Vec::new()));
        let kept = asked.clone();
        let client = crate::tests::daemon_answering(move |request| {
            kept.lock().unwrap().push(request.clone());
            match request["op"].as_str() {
                Some("approve_step") if refused.contains(&request["name"].as_str().unwrap()) => {
                    json!({"decision": "deny", "reason": "no"})
                }
                Some("approve_step") => json!({"decision": "allow"}),
                Some("bots") => json!({"bots": [{"name": "watch", "bot_id": 7}]}),
                Some("inspect") => json!({"name": "watch", "bot_id": 7, "settings": {},
                    "gates": [{"tag": "auto", "tools": ["start_task"]}]}),
                Some("step_result") => json!({}),
                _ => json!({"error": {"code": "unexpected"}}),
            }
        })
        .await;
        (client, asked)
    }

    /// The steps `asked` holds, as (tool, arguments).
    fn steps_of(asked: &[Value]) -> Vec<(String, Value)> {
        (asked.iter())
            .filter(|request| request["op"] == "approve_step")
            .map(|request| {
                let tool = request["name"].as_str().unwrap_or_default().to_owned();
                (tool, request["arguments"].clone())
            })
            .collect()
    }

    async fn add(client: &Arc<Client>, add: Value) -> Answer {
        let call = json!({"bot": "watch", "turn": 1, "call_id": "c1", "name": AUTOMATIONS,
            "arguments": {"add": add}});
        let none = Path::new("/nowhere");
        automate(client, &call, none, none, none, Place::Free).await
    }

    #[tokio::test]
    async fn an_automation_that_only_wakes_its_agent_asks_nothing() {
        let (client, asked) = steps_daemon(&[]).await;
        for wakes in [
            json!({"every": "30m", "message": "Check the build."}),
            json!({"every": "30m", "agent": "watch", "reply_to": "watch", "message": "Check."}),
        ] {
            let _ = add(&client, wakes).await;
        }
        let asked = asked.lock().unwrap();
        assert_eq!(steps_of(&asked), [], "{asked:?}");
        assert!(
            !asked.iter().any(|request| request["op"] == "inspect"),
            "{asked:?}"
        );
    }

    #[tokio::test]
    async fn each_part_of_an_automation_is_asked_as_the_tool_it_amounts_to() {
        let (client, asked) = steps_daemon(&[]).await;
        let _ = add(
            &client,
            json!({"every": "1h", "if": "test -f done", "agent": "shop.lead",
                "reply_to": "shop.lead", "message": "The build is done."}),
        )
        .await;
        let _ = add(
            &client,
            json!({"in": "5m", "start": {"name": "check", "model": "openai/gpt"},
                "message": "Check it."}),
        )
        .await;
        let asked = asked.lock().unwrap();
        assert_eq!(
            steps_of(&asked),
            [
                ("shell".to_owned(), json!({"command": "test -f done"})),
                (
                    "send_message".to_owned(),
                    json!({"to": "shop.lead", "message": "The build is done."})
                ),
                (
                    "send_message".to_owned(),
                    json!({"to": "shop.lead", "message": ANSWERS})
                ),
                (
                    START.to_owned(),
                    json!({"name": "check", "brief": "Check it.", "model": "openai/gpt"})
                ),
            ],
            "{asked:?}"
        );
        // A start is judged as start_task is: its agent's sandbox read first.
        assert!(
            asked.iter().any(|request| request["op"] == "inspect"),
            "{asked:?}"
        );
        // Each allowed step is told how the call ended.
        assert_eq!(
            (asked
                .iter()
                .filter(|request| request["op"] == "step_result"))
            .count(),
            4,
            "{asked:?}"
        );
    }

    #[tokio::test]
    async fn a_refused_part_refuses_the_automation() {
        let (client, asked) = steps_daemon(&["send_message"]).await;
        let refused = add(
            &client,
            json!({"every": "1h", "if": "true", "agent": "shop.lead", "message": "Go."}),
        )
        .await;
        let (code, why) = refused.unwrap_err();
        assert_eq!(code, "automation_refused");
        assert!(
            why.contains("the agent it wakes (send_message) was not approved: no"),
            "{why}"
        );
        let asked = asked.lock().unwrap();
        // Nothing was added: the tool never ran, and the part allowed
        // before is told it failed.
        assert!(
            !asked.iter().any(|request| request["op"] == "bots"),
            "{asked:?}"
        );
        let told: Vec<&Value> = (asked.iter())
            .filter(|request| request["op"] == "step_result")
            .collect();
        assert_eq!(told.len(), 1, "{asked:?}");
        assert_eq!(told[0]["failed"], true, "{told:?}");
    }

    #[test]
    fn a_standing_automation_s_parts_are_its_add_s() {
        use crate::trigger::{Target, Writer};
        let mut trigger = crate::trigger::Trigger {
            generation: "test-1".into(),
            not_before: None,
            name: "watch".into(),
            target: Target::Bot {
                name: "watch".into(),
                id: 7,
            },
            when: "every 30m".into(),
            at: None,
            commit: None,
            dir: None,
            reply_to: None,
            gate: None,
            gate_by: None,
            runs: None,
            until: None,
            turn_budget_tokens: None,
            budget_counts: None,
            file: None,
            daemon: crate::trigger::Daemon {
                store: None,
                socket: None,
            },
            store_id: "00ab".into(),
            message: "Check.".into(),
            shadow: false,
            live: false,
        };
        let caller = |store: &str, id: i64| crate::trigger::Caller {
            id: Some(id),
            store: Some(store.into()),
        };
        let watch = caller("00ab", 7);
        assert_eq!(standing_parts(&watch, &trigger), []);
        // Another agent's: the message to it.
        assert_eq!(
            standing_parts(&caller("00ab", 8), &trigger),
            [Part::message("watch", "the agent it wakes", "Check.")]
        );
        // So is an agent of the same id in another store sharing this HOME.
        assert_eq!(
            standing_parts(&caller("00cd", 7), &trigger),
            [Part::message("watch", "the agent it wakes", "Check.")]
        );
        // An if the agent wrote runs in its sandbox; anyone else's outside,
        // the same name's in another store too.
        trigger.gate = Some("make check".into());
        trigger.dir = Some(PathBuf::from("/work"));
        trigger.gate_by = Some(Writer::Agent("watch".into(), 7));
        assert_eq!(
            standing_parts(&watch, &trigger),
            [Part::check("make check", Some(Path::new("/work")), false)]
        );
        assert_eq!(
            standing_parts(&caller("00cd", 7), &trigger)[0].arguments["sandbox_permissions"],
            "require_escalated"
        );
        trigger.gate_by = Some(Writer::Person);
        assert_eq!(
            standing_parts(&watch, &trigger)[0].arguments["sandbox_permissions"],
            "require_escalated"
        );
        trigger.gate = None;
        trigger.target = Target::Start {
            name: "check".into(),
            model: "openai/gpt".into(),
            effort: None,
            by: Some(("watch".into(), 7)),
            fresh: false,
        };
        assert_eq!(
            standing_parts(&watch, &trigger),
            [Part::start("check", "openai/gpt", None, "Check.")]
        );
    }

    #[tokio::test]
    async fn a_confined_agent_no_one_reviews_is_refused_a_task() {
        // The listing leaves the sandbox out; the bot's own record says it.
        let client = crate::tests::daemon_answering(|asked| match asked["op"].as_str() {
            Some("bots") => json!({"bots": [{"name": "calc", "bot_id": 7, "gates": []}]}),
            Some("inspect") if asked["bot_id"] == 7 || asked["bot_id"].is_null() => {
                json!({"name": "calc",
                "bot_id": 7, "gates": [], "settings": {"sandbox": "read-only"}})
            }
            _ => json!({"error": {"code": "unexpected"}}),
        })
        .await;
        let call = json!({"bot": "calc", "turn": 1, "name": START,
            "arguments": {"name": "fix", "brief": "Fix it."}});
        let none = Path::new("/nowhere");
        let refused = answer(&client, &call, none, none, none, Place::Free).await;
        assert_eq!(refused.map_err(|(code, _)| code), Err("escalation_refused"));
        // A script calls MCP servers, which run as the person too.
        let script_call = json!({"bot": "calc", "turn": 1, "call_id": "c1", "name": EXEC,
            "arguments": {"input": "1"}});
        let hub = crate::mcp::Hub::new(None);
        let refused = script(&hub, &client, &script_call).await;
        assert_eq!(refused.map_err(|(code, _)| code), Err("escalation_refused"));
    }

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
        // `exec` comes and goes with the person's MCP servers, and
        // `browser` with agent-browser, and `desktop` with agent-desktop.
        let names = |tools: Vec<Value>| -> Vec<String> {
            (tools.iter().map(|t| str_of(&t["name"])))
                .filter(|name| name != EXEC && name != BROWSER && name != DESKTOP)
                .collect()
        };
        let every = [FOLLOWS, SHOW, AUTOMATIONS, MEMORY, WORKFLOW, COMMONS];
        // Every agent starts and removes tasks; the daemon bounds how deep.
        let mut task = vec![START, REMOVE];
        task.extend(every);
        assert_eq!(names(tools(Role::TASK)), task);
        let mut lead = vec![START, REMOVE, PROJECT];
        lead.extend(every);
        assert_eq!(names(tools(Role::LEAD)), lead);
        for tool in tools(Role::LEAD).into_iter().filter(|t| t["name"] != EXEC) {
            assert_eq!(tool["answered_by"], TAG);
            // Making projects alone is a lead's own, never its tasks'.
            let name = str_of(&tool["name"]);
            assert_eq!(tool.get("inherit").is_some(), name == PROJECT);
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
        let picture = json!({"type": "image", "media_type": "image/png", "data": "AA=="});
        let parts = cut_parts(
            json!([{"type": "text", "text": "x".repeat(OUTPUT_BYTES - 2)},
            picture, {"type": "text", "text": "abcd"}, picture, {"type": "text", "text": "gone"}]),
        );
        assert_eq!(parts[0]["text"].as_str().unwrap().len(), OUTPUT_BYTES - 2);
        assert_eq!(
            (&parts[1], &parts[2]["text"], &parts[3]),
            (&picture, &json!("ab\n[cut]"), &picture)
        );
        assert_eq!(parts.as_array().unwrap().len(), 4);
        assert_eq!(code("task_not_started"), "task_not_started");
        assert_eq!(code("tasks_exist"), "tasks_exist");
        assert_eq!(code("margin_full"), "margin_full");
        assert_eq!(code("ask_refused"), "ask_refused");
        assert_eq!(code("commons_refused"), "commons_refused");
        assert_eq!(code("live_template"), "live_template");
        let refused = refused_as("memory_refused", "live_template: the text: line 1".into());
        assert_eq!(refused, ("live_template", "the text: line 1".to_owned()));
        let refused = refused_as("memory_refused", "invalid_type: x".into());
        assert_eq!(refused, ("memory_refused", "invalid_type: x".to_owned()));
        assert_eq!(code("Anything Else"), "tool_failed");
    }
}
