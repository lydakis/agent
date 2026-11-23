//! The agent source: what happens in the tasks a subscriber watches, as
//! events in the one envelope every source uses (`sources::Event`). Its
//! topic is always the task's bot name, and it has five kinds:
//!
//! - `wrote`: someone other than the subscriber wrote in the task.
//! - `turn.ended`: a turn ended, with the task's reply.
//! - `question`: the task asked the person with the ask tool.
//! - `approval`: a call reached the person for approval.
//! - `margin`: the task put a margin note, or took one away.
//!
//! It holds the logic of this source alone, as any source holds its own:
//! who asked for each turn, whose words are whose, and two things it does
//! not say: how a turn a lookout note started ends (that turn answers the
//! note, so telling it would only loop), and an automation's run that
//! went well and said nothing (a schedule's quiet run is no event).
//! Nothing else is judged here: the lookout delivers what this says.
use super::sources::{Event, text_cut as cut};
use serde_json::{Value, json};
use std::collections::{HashMap, HashSet};
use std::sync::Arc;

/// The agent source's name, which no source in the settings may take.
pub const SOURCE: &str = agent_client::config::TASKS_SOURCE;

/// Who wrote a message with no sender, origin or person named.
pub const PERSON: &str = "the person";

/// The fields its events have, as the `follows` tool lists them for an
/// agent to filter on.
pub const FIELDS: [(&str, &str); 15] = [
    (
        "type",
        "string: wrote, turn.ended, question, approval or margin",
    ),
    (
        "bot",
        "string, the task it happened in, as \"calc.fix\": the topic",
    ),
    ("maker", "string, the bot that made that task"),
    (
        "from",
        "string, who wrote it: \"the person\", an agent's name, a client or \"someone\"; for the other kinds, the task",
    ),
    ("turn", "number, the turn's number in its task"),
    (
        "text",
        "string, what was written, the task's reply, the question, what waits for approval, or the margin note's handle",
    ),
    (
        "status",
        "string, how a turn ended: completed, failed, stopped or interrupted",
    ),
    (
        "done",
        "boolean, the task closed itself in the turn: it is done",
    ),
    (
        "asked_by",
        "string, who asked for the turn: \"you\", \"the person\", an agent's name, \"automation\" or a client",
    ),
    ("options", "list of strings, the answers a question offered"),
    (
        "command",
        "string, the line a question asked the person to run, for a step only they can take",
    ),
    ("call", "string, the tool a call waiting for approval calls"),
    (
        "reason",
        "string, why the reviewer handed a call waiting for approval to the person",
    ),
    ("id", "string, a margin note's id"),
    ("removed", "boolean, the margin note was taken away"),
];

type Key = (String, i64);

/// What a pending event still needs read before it is built.
#[derive(Clone, Debug, PartialEq)]
pub enum Kind {
    /// The words someone wrote, at their history node, or still queued in
    /// their turn's record; `brief` is the node of the subscriber's latest
    /// message to the task, words with no sender repeating it being not
    /// the person's.
    Words {
        from: String,
        node: Option<i64>,
        brief: Option<i64>,
    },
    /// A turn's end, with the reply it ended on; who asked, when the
    /// source did not see the turn start, is read with it.
    Reply {
        node: Option<i64>,
        completed: bool,
        status: String,
        error: Option<String>,
        done: bool,
        asked_by: Option<String>,
    },
    /// A call waiting on the person, whose arguments are in the history
    /// or the approvals listing; `reason` is the reviewer's, when it
    /// handed the call over.
    Call {
        call: String,
        name: Option<String>,
        node: Option<i64>,
        reason: Option<String>,
    },
}

/// An event to build once something is read.
#[derive(Clone, Debug, PartialEq)]
pub struct Pending {
    pub task: String,
    pub maker: Option<String>,
    pub turn: i64,
    /// Where in the stream it happened: a task's events are told in this
    /// order.
    pub order: i64,
    pub kind: Kind,
}

/// What one of the daemon's events makes.
#[derive(Debug)]
pub enum Out {
    Now(Box<Event>),
    Read(Pending),
}

/// Who asked for a turn, before it is known what it comes to.
#[derive(Clone, Debug, PartialEq)]
enum Asker {
    /// Named as its end names them.
    By(String),
    /// A lookout note: the turn answers it, and its end is no event.
    Note,
}

/// The agent source's view of the tasks one subscriber watches.
pub struct Activity {
    lead: String,
    /// Who asked for each turn seen starting, until it ends.
    asked: HashMap<Key, Asker>,
    /// Steers waiting to be taken in, by task and message: who sent it,
    /// `None` for the subscriber.
    steers: HashMap<Key, Option<String>>,
    /// Queued messages whose words were read as they were queued.
    pub queued: HashSet<Key>,
    /// The node of the subscriber's latest message to each task, until
    /// the person writes to it.
    briefs: HashMap<String, i64>,
    /// Turns that closed their task.
    resolving: crate::resolved::Own,
    /// Margin calls under way: the note's id and whether it goes.
    margin_calls: HashMap<(String, String), (String, bool)>,
    /// Ask calls under way: what they asked.
    ask_calls: HashMap<(String, String), crate::asks::Ask>,
    /// Calls told as waiting on the person, so one asked twice is one event.
    approvals: HashSet<(String, String)>,
}

/// The most calls of each kind kept under way: past it they are let go.
const CALLS: usize = 4096;

impl Activity {
    pub fn new(lead: &str) -> Self {
        Activity {
            lead: lead.to_owned(),
            asked: HashMap::new(),
            steers: HashMap::new(),
            queued: HashSet::new(),
            briefs: HashMap::new(),
            resolving: Default::default(),
            margin_calls: HashMap::new(),
            ask_calls: HashMap::new(),
            approvals: HashSet::new(),
        }
    }

    /// The task under `name` is gone: what was kept for it goes.
    pub fn gone(&mut self, name: &str) {
        self.asked.retain(|(task, _), _| task != name);
        self.steers.retain(|(task, _), _| task != name);
        self.queued.retain(|(task, _)| task != name);
        self.briefs.remove(name);
        self.margin_calls.retain(|(task, _), _| task != name);
        self.ask_calls.retain(|(task, _), _| task != name);
        self.approvals.retain(|(task, _)| task != name);
    }

    /// Who wrote a message, as its event names them; `None` for the
    /// subscriber, whose words are no event to it, for a lookout note, an
    /// automation's run and the daemon telling a bot its background
    /// command ended: how their turns end is the event.
    fn author(&self, data: &Value) -> Option<String> {
        if (data["request_id"].as_str()).is_some_and(|id| id.starts_with("lookout-"))
            || (data["origin"]
                .as_str()
                .is_some_and(|origin| crate::trigger::sent_it(origin) || origin == "background")
                && data["from"]["bot"].is_null())
        {
            return None;
        }
        let by = (data["origin"].as_str()).or(data["person"].as_str());
        match (data["from"]["bot"].as_str(), by) {
            (Some(bot), _) if bot == self.lead => None,
            (Some(bot), _) => Some(bot.to_owned()),
            (None, Some(by)) => Some(by.to_owned()),
            (None, None) => Some(PERSON.to_owned()),
        }
    }

    /// Who asked for a turn a message started, as its end names them.
    fn asker(&self, data: &Value) -> Asker {
        if (data["request_id"].as_str()).is_some_and(|id| id.starts_with("lookout-")) {
            return Asker::Note;
        }
        let origin = (data["origin"].as_str()).or(data["person"].as_str());
        Asker::By(by(&self.lead, data["from"]["bot"].as_str(), origin))
    }

    /// Someone wrote to the task, `None` for the subscriber: its words are
    /// the task's brief; the person's own words to it end that.
    fn briefed_by(&mut self, task: &str, from: Option<&str>, node: Option<i64>) {
        match (from, node) {
            (Some(PERSON), _) => {
                self.briefs.remove(task);
            }
            (None, Some(node)) => {
                self.briefs.insert(task.to_owned(), node);
            }
            _ => {}
        }
    }

    /// One of the daemon's events in a task the subscriber watches, at
    /// `order` in the stream: the events it makes, now or once read.
    pub fn event(&mut self, event: &Value, maker: Option<&str>, order: i64) -> Vec<Out> {
        let (Some(kind), Some(task)) = (event["event"].as_str(), event["bot"].as_str()) else {
            return Vec::new();
        };
        let data = &event["data"];
        let Some(turn) = super::number(event) else {
            return Vec::new();
        };
        self.resolving.event(event, &self.lead);
        let key = (task.to_owned(), turn);
        let pending = |kind: Kind| Pending {
            task: task.to_owned(),
            maker: maker.map(str::to_owned),
            turn,
            order,
            kind,
        };
        let words = |from: String, node: Option<i64>, brief: Option<i64>| {
            pending(Kind::Words { from, node, brief })
        };
        let mut out = Vec::new();
        match kind {
            "accepted" => {
                // Words read when they were queued are not read again, but
                // still say who briefed the task.
                let queued = self.queued.remove(&key);
                let asker = self.asker(data);
                self.asked.entry(key).or_insert(asker);
                let node = data["node"].as_i64();
                match self.author(data) {
                    Some(from) => {
                        if !queued {
                            let brief = self.briefs.get(task).copied();
                            out.push(Out::Read(words(from.clone(), node, brief)));
                        }
                        self.briefed_by(task, Some(&from), node);
                    }
                    None if data["from"]["bot"] == self.lead.as_str() => {
                        self.briefed_by(task, None, node);
                    }
                    None => {}
                }
            }
            // A steer joins a running turn; its words are read once taken in.
            "queued" if data["delivery"] == "steer" => {
                let from = self.author(data);
                if from.is_some() || data["from"]["bot"] == self.lead.as_str() {
                    self.steers.insert(key, from);
                }
            }
            // Queued work waits for a busy task, so it is read now.
            "queued" => {
                self.asked.insert(key.clone(), self.asker(data));
                if let Some(from) = self.author(data) {
                    let brief = self.briefs.get(task).copied();
                    self.queued.insert(key);
                    out.push(Out::Read(words(from, None, brief)));
                }
            }
            "steered" => {
                let Some(message) = data["message"].as_i64() else {
                    return out;
                };
                let node = data["node"].as_i64();
                match self.steers.remove(&(task.to_owned(), message)) {
                    Some(Some(from)) => {
                        let brief = self.briefs.get(task).copied();
                        out.push(Out::Read(Pending {
                            turn: message,
                            ..words(from.clone(), node, brief)
                        }));
                        self.briefed_by(task, Some(&from), node);
                    }
                    Some(None) => self.briefed_by(task, None, node),
                    None => {}
                }
            }
            // A message that ended in line was never the task's to answer.
            "message_ended" => {
                self.steers.remove(&key);
                self.queued.remove(&key);
                self.asked.remove(&key);
            }
            "tool_started" if data["name"] == crate::margin::TOOL => {
                if let (Some(call), Some(called)) =
                    (data["call_id"].as_str(), crate::margin::called(data))
                {
                    if self.margin_calls.len() >= CALLS {
                        self.margin_calls.clear();
                    }
                    self.margin_calls
                        .insert((task.to_owned(), call.to_owned()), called);
                }
            }
            "tool_started" if data["name"] == crate::asks::TOOL => {
                let asked = (data["arguments"].as_str())
                    .and_then(|a| serde_json::from_str::<Value>(a).ok())
                    .and_then(|arguments| crate::asks::call("", &arguments));
                if let (Some(call), Some(asked)) = (data["call_id"].as_str(), asked) {
                    if self.ask_calls.len() >= CALLS {
                        self.ask_calls.clear();
                    }
                    self.ask_calls
                        .insert((task.to_owned(), call.to_owned()), asked);
                }
            }
            "tool_completed" => {
                let call = (
                    task.to_owned(),
                    data["call_id"].as_str().unwrap_or("").to_owned(),
                );
                let carried = data["failed"] != true && data["denied"] != true;
                if let Some((id, removed)) = self.margin_calls.remove(&call)
                    && carried
                {
                    out.push(Out::Now(Box::new(margin(
                        task, maker, turn, &call.1, &id, removed,
                    ))));
                }
                if let Some(asked) = self.ask_calls.remove(&call)
                    && carried
                {
                    out.push(Out::Now(Box::new(question(
                        task, maker, turn, &call.1, &asked,
                    ))));
                }
            }
            "approval_requested" => {
                for call in data["calls"].as_array().into_iter().flatten() {
                    if crate::needs::person_call(call)
                        && let Some(id) = call["call_id"].as_str()
                        && self.approval(task, id)
                    {
                        out.push(Out::Read(pending(Kind::Call {
                            call: id.to_owned(),
                            name: call["name"].as_str().map(str::to_owned),
                            node: call["node"].as_i64().filter(|node| *node > 0),
                            reason: None,
                        })));
                    }
                }
            }
            // A reviewer handed the call to the person, and said why.
            "approval_asked" => {
                if let Some(id) = data["call_id"].as_str()
                    && self.approval(task, id)
                {
                    out.push(Out::Read(pending(Kind::Call {
                        call: id.to_owned(),
                        name: None,
                        node: None,
                        reason: data["reason"].as_str().map(str::to_owned),
                    })));
                }
            }
            "turn_finished" => {
                self.approvals.retain(|(t, _)| t != task);
                let done = self.resolving.ended(task, turn) && data["status"] == "completed";
                let asked_by = match self.asked.remove(&key) {
                    // How a turn a lookout note started ends answers it.
                    Some(Asker::Note) => return out,
                    Some(Asker::By(by)) => Some(by),
                    None => None,
                };
                let status = data["status"].as_str().unwrap_or("?").to_owned();
                let error = (data["error"].as_str())
                    .filter(|_| status != "completed")
                    .map(str::to_owned);
                out.push(Out::Read(pending(Kind::Reply {
                    node: data["checkpoint"].as_i64(),
                    completed: status == "completed",
                    status,
                    error,
                    done,
                    asked_by,
                })));
            }
            _ => {}
        }
        out
    }

    /// Whether a call waiting on the person is told now: once a turn.
    fn approval(&mut self, task: &str, call: &str) -> bool {
        if self.approvals.len() >= CALLS {
            self.approvals.clear();
        }
        self.approvals.insert((task.to_owned(), call.to_owned()))
    }
}

/// Who asked for a turn, as its end names them: `you` for the subscriber,
/// an agent by its name, an automation as `automation`, a client or a
/// person a client named as named, else the person.
pub fn by(lead: &str, from: Option<&str>, origin: Option<&str>) -> String {
    match (from, origin) {
        (Some(from), _) if from == lead => "you".to_owned(),
        (Some(from), _) => from.to_owned(),
        (None, Some(origin)) if crate::trigger::sent_it(origin) => {
            crate::trigger::ORIGIN.to_owned()
        }
        (None, Some(origin)) => origin.to_owned(),
        (None, None) => PERSON.to_owned(),
    }
}

/// An event of this source: `kind` in `task`'s turn, said by `from`.
#[allow(clippy::too_many_arguments)]
fn event(
    task: &str,
    maker: Option<&str>,
    turn: i64,
    kind: &str,
    id: String,
    from: String,
    text: String,
    mut fields: Value,
) -> Event {
    let text = cut(&text);
    fields["type"] = json!(kind);
    fields["bot"] = json!(task);
    if let Some(maker) = maker {
        fields["maker"] = json!(maker);
    }
    fields["turn"] = json!(turn);
    fields["text"] = json!(text);
    if fields.get("from").is_none() {
        fields["from"] = json!(task);
    }
    Event {
        source: SOURCE.to_owned(),
        id: Some(id),
        topic: Some(task.to_owned()),
        text,
        from: Some(from),
        details: None,
        quote: None,
        steer: false,
        steer_by: None,
        digest: None,
        fields: Arc::new(fields),
        lookout: false,
        at_ms: super::epoch_ms(),
        seq: super::sources::next_seq(),
    }
}

/// A margin note the task put or took away.
fn margin(
    task: &str,
    maker: Option<&str>,
    turn: i64,
    call: &str,
    id: &str,
    removed: bool,
) -> Event {
    let from = match removed {
        true => format!("{task} took the margin note \"{id}\" away"),
        false => format!("{task} put the margin note \"{id}\""),
    };
    event(
        task,
        maker,
        turn,
        "margin",
        format!("{task}/{turn}/margin/{call}"),
        from,
        format!("{task}/{id}"),
        json!({"id": id, "removed": removed}),
    )
}

/// A question the task put to the person, who answers it in the task.
fn question(
    task: &str,
    maker: Option<&str>,
    turn: i64,
    call: &str,
    asked: &crate::asks::Ask,
) -> Event {
    let options: Vec<String> = asked.options.iter().map(|c| c.label.clone()).collect();
    let numbered: Vec<String> = (options.iter().enumerate())
        .map(|(at, label)| match asked.recommended == Some(at) {
            true => format!("{} {label} (recommended)", at + 1),
            false => format!("{} {label}", at + 1),
        })
        .collect();
    // A step of the person's has nothing to pick: its why says how.
    let text = match asked.options.is_empty() {
        true => format!("{}\n{}", asked.question, asked.why),
        false => format!("{}\n{}", asked.question, numbered.join(" · ")),
    };
    let mut made = event(
        task,
        maker,
        turn,
        "question",
        format!("{task}/{turn}/question/{call}"),
        format!("{task} asked the person"),
        text,
        json!({"options": options, "recommended": asked.recommended}),
    );
    made.steer = true;
    made
}

/// Builds a pending event from what was read: `None` when it is no event,
/// as an automation's quiet run, or the words of a message that turned
/// out to be no one's.
pub fn built(lead: &str, pending: &Pending, read: Read) -> Option<Event> {
    let Pending {
        task,
        maker,
        turn,
        kind,
        ..
    } = pending;
    let maker = maker.as_deref();
    match (kind, read) {
        (Kind::Words { from, .. }, Read::Words { text, someone }) => {
            let from = match someone {
                true => "someone".to_owned(),
                false => from.clone(),
            };
            let person = from == PERSON;
            // The person's words go as the daemon quotes them from their
            // turn, never as the lookout says them.
            let said = match person {
                true => format!("(their words are quoted below, from {task}/{turn})"),
                false => text.clone(),
            };
            let mut made = event(
                task,
                maker,
                *turn,
                "wrote",
                format!("{task}/{turn}/wrote"),
                from.clone(),
                said,
                json!({"from": from}),
            );
            if person {
                made.quote = Some((task.clone(), *turn));
                made.steer = true;
                // Its filters read the words as written.
                Arc::make_mut(&mut made.fields)["text"] = json!(cut(&text));
            }
            Some(made)
        }
        (
            Kind::Reply {
                status,
                error,
                done,
                asked_by,
                ..
            },
            Read::Reply { text, asker },
        ) => {
            let asked_by = asked_by.clone().or_else(|| {
                let (from, origin) = asker?;
                Some(by(lead, from.as_deref(), origin.as_deref()))
            });
            // A schedule's run that went well and said nothing is no event.
            if status == "completed"
                && !done
                && asked_by.as_deref() == Some(crate::trigger::ORIGIN)
                && text.trim().is_empty()
            {
                return None;
            }
            let mut from = match text.trim().is_empty() {
                true => format!("{task} ended"),
                false => format!("{task} replied"),
            };
            if let Some(asker) = asked_by.as_deref().filter(|by| *by != "you") {
                from.push_str(&format!(" to {asker}"));
            }
            from.push_str(&format!(", {status}"));
            if let Some(error) = error {
                from.push_str(&format!(": {error}"));
            }
            if *done {
                from.push_str(", closed itself");
            }
            Some(event(
                task,
                maker,
                *turn,
                "turn.ended",
                format!("{task}/{turn}/ended"),
                from,
                text,
                json!({"status": status, "done": done, "asked_by": asked_by, "error": error}),
            ))
        }
        (
            Kind::Call {
                call, name, reason, ..
            },
            Read::Call { name: read, text },
        ) => {
            let name = read.or_else(|| name.clone());
            let mut made = event(
                task,
                maker,
                *turn,
                "approval",
                format!("{task}/{turn}/approval/{call}"),
                format!("{task} waits for the person's approval"),
                text,
                json!({"call": name, "reason": reason}),
            );
            // The reviewer's reason on its own line, after what waits.
            made.details = reason.as_deref().map(cut);
            made.steer = true;
            Some(made)
        }
        _ => None,
    }
}

/// What was read for a pending event.
#[derive(Clone, Debug)]
pub enum Read {
    /// The words, and whether words with no sender repeat the
    /// subscriber's own brief, so are not the person's.
    Words { text: String, someone: bool },
    /// The reply, and who asked for the turn when that was to read: the
    /// bot, and the client or person a client named.
    Reply {
        text: String,
        asker: Option<(Option<String>, Option<String>)>,
    },
    /// The call's tool and what it wants to run.
    Call { name: Option<String>, text: String },
}

/// What a call waiting for approval wants to run, from its arguments: the
/// command a shell call runs, or the tool and its arguments.
pub fn call_text(name: Option<&str>, arguments: Option<&str>) -> String {
    let parsed: Value = arguments
        .and_then(|a| serde_json::from_str(a).ok())
        .unwrap_or(Value::Null);
    let command = ["command", "cmd"]
        .iter()
        .find_map(|key| match &parsed[*key] {
            Value::String(line) => Some(line.clone()),
            Value::Array(words) => Some(
                (words.iter().filter_map(Value::as_str))
                    .collect::<Vec<_>>()
                    .join(" "),
            ),
            _ => None,
        });
    let name = name.unwrap_or("a tool");
    match (command, arguments) {
        (Some(command), _) => command,
        (None, Some(arguments)) if !arguments.trim().is_empty() => format!("{name} {arguments}"),
        _ => name.to_owned(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ev(bot: &str, turn: i64, kind: &str, data: Value) -> Value {
        json!({"bot": bot, "turn": turn, "event": kind, "data": data})
    }

    fn reads(outs: Vec<Out>) -> Vec<Pending> {
        (outs.into_iter())
            .filter_map(|out| match out {
                Out::Read(pending) => Some(pending),
                Out::Now(_) => None,
            })
            .collect()
    }

    #[test]
    fn the_person_s_words_are_quoted_and_steer() {
        let mut activity = Activity::new("calc.lead");
        let pending =
            reads(activity.event(&ev("calc.ui", 3, "accepted", json!({"node": 7})), None, 1));
        assert_eq!(pending.len(), 1);
        let made = built(
            "calc.lead",
            &pending[0],
            Read::Words {
                text: "make the button blue".into(),
                someone: false,
            },
        )
        .unwrap();
        assert_eq!(made.from.as_deref(), Some(PERSON));
        assert_eq!(made.quote, Some(("calc.ui".into(), 3)));
        assert!(made.steer);
        assert!(!made.text.contains("blue"), "the daemon quotes it");
        assert_eq!(made.fields["text"], "make the button blue");
        assert_eq!(made.fields["type"], "wrote");
    }

    #[test]
    fn the_subscriber_s_own_message_is_no_event_but_its_turn_s_end_is() {
        let mut activity = Activity::new("calc.lead");
        let from_lead = json!({"from": {"bot": "calc.lead"}, "node": 4});
        assert!(
            reads(activity.event(&ev("calc.ui", 3, "accepted", from_lead), None, 1)).is_empty()
        );
        let ended = reads(activity.event(
            &ev(
                "calc.ui",
                3,
                "turn_finished",
                json!({"status": "completed", "checkpoint": 9}),
            ),
            Some("calc.lead"),
            2,
        ));
        let made = built(
            "calc.lead",
            &ended[0],
            Read::Reply {
                text: "Done, the button is blue.".into(),
                asker: None,
            },
        )
        .unwrap();
        assert_eq!(made.from.as_deref(), Some("calc.ui replied, completed"));
        assert_eq!(made.text, "Done, the button is blue.");
        assert_eq!(made.fields["asked_by"], "you");
        assert_eq!(made.fields["maker"], "calc.lead");
        assert!(!made.steer);
    }

    #[test]
    fn a_turn_a_lookout_note_started_ends_without_an_event() {
        let mut activity = Activity::new("calc.lead");
        let note = json!({"request_id": "lookout-abc", "from": {"bot": null}});
        assert!(
            activity
                .event(&ev("calc.sub", 5, "accepted", note), None, 1)
                .is_empty()
        );
        let ended = ev(
            "calc.sub",
            5,
            "turn_finished",
            json!({"status": "completed"}),
        );
        assert!(activity.event(&ended, None, 2).is_empty());
    }

    #[test]
    fn an_automation_s_quiet_run_is_no_event_but_a_failed_one_is() {
        let mut activity = Activity::new("calc.lead");
        let run = json!({"origin": "automation"});
        activity.event(&ev("calc.cron", 5, "accepted", run.clone()), None, 1);
        let ended = reads(activity.event(
            &ev(
                "calc.cron",
                5,
                "turn_finished",
                json!({"status": "completed"}),
            ),
            None,
            2,
        ));
        let quiet = Read::Reply {
            text: " ".into(),
            asker: None,
        };
        assert_eq!(built("calc.lead", &ended[0], quiet), None);
        activity.event(&ev("calc.cron", 6, "accepted", run), None, 3);
        let failed = reads(activity.event(
            &ev(
                "calc.cron",
                6,
                "turn_finished",
                json!({"status": "failed", "error": "rate_limited"}),
            ),
            None,
            4,
        ));
        let made = built(
            "calc.lead",
            &failed[0],
            Read::Reply {
                text: String::new(),
                asker: None,
            },
        )
        .unwrap();
        assert_eq!(
            made.from.as_deref(),
            Some("calc.cron ended to automation, failed: rate_limited")
        );
    }

    #[test]
    fn a_turn_not_seen_starting_names_who_asked_once_read() {
        let mut activity = Activity::new("calc.lead");
        let ended = reads(activity.event(
            &ev(
                "calc.ui",
                8,
                "turn_finished",
                json!({"status": "completed"}),
            ),
            None,
            1,
        ));
        let asker = Some((None, None));
        let made = built(
            "calc.lead",
            &ended[0],
            Read::Reply {
                text: "ok".into(),
                asker,
            },
        )
        .unwrap();
        assert_eq!(
            made.from.as_deref(),
            Some("calc.ui replied to the person, completed")
        );
    }

    #[test]
    fn questions_margin_notes_and_approvals_are_events() {
        let mut activity = Activity::new("calc.lead");
        let ask = json!({"name": "ask", "call_id": "c1", "arguments": json!({"question": "Ship dark mode first?",
            "options": [{"label": "Yes"}, {"label": "No"}], "recommended": 0}).to_string()});
        activity.event(&ev("calc.ui", 2, "tool_started", ask), None, 1);
        let outs = activity.event(
            &ev("calc.ui", 2, "tool_completed", json!({"call_id": "c1"})),
            None,
            2,
        );
        let Some(Out::Now(asked)) = outs.into_iter().next() else {
            panic!("no question")
        };
        assert_eq!(asked.from.as_deref(), Some("calc.ui asked the person"));
        assert!(
            asked.text.starts_with("Ship dark mode first?\n1 Yes"),
            "{}",
            asked.text
        );
        assert!(asked.steer);

        let put = json!({"name": "margin", "call_id": "m1",
            "arguments": json!({"id": "release", "text": "v2"}).to_string()});
        activity.event(&ev("calc.ui", 2, "tool_started", put), None, 3);
        let outs = activity.event(
            &ev("calc.ui", 2, "tool_completed", json!({"call_id": "m1"})),
            None,
            4,
        );
        let Some(Out::Now(noted)) = outs.into_iter().next() else {
            panic!("no margin note")
        };
        assert_eq!(
            noted.from.as_deref(),
            Some("calc.ui put the margin note \"release\"")
        );
        assert_eq!(noted.text, "calc.ui/release");

        let waits =
            json!({"calls": [{"call_id": "x", "gates": ["manual"], "name": "exec", "node": 12}]});
        let pending = reads(activity.event(
            &ev("calc.db", 4, "approval_requested", waits.clone()),
            None,
            5,
        ));
        assert_eq!(pending.len(), 1);
        // Told once a turn, however often it is asked.
        assert!(
            reads(activity.event(&ev("calc.db", 4, "approval_requested", waits), None, 6))
                .is_empty()
        );
        let text = call_text(Some("exec"), Some(r#"{"command": "rm -rf build/"}"#));
        let made = built("calc.lead", &pending[0], Read::Call { name: None, text }).unwrap();
        assert_eq!(
            made.from.as_deref(),
            Some("calc.db waits for the person's approval")
        );
        assert_eq!(made.text, "rm -rf build/");
        assert_eq!(made.fields["call"], "exec");
        assert!(made.details.is_none());
        // One the approver answers is not the person's.
        let auto = json!({"calls": [{"call_id": "y", "gates": ["auto"], "name": "exec"}]});
        assert!(
            reads(activity.event(&ev("calc.db", 4, "approval_requested", auto), None, 7))
                .is_empty()
        );
    }

    #[test]
    fn a_call_the_reviewer_hands_over_names_what_waits_and_why() {
        let mut activity = Activity::new("calc.lead");
        let reason = "not reviewed: the call is too long to show. If your caller wants this, ask them to name the action in a message, then try again.";
        let asked = json!({"call_id": "script-0/3", "request": 1, "tag": "auto", "reason": reason,
            "step_of": "script-0"});
        let pending =
            reads(activity.event(&ev("calc.db", 4, "approval_asked", asked.clone()), None, 1));
        let [pending] = &pending[..] else {
            panic!("{pending:?}")
        };
        assert_eq!(
            pending.kind,
            Kind::Call {
                call: "script-0/3".into(),
                name: None,
                node: None,
                reason: Some(reason.into()),
            }
        );
        // Told once a turn.
        assert!(
            reads(activity.event(&ev("calc.db", 4, "approval_asked", asked), None, 2)).is_empty()
        );
        // Read from the calls waiting: the command it runs, and the reason
        // on its own line.
        let text = call_text(
            Some("shell"),
            Some(r#"{"command":"git push --force origin main"}"#),
        );
        let read = Read::Call {
            name: Some("shell".into()),
            text,
        };
        let made = built("calc.lead", pending, read).unwrap();
        assert_eq!(
            made.from.as_deref(),
            Some("calc.db waits for the person's approval")
        );
        assert_eq!(made.text, "git push --force origin main");
        assert_eq!(made.details.as_deref(), Some(reason));
        assert_eq!(
            (&made.fields["call"], &made.fields["reason"]),
            (&json!("shell"), &json!(reason))
        );
        let note = super::super::sources::line(&super::super::sources::Heard {
            event: made,
            subs: Vec::new(),
            order: 1,
            due: false,
        });
        assert!(
            note.ends_with(&format!("\ngit push --force origin main\n{reason}")),
            "{note}"
        );
        // A tool with no command shows its name and arguments.
        assert_eq!(
            call_text(Some("write"), Some(r#"{"path":"a.md"}"#)),
            r#"write {"path":"a.md"}"#
        );
    }

    #[test]
    fn a_task_that_closed_itself_says_so() {
        let mut activity = Activity::new("calc.lead");
        let call = json!({"name": crate::resolved::TOOL, "call_id": "d",
            "arguments": "{\"name\":\"calc.docs\"}"});
        activity.event(&ev("calc.docs", 3, "tool_started", call), None, 1);
        activity.event(
            &ev("calc.docs", 3, "tool_completed", json!({"call_id": "d"})),
            None,
            2,
        );
        let ended = reads(activity.event(
            &ev(
                "calc.docs",
                3,
                "turn_finished",
                json!({"status": "completed"}),
            ),
            None,
            3,
        ));
        let made = built(
            "calc.lead",
            &ended[0],
            Read::Reply {
                text: "Docs are up.".into(),
                asker: Some((Some("calc.lead".into()), None)),
            },
        )
        .unwrap();
        assert_eq!(
            made.from.as_deref(),
            Some("calc.docs replied, completed, closed itself")
        );
        assert_eq!(made.fields["done"], true);
    }
}
