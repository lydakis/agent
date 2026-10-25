//! Shell commands Sonny knows the meaning of, said as what they do: an
//! `agent` call starts, messages, waits on or removes an agent; a `rex` call
//! opens, reads or types into a window; the plan skill sets the plan. A
//! command line is read a command at a time; a command with a pipe, a
//! redirect or a substitution is shown as the command it is.
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
}

pub fn said(command: &str, output: &str, failed: bool) -> Option<Said> {
    let words = words(command)?;
    // Leading `VAR=value` and `env` only set up the call.
    let mut words = words.as_slice();
    while let Some(first) = words.first() {
        if first == "env" || (first.contains('=') && !first.starts_with('-')) {
            words = &words[1..];
        } else {
            break;
        }
    }
    let (program, args) = words.split_first()?;
    let name = program.rsplit('/').next().unwrap_or(program);
    match name {
        "$AGENT_BIN" | "${AGENT_BIN}" | "agent" => agent(args, output, failed),
        "$REX" | "${REX}" | "rex" => rex(args, output, failed),
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
    }
}

/// A command line's commands, split where `;`, `&&`, `||` or a newline
/// end one outside quotes. Pipes stay inside a command.
pub fn parts(command: &str) -> Vec<String> {
    let mut parts = Vec::new();
    let mut part = String::new();
    let mut quote: Option<char> = None;
    let mut chars = command.chars().peekable();
    while let Some(c) = chars.next() {
        match (quote, c) {
            (Some(q), c) if c == q => quote = None,
            (Some('"'), '\\') => {
                part.push(c);
                if let Some(next) = chars.next() {
                    part.push(next);
                }
                continue;
            }
            (Some(_), _) => {}
            (None, '\'' | '"') => quote = Some(c),
            (None, '\\') => {
                part.push(c);
                if let Some(next) = chars.next() {
                    part.push(next);
                }
                continue;
            }
            (None, ';' | '\n') => {
                parts.push(std::mem::take(&mut part));
                continue;
            }
            (None, '&' | '|') if chars.peek() == Some(&c) => {
                chars.next();
                parts.push(std::mem::take(&mut part));
                continue;
            }
            _ => {}
        }
        part.push(c);
    }
    parts.push(part);
    parts
        .into_iter()
        .map(|p| p.trim().to_owned())
        .filter(|p| !p.is_empty())
        .collect()
}

/// A command's words as the shell would split them, without expanding
/// anything; none when it is more than one simple command.
fn words(command: &str) -> Option<Vec<String>> {
    let mut words = Vec::new();
    let mut word: Option<String> = None;
    let mut chars = command.trim().chars().peekable();
    while let Some(c) = chars.next() {
        match c {
            '\'' => {
                let w = word.get_or_insert_with(String::new);
                loop {
                    match chars.next()? {
                        '\'' => break,
                        c => w.push(c),
                    }
                }
            }
            '"' => {
                let w = word.get_or_insert_with(String::new);
                loop {
                    match chars.next()? {
                        '"' => break,
                        '\\' => match chars.next()? {
                            c @ ('"' | '\\' | '$' | '`') => w.push(c),
                            '\n' => {}
                            c => {
                                w.push('\\');
                                w.push(c);
                            }
                        },
                        '`' => return None,
                        '$' if chars.peek() == Some(&'(') => return None,
                        c => w.push(c),
                    }
                }
            }
            '\\' => match chars.next()? {
                '\n' => {}
                c => word.get_or_insert_with(String::new).push(c),
            },
            ' ' | '\t' => words.extend(word.take()),
            ';' | '&' | '|' | '<' | '>' | '\n' | '`' | '(' | ')' => return None,
            '$' if chars.peek() == Some(&'(') => return None,
            c => word.get_or_insert_with(String::new).push(c),
        }
    }
    words.extend(word);
    (!words.is_empty()).then_some(words)
}

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
    "--new", "--detach", "--json", "--pretty", "--any", "--agents", "--trim", "--unwrap",
    "--clear", "--all", "--wait",
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

fn agent(words: &[String], output: &str, failed: bool) -> Option<Said> {
    let (verb, rest) = words.split_first()?;
    let args = Args::new(rest);
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
    };
    match verb.as_str() {
        "run" => {
            let new = args.has("--new");
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
            if let Some(prompt) = args.rest.as_deref().filter(|p| !p.is_empty()) {
                said.under
                    .insert(0, Line::from(first_line(prompt)).italic().dim());
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
            said.verb = "Listed agents".into();
            if let Some(rows) = reply
                .as_ref()
                .and_then(|r| r.as_array().or_else(|| r["bots"].as_array()).cloned())
            {
                let names: Vec<String> = rows
                    .iter()
                    .filter_map(|row| {
                        let name = row["name"].as_str().or_else(|| row["bot"].as_str())?;
                        Some(match row["status"].as_str() {
                            Some(status) => format!("{name} ({status})"),
                            None => name.to_owned(),
                        })
                    })
                    .collect();
                if !names.is_empty() {
                    said.under.push(dim(names.join(", ")));
                }
            }
        }
        "rm" => said.verb = "Removed".into(),
        "interrupt" => said.verb = "Interrupted".into(),
        "inspect" | "resume" => said.verb = "Looked up".into(),
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
        ("block" | "window" | "session", Some((sub, rest))) => (format!("{verb} {sub}"), rest),
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

/// `sonny show`, as where the agent's window went.
fn sonny_show(words: &[String], failed: bool) -> Option<Said> {
    let (bot, rest) = words.split_first()?;
    let args = Args::new(rest);
    let (verb, what) = match args.value(&["--split"]) {
        Some(side) => {
            let beside = args.value(&["--beside"]).unwrap_or("you");
            ("Moved", format!("{bot} to a split {side} of {beside}"))
        }
        None => ("Moved", format!("{bot} to a tab of its own")),
    };
    Some(Said {
        verb: verb.into(),
        what,
        under: Vec::new(),
        failed,
    })
}

/// The plan skill's call, as the plan it sets.
fn plan(items: &[String]) -> Option<Said> {
    let mut said = Said {
        verb: "Updated the plan".into(),
        what: String::new(),
        under: Vec::new(),
        failed: false,
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
    fn words_split_like_the_shell_and_refuse_more_than_one_command() {
        assert_eq!(
            words(r#""$AGENT_BIN" run --bot 'a b' -- "say \"hi\"""#).unwrap(),
            ["$AGENT_BIN", "run", "--bot", "a b", "--", "say \"hi\""]
        );
        assert!(words("pwd; agent ls").is_none());
        assert!(words("agent ls | head").is_none());
        assert!(words("agent run -- \"$(cat brief)\"").is_none());
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

    fn said_or_none(command: &str, output: &str) -> Said {
        said(command, output, false).unwrap()
    }

    #[test]
    fn a_command_line_splits_into_its_commands_outside_quotes() {
        assert_eq!(
            parts(
                "ls \"$HOME/.agent\"; \"$AGENT_BIN\" run --bot x -- 'a; b && c' && env | grep rex\npwd"
            ),
            [
                "ls \"$HOME/.agent\"",
                "\"$AGENT_BIN\" run --bot x -- 'a; b && c'",
                "env | grep rex",
                "pwd"
            ]
        );
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
