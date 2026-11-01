//! Command rules applied to a bot's shell calls. A bot's client gives
//! them as its `rules` setting, Codex's execpolicy as data
//! ([`agent_client::rules`]); reading rules files, and in which language,
//! is the client's. A script is read as the sandbox reads one
//! ([`crate::sandbox::commands`]): each plain command on its own, and
//! anything else matches no rule.
use crate::Error;
pub use agent_client::rules::{Decision, Matched, Policy, Verdict};

/// What `policy` says of `script`, a shell call's command. A script that
/// isn't plain matches nothing.
pub fn check(policy: &Policy, script: &str) -> Verdict {
    match crate::sandbox::commands(script).filter(|c| !c.is_empty()) {
        Some(commands) => policy.verdict(&commands),
        None => Verdict::default(),
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

impl Ruling {
    /// As a planned call keeps it, beside the call: none when no rule
    /// names it.
    pub fn stored(ruling: &std::result::Result<Ruling, Error>) -> Option<String> {
        let value = match ruling {
            Ok(Self::None) => return None,
            Ok(Self::Allow) => serde_json::json!({"decision": "allow"}),
            Ok(Self::Prompt(why)) => serde_json::json!({"decision": "prompt", "why": why}),
            Ok(Self::Forbidden(why)) => serde_json::json!({"decision": "forbidden", "why": why}),
            Err(error) => serde_json::json!({"error": error.code, "detail": error.detail}),
        };
        Some(value.to_string())
    }

    /// What [`Ruling::stored`] kept.
    pub fn read(stored: &str) -> std::result::Result<Ruling, Error> {
        let value: serde_json::Value = serde_json::from_str(stored).unwrap_or_default();
        let why = || value["why"].as_str().unwrap_or_default().to_owned();
        match (value["decision"].as_str(), value["error"].as_str()) {
            (Some("allow"), _) => Ok(Self::Allow),
            (Some("prompt"), _) => Ok(Self::Prompt(why())),
            (Some("forbidden"), _) => Ok(Self::Forbidden(why())),
            (_, Some(code)) => Err(match value["detail"].as_str() {
                Some(detail) => Error::with(code, detail),
                None => Error::new(code),
            }),
            _ => Ok(Self::None),
        }
    }
}

/// The rules a turn runs with: its bot's, or why they couldn't be read;
/// or, as a parked round's calls run, what its announcement ruled of each.
#[derive(Debug, Clone, Default)]
pub struct Rules {
    policy: Policy,
    /// The client couldn't read them: every command is refused with this.
    broken: Option<Error>,
    /// Each planned call's ruling as its round was announced, by call id.
    recorded: Option<std::collections::HashMap<String, std::result::Result<Ruling, Error>>>,
}

impl Rules {
    pub fn new(policy: Policy) -> Self {
        Self {
            broken: (policy.invalid.clone()).map(|why| Error::with("rules_invalid", why)),
            policy,
            recorded: None,
        }
    }

    /// The rulings a round's announcement recorded, by call id: the calls
    /// run as they were announced, also when the rules changed since, as
    /// across a park or a restart. A call with none, none ruled.
    pub fn recorded(
        rulings: std::collections::HashMap<String, std::result::Result<Ruling, Error>>,
    ) -> Self {
        Self {
            recorded: Some(rulings),
            ..Self::default()
        }
    }

    /// What these rules make of the shell call `call_id` running `script`.
    pub fn rule_call(&self, call_id: &str, script: &str) -> std::result::Result<Ruling, Error> {
        match &self.recorded {
            Some(recorded) => recorded.get(call_id).cloned().unwrap_or(Ok(Ruling::None)),
            None => self.rule(script),
        }
    }

    /// What these rules make of a shell call running `script`.
    pub fn rule(&self, script: &str) -> std::result::Result<Ruling, Error> {
        if let Some(error) = &self.broken {
            return Err(error.clone());
        }
        let verdict = check(&self.policy, script);
        let why = |decision: Decision| {
            let rule = (verdict.matched.iter())
                .find(|m| m.decision == decision)
                .expect("the decision is a match's");
            rule.justification
                .clone()
                .unwrap_or_else(|| match &rule.source {
                    Some(source) => {
                        format!("the rule at {source} names `{}`", rule.prefix.join(" "))
                    }
                    None => format!("a rule names `{}`", rule.prefix.join(" ")),
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

#[cfg(test)]
mod tests {
    use super::*;

    fn rules(value: serde_json::Value) -> Rules {
        Rules::new(serde_json::from_value(value).unwrap())
    }

    #[test]
    fn a_script_is_ruled_by_each_of_its_commands() {
        let rules = rules(serde_json::json!({"prefix_rules": [
            {"pattern": ["touch"]},
            {"pattern": ["cp"], "decision": "prompt", "source": "default.rules:2"},
            {"pattern": ["rm"], "decision": "forbidden", "justification": "Use the trash."},
            {"pattern": ["mv"], "decision": "prompt"},
        ]}));
        assert_eq!(rules.rule("touch a && touch b"), Ok(Ruling::Allow));
        // One no rule names keeps the rest in.
        assert_eq!(rules.rule("touch a; ls"), Ok(Ruling::None));
        assert_eq!(
            rules.rule("touch a | cp a b"),
            Ok(Ruling::Prompt(
                "the rule at default.rules:2 names `cp`".into()
            ))
        );
        assert_eq!(
            rules.rule("mv a b"),
            Ok(Ruling::Prompt("a rule names `mv`".into()))
        );
        assert_eq!(
            rules.rule("cp a b && rm -rf c"),
            Ok(Ruling::Forbidden("Use the trash.".into()))
        );
        // What the sandbox can't read as plain commands matches none.
        assert_eq!(rules.rule("touch $(echo a)"), Ok(Ruling::None));
        assert_eq!(rules.rule(""), Ok(Ruling::None));
    }

    #[test]
    fn rules_their_client_couldnt_read_refuse_every_command() {
        let rules = rules(
            serde_json::json!({"invalid": "default.rules:1:1: pattern is empty",
            "prefix_rules": [{"pattern": ["ls"]}]}),
        );
        let error = rules.rule("ls").unwrap_err();
        assert_eq!(error.code, "rules_invalid");
        assert_eq!(
            error.detail.as_deref(),
            Some("default.rules:1:1: pattern is empty")
        );
        // A ruling kept with its call reads back as it was.
        let kept = Ruling::stored(&Err(error.clone())).unwrap();
        assert_eq!(Ruling::read(&kept), Err(error));
        let asked = Ok(Ruling::Prompt("why".into()));
        assert_eq!(Ruling::read(&Ruling::stored(&asked).unwrap()), asked);
        assert_eq!(Ruling::stored(&Ok(Ruling::None)), None);
    }
}
