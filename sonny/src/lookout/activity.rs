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

/// When a task's news may be told, as the subscriptions that heard it say,
/// and which of them told it once it is: nothing heard yet says, so it
/// goes as news always went.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Hold {
    /// Something in it goes as soon as the subscriber rests: heard by a
    /// subscription with no digest, or the person's own.
    pub now: bool,
    /// Subscriptions with a digest that heard it, each with its digest:
    /// no sooner than one allows, this long after a note last told what
    /// that one heard. Each keeps its own time, so the earliest any allows
    /// is when it goes, and a note that tells it, however soon, starts
    /// each one's time again.
    pub windows: Vec<(String, Duration)>,
}

impl Hold {
    /// As soon as the subscriber rests.
    pub fn now() -> Hold {
        Hold {
            now: true,
            windows: Vec::new(),
        }
    }

    /// Two pieces of one task's news together: what may go now takes the
    /// rest along, and windows join, whichever allows first letting both
    /// go.
    pub fn merge(mut self, other: Hold) -> Hold {
        self.now |= other.now;
        for window in other.windows {
            if !self.windows.contains(&window) {
                self.windows.push(window);
            }
        }
        self
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
    let mut hold = Hold {
        now: skips,
        windows: Vec::new(),
    };
    let mut urgent = None;
    for rule in hearing {
        if urgent.is_none()
            && (rule.urgent.as_deref()).is_some_and(|u| super::pattern::matches(u, event))
        {
            urgent = Some(rule.sub.clone());
        }
        match rule.digest {
            Some(digest) => hold.windows.push((rule.sub.clone(), digest)),
            None => hold.now = true,
        }
    }
    // What is urgent goes as it comes by its own mark, while it is still
    // to be told.
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
                hold: Hold::now(),
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
            Hold {
                now: false,
                windows: vec![
                    ("hourly".into(), Duration::from_secs(3600)),
                    ("tenth".into(), Duration::from_secs(600))
                ]
            }
        );
        // What goes now still counts as told for the digests that heard it.
        let hourly_told = Hold {
            now: true,
            windows: vec![("hourly".into(), Duration::from_secs(3600))],
        };
        let plain = rule("plain", None, None, 0);
        assert_eq!(
            hear(&[hourly.clone(), plain], &event, false).unwrap().hold,
            hourly_told
        );
        assert_eq!(hear(&[hourly], &event, true).unwrap().hold, hourly_told);
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
                hold: Hold {
                    now: false,
                    windows: vec![("migration".into(), Duration::from_secs(3600))]
                },
                urgent: Some("migration".into())
            })
        );
        let calm = json!({"type": "turn.ended", "bot": "calc.fix", "status": "completed"});
        assert_eq!(hear(&rules, &calm, false).unwrap().urgent, None);
    }

    #[test]
    fn holds_merge_toward_now_and_keep_every_window() {
        let w = |s: &str, n| (s.to_owned(), Duration::from_secs(n));
        let a = Hold {
            now: false,
            windows: vec![w("a", 50)],
        };
        assert_eq!(Hold::default().merge(a.clone()), a);
        assert_eq!(
            a.clone().merge(Hold::now()),
            Hold {
                now: true,
                windows: vec![w("a", 50)]
            }
        );
        let b = Hold {
            now: false,
            windows: vec![w("b", 5), w("a", 50)],
        };
        assert_eq!(a.merge(b).windows, [w("a", 50), w("b", 5)]);
    }
}
