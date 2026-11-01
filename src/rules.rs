//! Command rules, as Codex's execpolicy writes them: `prefix_rule` calls in
//! Starlark files, each naming a command prefix and what happens when a
//! shell call starts with it. The calls, the matching and the decisions are
//! ported from Codex's `execpolicy` crate (Apache-2.0; see NOTICE), so a
//! rule written for Codex works here unchanged. Only the calls are read,
//! with Starlark's literals (strings, lists, `None`, `True`, `False`) and
//! comments, as Codex documents its rules and writes "don't ask again":
//! anything else Starlark has, a variable or a loop, is `rules_invalid`
//! rather than a language runtime in the daemon.
//!
//! A rule's `pattern` is words in order, any of them a list of
//! alternatives; `decision` is `allow`, `prompt` or `forbidden`, `allow`
//! when left out; `match` and `not_match` are examples checked as the file
//! loads. A command matches a rule when its words start with the
//! pattern's, and the strictest rule that matches decides. A script is
//! read as the sandbox reads one ([`crate::sandbox::commands`]): each plain
//! command on its own, and anything else matches no rule.
//!
//! The daemon reads every `*.rules` file of the folder its client names, in
//! name order, as each turn starts: a turn keeps the rules it started
//! with, as it keeps its bot's settings. A folder whose files changed is
//! read again; one that fails to read keeps the rules last read, or, with
//! none read yet, refuses every command with why.
use crate::{Error, Result};
use serde::Serialize;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

/// What a rule says of a command it matches, least strict first.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Decision {
    /// Runs without asking, outside a sandbox the bot has.
    Allow,
    /// Waits for the bot's approver, sandbox or not.
    Prompt,
    /// Refused.
    Forbidden,
}

impl Decision {
    fn parse(text: &str) -> std::result::Result<Self, String> {
        match text {
            "allow" => Ok(Self::Allow),
            "prompt" => Ok(Self::Prompt),
            "forbidden" => Ok(Self::Forbidden),
            other => Err(format!(
                "decision must be allow, prompt or forbidden, not {other:?}"
            )),
        }
    }
}

/// One `prefix_rule`, with the place it was written.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Rule {
    /// Each word's alternatives.
    pattern: Vec<Vec<String>>,
    decision: Decision,
    justification: Option<String>,
    /// `FILE:LINE`.
    at: String,
}

impl Rule {
    /// The words of `command` this rule's pattern matches, when it does.
    fn matches<'a>(&self, command: &'a [String]) -> Option<&'a [String]> {
        let prefix = command.get(..self.pattern.len())?;
        (prefix.iter().zip(&self.pattern))
            .all(|(word, alternatives)| alternatives.contains(word))
            .then_some(prefix)
    }
}

/// A rule a command matched, as `agent rules check` reports it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Matched {
    /// The command's words the rule matched.
    pub prefix: Vec<String>,
    pub decision: Decision,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub justification: Option<String>,
    /// Where the rule is: `FILE:LINE`.
    pub rule: String,
}

/// What the rules say of a script: the strictest decision, when one rules
/// it, and each rule that matched.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
pub struct Verdict {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub decision: Option<Decision>,
    pub matched: Vec<Matched>,
}

/// Rules read from files.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Policy {
    rules: Vec<Rule>,
}

impl Policy {
    /// Every `*.rules` file in `folder`, in name order; none when it is
    /// missing. One that doesn't read, or whose examples fail, is
    /// `rules_invalid`, with where.
    pub fn load(folder: &Path) -> Result<Self> {
        let mut policy = Self::default();
        for path in files(folder)? {
            let text = std::fs::read_to_string(&path)
                .map_err(|error| invalid(format!("{}: {error}", path.display())))?;
            policy.parse(&path.display().to_string(), &text)?;
        }
        Ok(policy)
    }

    /// The rules in `text`, named `name`, added after these; none are
    /// added when any of them is wrong.
    pub fn parse(&mut self, name: &str, text: &str) -> Result<()> {
        let rules = Reader::new(name, text).rules()?;
        self.rules.extend(rules);
        Ok(())
    }

    /// What these rules say of `script`, a shell call's command. A script
    /// that isn't plain matches nothing. One of several commands is
    /// forbidden, or asks, when any of them is; it is allowed only when
    /// every one is, since one no rule names runs as it would with none.
    pub fn check(&self, script: &str) -> Verdict {
        match crate::sandbox::commands(script).filter(|c| !c.is_empty()) {
            Some(commands) => self.verdict(&commands),
            None => Verdict::default(),
        }
    }

    /// What these rules say of one command, by its words.
    pub fn check_command(&self, words: &[String]) -> Verdict {
        self.verdict(&[words.to_vec()])
    }

    fn verdict(&self, commands: &[Vec<String>]) -> Verdict {
        let mut verdict = Verdict::default();
        let mut all_allowed = true;
        let mut strictest = None;
        for command in commands {
            let matched: Vec<Matched> = (self.rules.iter())
                .filter_map(|rule| {
                    rule.matches(command).map(|prefix| Matched {
                        prefix: prefix.to_vec(),
                        decision: rule.decision,
                        justification: rule.justification.clone(),
                        rule: rule.at.clone(),
                    })
                })
                .collect();
            let decision = matched.iter().map(|m| m.decision).max();
            all_allowed &= decision == Some(Decision::Allow);
            strictest = strictest.max(decision);
            verdict.matched.extend(matched);
        }
        verdict.decision = match strictest {
            Some(Decision::Allow) if !all_allowed => None,
            other => other,
        };
        verdict
    }

    /// How many rules there are.
    pub fn len(&self) -> usize {
        self.rules.len()
    }

    pub fn is_empty(&self) -> bool {
        self.rules.is_empty()
    }
}

/// What the rules make of one shell call.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Ruling {
    /// No rule names it: it runs as it would with none.
    None,
    Allow,
    /// Waits for the bot's approver; why, for the approver and a refusal.
    Prompt(String),
    /// Refused, with why.
    Forbidden(String),
}

/// The rules a turn runs with: those last read, or why none could be.
#[derive(Debug, Clone, Default)]
pub struct Rules {
    policy: Policy,
    /// The folder's files failed to read before any did: every command is
    /// refused with this until they read.
    broken: Option<Error>,
}

impl Rules {
    pub fn new(policy: Policy) -> Self {
        Self {
            policy,
            broken: None,
        }
    }

    /// What these rules make of a shell call running `script`.
    pub fn rule(&self, script: &str) -> std::result::Result<Ruling, Error> {
        if let Some(error) = &self.broken {
            return Err(error.clone());
        }
        let verdict = self.policy.check(script);
        let why = |decision: Decision| {
            let rule = (verdict.matched.iter())
                .find(|m| m.decision == decision)
                .expect("the decision is a match's");
            rule.justification.clone().unwrap_or_else(|| {
                format!(
                    "the rule at {} names `{}`",
                    rule.rule,
                    rule.prefix.join(" ")
                )
            })
        };
        Ok(match verdict.decision {
            None => Ruling::None,
            Some(Decision::Allow) => Ruling::Allow,
            Some(decision @ Decision::Prompt) => Ruling::Prompt(why(decision)),
            Some(decision @ Decision::Forbidden) => Ruling::Forbidden(why(decision)),
        })
    }
}

/// A folder of rules, read again when its files change.
#[derive(Debug, Default)]
pub struct Folder {
    path: Option<PathBuf>,
    state: Mutex<State>,
}

#[derive(Debug, Default)]
struct State {
    /// The files as last read; None before the first read.
    seen: Option<Vec<Stamp>>,
    current: Arc<Rules>,
    /// Whether `current` was read from the files, not refused.
    read: bool,
}

/// A rules file as the folder lists it: what changes when it is written.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Stamp {
    path: PathBuf,
    len: u64,
    modified: Option<std::time::SystemTime>,
    #[cfg(unix)]
    changed: (i64, i64),
}

impl Folder {
    /// The rules in `path`; with none, no rules at all.
    pub fn new(path: Option<PathBuf>) -> Self {
        Self {
            path,
            state: Mutex::default(),
        }
    }

    pub fn path(&self) -> Option<&Path> {
        self.path.as_deref()
    }

    /// The rules as the folder holds them now: read again when its files
    /// changed since, and kept as they were when the new ones don't read.
    pub fn current(&self) -> Arc<Rules> {
        let Some(path) = &self.path else {
            return Arc::default();
        };
        let stamps = stamps(path);
        let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        if state.seen.as_ref() == Some(&stamps) {
            return state.current.clone();
        }
        match Policy::load(path) {
            Ok(policy) => {
                state.current = Arc::new(Rules::new(policy));
                state.read = true;
            }
            // The rules last read stay, so a file saved half-written
            // loses no rule.
            Err(_) if state.read => {}
            Err(error) => {
                state.current = Arc::new(Rules {
                    policy: Policy::default(),
                    broken: Some(error),
                });
            }
        }
        state.seen = Some(stamps);
        state.current.clone()
    }
}

/// The `*.rules` files in `folder`, in name order: none when it is missing.
fn files(folder: &Path) -> Result<Vec<PathBuf>> {
    let entries = match std::fs::read_dir(folder) {
        Ok(entries) => entries,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(error) => return Err(invalid(format!("{}: {error}", folder.display()))),
    };
    let mut files = Vec::new();
    for entry in entries {
        let entry = entry.map_err(|error| invalid(format!("{}: {error}", folder.display())))?;
        let path = entry.path();
        if path.extension().is_some_and(|ext| ext == "rules")
            && std::fs::metadata(&path).is_ok_and(|found| found.is_file())
        {
            files.push(path);
        }
    }
    files.sort();
    Ok(files)
}

/// What says whether `folder`'s rules changed: each file's size and times.
/// A folder that can't be listed lists as it is, so its error is read.
fn stamps(folder: &Path) -> Vec<Stamp> {
    let Ok(files) = files(folder) else {
        return vec![Stamp {
            path: folder.to_owned(),
            len: u64::MAX,
            modified: None,
            #[cfg(unix)]
            changed: (0, 0),
        }];
    };
    files
        .into_iter()
        .map(|path| {
            let found = std::fs::metadata(&path).ok();
            Stamp {
                len: found.as_ref().map_or(0, |found| found.len()),
                modified: found.as_ref().and_then(|found| found.modified().ok()),
                #[cfg(unix)]
                changed: found.as_ref().map_or((0, 0), |found| {
                    use std::os::unix::fs::MetadataExt;
                    (found.ctime(), found.ctime_nsec())
                }),
                path,
            }
        })
        .collect()
}

fn invalid(detail: String) -> Error {
    Error::with("rules_invalid", detail)
}

/// A Starlark literal, as a rule's arguments are written.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Literal {
    Str(String),
    List(Vec<Literal>),
    Bool(bool),
    None,
}

impl Literal {
    fn kind(&self) -> &'static str {
        match self {
            Self::Str(_) => "a string",
            Self::List(_) => "a list",
            Self::Bool(_) => "a bool",
            Self::None => "None",
        }
    }
}

/// Reads one rules file: a sequence of calls, each on its own line or
/// after a `;`, with literal arguments.
struct Reader<'a> {
    name: &'a str,
    text: Vec<char>,
    at: usize,
    line: usize,
}

/// The words Starlark keeps for statements, named when one is met.
const STATEMENTS: &[&str] = &[
    "def", "for", "if", "elif", "else", "load", "return", "while", "lambda", "pass", "break",
    "continue",
];

impl<'a> Reader<'a> {
    fn new(name: &'a str, text: &str) -> Self {
        Self {
            name,
            text: text.chars().collect(),
            at: 0,
            line: 1,
        }
    }

    fn wrong(&self, line: usize, detail: impl std::fmt::Display) -> Error {
        invalid(format!("{}:{line}: {detail}", self.name))
    }

    fn here(&self, detail: impl std::fmt::Display) -> Error {
        self.wrong(self.line, detail)
    }

    fn peek(&self) -> Option<char> {
        self.text.get(self.at).copied()
    }

    fn next(&mut self) -> Option<char> {
        let found = self.peek()?;
        self.at += 1;
        if found == '\n' {
            self.line += 1;
        }
        Some(found)
    }

    /// Spaces and comments; new lines too inside brackets, where Starlark
    /// ignores them, and a backslash before one anywhere.
    fn skip(&mut self, lines: bool) {
        while let Some(found) = self.peek() {
            match found {
                ' ' | '\t' | '\r' | '\x0c' => drop(self.next()),
                '\n' if lines => drop(self.next()),
                '\\' if self.text.get(self.at + 1) == Some(&'\n') => {
                    self.next();
                    self.next();
                }
                '#' => {
                    while self.peek().is_some_and(|c| c != '\n') {
                        self.next();
                    }
                }
                _ => break,
            }
        }
    }

    fn word(&mut self) -> Option<String> {
        let start = self.at;
        while self.peek().is_some_and(|c| c == '_' || c.is_alphanumeric()) {
            self.next();
        }
        (self.at > start && !self.text[start].is_ascii_digit())
            .then(|| self.text[start..self.at].iter().collect())
            .or_else(|| {
                self.at = start;
                None
            })
    }

    fn rules(mut self) -> Result<Vec<Rule>> {
        let mut rules = Vec::new();
        loop {
            self.skip(true);
            while self.peek() == Some(';') {
                self.next();
                self.skip(true);
            }
            let Some(found) = self.peek() else {
                return Ok(rules);
            };
            let line = self.line;
            let Some(function) = self.word() else {
                return Err(self.here(format!(
                    "a rules file holds prefix_rule calls, not {found:?}"
                )));
            };
            if STATEMENTS.contains(&function.as_str()) {
                return Err(self.here(format!(
                    "a rules file holds prefix_rule calls with literal arguments; \
                     Starlark's `{function}` isn't read here"
                )));
            }
            self.skip(false);
            if self.next() != Some('(') {
                return Err(self.wrong(
                    line,
                    format!(
                        "a rules file holds prefix_rule calls with literal arguments; \
                         `{function}` is not called"
                    ),
                ));
            }
            let arguments = self.arguments()?;
            self.skip(false);
            match self.peek() {
                None | Some('\n' | ';') => {}
                Some(found) => {
                    return Err(self.here(format!("expected a new line, not {found:?}")));
                }
            }
            match function.as_str() {
                "prefix_rule" => rules.push(self.prefix_rule(line, arguments)?),
                "host_executable" => self.host_executable(line, arguments)?,
                "network_rule" => {
                    return Err(self.wrong(
                        line,
                        "network_rule isn't read here: a sandbox's network is on or off",
                    ));
                }
                other => {
                    return Err(self.wrong(
                        line,
                        format!(
                            "`{other}` isn't a rules function; prefix_rule and \
                             host_executable are"
                        ),
                    ));
                }
            }
        }
    }

    /// A call's arguments, after its `(`, through its `)`.
    fn arguments(&mut self) -> Result<Vec<(Option<String>, Literal)>> {
        let mut arguments = Vec::new();
        loop {
            self.skip(true);
            if self.peek() == Some(')') {
                self.next();
                return Ok(arguments);
            }
            let start = (self.at, self.line);
            let name = match self.word() {
                Some(word) if !matches!(word.as_str(), "None" | "True" | "False") => {
                    self.skip(true);
                    if self.next() != Some('=') || self.peek() == Some('=') {
                        return Err(self.wrong(
                            start.1,
                            format!("`{word}` is not a literal; arguments are written as literals"),
                        ));
                    }
                    Some(word)
                }
                _ => {
                    (self.at, self.line) = start;
                    None
                }
            };
            let value = self.literal()?;
            arguments.push((name, value));
            self.skip(true);
            match self.next() {
                Some(',') => {}
                Some(')') => return Ok(arguments),
                Some(found) => return Err(self.here(format!("expected , or ), not {found:?}"))),
                None => return Err(self.here("the call is not closed")),
            }
        }
    }

    fn literal(&mut self) -> Result<Literal> {
        self.skip(true);
        match self.peek() {
            Some('"' | '\'') => self.string(false).map(Literal::Str),
            Some('r' | 'R') if matches!(self.text.get(self.at + 1), Some('"' | '\'')) => {
                self.next();
                self.string(true).map(Literal::Str)
            }
            Some('[') => {
                self.next();
                let mut items = Vec::new();
                loop {
                    self.skip(true);
                    if self.peek() == Some(']') {
                        self.next();
                        return Ok(Literal::List(items));
                    }
                    items.push(self.literal()?);
                    self.skip(true);
                    match self.next() {
                        Some(',') => {}
                        Some(']') => return Ok(Literal::List(items)),
                        Some(found) => {
                            return Err(self.here(format!("expected , or ], not {found:?}")));
                        }
                        None => return Err(self.here("the list is not closed")),
                    }
                }
            }
            None => Err(self.here("the call is not closed")),
            Some(found) => match self.word().as_deref() {
                Some("None") => Ok(Literal::None),
                Some("True") => Ok(Literal::Bool(true)),
                Some("False") => Ok(Literal::Bool(false)),
                Some(word) => Err(self.here(format!(
                    "`{word}` is not a literal; arguments are written as literals"
                ))),
                None => Err(self.here(format!("expected a string or a list, not {found:?}"))),
            },
        }
    }

    /// A quoted string, at its quote, with Starlark's escapes unless raw.
    fn string(&mut self, raw: bool) -> Result<String> {
        let line = self.line;
        let quote = self.next().expect("a quote");
        let triple = self.text.get(self.at..self.at + 2) == Some(&[quote, quote][..]);
        if triple {
            self.next();
            self.next();
        }
        let mut text = String::new();
        loop {
            let Some(found) = self.next() else {
                return Err(self.wrong(line, "the string is not closed"));
            };
            match found {
                c if c == quote && !triple => return Ok(text),
                c if c == quote
                    && self.text.get(self.at..self.at + 2) == Some(&[quote, quote][..]) =>
                {
                    self.next();
                    self.next();
                    return Ok(text);
                }
                '\n' if !triple => return Err(self.wrong(line, "the string is not closed")),
                '\\' if raw => {
                    text.push('\\');
                    if let Some(next) = self.next() {
                        text.push(next);
                    }
                }
                '\\' => self.escape(&mut text)?,
                c => text.push(c),
            }
        }
    }

    fn escape(&mut self, text: &mut String) -> Result<()> {
        let Some(found) = self.next() else {
            return Err(self.here("the string is not closed"));
        };
        let simple = match found {
            '\n' => return Ok(()),
            'n' => '\n',
            't' => '\t',
            'r' => '\r',
            'a' => '\x07',
            'b' => '\x08',
            'f' => '\x0c',
            'v' => '\x0b',
            '\\' | '"' | '\'' => found,
            '0'..='7' => {
                let mut code = found.to_digit(8).expect("octal");
                for _ in 0..2 {
                    match self.peek().and_then(|c| c.to_digit(8)) {
                        Some(digit) => {
                            self.next();
                            code = code * 8 + digit;
                        }
                        None => break,
                    }
                }
                return self.push(text, code);
            }
            'x' => return self.hex(text, 2),
            'u' => return self.hex(text, 4),
            'U' => return self.hex(text, 8),
            other => return Err(self.here(format!("`\\{other}` is not an escape"))),
        };
        text.push(simple);
        Ok(())
    }

    fn hex(&mut self, text: &mut String, digits: usize) -> Result<()> {
        let mut code = 0;
        for _ in 0..digits {
            match self.next().and_then(|c| c.to_digit(16)) {
                Some(digit) => code = code * 16 + digit,
                None => return Err(self.here(format!("an escape needs {digits} hex digits"))),
            }
        }
        self.push(text, code)
    }

    fn push(&self, text: &mut String, code: u32) -> Result<()> {
        match char::from_u32(code) {
            Some(found) => {
                text.push(found);
                Ok(())
            }
            None => Err(self.here(format!("{code:#x} is not a character"))),
        }
    }

    /// Arguments by the function's parameters, as Starlark binds them:
    /// positional ones first, in order, then by name.
    fn bind<const N: usize>(
        &self,
        line: usize,
        function: &str,
        parameters: [&str; N],
        arguments: Vec<(Option<String>, Literal)>,
    ) -> Result<[Option<Literal>; N]> {
        let mut bound: [Option<Literal>; N] = std::array::from_fn(|_| None);
        let mut named = false;
        for (position, (name, value)) in arguments.into_iter().enumerate() {
            let index = match name {
                None if named => {
                    return Err(self.wrong(line, "a positional argument follows a named one"));
                }
                None if position < N => position,
                None => {
                    return Err(self.wrong(line, format!("{function} takes {N} arguments at most")));
                }
                Some(name) => {
                    named = true;
                    match parameters.iter().position(|p| *p == name) {
                        Some(index) => index,
                        None => {
                            return Err(
                                self.wrong(line, format!("{function} has no argument `{name}`"))
                            );
                        }
                    }
                }
            };
            if bound[index].is_some() {
                return Err(self.wrong(
                    line,
                    format!("{function} is given `{}` twice", parameters[index]),
                ));
            }
            // None is how Starlark leaves an argument out.
            bound[index] = Some(value).filter(|value| *value != Literal::None);
        }
        Ok(bound)
    }

    fn prefix_rule(&self, line: usize, arguments: Vec<(Option<String>, Literal)>) -> Result<Rule> {
        let [pattern, decision, examples, counterexamples, justification] = self.bind(
            line,
            "prefix_rule",
            ["pattern", "decision", "match", "not_match", "justification"],
            arguments,
        )?;
        let wrong = |detail: String| self.wrong(line, detail);
        let text = |value: Option<Literal>, what: &str| match value {
            None => Ok(None),
            Some(Literal::Str(text)) => Ok(Some(text)),
            Some(other) => Err(wrong(format!("{what} is a string, not {}", other.kind()))),
        };
        let decision = match text(decision, "decision")? {
            None => Decision::Allow,
            Some(decision) => Decision::parse(&decision).map_err(wrong)?,
        };
        let justification = text(justification, "justification")?;
        if justification
            .as_ref()
            .is_some_and(|text| text.trim().is_empty())
        {
            return Err(wrong("justification is empty".into()));
        }
        let pattern = match pattern {
            None => return Err(wrong("prefix_rule needs a pattern".into())),
            Some(Literal::List(words)) => words,
            Some(other) => {
                return Err(wrong(format!("pattern is a list, not {}", other.kind())));
            }
        };
        let mut words = Vec::new();
        for word in pattern {
            let alternatives = match word {
                Literal::Str(word) => vec![word],
                Literal::List(list) => (list.into_iter())
                    .map(|word| match word {
                        Literal::Str(word) => Ok(word),
                        _ => Err(wrong("a pattern's alternatives must be strings".into())),
                    })
                    .collect::<Result<_>>()?,
                _ => {
                    return Err(wrong(
                        "a pattern's words must be strings or lists of them".into(),
                    ));
                }
            };
            if alternatives.is_empty() {
                return Err(wrong("a pattern's alternatives may not be empty".into()));
            }
            words.push(alternatives);
        }
        if words.is_empty() {
            return Err(wrong("pattern is empty".into()));
        }
        let rule = Rule {
            pattern: words,
            decision,
            justification,
            at: format!("{}:{line}", self.name),
        };
        // Each example is checked against this rule alone, as Codex checks
        // a rule's examples.
        for (examples, what, matches) in [
            (examples, "a match example", true),
            (counterexamples, "a not_match example", false),
        ] {
            let examples = match examples {
                None => Vec::new(),
                Some(Literal::List(examples)) => examples,
                Some(other) => {
                    return Err(wrong(format!(
                        "match and not_match are lists, not {}",
                        other.kind()
                    )));
                }
            };
            for example in examples {
                let example = self::example(example, what).map_err(wrong)?;
                match (rule.matches(&example).is_some(), matches) {
                    (false, true) => {
                        return Err(wrong(format!(
                            "the rule at {} doesn't match its example `{}`",
                            rule.at,
                            example.join(" ")
                        )));
                    }
                    (true, false) => {
                        return Err(wrong(format!(
                            "the rule at {} matches `{}`, which it says it doesn't",
                            rule.at,
                            example.join(" ")
                        )));
                    }
                    _ => {}
                }
            }
        }
        Ok(rule)
    }

    /// Codex pins a program's name to where it may be found; matching here
    /// reads a command's first word as it is written, never by its path,
    /// so the pin changes nothing and is only checked.
    fn host_executable(
        &self,
        line: usize,
        arguments: Vec<(Option<String>, Literal)>,
    ) -> Result<()> {
        let [name, paths] = self.bind(line, "host_executable", ["name", "paths"], arguments)?;
        let wrong = |detail: &str| Err(self.wrong(line, detail));
        match name {
            Some(Literal::Str(name)) if !name.is_empty() && !name.contains('/') => {}
            _ => return wrong("host_executable's name is a program's bare name"),
        }
        let Some(Literal::List(paths)) = paths else {
            return wrong("host_executable's paths are a list of absolute paths");
        };
        for path in paths {
            match path {
                Literal::Str(path) if Path::new(&path).is_absolute() => {}
                _ => return wrong("host_executable's paths are absolute paths"),
            }
        }
        Ok(())
    }
}

/// A string, or a list of strings, as words: a string is read as the
/// sandbox reads a command.
fn example(value: Literal, what: &str) -> std::result::Result<Vec<String>, String> {
    let words = match value {
        Literal::Str(text) => match crate::sandbox::commands(&text).as_deref() {
            Some([words]) => words.clone(),
            _ => return Err(format!("{what} {text:?} is not one plain command")),
        },
        Literal::List(list) => (list.into_iter())
            .map(|word| match word {
                Literal::Str(word) => Ok(word),
                other => Err(format!(
                    "{what} words must be strings, not {}",
                    other.kind()
                )),
            })
            .collect::<std::result::Result<_, _>>()?,
        _ => return Err(format!("{what} must be a string or a list of strings")),
    };
    match words.is_empty() {
        true => Err(format!("{what} is empty")),
        false => Ok(words),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn policy(text: &str) -> Result<Policy> {
        let mut policy = Policy::default();
        policy.parse("test.rules", text)?;
        Ok(policy)
    }

    #[test]
    fn the_strictest_rule_that_matches_decides() {
        let policy = policy(
            r#"
prefix_rule(pattern = ["git"], decision = "prompt")
prefix_rule(
    pattern = ["git", "push", ["--force", "-f"]],
    decision = "forbidden",
    justification = "Use --force-with-lease.",
    match = ["git push --force origin main", ["git", "push", "-f"]],
    not_match = ["git push origin main"],
)
prefix_rule(pattern = ["cargo", ["test", "check"]])
"#,
        )
        .unwrap();
        assert_eq!(policy.len(), 3);
        let decision = |script: &str| policy.check(script).decision;
        assert_eq!(
            decision("git push --force origin main"),
            Some(Decision::Forbidden)
        );
        assert_eq!(decision("git push -f"), Some(Decision::Forbidden));
        assert_eq!(decision("git push origin"), Some(Decision::Prompt));
        assert_eq!(decision("cargo test -p x"), Some(Decision::Allow));
        assert_eq!(decision("cargo check"), Some(Decision::Allow));
        assert_eq!(decision("cargo build"), None);
        assert_eq!(decision("/usr/bin/git status"), None);
        let verdict = policy.check("git push -f");
        assert_eq!(verdict.matched.len(), 2);
        assert_eq!(verdict.matched[1].prefix, ["git", "push", "-f"]);
        assert_eq!(verdict.matched[1].rule, "test.rules:3");
        assert_eq!(
            Rules::new(policy.clone()).rule("git push -f"),
            Ok(Ruling::Forbidden("Use --force-with-lease.".into()))
        );
        assert_eq!(
            Rules::new(policy).rule("git status"),
            Ok(Ruling::Prompt(
                "the rule at test.rules:2 names `git`".into()
            ))
        );
    }

    #[test]
    fn a_script_is_ruled_command_by_command_and_only_a_plain_one() {
        let policy = policy(
            r#"
prefix_rule(pattern = ["mcpx"])
prefix_rule(pattern = ["rm"], decision = "forbidden")
"#,
        )
        .unwrap();
        let decision = |script: &str| policy.check(script).decision;
        assert_eq!(decision("mcpx a && mcpx b | mcpx c"), Some(Decision::Allow));
        // One command no rule names runs as it would with no rules.
        assert_eq!(decision("mcpx a | jq ."), None);
        assert_eq!(decision("ls; rm -rf x"), Some(Decision::Forbidden));
        assert_eq!(decision("mcpx a && rm x"), Some(Decision::Forbidden));
        // As in Codex, what isn't plain matches nothing.
        assert_eq!(decision("echo $(rm -rf x)"), None);
        assert_eq!(decision("mcpx > out"), None);
        assert_eq!(decision(""), None);
    }

    #[test]
    fn a_rule_that_doesnt_read_says_where() {
        for (text, wanted) in [
            (
                "prefix_rule(pattern = [])",
                "test.rules:1: pattern is empty",
            ),
            (
                "\n\nprefix_rule(pattern = [\"git\"], decision = \"deny\")",
                "test.rules:3: decision must be allow, prompt or forbidden, not \"deny\"",
            ),
            (
                "prefix_rule(pattern = [\"git\", \"push\"], match = [\"git pull\"])",
                "the rule at test.rules:1 doesn't match its example `git pull`",
            ),
            (
                "prefix_rule(pattern = [\"git\"], not_match = [[\"git\", \"x\"]])",
                "the rule at test.rules:1 matches `git x`, which it says it doesn't",
            ),
            (
                "prefix_rule(pattern = [\"git\"], justification = \" \")",
                "justification is empty",
            ),
            (
                "network_rule(host = \"x\", protocol = \"https\", decision = \"deny\")",
                "network_rule",
            ),
            ("prefix_rule(pattern = [\"git\"]", "test.rules:1"),
            (
                "prefix_rule(pattern = [\"git\"], color = \"red\")",
                "test.rules:1",
            ),
        ] {
            let error = policy(text).unwrap_err();
            assert_eq!(error.code, "rules_invalid");
            let detail = error.detail.unwrap();
            assert!(detail.contains(wanted), "{text}: {detail}");
        }
        // Starlark beyond literal calls is refused, with where.
        let error = policy(
            "host_executable(name = \"git\", paths = [\"/usr/bin/git\"])\n\
             for tool in [\"ruff\", \"mypy\"]:\n    prefix_rule(pattern = [tool])\n",
        )
        .unwrap_err();
        assert!(error.detail.unwrap().contains("test.rules:2: "));
        let error = policy("prefix_rule(pattern = [tool])").unwrap_err();
        assert!(error.detail.unwrap().contains("`tool` is not a literal"));
    }

    #[test]
    fn rules_read_as_codex_writes_them() {
        // Codex's documented forms, its "don't ask again" line, and
        // Starlark's string literals.
        let policy = policy(
            r#"# Rules.
host_executable(name = "git", paths = ["/usr/bin/git", "/opt/homebrew/bin/git"])
prefix_rule(pattern=["npm", "run", "build"], decision="allow")
prefix_rule(
    pattern = ["docker", ["compose", 'buildx']],  # trailing comment
    decision = "prompt",
    justification = """Ask before
running Docker.""",
    match = [["docker", "compose", "up"]],
    not_match = None,
);  prefix_rule(["say"], "forbidden", None, None, "No \"say\"\tthere é\x21")
prefix_rule(pattern = [r"a\b", "\101\n"], decision = "allow",)
"#,
        )
        .unwrap();
        assert_eq!(policy.len(), 4);
        let rule = &policy.rules[1];
        assert_eq!(
            rule.pattern,
            [
                vec!["docker".to_owned()],
                vec!["compose".into(), "buildx".into()]
            ]
        );
        assert_eq!(
            rule.justification.as_deref(),
            Some("Ask before\nrunning Docker.")
        );
        assert_eq!(rule.at, "test.rules:4");
        assert_eq!(policy.rules[2].at, "test.rules:11");
        assert_eq!(policy.rules[2].decision, Decision::Forbidden);
        assert_eq!(
            policy.rules[2].justification.as_deref(),
            Some("No \"say\"\tthere \u{e9}!")
        );
        assert_eq!(
            policy.rules[3].pattern,
            [vec![r"a\b".to_owned()], vec!["A\n".into()]]
        );
        // What Sonny's "always allow" writes reads back.
        let words = ["cargo", "test", "it's \"quoted\" \\ \u{1F600}"].map(String::from);
        let line = format!(
            "prefix_rule(pattern={}, decision=\"allow\")",
            serde_json::to_string(&words).unwrap()
        );
        let written = super::tests::policy(&line).unwrap();
        assert_eq!(written.rules[0].pattern, words.map(|word| vec![word]));
    }

    #[test]
    fn a_folder_reads_again_when_its_files_change_and_keeps_good_rules() {
        let dir = std::env::temp_dir().join(format!("agent-rules-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let folder = Folder::new(Some(dir.clone()));
        // No folder, no rules.
        assert_eq!(folder.current().rule("rm x"), Ok(Ruling::None));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(
            dir.join("b.rules"),
            "prefix_rule(pattern = [\"rm\"], decision = \"forbidden\")",
        )
        .unwrap();
        std::fs::write(dir.join("a.rules"), "prefix_rule(pattern = [\"rm\"])").unwrap();
        std::fs::write(dir.join("notes.txt"), "not rules").unwrap();
        assert!(matches!(
            folder.current().rule("rm x"),
            Ok(Ruling::Forbidden(_))
        ));
        // A half-written file keeps the rules last read.
        std::fs::write(dir.join("b.rules"), "prefix_rule(pattern = [").unwrap();
        assert!(matches!(
            folder.current().rule("rm x"),
            Ok(Ruling::Forbidden(_))
        ));
        std::fs::write(dir.join("b.rules"), "").unwrap();
        assert_eq!(folder.current().rule("rm x"), Ok(Ruling::Allow));
        // With none read yet, a broken file refuses every command.
        let fresh = Folder::new(Some(dir.clone()));
        std::fs::write(dir.join("b.rules"), "prefix_rule(pattern = [").unwrap();
        let error = fresh.current().rule("ls").unwrap_err();
        assert_eq!(error.code, "rules_invalid");
        assert!(error.detail.unwrap().contains("b.rules:1"));
        std::fs::write(dir.join("b.rules"), "# fixed\n").unwrap();
        assert_eq!(fresh.current().rule("rm x"), Ok(Ruling::Allow));
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
