//! Shell commands Sonny knows the meaning of, said as what they do: an
//! `agent` call starts, messages, waits on or removes an agent; a `rex` call
//! opens, reads or types into a window; Sonny's memory and the trigger
//! scripts save facts and schedule jobs. A command line is read a command
//! at a time; a pipe is said with its command and what came out of it.
//! The tools Sonny declares are said here too: a question to the person
//! and the plan.
use ratatui::{
    style::{Color, Stylize},
    text::{Line, Span},
};
use serde_json::Value;

/// A call said as an action: a verb, what it acted on, the lines under it,
/// and whether it failed.
pub struct Said {
    pub verb: String,
    pub what: String,
    pub under: Vec<Line<'static>>,
    pub failed: bool,
    /// What the call printed, shown as a command's output is: what a
    /// pipe made of it, or a remote command's own output.
    pub output: Option<String>,
}

/// What a call that failed tried to do: its past-tense verb turned to
/// `Couldn't` and the verb's base, `Started fix` read as `Couldn't start
/// fix`, so a refused call never reads as done.
pub fn attempted(verb: &str) -> String {
    let (first, rest) = verb.split_once(' ').unwrap_or((verb, ""));
    let base = match first {
        "Added" => "add",
        "Answered" => "answer",
        "Asked" => "ask",
        "Checked" => "check",
        "Cleaned" => "clean",
        "Cleared" => "clear",
        "Discarded" => "discard",
        "Ran" => "run",
        "Followed" => "follow",
        "Forked" => "fork",
        "Indexed" => "index",
        "Interrupted" => "interrupt",
        "Left" => "leave",
        "Listed" => "list",
        "Looked" => "look",
        "Moved" => "move",
        "Opened" => "open",
        "Paused" => "pause",
        "Pruned" => "prune",
        "Queued" => "queue",
        "Read" => "read",
        "Removed" => "remove",
        "Resolved" => "resolve",
        "Saved" => "save",
        "Scheduled" => "schedule",
        "Started" => "start",
        "Steered" => "steer",
        "Typed" => "type",
        "Unfollowed" => "unfollow",
        "Updated" => "update",
        "Waited" => "wait",
        _ => return format!("Failed: {}", verb.to_lowercase()),
    };
    match rest {
        "" => format!("Couldn't {base}"),
        rest => format!("Couldn't {base} {rest}"),
    }
}

pub fn said(command: &str, output: &str, failed: bool) -> Option<Said> {
    let simple = crate::shell::simple(command)?;
    let mut words = simple.words.as_slice();
    // `NAME=$(COMMAND)` keeps what one command prints: that command.
    if let [only] = words
        && let Some((name, value)) = only.split_once('=')
        && !name.is_empty()
        && name.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_')
        && let Some(inner) = value.strip_prefix("$(").and_then(|v| v.strip_suffix(')'))
    {
        return said(inner, output, failed);
    }
    // What only sets up the call: `env`, `VAR=value`, `if`, a loop's `do`.
    while let Some(first) = words.first() {
        let setup = matches!(
            first.as_str(),
            "env"
                | "command"
                | "exec"
                | "time"
                | "nohup"
                | "do"
                | "then"
                | "else"
                | "if"
                | "while"
                | "until"
                | "!"
        ) || (first.contains('=') && !first.starts_with('-'));
        if setup {
            words = &words[1..];
        } else if first == "timeout" && words.len() > 2 {
            words = &words[2..];
        } else {
            break;
        }
    }
    let (program, args) = words.split_first()?;
    let name = program.rsplit('/').next().unwrap_or(program);
    let stdin = simple.stdin.as_deref();
    // A pipe into more than `head`, `tail` or `jq` (which only cut or pick
    // from it) prints what it made of the output, not the output itself.
    let piped = simple
        .piped
        .as_deref()
        .filter(|p| !matches!(p.split_whitespace().next(), Some("head" | "tail" | "jq")));
    let printed = if piped.is_some() { "" } else { output };
    let mut said = match name {
        "$AGENT_BIN" | "${AGENT_BIN}" | "agent" => agent(args, stdin, printed, failed),
        "$REX" | "${REX}" | "rex" => rex(args, printed, failed),
        "memory" if program.contains(".agent/memory") => Some(memory(args, printed, failed)),
        // `trigger`, as the script was named before 0.8.0, in history.
        "automation" | "trigger"
            if program.contains(".agent/automation") || program.contains(".agent/trigger") =>
        {
            automation(args, printed, failed)
        }
        "sonny" if args.first().is_some_and(|a| a == "agent") => {
            agent(&args[1..], stdin, printed, failed)
        }
        _ => None,
    }?;
    // Where its output went, and what came out.
    if let Some(piped) = piped {
        said.what = format!("{} | {}", said.what, first_line(piped))
            .trim_start()
            .to_owned();
        said.output = Some(output.to_owned());
    }
    Some(said)
}

pub use crate::shell::parts;

/// Flags and their values, and what follows `--`.
struct Args<'a> {
    words: &'a [String],
    rest: Option<String>,
}

impl<'a> Args<'a> {
    fn new(words: &'a [String]) -> Self {
        match words.iter().position(|w| w == "--") {
            Some(at) => Self {
                words: &words[..at],
                rest: Some(words[at + 1..].join(" ")),
            },
            None => Self { words, rest: None },
        }
    }

    fn value(&self, flags: &[&str]) -> Option<&'a str> {
        let words = self.words;
        for (i, word) in words.iter().enumerate() {
            for flag in flags {
                if word == flag {
                    return words.get(i + 1).map(String::as_str);
                }
                if let Some(value) = word.strip_prefix(&format!("{flag}=")) {
                    return Some(value);
                }
            }
        }
        None
    }

    fn has(&self, flag: &str) -> bool {
        self.words.iter().any(|w| w == flag)
    }

    /// Words that are neither flags nor their values, after the first `skip`.
    fn plain(&self, skip: usize) -> Vec<&'a str> {
        let mut plain = Vec::new();
        let mut words = self.words.iter().skip(skip).peekable();
        while let Some(word) = words.next() {
            // `${VAR:+--flag "$VAR"}` is an option or nothing.
            if word.starts_with("${") {
                continue;
            }
            if word.starts_with("--") {
                if !word.contains('=') && !SWITCHES.contains(&word.as_str()) {
                    words.next();
                }
            } else if word.starts_with('-') && word.len() == 2 {
                words.next();
            } else {
                plain.push(word.as_str());
            }
        }
        plain
    }
}

/// Flags of `agent` and `rex` that take no value.
const SWITCHES: &[&str] = &[
    "--new",
    "--detach",
    "--pretty",
    "--no-spawn",
    "--agents",
    "--no-compaction",
    "--discover",
    "--all",
    "--any",
    "--full",
    "--active",
    "--newest",
    "--json",
    "--trim",
    "--unwrap",
    "--clear",
    "--wait",
    "--keep-open",
];

fn first_line(text: &str) -> String {
    text.lines()
        .map(str::trim)
        .find(|l| !l.is_empty())
        .unwrap_or("")
        .to_owned()
}

/// Each of `names` once, in the order first named: a wait on three turns
/// of one agent names it once.
pub fn once(names: impl Iterator<Item = String>) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    for name in names {
        if !out.contains(&name) {
            out.push(name);
        }
    }
    out
}

fn dim(text: impl Into<String>) -> Line<'static> {
    Line::from(text.into()).dim()
}

/// What a message to an agent did, from what the daemon answered when it
/// did not refuse, else from what it was sent as: it started a turn,
/// joined the running one, waits behind the agent's work or for room to
/// run, or is a note for the agent's next model call. Only `reject`
/// refuses a busy agent, so a `bot_busy` was a turn that could not start.
fn delivered(flag: Option<&str>, reply: Option<&Value>) -> &'static str {
    let status = reply.and_then(|reply| reply["status"].as_str());
    let busy = reply.is_some_and(|reply| reply["error"] == "bot_busy");
    match (status, flag) {
        (Some("running"), _) => "Started a turn for",
        (Some("queued"), Some("steer") | None) => "Steered",
        (Some("queued" | "ready"), _) => "Queued for",
        (Some("attached"), _) => "Left a note for",
        _ if busy => "Started a turn for",
        (_, Some("queue")) => "Queued for",
        (_, Some("attach")) => "Left a note for",
        (_, Some("reject")) => "Started a turn for",
        _ => "Steered",
    }
}

/// Why a script refused, from the `{"error", "detail"}` it printed.
fn refused(reply: &Value) -> Option<String> {
    let code = reply["error"].as_str()?;
    let detail = reply["detail"].as_str().filter(|d| !d.is_empty());
    Some(match detail {
        Some(detail) => format!("{detail} ({code})"),
        None => code.replace('_', " "),
    })
}

/// The app's memory script: what it saved or removed, by name.
fn memory(words: &[String], output: &str, failed: bool) -> Said {
    let reply: Value = serde_json::from_str(output.trim()).unwrap_or(Value::Null);
    let refusal = refused(&reply);
    let name = || words.get(1).cloned().unwrap_or_default();
    let (verb, what) = match words.first().map(String::as_str) {
        Some("show") | None => ("Read the memory indexes".into(), String::new()),
        Some("save") => {
            let what = reply["saved"].as_str().map_or_else(name, str::to_owned);
            let verb = if reply["duplicate"] == true {
                "Already in memory"
            } else if reply["replaced"] == true {
                "Updated in memory"
            } else {
                "Saved to memory"
            };
            (verb.into(), what)
        }
        Some("rm") => ("Removed from memory".into(), name()),
        Some("read") => ("Read from memory".into(), name()),
        Some("search") => {
            let searched: Vec<&str> = (words[1..].iter())
                .map(String::as_str)
                .take_while(|word| !word.starts_with("--"))
                .collect();
            ("Searched memory".into(), searched.join(" "))
        }
        Some("said") => ("Read what the person wrote".into(), String::new()),
        Some(other) => (format!("Used memory {other}"), String::new()),
    };
    Said {
        verb,
        what,
        under: refusal.map(Line::from).into_iter().collect(),
        failed: failed || reply.get("error").is_some(),
        output: None,
    }
}

/// The app's automation script: one scheduled, run or removed.
fn automation(words: &[String], output: &str, failed: bool) -> Option<Said> {
    let (verb, rest) = words.split_first()?;
    let args = Args::new(rest);
    let reply: Value = serde_json::from_str(output.trim()).unwrap_or(Value::Null);
    let refusal = refused(&reply);
    let named = args
        .plain(0)
        .first()
        .map(|n| n.to_string())
        .unwrap_or_default();
    let mut said = Said {
        verb: String::new(),
        what: named,
        under: Vec::new(),
        failed: failed || refusal.is_some(),
        output: None,
    };
    match verb.as_str() {
        "add" => {
            said.verb = "Scheduled".into();
            let name = args
                .value(&["--name"])
                .or(args.value(&["--bot", "--start"]));
            let when = [
                ("--every", "every"),
                ("--cron", "cron"),
                ("--at", "at"),
                ("--in", "in"),
            ]
            .into_iter()
            .find_map(|(flag, word)| Some(format!("{word} {}", args.value(&[flag])?)));
            said.what = [name.map(str::to_owned), when]
                .into_iter()
                .flatten()
                .collect::<Vec<_>>()
                .join(" · ");
            if let Some(message) = args.rest.as_deref().filter(|m| !m.trim().is_empty()) {
                said.under
                    .push(Line::from(first_line(message)).italic().dim());
            }
        }
        "rm" => said.verb = "Removed the automation".into(),
        "run" | "fire" => said.verb = "Ran the automation".into(),
        "ls" => said.verb = "Listed the automations".into(),
        _ => return None,
    }
    if let Some(refusal) = refusal {
        said.under.push(Line::from(refusal));
    }
    Some(said)
}

fn agent(words: &[String], stdin: Option<&str>, output: &str, failed: bool) -> Option<Said> {
    if words
        .iter()
        .take_while(|w| w.starts_with('-'))
        .any(|w| w == "--help" || w == "-h")
    {
        return Some(Said {
            verb: "Read agent's help".into(),
            what: String::new(),
            under: Vec::new(),
            failed,
            output: None,
        });
    }
    // Options may come before the command.
    let mut words = words;
    while let Some(first) = words.first().filter(|w| w.starts_with("--")) {
        let skip = if SWITCHES.contains(&first.as_str()) || first.contains('=') {
            1
        } else {
            2
        };
        words = words.get(skip..)?;
    }
    let Some((verb, rest)) = words.split_first() else {
        // `agent --help`, or options alone.
        return Some(Said {
            verb: "Read agent's help".into(),
            what: String::new(),
            under: Vec::new(),
            failed,
            output: None,
        });
    };
    let args = Args::new(rest);
    if verb == "help" || args.has("--help") || args.has("-h") {
        let topic = if verb == "help" {
            args.plain(0)
                .first()
                .map(|t| t.to_string())
                .unwrap_or_default()
        } else {
            verb.clone()
        };
        return Some(Said {
            verb: "Read the help for".into(),
            what: format!("agent {topic}").trim().to_owned(),
            under: Vec::new(),
            failed,
            output: None,
        });
    }
    let bot = args.value(&["--bot"]).map(str::to_owned);
    let reply: Option<Value> = serde_json::from_str(output.trim()).ok();
    let refusal = reply.as_ref().and_then(refused);
    let mut said = Said {
        verb: String::new(),
        what: bot.clone().unwrap_or_default(),
        under: Vec::new(),
        failed: failed || refusal.is_some(),
        output: None,
    };
    match verb.as_str() {
        "run" => {
            let new = args.has("--new") || bot.is_none();
            if bot.is_none() {
                said.what = "a new agent".into();
            }
            // Unsaid, a run steers, or queues when it names a folder or a
            // turn cap, as the agent CLI resolves it.
            let own_turn = args.value(&["--workspace", "--turn-budget-tokens"]);
            let flag = args.value(&["--delivery"]).or(own_turn.and(Some("queue")));
            said.verb = match new {
                true => "Started",
                false => delivered(flag, reply.as_ref()),
            }
            .into();
            if let Some(reply) = &reply
                && refusal.is_none()
            {
                // A message in line has no turn yet; the one it opens has
                // its number.
                if let Some(turn) = reply["turn"].as_i64().or_else(|| reply["message"].as_i64()) {
                    said.what = format!(
                        "{} · turn {turn}",
                        reply["bot"].as_str().unwrap_or(&said.what)
                    );
                }
                // Run without --detach waits and prints the turn's ending.
                if let Some(text) = reply["text"].as_str() {
                    said.under.push(dim(first_line(text)));
                }
            }
            // The prompt: its operands, `-` for the here-document.
            let mut prompt = args.plain(0).join(" ");
            if let Some(rest) = &args.rest {
                prompt = [prompt.as_str(), rest].join(" ").trim().to_owned();
            }
            if prompt == "-" || prompt.is_empty() {
                prompt = stdin.unwrap_or("").to_owned();
            }
            if !prompt.trim().is_empty() {
                said.under
                    .insert(0, Line::from(first_line(&prompt)).italic().dim());
            }
        }
        "wait" => {
            said.verb = "Waited for".into();
            if said.what.is_empty() {
                said.what = once(args.plain(0).iter().map(|h| handle_bot(h))).join(", ");
            }
            if let Some(lines) = reply.as_ref().and_then(waited) {
                said.under.extend(lines.into_iter().map(dim));
            }
        }
        "ls" => {
            said.verb = match args.value(&["--name"]) {
                // The project's own, by the lead's name or spelled out.
                Some(pattern) if pattern.starts_with("${AGENT_BOT%%.*}") => {
                    "Listed this project's agents".into()
                }
                Some("*.lead") => "Listed every project's lead".into(),
                Some(pattern) => match pattern.strip_suffix(".*") {
                    Some(project) if !project.contains(['*', '?', '[']) => {
                        format!("Listed {project}'s agents")
                    }
                    _ => format!("Listed agents named {pattern}"),
                },
                None if args.has("--active") => "Listed running agents".into(),
                None => "Listed agents".into(),
            };
            // One agent a row, as JSON or as `--pretty` prints it.
            let rows: Vec<(String, String)> = match reply
                .as_ref()
                .and_then(|r| r.as_array().or_else(|| r["bots"].as_array()).cloned())
            {
                Some(rows) => rows
                    .iter()
                    .filter_map(|row| {
                        let name = row["name"].as_str().or_else(|| row["bot"].as_str())?;
                        Some((
                            name.to_owned(),
                            row["status"].as_str().unwrap_or("").to_owned(),
                        ))
                    })
                    .collect(),
                None => output.lines().filter_map(listed_row).collect(),
            };
            for (name, status) in rows {
                said.under.push(Line::from(vec![
                    crate::status::mark(&status),
                    Span::raw(name),
                    Span::raw(format!(" {status}")).dim(),
                ]));
            }
        }
        "rm" => said.verb = "Removed".into(),
        "fork" => {
            said.verb = "Forked".into();
            let source = args.value(&["--source"]).unwrap_or("");
            said.what = format!("{source} as {}", said.what);
        }
        "follow" => {
            said.verb = "Followed".into();
            if args.has("--all") {
                said.what = "every agent".into();
            }
        }
        // It drops a bot's old replay records; the agent and its history stay.
        "prune" => said.verb = "Pruned the old turn records of".into(),
        "stats" => said.verb = "Read the daemon's stats".into(),
        "interrupt" => said.verb = "Interrupted".into(),
        "inspect" => said.verb = "Looked up".into(),
        "turns" | "history" => said.verb = "Read the turns of".into(),
        "models" => said.verb = "Listed models".into(),
        "approvals" => said.verb = "Checked approvals".into(),
        "answer" => said.verb = "Answered an approval".into(),
        _ => return None,
    }
    if let Some(refusal) = refusal {
        said.under.push(Line::from(refusal));
    }
    Some(said)
}

/// A row of `agent ls --pretty`: its name and status.
pub fn listed_row(line: &str) -> Option<(String, String)> {
    let mut fields = line.split_whitespace();
    let (name, status) = (fields.next()?, fields.next()?);
    STATUSES
        .contains(&status)
        .then(|| (name.to_owned(), status.to_owned()))
}

const STATUSES: &[&str] = &[
    "running",
    "completed",
    "idle",
    "queued",
    "failed",
    "interrupted",
    "waiting",
    "taken",
    "blocked",
    "paced",
];

/// Whether a command line only runs `agent` commands that do the same in
/// the agent's sandbox as out of it, on its own daemon, as itself: a
/// message to an agent there is, a list, turns read, a wait, a look-up,
/// models, usage, audit, stats, or a fork that may write nothing, with no
/// flag but those known to change nothing the sandbox decides. Another
/// store or socket, a new agent, a move, models discovered into the home
/// or anything else on the line is not.
pub fn agent_inside(command: &str) -> bool {
    let parts = crate::shell::parts(command);
    !parts.is_empty()
        && parts.iter().all(|part| {
            let Some(simple) = crate::shell::simple(part)
                .filter(|_| !writes_elsewhere(part) && !substitutes(part))
            else {
                return false;
            };
            // Every stage after it only cuts what it prints.
            let mut piped = simple.piped.clone();
            while let Some(stage) = piped {
                let Some(next) = crate::shell::simple(&stage).filter(|_| !writes_elsewhere(&stage))
                else {
                    return false;
                };
                if !matches!(
                    next.words.first().map(String::as_str),
                    Some("head" | "tail" | "jq")
                ) {
                    return false;
                }
                piped = next.piped;
            }
            let Some((program, words)) = simple.words.split_first() else {
                return false;
            };
            if !matches!(program.as_str(), "$AGENT_BIN" | "${AGENT_BIN}" | "agent") {
                return false;
            }
            // Only the flags named here, each known to change nothing the
            // sandbox decides: a flag that moves a bot, sets its model or
            // tools, reads a file or names another daemon is not.
            let mut words = words;
            while let Some(first) = words.first().filter(|w| w.starts_with("--")) {
                if !matches!(first.as_str(), "--pretty" | "--no-spawn") {
                    return false;
                }
                words = &words[1..];
            }
            let Some((verb, rest)) = words.split_first() else {
                return false;
            };
            let args = Args::new(rest);
            let only = |valued: &[&str], switches: &[&str], positional: bool| {
                let mut words = args.words.iter();
                while let Some(word) = words.next() {
                    let Some(flag) = word.strip_prefix("--") else {
                        if !positional {
                            return false;
                        }
                        continue;
                    };
                    let (name, inline) = match flag.split_once('=') {
                        Some((name, _)) => (name, true),
                        None => (flag, false),
                    };
                    if valued.contains(&name) {
                        if !inline && words.next().is_none() {
                            return false;
                        }
                    } else if inline
                        || !(switches.contains(&name) || ["pretty", "no-spawn"].contains(&name))
                    {
                        return false;
                    }
                }
                true
            };
            match verb.as_str() {
                "run" => {
                    args.value(&["--bot"]).is_some()
                        && only(
                            &[
                                "bot",
                                "bot-id",
                                "delivery",
                                "quote",
                                "request-id",
                                "attach-wait",
                            ],
                            &["detach"],
                            true,
                        )
                }
                "fork" => {
                    args.value(&["--allow"]) == Some("")
                        && only(
                            &["source", "bot", "checkpoint", "allow", "request-id"],
                            &[],
                            true,
                        )
                }
                "models" => only(&[], &[], false),
                "ls" => only(&["name", "created-by", "limit"], &["active"], false),
                "turns" | "messages" => only(&["bot", "after", "limit"], &["newest"], false),
                "wait" => only(&["bot", "bot-id", "timeout"], &["any"], true),
                "inspect" => only(&["bot", "bot-id"], &[], false),
                "usage" => only(&["name", "since", "bucket", "limit"], &[], false),
                "audit" => only(
                    &[
                        "bot", "turn", "kind", "place", "approved", "access", "outcome", "tool",
                        "since", "before", "limit",
                    ],
                    &[],
                    false,
                ),
                "stats" => only(&[], &[], false),
                _ => false,
            }
        })
}

/// Whether a command sends its output somewhere by a redirection, which
/// the sandbox may not let it write: any `>` outside quotes but a copy of
/// one stream to another (`2>&1`) or a discard (`>/dev/null`). A
/// here-document's text, after the first line, is the command's input.
fn writes_elsewhere(command: &str) -> bool {
    let line = command.lines().next().unwrap_or("");
    let (mut quote, mut chars) = (None, line.char_indices().peekable());
    while let Some((at, c)) = chars.next() {
        match (quote, c) {
            (Some(q), c) if c == q => quote = None,
            (Some(_), _) => {}
            (None, '\'') | (None, '"') => quote = Some(c),
            (None, '\\') => {
                chars.next();
            }
            (None, '>') => {
                let rest = line[at + 1..].trim_start_matches('>').trim_start();
                let duplicated =
                    rest.starts_with('&') && rest[1..].starts_with(|c: char| c.is_ascii_digit());
                if !duplicated && !rest.starts_with("/dev/null") {
                    return true;
                }
            }
            _ => {}
        }
    }
    false
}

/// Whether a command runs another inside it, whose own reach is not
/// known: `$(...)` or a backquote outside single quotes, or `<(...)` and
/// `>(...)` outside quotes; in a here-document's text too, unless its
/// delimiter is quoted.
fn substitutes(command: &str) -> bool {
    let (line, body) = command.split_once('\n').unwrap_or((command, ""));
    let (mut quote, mut chars) = (None, line.chars().peekable());
    while let Some(c) = chars.next() {
        let next = chars.peek().copied();
        match (quote, c) {
            (Some('\''), '\'') => quote = None,
            (Some('\''), _) => {}
            (_, '`') => return true,
            (_, '$') if next == Some('(') => return true,
            (Some('"'), '"') => quote = None,
            (Some(_), '\\') | (None, '\\') => {
                chars.next();
            }
            (Some(_), _) => {}
            (None, '\'' | '"') => quote = Some(c),
            (None, '<' | '>') if next == Some('(') => return true,
            _ => {}
        }
    }
    let expands = line.split_once("<<").is_some_and(|(_, rest)| {
        !rest
            .trim_start_matches('-')
            .trim_start()
            .starts_with(['\'', '"', '\\'])
    });
    expands && (body.contains("$(") || body.contains('`'))
}

/// The bot and number a `BOT/N` handle names.
pub fn handle_parts(handle: &str) -> Option<(&str, i64)> {
    let (bot, number) = handle.rsplit_once('/')?;
    if bot.is_empty() || bot.contains(['/', ':']) || !number.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    Some((bot, number.parse().ok()?))
}

/// The bot a `BOT/N` handle names.
pub fn handle_bot(handle: &str) -> String {
    handle_parts(handle)
        .map_or(handle, |(bot, _)| bot)
        .to_owned()
}

/// The task turns a wait returned completed, by bot and turn, from what
/// it printed: the wait tool's result, or `agent wait` lines among a
/// command's output.
pub fn returned(printed: &str) -> Vec<(String, i64)> {
    let replies: Vec<Value> = match serde_json::from_str(printed.trim()) {
        Ok(whole) => vec![whole],
        Err(_) => printed
            .lines()
            .filter_map(|line| serde_json::from_str(line.trim()).ok())
            .collect(),
    };
    let mut turns = Vec::new();
    for reply in replies {
        for (handle, result) in reply["results"].as_object().into_iter().flatten() {
            if result["status"] != "completed" {
                continue;
            }
            if let Some((bot, turn)) = handle_parts(handle) {
                turns.push((bot.to_owned(), turn));
            }
        }
    }
    turns
}

/// A wait's reply as lines: `BOT completed: first line of its reply`.
pub fn waited(reply: &Value) -> Option<Vec<String>> {
    let mut lines = Vec::new();
    for (handle, result) in reply["results"].as_object()? {
        let who = result["bot"]
            .as_str()
            .map(str::to_owned)
            .unwrap_or_else(|| handle_bot(handle));
        let line = match (result["status"].as_str(), result["error"].as_str()) {
            (_, Some(error)) => format!("{who}: {error}"),
            (Some(status), _) => match first_line(result["text"].as_str().unwrap_or("")) {
                text if text.is_empty() => format!("{who} {status}"),
                text => format!("{who} {status}: {text}"),
            },
            _ => continue,
        };
        if !lines.contains(&line) {
            lines.push(line);
        }
    }
    let pending = reply["pending"].as_array().map_or(0, Vec::len);
    if pending > 0 {
        lines.push(format!("{pending} still running"));
    }
    Some(lines)
}

/// A Rex target as a person reads it: a label as is, an id by its end.
fn target(target: &str) -> String {
    match target.split_once(':') {
        Some((_, id)) if id.chars().count() > 8 => {
            // By characters: an id is whatever the command's text held.
            let end: String = id.chars().skip(id.chars().count() - 6).collect();
            format!("…{end}")
        }
        _ => target.to_owned(),
    }
}

fn rex(words: &[String], output: &str, failed: bool) -> Option<Said> {
    // `-S HOST` picks the server and may come first.
    let (host, words) = match words {
        [flag, host, rest @ ..] if flag == "-S" => (Some(host.as_str()), rest),
        _ => (None, words),
    };
    let (verb, rest) = words.split_first()?;
    let (verb, rest) = match (verb.as_str(), rest.split_first()) {
        ("block" | "window" | "session" | "__internal" | "skill", Some((sub, rest))) => {
            (format!("{verb} {sub}"), rest)
        }
        _ => (verb.clone(), rest),
    };
    let args = Args::new(rest);
    let block = args.value(&["-b", "--block"]).map(target);
    let session = args.value(&["-s", "--session"]);
    let place = |what: String| match (session, host) {
        (Some(s), Some(h)) => format!("{what} ({s} on {h})"),
        (Some(s), None) => format!("{what} ({s})"),
        (None, Some(h)) => format!("{what} (on {h})"),
        (None, None) => what,
    };
    let mut said = Said {
        verb: String::new(),
        what: String::new(),
        under: Vec::new(),
        failed,
        output: None,
    };
    let command = args.rest.clone().unwrap_or_default();
    match verb.as_str() {
        "run" | "split" | "window new" => {
            said.verb = match verb.as_str() {
                "split" => "Opened a Rex split",
                _ => "Opened a Rex window",
            }
            .into();
            let label = args
                .value(&["--label", "-l"])
                .map(str::to_owned)
                .or_else(|| {
                    (verb == "window new")
                        .then(|| args.plain(0).first().map(|s| s.to_string()))
                        .flatten()
                });
            said.what = place(label.unwrap_or_default());
            if !command.is_empty() {
                said.under.push(dim(format!("$ {}", first_line(&command))));
            }
        }
        "capture" => {
            said.verb = "Read Rex block".into();
            said.what = place(block.unwrap_or_default());
            let lines: Vec<&str> = output.lines().filter(|l| !l.trim().is_empty()).collect();
            let shown = &lines[lines.len().saturating_sub(3)..];
            if lines.len() > shown.len() {
                said.under
                    .push(dim(format!("… {} lines above", lines.len() - shown.len())));
            }
            said.under.extend(shown.iter().map(|l| dim(l.to_string())));
        }
        "send" => {
            said.verb = "Typed into".into();
            said.what = block.unwrap_or_default();
            let text = args.plain(0).join(" ");
            said.under
                .push(Line::from(first_line(&text)).italic().dim());
        }
        "send-key" => {
            said.verb = format!("Pressed {}", args.plain(0).join(" "));
            said.what = block.map(|b| format!("in {b}")).unwrap_or_default();
        }
        "wait" => {
            said.verb = "Waited for Rex block".into();
            said.what = args.plain(0).first().map(|t| target(t)).unwrap_or_default();
        }
        "block close" | "window close" | "close" => {
            said.verb = "Closed Rex".into();
            said.what = args.plain(0).first().map(|t| target(t)).unwrap_or_default();
        }
        "__internal skill" | "skill show" | "skill" => {
            said.verb = "Read Rex's guide for agents".into();
        }
        "ls" | "block ls" | "window ls" | "hosts" => {
            said.verb = match verb.as_str() {
                "hosts" => "Listed Rex hosts",
                "ls" => "Listed Rex sessions",
                "block ls" => "Listed Rex blocks",
                _ => "Listed Rex windows",
            }
            .into();
            said.what = place(String::new());
        }
        _ => return None,
    }
    Some(said)
}

/// A call of a tool Sonny declares, said as what it does: a question to
/// the person with its choices, the plan it sets, a task it started,
/// removed or resolved, where a window or pane went, the project it made,
/// or the follow it changed.
pub fn declared(name: &str, arguments: &Value, failed: bool) -> Option<Said> {
    let field = |key: &str| arguments[key].as_str().map(str::trim);
    let said = |verb: &str, what: &str| Said {
        verb: verb.into(),
        what: what.to_owned(),
        under: Vec::new(),
        failed,
        output: None,
    };
    match name {
        crate::served::START => {
            let mut said = said("Started", field("name")?);
            if let Some(brief) = field("brief") {
                said.under
                    .push(Line::from(first_line(brief)).italic().dim());
            }
            Some(said)
        }
        crate::served::REMOVE => Some(said(
            match arguments["discard"] == true {
                true => "Discarded",
                false => "Cleaned up",
            },
            field("task")?,
        )),
        crate::served::SHOW => {
            let agent = field("agent")?;
            Some(match field("split") {
                Some(side) => {
                    let side = match side {
                        "right" => "right of",
                        "left" => "left of",
                        other => other,
                    };
                    let beside = field("beside").unwrap_or("you");
                    said("Moved", &format!("{agent} to a split {side} {beside}"))
                }
                None if crate::project::is_lead(agent) => said("Opened", agent),
                None => said("Moved", &format!("{agent} to a tab of its own")),
            })
        }
        crate::served::PROJECT => Some(said("Added the project", field("folder")?)),
        // `subscriptions`, as before 0.9.1, in history.
        crate::served::FOLLOWS | "subscriptions" => {
            Some(match (arguments["add"]["name"].as_str(), field("remove")) {
                (Some(added), _) => said("Followed", added),
                (None, Some(removed)) => said("Unfollowed", removed),
                (None, None) => said("Listed follows", ""),
            })
        }
        crate::served::MEMORY => Some(
            match (
                arguments["save"]["name"].as_str(),
                field("remove"),
                field("read"),
            ) {
                (Some(saved), _, _) => {
                    let mut said = said("Saved to memory", saved.trim());
                    if let Some(description) = arguments["save"]["description"].as_str() {
                        said.under
                            .push(Line::from(first_line(description)).italic().dim());
                    }
                    said
                }
                (None, Some(removed), _) => said("Removed from memory", removed),
                (None, None, Some(read)) => said("Read from memory", read),
                _ if field("search").is_some() => {
                    said("Searched memory", field("search").unwrap_or_default())
                }
                _ if arguments["index"] == true => said("Indexed memory", ""),
                _ if arguments["check"] == true => said("Checked memory", ""),
                _ if arguments["said"] == true => said("Read what the person wrote", ""),
                _ if arguments.get("cleanup").is_some() || arguments["changed"] == true => {
                    said("Cleaned up memory", "")
                }
                _ => said("Read memory", ""),
            },
        ),
        // `triggers` and its `fire`, as before 0.8.0, in history.
        crate::served::AUTOMATIONS | "triggers" => Some(
            match (
                arguments["add"].is_object(),
                field("remove"),
                field("run").or(field("fire")),
                field("show"),
            ) {
                (true, ..) => {
                    // As the automation script's add says it.
                    let add = &arguments["add"];
                    let name = (add["name"].as_str())
                        .or(add["start"]["name"].as_str())
                        .or(add["bot"].as_str())
                        .map(|name| name.trim().to_owned());
                    let when = ["every", "cron", "at", "in"]
                        .into_iter()
                        .find_map(|key| Some(format!("{key} {}", add[key].as_str()?.trim())));
                    let what = [name, when].into_iter().flatten().collect::<Vec<_>>();
                    let mut said = said("Scheduled", &what.join(" · "));
                    if let Some(message) = add["message"].as_str() {
                        said.under
                            .push(Line::from(first_line(message)).italic().dim());
                    }
                    said
                }
                (_, Some(removed), ..) => said("Removed the automation", removed),
                (_, _, Some(fired), _) => said("Ran the automation", fired),
                (_, _, _, Some(shown)) => said("Read the automation", shown),
                _ => said("Listed automations", ""),
            },
        ),
        crate::asks::TOOL => {
            let ask = crate::asks::call("", arguments)?;
            let mut said = Said {
                verb: "Asked you".into(),
                what: ask.question,
                under: Vec::new(),
                failed,
                output: None,
            };
            if !ask.options.is_empty() {
                let numbered: Vec<String> = ask
                    .options
                    .iter()
                    .enumerate()
                    .map(|(at, choice)| format!("{} {}", at + 1, choice.label))
                    .collect();
                said.under.push(dim(numbered.join(" · ")));
            }
            if let Some(command) = ask.command {
                said.under.push(dim(command));
            }
            Some(said)
        }
        crate::resolved::TOOL => Some(Said {
            verb: "Resolved".into(),
            what: match arguments["task"].as_str().map(str::trim) {
                Some(task) if !task.is_empty() => task.to_owned(),
                _ => "this task".into(),
            },
            under: Vec::new(),
            failed,
            output: None,
        }),
        crate::plan::TOOL => {
            let steps: Vec<String> = arguments["steps"]
                .as_array()?
                .iter()
                .map(|step| step.as_str().map(str::to_owned))
                .collect::<Option<_>>()?;
            let mut said = plan(&steps);
            said.failed = failed;
            Some(said)
        }
        _ => None,
    }
}

/// A call of the daemon's tools for other agents, said as the `agent` verbs
/// they stand for are: the message sent and how it landed, the agents
/// listed with each one's status, or the cord pulled and why.
pub fn agents(name: &str, arguments: &Value, output: Option<&str>, failed: bool) -> Option<Said> {
    let reply: Option<Value> = output.and_then(|o| serde_json::from_str(o.trim()).ok());
    let mut said = Said {
        verb: String::new(),
        what: String::new(),
        under: Vec::new(),
        failed,
        output: None,
    };
    match name {
        "send_message" => {
            let to = arguments["to"].as_str()?.trim();
            said.what = to.to_owned();
            let status = reply.as_ref().and_then(|r| r["status"].as_str());
            let interrupted = reply
                .as_ref()
                .is_some_and(|r| !r["interrupted_turn"].is_null());
            said.verb = match (status, arguments["delivery"].as_str()) {
                _ if interrupted => "Interrupted",
                // Asked to, but it had no turn running: a message alone.
                (Some("running"), _) => "Started a turn for",
                (None, _) if arguments["interrupt"] == true => "Interrupted",
                (_, Some("queue")) => "Queued for",
                _ => "Steered",
            }
            .into();
            // A message in line has no turn yet; the one it opens has its number.
            if let Some(turn) = reply
                .as_ref()
                .and_then(|r| r["handle"].as_str())
                .and_then(handle_parts)
                .map(|(_, n)| n)
            {
                said.what = format!("{to} · turn {turn}");
            }
            if let Some(message) = arguments["message"]
                .as_str()
                .filter(|m| !m.trim().is_empty())
            {
                said.under
                    .push(Line::from(first_line(message)).italic().dim());
            }
        }
        "list_agents" if arguments["approvals"] == true => {
            said.verb = "Checked approvals".into();
            let calls = reply
                .as_ref()
                .and_then(|r| r["approvals"].as_array().cloned());
            for call in calls.into_iter().flatten() {
                let agent = call["bot"].as_str().unwrap_or_default();
                let tool = call["tool"].as_str().unwrap_or_default();
                said.under.push(dim(format!("{agent} wants {tool}")));
            }
        }
        "list_agents" => {
            said.verb = match arguments["name"].as_str() {
                None if arguments["running"] == true => "Listed running agents".into(),
                None => "Listed this project's agents".into(),
                Some("*.lead") => "Listed every project's lead".into(),
                Some(pattern) => match pattern.strip_suffix(".*") {
                    Some(project) if !project.contains(['*', '?']) => {
                        format!("Listed {project}'s agents")
                    }
                    _ => format!("Listed agents named {pattern}"),
                },
            };
            let rows = reply.as_ref().and_then(|r| r["bots"].as_array().cloned());
            for row in rows.into_iter().flatten() {
                let status = row["status"].as_str().unwrap_or_default().to_owned();
                said.under.push(Line::from(vec![
                    crate::status::mark(&status),
                    Span::raw(row["name"].as_str().unwrap_or_default().to_owned()),
                    Span::raw(format!(" {status}")).dim(),
                ]));
            }
        }
        "pause_agents" => {
            said.verb = "Paused every agent".into();
            if let Some(reason) = arguments["reason"]
                .as_str()
                .filter(|r| !r.trim().is_empty())
            {
                said.under.push(dim(first_line(reason)));
            }
        }
        _ => return None,
    }
    Some(said)
}

/// A `plan` call said in one row: how far it has got and the step under
/// way, as an earlier plan reads once a newer one shows in full.
pub fn plan_short(items: &[String]) -> Said {
    let mut said = plan(items);
    said.under.clear();
    if items.is_empty() {
        return said;
    }
    let done = items.iter().filter(|item| item.starts_with("[x]")).count();
    let doing = (items.iter()).find_map(|item| item.strip_prefix("[>]").map(str::trim));
    said.what = match doing {
        _ if done == items.len() => format!("· all {done} done"),
        Some(step) => format!("· {done}/{} · → {step}", items.len()),
        None => format!("· {done}/{}", items.len()),
    };
    said
}

/// A `plan` call, as the plan it sets; no steps clears it.
pub fn plan(items: &[String]) -> Said {
    let mut said = Said {
        verb: "Updated the plan".into(),
        what: String::new(),
        under: Vec::new(),
        failed: false,
        output: None,
    };
    if items.is_empty() {
        said.verb = "Cleared the plan".into();
        return said;
    }
    for item in items {
        let (mark, text) = match item.split_at_checked(3) {
            Some(("[x]", text)) => ("✔", text),
            Some(("[>]", text)) => ("→", text),
            Some(("[ ]", text)) => ("□", text),
            // A step the bot sent without its mark is still a step.
            _ => ("□", item.as_str()),
        };
        let text = text.trim().to_owned();
        said.under.push(match mark {
            "✔" => Line::from(vec![
                Span::raw("✔ ").fg(Color::Green),
                Span::raw(text).dim(),
            ]),
            "→" => Line::from(vec![
                Span::raw("→ ").fg(Color::Cyan),
                Span::raw(text).bold(),
            ]),
            _ => Line::from(vec![Span::raw("□ ").dim(), Span::raw(text)]),
        });
    }
    said
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_failed_call_reads_as_what_it_tried() {
        assert_eq!(attempted("Started"), "Couldn't start");
        assert_eq!(
            attempted("Removed the automation"),
            "Couldn't remove the automation"
        );
        assert_eq!(attempted("Queued for"), "Couldn't queue for");
        assert_eq!(attempted("Saved to memory"), "Couldn't save to memory");
    }

    fn text(line: &Line) -> String {
        line.spans.iter().map(|s| s.content.as_ref()).collect()
    }

    #[test]
    fn a_prune_names_the_records_not_the_agent() {
        let said = said(
            "\"$AGENT_BIN\" prune --bot calc --keep-turns 8",
            "{}",
            false,
        )
        .unwrap();
        assert_eq!(
            format!("{} {}", said.verb, said.what),
            "Pruned the old turn records of calc"
        );
        assert_eq!(
            attempted(&said.verb),
            "Couldn't prune the old turn records of"
        );
    }

    #[test]
    fn a_brief_in_a_document_or_a_capture_still_reads_as_a_start() {
        let line = "h=$(\"$AGENT_BIN\" run --detach --new --bot calc.neg -- \"$(cat <<'EOF'\nAdd a negative test.\nKeep the rest.\nEOF\n)\" | jq -r .handle)";
        let said = said(line, "calc.neg/3", false).unwrap();
        assert_eq!(
            (said.verb.as_str(), said.what.as_str()),
            ("Started", "calc.neg")
        );
        assert_eq!(text(&said.under[0]), "Add a negative test.");
        assert_eq!(said.under.len(), 1);
        let said = said_or_none(
            "\"$AGENT_BIN\" --pretty run --bot calc.lead - <<'EOF'\nDone.\nEOF",
            "",
        );
        assert_eq!(
            (said.verb.as_str(), said.what.as_str()),
            ("Steered", "calc.lead")
        );
        assert_eq!(text(&said.under[0]), "Done.");
        let said = said_or_none("\"$AGENT_BIN\" run --bot calc.x \"Run the tests\" 2>&1", "");
        assert_eq!(text(&said.under[0]), "Run the tests");
    }

    #[test]
    fn starting_a_task_reads_as_its_name_turn_and_brief() {
        let said = said(
            r#""$AGENT_BIN" run --detach --new --model "$AGENT_MODEL" --bot calc.equal-test -- 'Add a test.
Keep the rest.'"#,
            r#"{"bot":"calc.equal-test","handle":"calc.equal-test/7","status":"running","turn":7}"#,
            false,
        )
        .unwrap();
        assert_eq!(
            (said.verb.as_str(), said.what.as_str()),
            ("Started", "calc.equal-test · turn 7")
        );
        assert_eq!(text(&said.under[0]), "Add a test.");
        assert!(!said.failed);
    }

    #[test]
    fn a_listing_says_whose_agents_it_lists() {
        let verb = |command: &str| said(command, "[]", false).unwrap().verb;
        assert_eq!(
            verb(r#""$AGENT_BIN" ls --name "${AGENT_BOT%%.*}.*" --pretty"#),
            "Listed this project's agents"
        );
        assert_eq!(
            verb(r#""$AGENT_BIN" ls --name 'calc.*'"#),
            "Listed calc's agents"
        );
        assert_eq!(
            verb(r#""$AGENT_BIN" ls --name '*.lead'"#),
            "Listed every project's lead"
        );
        assert_eq!(verb(r#""$AGENT_BIN" ls --active"#), "Listed running agents");
        assert_eq!(verb(r#""$AGENT_BIN" ls"#), "Listed agents");
    }

    #[test]
    fn a_message_reads_as_what_it_did_not_how_it_was_sent() {
        let verb = |command: &str, output: &str| said(command, output, false).unwrap().verb;
        let run =
            |flags: &str| format!(r#""$AGENT_BIN" run --detach {flags} --bot calc.fix -- 'More'"#);
        let replied =
            |status: &str| format!(r#"{{"bot":"calc.fix","status":"{status}","turn":7}}"#);
        // To an agent at rest, a message starts a turn, however it went.
        for flags in [
            "",
            "--delivery steer",
            "--delivery queue",
            "--delivery reject",
        ] {
            assert_eq!(verb(&run(flags), &replied("running")), "Started a turn for");
        }
        // To one at work: a steer joins its turn, a queued message waits.
        assert_eq!(verb(&run(""), &replied("queued")), "Steered");
        assert_eq!(
            verb(&run("--delivery=steer"), &replied("queued")),
            "Steered"
        );
        assert_eq!(
            verb(&run("--delivery queue"), &replied("queued")),
            "Queued for"
        );
        // A folder or a turn cap queues unless it says otherwise.
        let moved = r#""$AGENT_BIN" run --detach --workspace /x --bot calc.fix -- 'More'"#;
        assert_eq!(verb(moved, &replied("queued")), "Queued for");
        // Waiting for room to run.
        assert_eq!(verb(&run(""), &replied("ready")), "Queued for");
        assert_eq!(
            verb(&run("--delivery attach"), &replied("attached")),
            "Left a note for"
        );
        // Refused as busy: only a turn that had to start now is.
        let busy = said(
            &run("--delivery reject"),
            r#"{"error":"bot_busy","detail":"turn 4 is running","running_turn":4}"#,
            false,
        )
        .unwrap();
        assert!(busy.failed);
        assert_eq!(attempted(&busy.verb), "Couldn't start a turn for");
        assert_eq!(attempted("Left a note for"), "Couldn't leave a note for");
        // Nothing to read: as it was sent.
        assert_eq!(verb(&run(""), ""), "Steered");
        assert_eq!(verb(&run("--delivery queue"), ""), "Queued for");
    }

    #[test]
    fn a_refused_message_says_why_without_a_detail() {
        let said = said(
            r#""$AGENT_BIN" run --detach --delivery queue --bot calc.fix-tests -- 'More'"#,
            r#"{"error":"bot_not_found"}"#,
            false,
        )
        .unwrap();
        assert_eq!(said.verb, "Queued for");
        assert!(said.failed);
        assert_eq!(text(said.under.last().unwrap()), "bot not found");
    }

    #[test]
    fn a_wait_names_the_bot_not_the_handle() {
        let said = said(
            r#""$AGENT_BIN" wait calc.equal-test/7"#,
            r#"{"pending":[],"results":{"calc.equal-test/7":{"status":"completed","text":"Added it.\nMore"}}}"#,
            false,
        )
        .unwrap();
        assert_eq!(said.what, "calc.equal-test");
        assert_eq!(text(&said.under[0]), "calc.equal-test completed: Added it.");
    }

    #[test]
    fn rex_calls_read_as_windows_and_blocks() {
        let said = said(
            r#""$REX" run -s calc --label dev --shell=none --focus=false --json -- npm run dev"#,
            r#"{"window_id":"window:1","block_ids":["block:0123456789abcdef"]}"#,
            false,
        )
        .unwrap();
        assert_eq!(
            (said.verb.as_str(), said.what.as_str()),
            ("Opened a Rex window", "dev (calc)")
        );
        assert_eq!(text(&said.under[0]), "$ npm run dev");
        let said = said_or_none(
            "rex capture -b block:0123456789abcdef --trim --unwrap",
            "a\nb\nc\nd\n",
        );
        assert_eq!(said.what, "…abcdef");
        assert_eq!(said.under.len(), 4);
    }

    #[test]
    fn sonny_s_answered_tools_say_what_they_did() {
        let call = |name: &str, arguments: serde_json::Value| {
            let said = declared(name, &arguments, false).unwrap();
            format!("{} {}", said.verb, said.what)
        };
        assert_eq!(
            call(
                "start_task",
                serde_json::json!({"name": "fix", "brief": "Fix it.\nThen test."})
            ),
            "Started fix"
        );
        assert_eq!(
            call("remove_task", serde_json::json!({"task": "fix"})),
            "Cleaned up fix"
        );
        assert_eq!(
            call(
                "show_agent",
                serde_json::json!({"agent": "shop.fix", "split": "right"})
            ),
            "Moved shop.fix to a split right of you"
        );
        assert_eq!(
            call("show_agent", serde_json::json!({"agent": "web.lead"})),
            "Opened web.lead"
        );
        assert_eq!(
            call("new_project", serde_json::json!({"folder": "/src/web"})),
            "Added the project /src/web"
        );
        assert_eq!(
            call(
                "follows",
                serde_json::json!({"add": {"name": "prs", "bots": ["a"]}})
            ),
            "Followed prs"
        );
        assert_eq!(
            call("subscriptions", serde_json::json!({"remove": "prs"})),
            "Unfollowed prs"
        );
        // An agent that runs no Sonny tool is said some other way.
        assert!(declared("start_task", &serde_json::json!({}), false).is_none());
    }

    #[test]
    fn only_agent_commands_that_work_in_the_sandbox_read_as_inside() {
        for inside in [
            r#""$AGENT_BIN" run --bot shop.fix --detach --delivery steer -- 'Use the cache; then test'"#,
            "\"$AGENT_BIN\" run --bot shop.lead --detach - <<'EOF'\nDone (all of it).\nEOF",
            r#""$AGENT_BIN" ls --name "${AGENT_BOT%%.*}.*" --pretty | jq -r .name"#,
            r#""$AGENT_BIN" turns --bot shop.fix && "$AGENT_BIN" wait shop.fix/4"#,
            r#"agent --pretty inspect --bot shop.fix"#,
            r#""$AGENT_BIN" fork --bot shop.fix --allow '' -- 'What is left?'"#,
            r#""$AGENT_BIN" models"#,
            r#""$AGENT_BIN" ls --name 'a>b' 2>&1"#,
            r#""$AGENT_BIN" ls 2>/dev/null"#,
            r#""$AGENT_BIN" ls --pretty | jq -r '.name | ascii_upcase' | head -5"#,
            r#""$AGENT_BIN" run --bot shop.fix --detach -- 'Run $(make test) and say'"#,
            "\"$AGENT_BIN\" run --bot shop.lead --detach - <<'EOF'\nRan $(make) and `ls`.\nEOF",
        ] {
            assert!(agent_inside(inside), "{inside}");
        }
        for outside in [
            r#""$AGENT_BIN" run --new --bot shop.x -- go"#,
            r#""$AGENT_BIN" run -- go"#,
            r#""$AGENT_BIN" run --bot shop.y --fork shop.fix -- go"#,
            r#""$AGENT_BIN" --store /tmp/s ls"#,
            r#""$AGENT_BIN" --socket /tmp/other.sock ls"#,
            r#""$AGENT_BIN" run --bot shop.fix --workspace /outside --detach -- test"#,
            r#""$AGENT_BIN" run --bot shop.fix --model openai/x -- go"#,
            r#""$AGENT_BIN" run --bot shop.fix --image ~/secret.png -- look"#,
            r#""$AGENT_BIN" audit trim --before 9"#,
            r#""$AGENT_BIN" fork --bot shop.fix --allow '' --workspace /outside -- go"#,
            r#""$AGENT_BIN" --unknown ls"#,
            r#""$AGENT_BIN" ls --name=x --active=1"#,
            r#""$AGENT_BIN" ls --socket=/tmp/other.sock"#,
            r#"AGENT_SOCKET=/tmp/other.sock "$AGENT_BIN" ls"#,
            r#"AGENT_STORE=/tmp/s "$AGENT_BIN" ls"#,
            r#""$AGENT_BIN" start"#,
            r#""$AGENT_BIN" answer --bot shop.fix --turn 3 --allow"#,
            r#""$AGENT_BIN" models --discover"#,
            r#""$AGENT_BIN" fork --bot shop.fix -- go"#,
            r#""$AGENT_BIN" ls | sh"#,
            r#"git push && "$AGENT_BIN" run --bot shop.lead -- pushed"#,
            r#""$AGENT_BIN" ls > "$HOME/report.json""#,
            r#""$AGENT_BIN" turns --bot shop.fix >> /tmp/out 2>&1"#,
            r#""$AGENT_BIN" ls | head | tee "$HOME/report.json""#,
            r#""$AGENT_BIN" ls | jq . | sh"#,
            r#""$AGENT_BIN" run --bot shop.lead --detach -- "$(touch "$HOME/report.json"; printf done)""#,
            r#""$AGENT_BIN" run --bot shop.lead --detach -- `touch ~/r; echo done`"#,
            r#""$AGENT_BIN" run --bot shop.lead --detach -- "$(<(cat ~/x))""#,
            r#""$AGENT_BIN" ls --name "$(cat ~/.name)""#,
            "\"$AGENT_BIN\" run --bot shop.lead --detach - <<EOF\nRan $(touch ~/r).\nEOF",
            "",
        ] {
            assert!(!agent_inside(outside), "{outside}");
        }
    }

    #[test]
    fn the_daemon_tools_for_agents_read_as_the_verbs_they_stand_for() {
        use serde_json::json;
        let send = |arguments: Value, reply: Value| {
            let said = agents("send_message", &arguments, Some(&reply.to_string()), false).unwrap();
            (
                said.verb,
                said.what,
                said.under.iter().map(text).collect::<Vec<_>>(),
            )
        };
        let message = json!({"to": "shop.fix", "message": "Use the cache\nthen test"});
        assert_eq!(
            send(
                message.clone(),
                json!({"status": "running", "handle": "shop.fix/4"})
            ),
            (
                "Started a turn for".into(),
                "shop.fix · turn 4".into(),
                vec!["Use the cache".into()]
            )
        );
        assert_eq!(
            send(
                message.clone(),
                json!({"status": "queued", "handle": "shop.fix/5"})
            )
            .0,
            "Steered"
        );
        let queued = json!({"to": "shop.fix", "message": "m", "delivery": "queue"});
        assert_eq!(send(queued, json!({"status": "queued"})).0, "Queued for");
        let interrupt = json!({"to": "shop.fix", "message": "m", "interrupt": true});
        assert_eq!(
            send(
                interrupt.clone(),
                json!({"status": "queued", "interrupted_turn": 3})
            )
            .0,
            "Interrupted"
        );
        // It had no turn running: the message started one.
        assert_eq!(
            send(interrupt.clone(), json!({"status": "running"})).0,
            "Started a turn for"
        );
        assert_eq!(
            agents("send_message", &interrupt, None, false)
                .unwrap()
                .verb,
            "Interrupted"
        );

        let listed =
            |arguments: Value| agents("list_agents", &arguments, None, false).unwrap().verb;
        assert_eq!(listed(json!({})), "Listed this project's agents");
        assert_eq!(
            listed(json!({"name": "*.lead"})),
            "Listed every project's lead"
        );
        assert_eq!(listed(json!({"name": "web.*"})), "Listed web's agents");
        assert_eq!(
            listed(json!({"name": "*fix*"})),
            "Listed agents named *fix*"
        );
        assert_eq!(listed(json!({"running": true})), "Listed running agents");
        let rows = json!({"bots": [{"name": "shop.fix", "status": "running"}]}).to_string();
        let said = agents("list_agents", &json!({}), Some(&rows), false).unwrap();
        assert!(
            text(&said.under[0]).ends_with("shop.fix running"),
            "{:?}",
            said.under
        );
        let waiting = json!({"approvals": [{"bot": "shop.fix", "tool": "shell"}]}).to_string();
        let said = agents(
            "list_agents",
            &json!({"approvals": true}),
            Some(&waiting),
            false,
        )
        .unwrap();
        assert_eq!(
            (said.verb.as_str(), text(&said.under[0]).as_str()),
            ("Checked approvals", "shop.fix wants shell")
        );

        let said = agents(
            "pause_agents",
            &json!({"reason": "emails going out"}),
            None,
            false,
        )
        .unwrap();
        assert_eq!(
            (said.verb.as_str(), text(&said.under[0]).as_str()),
            ("Paused every agent", "emails going out")
        );
        assert_eq!(attempted(&said.verb), "Couldn't pause every agent");
        assert!(agents("wait", &json!({}), None, false).is_none());
    }

    #[test]
    fn a_question_reads_with_its_choices() {
        let said = declared(
            "ask",
            &serde_json::json!({"question": "Round each line?", "options": [
                {"label": "Yes", "explanation": "e"}, {"label": "No", "explanation": "e"}]}),
            false,
        )
        .unwrap();
        assert_eq!(
            (said.verb.as_str(), said.what.as_str()),
            ("Asked you", "Round each line?")
        );
        assert_eq!(text(&said.under[0]), "1 Yes · 2 No");
    }

    #[test]
    fn a_wait_returns_only_the_turns_that_completed() {
        let tool = r#"{"results":{"shop.fix/7":{"bot":"shop.fix","status":"completed","text":"ok"},"shop.docs/12":{"pending":true},"shop.bad/3":{"status":"failed"}},"pending":["shop.docs/12"]}"#;
        assert_eq!(returned(tool), [("shop.fix".to_owned(), 7)]);
        let shell = format!("Cloning...\n{tool}\ndone");
        assert_eq!(returned(&shell), [("shop.fix".to_owned(), 7)]);
        assert!(returned("no wait here").is_empty());
    }

    #[test]
    fn memory_and_trigger_scripts_say_what_they_did() {
        let saved = said_or_none(
            r#""$HOME/.agent/memory" save flow --type user -- -"#,
            r#"{"duplicate":true,"saved":"flow"}"#,
        );
        assert_eq!(
            (saved.verb.as_str(), saved.what.as_str()),
            ("Already in memory", "flow")
        );
        assert!(saved.under.is_empty() && saved.output.is_none());
        let read = said_or_none(
            r#""$HOME/.agent/memory" read flow --user"#,
            r#"{"read":"flow","reads":3}"#,
        );
        assert_eq!(
            (read.verb.as_str(), read.what.as_str()),
            ("Read from memory", "flow")
        );
        let refused = said_or_none(
            r#"~/.agent/automation add --bot shop.lead --cron '0 9 * * 1' -- Brief me"#,
            r#"{"error":"automations_unsupported","detail":"needs macOS"}"#,
        );
        assert_eq!(refused.verb, "Scheduled");
        assert_eq!(refused.what, "shop.lead · cron 0 9 * * 1");
        assert!(refused.failed);
        let under: Vec<String> = refused.under.iter().map(text).collect();
        assert_eq!(under, ["Brief me", "needs macOS (automations_unsupported)"]);
        let removed = said_or_none(
            r#""$HOME/.agent/automation" rm watch"#,
            r#"{"removed":"watch"}"#,
        );
        assert_eq!(
            (removed.verb.as_str(), removed.what.as_str()),
            ("Removed the automation", "watch")
        );
    }

    #[test]
    fn a_pipe_shows_what_came_out_of_it() {
        let listed = said_or_none(
            r#""$AGENT_BIN" ls | python3 -c 'import sys; print(len(sys.stdin.read()))'"#,
            "42\n",
        );
        assert_eq!(listed.verb, "Listed agents");
        assert!(listed.what.starts_with("| python3 -c"));
        assert!(listed.under.is_empty());
        assert_eq!(listed.output.as_deref(), Some("42\n"));
    }

    fn said_or_none(command: &str, output: &str) -> Said {
        said(command, output, false).unwrap()
    }

    #[test]
    fn the_plan_reads_as_a_checklist() {
        let steps = serde_json::json!({"steps": ["[x] Read", "[>] Fix", "[ ] Test", "Ship"]});
        let said = declared("plan", &steps, false).unwrap();
        let lines: Vec<String> = said.under.iter().map(text).collect();
        assert_eq!(lines, ["✔ Read", "→ Fix", "□ Test", "□ Ship"]);
        let cleared = declared("plan", &serde_json::json!({"steps": []}), false).unwrap();
        assert_eq!(cleared.verb, "Cleared the plan");
    }
}
