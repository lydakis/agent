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
//! for it or lists none. Of the rules a command matches, the strictest
//! decides.
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

    /// The rules `command` matches: those for its first word as written,
    /// or, when none match and it is an absolute path, those for the
    /// program's name, unless `host_executables` lists other paths for it.
    pub fn matching(&self, command: &[String]) -> Vec<Matched> {
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
        if (self.host_executables.get(name))
            .is_some_and(|paths| !paths.iter().any(|path| Path::new(path) == program))
        {
            return exact;
        }
        let named: Vec<String> = std::iter::once(name.to_owned())
            .chain(command[1..].iter().cloned())
            .collect();
        found(&named)
    }
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
            ],
            "host_executables": {"git": ["/usr/bin/git"]},
        }));
        let decision = |command: &str| policy.verdict(&[words(command)]).decision;
        assert_eq!(decision("/usr/bin/git push"), Some(Decision::Forbidden));
        assert_eq!(decision("/tmp/git push"), None);
        assert_eq!(decision("/usr/local/bin/rg x"), Some(Decision::Allow));
        assert_eq!(decision("./rg x"), None);
        let verdict = policy.verdict(&[words("/usr/bin/git push")]);
        assert_eq!(verdict.matched[0].prefix, ["/usr/bin/git", "push"]);
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
