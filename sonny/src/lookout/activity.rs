//! What the bots a subscription watches did, as typed events: the same
//! shape a source's events have, so a subscription's `filter`, `digest`
//! and `urgent` read both alike. A bot watched only by subscriptions that
//! say none of those costs nothing more: no event is built for it, and its
//! news goes as it always went.
use super::subs::Sub;
use serde_json::Value;
use std::sync::Arc;
use std::time::Duration;

/// The fields a bot's events may have, as the `subscriptions` tool lists
/// them for an agent to filter on.
pub const FIELDS: [(&str, &str); 13] = [
    (
        "type",
        "string: message.written, turn.ended, approval.waiting, ask.posted, turn.silent or note.changed",
    ),
    ("bot", "string, the bot it happened in, as \"calc.fix\""),
    ("maker", "string, the bot that made that bot"),
    (
        "by",
        "string, who wrote the message or asked for the turn: \"you\", \"person\", an agent's name, \"trigger\" or a client",
    ),
    ("turn", "number, the turn's number in its bot"),
    (
        "status",
        "string, how a turn ended: completed, failed, stopped or interrupted",
    ),
    ("resolved", "boolean, the turn resolved its task"),
    ("goal", "string, how the task's goal ended with the turn"),
    (
        "theirs",
        "boolean, the person asked for the turn in the task themselves",
    ),
    (
        "text",
        "string, the person's own words, on a message they wrote",
    ),
    ("question", "string, what a task asked the person"),
    ("options", "list of strings, the answers it offered"),
    ("id", "string, a margin note's id"),
];

/// What one subscription that watches a bot says of its events.
#[derive(Clone, Debug, PartialEq)]
pub struct Rule {
    pub sub: String,
    pub filter: Option<Arc<Value>>,
    pub urgent: Option<Arc<Value>>,
    pub digest: Option<Duration>,
}

impl Rule {
    pub fn of(sub: &Sub) -> Rule {
        Rule {
            sub: sub.name.clone(),
            filter: sub.filter.clone().map(Arc::new),
            urgent: sub.urgent.clone().map(Arc::new),
            digest: sub.digest_span(),
        }
    }
}

/// When a task's news may be told, as the subscriptions that heard it say.
#[derive(Clone, Debug, Default, PartialEq)]
pub enum Hold {
    /// Nothing heard yet says.
    #[default]
    Unset,
    /// As soon as the subscriber rests, as news always went.
    Now,
    /// No sooner than this long after a note last told what this
    /// subscription held.
    Window(String, Duration),
}

impl Hold {
    /// Two pieces of one task's news together: what may go now takes the
    /// rest along, and of two windows the shorter holds.
    pub fn merge(self, other: Hold) -> Hold {
        match (self, other) {
            (Hold::Unset, other) | (other, Hold::Unset) => other,
            (Hold::Now, _) | (_, Hold::Now) => Hold::Now,
            (Hold::Window(a, x), Hold::Window(b, y)) => match y < x {
                true => Hold::Window(b, y),
                false => Hold::Window(a, x),
            },
        }
    }
}

/// What the subscriptions make of one event they watch.
#[derive(Clone, Debug, PartialEq)]
pub struct Heard {
    pub hold: Hold,
    /// The subscription whose `urgent` matched it, if one did.
    pub urgent: Option<String>,
}

/// What `rules`, every subscription that watches the event's bot, make
/// of `event`: none when no filter among them hears it. One that `skips`,
/// as the person's words, a question or an approval only they can give,
/// waits for no digest.
pub fn hear(rules: &[Rule], event: &Value, skips: bool) -> Option<Heard> {
    let mut hearing = (rules.iter())
        .filter(|rule| (rule.filter.as_deref()).is_none_or(|f| super::pattern::matches(f, event)))
        .peekable();
    hearing.peek()?;
    let mut hold = Hold::Unset;
    let mut urgent = None;
    for rule in hearing {
        if urgent.is_none()
            && (rule.urgent.as_deref()).is_some_and(|u| super::pattern::matches(u, event))
        {
            urgent = Some(rule.sub.clone());
        }
        hold = hold.merge(match (skips, rule.digest) {
            (false, Some(digest)) => Hold::Window(rule.sub.clone(), digest),
            _ => Hold::Now,
        });
    }
    if urgent.is_some() {
        hold = Hold::Now;
    }
    Some(Heard { hold, urgent })
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn rule(sub: &str, filter: Option<Value>, urgent: Option<Value>, digest: u64) -> Rule {
        Rule {
            sub: sub.into(),
            filter: filter.map(Arc::new),
            urgent: urgent.map(Arc::new),
            digest: (digest > 0).then(|| Duration::from_secs(digest)),
        }
    }

    #[test]
    fn a_filter_hears_only_what_it_matches_and_any_hearer_will_do() {
        let failed = json!({"type": ["turn.ended"], "status": ["failed"]});
        let event = json!({"type": "turn.ended", "bot": "calc.fix", "status": "completed"});
        assert_eq!(
            hear(
                &[rule("fails", Some(failed.clone()), None, 0)],
                &event,
                false
            ),
            None
        );
        let both = [
            rule("fails", Some(failed), None, 0),
            rule("all", None, None, 0),
        ];
        assert_eq!(
            hear(&both, &event, false),
            Some(Heard {
                hold: Hold::Now,
                urgent: None
            })
        );
    }

    #[test]
    fn a_digest_holds_unless_another_hearer_has_none_or_the_event_skips_it() {
        let event = json!({"type": "turn.ended", "bot": "calc.fix", "status": "completed"});
        let hourly = rule("hourly", None, None, 3600);
        let tenth = rule("tenth", None, None, 600);
        assert_eq!(
            hear(&[hourly.clone(), tenth.clone()], &event, false)
                .unwrap()
                .hold,
            Hold::Window("tenth".into(), Duration::from_secs(600))
        );
        let plain = rule("plain", None, None, 0);
        assert_eq!(
            hear(&[hourly.clone(), plain], &event, false).unwrap().hold,
            Hold::Now
        );
        assert_eq!(hear(&[hourly], &event, true).unwrap().hold, Hold::Now);
    }

    #[test]
    fn an_urgent_match_names_its_subscription_and_waits_for_no_digest() {
        let event = json!({"type": "turn.ended", "bot": "calc.fix", "status": "failed"});
        let rules = [rule(
            "migration",
            None,
            Some(json!({"status": ["failed"]})),
            3600,
        )];
        assert_eq!(
            hear(&rules, &event, false),
            Some(Heard {
                hold: Hold::Now,
                urgent: Some("migration".into())
            })
        );
        let calm = json!({"type": "turn.ended", "bot": "calc.fix", "status": "completed"});
        assert_eq!(hear(&rules, &calm, false).unwrap().urgent, None);
    }

    #[test]
    fn holds_merge_toward_now_and_the_shorter_window() {
        let w = |s: &str, n| Hold::Window(s.into(), Duration::from_secs(n));
        assert_eq!(Hold::Unset.merge(w("a", 5)), w("a", 5));
        assert_eq!(w("a", 5).merge(Hold::Now), Hold::Now);
        assert_eq!(w("a", 50).merge(w("b", 5)), w("b", 5));
        assert_eq!(w("a", 5).merge(w("b", 50)), w("a", 5));
    }
}
