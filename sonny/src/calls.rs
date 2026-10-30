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
        "Fired" => "fire",
        "Followed" => "follow",
        "Forked" => "fork",
        "Indexed" => "index",
        "Interrupted" => "interrupt",
        "Listed" => "list",
        "Looked" => "look",
        "Messaged" => "message",
        "Moved" => "move",
        "Opened" => "open",
        "Pruned" => "prune",
        "Queued" => "queue",
        "Read" => "read",
        "Removed" => "remove",
        "Resolved" => "resolve",
        "Saved" => "save",
        "Scheduled" => "schedule",
        "Started" => "start",
        "Steered" => "steer",
        "Subscribed" => "subscribe",
        "Typed" => "type",
        "Unsubscribed" => "unsubscribe",
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
        "trigger" if program.contains(".agent/trigger") => trigger(args, printed, failed),
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

/// The app's trigger script: a job scheduled, fired or removed.
fn trigger(words: &[String], output: &str, failed: bool) -> Option<Said> {
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
        "rm" => said.verb = "Removed the trigger".into(),
        "fire" => said.verb = "Fired the trigger".into(),
        "ls" => said.verb = "Listed the triggers".into(),
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
            said.verb = match (new, args.value(&["--delivery"])) {
                (true, _) => "Started",
                (false, Some("steer")) => "Steered",
                (false, Some("queue")) => "Queued for",
                _ => "Messaged",
            }
            .into();
            if let Some(reply) = &reply
                && refusal.is_none()
            {
                if let Some(turn) = reply["turn"].as_i64() {
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
        "prune" => said.verb = "Pruned finished agents".into(),
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
    "steered",
    "blocked",
    "paced",
];

/// The bot a `turn:BOT/N` handle names.
pub fn handle_bot(handle: &str) -> String {
    let bot = handle.strip_prefix("turn:").unwrap_or(handle);
    match bot.rsplit_once('/') {
        Some((bot, turn)) if turn.bytes().all(|b| b.is_ascii_digit()) => bot.to_owned(),
        _ => bot.to_owned(),
    }
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
            let Some((bot, turn)) = handle
                .strip_prefix("turn:")
                .and_then(|h| h.rsplit_once('/'))
            else {
                continue;
            };
            if let Ok(turn) = turn.parse() {
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
        Some((_, id)) if id.len() > 8 => format!("…{}", &id[id.len() - 6..]),
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
/// or the subscription it changed.
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
                false => "Removed",
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
        crate::served::RUN => {
            let verb = match field("split") {
                Some(_) => "Opened a split",
                None => "Opened a tab",
            };
            let mut said = said(verb, field("name")?);
            if let Some(command) = field("command") {
                said.under.push(dim(format!("$ {}", first_line(command))));
            }
            Some(said)
        }
        crate::served::PROJECT => Some(said("Added the project", field("folder")?)),
        crate::served::SUBSCRIPTIONS => {
            Some(match (arguments["add"]["name"].as_str(), field("remove")) {
                (Some(added), _) => said("Subscribed to", added),
                (None, Some(removed)) => said("Unsubscribed from", removed),
                (None, None) => said("Listed subscriptions", ""),
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
                _ if arguments["index"] == true => said("Indexed memory", ""),
                _ if arguments["check"] == true => said("Checked memory", ""),
                _ if arguments["said"] == true => said("Read what the person wrote", ""),
                _ if arguments.get("cleanup").is_some() || arguments["changed"] == true => {
                    said("Cleaned up memory", "")
                }
                _ => said("Read memory", ""),
            },
        ),
        crate::served::TRIGGERS => Some(
            match (
                arguments["add"].is_object(),
                field("remove"),
                field("fire"),
                field("show"),
            ) {
                (true, ..) => {
                    // As the trigger script's add says it.
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
                (_, Some(removed), ..) => said("Removed the trigger", removed),
                (_, _, Some(fired), _) => said("Fired the trigger", fired),
                (_, _, _, Some(shown)) => said("Read the trigger", shown),
                _ => said("Listed triggers", ""),
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
            attempted("Removed the trigger"),
            "Couldn't remove the trigger"
        );
        assert_eq!(attempted("Queued for"), "Couldn't queue for");
        assert_eq!(attempted("Saved to memory"), "Couldn't save to memory");
    }

    fn text(line: &Line) -> String {
        line.spans.iter().map(|s| s.content.as_ref()).collect()
    }

    #[test]
    fn a_brief_in_a_document_or_a_capture_still_reads_as_a_start() {
        let line = "h=$(\"$AGENT_BIN\" run --detach --new --bot calc.neg -- \"$(cat <<'EOF'\nAdd a negative test.\nKeep the rest.\nEOF\n)\" | jq -r .handle)";
        let said = said(line, "turn:calc.neg/3", false).unwrap();
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
            ("Messaged", "calc.lead")
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
            r#"{"bot":"calc.equal-test","handle":"turn:calc.equal-test/7","status":"running","turn":7}"#,
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
            r#""$AGENT_BIN" wait turn:calc.equal-test/7"#,
            r#"{"pending":[],"results":{"turn:calc.equal-test/7":{"status":"completed","text":"Added it.\nMore"}}}"#,
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
        let said = declared(
            "run_in_pane",
            &serde_json::json!({"name": "tests", "command": "cargo test --workspace"}),
            false,
        )
        .unwrap();
        assert_eq!(
            (said.verb.as_str(), said.what.as_str()),
            ("Opened a tab", "tests")
        );
        assert_eq!(text(&said.under[0]), "$ cargo test --workspace");
        let said = declared(
            "run_in_pane",
            &serde_json::json!({"name": "server", "split": "right", "command": "npm run dev"}),
            false,
        )
        .unwrap();
        assert_eq!(
            (said.verb.as_str(), said.what.as_str()),
            ("Opened a split", "server")
        );
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
            "Removed fix"
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
                "subscriptions",
                serde_json::json!({"add": {"name": "prs", "bots": ["a"]}})
            ),
            "Subscribed to prs"
        );
        // An agent that runs no Sonny tool is said some other way.
        assert!(declared("start_task", &serde_json::json!({}), false).is_none());
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
        let tool = r#"{"results":{"turn:shop.fix/7":{"bot":"shop.fix","status":"completed","text":"ok"},"turn:shop.docs/12":{"pending":true},"turn:shop.bad/3":{"status":"failed"}},"pending":["turn:shop.docs/12"]}"#;
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
            r#"~/.agent/trigger add --bot shop.lead --cron '0 9 * * 1' -- Brief me"#,
            r#"{"error":"triggers_unsupported","detail":"needs macOS"}"#,
        );
        assert_eq!(refused.verb, "Scheduled");
        assert_eq!(refused.what, "shop.lead · cron 0 9 * * 1");
        assert!(refused.failed);
        let under: Vec<String> = refused.under.iter().map(text).collect();
        assert_eq!(under, ["Brief me", "needs macOS (triggers_unsupported)"]);
        let removed = said_or_none(
            r#""$HOME/.agent/trigger" rm watch"#,
            r#"{"removed":"watch"}"#,
        );
        assert_eq!(
            (removed.verb.as_str(), removed.what.as_str()),
            ("Removed the trigger", "watch")
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
