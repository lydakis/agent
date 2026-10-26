//! Shell commands Sonny knows the meaning of, said as what they do: an
//! `agent` call starts, messages, waits on or removes an agent; a `rex` call
//! opens, reads or types into a window; an `errand` call runs a command on
//! another machine or acts on its job; the plan skill sets the plan. A
//! command line is read a command at a time; a pipe is said with its
//! command and what came out of it.
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
        "errand" => errand(args, printed, failed),
        "$REX" | "${REX}" | "rex" => rex(args, printed, failed),
        "memory" if program.contains(".agent/memory") => Some(Said {
            verb: match args.first().map(String::as_str) {
                Some("show") | None => "Read the memory indexes".into(),
                Some(other) => format!("Used memory {other}"),
            },
            what: String::new(),
            under: Vec::new(),
            failed,
            output: None,
        }),
        "project" if program.contains(".agent/project") => Some(Said {
            verb: match args.first().map(String::as_str) {
                Some("add") => "Added the project".into(),
                _ => "Read the help for the project script".into(),
            },
            what: args.get(1).cloned().unwrap_or_default(),
            under: Vec::new(),
            failed,
            output: None,
        }),
        "plan" if program.contains("skills/plan") => plan(args),
        "sonny" if args.first().is_some_and(|a| a == "show") => sonny_show(&args[1..], failed),
        "sh" | "bash"
            if args
                .first()
                .is_some_and(|a| a.ends_with("skills/plan/plan")) =>
        {
            plan(&args[1..])
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
    "--fallbacks",
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

fn dim(text: impl Into<String>) -> Line<'static> {
    Line::from(text.into()).dim()
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
    let refusal = reply.as_ref().and_then(|r| {
        let code = r["error"].as_str()?;
        let detail = r["detail"].as_str().filter(|d| !d.is_empty());
        Some(match detail {
            Some(detail) => format!("{detail} ({code})"),
            None => code.replace('_', " "),
        })
    });
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
                said.what = args
                    .plain(0)
                    .iter()
                    .map(|h| handle_bot(h))
                    .collect::<Vec<_>>()
                    .join(", ");
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
        said.under.push(Line::from(refusal).fg(Color::Red));
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
            (Some(status), _) => {
                let text = result["text"].as_str().unwrap_or("");
                format!("{who} {status}: {}", first_line(text))
            }
            _ => continue,
        };
        lines.push(line);
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

/// The command an `errand -d -- COMMAND` call starts, detached.
pub fn errand_detached(command: &str) -> Option<String> {
    let simple = crate::shell::simple(command)?;
    let (program, words) = simple.words.split_first()?;
    if program.rsplit('/').next() != Some("errand") {
        return None;
    }
    let args = Args::new(words);
    (args.has("-d") || args.has("--detach"))
        .then(|| args.rest.clone())
        .flatten()
}

/// An Errand call: a command run on another machine, or a look at or
/// action on a job, by its `peer/ULID` handle.
fn errand(words: &[String], output: &str, failed: bool) -> Option<Said> {
    let args = Args::new(words);
    let mut said = Said {
        verb: String::new(),
        what: String::new(),
        under: Vec::new(),
        failed,
        output: None,
    };
    if let Some(command) = &args.rest {
        // Where it runs: the runner named, what it needs, or the default.
        let place = match (args.value(&["--on"]), args.value(&["--where"])) {
            (Some("local"), _) => "on a local copy".to_owned(),
            (Some(peer), _) => format!("on {peer}"),
            (None, Some(facts)) => format!("where {facts}"),
            (None, None) => "on Errand".to_owned(),
        };
        said.what = format!("{} {place}", first_line(command));
        if args.has("-d") || args.has("--detach") {
            said.verb = "Started".into();
            if let Some(handle) = output.lines().map(str::trim).find(|l| l.contains('/')) {
                said.under.push(dim(handle.to_owned()));
            }
        } else {
            said.verb = "Ran".into();
            said.output = Some(output.to_owned());
        }
        return Some(said);
    }
    let (verb, rest) = args.words.split_first()?;
    let handle = rest
        .iter()
        .find(|w| !w.starts_with('-') && w.contains('/'))
        .cloned()
        .unwrap_or_default();
    said.what = handle;
    said.verb = match verb.as_str() {
        "ps" => "Listed Errand jobs",
        "peers" => "Listed Errand runners",
        "status" => "Checked Errand job",
        "attach" => "Read the output of Errand job",
        "fetch" if args.has("--apply") => "Applied the changes of Errand job",
        "fetch" => "Fetched the changes of Errand job",
        "kill" => "Stopped Errand job",
        _ => return None,
    }
    .into();
    if matches!(verb.as_str(), "attach" | "ps" | "peers") {
        said.output = Some(output.to_owned());
    }
    Some(said)
}

/// `sonny show`, as where the agent's window went.
fn sonny_show(words: &[String], failed: bool) -> Option<Said> {
    let (bot, rest) = words.split_first()?;
    let args = Args::new(rest);
    let (verb, what) = match args.value(&["--split"]) {
        Some(side) => {
            let beside = args.value(&["--beside"]).unwrap_or("you");
            let side = match side {
                "right" => "right of",
                "left" => "left of",
                other => other,
            };
            ("Moved", format!("{bot} to a split {side} {beside}"))
        }
        None => ("Moved", format!("{bot} to a tab of its own")),
    };
    Some(Said {
        verb: verb.into(),
        what,
        under: Vec::new(),
        failed,
        output: None,
    })
}

/// The plan skill's call, as the plan it sets.
fn plan(items: &[String]) -> Option<Said> {
    let mut said = Said {
        verb: "Updated the plan".into(),
        what: String::new(),
        under: Vec::new(),
        failed: false,
        output: None,
    };
    if items.iter().any(|i| i == "--clear") {
        said.verb = "Cleared the plan".into();
        return Some(said);
    }
    for item in items {
        let (mark, text) = match item.split_at_checked(3) {
            Some(("[x]", text)) => ("✔", text),
            Some(("[>]", text)) => ("→", text),
            Some(("[ ]", text)) => ("□", text),
            _ => return None,
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
    Some(said)
}

#[cfg(test)]
mod tests {
    use super::*;

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
    }

    #[test]
    fn errand_calls_say_where_they_ran() {
        let ran = said_or_none("errand --on mac-mini -- cargo test --workspace", "ok\n");
        assert_eq!(
            (ran.verb.as_str(), ran.what.as_str()),
            ("Ran", "cargo test --workspace on mac-mini")
        );
        assert_eq!(ran.output.as_deref(), Some("ok\n"));
        let started = said_or_none(
            "errand -d --where 'os=linux,gpu' -- python train.py",
            "gpu-box/01JB2Q8W3V4K\n",
        );
        assert_eq!(started.verb, "Started");
        assert_eq!(started.what, "python train.py where os=linux,gpu");
        assert_eq!(started.under.len(), 1);
        let fetched = said_or_none("errand fetch --apply mini/01JB2Q8W3V4K", "");
        assert_eq!(
            (fetched.verb.as_str(), fetched.what.as_str()),
            ("Applied the changes of Errand job", "mini/01JB2Q8W3V4K")
        );
        assert!(said("errand doctor", "", false).is_none());
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

    #[test]
    fn a_detached_errand_run_is_found() {
        assert_eq!(
            errand_detached("errand -d --on mini -- make test").as_deref(),
            Some("make test")
        );
        assert_eq!(errand_detached("errand --on mini -- make test"), None);
        assert_eq!(errand_detached("errand -d ps"), None);
    }

    fn said_or_none(command: &str, output: &str) -> Said {
        said(command, output, false).unwrap()
    }

    #[test]
    fn the_plan_reads_as_a_checklist() {
        let said = said_or_none(
            r#"sh "$HOME/.agents/skills/plan/plan" '[x] Read' '[>] Fix' '[ ] Test'"#,
            "plan saved: 3 steps",
        );
        let lines: Vec<String> = said.under.iter().map(text).collect();
        assert_eq!(lines, ["✔ Read", "→ Fix", "□ Test"]);
    }
}
