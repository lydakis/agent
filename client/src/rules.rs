//! Command rules as data: what a client gives a bot as its `rules` setting
//! and the daemon applies to the bot's shell calls. The shape is Codex's
//! execpolicy (`prefix_rule` and `host_executable`), written as JSON, and
//! the matching is ported from Codex's `execpolicy` crate (Apache-2.0; see
//! NOTICE). Reading rules files is the client's: Sonny runs Codex's
//! Starlark files and passes what they made.
//!
//! A rule's `pattern` is words in order, any of them a list of
//! alternatives. A command matches a rule when its words start with the
//! pattern's; one run by an absolute path that no rule names as written is
//! matched by its program's name, when `host_executables` lists that path
//! for it or lists none, though only one it lists is allowed by that name.
//! A command run under a wrapper this reads, such as `timeout 60 git
//! status`, is also matched as the command it runs ([`unwrapped`]). Of
//! the rules a command matches, the strictest decides.
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::path::Path;

/// What a rule says of a command it matches, least strict first.
#[derive(
    Debug, Clone, Copy, Default, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize,
)]
#[serde(rename_all = "lowercase")]
pub enum Decision {
    /// Runs without asking, outside a sandbox the bot has.
    #[default]
    Allow,
    /// Waits for the bot's approver, sandbox or not.
    Prompt,
    /// Refused.
    Forbidden,
}

impl Decision {
    pub fn parse(text: &str) -> Result<Self, String> {
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

/// One word of a pattern: the word, or a list of alternatives.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(untagged)]
pub enum Word {
    One(String),
    Any(Vec<String>),
}

impl Word {
    fn matches(&self, word: &str) -> bool {
        match self {
            Self::One(one) => one == word,
            Self::Any(any) => any.iter().any(|one| one == word),
        }
    }
}

/// One `prefix_rule`.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PrefixRule {
    pub pattern: Vec<Word>,
    #[serde(default)]
    pub decision: Decision,
    /// Said to the model when the rule refuses or asks.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub justification: Option<String>,
    /// Where the rule was written, such as `default.rules:3`, for what is
    /// said of it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source: Option<String>,
}

impl PrefixRule {
    /// The words of `command` this rule's pattern matches, when it does.
    pub fn matches<'a>(&self, command: &'a [String]) -> Option<&'a [String]> {
        let prefix = command.get(..self.pattern.len())?;
        (prefix.iter().zip(&self.pattern))
            .all(|(word, pattern)| pattern.matches(word))
            .then_some(prefix)
    }
}

/// A bot's command rules, as its `rules` setting holds them.
#[derive(Debug, Clone, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Policy {
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub prefix_rules: Vec<PrefixRule>,
    /// Where each program named by its bare name may be found, as
    /// `host_executable` says: absolute paths ending in the name.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub host_executables: BTreeMap<String, Vec<String>>,
    /// The rules could not be read, and why: every shell call is refused
    /// with this until they can.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub invalid: Option<String>,
}

/// A rule a command matched.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Matched {
    /// The command's words the rule matched.
    pub prefix: Vec<String>,
    pub decision: Decision,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub justification: Option<String>,
    /// Where the rule was written, when that is known.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub source: Option<String>,
}

/// What the rules say of a script: the strictest decision, when one rules
/// it, and each rule that matched.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
pub struct Verdict {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub decision: Option<Decision>,
    pub matched: Vec<Matched>,
}

impl Policy {
    /// Refuses rules that could never match as written, with why.
    pub fn validate(&self) -> Result<(), String> {
        for rule in &self.prefix_rules {
            if rule.pattern.is_empty() {
                return Err("a rule's pattern is empty".into());
            }
            if (rule.pattern.iter()).any(|word| matches!(word, Word::Any(any) if any.is_empty())) {
                return Err("a pattern's alternatives may not be empty".into());
            }
            if (rule.justification.as_ref()).is_some_and(|text| text.trim().is_empty()) {
                return Err("a rule's justification is empty".into());
            }
        }
        for (name, paths) in &self.host_executables {
            let bare = Path::new(name);
            if name.is_empty()
                || bare.components().count() != 1
                || bare.file_name().and_then(|n| n.to_str()) != Some(name)
            {
                return Err(format!(
                    "host_executables names a program by its bare name, not {name:?}"
                ));
            }
            for path in paths {
                let path = Path::new(path);
                if !path.is_absolute() {
                    return Err(format!("host_executables paths are absolute, not {path:?}"));
                }
                if path.file_name().and_then(|n| n.to_str()) != Some(name) {
                    return Err(format!(
                        "host_executables path {path:?} must end in `{name}`"
                    ));
                }
            }
        }
        Ok(())
    }

    /// What these rules say of one command or several, by their words. One
    /// of several commands is forbidden, or asks, when any of them is; they
    /// are allowed only when every one is, since one no rule names runs as
    /// it would with none.
    pub fn verdict(&self, commands: &[Vec<String>]) -> Verdict {
        let mut verdict = Verdict::default();
        let mut all_allowed = true;
        let mut strictest = None;
        for command in commands {
            let matched = self.matching(command);
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

    /// The rules `command` matches as written and, under each wrapper
    /// [`unwrapped`] reads, as the command the wrapper runs. Through a
    /// wrapper that changes what the command runs with (its environment or
    /// folder), only the rules that ask or refuse match: the same words
    /// could run another program there, so they may not let it out.
    pub fn matching(&self, command: &[String]) -> Vec<Matched> {
        let mut matched = self.matching_words(command);
        let (mut rest, mut plain) = (command, true);
        // Each wrapper read takes at least its own word, so this ends.
        while let Some(wrapped) = unwrapped(rest) {
            plain &= wrapped.plain;
            let mut found = self.matching_words(wrapped.command);
            if !plain {
                found.retain(|m| m.decision != Decision::Allow);
            }
            matched.extend(found);
            rest = wrapped.command;
        }
        matched
    }

    /// The rules `command` matches by its own words: those for its first
    /// word as written, or, when none match and it is an absolute path,
    /// those for the program's name, unless `host_executables` lists other
    /// paths for it; its `allow` rules only when `host_executables` lists
    /// this path.
    fn matching_words(&self, command: &[String]) -> Vec<Matched> {
        let found = |words: &[String]| -> Vec<Matched> {
            (self.prefix_rules.iter())
                .filter(|rule| rule.matches(words).is_some())
                .map(|rule| Matched {
                    prefix: command[..rule.pattern.len()].to_vec(),
                    decision: rule.decision,
                    justification: rule.justification.clone(),
                    source: rule.source.clone(),
                })
                .collect()
        };
        let exact = found(command);
        if !exact.is_empty() {
            return exact;
        }
        let Some(program) = command.first().map(Path::new).filter(|p| p.is_absolute()) else {
            return exact;
        };
        let Some(name) = program.file_name().and_then(|name| name.to_str()) else {
            return exact;
        };
        let pinned = match self.host_executables.get(name) {
            Some(paths) if !paths.iter().any(|path| Path::new(path) == program) => return exact,
            Some(_) => true,
            None => false,
        };
        let named: Vec<String> = std::iter::once(name.to_owned())
            .chain(command[1..].iter().cloned())
            .collect();
        let mut matched = found(&named);
        // Where Codex differs: a path no `host_executable` names could be a
        // file the bot wrote itself, so the name's rules may ask about it
        // or refuse it but never let it out unasked.
        if !pinned {
            matched.retain(|m| m.decision != Decision::Allow);
        }
        matched
    }
}

/// A command as a wrapper runs it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Wrapped<'a> {
    /// The words of the command the wrapper runs.
    pub command: &'a [String],
    /// Whether the command runs as it would on its own: false when the
    /// wrapper sets its environment or its folder.
    pub plain: bool,
}

/// The command `command` runs under a wrapper read by its words, when it
/// is one of these, by its bare name and in a form written out here; any
/// other form, such as a wrapper by its path, which could be a program the
/// bot wrote, is not read and is ruled as written:
///
/// - `timeout [-k D] [-s SIGNAL] [--foreground] [--preserve-status] [-v] DURATION COMMAND`
/// - `nice [-n N | -N] COMMAND` and `nohup COMMAND`
/// - `time [-p] COMMAND`
/// - `env [-i] [-u NAME] [-C DIR] [NAME=VALUE]... COMMAND`, plain only
///   bare
/// - `NAME=VALUE... COMMAND`, the shell's assignments, never plain
pub fn unwrapped(command: &[String]) -> Option<Wrapped<'_>> {
    let first = command.first()?.as_str();
    let (rest, plain) = if is_assignment(first) {
        let set = command.iter().take_while(|w| is_assignment(w)).count();
        (&command[set..], false)
    } else {
        let args = &command[1..];
        match first {
            "timeout" => (after_timeout(args)?, true),
            "nice" => (after_nice(args)?, true),
            "nohup" => (args.strip_prefix(&["--".to_owned()]).unwrap_or(args), true),
            "time" => (args.strip_prefix(&["-p".to_owned()]).unwrap_or(args), true),
            "env" => after_env(args)?,
            _ => return None,
        }
    };
    (!rest.is_empty()).then_some(Wrapped {
        command: rest,
        plain,
    })
}

/// `NAME=VALUE` as the shell reads an assignment before a command.
fn is_assignment(word: &str) -> bool {
    word.split_once('=').is_some_and(|(name, _)| {
        let mut chars = name.chars();
        chars
            .next()
            .is_some_and(|c| c == '_' || c.is_ascii_alphabetic())
            && chars.all(|c| c == '_' || c.is_ascii_alphanumeric())
    })
}

/// What `timeout`'s options and duration leave: the command.
fn after_timeout(args: &[String]) -> Option<&[String]> {
    let mut at = 0;
    loop {
        let arg = args.get(at)?.as_str();
        at += 1;
        match arg {
            "--" => break,
            "--foreground" | "-f" | "--preserve-status" | "-p" | "-v" | "--verbose" => {}
            "-k" | "--kill-after" => {
                is_duration(args.get(at)?).then_some(())?;
                at += 1;
            }
            "-s" | "--signal" => {
                is_signal(args.get(at)?).then_some(())?;
                at += 1;
            }
            _ if arg.starts_with('-') && !is_duration(arg) => {
                let (value, valid): (&str, fn(&str) -> bool) =
                    match (arg.strip_prefix("--kill-after="), arg.strip_prefix("-k")) {
                        (Some(value), _) | (None, Some(value)) => (value, is_duration),
                        _ => match (arg.strip_prefix("--signal="), arg.strip_prefix("-s")) {
                            (Some(value), _) | (None, Some(value)) => (value, is_signal),
                            _ => return None,
                        },
                    };
                valid(value).then_some(())?;
            }
            _ => {
                at -= 1;
                break;
            }
        }
    }
    is_duration(args.get(at)?).then(|| &args[at + 1..])
}

/// A duration as `timeout` reads one: a number, whole or not, and a unit.
fn is_duration(word: &str) -> bool {
    let number = word.strip_suffix(['s', 'm', 'h', 'd']).unwrap_or(word);
    let (whole, fraction) = number.split_once('.').unwrap_or((number, ""));
    !(whole.is_empty() && fraction.is_empty())
        && whole.chars().all(|c| c.is_ascii_digit())
        && fraction.chars().all(|c| c.is_ascii_digit())
}

/// A signal by its name or number, as `timeout -s` takes one.
fn is_signal(word: &str) -> bool {
    !word.is_empty() && word.chars().all(|c| c.is_ascii_alphanumeric() || c == '+')
}

/// What `nice`'s adjustment leaves: the command.
fn after_nice(args: &[String]) -> Option<&[String]> {
    let number = |word: &str| {
        let digits = word.strip_prefix(['-', '+']).unwrap_or(word);
        !digits.is_empty() && digits.chars().all(|c| c.is_ascii_digit())
    };
    let first = args.first().map(String::as_str);
    let taken = match first {
        Some("-n" | "--adjustment") => number(args.get(1)?).then_some(2)?,
        Some(arg) if arg.starts_with('-') => {
            let value = (arg.strip_prefix("--adjustment="))
                .or_else(|| arg.strip_prefix("-n"))
                .or_else(|| arg.strip_prefix('-'))?;
            number(value).then_some(1)?
        }
        _ => 0,
    };
    let args = &args[taken..];
    Some(args.strip_prefix(&["--".to_owned()]).unwrap_or(args))
}

/// What `env`'s options and assignments leave, the command, and whether
/// it runs as it would without them.
fn after_env(args: &[String]) -> Option<(&[String], bool)> {
    let mut at = 0;
    while let Some(arg) = args.get(at).map(String::as_str) {
        match arg {
            "--" => {
                at += 1;
                break;
            }
            "-i" | "--ignore-environment" | "-" => at += 1,
            "-u" | "--unset" | "-C" | "--chdir" => {
                args.get(at + 1)?;
                at += 2;
            }
            _ if arg.starts_with("--unset=") || arg.starts_with("--chdir=") => at += 1,
            _ if arg.len() > 2 && (arg.starts_with("-u") || arg.starts_with("-C")) => at += 1,
            // Another option, such as -S, which splits a word into more.
            _ if arg.starts_with('-') => return None,
            _ => break,
        }
    }
    let set = args[at..]
        .iter()
        .take_while(|word| word.contains('='))
        .count();
    Some((&args[at + set..], at + set == 0))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn words(text: &str) -> Vec<String> {
        text.split(' ').map(String::from).collect()
    }

    fn policy(value: serde_json::Value) -> Policy {
        let policy: Policy = serde_json::from_value(value).unwrap();
        policy.validate().unwrap();
        policy
    }

    #[test]
    fn the_strictest_rule_that_matches_decides() {
        let policy = policy(serde_json::json!({"prefix_rules": [
            {"pattern": ["git"], "decision": "prompt", "source": "default.rules:1"},
            {"pattern": ["git", "push", ["--force", "-f"]], "decision": "forbidden",
             "justification": "Use --force-with-lease."},
            {"pattern": ["cargo", ["test", "check"]]},
        ]}));
        let decision = |command: &str| policy.verdict(&[words(command)]).decision;
        assert_eq!(decision("git push -f"), Some(Decision::Forbidden));
        assert_eq!(decision("git push origin"), Some(Decision::Prompt));
        assert_eq!(decision("cargo check"), Some(Decision::Allow));
        assert_eq!(decision("cargo build"), None);
        let verdict = policy.verdict(&[words("git push -f")]);
        assert_eq!(verdict.matched.len(), 2);
        assert_eq!(
            verdict.matched[0].source.as_deref(),
            Some("default.rules:1")
        );
        // Of several commands, one no rule names keeps them from going out.
        let both = policy.verdict(&[words("cargo test"), words("ls")]);
        assert_eq!(both.decision, None);
    }

    #[test]
    fn a_program_by_its_path_is_ruled_by_its_name_where_it_is_found() {
        let policy = policy(serde_json::json!({
            "prefix_rules": [
                {"pattern": ["git", "push"], "decision": "forbidden"},
                {"pattern": ["rg"]},
                {"pattern": ["rm"], "decision": "forbidden"},
            ],
            "host_executables": {"git": ["/usr/bin/git"]},
        }));
        let decision = |command: &str| policy.verdict(&[words(command)]).decision;
        assert_eq!(decision("/usr/bin/git push"), Some(Decision::Forbidden));
        assert_eq!(decision("/tmp/git push"), None);
        // A path no host_executable names may ask or refuse by the name's
        // rules, but is never let out by them: the bot could have written it.
        assert_eq!(decision("/tmp/rg x"), None);
        assert_eq!(decision("/tmp/rm -rf x"), Some(Decision::Forbidden));
        assert_eq!(decision("./rg x"), None);
        let verdict = policy.verdict(&[words("/usr/bin/git push")]);
        assert_eq!(verdict.matched[0].prefix, ["/usr/bin/git", "push"]);
    }

    #[test]
    fn a_wrapped_command_is_ruled_as_the_command_it_runs() {
        let policy = policy(serde_json::json!({"prefix_rules": [
            {"pattern": ["mcpx"]},
            {"pattern": ["git", "push"], "decision": "forbidden", "source": "default.rules:2"},
            {"pattern": ["make"], "decision": "prompt"},
        ]}));
        let decision = |command: &str| policy.verdict(&[words(command)]).decision;
        for allowed in [
            "mcpx call",
            "timeout 60 mcpx call",
            "timeout -k 5 -s KILL 1.5m mcpx call",
            "timeout --signal=TERM --kill-after=10s --preserve-status 30 mcpx",
            "timeout -sINT -- 60 mcpx",
            "nice -n 10 mcpx",
            "nice -5 timeout 60 nohup mcpx",
            "time -p env mcpx",
        ] {
            assert_eq!(decision(allowed), Some(Decision::Allow), "{allowed}");
        }
        // What sets the command's environment or folder, or a wrapper by
        // its path, may still ask or refuse, but never lets it out.
        for kept in [
            "env PATH=/tmp mcpx",
            "env -C /tmp mcpx",
            "env -i mcpx",
            "PATH=/tmp mcpx",
            "timeout 60 env FOO=1 mcpx",
            "/tmp/timeout 60 mcpx",
        ] {
            assert_eq!(decision(kept), None, "{kept}");
        }
        for refused in [
            "timeout 60 git push",
            "env PATH=/tmp git push -f",
            "FOO=1 nice git push",
            "nohup -- time git push",
        ] {
            assert_eq!(decision(refused), Some(Decision::Forbidden), "{refused}");
        }
        assert_eq!(
            decision("env -u HOME make"),
            Some(Decision::Prompt),
            "an unset is read too"
        );
        // Forms not written out here are ruled as written.
        for unread in [
            "timeout 60",
            "timeout --frobnicate 60 git push",
            "timeout sixty git push",
            "env -S 'git push' x",
            "nice -n x git push",
            "command git push",
        ] {
            assert_eq!(decision(unread), None, "{unread}");
        }
        let verdict = policy.verdict(&[words("timeout 60 git push")]);
        assert_eq!(verdict.matched[0].prefix, ["git", "push"]);
        assert_eq!(
            verdict.matched[0].source.as_deref(),
            Some("default.rules:2")
        );
        // A rule for the wrapper itself still decides as written.
        let wrapper = policy_with_timeout();
        assert_eq!(
            wrapper.verdict(&[words("timeout 60 ls")]).decision,
            Some(Decision::Prompt)
        );
    }

    fn policy_with_timeout() -> Policy {
        policy(serde_json::json!({"prefix_rules": [
            {"pattern": ["timeout"], "decision": "prompt"},
            {"pattern": ["ls"]},
        ]}))
    }

    #[test]
    fn rules_that_could_never_match_are_refused() {
        for (value, wanted) in [
            (
                serde_json::json!({"prefix_rules": [{"pattern": []}]}),
                "pattern is empty",
            ),
            (
                serde_json::json!({"prefix_rules": [{"pattern": [[]]}]}),
                "alternatives may not be empty",
            ),
            (
                serde_json::json!({"prefix_rules": [{"pattern": ["x"], "justification": " "}]}),
                "justification is empty",
            ),
            (
                serde_json::json!({"host_executables": {"git": ["git"]}}),
                "are absolute",
            ),
            (
                serde_json::json!({"host_executables": {"git": ["/usr/bin/hg"]}}),
                "must end in `git`",
            ),
            (
                serde_json::json!({"host_executables": {"a/git": []}}),
                "bare name",
            ),
        ] {
            let policy: Policy = serde_json::from_value(value).unwrap();
            let error = policy.validate().unwrap_err();
            assert!(error.contains(wanted), "{error}");
        }
        assert!(
            serde_json::from_value::<Policy>(serde_json::json!({"prefix_rules": [
                {"pattern": ["x"], "decision": "deny"}
            ]}))
            .is_err()
        );
    }
}
